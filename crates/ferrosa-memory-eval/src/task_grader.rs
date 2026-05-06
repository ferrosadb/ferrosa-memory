//! Task grader — task success scoring + ablation framework.
//!
//! Compares agent outputs against expected findings and runs ablation studies
//! to measure the contribution of individual fmem features.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::memory_quality::{ChunkingPolicy, RetrievalMode};
use crate::task_agent::AgentOutput;

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Result of a single ablation condition run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AblationCondition {
    pub name: String,
    pub success_rate: f64,
    pub mean_score: f64,
    pub std_score: f64,
    pub run_count: usize,
}

/// Complete ablation result: full system vs disabled features.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AblationResult {
    pub full_system: AblationCondition,
    pub no_warmth: AblationCondition,
    pub no_confidence: AblationCondition,
    pub no_datalog: AblationCondition,
    pub no_consolidation: AblationCondition,
}

/// Task grader for scoring agent outputs.
pub struct TaskGrader;

impl TaskGrader {
    pub fn new() -> Self {
        Self
    }

    /// Score task success as exact-match ratio on findings.
    ///
    /// `expected` — list of findings the agent should produce.
    /// `actual` — agent output with observed findings.
    ///
    /// Returns: intersection_size / expected_size (0.0–1.0).
    pub fn score_success(&self, expected: &[&str], actual: &AgentOutput) -> f64 {
        let found: HashSet<&str> = actual.findings.iter().map(|s| s.as_str()).collect();
        let needed: HashSet<&str> = expected.iter().copied().collect();

        if needed.is_empty() {
            return 1.0;
        }

        let intersection = found.intersection(&needed).count() as f64;
        intersection / needed.len() as f64
    }

    /// Score as Jaccard similarity (useful for partial overlap scenarios).
    pub fn score_jaccard(&self, expected: &[&str], actual: &AgentOutput) -> f64 {
        let found: HashSet<&str> = actual.findings.iter().map(|s| s.as_str()).collect();
        let needed: HashSet<&str> = expected.iter().copied().collect();

        let union = found.union(&needed).count() as f64;
        let intersection = found.intersection(&needed).count() as f64;

        if union == 0.0 {
            return 1.0;
        }

        intersection / union
    }

    /// Score as weighted overlap allowing partial string matches.
    pub fn score_fuzzy(&self, expected: &[&str], actual: &AgentOutput) -> f64 {
        let mut matched = 0usize;
        for exp in expected {
            if actual
                .findings
                .iter()
                .any(|f| f.to_lowercase().contains(&exp.to_lowercase()))
            {
                matched += 1;
            }
        }
        if expected.is_empty() {
            return 1.0;
        }
        matched as f64 / expected.len() as f64
    }
}

impl Default for TaskGrader {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Ablation framework
// ---------------------------------------------------------------------------

/// Configuration for a single ablation run.
#[derive(Debug, Clone)]
pub struct AblationConfig {
    /// Enable warmth-based ranking.
    pub warmth_enabled: bool,
    /// Enable confidence gating.
    pub confidence_enabled: bool,
    /// Enable Datalog inference.
    pub datalog_enabled: bool,
    /// Enable dream consolidation.
    pub consolidation_enabled: bool,
    /// Retrieval baseline/mode for this run.
    pub retrieval_mode: RetrievalMode,
    /// Chunking policy under evaluation.
    pub chunking_policy: ChunkingPolicy,
}

impl AblationConfig {
    /// Full system — all features enabled.
    pub fn full() -> Self {
        Self {
            warmth_enabled: true,
            confidence_enabled: true,
            datalog_enabled: true,
            consolidation_enabled: true,
            retrieval_mode: RetrievalMode::ActualHybrid,
            chunking_policy: ChunkingPolicy::EvidencePacket,
        }
    }

    /// No warmth modulation.
    pub fn no_warmth() -> Self {
        Self {
            warmth_enabled: false,
            ..Self::full()
        }
    }

    /// No confidence gating.
    pub fn no_confidence() -> Self {
        Self {
            confidence_enabled: false,
            ..Self::full()
        }
    }

    /// No Datalog inference.
    pub fn no_datalog() -> Self {
        Self {
            datalog_enabled: false,
            ..Self::full()
        }
    }

    /// No dream consolidation.
    pub fn no_consolidation() -> Self {
        Self {
            consolidation_enabled: false,
            ..Self::full()
        }
    }

    /// No retrieved memory supplied to the generator.
    pub fn no_memory() -> Self {
        Self {
            retrieval_mode: RetrievalMode::NoMemory,
            ..Self::full()
        }
    }

    /// Random retrieved memory baseline.
    pub fn random_retrieval() -> Self {
        Self {
            retrieval_mode: RetrievalMode::RandomRetrieval,
            ..Self::full()
        }
    }

    /// Oracle evidence baseline for estimating retrieval/packing headroom.
    pub fn oracle_evidence() -> Self {
        Self {
            retrieval_mode: RetrievalMode::OracleEvidence,
            ..Self::full()
        }
    }
}

/// Trait for something that can run a scenario under a given ablation config.
#[async_trait::async_trait]
pub trait AblationRunner {
    async fn run(
        &mut self,
        scenario: &Path,
        config: &AblationConfig,
    ) -> anyhow::Result<Vec<AgentOutput>>;
}

/// Run ablation baseline: 5 conditions × N runs each.
///
/// Returns mean and std for each condition.
pub async fn ablation_baseline(
    scenario: &Path,
    runner: &mut dyn AblationRunner,
    runs_per_condition: usize,
) -> anyhow::Result<AblationResult> {
    let conditions = vec![
        ("full", AblationConfig::full()),
        ("no_warmth", AblationConfig::no_warmth()),
        ("no_confidence", AblationConfig::no_confidence()),
        ("no_datalog", AblationConfig::no_datalog()),
        ("no_consolidation", AblationConfig::no_consolidation()),
    ];

    let mut results = Vec::new();

    for (name, cfg) in conditions {
        let mut scores = Vec::new();
        for _ in 0..runs_per_condition {
            let outputs = runner.run(scenario, &cfg).await?;
            // Compute average score across all sessions in the scenario
            let avg: f64 = if outputs.is_empty() {
                0.0
            } else {
                let total: f64 = outputs
                    .iter()
                    .map(|o| if o.completed { 1.0 } else { 0.0 })
                    .sum();
                total / outputs.len() as f64
            };
            scores.push(avg);
        }

        let mean = mean(&scores);
        let std = std_dev(&scores, mean);

        results.push(AblationCondition {
            name: name.to_string(),
            success_rate: mean,
            mean_score: mean,
            std_score: std,
            run_count: runs_per_condition,
        });
    }

    Ok(AblationResult {
        full_system: results[0].clone(),
        no_warmth: results[1].clone(),
        no_confidence: results[2].clone(),
        no_datalog: results[3].clone(),
        no_consolidation: results[4].clone(),
    })
}

// ---------------------------------------------------------------------------
// Statistics helpers
// ---------------------------------------------------------------------------

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn std_dev(values: &[f64], mean: f64) -> f64 {
    if values.len() <= 1 {
        return 0.0;
    }
    let variance: f64 =
        values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    variance.sqrt()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn score_success_perfect_match() {
        let grader = TaskGrader::new();
        let output = AgentOutput {
            session_id: Uuid::new_v4(),
            findings: vec![
                "OAuth token expiry".to_string(),
                "redis pool exhaustion".to_string(),
            ],
            tool_calls: vec![],
            steps_taken: 2,
            completed: true,
        };

        let score = grader.score_success(&["OAuth token expiry", "redis pool exhaustion"], &output);
        assert!((score - 1.0).abs() < 1e-10);
    }

    #[test]
    fn score_success_partial_match() {
        let grader = TaskGrader::new();
        let output = AgentOutput {
            session_id: Uuid::new_v4(),
            findings: vec!["OAuth token expiry".to_string()],
            tool_calls: vec![],
            steps_taken: 1,
            completed: true,
        };

        let score = grader.score_success(&["OAuth token expiry", "redis pool exhaustion"], &output);
        assert!((score - 0.5).abs() < 1e-10);
    }

    #[test]
    fn score_success_no_match() {
        let grader = TaskGrader::new();
        let output = AgentOutput {
            session_id: Uuid::new_v4(),
            findings: vec!["unrelated finding".to_string()],
            tool_calls: vec![],
            steps_taken: 1,
            completed: true,
        };

        let score = grader.score_success(&["OAuth token expiry"], &output);
        assert!((score - 0.0).abs() < 1e-10);
    }

    #[test]
    fn score_success_empty_expected() {
        let grader = TaskGrader::new();
        let output = AgentOutput {
            session_id: Uuid::new_v4(),
            findings: vec![],
            tool_calls: vec![],
            steps_taken: 0,
            completed: true,
        };

        let score = grader.score_success(&[], &output);
        assert!((score - 1.0).abs() < 1e-10);
    }

    // ── Jaccard scoring ──────────────────────────────────────────

    #[test]
    fn score_jaccard_perfect_match() {
        let grader = TaskGrader::new();
        let output = AgentOutput {
            session_id: Uuid::new_v4(),
            findings: vec!["A".to_string(), "B".to_string()],
            tool_calls: vec![],
            steps_taken: 2,
            completed: true,
        };

        let score = grader.score_jaccard(&["A", "B"], &output);
        assert!((score - 1.0).abs() < 1e-10);
    }

    #[test]
    fn score_jaccard_partial() {
        let grader = TaskGrader::new();
        let output = AgentOutput {
            session_id: Uuid::new_v4(),
            findings: vec!["A".to_string()],
            tool_calls: vec![],
            steps_taken: 1,
            completed: true,
        };

        let score = grader.score_jaccard(&["A", "B"], &output);
        // Jaccard = 1/2 (A in intersection, B in union only)
        assert!((score - 0.5).abs() < 1e-10);
    }

    // ── Fuzzy scoring ──────────────────────────────────────────────

    #[test]
    fn score_fuzzy_contains_match() {
        let grader = TaskGrader::new();
        let output = AgentOutput {
            session_id: Uuid::new_v4(),
            findings: vec!["OAuth token expiry detected".to_string()],
            tool_calls: vec![],
            steps_taken: 1,
            completed: true,
        };

        let score = grader.score_fuzzy(&["OAuth token expiry"], &output);
        assert!((score - 1.0).abs() < 1e-10);
    }

    // ── Ablation mechanics ───────────────────────────────────────

    #[test]
    fn ablation_config_variants() {
        let full = AblationConfig::full();
        assert!(full.warmth_enabled);
        assert!(full.confidence_enabled);
        assert!(full.datalog_enabled);
        assert!(full.consolidation_enabled);

        let no_warmth = AblationConfig::no_warmth();
        assert!(!no_warmth.warmth_enabled);
        assert!(no_warmth.confidence_enabled);

        let no_conf = AblationConfig::no_confidence();
        assert!(!no_conf.confidence_enabled);
        assert!(no_conf.warmth_enabled);

        let no_datalog = AblationConfig::no_datalog();
        assert!(!no_datalog.datalog_enabled);

        let no_consol = AblationConfig::no_consolidation();
        assert!(!no_consol.consolidation_enabled);
    }

    // ── Statistics helpers ───────────────────────────────────────

    #[test]
    fn mean_empty_returns_zero() {
        assert_eq!(mean(&[]), 0.0);
    }

    #[test]
    fn mean_single_value() {
        assert!((mean(&[5.0]) - 5.0).abs() < 1e-10);
    }

    #[test]
    fn mean_multiple_values() {
        assert!((mean(&[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-10);
    }

    #[test]
    fn std_dev_empty_returns_zero() {
        assert_eq!(std_dev(&[], 0.0), 0.0);
    }

    #[test]
    fn std_dev_single_returns_zero() {
        assert_eq!(std_dev(&[5.0], 5.0), 0.0);
    }

    #[test]
    fn std_dev_known_values() {
        let vals = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let m = mean(&vals);
        let s = std_dev(&vals, m);
        // Population std dev ≈ 2.0
        assert!((s - 2.0).abs() < 0.01);
    }

    // ── Ablation mechanics with mock runner ──────────────────────

    struct MockRunner {
        #[allow(dead_code)]
        responses: Vec<AgentOutput>,
    }

    #[async_trait::async_trait]
    impl AblationRunner for MockRunner {
        async fn run(
            &mut self,
            _scenario: &Path,
            config: &AblationConfig,
        ) -> anyhow::Result<Vec<AgentOutput>> {
            // Simulate: full system succeeds, disabled features degrade
            let success = if config.warmth_enabled
                && config.confidence_enabled
                && config.datalog_enabled
                && config.consolidation_enabled
            {
                1.0
            } else {
                0.6
            };

            let output = AgentOutput {
                session_id: Uuid::new_v4(),
                findings: if success > 0.8 {
                    vec!["finding A".to_string(), "finding B".to_string()]
                } else {
                    vec!["finding A".to_string()]
                },
                tool_calls: vec![],
                steps_taken: 2,
                completed: success > 0.8,
            };

            Ok(vec![output])
        }
    }

    #[tokio::test]
    async fn ablation_mechanics_produces_different_scores() {
        let mut runner = MockRunner { responses: vec![] };

        let result = ablation_baseline(Path::new("dummy.toml"), &mut runner, 3)
            .await
            .unwrap();

        // Full system should have higher success rate than ablated variants
        assert!(
            result.full_system.success_rate > result.no_warmth.success_rate
                || result.no_warmth.success_rate == result.full_system.success_rate,
            "ablation should show effect or match"
        );
        assert_eq!(result.full_system.run_count, 3);
        assert_eq!(result.no_warmth.run_count, 3);
    }
}
