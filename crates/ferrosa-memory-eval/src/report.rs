use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bright_pro::BrightProScore;
use crate::memory_quality::MemoryQualityScore;

// ---------------------------------------------------------------------------
// ANSI helpers
// ---------------------------------------------------------------------------

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

// ---------------------------------------------------------------------------
// Result types (spec section 6.2)
// ---------------------------------------------------------------------------

/// Per-step programmatic check result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgrammaticScore {
    /// Number of steps that passed schema + assertion checks.
    pub passed: usize,
    /// Total steps evaluated.
    pub total: usize,
    /// Normalized score (0.0-1.0).
    pub score: f64,
}

/// LLM judge pass/fail verdict.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JudgeVerdict {
    pub passed: bool,
    pub reasoning: String,
}

/// Claim rubric partial-credit score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimScore {
    pub claims_met: usize,
    pub claims_total: usize,
    /// Normalized score (0.0-1.0).
    pub score: f64,
}

/// Tool usage efficiency score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolUsageScore {
    pub total_calls: usize,
    pub unnecessary_calls: usize,
    pub total_tokens: u64,
    pub total_latency: Duration,
    /// Normalized efficiency score (0.0-1.0).
    pub efficiency: f64,
}

/// Level 1: Standard MCP quality scores.
/// Internal representation uses 0.0-1.0 scale.
/// Mapped to 1-5 for display only (EF25 fix).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpQualityScores {
    pub accuracy: f64,
    pub completeness: f64,
    pub relevance: f64,
    pub clarity: f64,
    pub reasoning: f64,
    /// Composite (0.0-1.0 internal).
    pub composite: f64,
}

impl McpQualityScores {
    /// Map a 0.0-1.0 score to the 1-5 display scale.
    pub fn to_display_scale(score: f64) -> f64 {
        1.0 + score * 4.0
    }
}

/// A single DIKW transition score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransitionScore {
    pub label: String,
    pub score: f64,
    pub detail: String,
}

/// Level 2: DIKW Knowledge Transformation scores.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DIKWScore {
    pub data_to_info: TransitionScore,
    pub info_to_knowledge: TransitionScore,
    pub knowledge_to_wisdom: TransitionScore,
    pub emergence: EmergenceScore,
    pub composite: f64,
}

/// Emergent relationship tracking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmergenceScore {
    pub entities_before: usize,
    pub entities_after: usize,
    pub edges_before: usize,
    pub edges_after: usize,
    pub derived_facts_created: usize,
    pub new_edge_types: Vec<String>,
    pub graph_density: f64,
    pub density_delta: f64,
    pub score: f64,
}

/// Level 3: Semantic Repository Maturity scores.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticRepoScore {
    pub inference_correctness: f64,
    pub ontological_consistency: f64,
    pub graph_completeness: f64,
    pub query_expressiveness: f64,
    pub dedup_accuracy: f64,
    pub composite: f64,
}

/// Aggregate result for one scenario.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScenarioResult {
    pub scenario_id: String,
    pub level: u8,
    pub mcp_quality: McpQualityScores,
    pub programmatic: ProgrammaticScore,
    pub judge: Option<JudgeVerdict>,
    pub claims: Option<ClaimScore>,
    pub tool_usage: ToolUsageScore,
    pub dikw: Option<DIKWScore>,
    pub semantic: Option<SemanticRepoScore>,
    #[serde(default)]
    pub memory_quality: Option<MemoryQualityScore>,
    #[serde(default)]
    pub bright_pro: Option<BrightProScore>,
    pub passed: bool,
    pub duration: Duration,
}

// ---------------------------------------------------------------------------
// Evaluation run report
// ---------------------------------------------------------------------------

/// Top-level report covering all scenarios in a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub run_timestamp: DateTime<Utc>,
    pub results: Vec<ScenarioResult>,
    pub total_duration: Duration,
    /// Configurable level weights for composite (L1, L2, L3).
    pub level_weights: [f64; 3],
}

impl EvalReport {
    /// Create a new report with default equal weights.
    pub fn new(
        run_timestamp: DateTime<Utc>,
        results: Vec<ScenarioResult>,
        total_duration: Duration,
    ) -> Self {
        Self {
            run_timestamp,
            results,
            total_duration,
            level_weights: [1.0, 1.0, 1.0],
        }
    }

    /// Create a report with custom level weights.
    pub fn with_weights(
        run_timestamp: DateTime<Utc>,
        results: Vec<ScenarioResult>,
        total_duration: Duration,
        weights: [f64; 3],
    ) -> Self {
        Self {
            run_timestamp,
            results,
            total_duration,
            level_weights: weights,
        }
    }

    // -----------------------------------------------------------------------
    // Aggregation helpers
    // -----------------------------------------------------------------------

    fn level_results(&self, level: u8) -> Vec<&ScenarioResult> {
        self.results.iter().filter(|r| r.level == level).collect()
    }

    /// Weighted composite across all levels (0.0-1.0).
    pub fn composite_score(&self) -> f64 {
        let mut total_weight = 0.0;
        let mut weighted_sum = 0.0;

        for level in 1u8..=3 {
            let results = self.level_results(level);
            if results.is_empty() {
                continue;
            }
            let avg: f64 = match level {
                1 => {
                    results.iter().map(|r| r.mcp_quality.composite).sum::<f64>()
                        / results.len() as f64
                }
                2 => {
                    let dikw_scores: Vec<f64> = results
                        .iter()
                        .filter_map(|r| r.dikw.as_ref().map(|d| d.composite))
                        .collect();
                    if dikw_scores.is_empty() {
                        0.0
                    } else {
                        dikw_scores.iter().sum::<f64>() / dikw_scores.len() as f64
                    }
                }
                3 => {
                    let sem_scores: Vec<f64> = results
                        .iter()
                        .filter_map(|r| r.semantic.as_ref().map(|s| s.composite))
                        .collect();
                    if sem_scores.is_empty() {
                        0.0
                    } else {
                        sem_scores.iter().sum::<f64>() / sem_scores.len() as f64
                    }
                }
                _ => 0.0,
            };
            let w = self.level_weights[(level - 1) as usize];
            weighted_sum += avg * w;
            total_weight += w;
        }

        if total_weight == 0.0 {
            0.0
        } else {
            weighted_sum / total_weight
        }
    }

    // -----------------------------------------------------------------------
    // JSON output
    // -----------------------------------------------------------------------

    /// Serialize to pretty JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Write JSON to `results/run-{timestamp}.json` under `output_dir`.
    /// Creates the directory if needed.
    pub fn write_json(&self, output_dir: &Path) -> anyhow::Result<PathBuf> {
        let ts = self.run_timestamp.format("%Y-%m-%dT%H-%M-%SZ");
        let filename = format!("run-{ts}.json");
        let dir = output_dir.join("results");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(filename);
        let json = self.to_json()?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    // -----------------------------------------------------------------------
    // CLI text output
    // -----------------------------------------------------------------------

    /// Render the full CLI report with ANSI color codes.
    pub fn render_cli(&self) -> String {
        let mut out = String::new();

        let total = self.results.len();
        let passed = self.results.iter().filter(|r| r.passed).count();
        let failed = total - passed;
        let dur = self.total_duration.as_secs_f64();

        // Header
        out.push_str("=== ferrosa-memory-mcp Evaluation Report ===\n");
        out.push_str(&format!(
            "Run: {}  |  Scenarios: {} run, {} passed, {} failed  |  Duration: {:.1}s\n",
            self.run_timestamp.format("%Y-%m-%dT%H:%M:%SZ"),
            total,
            passed,
            failed,
            dur,
        ));

        // Level 1
        self.render_level1(&mut out);
        // Level 2
        self.render_level2(&mut out);
        // Level 3
        self.render_level3(&mut out);
        // Memory quality
        self.render_memory_quality(&mut out);
        // BRIGHT-Pro
        self.render_bright_pro(&mut out);
        // Aggregate
        self.render_aggregate(&mut out);

        out
    }

    fn render_level1(&self, out: &mut String) {
        let results = self.level_results(1);
        if results.is_empty() {
            return;
        }

        out.push_str("\n--- Level 1: Standard MCP Metrics (target: 3.5/5.0) ---\n");

        for r in &results {
            let display_score = McpQualityScores::to_display_scale(r.mcp_quality.composite);
            let status = pass_fail_str(r.passed);
            let detail = format_l1_detail(r);

            out.push_str(&format!(
                "  {} {} {}  {:.1}/5.0  [{}]\n",
                r.scenario_id,
                dots_pad(&r.scenario_id, 25),
                status,
                display_score,
                detail,
            ));
        }
    }

    fn render_level2(&self, out: &mut String) {
        let results = self.level_results(2);
        if results.is_empty() {
            return;
        }

        out.push_str("\n--- Level 2: DIKW Knowledge Transformation (target: 0.60) ---\n");

        // Aggregate DIKW sub-scores across all L2 scenarios.
        let mut d2i_scores = Vec::new();
        let mut i2k_scores = Vec::new();
        let mut k2w_scores = Vec::new();
        let mut emergence_scores = Vec::new();
        let mut composites = Vec::new();

        for r in &results {
            if let Some(dikw) = &r.dikw {
                d2i_scores.push(dikw.data_to_info.score);
                i2k_scores.push(dikw.info_to_knowledge.score);
                k2w_scores.push(dikw.knowledge_to_wisdom.score);
                emergence_scores.push(dikw.emergence.score);
                composites.push(dikw.composite);
            }
        }

        let avg = |v: &[f64]| -> f64 {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };

        out.push_str(&format!("  Data->Info:       {:.2}\n", avg(&d2i_scores)));
        out.push_str(&format!("  Info->Knowledge:  {:.2}\n", avg(&i2k_scores)));
        out.push_str(&format!("  Knowledge->Wisdom: {:.2}\n", avg(&k2w_scores)));
        out.push_str(&format!(
            "  Emergence:        {:.2}\n",
            avg(&emergence_scores)
        ));
        out.push_str(&format!("  DIKW Composite:   {:.2}\n", avg(&composites)));
    }

    fn render_level3(&self, out: &mut String) {
        let results = self.level_results(3);
        if results.is_empty() {
            return;
        }

        out.push_str("\n--- Level 3: Semantic Repository Maturity (target: 0.60) ---\n");

        let mut inference = Vec::new();
        let mut ontology = Vec::new();
        let mut graph = Vec::new();
        let mut multi_hop = Vec::new();
        let mut dedup = Vec::new();
        let mut composites = Vec::new();

        for r in &results {
            if let Some(sem) = &r.semantic {
                inference.push(sem.inference_correctness);
                ontology.push(sem.ontological_consistency);
                graph.push(sem.graph_completeness);
                multi_hop.push(sem.query_expressiveness);
                dedup.push(sem.dedup_accuracy);
                composites.push(sem.composite);
            }
        }

        let avg = |v: &[f64]| -> f64 {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };

        out.push_str(&format!("  Inference:     {:.2}\n", avg(&inference)));
        out.push_str(&format!("  Ontology:      {:.2}\n", avg(&ontology)));
        out.push_str(&format!("  Graph:         {:.2}\n", avg(&graph)));
        out.push_str(&format!("  Multi-hop:     {:.2}\n", avg(&multi_hop)));
        out.push_str(&format!("  Dedup:         {:.2}\n", avg(&dedup)));
        out.push_str(&format!("  Semantic Composite: {:.2}\n", avg(&composites)));
    }

    fn render_memory_quality(&self, out: &mut String) {
        let results: Vec<_> = self
            .results
            .iter()
            .filter_map(|r| r.memory_quality.as_ref().map(|mq| (r, mq)))
            .collect();
        if results.is_empty() {
            return;
        }

        out.push_str("\n--- Memory Quality: Retrieval Evidence Metrics ---\n");
        for (r, mq) in results {
            out.push_str(&format!(
                "  {} {} mode={:?} chunk={:?} recall={:.2} precision={:.2} mrr={:.2} ndcg={:.2} distractors={} failure={:?}\n",
                r.scenario_id,
                dots_pad(&r.scenario_id, 25),
                mq.retrieval_mode,
                mq.chunking_policy,
                mq.metrics.recall_at_k,
                mq.metrics.precision_at_k,
                mq.metrics.mrr,
                mq.metrics.ndcg,
                mq.metrics.distractor_hits,
                mq.failure_kind,
            ));
        }
    }

    fn render_bright_pro(&self, out: &mut String) {
        let results: Vec<_> = self
            .results
            .iter()
            .filter_map(|r| r.bright_pro.as_ref().map(|bp| (r, bp)))
            .collect();
        if results.is_empty() {
            return;
        }

        out.push_str("\n--- BRIGHT-Pro: Aspect-Aware Retrieval Metrics ---\n");
        for (r, bp) in results {
            let aer = bp
                .aer
                .map(|score| format!(" aer={score:.2}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {} {} protocol={:?} alpha_ndcg={:.2} aspect_recall={:.2} rounds={} unique_docs={:.2} failure={:?}{}\n",
                r.scenario_id,
                dots_pad(&r.scenario_id, 25),
                bp.protocol,
                bp.alpha_ndcg,
                bp.aspect_recall,
                bp.rounds,
                bp.unique_doc_ratio,
                bp.failure_mode,
                aer,
            ));
        }
    }

    fn render_aggregate(&self, out: &mut String) {
        let l1 = self.level_results(1);
        let l2 = self.level_results(2);
        let l3 = self.level_results(3);

        out.push_str("\n--- Aggregate ---\n");

        // L1: display on 1-5 scale
        if !l1.is_empty() {
            let avg_mcp: f64 =
                l1.iter().map(|r| r.mcp_quality.composite).sum::<f64>() / l1.len() as f64;
            let display = McpQualityScores::to_display_scale(avg_mcp);
            let l1_passed = l1.iter().all(|r| r.passed);
            out.push_str(&format!(
                "  MCP Quality:     {:.1}/5.0  {}\n",
                display,
                pass_fail_str(l1_passed)
            ));
        }

        // L2: display on 0-1 scale
        if !l2.is_empty() {
            let avg_dikw: f64 = l2
                .iter()
                .filter_map(|r| r.dikw.as_ref().map(|d| d.composite))
                .sum::<f64>()
                / l2.iter().filter(|r| r.dikw.is_some()).count().max(1) as f64;
            let l2_passed = l2.iter().all(|r| r.passed);
            out.push_str(&format!(
                "  DIKW:            {:.2}     {}\n",
                avg_dikw,
                pass_fail_str(l2_passed)
            ));
        }

        // L3: display on 0-1 scale
        if !l3.is_empty() {
            let avg_sem: f64 = l3
                .iter()
                .filter_map(|r| r.semantic.as_ref().map(|s| s.composite))
                .sum::<f64>()
                / l3.iter().filter(|r| r.semantic.is_some()).count().max(1) as f64;
            let l3_passed = l3.iter().all(|r| r.passed);
            out.push_str(&format!(
                "  Semantic Repo:   {:.2}     {}\n",
                avg_sem,
                pass_fail_str(l3_passed)
            ));
        }

        // Totals across all scenarios
        let total_tokens: u64 = self.results.iter().map(|r| r.tool_usage.total_tokens).sum();
        let total_latency: Duration = self
            .results
            .iter()
            .map(|r| r.tool_usage.total_latency)
            .sum();

        out.push_str(&format!(
            "  Total tokens:    {}\n",
            format_tokens(total_tokens)
        ));
        out.push_str(&format!(
            "  Total latency:   {:.1}s\n",
            total_latency.as_secs_f64()
        ));
    }
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn pass_fail_str(passed: bool) -> String {
    if passed {
        format!("{GREEN}PASS{RESET}")
    } else {
        format!("{RED}FAIL{RESET}")
    }
}

/// Right-pad scenario name with dots to reach `width` columns.
fn dots_pad(name: &str, width: usize) -> String {
    let padding = width.saturating_sub(name.len());
    if padding < 2 {
        " ".to_string()
    } else {
        format!(" {}", ".".repeat(padding - 1))
    }
}

fn format_l1_detail(r: &ScenarioResult) -> String {
    let mut parts = Vec::new();
    parts.push(format!("prog:{:.2}", r.programmatic.score));
    if let Some(claims) = &r.claims {
        parts.push(format!("claims:{:.2}", claims.score));
    }
    if let Some(judge) = &r.judge {
        let verdict = if judge.passed { "PASS" } else { "FAIL" };
        parts.push(format!("judge:{verdict}"));
    }
    parts.push(format!("eff:{:.2}", r.tool_usage.efficiency));
    parts.join(" ")
}

/// Format token counts with comma separators.
fn format_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::time::Duration;

    // -- Fixtures --

    fn mock_l1_pass(id: &str, composite: f64) -> ScenarioResult {
        ScenarioResult {
            scenario_id: id.to_string(),
            level: 1,
            mcp_quality: McpQualityScores {
                accuracy: composite,
                completeness: composite,
                relevance: composite,
                clarity: composite,
                reasoning: composite,
                composite,
            },
            programmatic: ProgrammaticScore {
                passed: 3,
                total: 3,
                score: 1.0,
            },
            judge: Some(JudgeVerdict {
                passed: true,
                reasoning: "All checks passed.".into(),
            }),
            claims: Some(ClaimScore {
                claims_met: 4,
                claims_total: 4,
                score: 1.0,
            }),
            tool_usage: ToolUsageScore {
                total_calls: 5,
                unnecessary_calls: 0,
                total_tokens: 2500,
                total_latency: Duration::from_millis(800),
                efficiency: 0.95,
            },
            dikw: None,
            semantic: None,
            memory_quality: None,
            bright_pro: None,
            passed: true,
            duration: Duration::from_secs(3),
        }
    }

    fn mock_l1_fail(id: &str, composite: f64) -> ScenarioResult {
        ScenarioResult {
            scenario_id: id.to_string(),
            level: 1,
            mcp_quality: McpQualityScores {
                accuracy: composite,
                completeness: 0.4,
                relevance: composite,
                clarity: composite,
                reasoning: composite,
                composite,
            },
            programmatic: ProgrammaticScore {
                passed: 2,
                total: 3,
                score: 0.67,
            },
            judge: None,
            claims: Some(ClaimScore {
                claims_met: 2,
                claims_total: 4,
                score: 0.5,
            }),
            tool_usage: ToolUsageScore {
                total_calls: 8,
                unnecessary_calls: 3,
                total_tokens: 3500,
                total_latency: Duration::from_millis(1200),
                efficiency: 0.62,
            },
            dikw: None,
            semantic: None,
            memory_quality: None,
            bright_pro: None,
            passed: false,
            duration: Duration::from_secs(5),
        }
    }

    fn mock_l2(id: &str) -> ScenarioResult {
        ScenarioResult {
            scenario_id: id.to_string(),
            level: 2,
            mcp_quality: McpQualityScores {
                accuracy: 0.8,
                completeness: 0.8,
                relevance: 0.8,
                clarity: 0.8,
                reasoning: 0.8,
                composite: 0.8,
            },
            programmatic: ProgrammaticScore {
                passed: 4,
                total: 4,
                score: 1.0,
            },
            judge: None,
            claims: None,
            tool_usage: ToolUsageScore {
                total_calls: 6,
                unnecessary_calls: 0,
                total_tokens: 3200,
                total_latency: Duration::from_millis(900),
                efficiency: 0.90,
            },
            dikw: Some(DIKWScore {
                data_to_info: TransitionScore {
                    label: "Data->Info".into(),
                    score: 0.85,
                    detail: "types:6/7".into(),
                },
                info_to_knowledge: TransitionScore {
                    label: "Info->Knowledge".into(),
                    score: 0.78,
                    detail: "consolidation:4 edges".into(),
                },
                knowledge_to_wisdom: TransitionScore {
                    label: "Knowledge->Wisdom".into(),
                    score: 0.70,
                    detail: "intentions:2/3".into(),
                },
                emergence: EmergenceScore {
                    entities_before: 5,
                    entities_after: 12,
                    edges_before: 3,
                    edges_after: 18,
                    derived_facts_created: 4,
                    new_edge_types: vec!["CO_OCCURS".into(), "SUPERSEDES".into()],
                    graph_density: 0.12,
                    density_delta: 0.09,
                    score: 0.65,
                },
                composite: 0.75,
            }),
            semantic: None,
            memory_quality: None,
            bright_pro: None,
            passed: true,
            duration: Duration::from_secs(8),
        }
    }

    fn mock_l3(id: &str) -> ScenarioResult {
        ScenarioResult {
            scenario_id: id.to_string(),
            level: 3,
            mcp_quality: McpQualityScores {
                accuracy: 0.85,
                completeness: 0.85,
                relevance: 0.85,
                clarity: 0.85,
                reasoning: 0.85,
                composite: 0.85,
            },
            programmatic: ProgrammaticScore {
                passed: 5,
                total: 5,
                score: 1.0,
            },
            judge: None,
            claims: None,
            tool_usage: ToolUsageScore {
                total_calls: 10,
                unnecessary_calls: 1,
                total_tokens: 3647,
                total_latency: Duration::from_millis(1100),
                efficiency: 0.88,
            },
            dikw: None,
            semantic: Some(SemanticRepoScore {
                inference_correctness: 0.80,
                ontological_consistency: 0.90,
                graph_completeness: 0.65,
                query_expressiveness: 0.70,
                dedup_accuracy: 0.75,
                composite: 0.76,
            }),
            memory_quality: None,
            bright_pro: None,
            passed: true,
            duration: Duration::from_secs(10),
        }
    }

    fn test_timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 5, 14, 32, 0).unwrap()
    }

    fn make_5_scenario_report() -> EvalReport {
        let results = vec![
            mock_l1_pass("memo_cache", 0.80),
            mock_l1_pass("entity_crud", 0.875),
            mock_l1_fail("search_retrieval", 0.45),
            mock_l2("smart_ingest"),
            mock_l3("inference_correctness"),
        ];
        let total_duration = Duration::from_millis(47300);
        EvalReport::new(test_timestamp(), results, total_duration)
    }

    // -- Tests --

    #[test]
    fn test_json_round_trip() {
        let report = make_5_scenario_report();
        let json = report.to_json().expect("serialize");
        let deser: EvalReport = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deser.results.len(), 5);
        assert_eq!(deser.results[0].scenario_id, "memo_cache");
        assert_eq!(deser.results[0].level, 1);
        assert!(deser.results[0].passed);
        assert!(!deser.results[2].passed);
        assert_eq!(deser.results[3].scenario_id, "smart_ingest");
        assert!(deser.results[3].dikw.is_some());
        assert!(deser.results[4].semantic.is_some());
    }

    #[test]
    fn test_json_preserves_all_fields() {
        let report = make_5_scenario_report();
        let json = report.to_json().expect("serialize");
        let deser: EvalReport = serde_json::from_str(&json).expect("deserialize");

        // Check DIKW sub-scores round-trip
        let dikw = deser.results[3].dikw.as_ref().unwrap();
        assert!((dikw.data_to_info.score - 0.85).abs() < f64::EPSILON);
        assert!((dikw.emergence.score - 0.65).abs() < f64::EPSILON);
        assert_eq!(
            dikw.emergence.new_edge_types,
            vec!["CO_OCCURS", "SUPERSEDES"]
        );

        // Check SemanticRepoScore round-trip
        let sem = deser.results[4].semantic.as_ref().unwrap();
        assert!((sem.inference_correctness - 0.80).abs() < f64::EPSILON);
        assert!((sem.composite - 0.76).abs() < f64::EPSILON);

        // Check ToolUsageScore round-trip
        let tool = &deser.results[0].tool_usage;
        assert_eq!(tool.total_calls, 5);
        assert_eq!(tool.total_tokens, 2500);
        assert!((tool.efficiency - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_normalization_ef25() {
        // EF25: all internal scores must be 0.0-1.0.
        // MCP quality mapped to 1-5 only for display.
        let report = make_5_scenario_report();

        // Internal scores are 0.0-1.0
        for r in &report.results {
            assert!(
                r.mcp_quality.composite >= 0.0 && r.mcp_quality.composite <= 1.0,
                "MCP composite must be 0-1 internal: got {}",
                r.mcp_quality.composite
            );
        }

        // Display scale is 1-5
        let display = McpQualityScores::to_display_scale(0.0);
        assert!((display - 1.0).abs() < f64::EPSILON, "0.0 maps to 1.0");

        let display = McpQualityScores::to_display_scale(1.0);
        assert!((display - 5.0).abs() < f64::EPSILON, "1.0 maps to 5.0");

        let display = McpQualityScores::to_display_scale(0.5);
        assert!((display - 3.0).abs() < f64::EPSILON, "0.5 maps to 3.0");

        // Verify the CLI output shows 1-5 scale but composite calc uses 0-1
        let composite = report.composite_score();
        assert!(
            (0.0..=1.0).contains(&composite),
            "Composite must be 0-1: got {composite}"
        );
    }

    #[test]
    fn test_separate_pass_fail_per_level() {
        let report = make_5_scenario_report();

        // L1: 2 pass, 1 fail
        let l1_passed = report.level_results(1).iter().filter(|r| r.passed).count();
        let l1_failed = report.level_results(1).iter().filter(|r| !r.passed).count();
        assert_eq!(l1_passed, 2);
        assert_eq!(l1_failed, 1);

        // L2: 1 pass, 0 fail
        let l2_passed = report.level_results(2).iter().filter(|r| r.passed).count();
        assert_eq!(l2_passed, 1);

        // L3: 1 pass, 0 fail
        let l3_passed = report.level_results(3).iter().filter(|r| r.passed).count();
        assert_eq!(l3_passed, 1);
    }

    #[test]
    fn test_cli_output_header() {
        let report = make_5_scenario_report();
        let output = report.render_cli();

        assert!(
            output.contains("=== ferrosa-memory-mcp Evaluation Report ==="),
            "Must contain report header"
        );
        assert!(
            output.contains("Scenarios: 5 run, 4 passed, 1 failed"),
            "Must show scenario counts"
        );
        assert!(
            output.contains("Duration: 47.3s"),
            "Must show total duration"
        );
    }

    #[test]
    fn test_cli_output_level1_section() {
        let report = make_5_scenario_report();
        let output = report.render_cli();

        assert!(
            output.contains("Level 1: Standard MCP Metrics"),
            "Must contain L1 header"
        );
        assert!(output.contains("memo_cache"), "Must list L1 scenario");
        // memo_cache composite = 0.80 -> display = 4.2
        assert!(
            output.contains("4.2/5.0"),
            "Must display score on 1-5 scale: {}",
            output
        );
    }

    #[test]
    fn test_cli_output_ansi_pass_fail() {
        let report = make_5_scenario_report();
        let output = report.render_cli();

        assert!(
            output.contains(&format!("{GREEN}PASS{RESET}")),
            "Must contain green PASS"
        );
        assert!(
            output.contains(&format!("{RED}FAIL{RESET}")),
            "Must contain red FAIL"
        );
    }

    #[test]
    fn test_cli_output_level2_section() {
        let report = make_5_scenario_report();
        let output = report.render_cli();

        assert!(
            output.contains("Level 2: DIKW Knowledge Transformation"),
            "Must contain L2 header"
        );
        assert!(output.contains("Data->Info:"), "Must show DIKW sub-scores");
        assert!(
            output.contains("DIKW Composite:"),
            "Must show DIKW composite"
        );
    }

    #[test]
    fn test_cli_output_level3_section() {
        let report = make_5_scenario_report();
        let output = report.render_cli();

        assert!(
            output.contains("Level 3: Semantic Repository Maturity"),
            "Must contain L3 header"
        );
        assert!(
            output.contains("Inference:"),
            "Must show semantic sub-scores"
        );
        assert!(
            output.contains("Semantic Composite:"),
            "Must show semantic composite"
        );
    }

    #[test]
    fn test_cli_output_aggregate_section() {
        let report = make_5_scenario_report();
        let output = report.render_cli();

        assert!(
            output.contains("--- Aggregate ---"),
            "Must contain aggregate header"
        );
        assert!(
            output.contains("MCP Quality:"),
            "Must show MCP quality aggregate"
        );
        assert!(output.contains("DIKW:"), "Must show DIKW aggregate");
        assert!(
            output.contains("Semantic Repo:"),
            "Must show semantic aggregate"
        );
        assert!(output.contains("Total tokens:"), "Must show total tokens");
        assert!(output.contains("Total latency:"), "Must show total latency");
    }

    #[test]
    fn test_format_tokens_comma_separated() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(123), "123");
        assert_eq!(format_tokens(1234), "1,234");
        assert_eq!(format_tokens(12847), "12,847");
        assert_eq!(format_tokens(1000000), "1,000,000");
    }

    #[test]
    fn test_dots_pad_alignment() {
        let pad = dots_pad("memo_cache", 25);
        assert!(pad.contains("..."), "Should contain dots for padding");

        // Short name gets more dots
        let short = dots_pad("ab", 25);
        assert!(short.len() > pad.len(), "Shorter name needs more dots");

        // Very long name gets no dots
        let long = dots_pad("a_very_long_scenario_name_indeed", 25);
        assert_eq!(long, " ", "Overlong name gets single space");
    }

    #[test]
    fn test_composite_score_weighted() {
        let results = vec![mock_l1_pass("a", 0.80), mock_l2("b"), mock_l3("c")];
        let report = EvalReport::with_weights(
            test_timestamp(),
            results,
            Duration::from_secs(10),
            [1.0, 2.0, 3.0], // weight L3 heaviest
        );

        let composite = report.composite_score();
        // L1 avg = 0.80, L2 avg (dikw composite) = 0.75, L3 avg (sem composite) = 0.76
        // weighted = (0.80*1 + 0.75*2 + 0.76*3) / (1+2+3)
        let expected = (0.80 + 0.75 * 2.0 + 0.76 * 3.0) / 6.0;
        assert!(
            (composite - expected).abs() < 1e-10,
            "Weighted composite: got {composite}, expected {expected}"
        );
    }

    #[test]
    fn test_composite_score_empty() {
        let report = EvalReport::new(test_timestamp(), vec![], Duration::from_secs(0));
        assert!((report.composite_score() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_json_write_creates_file() {
        let report = make_5_scenario_report();
        let tmp = std::env::temp_dir().join("ferrosa-eval-test");
        let _ = std::fs::remove_dir_all(&tmp);

        let path = report.write_json(&tmp).expect("write_json");

        assert!(path.exists(), "JSON file should exist");
        assert!(
            path.to_string_lossy()
                .contains("run-2026-04-05T14-32-00Z.json"),
            "Filename must match spec format: {}",
            path.display()
        );

        // Verify content is valid JSON that round-trips
        let content = std::fs::read_to_string(&path).expect("read");
        let deser: EvalReport = serde_json::from_str(&content).expect("parse");
        assert_eq!(deser.results.len(), 5);

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_l1_detail_format() {
        let r = mock_l1_pass("test", 0.80);
        let detail = format_l1_detail(&r);
        assert!(detail.contains("prog:1.00"));
        assert!(detail.contains("claims:1.00"));
        assert!(detail.contains("judge:PASS"));
        assert!(detail.contains("eff:0.95"));
    }

    #[test]
    fn test_l1_detail_without_judge() {
        let r = mock_l1_fail("test", 0.45);
        let detail = format_l1_detail(&r);
        assert!(detail.contains("prog:0.67"));
        assert!(detail.contains("claims:0.50"));
        assert!(!detail.contains("judge:"), "No judge when None");
        assert!(detail.contains("eff:0.62"));
    }

    #[test]
    fn test_mcp_display_scale_boundary_values() {
        // Ensure the 0.0-1.0 -> 1.0-5.0 mapping is correct at boundaries
        assert!((McpQualityScores::to_display_scale(0.0) - 1.0).abs() < f64::EPSILON);
        assert!((McpQualityScores::to_display_scale(0.25) - 2.0).abs() < f64::EPSILON);
        assert!((McpQualityScores::to_display_scale(0.5) - 3.0).abs() < f64::EPSILON);
        assert!((McpQualityScores::to_display_scale(0.75) - 4.0).abs() < f64::EPSILON);
        assert!((McpQualityScores::to_display_scale(1.0) - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_render_cli_includes_memory_quality_section() {
        let mut result = mock_l1_pass("memory_grounded", 0.90);
        result.memory_quality = Some(crate::memory_quality::MemoryQualityScore {
            retrieval_mode: crate::memory_quality::RetrievalMode::ActualHybrid,
            chunking_policy: crate::memory_quality::ChunkingPolicy::EvidencePacket,
            metrics: crate::memory_quality::MemoryEvalMetrics {
                required_total: 2,
                required_hits: 2,
                recall_at_k: 1.0,
                precision_at_k: 0.67,
                mrr: 0.5,
                ndcg: 0.8,
                distractor_hits: 1,
            },
            failure_kind: crate::memory_quality::MemoryFailureKind::Passed,
        });
        let report = EvalReport::new(test_timestamp(), vec![result], Duration::from_secs(5));
        let output = report.render_cli();

        assert!(output.contains("Memory Quality: Retrieval Evidence Metrics"));
        assert!(output.contains("mode=ActualHybrid"));
        assert!(output.contains("chunk=EvidencePacket"));
        assert!(output.contains("recall=1.00"));
        assert!(output.contains("distractors=1"));
        assert!(output.contains("failure=Passed"));
    }

    #[test]
    fn test_render_cli_includes_bright_pro_section() {
        let mut result = mock_l1_pass("bright_grounded", 0.90);
        result.bright_pro = Some(crate::bright_pro::BrightProScore {
            protocol: crate::bright_pro::BrightProProtocol::FixedThree,
            alpha_ndcg: 0.82,
            aspect_recall: 0.75,
            rounds: 3,
            unique_doc_ratio: 0.67,
            aer: Some(0.68),
            failure_mode: crate::bright_pro::AgenticFailureMode::AspectTunnelVision,
        });
        let report = EvalReport::new(test_timestamp(), vec![result], Duration::from_secs(5));
        let output = report.render_cli();

        assert!(output.contains("BRIGHT-Pro: Aspect-Aware Retrieval Metrics"));
        assert!(output.contains("protocol=FixedThree"));
        assert!(output.contains("alpha_ndcg=0.82"));
        assert!(output.contains("aspect_recall=0.75"));
        assert!(output.contains("failure=AspectTunnelVision"));
        assert!(output.contains("aer=0.68"));
    }

    #[test]
    fn test_render_cli_with_only_l1() {
        let results = vec![mock_l1_pass("a", 0.80), mock_l1_pass("b", 0.90)];
        let report = EvalReport::new(test_timestamp(), results, Duration::from_secs(5));
        let output = report.render_cli();

        assert!(output.contains("Level 1:"));
        assert!(!output.contains("Level 2:"), "No L2 when no L2 results");
        assert!(!output.contains("Level 3:"), "No L3 when no L3 results");
        assert!(output.contains("MCP Quality:"));
        assert!(!output.contains("DIKW:"), "No DIKW aggregate without L2");
    }
}
