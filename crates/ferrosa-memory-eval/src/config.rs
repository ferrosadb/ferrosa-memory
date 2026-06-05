//! Eval harness configuration.
//!
//! Loads from the `[eval]` section of `ferrosa-memory.toml`, with CLI flag
//! overrides. All fields have sensible defaults so a bare `[eval]` table is
//! sufficient.

use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;
use uuid::Uuid;

use crate::llm::LlmConfig;

// ── Default value functions (serde) ─────────────────────────────────────────

fn default_scenario_dir() -> PathBuf {
    PathBuf::from("scenarios/")
}

fn default_output_dir() -> PathBuf {
    PathBuf::from("eval-results/")
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn default_passing_threshold() -> f64 {
    0.75
}

fn default_warmup() -> bool {
    true
}

fn default_mcp_binary() -> PathBuf {
    PathBuf::from("ferrosa-memory-mcp")
}

fn default_cql_seeds() -> String {
    "127.0.0.1:9042".to_string()
}

fn default_tenant_id() -> String {
    "00000000-0000-0000-0000-e0a100000000".to_string()
}

fn default_preflight_timeout_ms() -> u64 {
    100
}

fn default_transport() -> String {
    "stdio".to_string()
}

fn default_max_parallel() -> usize {
    4
}

fn default_retrieval_k() -> usize {
    25
}

fn default_judge_provider() -> String {
    ferrosa_memory_core::config::JudgeConfig::default().provider
}

fn default_judge_url() -> String {
    ferrosa_memory_core::config::JudgeConfig::default().base_url
}

fn default_judge_model() -> String {
    ferrosa_memory_core::config::JudgeConfig::default().model
}

fn default_judge_timeout_seconds() -> u64 {
    ferrosa_memory_core::config::JudgeConfig::default().timeout_seconds
}

fn default_judge_temperature() -> f64 {
    0.0
}

// ── TOML-deserializable config ──────────────────────────────────────────────

/// The `[eval]` section of `ferrosa-memory.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EvalToml {
    #[serde(default = "default_scenario_dir")]
    pub scenario_dir: PathBuf,

    #[serde(default = "default_output_dir")]
    pub output_dir: PathBuf,

    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    #[serde(default = "default_passing_threshold")]
    pub passing_threshold: f64,

    #[serde(default)]
    pub judge_enabled: bool,

    #[serde(default)]
    pub parallel: bool,

    #[serde(default)]
    pub stability_canary: bool,

    #[serde(default = "default_warmup")]
    pub warmup: bool,

    #[serde(default = "default_mcp_binary")]
    pub mcp_binary: PathBuf,

    #[serde(default = "default_cql_seeds")]
    pub cql_seeds: String,

    #[serde(default = "default_tenant_id")]
    pub tenant_id: String,

    #[serde(default = "default_preflight_timeout_ms")]
    pub preflight_timeout_ms: u64,

    /// Transport mode: "stdio" (default) or "http"
    #[serde(default = "default_transport")]
    pub transport: String,

    /// MCP server URL for HTTP transport mode
    #[serde(default)]
    pub mcp_url: Option<String>,

    /// Maximum parallel scenario executions (default 4)
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,

    /// Ranked retrieval depth for fixture/evidence evals.
    #[serde(default = "default_retrieval_k")]
    pub retrieval_k: usize,

    /// Provider for optional LLM generation and judge calls.
    #[serde(default = "default_judge_provider")]
    pub judge_provider: String,

    /// Base URL for optional LLM generation and judge calls.
    #[serde(default = "default_judge_url")]
    pub judge_url: String,

    /// Model name for optional LLM generation and judge calls.
    #[serde(default = "default_judge_model")]
    pub judge_model: String,

    /// Environment variable containing the optional provider token.
    #[serde(default)]
    pub judge_token_env: Option<String>,

    /// Request timeout for optional LLM generation and judge calls.
    #[serde(default = "default_judge_timeout_seconds")]
    pub judge_timeout_seconds: u64,

    /// Sampling temperature for synthetic fixture generation.
    #[serde(default = "default_judge_temperature")]
    pub judge_temperature: f64,

    /// Expected server binary SHA-256 hash (optional verification)
    #[serde(default)]
    pub expect_server_hash: Option<String>,

    /// Path to manifest JSON for verification
    #[serde(default)]
    pub verify_manifest: Option<String>,
}

impl Default for EvalToml {
    fn default() -> Self {
        Self {
            scenario_dir: default_scenario_dir(),
            output_dir: default_output_dir(),
            timeout_ms: default_timeout_ms(),
            passing_threshold: default_passing_threshold(),
            judge_enabled: false,
            parallel: false,
            stability_canary: false,
            warmup: default_warmup(),
            mcp_binary: default_mcp_binary(),
            cql_seeds: default_cql_seeds(),
            tenant_id: default_tenant_id(),
            preflight_timeout_ms: default_preflight_timeout_ms(),
            transport: default_transport(),
            mcp_url: None,
            max_parallel: default_max_parallel(),
            retrieval_k: default_retrieval_k(),
            judge_provider: default_judge_provider(),
            judge_url: default_judge_url(),
            judge_model: default_judge_model(),
            judge_token_env: None,
            judge_timeout_seconds: default_judge_timeout_seconds(),
            judge_temperature: default_judge_temperature(),
            expect_server_hash: None,
            verify_manifest: None,
        }
    }
}

/// Wrapper so we can parse `[eval]` out of the full config file.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    eval: EvalToml,
}

// ── CLI overrides ───────────────────────────────────────────────────────────

/// CLI flags that can override TOML values. All are optional so absence means
/// "use the TOML / default value".
#[derive(Debug, Clone, Parser)]
#[command(name = "ferrosa-memory-eval", about = "MCP evaluation framework")]
pub struct EvalCliOverrides {
    /// Path to scenario files or directory
    #[arg(long)]
    pub scenario_dir: Option<PathBuf>,

    /// Directory for writing evaluation results
    #[arg(long)]
    pub output_dir: Option<PathBuf>,

    /// Per-scenario timeout in milliseconds
    #[arg(long)]
    pub timeout_ms: Option<u64>,

    /// Minimum score to pass (0.0 – 1.0)
    #[arg(long)]
    pub passing_threshold: Option<f64>,

    /// Enable LLM-as-Judge grading
    #[arg(long)]
    pub judge_enabled: Option<bool>,

    /// Run scenarios in parallel
    #[arg(long)]
    pub parallel: Option<bool>,

    /// Run stability canary checks
    #[arg(long)]
    pub stability_canary: Option<bool>,

    /// Run warmup phase before evaluation
    #[arg(long)]
    pub warmup: Option<bool>,

    /// Path to the MCP server binary
    #[arg(long)]
    pub mcp_binary: Option<PathBuf>,

    /// CQL seed addresses (comma-separated)
    #[arg(long)]
    pub cql_seeds: Option<String>,

    /// Dedicated eval tenant UUID (isolates eval data from production)
    #[arg(long)]
    pub tenant_id: Option<String>,

    /// Pre-flight health check timeout in milliseconds
    #[arg(long)]
    pub preflight_timeout_ms: Option<u64>,

    /// Transport mode: "stdio" or "http"
    #[arg(long)]
    pub transport: Option<String>,

    /// MCP server URL for HTTP transport
    #[arg(long)]
    pub mcp_url: Option<String>,

    /// Maximum parallel scenario executions
    #[arg(long)]
    pub max_parallel: Option<usize>,

    /// Ranked retrieval depth for fixture/evidence evals
    #[arg(long)]
    pub retrieval_k: Option<usize>,

    /// Provider for optional LLM generation/judging: mock, ollama, lmstudio, openai_compatible
    #[arg(long)]
    pub judge_provider: Option<String>,

    /// Base URL for optional LLM generation/judging
    #[arg(long)]
    pub judge_url: Option<String>,

    /// Model for optional LLM generation/judging
    #[arg(long)]
    pub judge_model: Option<String>,

    /// Environment variable containing the optional provider token
    #[arg(long)]
    pub judge_token_env: Option<String>,

    /// Request timeout for optional LLM generation/judging
    #[arg(long)]
    pub judge_timeout_seconds: Option<u64>,

    /// Sampling temperature for synthetic fixture generation
    #[arg(long)]
    pub judge_temperature: Option<f64>,

    /// Expected server binary SHA-256 hash
    #[arg(long)]
    pub expect_server_hash: Option<String>,

    /// Path to manifest JSON for verification
    #[arg(long)]
    pub verify_manifest: Option<String>,

    /// Path to config file (overrides auto-detection)
    #[arg(long, short)]
    pub config: Option<PathBuf>,
}

// ── Resolved config ─────────────────────────────────────────────────────────

/// Fully-resolved eval configuration (TOML defaults + CLI overrides merged).
#[derive(Debug, Clone, PartialEq)]
pub struct EvalConfig {
    pub scenario_dir: PathBuf,
    pub output_dir: PathBuf,
    pub timeout_ms: u64,
    pub passing_threshold: f64,
    pub judge_enabled: bool,
    pub parallel: bool,
    pub stability_canary: bool,
    pub warmup: bool,
    pub mcp_binary: PathBuf,
    pub cql_seeds: String,
    /// Dedicated eval tenant UUID — isolates eval data from production.
    pub tenant_id: Uuid,
    /// Pre-flight health check timeout in milliseconds.
    pub preflight_timeout_ms: u64,
    /// Transport mode: "stdio" or "http".
    pub transport: String,
    /// MCP server URL for HTTP transport mode.
    pub mcp_url: Option<String>,
    /// Maximum parallel scenario executions.
    pub max_parallel: usize,
    /// Ranked retrieval depth for fixture/evidence evals.
    pub retrieval_k: usize,
    /// Provider for optional LLM generation and judge calls.
    pub judge_provider: String,
    /// Base URL for optional LLM generation and judge calls.
    pub judge_url: String,
    /// Model name for optional LLM generation and judge calls.
    pub judge_model: String,
    /// Environment variable containing the optional provider token.
    pub judge_token_env: Option<String>,
    /// Request timeout for optional LLM generation and judge calls.
    pub judge_timeout_seconds: u64,
    /// Sampling temperature for synthetic fixture generation.
    pub judge_temperature: f64,
    /// Expected server binary SHA-256 hash (optional verification).
    pub expect_server_hash: Option<String>,
    /// Path to manifest JSON for verification.
    pub verify_manifest: Option<String>,
}

impl Default for EvalConfig {
    fn default() -> Self {
        EvalToml::default().into()
    }
}

impl From<EvalToml> for EvalConfig {
    fn from(t: EvalToml) -> Self {
        let tenant_id = Uuid::parse_str(&t.tenant_id).unwrap_or_else(|_| {
            Uuid::parse_str(&default_tenant_id()).expect("default tenant_id is valid")
        });
        Self {
            scenario_dir: t.scenario_dir,
            output_dir: t.output_dir,
            timeout_ms: t.timeout_ms,
            passing_threshold: t.passing_threshold,
            judge_enabled: t.judge_enabled,
            parallel: t.parallel,
            stability_canary: t.stability_canary,
            warmup: t.warmup,
            mcp_binary: t.mcp_binary,
            cql_seeds: t.cql_seeds,
            tenant_id,
            preflight_timeout_ms: t.preflight_timeout_ms,
            transport: t.transport,
            mcp_url: t.mcp_url,
            max_parallel: t.max_parallel,
            retrieval_k: t.retrieval_k,
            judge_provider: t.judge_provider,
            judge_url: t.judge_url,
            judge_model: t.judge_model,
            judge_token_env: t.judge_token_env,
            judge_timeout_seconds: t.judge_timeout_seconds,
            judge_temperature: t.judge_temperature,
            expect_server_hash: t.expect_server_hash,
            verify_manifest: t.verify_manifest,
        }
    }
}

impl EvalConfig {
    pub fn llm_config(&self) -> LlmConfig {
        let token = self
            .judge_token_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty());
        LlmConfig {
            provider: self.judge_provider.clone(),
            base_url: self.judge_url.clone(),
            model: self.judge_model.clone(),
            token,
            timeout_seconds: self.judge_timeout_seconds,
            temperature: self.judge_temperature,
        }
    }

    /// Merge CLI overrides on top of TOML-loaded config. CLI wins when present.
    pub fn with_overrides(mut self, cli: &EvalCliOverrides) -> Self {
        if let Some(ref v) = cli.scenario_dir {
            self.scenario_dir = v.clone();
        }
        if let Some(ref v) = cli.output_dir {
            self.output_dir = v.clone();
        }
        if let Some(v) = cli.timeout_ms {
            self.timeout_ms = v;
        }
        if let Some(v) = cli.passing_threshold {
            self.passing_threshold = v;
        }
        if let Some(v) = cli.judge_enabled {
            self.judge_enabled = v;
        }
        if let Some(v) = cli.parallel {
            self.parallel = v;
        }
        if let Some(v) = cli.stability_canary {
            self.stability_canary = v;
        }
        if let Some(v) = cli.warmup {
            self.warmup = v;
        }
        if let Some(ref v) = cli.mcp_binary {
            self.mcp_binary = v.clone();
        }
        if let Some(ref v) = cli.cql_seeds {
            self.cql_seeds = v.clone();
        }
        if let Some(ref v) = cli.tenant_id
            && let Ok(parsed) = Uuid::parse_str(v)
        {
            self.tenant_id = parsed;
        }
        if let Some(v) = cli.preflight_timeout_ms {
            self.preflight_timeout_ms = v;
        }
        if let Some(ref v) = cli.transport {
            self.transport = v.clone();
        }
        if let Some(ref v) = cli.mcp_url {
            self.mcp_url = Some(v.clone());
        }
        if let Some(v) = cli.max_parallel {
            self.max_parallel = v;
        }
        if let Some(v) = cli.retrieval_k {
            self.retrieval_k = v.clamp(1, 50);
        }
        if let Some(ref v) = cli.judge_provider {
            self.judge_provider = v.clone();
        }
        if let Some(ref v) = cli.judge_url {
            self.judge_url = v.clone();
        }
        if let Some(ref v) = cli.judge_model {
            self.judge_model = v.clone();
        }
        if let Some(ref v) = cli.judge_token_env {
            self.judge_token_env = Some(v.clone());
        }
        if let Some(v) = cli.judge_timeout_seconds {
            self.judge_timeout_seconds = v.clamp(1, 300);
        }
        if let Some(v) = cli.judge_temperature {
            self.judge_temperature = v.clamp(0.0, 2.0);
        }
        if let Some(ref v) = cli.expect_server_hash {
            self.expect_server_hash = Some(v.clone());
        }
        if let Some(ref v) = cli.verify_manifest {
            self.verify_manifest = Some(v.clone());
        }
        self
    }
}

// ── Parsing helpers ─────────────────────────────────────────────────────────

/// Parse the `[eval]` section from a full `ferrosa-memory.toml` string.
/// Missing `[eval]` table produces defaults for every field.
pub fn parse_eval_toml(toml_str: &str) -> Result<EvalToml, toml::de::Error> {
    let file: ConfigFile = toml::from_str(toml_str)?;
    Ok(file.eval)
}

/// Load eval config from the given path, falling back to auto-detected paths.
/// Returns defaults when no config file is found.
pub fn load_eval_config(path: Option<&PathBuf>) -> anyhow::Result<EvalConfig> {
    let toml_str = match resolve_config_contents(path) {
        Some(s) => s,
        None => return Ok(EvalConfig::default()),
    };
    let eval_toml = parse_eval_toml(&toml_str)
        .map_err(|e| anyhow::anyhow!("failed to parse [eval] config: {e}"))?;
    Ok(eval_toml.into())
}

/// Resolve config file contents: explicit path > env var > CWD > XDG config dir.
fn resolve_config_contents(explicit: Option<&PathBuf>) -> Option<String> {
    if let Some(p) = explicit {
        return std::fs::read_to_string(p).ok();
    }
    if let Ok(p) = std::env::var("FERROSA_MEMORY_CONFIG")
        && let Ok(s) = std::fs::read_to_string(&p)
    {
        return Some(s);
    }
    if let Ok(s) = std::fs::read_to_string("ferrosa-memory.toml") {
        return Some(s);
    }
    if let Some(config_dir) = dirs::config_dir() {
        let path = config_dir.join("ferrosa").join("memory.toml");
        if let Ok(s) = std::fs::read_to_string(path) {
            return Some(s);
        }
    }
    None
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── T1: Defaults are sensible when [eval] section is absent ─────────

    #[test]
    fn defaults_when_eval_section_missing() {
        let toml_str = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let cfg: EvalConfig = parse_eval_toml(toml_str).unwrap().into();

        assert_eq!(cfg.scenario_dir, PathBuf::from("scenarios/"));
        assert_eq!(cfg.output_dir, PathBuf::from("eval-results/"));
        assert_eq!(cfg.timeout_ms, 30_000);
        assert!((cfg.passing_threshold - 0.75).abs() < f64::EPSILON);
        assert!(!cfg.judge_enabled);
        assert!(!cfg.parallel);
        assert!(!cfg.stability_canary);
        assert!(cfg.warmup);
        assert_eq!(cfg.mcp_binary, PathBuf::from("ferrosa-memory-mcp"));
        assert_eq!(cfg.cql_seeds, "127.0.0.1:9042");
        assert_eq!(cfg.retrieval_k, 25);
        assert_eq!(cfg.judge_provider, "ollama");
        assert_eq!(cfg.judge_url, "http://127.0.0.1:11434");
        assert_eq!(
            cfg.judge_model,
            ferrosa_memory_core::config::JudgeConfig::default().model
        );
        assert_eq!(cfg.judge_timeout_seconds, 30);
        assert!((cfg.judge_temperature - 0.0).abs() < f64::EPSILON);
        assert_eq!(
            cfg.tenant_id,
            Uuid::parse_str("00000000-0000-0000-0000-e0a100000000").unwrap()
        );
        assert_eq!(cfg.preflight_timeout_ms, 100);
    }

    // ── T2: Defaults when [eval] section is empty ───────────────────────

    #[test]
    fn defaults_when_eval_section_empty() {
        let toml_str = r#"
[eval]
"#;
        let cfg: EvalConfig = parse_eval_toml(toml_str).unwrap().into();
        assert_eq!(cfg, EvalConfig::default());
    }

    // ── T3: Parse a fully specified [eval] section ──────────────────────

    #[test]
    fn parse_full_eval_section() {
        let toml_str = r#"
[eval]
scenario_dir = "/tmp/my-scenarios"
output_dir = "/tmp/my-results"
timeout_ms = 60000
passing_threshold = 0.9
judge_enabled = true
parallel = true
stability_canary = true
warmup = false
mcp_binary = "/usr/local/bin/ferrosa-mcp"
cql_seeds = "10.0.0.1:9042,10.0.0.2:9042"
retrieval_k = 25
judge_provider = "lmstudio"
judge_url = "http://127.0.0.1:1234"
judge_model = "local-judge"
judge_token_env = "FMEM_JUDGE_TOKEN"
judge_timeout_seconds = 12
judge_temperature = 0.7
"#;
        let cfg: EvalConfig = parse_eval_toml(toml_str).unwrap().into();

        assert_eq!(cfg.scenario_dir, PathBuf::from("/tmp/my-scenarios"));
        assert_eq!(cfg.output_dir, PathBuf::from("/tmp/my-results"));
        assert_eq!(cfg.timeout_ms, 60_000);
        assert!((cfg.passing_threshold - 0.9).abs() < f64::EPSILON);
        assert!(cfg.judge_enabled);
        assert!(cfg.parallel);
        assert!(cfg.stability_canary);
        assert!(!cfg.warmup);
        assert_eq!(cfg.mcp_binary, PathBuf::from("/usr/local/bin/ferrosa-mcp"));
        assert_eq!(cfg.cql_seeds, "10.0.0.1:9042,10.0.0.2:9042");
        assert_eq!(cfg.retrieval_k, 25);
        assert_eq!(cfg.judge_provider, "lmstudio");
        assert_eq!(cfg.judge_url, "http://127.0.0.1:1234");
        assert_eq!(cfg.judge_model, "local-judge");
        assert_eq!(cfg.judge_token_env.as_deref(), Some("FMEM_JUDGE_TOKEN"));
        assert_eq!(cfg.judge_timeout_seconds, 12);
        assert!((cfg.judge_temperature - 0.7).abs() < f64::EPSILON);
    }

    // ── T4: Partial config fills missing fields with defaults ───────────

    #[test]
    fn parse_partial_eval_section() {
        let toml_str = r#"
[eval]
timeout_ms = 5000
judge_enabled = true
"#;
        let cfg: EvalConfig = parse_eval_toml(toml_str).unwrap().into();

        // Overridden
        assert_eq!(cfg.timeout_ms, 5000);
        assert!(cfg.judge_enabled);

        // Defaults
        assert_eq!(cfg.scenario_dir, PathBuf::from("scenarios/"));
        assert_eq!(cfg.output_dir, PathBuf::from("eval-results/"));
        assert!((cfg.passing_threshold - 0.75).abs() < f64::EPSILON);
        assert!(!cfg.parallel);
        assert!(!cfg.stability_canary);
        assert!(cfg.warmup);
        assert_eq!(cfg.mcp_binary, PathBuf::from("ferrosa-memory-mcp"));
        assert_eq!(cfg.cql_seeds, "127.0.0.1:9042");
        assert_eq!(cfg.retrieval_k, 25);
    }

    // ── T5: CLI overrides beat TOML values ──────────────────────────────

    #[test]
    fn cli_overrides_toml_values() {
        let toml_str = r#"
[eval]
timeout_ms = 5000
passing_threshold = 0.8
parallel = false
warmup = true
cql_seeds = "10.0.0.1:9042"
"#;
        let base: EvalConfig = parse_eval_toml(toml_str).unwrap().into();
        let cli = EvalCliOverrides {
            scenario_dir: Some(PathBuf::from("/cli/scenarios")),
            output_dir: None,
            timeout_ms: Some(120_000),
            passing_threshold: None,
            judge_enabled: Some(true),
            parallel: Some(true),
            stability_canary: None,
            warmup: Some(false),
            mcp_binary: None,
            cql_seeds: Some("cli-host:9042".to_string()),
            tenant_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            preflight_timeout_ms: Some(200),
            transport: None,
            mcp_url: None,
            max_parallel: None,
            retrieval_k: Some(12),
            judge_provider: Some("mock".to_string()),
            judge_url: Some("http://mock.local".to_string()),
            judge_model: Some("mock-judge".to_string()),
            judge_token_env: Some("TOKEN_ENV".to_string()),
            judge_timeout_seconds: Some(600),
            judge_temperature: Some(3.0),
            expect_server_hash: None,
            verify_manifest: None,
            config: None,
        };

        let merged = base.with_overrides(&cli);

        // CLI wins
        assert_eq!(merged.scenario_dir, PathBuf::from("/cli/scenarios"));
        assert_eq!(merged.timeout_ms, 120_000);
        assert!(merged.judge_enabled);
        assert!(merged.parallel);
        assert!(!merged.warmup);
        assert_eq!(merged.cql_seeds, "cli-host:9042");
        assert_eq!(merged.retrieval_k, 12);
        assert_eq!(merged.judge_provider, "mock");
        assert_eq!(merged.judge_url, "http://mock.local");
        assert_eq!(merged.judge_model, "mock-judge");
        assert_eq!(merged.judge_token_env.as_deref(), Some("TOKEN_ENV"));
        assert_eq!(merged.judge_timeout_seconds, 300);
        assert_eq!(merged.judge_temperature, 2.0);
        assert_eq!(
            merged.tenant_id,
            Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
        );
        assert_eq!(merged.preflight_timeout_ms, 200);

        // TOML wins (CLI was None)
        assert_eq!(merged.output_dir, PathBuf::from("eval-results/"));
        assert!((merged.passing_threshold - 0.8).abs() < f64::EPSILON);
        assert!(!merged.stability_canary);
        assert_eq!(merged.mcp_binary, PathBuf::from("ferrosa-memory-mcp"));
    }

    // ── T6: CLI overrides on top of defaults ────────────────────────────

    #[test]
    fn cli_overrides_on_defaults() {
        let base = EvalConfig::default();
        let cli = EvalCliOverrides {
            scenario_dir: None,
            output_dir: Some(PathBuf::from("/custom/output")),
            timeout_ms: None,
            passing_threshold: Some(0.5),
            judge_enabled: None,
            parallel: None,
            stability_canary: Some(true),
            warmup: None,
            mcp_binary: Some(PathBuf::from("/bin/mcp")),
            cql_seeds: None,
            tenant_id: None,
            preflight_timeout_ms: None,
            transport: None,
            mcp_url: None,
            max_parallel: None,
            retrieval_k: None,
            judge_provider: None,
            judge_url: None,
            judge_model: None,
            judge_token_env: None,
            judge_timeout_seconds: None,
            judge_temperature: None,
            expect_server_hash: None,
            verify_manifest: None,
            config: None,
        };

        let merged = base.with_overrides(&cli);

        assert_eq!(merged.output_dir, PathBuf::from("/custom/output"));
        assert!((merged.passing_threshold - 0.5).abs() < f64::EPSILON);
        assert!(merged.stability_canary);
        assert_eq!(merged.mcp_binary, PathBuf::from("/bin/mcp"));

        // Untouched defaults
        assert_eq!(merged.scenario_dir, PathBuf::from("scenarios/"));
        assert_eq!(merged.timeout_ms, 30_000);
        assert!(!merged.judge_enabled);
        assert!(!merged.parallel);
        assert!(merged.warmup);
        assert_eq!(merged.cql_seeds, "127.0.0.1:9042");
    }

    // ── T7: Invalid TOML produces an error ──────────────────────────────

    #[test]
    fn invalid_toml_returns_error() {
        let bad = "this is not [valid toml";
        assert!(parse_eval_toml(bad).is_err());
    }

    // ── T8: Wrong types in [eval] produce an error ──────────────────────

    #[test]
    fn wrong_type_returns_error() {
        let toml_str = r#"
[eval]
timeout_ms = "not a number"
"#;
        assert!(parse_eval_toml(toml_str).is_err());
    }

    // ── T9: EvalConfig Default matches EvalToml Default ─────────────────

    #[test]
    fn eval_config_default_matches_eval_toml_default() {
        let from_toml: EvalConfig = EvalToml::default().into();
        let direct = EvalConfig::default();
        assert_eq!(from_toml, direct);
    }

    // ── T10: [eval] alongside other sections is fine ────────────────────

    #[test]
    fn eval_coexists_with_other_sections() {
        let toml_str = r#"
[server]
transport = "stdio"

[ferrosa]
contact_points = ["localhost:9042"]

[eval]
timeout_ms = 10000
judge_enabled = true
mcp_binary = "/opt/mcp"
"#;
        let cfg: EvalConfig = parse_eval_toml(toml_str).unwrap().into();

        assert_eq!(cfg.timeout_ms, 10_000);
        assert!(cfg.judge_enabled);
        assert_eq!(cfg.mcp_binary, PathBuf::from("/opt/mcp"));
        // Rest defaults
        assert_eq!(cfg.scenario_dir, PathBuf::from("scenarios/"));
    }
}
