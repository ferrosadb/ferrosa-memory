use clap::{Parser, Subcommand, ValueEnum};
use ferrosa_memory_eval::bright_pro::{BrightProConfig, BrightProProtocol, ReasoningAspect};
use ferrosa_memory_eval::fixture::{
    BrightProFixture, CorpusDocument, LexicalFixtureRetriever, run_bright_pro_fixture,
};
use ferrosa_memory_eval::live_fixture::{
    LiveMcpFixtureRunner, run_bright_pro_fixture_live, run_memorybench_fixture_live,
};
use ferrosa_memory_eval::mcp_client::HttpMcpClient;
use ferrosa_memory_eval::memory_quality::EvidenceGroundTruth;
use ferrosa_memory_eval::memorybench::{
    LocalLlmConfig, MemoryBenchCase, MemoryBenchFixture, SyntheticConversationSpec,
    generate_two_agent_conversation, run_memorybench_fixture, synthetic_memorybench_fixture,
};
use serde::Serialize;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "ferrosa-memory-eval", about = "MCP evaluation framework")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run deterministic local fixture suites without a live MCP server.
    FixtureSmoke {
        #[arg(long, value_enum, default_value_t = FixtureSuite::All)]
        suite: FixtureSuite,
        #[arg(long, value_enum, default_value_t = FixtureBackend::Lexical)]
        backend: FixtureBackend,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "BRIGHT-Pro")]
        synthetic_topic: String,
        #[arg(long, default_value_t = 3)]
        synthetic_conversations: usize,
        /// Load a BRIGHT-Pro fixture JSON file instead of the built-in smoke fixture.
        #[arg(long)]
        bright_pro_fixture: Option<PathBuf>,
        /// Load a MemoryBench fixture JSON file instead of the synthetic fixture.
        #[arg(long)]
        memorybench_fixture: Option<PathBuf>,
        /// Generate MemoryBench synthetic conversations with a local Ollama-compatible LLM.
        #[arg(long)]
        use_local_llm: bool,
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        ollama_url: String,
        #[arg(long, default_value = "qwen3.5:27b")]
        ollama_model: String,
        #[arg(long, default_value_t = 0.9)]
        temperature: f64,
        /// MCP JSON-RPC HTTP endpoint used by --backend mcp-http.
        #[arg(long, default_value = "http://127.0.0.1:18775/mcp")]
        mcp_url: String,
        #[arg(long, default_value = "user")]
        mcp_user: String,
        #[arg(long, default_value = "pass")]
        mcp_password: String,
        /// Optional session id for live MCP fixture data. Defaults to a fresh UUID per suite.
        #[arg(long)]
        mcp_session_id: Option<Uuid>,
        /// Ranked retrieval depth for fixture scoring.
        #[arg(long, default_value_t = 25)]
        retrieval_k: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FixtureSuite {
    All,
    BrightPro,
    MemoryBench,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FixtureBackend {
    Lexical,
    McpHttp,
}

#[derive(Debug, Serialize)]
struct FixtureSmokeReport {
    bright_pro: Option<ferrosa_memory_eval::fixture::BrightProFixtureResult>,
    memorybench: Option<ferrosa_memory_eval::memorybench::MemoryBenchResult>,
    backend: String,
    retrieval_k: usize,
    live_session_ids: Vec<Uuid>,
}

struct FixtureSmokeConfig<'a> {
    suite: FixtureSuite,
    synthetic_topic: &'a str,
    synthetic_conversations: usize,
    bright_pro_fixture_path: Option<PathBuf>,
    memorybench_fixture_path: Option<PathBuf>,
    llm: Option<&'a LocalLlmConfig>,
    backend: FixtureBackend,
    mcp_url: &'a str,
    mcp_user: &'a str,
    mcp_password: &'a str,
    mcp_session_id: Option<Uuid>,
    retrieval_k: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::FixtureSmoke {
            suite,
            backend,
            json,
            synthetic_topic,
            synthetic_conversations,
            bright_pro_fixture,
            memorybench_fixture,
            use_local_llm,
            ollama_url,
            ollama_model,
            temperature,
            mcp_url,
            mcp_user,
            mcp_password,
            mcp_session_id,
            retrieval_k,
        }) => {
            let llm = use_local_llm.then_some(LocalLlmConfig {
                base_url: ollama_url,
                model: ollama_model,
                temperature,
            });
            let report = run_fixture_smoke(FixtureSmokeConfig {
                suite,
                synthetic_topic: &synthetic_topic,
                synthetic_conversations,
                bright_pro_fixture_path: bright_pro_fixture,
                memorybench_fixture_path: memorybench_fixture,
                llm: llm.as_ref(),
                backend,
                mcp_url: &mcp_url,
                mcp_user: &mcp_user,
                mcp_password: &mcp_password,
                mcp_session_id,
                retrieval_k: retrieval_k.clamp(1, 50),
            })
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                render_fixture_smoke_text(&report);
            }
        }
        None => {
            eprintln!("ferrosa-memory-eval: use `fixture-smoke` for local benchmark fixtures");
            std::process::exit(2);
        }
    }
    Ok(())
}

async fn run_fixture_smoke(config: FixtureSmokeConfig<'_>) -> anyhow::Result<FixtureSmokeReport> {
    let FixtureSmokeConfig {
        suite,
        synthetic_topic,
        synthetic_conversations,
        bright_pro_fixture_path,
        memorybench_fixture_path,
        llm,
        backend,
        mcp_url,
        mcp_user,
        mcp_password,
        mcp_session_id,
        retrieval_k,
    } = config;

    let mut live_session_ids = Vec::new();
    let bright_pro = if matches!(suite, FixtureSuite::All | FixtureSuite::BrightPro) {
        let fixture = match bright_pro_fixture_path {
            Some(path) => read_json_fixture::<BrightProFixture>(&path)?,
            None => bright_pro_smoke_fixture(),
        };
        Some(match backend {
            FixtureBackend::Lexical => {
                let retriever = LexicalFixtureRetriever::new(fixture.corpus.clone());
                run_bright_pro_fixture(&fixture, &retriever, retrieval_k)
            }
            FixtureBackend::McpHttp => {
                let mut runner =
                    live_runner(mcp_url, mcp_user, mcp_password, mcp_session_id).await?;
                live_session_ids.push(runner.session_id());
                run_bright_pro_fixture_live(&fixture, &mut runner, retrieval_k).await?
            }
        })
    } else {
        None
    };
    let memorybench = if matches!(suite, FixtureSuite::All | FixtureSuite::MemoryBench) {
        let fixture = match memorybench_fixture_path {
            Some(path) => read_json_fixture::<MemoryBenchFixture>(&path)?,
            None if llm.is_some() => {
                local_llm_memorybench_fixture(synthetic_topic, synthetic_conversations, llm).await
            }
            None => synthetic_memorybench_fixture(synthetic_topic, synthetic_conversations),
        };
        Some(match backend {
            FixtureBackend::Lexical => {
                let retriever = LexicalFixtureRetriever::new(fixture.corpus_documents());
                run_memorybench_fixture(&fixture, &retriever, retrieval_k)
            }
            FixtureBackend::McpHttp => {
                let mut runner =
                    live_runner(mcp_url, mcp_user, mcp_password, mcp_session_id).await?;
                live_session_ids.push(runner.session_id());
                run_memorybench_fixture_live(&fixture, &mut runner, retrieval_k).await?
            }
        })
    } else {
        None
    };

    Ok(FixtureSmokeReport {
        bright_pro,
        memorybench,
        backend: match backend {
            FixtureBackend::Lexical => "lexical".to_string(),
            FixtureBackend::McpHttp => "mcp-http".to_string(),
        },
        retrieval_k,
        live_session_ids,
    })
}

async fn live_runner(
    mcp_url: &str,
    mcp_user: &str,
    mcp_password: &str,
    mcp_session_id: Option<Uuid>,
) -> anyhow::Result<LiveMcpFixtureRunner> {
    let client = HttpMcpClient::new(mcp_url).with_basic_auth(mcp_user, mcp_password);
    let mut runner = LiveMcpFixtureRunner::new(client, mcp_session_id.unwrap_or_else(Uuid::new_v4));
    runner.initialize().await?;
    Ok(runner)
}

fn read_json_fixture<T: serde::de::DeserializeOwned>(path: &PathBuf) -> anyhow::Result<T> {
    let contents = std::fs::read_to_string(path)?;
    serde_json::from_str(&contents)
        .map_err(|err| anyhow::anyhow!("failed to parse fixture {}: {err}", path.display()))
}

async fn local_llm_memorybench_fixture(
    synthetic_topic: &str,
    synthetic_conversations: usize,
    llm: Option<&LocalLlmConfig>,
) -> MemoryBenchFixture {
    let mut conversations = Vec::with_capacity(synthetic_conversations);
    for idx in 0..synthetic_conversations {
        let spec = SyntheticConversationSpec::deterministic(synthetic_topic, idx);
        conversations.push(generate_two_agent_conversation(&spec, llm).await);
    }
    let evidence_id = conversations
        .first()
        .map(|conversation| conversation.id.clone())
        .unwrap_or_else(|| format!("synthetic:{synthetic_topic}:0").replace(' ', "_"));
    MemoryBenchFixture {
        id: format!("memorybench-llm-{synthetic_topic}").replace(' ', "_"),
        static_corpus: Vec::new(),
        training_conversations: Vec::new(),
        synthetic_conversations: conversations,
        cases: vec![MemoryBenchCase {
            id: "retrieve-local-llm-synthetic-preference".into(),
            query: format!("{synthetic_topic} exact files concrete implementation details"),
            expected_answer_terms: vec!["exact".into(), "files".into(), "concrete".into()],
            ground_truth: EvidenceGroundTruth {
                required_entities: vec![evidence_id],
                required_folds: Vec::new(),
                required_facts: Vec::new(),
                required_edges: Vec::new(),
                distractor_entities: Vec::new(),
            },
        }],
    }
}

fn bright_pro_smoke_fixture() -> BrightProFixture {
    BrightProFixture {
        id: "bright-pro-smoke".into(),
        query: "reasoning intensive retrieval needs aspect aware complementary evidence".into(),
        config: BrightProConfig {
            protocol: BrightProProtocol::Static,
            alpha: 0.5,
            gamma: 0.05,
            aspects: vec![
                ReasoningAspect {
                    id: "aspect-coverage".into(),
                    weight: 1.0,
                    evidence_ids: vec!["doc:aspect-coverage".into()],
                },
                ReasoningAspect {
                    id: "agentic-evidence".into(),
                    weight: 1.0,
                    evidence_ids: vec!["doc:agentic-evidence".into()],
                },
            ],
        },
        corpus: vec![
            CorpusDocument::new(
                "doc:aspect-coverage",
                "Aspect aware retrieval rewards covering complementary reasoning needs.",
            ),
            CorpusDocument::new(
                "doc:agentic-evidence",
                "Agentic search needs evidence portfolios across iterative retrieval rounds.",
            ),
            CorpusDocument::new(
                "doc:noise",
                "General embeddings can match topics without evidence diversity.",
            ),
        ],
    }
}

fn render_fixture_smoke_text(report: &FixtureSmokeReport) {
    println!("backend={}", report.backend);
    println!("retrieval_k={}", report.retrieval_k);
    for session_id in &report.live_session_ids {
        println!("live_session_id={session_id}");
    }
    if let Some(bright) = &report.bright_pro {
        println!(
            "BRIGHT-Pro {} alpha_ndcg={:.3} aspect_recall={:.3} hits={}",
            bright.fixture_id,
            bright.score.alpha_ndcg,
            bright.score.aspect_recall,
            bright.hits.len()
        );
    }
    if let Some(memory) = &report.memorybench {
        println!(
            "MemoryBench {} recall={:.3} answer_terms={:.3} feedback_gain={:.3} cases={}",
            memory.fixture_id,
            memory.mean_recall_at_k,
            memory.mean_answer_term_recall,
            memory.mean_feedback_gain,
            memory.cases.len()
        );
    }
}
