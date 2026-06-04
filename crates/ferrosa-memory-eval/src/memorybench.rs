//! MemoryBench-style continual-learning fixtures.
//!
//! This module adapts MemoryBench's core idea to fmem CI: feed a memory system
//! conversations with explicit/implicit feedback, then test whether later
//! requests retrieve the relevant procedural/declarative memories. The fixture
//! format is intentionally local and serializable so we can run small PR gates,
//! generate synthetic cases, and later map the same cases to live MCP calls.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::fixture::{CorpusDocument, FixtureHit, FixtureRetriever, tokenize};
use crate::memory_quality::{
    EvidenceGroundTruth, EvidenceHit, MemoryEvalMetrics, evaluate_retrieval,
};

/// One turn in a synthetic or corpus-backed conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub speaker: String,
    pub content: String,
}

impl ConversationTurn {
    pub fn new(speaker: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            speaker: speaker.into(),
            content: content.into(),
        }
    }
}

/// Feedback signal used as procedural memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSignal {
    Verbose { critique: String },
    Like,
    Dislike,
    Copy,
}

/// A conversation that can be ingested as benchmark memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConversation {
    pub id: String,
    #[serde(default)]
    pub turns: Vec<ConversationTurn>,
    #[serde(default)]
    pub feedback: Vec<FeedbackSignal>,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
}

impl MemoryConversation {
    pub fn as_document(&self) -> CorpusDocument {
        let mut text = String::new();
        for turn in &self.turns {
            text.push_str(&turn.speaker);
            text.push_str(": ");
            text.push_str(&turn.content);
            text.push('\n');
        }
        for feedback in &self.feedback {
            match feedback {
                FeedbackSignal::Verbose { critique } => {
                    text.push_str("feedback: ");
                    text.push_str(critique);
                    text.push('\n');
                }
                FeedbackSignal::Like => text.push_str("feedback: user liked the answer\n"),
                FeedbackSignal::Dislike => text.push_str("feedback: user disliked the answer\n"),
                FeedbackSignal::Copy => text.push_str("feedback: user copied the answer\n"),
            }
        }

        let mut doc = CorpusDocument::new(self.id.clone(), text);
        doc.metadata
            .insert("source".to_string(), "memorybench_conversation".to_string());
        doc
    }
}

/// One MemoryBench-style test case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBenchCase {
    pub id: String,
    pub query: String,
    pub expected_answer_terms: Vec<String>,
    pub ground_truth: EvidenceGroundTruth,
}

/// A self-contained MemoryBench-style fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBenchFixture {
    pub id: String,
    #[serde(default)]
    pub static_corpus: Vec<CorpusDocument>,
    #[serde(default)]
    pub training_conversations: Vec<MemoryConversation>,
    #[serde(default)]
    pub synthetic_conversations: Vec<MemoryConversation>,
    #[serde(default)]
    pub cases: Vec<MemoryBenchCase>,
}

impl MemoryBenchFixture {
    pub fn corpus_documents(&self) -> Vec<CorpusDocument> {
        self.static_corpus
            .iter()
            .cloned()
            .chain(
                self.training_conversations
                    .iter()
                    .map(MemoryConversation::as_document),
            )
            .chain(
                self.synthetic_conversations
                    .iter()
                    .map(MemoryConversation::as_document),
            )
            .collect()
    }
}

/// Per-case MemoryBench-style score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryBenchCaseResult {
    pub case_id: String,
    pub hits: Vec<FixtureHit>,
    pub retrieval: MemoryEvalMetrics,
    pub answer_term_recall: f64,
    pub feedback_gain: f64,
}

/// Aggregate fixture score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryBenchResult {
    pub fixture_id: String,
    pub cases: Vec<MemoryBenchCaseResult>,
    pub mean_recall_at_k: f64,
    pub mean_answer_term_recall: f64,
    pub mean_feedback_gain: f64,
}

pub fn run_memorybench_fixture<R: FixtureRetriever>(
    fixture: &MemoryBenchFixture,
    retriever: &R,
    k: usize,
) -> MemoryBenchResult {
    let cases = fixture
        .cases
        .iter()
        .map(|case| run_memorybench_case(case, retriever, k))
        .collect::<Vec<_>>();
    let mean_recall_at_k = mean(cases.iter().map(|case| case.retrieval.recall_at_k));
    let mean_answer_term_recall = mean(cases.iter().map(|case| case.answer_term_recall));
    let mean_feedback_gain = mean(cases.iter().map(|case| case.feedback_gain));

    MemoryBenchResult {
        fixture_id: fixture.id.clone(),
        cases,
        mean_recall_at_k,
        mean_answer_term_recall,
        mean_feedback_gain,
    }
}

fn run_memorybench_case<R: FixtureRetriever>(
    case: &MemoryBenchCase,
    retriever: &R,
    k: usize,
) -> MemoryBenchCaseResult {
    let hits = retriever.retrieve(&case.query, k);
    let evidence_hits = hits
        .iter()
        .map(|hit| EvidenceHit::new(hit.id.clone()))
        .collect::<Vec<_>>();
    let retrieval = evaluate_retrieval(&case.ground_truth, &evidence_hits, k);
    let answer_term_recall = answer_term_recall(&case.expected_answer_terms, &hits);
    let feedback_gain = (answer_term_recall - no_memory_answer_term_recall(case)).max(0.0);

    MemoryBenchCaseResult {
        case_id: case.id.clone(),
        hits,
        retrieval,
        answer_term_recall,
        feedback_gain,
    }
}

fn answer_term_recall(expected_terms: &[String], hits: &[FixtureHit]) -> f64 {
    if expected_terms.is_empty() {
        return 1.0;
    }
    let expected = expected_terms
        .iter()
        .map(|term| term.to_lowercase())
        .collect::<HashSet<_>>();
    let retrieved = hits
        .iter()
        .flat_map(|hit| {
            tokenize(&format!("{} {}", hit.id, hit.text))
                .into_iter()
                .collect::<Vec<_>>()
        })
        .collect::<HashSet<_>>();
    let covered = expected.intersection(&retrieved).count();
    covered as f64 / expected.len() as f64
}

fn no_memory_answer_term_recall(_case: &MemoryBenchCase) -> f64 {
    0.0
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let mut count = 0usize;
    let mut total = 0.0;
    for value in values {
        count += 1;
        total += value;
    }
    if count == 0 {
        0.0
    } else {
        total / count as f64
    }
}

/// Parameters for deterministic synthetic two-agent conversation generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticConversationSpec {
    pub topic: String,
    pub preference: String,
    pub correction: String,
    pub evidence_id: String,
}

impl SyntheticConversationSpec {
    pub fn deterministic(topic: impl Into<String>, idx: usize) -> Self {
        let topic = topic.into();
        Self {
            topic: topic.clone(),
            preference: format!("{topic} should prefer concrete implementation details"),
            correction: format!("{topic} answers must cite exact files and avoid vague summaries"),
            evidence_id: format!("synthetic:{topic}:{idx}").replace(' ', "_"),
        }
    }
}

/// Local LLM config for generating more varied conversations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalLlmConfig {
    pub base_url: String,
    pub model: String,
    pub temperature: f64,
}

impl Default for LocalLlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434".to_string(),
            model: "qwen3.5:27b".to_string(),
            temperature: 0.9,
        }
    }
}

/// Generate a two-agent conversation. Uses a local Ollama-compatible model
/// when available, otherwise falls back to deterministic synthetic turns.
pub async fn generate_two_agent_conversation(
    spec: &SyntheticConversationSpec,
    llm: Option<&LocalLlmConfig>,
) -> MemoryConversation {
    if let Some(llm) = llm
        && let Ok(generated) = try_generate_with_ollama(spec, llm).await
    {
        return generated;
    }
    deterministic_two_agent_conversation(spec)
}

fn deterministic_two_agent_conversation(spec: &SyntheticConversationSpec) -> MemoryConversation {
    MemoryConversation {
        id: spec.evidence_id.clone(),
        evidence_ids: vec![spec.evidence_id.clone()],
        turns: vec![
            ConversationTurn::new(
                "agent_a",
                format!("For {}, I would answer with a broad overview.", spec.topic),
            ),
            ConversationTurn::new(
                "agent_b",
                format!("User feedback says: {}.", spec.preference),
            ),
            ConversationTurn::new(
                "agent_a",
                format!("Correction accepted: {}.", spec.correction),
            ),
        ],
        feedback: vec![
            FeedbackSignal::Dislike,
            FeedbackSignal::Verbose {
                critique: spec.correction.clone(),
            },
        ],
    }
}

async fn try_generate_with_ollama(
    spec: &SyntheticConversationSpec,
    llm: &LocalLlmConfig,
) -> anyhow::Result<MemoryConversation> {
    let client = reqwest::Client::new();
    let prompt = format!(
        "Create a concise two-agent conversation about {topic}. Agent A gives an initial answer, \
         Agent B provides user feedback, Agent A corrects course. Include this durable preference \
         verbatim: {preference}. Include this correction verbatim: {correction}. Return plain text.",
        topic = spec.topic,
        preference = spec.preference,
        correction = spec.correction,
    );
    let response: serde_json::Value = client
        .post(format!(
            "{}/api/generate",
            llm.base_url.trim_end_matches('/')
        ))
        .json(&serde_json::json!({
            "model": llm.model,
            "prompt": prompt,
            "stream": false,
            "options": {
                "temperature": llm.temperature
            }
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let text = response
        .get("response")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if text.trim().is_empty() {
        anyhow::bail!("local LLM returned empty synthetic conversation");
    }

    Ok(MemoryConversation {
        id: spec.evidence_id.clone(),
        evidence_ids: vec![spec.evidence_id.clone()],
        turns: vec![ConversationTurn::new("local_llm", text.to_string())],
        feedback: vec![FeedbackSignal::Verbose {
            critique: spec.correction.clone(),
        }],
    })
}

pub fn synthetic_memorybench_fixture(topic: &str, count: usize) -> MemoryBenchFixture {
    let synthetic_conversations = (0..count)
        .map(|idx| {
            deterministic_two_agent_conversation(&SyntheticConversationSpec::deterministic(
                topic, idx,
            ))
        })
        .collect::<Vec<_>>();
    let evidence_id = synthetic_conversations
        .first()
        .map(|conversation| conversation.id.clone())
        .unwrap_or_else(|| format!("synthetic:{topic}:0").replace(' ', "_"));

    MemoryBenchFixture {
        id: format!("memorybench-synthetic-{topic}").replace(' ', "_"),
        static_corpus: Vec::new(),
        training_conversations: Vec::new(),
        synthetic_conversations,
        cases: vec![MemoryBenchCase {
            id: "retrieve-synthetic-preference".into(),
            query: format!("{topic} exact files concrete implementation details"),
            expected_answer_terms: vec!["synthetic".into(), topic.to_lowercase().replace(' ', "_")],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::LexicalFixtureRetriever;

    #[tokio::test]
    async fn deterministic_two_agent_generation_produces_retrievable_memory() {
        let spec = SyntheticConversationSpec::deterministic("BRIGHT-Pro", 0);
        let conversation = generate_two_agent_conversation(&spec, None).await;

        assert_eq!(conversation.turns.len(), 3);
        assert!(conversation.as_document().text.contains(&spec.correction));
    }

    #[test]
    fn memorybench_fixture_retrieves_from_additional_synthetic_conversations() {
        let fixture = synthetic_memorybench_fixture("BRIGHT-Pro", 3);
        let retriever = LexicalFixtureRetriever::new(fixture.corpus_documents());

        let result = run_memorybench_fixture(&fixture, &retriever, 3);

        assert_eq!(result.cases.len(), 1);
        assert_eq!(result.cases[0].retrieval.required_hits, 1);
        assert_eq!(result.mean_recall_at_k, 1.0);
        assert!(result.mean_feedback_gain > 0.0);
    }
}
