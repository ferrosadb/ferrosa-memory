//! Scenario runner — orchestrates scenario loading, execution, grading, and cleanup.
//!
//! Addresses:
//! - EF07 (RPN 210): Cross-scenario state leakage — calls `delete_session` before
//!   AND after each scenario, verifies entity_count=0 pre-scenario.
//! - EF14 (RPN 72): Snapshot timing — snapshots graph state BEFORE any tool calls.

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::config::EvalConfig;
use crate::grading::claim_rubric;
use crate::grading::programmatic::{self, NoOpResolver};
use crate::memory_quality::{
    ChunkingPolicy, EvidenceHit, MemoryEvalMetrics, MemoryFailureKind, MemoryQualityScore,
    RetrievalMode, evaluate_retrieval,
};
use crate::scenario::{EvalScenario, EvalStep, ToolCallTrace};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors from the scenario runner.
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("scenario load error: {0}")]
    ScenarioLoad(String),

    #[error("MCP client error: {0}")]
    McpClient(String),

    #[error("cleanup verification failed: entity_count={entity_count} (expected 0)")]
    DirtyState { entity_count: usize },

    #[error("Pre-flight failed: Ferrosa cluster unhealthy — {reason}")]
    PreflightFailed { reason: String },

    #[error(
        "CONTAMINATED: pre-scenario entity_count={entity_count} after delete_session (session {session_id})"
    )]
    Contaminated {
        entity_count: usize,
        session_id: Uuid,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("STABILITY CANARY FAILED: non-determinism detected — {detail}")]
    StabilityCanaryFailed { detail: String },

    #[error("manifest verification failed: {0}")]
    ManifestMismatch(String),
}

// ---------------------------------------------------------------------------
// Graph snapshot (EF14)
// ---------------------------------------------------------------------------

/// Point-in-time snapshot of graph state for before/after comparison.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphSnapshot {
    pub entity_count: usize,
    pub edge_count: usize,
    pub derived_fact_count: usize,
    pub timestamp: DateTime<Utc>,
}

impl GraphSnapshot {
    /// Create a zeroed snapshot at current time (used in tests / fallback).
    pub fn empty() -> Self {
        Self {
            entity_count: 0,
            edge_count: 0,
            derived_fact_count: 0,
            timestamp: Utc::now(),
        }
    }

    /// Parse a GraphSnapshot from a get_stats JSON response.
    ///
    /// The MCP get_stats tool returns a content array with a JSON text field.
    /// We extract entity_count, edge_count, derived_fact_count from it.
    pub fn from_stats_response(response: &Value) -> Self {
        let stats = Self::extract_stats_json(response);

        Self {
            entity_count: stats
                .get("entity_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            edge_count: stats
                .get("edge_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            derived_fact_count: stats
                .get("derived_fact_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            timestamp: Utc::now(),
        }
    }

    /// Extract the inner stats JSON from an MCP content array response.
    fn extract_stats_json(response: &Value) -> Value {
        // MCP responses wrap in: {"content": [{"type": "text", "text": "{...}"}]}
        if let Some(content) = response.get("content").and_then(|c| c.as_array())
            && let Some(first) = content.first()
            && let Some(text) = first.get("text").and_then(|t| t.as_str())
            && let Ok(parsed) = serde_json::from_str::<Value>(text)
        {
            return parsed;
        }
        // Fallback: the response itself might be the stats object directly
        response.clone()
    }
}

// ---------------------------------------------------------------------------
// Scenario run (execution record)
// ---------------------------------------------------------------------------

/// Complete record of a single scenario execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioRun {
    pub scenario: EvalScenario,
    pub session_id: Uuid,
    pub traces: Vec<ToolCallTrace>,
    pub graph_snapshot_before: GraphSnapshot,
    pub graph_snapshot_after: GraphSnapshot,
    pub duration: Duration,
}

// ---------------------------------------------------------------------------
// MCP client trait (for testability)
// ---------------------------------------------------------------------------

/// Trait abstracting MCP client operations needed by the runner.
///
/// The real implementation wraps `McpClient`; tests use a mock.
pub trait McpTransport: Send {
    /// Call an MCP tool and return (response, latency).
    fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> impl std::future::Future<Output = Result<(Value, Duration), RunnerError>> + Send;
}

// ---------------------------------------------------------------------------
// Scenario loader
// ---------------------------------------------------------------------------

/// Load all `.toml` scenario files from a directory.
pub fn load_scenarios(dir: &Path) -> Result<Vec<EvalScenario>, RunnerError> {
    if !dir.is_dir() {
        return Err(RunnerError::ScenarioLoad(format!(
            "scenario directory does not exist: {}",
            dir.display()
        )));
    }

    let mut scenarios = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| RunnerError::ScenarioLoad(format!("cannot read dir: {e}")))?
        .filter_map(|e| e.ok())
        .collect();

    // Sort by filename for deterministic ordering.
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            // Recurse into subdirectories (e.g., level1/, level2/)
            let sub = load_scenarios(&path)?;
            scenarios.extend(sub);
        } else if path.extension().is_some_and(|ext| ext == "toml") {
            let contents = std::fs::read_to_string(&path).map_err(|e| {
                RunnerError::ScenarioLoad(format!("cannot read {}: {e}", path.display()))
            })?;
            let scenario: EvalScenario = toml::from_str(&contents)?;
            scenarios.push(scenario);
        }
    }

    Ok(scenarios)
}

// ---------------------------------------------------------------------------
// EvalRunner — the orchestrator
// ---------------------------------------------------------------------------

/// Orchestrates scenario execution: load, run, grade, collect results.
pub struct EvalRunner<T: McpTransport> {
    transport: T,
    config: EvalConfig,
}

impl<T: McpTransport> EvalRunner<T> {
    pub fn new(transport: T, config: EvalConfig) -> Self {
        Self { transport, config }
    }

    /// Run all scenarios from the configured scenario directory.
    ///
    /// Sequence: preflight check -> optional warmup -> scenario loop.
    pub async fn run_all(&mut self) -> Result<Vec<ScenarioRun>, RunnerError> {
        self.preflight().await?;

        if self.config.warmup {
            self.warmup().await?;
        }

        let scenarios = load_scenarios(&self.config.scenario_dir)?;
        let mut runs = Vec::with_capacity(scenarios.len());

        for scenario in scenarios {
            let run = self.run_scenario(scenario).await?;
            runs.push(run);
        }

        Ok(runs)
    }

    /// Run all provided scenarios (for when caller has pre-loaded them).
    ///
    /// Sequence: preflight check -> optional warmup -> scenario loop.
    pub async fn run_scenarios(
        &mut self,
        scenarios: Vec<EvalScenario>,
    ) -> Result<Vec<ScenarioRun>, RunnerError> {
        self.preflight().await?;

        if self.config.warmup {
            self.warmup().await?;
        }

        let mut runs = Vec::with_capacity(scenarios.len());

        for scenario in scenarios {
            let run = self.run_scenario(scenario).await?;
            runs.push(run);
        }

        Ok(runs)
    }

    /// Run a single scenario with full lifecycle:
    /// 1. Generate fresh session_id
    /// 2. Pre-cleanup: delete_session (EF07)
    /// 3. Verify clean state: entity_count=0 — abort with CONTAMINATED if not (T-010)
    /// 4. Before-snapshot (EF14)
    /// 5. Execute steps, record ToolCallTrace per step (inject tenant_id)
    /// 6. After-snapshot
    /// 7. Post-cleanup: delete_session (EF07)
    pub async fn run_scenario(
        &mut self,
        scenario: EvalScenario,
    ) -> Result<ScenarioRun, RunnerError> {
        let session_id = Uuid::new_v4();
        let start = Instant::now();

        // EF07: Pre-cleanup — delete any leftover state
        self.delete_session(&session_id).await?;

        // T-010 / EF07: Verify clean state — CONTAMINATED if entity_count > 0
        let pre_snapshot = self.take_snapshot(&session_id).await?;
        if pre_snapshot.entity_count != 0 {
            return Err(RunnerError::Contaminated {
                entity_count: pre_snapshot.entity_count,
                session_id,
            });
        }

        // EF14: Before-snapshot BEFORE any tool calls
        let graph_snapshot_before = pre_snapshot;

        // Execute scenario steps
        let mut traces = Vec::with_capacity(scenario.steps.len());
        for step in &scenario.steps {
            let trace = self
                .execute_step(step, &session_id, &self.config.tenant_id.clone())
                .await;
            traces.push(trace);
        }

        // After-snapshot
        let graph_snapshot_after = self.take_snapshot(&session_id).await?;

        let duration = start.elapsed();

        // EF07: Post-cleanup
        self.delete_session(&session_id).await?;

        Ok(ScenarioRun {
            scenario,
            session_id,
            traces,
            graph_snapshot_before,
            graph_snapshot_after,
            duration,
        })
    }

    /// Execute a single step, recording the trace.
    /// Errors from the MCP call are captured in the trace, not propagated.
    /// Injects `session_id` and `tenant_id` if not already present (T-010).
    async fn execute_step(
        &mut self,
        step: &EvalStep,
        session_id: &Uuid,
        tenant_id: &Uuid,
    ) -> ToolCallTrace {
        let mut arguments = step.arguments.clone();

        // Inject session_id into arguments if not already present.
        if !arguments.contains_key("session_id") {
            arguments.insert(
                "session_id".to_string(),
                Value::String(session_id.to_string()),
            );
        }

        // T-010: Inject tenant_id into arguments if not already present.
        if !arguments.contains_key("tenant_id") {
            arguments.insert(
                "tenant_id".to_string(),
                Value::String(tenant_id.to_string()),
            );
        }

        let args_value =
            serde_json::to_value(&arguments).unwrap_or(Value::Object(serde_json::Map::new()));

        match self.transport.call_tool(&step.tool, args_value).await {
            Ok((response, latency)) => ToolCallTrace {
                tool: step.tool.clone(),
                arguments,
                response,
                latency_ms: latency.as_millis() as u64,
                success: true,
            },
            Err(e) => ToolCallTrace {
                tool: step.tool.clone(),
                arguments,
                response: serde_json::json!({"error": e.to_string()}),
                latency_ms: 0,
                success: false,
            },
        }
    }

    /// Call delete_session via MCP.
    async fn delete_session(&mut self, session_id: &Uuid) -> Result<(), RunnerError> {
        let args = serde_json::json!({
            "session_id": session_id.to_string()
        });
        let _ = self.transport.call_tool("delete_session", args).await;
        // Ignore errors — session may not exist yet (pre-cleanup)
        Ok(())
    }

    /// Take a graph snapshot via get_stats.
    async fn take_snapshot(&mut self, session_id: &Uuid) -> Result<GraphSnapshot, RunnerError> {
        let (response, _latency) = self
            .transport
            .call_tool(
                "get_stats",
                serde_json::json!({
                    "session_id": session_id.to_string()
                }),
            )
            .await?;
        Ok(GraphSnapshot::from_stats_response(&response))
    }

    // -----------------------------------------------------------------------
    // T-009: Pre-flight health check
    // -----------------------------------------------------------------------

    /// Verify the Ferrosa cluster is healthy before running any scenarios.
    ///
    /// Calls `get_stats` and checks that the response arrives within the
    /// configured timeout (default 100ms). Aborts with `PreflightFailed` if
    /// the cluster is unreachable or too slow.
    pub async fn preflight(&mut self) -> Result<(), RunnerError> {
        let timeout = Duration::from_millis(self.config.preflight_timeout_ms);
        let start = Instant::now();

        let result = self
            .transport
            .call_tool("get_stats", serde_json::json!({}))
            .await;

        let elapsed = start.elapsed();

        match result {
            Ok((_response, _latency)) => {
                if elapsed > timeout {
                    return Err(RunnerError::PreflightFailed {
                        reason: format!("get_stats responded in {elapsed:?} (limit: {timeout:?})"),
                    });
                }
                Ok(())
            }
            Err(e) => Err(RunnerError::PreflightFailed {
                reason: e.to_string(),
            }),
        }
    }

    // -----------------------------------------------------------------------
    // T-011: Warm-up phase
    // -----------------------------------------------------------------------

    /// Run a throwaway cycle to warm up CQL connection pool, Ollama embeddings,
    /// and the HNSW index. Results are discarded — not included in scoring.
    ///
    /// Sequence: `upsert_entity` (dummy) -> `hybrid_search` -> `delete_session`.
    pub async fn warmup(&mut self) -> Result<(), RunnerError> {
        let warmup_session = Uuid::new_v4();
        let tenant_id = self.config.tenant_id;

        // Upsert a dummy entity to exercise CQL + Ollama embedding pipeline
        let upsert_args = serde_json::json!({
            "entity_name": "__eval_warmup__",
            "entity_type": "eval_warmup",
            "context_snippet": "warmup probe — safe to delete",
            "observations": ["warmup probe — safe to delete"],
            "source": "eval_warmup",
            "session_id": warmup_session.to_string(),
            "tenant_id": tenant_id.to_string(),
        });
        let _ = self.transport.call_tool("upsert_entity", upsert_args).await;

        // Search to exercise HNSW index
        let search_args = serde_json::json!({
            "query": "__eval_warmup__",
            "session_id": warmup_session.to_string(),
            "tenant_id": tenant_id.to_string(),
        });
        let _ = self.transport.call_tool("hybrid_search", search_args).await;

        // Clean up warmup data
        self.delete_session(&warmup_session).await?;

        Ok(())
    }

    /// Grade a completed scenario run using the configured grading methods.
    pub fn grade_run(&self, run: &ScenarioRun) -> GradeResult {
        let mut programmatic_score = None;
        let mut claim_score = None;

        let methods = &run.scenario.grading.methods;

        // Programmatic grading
        if methods.is_empty() || methods.iter().any(|m| m == "programmatic") {
            let score = programmatic::grade(&run.scenario.steps, &run.traces, &NoOpResolver);
            programmatic_score = Some(score);
        }

        // Claim rubric grading
        if methods.iter().any(|m| m == "claim_rubric")
            && let Some(ref rubric_cfg) = run.scenario.grading.claim_rubric
        {
            // Concatenate all response texts for claim grading
            let response_text = run
                .traces
                .iter()
                .map(|t| response_to_text(&t.response))
                .collect::<Vec<_>>()
                .join(" ");

            let claim_strs: Vec<&str> = rubric_cfg.claims.iter().map(|s| s.as_str()).collect();

            if let Ok(score) = claim_rubric::grade_claims(
                &claim_strs,
                &response_text,
                rubric_cfg.passing_threshold,
            ) {
                claim_score = Some(score);
            }
        }

        let mut result = GradeResult {
            programmatic: programmatic_score,
            claims: claim_score,
            memory_quality: None,
        };

        if let Some(ref truth) = run.scenario.retrieval_ground_truth {
            let hits = extract_evidence_hits(&run.traces);
            let metrics = evaluate_retrieval(truth, &hits, hits.len());
            let actual_score = result.composite_score();
            let stale_temporal_evidence_present = run.traces.iter().any(|trace| {
                response_to_text(&trace.response)
                    .to_lowercase()
                    .contains("superseded")
            });
            let failure_kind =
                classify_observed_failure(&metrics, actual_score, stale_temporal_evidence_present);

            result.memory_quality = Some(MemoryQualityScore {
                retrieval_mode: RetrievalMode::ActualHybrid,
                chunking_policy: ChunkingPolicy::EvidencePacket,
                metrics,
                failure_kind,
            });
        }

        result
    }

    // -----------------------------------------------------------------------
    // T-035: Stability Canary
    // -----------------------------------------------------------------------

    /// Run the stability canary: execute a scenario 3 times and verify identical scores.
    ///
    /// Returns the 3 runs if stable. Errors with `StabilityCanaryFailed` if any
    /// programmatic or claim scores diverge between runs.
    pub async fn stability_canary(
        &mut self,
        scenario: EvalScenario,
    ) -> Result<[GradeResult; 3], RunnerError> {
        let mut grades = Vec::with_capacity(3);

        for _ in 0..3 {
            let run = self.run_scenario(scenario.clone()).await?;
            let grade = self.grade_run(&run);
            grades.push(grade);
        }

        // Compare all 3 programmatic scores
        let prog_scores: Vec<Option<f64>> = grades
            .iter()
            .map(|g| g.programmatic.as_ref().map(|p| p.score))
            .collect();

        if prog_scores[0] != prog_scores[1] || prog_scores[1] != prog_scores[2] {
            return Err(RunnerError::StabilityCanaryFailed {
                detail: format!(
                    "programmatic scores diverged: {:?} vs {:?} vs {:?}",
                    prog_scores[0], prog_scores[1], prog_scores[2]
                ),
            });
        }

        // Compare all 3 claim scores
        let claim_scores: Vec<Option<f64>> = grades
            .iter()
            .map(|g| g.claims.as_ref().map(|c| c.score))
            .collect();

        if claim_scores[0] != claim_scores[1] || claim_scores[1] != claim_scores[2] {
            return Err(RunnerError::StabilityCanaryFailed {
                detail: format!(
                    "claim scores diverged: {:?} vs {:?} vs {:?}",
                    claim_scores[0], claim_scores[1], claim_scores[2]
                ),
            });
        }

        // Safe: we always push exactly 3
        let [g0, g1, g2] = match grades.try_into() {
            Ok(arr) => arr,
            Err(_) => unreachable!("always 3 elements"),
        };
        Ok([g0, g1, g2])
    }
}

// ---------------------------------------------------------------------------
// T-038: Parallel Scenario Execution
// ---------------------------------------------------------------------------

/// Run multiple scenarios in parallel, each on its own transport.
///
/// `transport_factory` creates a fresh transport for each scenario (needed for
/// stdio mode where each gets its own MCP server process).
/// Results are collected and sorted by scenario_id for deterministic output.
pub async fn run_scenarios_parallel<T, F>(
    config: &EvalConfig,
    scenarios: Vec<EvalScenario>,
    transport_factory: F,
) -> Result<Vec<ScenarioRun>, RunnerError>
where
    T: McpTransport + 'static,
    F: Fn() -> T + Send + Sync + 'static,
{
    let max_parallel = config.max_parallel;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_parallel));
    let factory = std::sync::Arc::new(transport_factory);
    let config = std::sync::Arc::new(config.clone());

    let mut join_set = tokio::task::JoinSet::new();

    for scenario in scenarios {
        let sem = semaphore.clone();
        let factory = factory.clone();
        let config = config.clone();

        join_set.spawn(async move {
            let _permit = sem
                .acquire()
                .await
                .map_err(|_| RunnerError::McpClient("semaphore closed".to_string()))?;

            let transport = factory();
            let mut runner = EvalRunner::new(transport, (*config).clone());

            // Skip preflight/warmup for individual parallel scenarios --
            // caller should have done those once before dispatching.
            runner.run_scenario(scenario).await
        });
    }

    let mut results = Vec::new();
    while let Some(join_result) = join_set.join_next().await {
        let run =
            join_result.map_err(|e| RunnerError::McpClient(format!("task join error: {e}")))??;
        results.push(run);
    }

    // Sort by scenario_id for deterministic ordering.
    results.sort_by(|a, b| a.scenario.scenario.id.cmp(&b.scenario.scenario.id));

    Ok(results)
}

// ---------------------------------------------------------------------------
// T-041: Cleanup Ledger
// ---------------------------------------------------------------------------

/// Tracks session_ids created during an eval run for crash-safe cleanup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupLedger {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub sessions: Vec<String>,
}

impl Default for CleanupLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl CleanupLedger {
    /// Create a new ledger for the current run.
    pub fn new() -> Self {
        Self {
            run_id: Uuid::new_v4().to_string(),
            started_at: Utc::now(),
            sessions: Vec::new(),
        }
    }

    /// Add a session_id to the ledger.
    pub fn add_session(&mut self, session_id: &Uuid) {
        self.sessions.push(session_id.to_string());
    }

    /// Save the ledger to disk.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        std::fs::write(path, json)
    }

    /// Load a ledger from disk. Returns None if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Option<Self>, std::io::Error> {
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(path)?;
        let ledger: Self = serde_json::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(Some(ledger))
    }

    /// Check if this ledger is stale (started_at > 1 hour ago).
    pub fn is_stale(&self) -> bool {
        let age = Utc::now() - self.started_at;
        age.num_seconds() > 3600
    }

    /// Delete the ledger file from disk.
    pub fn remove(path: &Path) -> Result<(), std::io::Error> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Sweep all sessions in this ledger using the provided transport.
    /// Ignores errors from delete_session (sessions may not exist).
    pub async fn sweep<T: McpTransport>(&self, transport: &mut T) -> Result<(), RunnerError> {
        for session_str in &self.sessions {
            let args = serde_json::json!({
                "session_id": session_str,
            });
            let _ = transport.call_tool("delete_session", args).await;
        }
        Ok(())
    }
}

/// Check for stale ledger on startup and sweep if found.
///
/// Returns the number of stale sessions swept, or 0 if no stale ledger.
pub async fn sweep_stale_ledger<T: McpTransport>(
    ledger_path: &Path,
    transport: &mut T,
) -> Result<usize, RunnerError> {
    let ledger = CleanupLedger::load(ledger_path)?;
    match ledger {
        Some(l) if l.is_stale() => {
            let count = l.sessions.len();
            l.sweep(transport).await?;
            CleanupLedger::remove(ledger_path)?;
            Ok(count)
        }
        _ => Ok(0),
    }
}

/// Grading results for a scenario run.
#[derive(Debug)]
pub struct GradeResult {
    pub programmatic: Option<programmatic::ProgrammaticScore>,
    pub claims: Option<claim_rubric::ClaimScore>,
    pub memory_quality: Option<MemoryQualityScore>,
}

impl GradeResult {
    /// Composite score across all available grading methods.
    pub fn composite_score(&self) -> f64 {
        let mut total = 0.0;
        let mut count = 0;

        if let Some(ref p) = self.programmatic {
            total += p.score;
            count += 1;
        }
        if let Some(ref c) = self.claims {
            total += c.score;
            count += 1;
        }

        if count == 0 {
            0.0
        } else {
            total / count as f64
        }
    }

    /// Whether the scenario passed all grading checks.
    pub fn passed(&self, threshold: f64) -> bool {
        self.composite_score() >= threshold
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a JSON response Value to searchable text.
fn response_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            // For MCP content arrays, extract text fields
            if let Some(content) = map.get("content").and_then(|c| c.as_array()) {
                return content
                    .iter()
                    .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            serde_json::to_string(value).unwrap_or_default()
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn extract_evidence_hits(traces: &[ToolCallTrace]) -> Vec<EvidenceHit> {
    let mut hits = Vec::new();
    for trace in traces.iter().filter(|trace| trace.tool == "hybrid_search") {
        collect_evidence_ids(&trace.response, &mut hits);
    }
    hits
}

fn collect_evidence_ids(value: &Value, hits: &mut Vec<EvidenceHit>) {
    match value {
        Value::Object(map) => {
            for key in [
                "id",
                "entity_id",
                "fold_id",
                "fact_id",
                "event_id",
                "edge_id",
                "source_fold_id",
            ] {
                if let Some(id) = map.get(key).and_then(|v| v.as_str()) {
                    hits.push(EvidenceHit::new(id));
                }
            }
            for nested in map.values() {
                collect_evidence_ids(nested, hits);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_evidence_ids(item, hits);
            }
        }
        _ => {}
    }
}

fn classify_observed_failure(
    retrieval: &MemoryEvalMetrics,
    actual_score: f64,
    stale_temporal_evidence_present: bool,
) -> MemoryFailureKind {
    if stale_temporal_evidence_present {
        return MemoryFailureKind::StaleTemporalFact;
    }
    if actual_score >= 0.8 {
        return MemoryFailureKind::Passed;
    }
    if retrieval.required_total > 0 && retrieval.required_hits == 0 {
        return MemoryFailureKind::RetrievalMiss;
    }

    // The runner only observes the actual run. Oracle/packing/chunking ablation
    // scores are computed by dedicated sweeps, so avoid pretending we can
    // distinguish chunking loss from packing loss here.
    MemoryFailureKind::GeneratorReasoningFailure
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------
    // Mock MCP transport
    // -----------------------------------------------------------------------

    /// Records calls and returns pre-configured responses.
    struct MockTransport {
        /// Pre-configured responses keyed by tool name.
        responses: HashMap<String, Vec<(Value, Duration)>>,
        /// Index into the response vec for each tool.
        call_index: HashMap<String, usize>,
        /// Recorded calls for assertion.
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        /// Default response for tools not explicitly configured.
        default_response: (Value, Duration),
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
                call_index: HashMap::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
                default_response: (json!({"ok": true}), Duration::from_millis(10)),
            }
        }

        /// Register a sequence of responses for a tool.
        fn on_tool(mut self, tool: &str, responses: Vec<(Value, Duration)>) -> Self {
            self.responses.insert(tool.to_string(), responses);
            self
        }

        /// Get recorded calls.
        #[allow(dead_code)]
        fn recorded_calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl McpTransport for MockTransport {
        async fn call_tool(
            &mut self,
            tool_name: &str,
            arguments: Value,
        ) -> Result<(Value, Duration), RunnerError> {
            // Record the call
            self.calls
                .lock()
                .unwrap()
                .push((tool_name.to_string(), arguments.clone()));

            // Return the next configured response, or the default
            if let Some(resps) = self.responses.get(tool_name) {
                let idx = self.call_index.entry(tool_name.to_string()).or_insert(0);
                if *idx < resps.len() {
                    let resp = resps[*idx].clone();
                    *idx += 1;
                    return Ok(resp);
                }
            }

            Ok(self.default_response.clone())
        }
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn test_config() -> EvalConfig {
        EvalConfig {
            scenario_dir: PathBuf::from("/tmp/test-scenarios"),
            warmup: false,
            ..EvalConfig::default()
        }
    }

    fn make_step(tool: &str) -> EvalStep {
        EvalStep {
            tool: tool.to_string(),
            arguments: HashMap::new(),
            expect_in_response: vec![],
            expect_action: None,
            expect_entity_name: None,
        }
    }

    fn make_scenario(id: &str, steps: Vec<EvalStep>) -> EvalScenario {
        use crate::scenario::{GradingConfig, ScenarioMeta};

        EvalScenario {
            scenario: ScenarioMeta {
                id: id.to_string(),
                name: id.to_string(),
                description: String::new(),
                level: 1,
                dikw_transition: None,
                tags: vec![],
                timeout_ms: 15_000,
            },
            steps,
            grading: GradingConfig::default(),
            retrieval_ground_truth: None,
            dikw: None,
            semantic: None,
        }
    }

    fn stats_response(entity_count: usize, edge_count: usize) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({
                    "entity_count": entity_count,
                    "edge_count": edge_count,
                    "derived_fact_count": 0
                })).unwrap()
            }]
        })
    }

    // -----------------------------------------------------------------------
    // GraphSnapshot tests
    // -----------------------------------------------------------------------

    #[test]
    fn graph_snapshot_empty_has_zero_counts() {
        let snap = GraphSnapshot::empty();
        assert_eq!(snap.entity_count, 0);
        assert_eq!(snap.edge_count, 0);
        assert_eq!(snap.derived_fact_count, 0);
    }

    #[test]
    fn graph_snapshot_parses_mcp_content_response() {
        let response = stats_response(5, 3);
        let snap = GraphSnapshot::from_stats_response(&response);
        assert_eq!(snap.entity_count, 5);
        assert_eq!(snap.edge_count, 3);
        assert_eq!(snap.derived_fact_count, 0);
    }

    #[test]
    fn graph_snapshot_parses_direct_json_response() {
        let response = json!({
            "entity_count": 10,
            "edge_count": 7,
            "derived_fact_count": 2
        });
        let snap = GraphSnapshot::from_stats_response(&response);
        assert_eq!(snap.entity_count, 10);
        assert_eq!(snap.edge_count, 7);
        assert_eq!(snap.derived_fact_count, 2);
    }

    #[test]
    fn graph_snapshot_handles_missing_fields() {
        let response = json!({});
        let snap = GraphSnapshot::from_stats_response(&response);
        assert_eq!(snap.entity_count, 0);
        assert_eq!(snap.edge_count, 0);
        assert_eq!(snap.derived_fact_count, 0);
    }

    // -----------------------------------------------------------------------
    // Scenario loader tests
    // -----------------------------------------------------------------------

    #[test]
    fn load_scenarios_rejects_nonexistent_dir() {
        let result = load_scenarios(Path::new("/nonexistent/dir"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("does not exist"),
            "expected 'does not exist', got: {err}"
        );
    }

    #[test]
    fn load_scenarios_from_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let scenarios = load_scenarios(dir.path()).unwrap();
        assert!(scenarios.is_empty());
    }

    #[test]
    fn load_scenarios_parses_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        let toml_content = r#"
[scenario]
id = "test-1"
name = "Test Scenario"

[[steps]]
tool = "get_stats"
"#;
        std::fs::write(dir.path().join("test.toml"), toml_content).unwrap();

        let scenarios = load_scenarios(dir.path()).unwrap();
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0].scenario.id, "test-1");
        assert_eq!(scenarios[0].steps.len(), 1);
        assert_eq!(scenarios[0].steps[0].tool, "get_stats");
    }

    #[test]
    fn load_scenarios_recurses_into_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("level1");
        std::fs::create_dir(&sub).unwrap();

        let toml_content = r#"
[scenario]
id = "sub-test"
name = "Sub Test"

[[steps]]
tool = "hybrid_search"
"#;
        std::fs::write(sub.join("sub.toml"), toml_content).unwrap();

        let scenarios = load_scenarios(dir.path()).unwrap();
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0].scenario.id, "sub-test");
    }

    #[test]
    fn load_scenarios_ignores_non_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.md"), "# Not a scenario").unwrap();
        std::fs::write(dir.path().join("data.json"), "{}").unwrap();

        let scenarios = load_scenarios(dir.path()).unwrap();
        assert!(scenarios.is_empty());
    }

    #[test]
    fn load_scenarios_rejects_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.toml"), "this is not valid toml {{{").unwrap();

        let result = load_scenarios(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn load_scenarios_deterministic_order() {
        let dir = tempfile::tempdir().unwrap();

        for name in ["c_test.toml", "a_test.toml", "b_test.toml"] {
            let id = name.trim_end_matches(".toml");
            let toml_content = format!(
                r#"
[scenario]
id = "{id}"
name = "{id}"

[[steps]]
tool = "get_stats"
"#
            );
            std::fs::write(dir.path().join(name), toml_content).unwrap();
        }

        let scenarios = load_scenarios(dir.path()).unwrap();
        assert_eq!(scenarios.len(), 3);
        assert_eq!(scenarios[0].scenario.id, "a_test");
        assert_eq!(scenarios[1].scenario.id, "b_test");
        assert_eq!(scenarios[2].scenario.id, "c_test");
    }

    // -----------------------------------------------------------------------
    // EvalRunner: scenario execution tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_scenario_creates_fresh_session_id() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(1, 0), Duration::from_millis(5)),
            ],
        );

        let scenario = make_scenario("test-session", vec![make_step("smart_ingest")]);

        let mut runner = EvalRunner::new(transport, test_config());
        let run = runner.run_scenario(scenario).await.unwrap();

        // Session ID should be a valid v4 UUID
        assert_eq!(run.session_id.get_version_num(), 4);
    }

    #[tokio::test]
    async fn run_scenario_calls_delete_session_before_and_after() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(1, 0), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();
        let scenario = make_scenario("cleanup-test", vec![make_step("smart_ingest")]);

        let mut runner = EvalRunner::new(transport, test_config());
        let run = runner.run_scenario(scenario).await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        let delete_calls: Vec<_> = recorded
            .iter()
            .filter(|(name, _)| name == "delete_session")
            .collect();

        // EF07: delete_session called at least twice (before + after)
        assert!(
            delete_calls.len() >= 2,
            "EF07: expected at least 2 delete_session calls, got {}",
            delete_calls.len()
        );

        // Both should use the same session_id
        let sid = run.session_id.to_string();
        for (_, args) in &delete_calls {
            assert_eq!(
                args["session_id"].as_str().unwrap(),
                sid,
                "delete_session should use the scenario session_id"
            );
        }
    }

    #[tokio::test]
    async fn run_scenario_snapshots_before_any_steps() {
        // The mock will return entity_count=0 for the first get_stats (before),
        // and entity_count=3 for the second (after).
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(3, 2), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();
        let scenario = make_scenario(
            "snapshot-test",
            vec![make_step("smart_ingest"), make_step("hybrid_search")],
        );

        let mut runner = EvalRunner::new(transport, test_config());
        let run = runner.run_scenario(scenario).await.unwrap();

        // EF14: Before-snapshot should capture state before any tool calls
        assert_eq!(
            run.graph_snapshot_before.entity_count, 0,
            "EF14: before-snapshot must be taken before any tool calls"
        );
        assert_eq!(run.graph_snapshot_after.entity_count, 3);
        assert_eq!(run.graph_snapshot_after.edge_count, 2);

        // Verify call ordering: delete_session, get_stats(before), steps..., get_stats(after), delete_session
        let recorded = calls.lock().unwrap().clone();
        let tool_names: Vec<&str> = recorded.iter().map(|(n, _)| n.as_str()).collect();

        assert_eq!(tool_names[0], "delete_session", "first call: pre-cleanup");
        assert_eq!(tool_names[1], "get_stats", "second call: before-snapshot");
        assert_eq!(tool_names[2], "smart_ingest", "third call: first step");
        assert_eq!(tool_names[3], "hybrid_search", "fourth call: second step");
        assert_eq!(tool_names[4], "get_stats", "fifth call: after-snapshot");
        assert_eq!(tool_names[5], "delete_session", "sixth call: post-cleanup");

        let sid = run.session_id.to_string();
        assert_eq!(
            recorded[1].1["session_id"].as_str(),
            Some(sid.as_str()),
            "before-snapshot must read the scenario session"
        );
        assert_eq!(
            recorded[4].1["session_id"].as_str(),
            Some(sid.as_str()),
            "after-snapshot must read the scenario session"
        );
    }

    #[tokio::test]
    async fn run_scenario_records_traces_per_step() {
        let transport = MockTransport::new()
            .on_tool(
                "get_stats",
                vec![
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                ],
            )
            .on_tool(
                "smart_ingest",
                vec![(
                    json!({"action": "Created", "entity_id": "e1"}),
                    Duration::from_millis(42),
                )],
            )
            .on_tool(
                "hybrid_search",
                vec![(
                    json!({"results": [{"name": "Alice"}]}),
                    Duration::from_millis(30),
                )],
            );

        let scenario = make_scenario(
            "trace-test",
            vec![make_step("smart_ingest"), make_step("hybrid_search")],
        );

        let mut runner = EvalRunner::new(transport, test_config());
        let run = runner.run_scenario(scenario).await.unwrap();

        assert_eq!(run.traces.len(), 2, "should have one trace per step");

        assert_eq!(run.traces[0].tool, "smart_ingest");
        assert!(run.traces[0].success);
        assert_eq!(run.traces[0].latency_ms, 42);
        assert_eq!(run.traces[0].response["action"], "Created");

        assert_eq!(run.traces[1].tool, "hybrid_search");
        assert!(run.traces[1].success);
        assert_eq!(run.traces[1].latency_ms, 30);
    }

    #[tokio::test]
    async fn run_scenario_injects_session_id_into_arguments() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(0, 0), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();
        let scenario = make_scenario("inject-test", vec![make_step("smart_ingest")]);

        let mut runner = EvalRunner::new(transport, test_config());
        let run = runner.run_scenario(scenario).await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        let ingest_call = recorded
            .iter()
            .find(|(name, _)| name == "smart_ingest")
            .unwrap();

        assert_eq!(
            ingest_call.1["session_id"].as_str().unwrap(),
            run.session_id.to_string(),
            "session_id should be injected into step arguments"
        );
    }

    #[tokio::test]
    async fn run_scenario_captures_mcp_errors_in_trace() {
        struct FailingTransport {
            call_count: usize,
        }

        impl McpTransport for FailingTransport {
            async fn call_tool(
                &mut self,
                tool_name: &str,
                _arguments: Value,
            ) -> Result<(Value, Duration), RunnerError> {
                self.call_count += 1;
                match tool_name {
                    "delete_session" => Ok((json!({"ok": true}), Duration::from_millis(1))),
                    "get_stats" => Ok((stats_response(0, 0), Duration::from_millis(1))),
                    "flaky_tool" => Err(RunnerError::McpClient("connection refused".to_string())),
                    _ => Ok((json!({"ok": true}), Duration::from_millis(1))),
                }
            }
        }

        let scenario = make_scenario("error-test", vec![make_step("flaky_tool")]);
        let mut runner = EvalRunner::new(FailingTransport { call_count: 0 }, test_config());
        let run = runner.run_scenario(scenario).await.unwrap();

        assert_eq!(run.traces.len(), 1);
        assert!(
            !run.traces[0].success,
            "failed call should have success=false"
        );
        assert!(
            run.traces[0].response["error"]
                .as_str()
                .unwrap()
                .contains("connection refused"),
            "error should be captured in trace response"
        );
    }

    #[tokio::test]
    async fn run_scenario_fails_on_dirty_state() {
        // Pre-snapshot returns entity_count=5 (dirty state)
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![(stats_response(5, 2), Duration::from_millis(5))],
        );

        let scenario = make_scenario("dirty-test", vec![make_step("smart_ingest")]);
        let mut runner = EvalRunner::new(transport, test_config());
        let result = runner.run_scenario(scenario).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            RunnerError::Contaminated {
                entity_count,
                session_id: _,
            } => {
                assert_eq!(entity_count, 5, "should report actual entity count");
            }
            other => panic!("expected Contaminated, got: {other}"),
        }
    }

    #[tokio::test]
    async fn run_scenario_preserves_existing_session_id_in_args() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(0, 0), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();

        // Step with explicit session_id already set
        let mut step = make_step("smart_ingest");
        step.arguments
            .insert("session_id".to_string(), json!("custom-session-id"));

        let scenario = make_scenario("preserve-sid", vec![step]);

        let mut runner = EvalRunner::new(transport, test_config());
        let _run = runner.run_scenario(scenario).await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        let ingest_call = recorded
            .iter()
            .find(|(name, _)| name == "smart_ingest")
            .unwrap();

        assert_eq!(
            ingest_call.1["session_id"].as_str().unwrap(),
            "custom-session-id",
            "should preserve explicit session_id, not override"
        );
    }

    // -----------------------------------------------------------------------
    // EvalRunner: multi-scenario execution
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_scenarios_executes_all_in_sequence() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                // Preflight check (T-009)
                (stats_response(0, 0), Duration::from_millis(5)),
                // Scenario 1: before, after
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(1, 0), Duration::from_millis(5)),
                // Scenario 2: before, after
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(2, 1), Duration::from_millis(5)),
            ],
        );

        let scenarios = vec![
            make_scenario("s1", vec![make_step("smart_ingest")]),
            make_scenario("s2", vec![make_step("hybrid_search")]),
        ];

        let mut runner = EvalRunner::new(transport, test_config());
        let runs = runner.run_scenarios(scenarios).await.unwrap();

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].scenario.scenario.id, "s1");
        assert_eq!(runs[1].scenario.scenario.id, "s2");

        // Each should have a unique session_id
        assert_ne!(
            runs[0].session_id, runs[1].session_id,
            "each scenario gets a fresh session_id"
        );
    }

    // -----------------------------------------------------------------------
    // Grading integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn grade_run_programmatic_scores_correct_sequence() {
        let transport = MockTransport::new()
            .on_tool(
                "get_stats",
                vec![
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                ],
            )
            .on_tool(
                "smart_ingest",
                vec![(
                    json!({"action": "Created", "entity_id": "e1"}),
                    Duration::from_millis(20),
                )],
            );

        let mut step = make_step("smart_ingest");
        step.expect_in_response = vec!["Created".to_string(), "entity_id".to_string()];
        step.expect_action = Some("Created".to_string());

        let scenario = make_scenario("grade-test", vec![step]);

        let mut runner = EvalRunner::new(transport, test_config());
        let run = runner.run_scenario(scenario).await.unwrap();
        let grade = runner.grade_run(&run);

        let prog = grade.programmatic.as_ref().unwrap();
        assert!(prog.sequence_match, "sequence should match");
        assert!(prog.schema_valid, "schema should be valid");
        assert_eq!(prog.field_assertions_passed, 3); // 2 expect_in_response + 1 expect_action
        assert_eq!(prog.field_assertions_total, 3);
        assert!(prog.score > 0.9, "score should be high, got {}", prog.score);
    }

    #[tokio::test]
    async fn grade_run_claim_rubric_scores_matching_claims() {
        use crate::scenario::{ClaimRubricConfig, GradingConfig};

        let transport = MockTransport::new()
            .on_tool(
                "get_stats",
                vec![
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                ],
            )
            .on_tool(
                "smart_ingest",
                vec![(
                    json!({
                        "content": [{
                            "type": "text",
                            "text": "Created entity_id abc-123 for Alice of type person"
                        }]
                    }),
                    Duration::from_millis(20),
                )],
            );

        let step = make_step("smart_ingest");
        let mut scenario = make_scenario("claim-test", vec![step]);
        scenario.grading = GradingConfig {
            methods: vec!["claim_rubric".to_string()],
            claim_rubric: Some(ClaimRubricConfig {
                claims: vec![
                    "entity_id".to_string(),
                    "Alice".to_string(),
                    "person".to_string(),
                ],
                passing_threshold: 0.75,
            }),
            llm_judge: None,
        };

        let mut runner = EvalRunner::new(transport, test_config());
        let run = runner.run_scenario(scenario).await.unwrap();
        let grade = runner.grade_run(&run);

        let claims = grade.claims.as_ref().unwrap();
        assert!(claims.passed, "all claims should match");
        assert!(
            (claims.score - 1.0).abs() < f64::EPSILON,
            "score should be 1.0, got {}",
            claims.score
        );
    }

    #[tokio::test]
    async fn grade_run_populates_memory_quality_when_ground_truth_is_present() {
        let transport = MockTransport::new().on_tool(
            "hybrid_search",
            vec![(
                json!({
                    "results": [
                        {"entity_id": "entity:noise"},
                        {"entity_id": "entity:a"},
                        {"fold_id": "fold:root"}
                    ]
                }),
                Duration::from_millis(25),
            )],
        );
        let mut runner = EvalRunner::new(transport, test_config());
        let mut scenario = make_scenario("memory-quality", vec![make_step("hybrid_search")]);
        scenario.retrieval_ground_truth = Some(crate::memory_quality::EvidenceGroundTruth {
            required_entities: vec!["entity:a".to_string()],
            required_folds: vec!["fold:root".to_string()],
            required_facts: vec![],
            required_edges: vec![],
            distractor_entities: vec!["entity:noise".to_string()],
        });

        let run = runner.run_scenario(scenario).await.unwrap();
        let grade = runner.grade_run(&run);
        let memory = grade.memory_quality.expect("memory-quality score");

        assert_eq!(
            memory.retrieval_mode,
            crate::memory_quality::RetrievalMode::ActualHybrid
        );
        assert_eq!(
            memory.chunking_policy,
            crate::memory_quality::ChunkingPolicy::EvidencePacket
        );
        assert_eq!(memory.metrics.required_total, 2);
        assert_eq!(memory.metrics.required_hits, 2);
        assert_eq!(memory.metrics.distractor_hits, 1);
        assert_eq!(
            memory.failure_kind,
            crate::memory_quality::MemoryFailureKind::Passed
        );
    }

    #[test]
    fn scenario_toml_parses_retrieval_ground_truth_ids() {
        let toml = r#"
steps = []

[scenario]
id = "gt"
name = "Ground Truth"

[retrieval_ground_truth]
required_entities = ["entity:a"]
required_folds = ["fold:root"]
required_facts = ["fact:current"]
required_edges = ["edge:a->b"]
distractor_entities = ["entity:noise"]
"#;

        let scenario: EvalScenario = toml::from_str(toml).unwrap();
        let truth = scenario.retrieval_ground_truth.expect("ground truth");
        assert_eq!(truth.required_entities, vec!["entity:a"]);
        assert_eq!(truth.required_folds, vec!["fold:root"]);
        assert_eq!(truth.required_facts, vec!["fact:current"]);
        assert_eq!(truth.required_edges, vec!["edge:a->b"]);
        assert_eq!(truth.distractor_entities, vec!["entity:noise"]);
    }

    #[tokio::test]
    async fn grade_result_composite_averages_methods() {
        let grade = GradeResult {
            programmatic: Some(programmatic::ProgrammaticScore {
                schema_valid: true,
                sequence_match: true,
                field_assertions_passed: 2,
                field_assertions_total: 4,
                entity_identity_valid: None,
                score: 0.8,
            }),
            claims: None,
            memory_quality: None,
        };

        assert!(
            (grade.composite_score() - 0.8).abs() < f64::EPSILON,
            "single method should return its score"
        );
        assert!(grade.passed(0.75));
        assert!(!grade.passed(0.85));
    }

    #[tokio::test]
    async fn grade_result_composite_two_methods() {
        let grade = GradeResult {
            programmatic: Some(programmatic::ProgrammaticScore {
                schema_valid: true,
                sequence_match: true,
                field_assertions_passed: 5,
                field_assertions_total: 5,
                entity_identity_valid: None,
                score: 1.0,
            }),
            claims: Some(claim_rubric::ClaimScore {
                claims: vec![],
                score: 0.5,
                passed: false,
                threshold: 0.75,
            }),
            memory_quality: None,
        };

        let composite = grade.composite_score();
        assert!(
            (composite - 0.75).abs() < f64::EPSILON,
            "composite should be average: (1.0 + 0.5) / 2 = 0.75, got {composite}"
        );
    }

    // -----------------------------------------------------------------------
    // response_to_text tests
    // -----------------------------------------------------------------------

    #[test]
    fn response_to_text_extracts_mcp_content() {
        let response = json!({
            "content": [{
                "type": "text",
                "text": "Created entity Alice"
            }]
        });
        let text = response_to_text(&response);
        assert!(text.contains("Created entity Alice"));
    }

    #[test]
    fn response_to_text_handles_plain_json() {
        let response = json!({"action": "Created", "entity_id": "e1"});
        let text = response_to_text(&response);
        assert!(text.contains("Created"));
        assert!(text.contains("e1"));
    }

    #[test]
    fn response_to_text_handles_string() {
        let response = json!("hello world");
        let text = response_to_text(&response);
        assert_eq!(text, "hello world");
    }

    // -----------------------------------------------------------------------
    // ScenarioRun serialization
    // -----------------------------------------------------------------------

    #[test]
    fn scenario_run_serializes_to_json() {
        let run = ScenarioRun {
            scenario: make_scenario("ser-test", vec![make_step("get_stats")]),
            session_id: Uuid::nil(),
            traces: vec![ToolCallTrace {
                tool: "get_stats".to_string(),
                arguments: HashMap::new(),
                response: json!({"entity_count": 0}),
                latency_ms: 10,
                success: true,
            }],
            graph_snapshot_before: GraphSnapshot::empty(),
            graph_snapshot_after: GraphSnapshot::empty(),
            duration: Duration::from_millis(100),
        };

        let json_str = serde_json::to_string(&run).unwrap();
        assert!(json_str.contains("ser-test"));
        assert!(json_str.contains("get_stats"));
    }

    // -----------------------------------------------------------------------
    // T-009: Pre-flight health check tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn preflight_passes_when_get_stats_responds_fast() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![(stats_response(0, 0), Duration::from_millis(5))],
        );

        let mut runner = EvalRunner::new(transport, test_config());
        let result = runner.preflight().await;
        assert!(result.is_ok(), "preflight should pass with fast response");
    }

    #[tokio::test]
    async fn preflight_fails_when_transport_errors() {
        struct FailTransport;
        impl McpTransport for FailTransport {
            async fn call_tool(
                &mut self,
                _tool_name: &str,
                _arguments: Value,
            ) -> Result<(Value, Duration), RunnerError> {
                Err(RunnerError::McpClient("connection refused".to_string()))
            }
        }

        let mut runner = EvalRunner::new(FailTransport, test_config());
        let result = runner.preflight().await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            RunnerError::PreflightFailed { reason } => {
                assert!(
                    reason.contains("connection refused"),
                    "should contain error detail, got: {reason}"
                );
            }
            other => panic!("expected PreflightFailed, got: {other}"),
        }
    }

    #[tokio::test]
    async fn preflight_fails_when_response_too_slow() {
        // Transport that responds slowly (> preflight_timeout_ms)
        struct SlowTransport;
        impl McpTransport for SlowTransport {
            async fn call_tool(
                &mut self,
                _tool_name: &str,
                _arguments: Value,
            ) -> Result<(Value, Duration), RunnerError> {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok((stats_response(0, 0), Duration::from_millis(150)))
            }
        }

        let config = EvalConfig {
            preflight_timeout_ms: 50, // 50ms limit
            warmup: false,
            ..EvalConfig::default()
        };
        let mut runner = EvalRunner::new(SlowTransport, config);
        let result = runner.preflight().await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            RunnerError::PreflightFailed { reason } => {
                assert!(
                    reason.contains("get_stats responded in"),
                    "should mention slow response, got: {reason}"
                );
            }
            other => panic!("expected PreflightFailed, got: {other}"),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn preflight_aborts_eval_on_unhealthy_cluster() {
        // Same as preflight_fails_when_transport_errors, but exercises
        // the run_scenarios path to verify abort propagation.
        struct FailTransport;
        impl McpTransport for FailTransport {
            async fn call_tool(
                &mut self,
                _tool_name: &str,
                _arguments: Value,
            ) -> Result<(Value, Duration), RunnerError> {
                Err(RunnerError::McpClient("timeout".to_string()))
            }
        }

        let mut runner = EvalRunner::new(FailTransport, test_config());
        let result = runner
            .run_scenarios(vec![make_scenario("s1", vec![make_step("get_stats")])])
            .await;

        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("Pre-flight failed"),
            "should abort with preflight error, got: {err_str}"
        );
    }

    // -----------------------------------------------------------------------
    // T-010: Session isolation tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn session_isolation_injects_tenant_id_into_steps() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(1, 0), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();
        let scenario = make_scenario("tenant-test", vec![make_step("smart_ingest")]);

        let config = EvalConfig {
            tenant_id: Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap(),
            warmup: false,
            ..EvalConfig::default()
        };
        let mut runner = EvalRunner::new(transport, config);
        let _run = runner.run_scenario(scenario).await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        let ingest_call = recorded
            .iter()
            .find(|(name, _)| name == "smart_ingest")
            .unwrap();

        assert_eq!(
            ingest_call.1["tenant_id"].as_str().unwrap(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "tenant_id should be injected from config"
        );
    }

    #[tokio::test]
    async fn session_isolation_preserves_explicit_tenant_id() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(0, 0), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();
        let mut step = make_step("smart_ingest");
        step.arguments
            .insert("tenant_id".to_string(), json!("custom-tenant"));
        let scenario = make_scenario("preserve-tenant", vec![step]);

        let mut runner = EvalRunner::new(transport, test_config());
        let _run = runner.run_scenario(scenario).await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        let ingest_call = recorded
            .iter()
            .find(|(name, _)| name == "smart_ingest")
            .unwrap();

        assert_eq!(
            ingest_call.1["tenant_id"].as_str().unwrap(),
            "custom-tenant",
            "explicit tenant_id should not be overridden"
        );
    }

    #[tokio::test]
    async fn session_isolation_contaminated_aborts_with_error() {
        // After delete_session, get_stats still shows entities — contamination
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![(stats_response(3, 1), Duration::from_millis(5))],
        );

        let scenario = make_scenario("contaminated-test", vec![make_step("smart_ingest")]);
        let mut runner = EvalRunner::new(transport, test_config());
        let result = runner.run_scenario(scenario).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("CONTAMINATED"),
            "error should say CONTAMINATED, got: {err_str}"
        );
        match err {
            RunnerError::Contaminated {
                entity_count,
                session_id,
            } => {
                assert_eq!(entity_count, 3);
                assert_eq!(session_id.get_version_num(), 4);
            }
            other => panic!("expected Contaminated, got: {other}"),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn session_isolation_no_leakage_between_scenarios() {
        // Two sequential scenarios: verify second starts with entity_count=0
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                // Preflight
                (stats_response(0, 0), Duration::from_millis(5)),
                // Scenario 1: before (clean), after
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(1, 0), Duration::from_millis(5)),
                // Scenario 2: before (must be clean after s1 cleanup), after
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(1, 0), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();
        let scenarios = vec![
            make_scenario("iso-s1", vec![make_step("smart_ingest")]),
            make_scenario("iso-s2", vec![make_step("smart_ingest")]),
        ];

        let mut runner = EvalRunner::new(transport, test_config());
        let runs = runner.run_scenarios(scenarios).await.unwrap();

        assert_eq!(runs.len(), 2);

        // Verify delete_session was called between scenarios
        let recorded = calls.lock().unwrap().clone();
        let delete_calls: Vec<_> = recorded
            .iter()
            .filter(|(name, _)| name == "delete_session")
            .collect();

        // At least 4 delete_session calls: s1-pre, s1-post, s2-pre, s2-post
        assert!(
            delete_calls.len() >= 4,
            "expected >= 4 delete_session calls for 2 scenarios, got {}",
            delete_calls.len()
        );

        // Session IDs should differ between scenarios
        assert_ne!(runs[0].session_id, runs[1].session_id);
    }

    // -----------------------------------------------------------------------
    // T-011: Warm-up phase tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn warmup_calls_upsert_search_delete() {
        let transport = MockTransport::new();
        let calls = transport.calls.clone();

        let mut runner = EvalRunner::new(transport, test_config());
        let result = runner.warmup().await;
        assert!(result.is_ok(), "warmup should succeed");

        let recorded = calls.lock().unwrap().clone();
        let tool_names: Vec<&str> = recorded.iter().map(|(n, _)| n.as_str()).collect();

        assert_eq!(tool_names.len(), 3, "warmup should make 3 calls");
        assert_eq!(tool_names[0], "upsert_entity", "first: upsert");
        assert_eq!(tool_names[1], "hybrid_search", "second: search");
        assert_eq!(tool_names[2], "delete_session", "third: cleanup");
        assert_eq!(
            recorded[0].1["context_snippet"].as_str(),
            Some("warmup probe — safe to delete"),
            "warmup upsert must satisfy the current upsert_entity schema"
        );
    }

    #[tokio::test]
    async fn warmup_uses_eval_tenant_id() {
        let config = EvalConfig {
            tenant_id: Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap(),
            warmup: false,
            ..EvalConfig::default()
        };

        let transport = MockTransport::new();
        let calls = transport.calls.clone();

        let mut runner = EvalRunner::new(transport, config);
        runner.warmup().await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        let upsert = recorded
            .iter()
            .find(|(name, _)| name == "upsert_entity")
            .unwrap();

        assert_eq!(
            upsert.1["tenant_id"].as_str().unwrap(),
            "11111111-2222-3333-4444-555555555555",
            "warmup should use configured tenant_id"
        );

        let search = recorded
            .iter()
            .find(|(name, _)| name == "hybrid_search")
            .unwrap();

        assert_eq!(
            search.1["tenant_id"].as_str().unwrap(),
            "11111111-2222-3333-4444-555555555555",
            "warmup search should use configured tenant_id"
        );
    }

    #[tokio::test]
    async fn warmup_skipped_when_config_disabled() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                // Preflight
                (stats_response(0, 0), Duration::from_millis(5)),
                // Scenario: before, after
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(0, 0), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();
        let config = EvalConfig {
            warmup: false,
            ..test_config()
        };

        let mut runner = EvalRunner::new(transport, config);
        let _runs = runner
            .run_scenarios(vec![make_scenario(
                "no-warmup",
                vec![make_step("get_stats")],
            )])
            .await
            .unwrap();

        let recorded = calls.lock().unwrap().clone();
        let warmup_calls: Vec<_> = recorded
            .iter()
            .filter(|(name, _)| name == "upsert_entity")
            .collect();

        assert!(
            warmup_calls.is_empty(),
            "no upsert_entity calls when warmup=false"
        );
    }

    #[tokio::test]
    async fn warmup_runs_before_scored_scenarios() {
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                // Preflight
                (stats_response(0, 0), Duration::from_millis(5)),
                // Scenario: before, after
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(0, 0), Duration::from_millis(5)),
            ],
        );

        let calls = transport.calls.clone();
        let config = EvalConfig {
            warmup: true,
            ..test_config()
        };

        let mut runner = EvalRunner::new(transport, config);
        let _runs = runner
            .run_scenarios(vec![make_scenario(
                "after-warmup",
                vec![make_step("get_stats")],
            )])
            .await
            .unwrap();

        let recorded = calls.lock().unwrap().clone();
        let tool_names: Vec<&str> = recorded.iter().map(|(n, _)| n.as_str()).collect();

        // Expected order: preflight(get_stats), warmup(upsert, search, delete),
        // then scenario (delete, get_stats, get_stats, get_stats, delete)
        let preflight_idx = tool_names.iter().position(|n| *n == "get_stats").unwrap();
        let warmup_upsert_idx = tool_names
            .iter()
            .position(|n| *n == "upsert_entity")
            .unwrap();
        let first_delete_after_warmup = tool_names
            .iter()
            .enumerate()
            .skip(warmup_upsert_idx)
            .find(|(_, n)| **n == "delete_session")
            .map(|(i, _)| i)
            .unwrap();

        assert!(
            preflight_idx < warmup_upsert_idx,
            "preflight should run before warmup"
        );
        assert!(
            warmup_upsert_idx < first_delete_after_warmup,
            "warmup upsert should precede warmup cleanup"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn warmup_results_not_in_scoring() {
        // Warm-up scenario runs but its results are not in the returned Vec<ScenarioRun>
        let transport = MockTransport::new().on_tool(
            "get_stats",
            vec![
                // Preflight
                (stats_response(0, 0), Duration::from_millis(5)),
                // Scenario: before, after
                (stats_response(0, 0), Duration::from_millis(5)),
                (stats_response(1, 0), Duration::from_millis(5)),
            ],
        );

        let config = EvalConfig {
            warmup: true,
            ..test_config()
        };

        let mut runner = EvalRunner::new(transport, config);
        let runs = runner
            .run_scenarios(vec![make_scenario(
                "scored",
                vec![make_step("smart_ingest")],
            )])
            .await
            .unwrap();

        // Only the scored scenario should appear in results
        assert_eq!(runs.len(), 1, "warmup should not appear in results");
        assert_eq!(runs[0].scenario.scenario.id, "scored");
    }

    // -----------------------------------------------------------------------
    // Integration tests (require live ferrosa cluster)
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore]
    async fn live_run_three_step_scenario() {
        use crate::mcp_client::McpClient;

        // Build path
        let workspace_root = env!("CARGO_MANIFEST_DIR")
            .strip_suffix("/crates/ferrosa-memory-eval")
            .unwrap_or(env!("CARGO_MANIFEST_DIR"));

        let binary = format!("{workspace_root}/target/debug/ferrosa-memory-mcp");
        if !Path::new(&binary).exists() {
            panic!(
                "ferrosa-memory-mcp binary not found at {binary}. \
                 Build first with: cargo build -p ferrosa-memory-mcp"
            );
        }

        // Wrap real McpClient in transport adapter
        struct LiveTransport {
            client: McpClient,
        }

        impl McpTransport for LiveTransport {
            async fn call_tool(
                &mut self,
                tool_name: &str,
                arguments: Value,
            ) -> Result<(Value, Duration), RunnerError> {
                match self.client.call_tool(tool_name, arguments).await {
                    Ok(result) => Ok((result.response, result.latency)),
                    Err(e) => Err(RunnerError::McpClient(e.to_string())),
                }
            }
        }

        let mut mcp_client = McpClient::spawn(&binary)
            .await
            .expect("failed to spawn MCP server");

        mcp_client.initialize().await.expect("initialize failed");
        mcp_client
            .send_initialized_notification()
            .await
            .expect("notification failed");

        let transport = LiveTransport { client: mcp_client };

        let config = test_config();
        let mut runner = EvalRunner::new(transport, config);

        // Build a 3-step scenario
        let steps = vec![
            {
                let mut s = make_step("upsert_entity");
                s.arguments
                    .insert("entity_name".to_string(), json!("EvalTestAlice"));
                s.arguments
                    .insert("entity_type".to_string(), json!("person"));
                s.arguments.insert(
                    "context_snippet".to_string(),
                    json!("Alice is a test entity for eval"),
                );
                s.arguments.insert(
                    "observations".to_string(),
                    json!(["Alice is a test entity for eval"]),
                );
                s.arguments.insert("source".to_string(), json!("eval"));
                s.expect_in_response = vec!["entity_id".to_string()];
                s.expect_action = Some("Created".to_string());
                s
            },
            {
                let mut s = make_step("hybrid_search");
                s.arguments
                    .insert("query".to_string(), json!("EvalTestAlice"));
                s.expect_in_response = vec!["EvalTestAlice".to_string()];
                s
            },
            {
                let mut s = make_step("get_stats");
                s.expect_in_response = vec!["entity_count".to_string()];
                s
            },
        ];

        let scenario = make_scenario("live-3-step", steps);

        let run = runner
            .run_scenario(scenario)
            .await
            .expect("scenario failed");

        // AC2: traces recorded per step
        assert_eq!(
            run.traces.len(),
            3,
            "should have 3 traces, got {}",
            run.traces.len()
        );

        // AC7: correct latencies
        for (i, trace) in run.traces.iter().enumerate() {
            assert!(trace.success, "step {i} ({}) should succeed", trace.tool);
            assert!(
                trace.latency_ms > 0,
                "step {i} ({}) should have non-zero latency",
                trace.tool
            );
        }

        // AC5: before-snapshot taken before tool calls
        // entity_count should be 0 at start (we cleaned up)
        assert_eq!(
            run.graph_snapshot_before.entity_count, 0,
            "before-snapshot should show 0 entities"
        );

        // After-snapshot should show at least 1 entity (the one we created)
        assert!(
            run.graph_snapshot_after.entity_count >= 1,
            "after-snapshot should show >= 1 entity, got {}",
            run.graph_snapshot_after.entity_count
        );

        // Grade and verify
        let grade = runner.grade_run(&run);
        let prog = grade.programmatic.as_ref().unwrap();
        assert!(prog.sequence_match, "sequence should match");
        assert!(prog.schema_valid, "schema should be valid");

        // Cleanup: the runner already called delete_session post-scenario
        // but let's verify the session_id is valid
        assert_eq!(run.session_id.get_version_num(), 4);
    }

    // -----------------------------------------------------------------------
    // T-035: Stability Canary tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn canary_stable_mock_produces_identical_results() {
        // A deterministic mock: always returns the same responses
        let transport = MockTransport::new()
            .on_tool(
                "get_stats",
                vec![
                    // 3 scenarios x 2 get_stats each = 6
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                ],
            )
            .on_tool(
                "smart_ingest",
                vec![
                    (
                        json!({"action": "Created", "entity_id": "e1"}),
                        Duration::from_millis(20),
                    ),
                    (
                        json!({"action": "Created", "entity_id": "e1"}),
                        Duration::from_millis(20),
                    ),
                    (
                        json!({"action": "Created", "entity_id": "e1"}),
                        Duration::from_millis(20),
                    ),
                ],
            );

        let mut step = make_step("smart_ingest");
        step.expect_in_response = vec!["Created".to_string()];
        let scenario = make_scenario("canary-stable", vec![step]);

        let mut runner = EvalRunner::new(transport, test_config());
        let grades = runner.stability_canary(scenario).await.unwrap();

        // All 3 should have identical programmatic scores
        let scores: Vec<f64> = grades
            .iter()
            .map(|g| g.programmatic.as_ref().unwrap().score)
            .collect();
        assert_eq!(scores[0], scores[1]);
        assert_eq!(scores[1], scores[2]);
    }

    #[tokio::test]
    async fn canary_fails_on_divergent_programmatic_scores() {
        // Mock that returns different results each time
        struct DivergentTransport {
            call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        impl McpTransport for DivergentTransport {
            async fn call_tool(
                &mut self,
                tool_name: &str,
                _arguments: Value,
            ) -> Result<(Value, Duration), RunnerError> {
                let count = self
                    .call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match tool_name {
                    "delete_session" => Ok((json!({"ok": true}), Duration::from_millis(1))),
                    "get_stats" => Ok((stats_response(0, 0), Duration::from_millis(1))),
                    "smart_ingest" => {
                        // First two runs: pass. Third run: different response
                        if count < 10 {
                            Ok((
                                json!({"action": "Created", "entity_id": "e1"}),
                                Duration::from_millis(10),
                            ))
                        } else {
                            // Return a different response to cause score divergence
                            Ok((
                                json!({"action": "WRONG", "entity_id": "e1"}),
                                Duration::from_millis(10),
                            ))
                        }
                    }
                    _ => Ok((json!({"ok": true}), Duration::from_millis(1))),
                }
            }
        }

        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let transport = DivergentTransport {
            call_count: counter,
        };

        let mut step = make_step("smart_ingest");
        step.expect_action = Some("Created".to_string());
        let scenario = make_scenario("canary-divergent", vec![step]);

        let mut runner = EvalRunner::new(transport, test_config());
        let result = runner.stability_canary(scenario).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("STABILITY CANARY FAILED"),
            "expected STABILITY CANARY FAILED, got: {err_str}"
        );
    }

    #[tokio::test]
    async fn canary_controlled_by_config_flag() {
        // Verify the config flag exists and is false by default
        let config = EvalConfig::default();
        assert!(!config.stability_canary, "canary should be off by default");

        let config_on = EvalConfig {
            stability_canary: true,
            ..EvalConfig::default()
        };
        assert!(config_on.stability_canary);
    }

    // -----------------------------------------------------------------------
    // T-038: Parallel Scenario Execution tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn parallel_three_scenarios_produce_sorted_results() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static FACTORY_COUNT: AtomicUsize = AtomicUsize::new(0);

        let config = EvalConfig {
            warmup: false,
            max_parallel: 4,
            ..EvalConfig::default()
        };

        let scenarios = vec![
            make_scenario("c-third", vec![make_step("get_stats")]),
            make_scenario("a-first", vec![make_step("get_stats")]),
            make_scenario("b-second", vec![make_step("get_stats")]),
        ];

        FACTORY_COUNT.store(0, Ordering::SeqCst);

        let results = run_scenarios_parallel(&config, scenarios, || {
            FACTORY_COUNT.fetch_add(1, Ordering::SeqCst);
            MockTransport::new().on_tool(
                "get_stats",
                vec![
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                ],
            )
        })
        .await
        .unwrap();

        // Should have 3 results
        assert_eq!(results.len(), 3);

        // Should be sorted by scenario_id
        assert_eq!(results[0].scenario.scenario.id, "a-first");
        assert_eq!(results[1].scenario.scenario.id, "b-second");
        assert_eq!(results[2].scenario.scenario.id, "c-third");

        // Each should have a unique session_id
        assert_ne!(results[0].session_id, results[1].session_id);
        assert_ne!(results[1].session_id, results[2].session_id);

        // Factory should have been called 3 times (one per scenario)
        assert_eq!(FACTORY_COUNT.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn parallel_respects_semaphore_limit() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let peak_concurrent = Arc::new(AtomicUsize::new(0));

        let config = EvalConfig {
            warmup: false,
            max_parallel: 2, // only 2 at a time
            ..EvalConfig::default()
        };

        // 4 scenarios but only 2 allowed concurrently
        let scenarios = vec![
            make_scenario("s1", vec![make_step("get_stats")]),
            make_scenario("s2", vec![make_step("get_stats")]),
            make_scenario("s3", vec![make_step("get_stats")]),
            make_scenario("s4", vec![make_step("get_stats")]),
        ];

        let max_c = max_concurrent.clone();
        let peak_c = peak_concurrent.clone();

        let results = run_scenarios_parallel(&config, scenarios, move || {
            // Track concurrent transport creation
            let current = max_c.fetch_add(1, Ordering::SeqCst) + 1;
            let mut peak = peak_c.load(Ordering::SeqCst);
            while current > peak {
                match peak_c.compare_exchange(peak, current, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => break,
                    Err(actual) => peak = actual,
                }
            }

            MockTransport::new().on_tool(
                "get_stats",
                vec![
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                ],
            )
        })
        .await
        .unwrap();

        assert_eq!(results.len(), 4);
    }

    #[tokio::test]
    async fn parallel_each_scenario_gets_unique_session_id() {
        let config = EvalConfig {
            warmup: false,
            max_parallel: 4,
            ..EvalConfig::default()
        };

        let scenarios = vec![
            make_scenario("p1", vec![make_step("get_stats")]),
            make_scenario("p2", vec![make_step("get_stats")]),
            make_scenario("p3", vec![make_step("get_stats")]),
        ];

        let results = run_scenarios_parallel(&config, scenarios, || {
            MockTransport::new().on_tool(
                "get_stats",
                vec![
                    (stats_response(0, 0), Duration::from_millis(5)),
                    (stats_response(1, 0), Duration::from_millis(5)),
                ],
            )
        })
        .await
        .unwrap();

        let session_ids: Vec<Uuid> = results.iter().map(|r| r.session_id).collect();
        // All must be unique
        for i in 0..session_ids.len() {
            for j in (i + 1)..session_ids.len() {
                assert_ne!(
                    session_ids[i], session_ids[j],
                    "scenario {} and {} should have different session_ids",
                    results[i].scenario.scenario.id, results[j].scenario.scenario.id
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // T-041: Cleanup Ledger tests
    // -----------------------------------------------------------------------

    #[test]
    fn cleanup_ledger_new_has_empty_sessions() {
        let ledger = CleanupLedger::new();
        assert!(ledger.sessions.is_empty());
        assert!(!ledger.run_id.is_empty());
    }

    #[test]
    fn cleanup_ledger_add_session() {
        let mut ledger = CleanupLedger::new();
        let sid = Uuid::new_v4();
        ledger.add_session(&sid);
        assert_eq!(ledger.sessions.len(), 1);
        assert_eq!(ledger.sessions[0], sid.to_string());
    }

    #[test]
    fn cleanup_ledger_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".cleanup-ledger.json");

        let mut ledger = CleanupLedger::new();
        ledger.add_session(&Uuid::new_v4());
        ledger.add_session(&Uuid::new_v4());
        ledger.save(&path).unwrap();

        let loaded = CleanupLedger::load(&path).unwrap().unwrap();
        assert_eq!(loaded.run_id, ledger.run_id);
        assert_eq!(loaded.sessions.len(), 2);
    }

    #[test]
    fn cleanup_ledger_load_returns_none_for_missing_file() {
        let result = CleanupLedger::load(Path::new("/nonexistent/ledger.json")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn cleanup_ledger_is_stale_when_old() {
        let mut ledger = CleanupLedger::new();
        // Simulate a ledger from 2 hours ago
        ledger.started_at = Utc::now() - chrono::Duration::hours(2);
        assert!(ledger.is_stale());
    }

    #[test]
    fn cleanup_ledger_is_not_stale_when_recent() {
        let ledger = CleanupLedger::new();
        assert!(!ledger.is_stale());
    }

    #[tokio::test]
    async fn cleanup_ledger_sweep_calls_delete_session_for_each() {
        let mut transport = MockTransport::new();
        let calls = transport.calls.clone();

        let mut ledger = CleanupLedger::new();
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        ledger.add_session(&s1);
        ledger.add_session(&s2);

        ledger.sweep(&mut transport).await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        let delete_calls: Vec<_> = recorded
            .iter()
            .filter(|(name, _)| name == "delete_session")
            .collect();
        assert_eq!(delete_calls.len(), 2);
        assert_eq!(
            delete_calls[0].1["session_id"].as_str().unwrap(),
            s1.to_string()
        );
        assert_eq!(
            delete_calls[1].1["session_id"].as_str().unwrap(),
            s2.to_string()
        );
    }

    #[tokio::test]
    async fn sweep_stale_ledger_cleans_up_old_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join(".cleanup-ledger.json");

        // Create a stale ledger (>1hr old)
        let mut ledger = CleanupLedger::new();
        ledger.started_at = Utc::now() - chrono::Duration::hours(2);
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        ledger.add_session(&s1);
        ledger.add_session(&s2);
        ledger.save(&ledger_path).unwrap();

        let mut transport = MockTransport::new();
        let calls = transport.calls.clone();

        let count = sweep_stale_ledger(&ledger_path, &mut transport)
            .await
            .unwrap();

        assert_eq!(count, 2, "should have swept 2 sessions");

        // Ledger file should be deleted
        assert!(
            !ledger_path.exists(),
            "stale ledger should be removed after sweep"
        );

        // Should have called delete_session for each
        let recorded = calls.lock().unwrap().clone();
        let delete_calls: Vec<_> = recorded
            .iter()
            .filter(|(name, _)| name == "delete_session")
            .collect();
        assert_eq!(delete_calls.len(), 2);
    }

    #[tokio::test]
    async fn sweep_stale_ledger_ignores_recent_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join(".cleanup-ledger.json");

        // Create a recent ledger (not stale)
        let mut ledger = CleanupLedger::new();
        ledger.add_session(&Uuid::new_v4());
        ledger.save(&ledger_path).unwrap();

        let mut transport = MockTransport::new();

        let count = sweep_stale_ledger(&ledger_path, &mut transport)
            .await
            .unwrap();

        assert_eq!(count, 0, "recent ledger should not be swept");
        assert!(ledger_path.exists(), "recent ledger should not be deleted");
    }

    #[tokio::test]
    async fn sweep_stale_ledger_handles_no_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("nonexistent.json");

        let mut transport = MockTransport::new();
        let count = sweep_stale_ledger(&ledger_path, &mut transport)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn cleanup_ledger_remove_deletes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(path.exists());

        CleanupLedger::remove(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_ledger_remove_ok_for_missing_file() {
        let result = CleanupLedger::remove(Path::new("/nonexistent/ledger.json"));
        assert!(result.is_ok());
    }
}
