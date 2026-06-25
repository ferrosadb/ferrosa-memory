//! Tool dispatch — registry of MCP tools with schema validation.
//!
//! Maps MCP tool names to handler functions. Validates input schemas before
//! dispatch. Returns tool definitions for `tools/list`.
//!
//! ## MCP protocol methods handled
//!
//! - `initialize` — server capability handshake
//! - `tools/list` — returns all tool schemas
//! - `tools/call` — dispatches to the named tool handler
//! - `notifications/initialized` — client acknowledgment (no-op)

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures_util::{StreamExt, future::join_all, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::context_segment::{
    ContextMessage, ContextSegmentSearchParams, ContextWindowParams, IngestContextSegmentsParams,
    SegmentationConfig,
};
use crate::transport::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};

const CONSOLIDATION_QUEUE_CAPACITY: usize = 1024;
const SMART_INGEST_AUTO_CONSOLIDATE_THRESHOLD: usize = 10;
const BATCH_MUTATION_CONCURRENCY: usize = 16;
const MIN_RETRIEVAL_LIMIT: usize = 1;
const MAX_RETRIEVAL_LIMIT: usize = 50;
const DEFAULT_RETRIEVAL_LIMIT: usize = 10;
// LLM rerank tunables were promoted to `[search]` config; see
// `crate::config::SearchConfig` and `RerankTunables`. Defaults preserved there.
/// Connection-establishment budget for the judge endpoint. Kept small so a
/// judge-on-by-default search skips quickly when the endpoint is down, rather
/// than blocking on the much longer generation timeout.
const JUDGE_CONNECT_TIMEOUT_SECONDS: u64 = 2;
const EDGE_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type ToolDispatchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, (i32, String)>> + Send + 'a>>;

#[derive(Clone, Copy)]
enum BatchMutationKind {
    Updated,
    Unchanged,
    NotFound,
    Error,
    Deleted,
    Missing,
    Invalid,
    Upserted,
}

struct BatchMutationOutcome {
    index: usize,
    kind: BatchMutationKind,
    result: Value,
}

fn ordered_batch_results(mut outcomes: Vec<BatchMutationOutcome>) -> Vec<Value> {
    outcomes.sort_by_key(|outcome| outcome.index);
    outcomes.into_iter().map(|outcome| outcome.result).collect()
}

/// Rotating hint counter for memory formation encouragement.
static HINT_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Pick a hint from a pool, rotating through them.
fn pick_hint(hints: &[&str]) -> String {
    let idx = HINT_COUNTER.fetch_add(1, Ordering::Relaxed) % hints.len();
    hints[idx].to_string()
}

/// Hints shown after successful smart_ingest (one per response, rotating).
const INGEST_HINTS: &[&str] = &[
    "Did you learn something about the user's preferences or working style? Ingest that too.",
    "Technical decisions and their reasoning are high-value memories — don't skip those.",
    "User corrections ('no, do it this way') are especially important to remember.",
    "Project constraints, deadlines, or blockers? Those are worth remembering.",
    "Did you notice an architecture pattern or library gotcha? Ingest it.",
    "People, roles, and relationships mentioned? Those build context over time.",
    "Debugging insights — what caused a bug, what fixed it — save future you the trouble.",
    "Configuration gotchas and environment details are easy to forget. Ingest them.",
    "What did you learn about how this codebase works? That's worth an ingest.",
    "Any surprising behavior or non-obvious design choices? Those are prime memory candidates.",
];

/// Tracks per-entity retrieval counts for anomaly detection (FMEA F19).
#[derive(Default)]
pub struct RetrievalTracker {
    counts: std::collections::HashMap<uuid::Uuid, usize>,
    /// Ordered list of recently accessed entity IDs (most recent last).
    recent: Vec<uuid::Uuid>,
}

impl RetrievalTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, entity_id: uuid::Uuid) {
        *self.counts.entry(entity_id).or_insert(0) += 1;
        // Deduplicate in recent list — keep only the latest position.
        self.recent.retain(|&id| id != entity_id);
        self.recent.push(entity_id);
        // Cap at 50 to bound memory.
        if self.recent.len() > 50 {
            self.recent.remove(0);
        }
    }

    pub fn count(&self, entity_id: &uuid::Uuid) -> usize {
        self.counts.get(entity_id).copied().unwrap_or(0)
    }

    /// Returns the most recently accessed entity IDs (most recent last).
    pub fn recent_ids(&self, limit: usize) -> Vec<uuid::Uuid> {
        let start = self.recent.len().saturating_sub(limit);
        self.recent[start..].to_vec()
    }

    pub fn mean(&self) -> f64 {
        if self.counts.is_empty() {
            return 0.0;
        }
        let sum: usize = self.counts.values().sum();
        sum as f64 / self.counts.len() as f64
    }

    pub fn stddev(&self) -> f64 {
        if self.counts.len() < 2 {
            return 0.0;
        }
        let mean = self.mean();
        let variance = self
            .counts
            .values()
            .map(|&c| {
                let diff = c as f64 - mean;
                diff * diff
            })
            .sum::<f64>()
            / self.counts.len() as f64;
        variance.sqrt()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsolidationRunStatus {
    pub session_id: uuid::Uuid,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub entities_processed: usize,
    pub connections_created: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LastRetrievalResult {
    pub entity_id: uuid::Uuid,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LastRetrievalCall {
    pub query_id: uuid::Uuid,
    pub query: String,
    pub cwd: Option<String>,
    pub results: Vec<LastRetrievalResult>,
    pub recorded_at: String,
}

/// Per-session mutable state (not persisted in CQL).
pub struct SessionState {
    pub intentions: Arc<Mutex<crate::intention::IntentionStore>>,
    pub graph: Option<Arc<crate::graph::GraphClient>>,
    pub event_bus: Arc<crate::viz::EventBus>,
    pub retrieval_tracker: Arc<Mutex<RetrievalTracker>>,
    pub co_access: Arc<Mutex<crate::speculative::CoAccessTracker>>,
    /// Configured default session_id for cross-session memory continuity.
    pub default_session_id: Option<uuid::Uuid>,
    /// Runtime session_id selected by a mechanical session-start hook.
    pub runtime_session_id: Arc<std::sync::RwLock<Option<uuid::Uuid>>>,
    /// Repository path for intention scoping (from CLAUDE_PROJECT_DIR, config, or MCP initialize roots).
    pub repo: std::sync::OnceLock<String>,
    /// Notified on every tool call; used by the idle consolidation timer.
    pub last_activity: Arc<tokio::sync::Notify>,
    /// Set to true when a write tool succeeds; cleared by idle consolidation.
    pub dirty: Arc<AtomicBool>,
    /// Session IDs explicitly queued for background consolidation.
    pub consolidation_queue: Arc<Mutex<VecDeque<uuid::Uuid>>>,
    /// Per-session count of newly created smart-ingest entities since the
    /// session was last queued for consolidation.
    pub smart_ingest_created_since_consolidation: Arc<Mutex<HashMap<uuid::Uuid, usize>>>,
    /// Latest background consolidation status by session.
    pub last_consolidation_status: Arc<Mutex<HashMap<uuid::Uuid, ConsolidationRunStatus>>>,
    /// Last retrieval result ids/sources by session, used by feedback-on-last-call.
    pub last_retrieval: Arc<Mutex<HashMap<uuid::Uuid, LastRetrievalCall>>>,
    /// Embedding provider used for semantic vectors (`ollama`, `synthetic`, or disabled).
    pub embed_provider: String,
    /// Base URL for the Ollama API (used for Ollama embeddings and NER extraction).
    pub ollama_base_url: String,
    /// Model name for NER entity extraction via Ollama.
    pub ner_model: String,
    /// Model name for text embedding (default nomic-embed-text-v2-moe).
    pub embed_model: String,
    /// Expected embedding dimensions (default 768).
    pub embed_dimensions: u32,
    /// Dynamic entity types loaded from the type registry table.
    pub entity_types: Vec<String>,
    /// Dynamic edge types loaded from the type registry table.
    pub edge_types: Vec<String>,
    /// Base URL for the enrichment LLM (OpenAI-compatible API).
    pub enrich_llm_url: String,
    /// Model name for enrichment LLM.
    pub enrich_llm_model: String,
    /// Runtime judge model configuration used by eval and operator workflows.
    pub judge_config: Arc<Mutex<crate::config::JudgeConfig>>,
    /// Runtime default ranked-result count for retrieval tools when omitted by the caller.
    pub retrieval_default_limit: Arc<AtomicUsize>,
    /// Runtime search & rerank tunables (`[search]` section), editable via the workbench.
    pub search: Arc<Mutex<crate::config::SearchConfig>>,
    /// Immutable startup snapshot used by the `system_describe` management tool.
    pub system_info: Arc<crate::system_describe::SystemInfo>,
    /// Forget-feature configuration (`[forget]` section): purge window,
    /// candidate caps, token TTL, high-impact threshold. Editable via the workbench.
    pub forget: Arc<Mutex<crate::config::ForgetConfig>>,
    /// Secret key for signing/verifying stateless `forget` tokens. Keyed-SHA256
    /// over the token payload; rotating it invalidates outstanding tokens.
    pub forget_token_key: Vec<u8>,
    /// Path to the config file used for workbench persistence (managed block).
    /// `None` when no config file was resolved at startup; persistence is then
    /// skipped and reported to the operator rather than silently faked.
    pub config_path: Option<std::path::PathBuf>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            intentions: Arc::new(Mutex::new(crate::intention::IntentionStore::new())),
            graph: None,
            event_bus: Arc::new(crate::viz::EventBus::new()),
            retrieval_tracker: Arc::new(Mutex::new(RetrievalTracker::new())),
            co_access: Arc::new(Mutex::new(crate::speculative::CoAccessTracker::new(10))),
            default_session_id: None,
            runtime_session_id: Arc::new(std::sync::RwLock::new(None)),
            repo: std::sync::OnceLock::new(),
            last_activity: Arc::new(tokio::sync::Notify::new()),
            dirty: Arc::new(AtomicBool::new(false)),
            consolidation_queue: Arc::new(Mutex::new(VecDeque::new())),
            smart_ingest_created_since_consolidation: Arc::new(Mutex::new(HashMap::new())),
            last_consolidation_status: Arc::new(Mutex::new(HashMap::new())),
            last_retrieval: Arc::new(Mutex::new(HashMap::new())),
            embed_provider: "ollama".to_string(),
            ollama_base_url: "http://127.0.0.1:11434".to_string(),
            ner_model: "qwen3.5:27b".to_string(),
            embed_model: "nomic-embed-text-v2-moe".to_string(),
            embed_dimensions: 768,
            entity_types: crate::cql_storage::CqlStorage::default_entity_types(),
            edge_types: Vec::new(),
            enrich_llm_url: "http://localhost:1234".to_string(),
            enrich_llm_model: "google/gemma-4-31b".to_string(),
            judge_config: Arc::new(Mutex::new(crate::config::JudgeConfig::default())),
            retrieval_default_limit: Arc::new(AtomicUsize::new(DEFAULT_RETRIEVAL_LIMIT)),
            search: Arc::new(Mutex::new(crate::config::SearchConfig::default())),
            system_info: Arc::new(crate::system_describe::SystemInfo::default()),
            forget: Arc::new(Mutex::new(crate::config::ForgetConfig::default())),
            // Fixed, deterministic key for tests; production overrides this with
            // random bytes in the MCP server's SessionState constructors.
            forget_token_key: b"forget-test-key-0000000000000000".to_vec(),
            config_path: None,
        }
    }
}

impl SessionState {
    pub fn effective_default_session_id(&self) -> Option<uuid::Uuid> {
        self.runtime_session_id
            .read()
            .ok()
            .and_then(|guard| *guard)
            .or(self.default_session_id)
    }

    fn set_runtime_session_id(&self, session_id: uuid::Uuid) -> Result<(), (i32, String)> {
        let mut guard = self.runtime_session_id.write().map_err(|_| {
            (
                INTERNAL_ERROR,
                "runtime session_id lock poisoned".to_string(),
            )
        })?;
        *guard = Some(session_id);
        Ok(())
    }
}

fn retrieval_default_limit(session: &SessionState) -> usize {
    session
        .retrieval_default_limit
        .load(Ordering::Relaxed)
        .clamp(MIN_RETRIEVAL_LIMIT, MAX_RETRIEVAL_LIMIT)
}

fn optional_retrieval_limit(
    args: &Value,
    keys: &[&str],
    session: &SessionState,
) -> Result<usize, (i32, String)> {
    for key in keys {
        if let Some(raw) = args.get(*key).and_then(|v| v.as_u64()) {
            let value = raw as usize;
            if !(MIN_RETRIEVAL_LIMIT..=MAX_RETRIEVAL_LIMIT).contains(&value) {
                return Err((
                    INVALID_PARAMS,
                    format!(
                        "{key} must be between {MIN_RETRIEVAL_LIMIT} and {MAX_RETRIEVAL_LIMIT}"
                    ),
                ));
            }
            return Ok(value);
        }
    }
    Ok(retrieval_default_limit(session))
}

/// MCP tool definition for `tools/list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

fn short_tool_name(canonical: &str) -> Option<&'static str> {
    match canonical {
        "check_memo_cache" => Some("memo"),
        "store_memo_result" => Some("memo_store"),
        "write_plan_node" => Some("plan_write"),
        "get_plan_context" => Some("plan"),
        "update_plan_node" => Some("plan_update"),
        "session_task_put" => Some("task_put"),
        "session_task_get" => Some("task_get"),
        "session_task_current" => Some("task_current"),
        "session_task_list" => Some("task_list"),
        "session_task_complete" => Some("task_done"),
        "session_task_cancel" => Some("task_cancel"),
        "session_task_focus" => Some("task_focus"),
        "session_task_observe" => Some("task_observe"),
        "start_fold" => Some("fold_start"),
        "append_to_fold" => Some("fold_append"),
        "complete_fold" => Some("fold_done"),
        "retrieve_fold_context" => Some("fold"),
        "ingest_context_segments" => Some("ctx_ingest"),
        "search_context_segments" => Some("ctx_search"),
        "get_context_window" => Some("ctx_window"),
        "get_chunk_context" => Some("chunk_ctx"),
        "get_turn_chain" => Some("turn_chain"),
        "upsert_entity" => Some("upsert"),
        "batch_ingest" => Some("ingest_batch"),
        "ingest_entities" => Some("ingest_many"),
        "create_edge" => Some("edge"),
        "batch_create_edges" => Some("edges_add"),
        "batch_update_edges" => Some("edges_update"),
        "batch_delete_edges" => Some("edges_delete"),
        "batch_update_entities" => Some("entities_update"),
        "batch_delete_entities" => Some("entities_delete"),
        "retrieve_entities" => Some("find"),
        "list_entities" => Some("list"),
        "record_outcome" => Some("outcome"),
        "record_feedback" => Some("feedback"),
        "configure" => Some("config"),
        "delete_session" => Some("delete_session"),
        "smart_ingest" => Some("ingest"),
        "ingest_skill" => Some("skill_ingest"),
        "retrieve_skills_for_context" => Some("skills"),
        "invoke_skill" => Some("skill"),
        "ensure_parent_tag" => Some("tag_parent"),
        "verify_skill" => Some("skill_verify"),
        "set_intention" => Some("intend"),
        "check_intentions" => Some("check"),
        "complete_intention" => Some("done"),
        "list_intentions" => Some("intentions"),
        "snooze_intention" => Some("snooze"),
        "write_temporal_fact" => Some("fact"),
        "get_temporal_chain" => Some("history"),
        "explore_connections" => Some("explore"),
        "hybrid_search" => Some("search"),
        "manage_authority" => Some("authority"),
        "run_consolidation" => Some("consolidate"),
        "enrich_entities" => Some("enrich"),
        "get_stats" => Some("stats"),
        "memory_metrics" => Some("metrics"),
        "migration_status" => Some("migrations"),
        "count_entities_by_type" => Some("type_counts"),
        "promote_memory" => Some("promote"),
        "demote_memory" => Some("demote"),
        "importance_score" => Some("importance"),
        "find_memory_chain" => Some("chain"),
        "predict_needed" => Some("predict"),
        "spread_activation" => Some("spread"),
        "find_duplicates" => Some("duplicates"),
        "recursive_explore" => Some("recurse"),
        "query_derived" => Some("derive"),
        "manage_rules" => Some("rules"),
        "manage_claims" => Some("claims"),
        "manage_approvals" => Some("approvals"),
        "manage_aliases" => Some("aliases"),
        "explain_derived" => Some("explain"),
        "get_effective_rule_set" => Some("ruleset"),
        "promote_predicate" => Some("pred_promote"),
        "list_derived_cache" => Some("derived_cache"),
        "restore_forgotten" => Some("restore"),
        _ => None,
    }
}

fn canonical_tool_name(name: &str) -> &str {
    match name {
        "memo" => "check_memo_cache",
        "memo_store" => "store_memo_result",
        "plan_write" => "write_plan_node",
        "plan" => "get_plan_context",
        "plan_update" => "update_plan_node",
        "task_put" => "session_task_put",
        "task_get" => "session_task_get",
        "task_current" => "session_task_current",
        "task_list" => "session_task_list",
        "task_done" => "session_task_complete",
        "task_cancel" => "session_task_cancel",
        "task_focus" => "session_task_focus",
        "task_observe" => "session_task_observe",
        "fold_start" => "start_fold",
        "fold_append" => "append_to_fold",
        "fold_done" => "complete_fold",
        "fold" => "retrieve_fold_context",
        "ctx_ingest" => "ingest_context_segments",
        "ctx_search" => "search_context_segments",
        "ctx_window" => "get_context_window",
        "chunk_ctx" => "get_chunk_context",
        "turn_chain" => "get_turn_chain",
        "upsert" => "upsert_entity",
        "ingest_batch" => "batch_ingest",
        "ingest_many" => "ingest_entities",
        "edge" => "create_edge",
        "edges_add" => "batch_create_edges",
        "edges_update" => "batch_update_edges",
        "edges_delete" => "batch_delete_edges",
        "entities_update" => "batch_update_entities",
        "entities_delete" => "batch_delete_entities",
        "find" => "retrieve_entities",
        "list" => "list_entities",
        "outcome" => "record_outcome",
        "feedback" => "record_feedback",
        "config" => "configure",
        "ingest" => "smart_ingest",
        "skill_ingest" => "ingest_skill",
        "skills" => "retrieve_skills_for_context",
        "skill" => "invoke_skill",
        "tag_parent" => "ensure_parent_tag",
        "skill_verify" => "verify_skill",
        "intend" => "set_intention",
        "check" => "check_intentions",
        "done" => "complete_intention",
        "intentions" => "list_intentions",
        "snooze" => "snooze_intention",
        "fact" => "write_temporal_fact",
        "history" => "get_temporal_chain",
        "explore" => "explore_connections",
        "search" => "hybrid_search",
        "authority" => "manage_authority",
        "consolidate" => "run_consolidation",
        "enrich" => "enrich_entities",
        "stats" => "get_stats",
        "metrics" => "memory_metrics",
        "migrations" => "migration_status",
        "type_counts" => "count_entities_by_type",
        "promote" => "promote_memory",
        "demote" => "demote_memory",
        "importance" => "importance_score",
        "chain" => "find_memory_chain",
        "predict" => "predict_needed",
        "spread" => "spread_activation",
        "duplicates" => "find_duplicates",
        "recurse" => "recursive_explore",
        "derive" => "query_derived",
        "rules" => "manage_rules",
        "claims" => "manage_claims",
        "approvals" => "manage_approvals",
        "aliases" => "manage_aliases",
        "explain" => "explain_derived",
        "ruleset" => "get_effective_rule_set",
        "pred_promote" => "promote_predicate",
        "derived_cache" => "list_derived_cache",
        // Management self-description. Canonical name is `describe`; also accept
        // the dotted contract names from the spec as aliases.
        "system_describe" | "system.describe" | "ferrosa_memory.system.describe" => "describe",
        "restore" => "restore_forgotten",
        other => other,
    }
}

/// Build all tool definitions for the memory server.
/// Entity types are loaded dynamically from the type registry.
pub fn tool_definitions(entity_types: &[String]) -> Vec<ToolDef> {
    let entity_type_enum: Value = serde_json::json!(entity_types);
    let mut tools = vec![
        ToolDef {
            name: "all_tools".into(),
            description: "Return the full Ferrosa Memory tool catalog when the compact default tools are not enough.\n\nCALL WHEN: You need deeper memory operations, batching, folds, derived facts, governance, or diagnostics not exposed in the compact default tool set.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        // --- Remote teacher/learner memory tools ---
        ToolDef {
            name: "feedback_record".into(),
            description: "Record terse feedback about a remote-memory candidate, classify it into a structured Packet H signal, and persist a queryable feedback explanation under the authenticated tenant.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Must match the authenticated tenant context." },
                    "remote_id": { "type": "string" },
                    "target_id": { "type": "string", "description": "Remote item/entity/candidate UUID receiving feedback." },
                    "source_namespace": { "type": "string", "minLength": 1 },
                    "scope": { "type": "string", "minLength": 1 },
                    "feedback": { "type": "string", "minLength": 1, "maxLength": 4096 }
                },
                "required": ["tenant_id", "remote_id", "target_id", "source_namespace", "scope", "feedback"]
            }),
        },
        ToolDef {
            name: "usage_mark".into(),
            description: "Mark a remote-memory item as selected, confirmed, or successful and return a scoped trust reinforcement preview. Tenant id must match authenticated context.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "remote_id": { "type": "string" },
                    "target_id": { "type": "string" },
                    "source_namespace": { "type": "string", "minLength": 1 },
                    "scope": { "type": "string", "minLength": 1 },
                    "usage": { "type": "string", "enum": ["chosen", "confirmed", "success"] }
                },
                "required": ["tenant_id", "remote_id", "target_id", "source_namespace", "scope", "usage"]
            }),
        },
        ToolDef {
            name: "trust_update".into(),
            description: "Apply scoped Packet H trust reinforcements for one remote namespace/scope and persist a not_trusted_for policy fact when repeated strong negatives cross threshold.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "remote_id": { "type": "string" },
                    "source_namespace": { "type": "string", "minLength": 1 },
                    "scope": { "type": "string", "minLength": 1 },
                    "reinforcements": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "string", "enum": ["chosen", "policy_chosen", "confirmed", "user_confirmed", "success", "wrong_scope", "strong_negative"] }
                    }
                },
                "required": ["tenant_id", "remote_id", "source_namespace", "scope", "reinforcements"]
            }),
        },
        ToolDef {
            name: "teach_query_stream".into(),
            description: "Teacher-side remote memory query stream. Returns a transport-neutral JSON event array beginning with a start event before retrieval completion; raw context/detail/skill output requires explicit grants.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "remote_id": { "type": "string", "format": "uuid" },
                    "learner_instance_id": { "type": "string", "format": "uuid" },
                    "query": { "type": "string", "maxLength": 4096 },
                    "namespaces": { "type": "array", "items": { "type": "string" } },
                    "max_items": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "query_embedding": { "type": "array", "items": { "type": "number" } },
                    "grants": { "type": "array", "items": { "type": "string", "enum": ["raw_context", "detail", "skill"] } },
                    "include_raw_context": { "type": "boolean" },
                    "include_detail": { "type": "boolean" },
                    "include_skill": { "type": "boolean" }
                },
                "required": ["remote_id", "query"]
            }),
        },
        ToolDef {
            name: "pull_preview".into(),
            description: "Learner-side remote memory pull preview. Verifies a signed teaching packet, evaluates dry-run import policy, and reports duplicate/conflict candidates without mutating local storage.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "remote_id": { "type": "string", "format": "uuid" },
                    "remote_name": { "type": "string", "maxLength": 256 },
                    "query": { "type": "string", "maxLength": 4096 },
                    "public_identity": { "type": "object", "description": "Teacher InstancePublicIdentity used to verify signed_packet" },
                    "signed_packet": { "type": "object", "description": "SignedEnvelope<TeachingPacket> from teach_query_stream or remote transport" },
                    "local_applicability": { "type": "object" },
                    "preview_ttl_seconds": { "type": "integer", "minimum": 1, "maximum": 86400 }
                },
                "required": ["remote_id", "remote_name", "query", "public_identity", "signed_packet"]
            }),
        },
        ToolDef {
            name: "pull_commit".into(),
            description: "Commit an accepted learner-side remote memory pull preview. Writes active imports with provenance, persists stubs/quarantine decisions, and records an import batch.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "preview": { "type": "object", "description": "PullPreviewPlan returned by pull_preview" },
                    "learner_decision": { "type": "object", "description": "SignedEnvelope<ImportDecisionPayload> authorizing this commit" }
                },
                "required": ["preview", "learner_decision"]
            }),
        },
        ToolDef {
            name: "remote_list".into(),
            description: "List tenant-scoped configured remote memory providers without exposing credentials.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                }
            }),
        },
        ToolDef {
            name: "remote_add".into(),
            description: "Register or replace a tenant-scoped remote memory endpoint, trust class, instance id, and public-key fingerprint. Endpoints must be HTTPS/HTTP URLs; secrets are not accepted.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "format": "uuid" },
                    "remote_id": { "type": "string", "format": "uuid" },
                    "instance_id": { "type": "string", "format": "uuid" },
                    "name": { "type": "string", "minLength": 1, "maxLength": 256 },
                    "endpoint": { "type": "string", "description": "HTTP(S) endpoint for the remote MCP server; do not include credentials." },
                    "trust_class": { "type": "string", "enum": ["personal", "team", "partner", "public", "archive"] },
                    "public_key_fingerprint": { "type": "string", "minLength": 1, "maxLength": 256 }
                },
                "required": ["tenant_id", "instance_id", "name", "endpoint", "trust_class", "public_key_fingerprint"]
            }),
        },
        ToolDef {
            name: "remote_update_policy".into(),
            description: "Append tenant-scoped Datalog policy facts for a configured remote. Supported actions: read, detail_fetch, autocommit, requires_activation, should_consult, trusted_for, not_trusted_for, fallback_enabled.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "format": "uuid" },
                    "remote_id": { "type": "string", "format": "uuid" },
                    "facts": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["grant", "deny"] },
                                "namespace": { "type": "string", "minLength": 1 },
                                "action": { "type": "string", "minLength": 1 },
                                "expires_at": { "type": "string", "format": "date-time" }
                            },
                            "required": ["kind", "namespace", "action"]
                        }
                    }
                },
                "required": ["tenant_id", "remote_id", "facts"]
            }),
        },
        ToolDef {
            name: "remote_remove".into(),
            description: "Disable a configured remote while preserving import provenance and policy audit rows.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "format": "uuid" },
                    "remote_id": { "type": "string", "format": "uuid" }
                },
                "required": ["tenant_id", "remote_id"]
            }),
        },
        ToolDef {
            name: "remote_health".into(),
            description: "Report local configuration health for one remote without dialing or leaking credentials.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "remote_id": { "type": "string", "format": "uuid" } },
                "required": ["remote_id"]
            }),
        },
        ToolDef {
            name: "remote_capabilities".into(),
            description: "Return the remote-memory MCP capabilities expected for a configured remote.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "remote_id": { "type": "string", "format": "uuid" } },
                "required": ["remote_id"]
            }),
        },
        ToolDef {
            name: "remote_detail".into(),
            description: "Return configured remote-memory details plus the transport/security capabilities required by remote pull smokes.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "remote_id": { "type": "string", "format": "uuid" } },
                "required": ["remote_id"]
            }),
        },
        ToolDef {
            name: "remote_explain_policy".into(),
            description: "Evaluate and explain Datalog-backed remote policy for a configured remote, action, and namespace.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "remote_id": { "type": "string", "format": "uuid" },
                    "action": { "type": "string", "enum": ["read", "detail_fetch", "autocommit", "requires_activation", "should_consult"] },
                    "namespace": { "type": "string", "minLength": 1 }
                },
                "required": ["remote_id", "action", "namespace"]
            }),
        },
        ToolDef {
            name: "ingest_context_segments".into(),
            description: "Persist raw pre-compaction conversation context as deterministic semantic segments, with Nomic embeddings when configured and temporal prev/next links for later expansion.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "conversation_id": { "type": "string", "maxLength": 512 },
                    "messages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": { "type": "string" },
                                "content": { "type": "string", "maxLength": 131072 },
                                "turn_index": { "type": "integer" },
                                "created_at": { "type": "string", "description": "Optional RFC3339 timestamp" },
                                "metadata": { "type": "object" }
                            },
                            "required": ["role", "content", "turn_index"]
                        },
                        "minItems": 1
                    },
                    "segmentation": { "type": "object" },
                    "embed_missing": { "type": "boolean" }
                },
                "required": ["conversation_id", "messages"]
            }),
        },
        ToolDef {
            name: "search_context_segments".into(),
            description: "Hybrid-search raw context segments with lexical BM25 fallback plus Nomic vector ANN, optionally returning bounded prev/next temporal expansion windows.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "query": { "type": "string", "maxLength": 4096 },
                    "query_embedding": { "type": "array", "items": { "type": "number" } },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "expand": {
                        "type": "object",
                        "properties": {
                            "prev": { "type": "integer", "minimum": 0, "maximum": 10 },
                            "next": { "type": "integer", "minimum": 0, "maximum": 10 },
                            "max_tokens": { "type": "integer", "minimum": 1, "maximum": 50000 }
                        }
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "get_context_window".into(),
            description: "Return ordered previous/hit/next context segment pages around a retrieved segment using temporal edges, bounded by token budget.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "segment_id": { "type": "string", "format": "uuid" },
                    "prev": { "type": "integer", "minimum": 0, "maximum": 20 },
                    "next": { "type": "integer", "minimum": 0, "maximum": 20 },
                    "max_tokens": { "type": "integer", "minimum": 1, "maximum": 100000 }
                },
                "required": ["segment_id"]
            }),
        },
        ToolDef {
            name: "get_turn_chain".into(),
            description: "Walk the next_turn temporal edge chain from a starting turn entity, returning turns in forward (chronological arrival) order.\n\nCALL WHEN: You need to reconstruct what happened in an agent session after a known turn, follow a conversation thread, or inspect the sequence of turns the harness hook captured.\nRETURNS: ordered list of turn entities from start_turn_id forward, up to limit turns.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session partition for the turn chain. Omit or pass \"default\" to use the configured default session." },
                    "start_turn_id": { "type": "string", "format": "uuid", "description": "Entity ID of the first turn to include" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 20, "description": "Maximum number of turns to return" }
                },
                "required": ["start_turn_id"]
            }),
        },
        ToolDef {
            name: "get_chunk_context".into(),
            description: "Expand a retrieved document chunk through semantic prev/next links. Use after search returns a document_chunk hit whose answer may sit in adjacent chunks or split list items.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "chunk_id": { "type": "string", "format": "uuid" },
                    "prev": { "type": "integer", "minimum": 0, "maximum": 10 },
                    "next": { "type": "integer", "minimum": 0, "maximum": 10 },
                    "max_tokens": { "type": "integer", "minimum": 1, "maximum": 50000 }
                },
                "required": ["chunk_id"]
            }),
        },
        ToolDef {
            name: "check_memo_cache".into(),
            description: "Looks up a prior sub-call result by content hash. Returns cached result if found, or miss signal if not.\n\nCALL WHEN: Before every sub-LLM invocation within a long-horizon task. This is the first step in the usage loop.\nDO NOT CALL: For top-level queries or tasks where you are not making sub-calls. Do not call more than once per sub-call.\nON HIT: Use the cached result directly. Do not invoke the sub-LLM. Call record_outcome with program_type='memo_hit'.\nON MISS: Proceed with the sub-call. After it completes, call store_memo_result.\nCost: ~1ms. Zero token cost.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "maxLength": 4096, "description": "The prompt text" },
                    "context_slice": { "type": "string", "maxLength": 131072, "description": "Context slice for cache key" },
                    "model_version": { "type": "string", "maxLength": 64, "description": "Model version string" }
                },
                "required": ["prompt", "context_slice", "model_version"]
            }),
        },
        ToolDef {
            name: "store_memo_result".into(),
            description: "Stores a completed sub-call result for future reuse.\n\nCALL WHEN: Immediately after any sub-call completes on a task where the same chunk might be processed again.\nDO NOT CALL: For top-level responses or ephemeral computations. Do not call if check_memo_cache returned a hit.\nCost: ~5ms write.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "maxLength": 4096 },
                    "context_slice": { "type": "string", "maxLength": 131072 },
                    "model_version": { "type": "string", "maxLength": 64 },
                    "result": { "type": "string", "maxLength": 131072, "description": "The sub-call result to cache" },
                    "embedding": {
                        "type": "array", "items": { "type": "number" },
                        "description": "Optional embedding vector"
                    },
                    "ttl_days": { "type": "integer", "minimum": 1, "maximum": 365, "description": "TTL in days (default: 7)" }
                },
                "required": ["prompt", "context_slice", "model_version", "result"]
            }),
        },
        ToolDef {
            name: "write_plan_node".into(),
            description: "Records a sub-task node in the hierarchical plan tree. Enables structured re-injection of parent plan context on recursive return.\n\nCALL WHEN: At the start of each sub-task, before execution. Always call when decomposing a complex task into sub-tasks. Depth=0 is the root goal.\nDO NOT CALL: For single-step tasks with no decomposition.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "subtask_id": { "type": "string", "maxLength": 256 },
                    "parent_subtask": { "type": "string", "maxLength": 256 },
                    "goal_text": { "type": "string", "maxLength": 4096 }
                },
                "required": ["depth", "subtask_id", "goal_text"]
            }),
        },
        ToolDef {
            name: "get_plan_context".into(),
            description: "Returns the full plan tree for the current session as compact JSON. Use to re-inject parent context when returning from recursive sub-tasks.\n\nCALL WHEN: At the start of each sub-task execution and on return from a sub-task call.\nInclude the returned plan tree in your prompt preamble with 'Current task hierarchy:' to prevent goal drift.\nCost: ~2ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "max_depth": { "type": "integer", "minimum": 0, "maximum": 100 }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "update_plan_node".into(),
            description: "Marks a plan node complete or failed and records an outcome summary.\n\nCALL WHEN: When a sub-task finishes (success or failure). Always provide outcome_summary — this is what parent nodes will see.\nWrite outcome_summary describing what was found, not the process used.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "subtask_id": { "type": "string", "maxLength": 256 },
                    "status": { "type": "string", "enum": ["pending", "active", "complete", "failed"] },
                    "outcome_summary": { "type": "string", "maxLength": 4096 }
                },
                "required": ["depth", "subtask_id", "status"]
            }),
        },
        ToolDef {
            name: "session_task_put".into(),
            description: "Creates or upserts a durable fmem-owned session task. If task_id is omitted, fmem generates the canonical id. Use aliases only as scoped client-visible references.\n\nCALL WHEN: Starting work, updating visible work-item metadata, or switching focus to a new task. Prefer this over plan tools for current-task continuity across compaction.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid", "description": "Optional canonical id returned by fmem for updates; omit on create." },
                    "title": { "type": "string", "maxLength": 512 },
                    "description": { "type": "string", "maxLength": 8192 },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "blocked", "completed", "cancelled", "superseded"] },
                    "priority": { "type": "integer", "minimum": 0, "maximum": 1000 },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "parent_task_id": { "type": "string", "format": "uuid" },
                    "alias_scope": { "type": "string", "maxLength": 256 },
                    "alias": { "type": "string", "maxLength": 256 },
                    "focus": { "type": "boolean", "description": "Default true. Pushes current focus down the stack and focuses this task." },
                    "client_agent": { "type": "string" },
                    "workspace": { "type": "string" },
                    "thread_id": { "type": "string" },
                    "external_session_id": { "type": "string" }
                },
                "required": ["title"]
            }),
        },
        ToolDef {
            name: "session_task_get".into(),
            description: "Reads a durable session task by canonical task_id, or by scoped alias when task_id is omitted.\n\nCALL WHEN: Rehydrating task detail after compaction or resolving a client-visible work-item alias.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid" },
                    "alias_scope": { "type": "string" },
                    "alias": { "type": "string" }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "session_task_current".into(),
            description: "Returns the deterministic current-task snapshot: foreground task, active working set, focus stack, and recovery hints.\n\nCALL WHEN: Session starts, after compaction, before writing if the agent may be lost, or before deciding to plan more work.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "session_id": { "type": "string" } },
                "required": []
            }),
        },
        ToolDef {
            name: "session_task_list".into(),
            description: "Lists durable session tasks, optionally filtered by lifecycle status. Returns focus/priority sorted tasks.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "blocked", "completed", "cancelled", "superseded"] }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "session_task_complete".into(),
            description: "Marks a task completed without hard delete. If a suspended task is on the focus stack, returns a resume candidate and action according to policy.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid" },
                    "outcome_summary": { "type": "string", "maxLength": 4096 }
                },
                "required": ["task_id"]
            }),
        },
        ToolDef {
            name: "session_task_cancel".into(),
            description: "Marks a task cancelled without hard delete and updates focus stack recovery state.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid" },
                    "outcome_summary": { "type": "string", "maxLength": 4096 }
                },
                "required": ["task_id"]
            }),
        },
        ToolDef {
            name: "session_task_focus".into(),
            description: "Moves an existing non-terminal task to foreground and pushes the previous foreground down the focus stack.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid" },
                    "reason": { "type": "string", "maxLength": 512 }
                },
                "required": ["task_id"]
            }),
        },
        ToolDef {
            name: "session_task_observe".into(),
            description: "Deterministic v1 observation hook for clients/hook code. Handles explicit task-shift, completion, and lost-agent signals; returns actions and hints without requiring an LLM judge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "event_type": { "type": "string", "enum": ["user_requested_new_task", "user_requested_switch", "task_completed", "agent_lost", "context_reset"] },
                    "task_id": { "type": "string", "format": "uuid" },
                    "title": { "type": "string" },
                    "payload": { "type": "object" }
                },
                "required": ["event_type"]
            }),
        },
        // --- Fold tools (Sprint 2) ---
        ToolDef {
            name: "start_fold".into(),
            description: "Opens a new trajectory fold for a sub-task. Returns fold_id to append REPL turns as the sub-task executes.\n\nCALL WHEN: Starting any sub-task that involves multiple steps and whose results you want retrievable later. Always call write_plan_node first.\nA fold is the durable equivalent of a REPL scope.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "parent_fold_id": { "type": "string", "format": "uuid" },
                    "initial_context": { "type": "string", "maxLength": 131072 }
                },
                "required": ["depth", "initial_context"]
            }),
        },
        ToolDef {
            name: "append_to_fold".into(),
            description: "Appends a REPL turn to an active fold. Returns current token_count.\n\nCALL WHEN: After each step within an active fold.\nMONITOR token_count: If it exceeds ~80000, open a nested fold for the next phase.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fold_id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string" },
                    "repl_turn": { "type": "string", "maxLength": 131072 }
                },
                "required": ["fold_id", "repl_turn"]
            }),
        },
        ToolDef {
            name: "complete_fold".into(),
            description: "Seals a fold with summary and embedding. Creates FOLDED_INTO graph edge to parent. Queues trajectory for compression.\n\nCALL WHEN: When a sub-task is fully complete. Always call before returning from a recursive level.\nWrite summary as dense NL capsule: key findings, state changes, answers. Summarize outcomes, not process.\nCost: ~10ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fold_id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string" },
                    "summary": { "type": "string", "maxLength": 131072 },
                    "embedding": { "type": "array", "items": { "type": "number" } }
                },
                "required": ["fold_id", "summary", "embedding"]
            }),
        },
        ToolDef {
            name: "retrieve_fold_context".into(),
            description: "ANN vector search over prior fold summaries. Returns k most semantically similar fold summaries.\n\nCALL WHEN: Starting a new task where prior work might be relevant. Also call when stuck — prior folds often contain relevant evidence.\nRETRIEVAL LOOP: If results partially answer but leave gaps, call again with a more specific query targeting the gap. 2-3 rounds is normal.\nCost: ~10ms (HNSW). include_raw adds ~200-2000ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "query_embedding": { "type": "array", "items": { "type": "number" } },
                    "query": { "type": "string", "maxLength": 4096, "description": "Optional text query for routing optimization. If provided, the router selects optimal k and include_raw." },
                    "k": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "include_raw": { "type": "boolean" }
                },
                "required": ["query_embedding"]
            }),
        },
        // --- Entity tools (Sprint 3) ---
        ToolDef {
            name: "upsert_entity".into(),
            description: "Writes a discovered named entity to the entity store. Deduplicates via phonetic matching.\n\nCALL WHEN: Any time you identify a named entity (person, place, org, event, concept) from content.\nCheck is_new in response: if false, entity already exists — use the returned entity_id to attach new facts.\n\nNote: source_fold_id is optional — omit if not in a fold context.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entity_name": { "type": "string", "maxLength": 512 },
                    "entity_type": { "type": "string", "enum": entity_type_enum },
                    "context_snippet": { "type": "string", "maxLength": 4096 },
                    "embedding": { "type": "array", "items": { "type": "number" } },
                    "source_fold_id": { "type": "string", "format": "uuid", "description": "Optional: fold UUID from start_fold. Omit if not in a fold context." },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                },
                "required": ["entity_name", "entity_type", "context_snippet"]
            }),
        },
        ToolDef {
            name: "batch_ingest".into(),
            description: "Batch ingest multiple entities in a single call.\n\n\
                CALL WHEN:\n\
                - Ingesting 5+ entities at once (codebase indexing, document extraction, bulk import)\n\
                - Performance matters — single round-trip instead of N sequential calls\n\n\
                Each entity follows the same schema as upsert_entity. Returns array of results.\n\n\
                Cost: ~15ms + 5ms per entity (vs 15ms per entity with individual calls).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID" },
                    "entities": {
                        "type": "array",
                        "description": "Array of entities to ingest",
                        "items": {
                            "type": "object",
                            "properties": {
                                "entity_name": { "type": "string", "maxLength": 512 },
                                "entity_type": { "type": "string", "enum": entity_type_enum },
                                "context_snippet": { "type": "string", "maxLength": 4096 },
                                "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                            },
                            "required": ["entity_name", "entity_type", "context_snippet"]
                        },
                        "maxItems": 100
                    }
                },
                "required": ["entities"]
            }),
        },
        ToolDef {
            name: "batch_update_entities".into(),
            description: "Batch update entities by entity_id with explicit patch fields.\n\nReturns per-row success/failure and supports partial update.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entities": {
                        "type": "array",
                        "description": "Array of entity patches keyed by entity_id",
                        "items": {
                            "type": "object",
                            "properties": {
                                "entity_id": { "type": "string" },
                                "entity_name": { "type": "string", "maxLength": 512 },
                                "entity_type": { "type": "string" },
                                "context_snippet": { "type": "string", "maxLength": 4096 },
                                "source_fold_id": { "type": "string" },
                                "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                                "state": { "type": "string", "enum": ["active", "dormant", "silent", "unavailable"] },
                                "description": { "type": "string", "maxLength": 4096 },
                                "tags": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "properties": { "type": "object" },
                                "embedding": {
                                    "type": "array",
                                    "items": { "type": "number" },
                                    "description": "Replacement embedding vector"
                                }
                            },
                            "required": ["entity_id"]
                        },
                        "maxItems": 100
                    }
                },
                "required": ["entities"]
            }),
        },
        ToolDef {
            name: "batch_delete_entities".into(),
            description: "Batch delete entities by id with per-row success/failure reporting. Existing rows are hard-deleted from ferrosa-memory owned storage.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entities": {
                        "type": "array",
                        "description": "Entity IDs to delete",
                        "items": {
                            "type": "object",
                            "properties": {
                                "entity_id": { "type": "string", "description": "Target entity UUID" }
                            },
                            "required": ["entity_id"]
                        },
                        "maxItems": 100
                    }
                },
                "required": ["entities"]
            }),
        },
        ToolDef {
            name: "ingest_entities".into(),
            description: "Bulk-ingest entities and typed edges in a single call. The server owns schema mapping, conflict semantics, optional embedding generation, and structured per-row failures.\n\nCALL WHEN: You already have a batch of stable entity IDs and typed edges and want one fail-loud ingest call instead of direct CQL writes or multiple tool calls.\nRETURNS: counts plus structured failed[] arrays for entities, edges, and embeddings. dry_run validates without writing.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string" },
                    "entities": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "format": "uuid" },
                                "name": { "type": "string", "maxLength": 512 },
                                "entity_type": { "type": "string", "enum": entity_type_enum },
                                "context": { "type": "string", "maxLength": 16384 },
                                "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                                "state": { "type": "string" },
                                "embedding": { "type": "array", "items": { "type": "number" } },
                                "attrs": { "type": "object" }
                            },
                            "required": ["id", "name", "entity_type", "context"]
                        }
                    },
                    "edges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "src_id": { "type": "string", "format": "uuid" },
                                "dst_id": { "type": "string", "format": "uuid" },
                                "edge_type": { "type": "string" },
                                "weight": { "type": "number" },
                                "metadata": { "type": "object" }
                            },
                            "required": ["src_id", "dst_id", "edge_type"]
                        }
                    },
                    "options": {
                        "type": "object",
                        "properties": {
                            "embed_missing": { "type": "boolean" },
                            "embedding_model": { "type": "string" },
                            "on_conflict": { "type": "string", "enum": ["update", "skip", "error"] },
                            "strict_edges": { "type": "boolean" },
                            "dry_run": { "type": "boolean" }
                        }
                    }
                },
                "required": ["tenant_id", "entities"]
            }),
        },
        ToolDef {
            name: "retrieve_entities".into(),
            description: "Retrieves named entities by name (phonetic fuzzy match), semantic similarity (ANN), or both.\n\nCALL WHEN: Need to find entities related to current query. Use strategy='phonetic' for known names with possible variants. Use strategy='ann' for semantic search. Use strategy='both' for maximum recall.\nCost: phonetic ~5ms, ann ~10ms, both ~15ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "query": { "type": "string", "maxLength": 4096 },
                    "embedding": { "type": "array", "items": { "type": "number" } },
                    "strategy": { "type": "string", "enum": ["ann", "phonetic", "both"] },
                    "k": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "list_entities".into(),
            description: "List entities with structured equality predicates over entity fields and properties. Use this for kanban/task-style queries such as all task entities with status=ready and assignee=claude; use hybrid_search for semantic recall.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entity_type": { "type": "string", "description": "Optional entity_type filter, e.g. task" },
                    "filters": {
                        "type": "object",
                        "description": "Equality predicates. Known entity fields include entity_id/id, session_id, entity_name/name, entity_type, state, scope, tags, content_hash, confidence. Other keys match properties.<key>."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["session", "global", "both", "all"],
                        "description": "session=current session; global=tenant global plus legacy nil session; both=session+global; all=tenant-wide scan. Default all."
                    },
                    "include_cross_session": {
                        "type": "boolean",
                        "description": "Compatibility flag. true is equivalent to scope=all; false is equivalent to scope=session when scope is omitted."
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Max results to return (default 50)" }
                },
                "required": []
            }),
        },
        // --- Feedback tool (Sprint 3) ---
        ToolDef {
            name: "record_outcome".into(),
            description: "Records the result of a retrieval operation for offline routing improvement.\n\nCALL WHEN: After every retrieval operation (retrieve_fold_context, retrieve_entities, check_memo_cache). Provide program_type, task_complexity, succeeded, latency_ms, token_cost.\nThis is write-only (~1ms). No effect on current task but improves routing for future sessions.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "query_id": { "type": "string", "format": "uuid" },
                    "program_type": { "type": "string", "enum": ["hnsw_ann", "phonetic", "cypher_hop", "btree_range", "memo_hit", "hybrid_search", "hybrid_search_auto", "workspace", "retrieval_miss"] },
                    "task_complexity": { "type": "string", "enum": ["simple", "linear", "quadratic"] },
                    "succeeded": { "type": "boolean" },
                    "latency_ms": { "type": "integer", "minimum": 0 },
                    "token_cost": { "type": "integer", "minimum": 0 },
                    "entity_ids": { "type": "array", "items": { "type": "string", "format": "uuid" }, "description": "Entity IDs this outcome applies to. Success → warmth/workspace boost. Failure → warmth/workspace penalty." },
                    "cwd": { "type": "string", "maxLength": 1024, "description": "Working directory where the retrieval was evaluated. Enables workspace-specific reranking feedback." },
                    "retrieval_sources": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional retrieval mechanisms/sources involved, e.g. entity_phonetic, entity_ann, workspace."
                    }
                },
                "required": ["query_id", "program_type", "task_complexity", "succeeded", "latency_ms", "token_cost"]
            }),
        },
        ToolDef {
            name: "record_feedback".into(),
            description: "Records feedback on the most recent hybrid_search result set for this session.\n\nCALL WHEN: Retrieved memories were clearly helpful, irrelevant, wrong, or impossible to judge for the current working directory. Cheapest form: pass scores in last-result order, where 1=helpful, -1=irrelevant/wrong, 0=neutral, and \"-\"=judge abstained/failed. Include cwd so future searches in the same directory are reranked dynamically.\nCost: ~5ms + small entity property updates.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "relevant": { "type": "boolean", "description": "Fallback when scores is omitted: apply one relevance label to all last results." },
                    "scores": {
                        "type": "array",
                        "items": {},
                        "description": "Per-result feedback in last retrieval order. 1=helpful, -1=irrelevant/wrong, 0=neutral, \"-\" or null=judge abstained/failed."
                    },
                    "judge": { "type": "string", "maxLength": 64, "description": "Who made this judgment, e.g. caller_llm, human, judge_model. Scores from multiple judges are summed; abstentions are tracked separately." },
                    "cwd": { "type": "string", "maxLength": 1024 },
                    "reason": { "type": "string", "maxLength": 1024 },
                    "entity_ids": {
                        "type": "array",
                        "items": { "type": "string", "format": "uuid" },
                        "description": "Optional subset of last results to score. Omit to score all entity results from the last retrieval."
                    }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "configure".into(),
            description: "Read or update compact runtime defaults. Session-start hooks call this to let fmem create and store the active session_id; retrieval_limit controls default search result count.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_start": {
                        "oneOf": [
                            { "type": "boolean" },
                            {
                                "type": "object",
                                "properties": {
                                    "agent": { "type": "string", "maxLength": 128 },
                                    "agent_session_id": { "type": "string", "maxLength": 512 },
                                    "external_session_id": { "type": "string", "maxLength": 512 },
                                    "thread_id": { "type": "string", "maxLength": 512 },
                                    "workspace": { "type": "string", "maxLength": 2048 },
                                    "cwd": { "type": "string", "maxLength": 2048 }
                                }
                            }
                        ],
                        "description": "Set by a deterministic agent SessionStart hook. fmem creates/stores the active session_id from this metadata."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional explicit fmem UUID to install as the active runtime session. Hooks normally omit this."
                    },
                    "agent": { "type": "string", "maxLength": 128 },
                    "agent_session_id": { "type": "string", "maxLength": 512 },
                    "external_session_id": { "type": "string", "maxLength": 512 },
                    "thread_id": { "type": "string", "maxLength": 512 },
                    "workspace": { "type": "string", "maxLength": 2048 },
                    "cwd": { "type": "string", "maxLength": 2048 },
                    "retrieval_limit": {
                        "type": "integer",
                        "minimum": MIN_RETRIEVAL_LIMIT,
                        "maximum": MAX_RETRIEVAL_LIMIT,
                        "description": "Default ranked results returned by retrieval tools when k/limit is omitted."
                    },
                    "default_limit": {
                        "type": "integer",
                        "minimum": MIN_RETRIEVAL_LIMIT,
                        "maximum": MAX_RETRIEVAL_LIMIT,
                        "description": "Alias for retrieval_limit."
                    }
                },
                "required": []
            }),
        },
        // --- Session lifecycle ---
        ToolDef {
            name: "delete_session".into(),
            description: "Deletes all memory objects for a session across all tables (right-to-deletion).\n\nCALL WHEN: User explicitly requests data deletion, or session cleanup is needed.\nDO NOT CALL: During normal operation. This is destructive and irreversible.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id"]
            }),
        },
        // --- Cognitive memory tools ---
        ToolDef {
            name: "smart_ingest".into(),
            description: "YOUR PRIMARY TOOL FOR BUILDING LONG-TERM MEMORY. Automatically decides whether to CREATE, UPDATE, SUPERSEDE, or SKIP based on what you already know.\n\nCALL AGGRESSIVELY — every time you encounter something worth remembering:\n- User preferences, habits, or working style\n- Technical decisions and WHY they were made\n- Architecture patterns, library choices, configuration gotchas\n- People, roles, relationships mentioned in conversation\n- Project context: goals, constraints, deadlines, blockers\n- Debugging insights: what caused a bug, what fixed it\n- Tool/framework knowledge: 'X works well for Y', 'avoid Z because...'\n- Domain knowledge: business rules, API behaviors, data models\n- Corrections: 'user said X is wrong, Y is correct'\n\nDO NOT CALL for: ephemeral task state (use plan tools), raw code (derivable from files), or content the user explicitly marks as temporary.\n\nThe prediction error gate handles dedup — calling too often is better than missing important information. If in doubt, ingest it.\n\nRETURNS: action taken (Created/Updated/Superseded/Skipped) + entity_id.\nCost: ~15ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "content": { "type": "string", "maxLength": 8192, "description": "The content to ingest" },
                    "entity_type": { "type": "string", "enum": entity_type_enum },
                    "entity_name": { "type": "string", "maxLength": 256, "description": "Clean entity name (e.g. 'Ben Kearns', 'Ferrosa'). If omitted, extracted automatically from content via LLM or heuristic." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding vector" },
                    "source_fold_id": { "type": "string", "format": "uuid", "description": "Optional: UUID of the fold (conversation thread) that produced this content. Omit or pass null if not in a fold context. DO NOT pass a file path — this field expects a fold UUID from start_fold, or null." }
                },
                "required": ["content", "entity_type"]
            }),
        },
        // --- Skills layer ---
        ToolDef {
            name: "ingest_skill".into(),
            description: "Ingest a methodology into the global skill catalog. Skills are shared across all sessions and tenants' queries.\n\nCALL WHEN: You encounter or refine a reusable methodology — TDD, STRIDE threat modeling, debugging process, refactoring pattern, etc.\n\nThe server generates the version (YYYYMMDDNN) — do not pass it. Pass content_hash for idempotent re-ingest; re-running with an unchanged hash is a no-op.\n\nSkills are stored with entity_type='skill', scope='global'. Category and additional tags become tag entities + TAGGED_AS edges. Prerequisites become REQUIRES edges. If a prerequisite skill doesn't exist yet, its name is recorded in `missing_prerequisites` on the response — the skill itself still lands. Either ingest the missing prereqs and re-run this skill, or accept the partial graph.\n\nTAG NORMALIZATION: category and tags are normalized to lowercase, alphanumeric + dash only. Any other character (including underscore, space, slash) becomes `-`; consecutive dashes collapse and leading/trailing dashes are stripped. Example: 'Chaos Engineering' → 'chaos-engineering', 'unit_testing' → 'unit-testing', 'foo/bar/baz' → 'foo-bar-baz'. Use the same normalized form when calling retrieve_skills_for_context or ensure_parent_tag.\n\nLEARN AND REFINE: If you use a skill and discover a better step, a missing prerequisite, or a clearer description, call ingest_skill again to refine it. Your changes persist across all sessions.\nCost: ~20ms + one embed call for description.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller's session UUID (optional, recorded for audit). Omit or pass 'default' to use the configured default session." },
                    "name": { "type": "string", "maxLength": 256, "description": "Unique skill identifier (e.g., 'tdd', 'threat-model')" },
                    "category": { "type": "string", "maxLength": 128, "description": "Primary tag (e.g., 'testing', 'security'). Becomes a tag entity + TAGGED_AS edge." },
                    "description": { "type": "string", "maxLength": 4096, "description": "2-4 sentence description of what the skill does and when to use it. Embedded for retrieval." },
                    "trigger_keywords": { "type": "array", "items": { "type": "string" }, "description": "Keywords that indicate this skill is relevant." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Additional tags beyond category." },
                    "prerequisites": { "type": "array", "items": { "type": "string" }, "description": "Names of other skills this requires." },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "phase": { "type": "string" },
                                "instruction": { "type": "string" }
                            },
                            "required": ["instruction"]
                        },
                        "description": "Ordered steps to follow when invoking the skill."
                    },
                    "output_artifacts": { "type": "array", "items": { "type": "string" }, "description": "Artifacts the skill produces (e.g., 'checklist', 'diagram')." },
                    "completion_criteria": { "type": "string", "maxLength": 1024, "description": "How to tell when the skill's work is done." },
                    "content_hash": { "type": "string", "maxLength": 128, "description": "Caller-computed content hash for idempotent re-ingest. Passing the same hash as the stored skill is a no-op." }
                },
                "required": ["name", "category", "description"]
            }),
        },
        ToolDef {
            name: "retrieve_skills_for_context".into(),
            description: "Find methodologies relevant to your current task from the global skill catalog.\n\nCALL AT TASK START or whenever you encounter a problem you've solved before — 'how do I test this?', 'how should I refactor this?', 'what's the threat model here?'\n\nReturns ranked skills with description, category, version, and a used_in_session flag. Match scoring combines description-embedding similarity, trigger_keyword overlap, tag overlap, and name hits.\n\nThese skills are GLOBAL — shared across every session. If a result is marked used_in_session=true, you've already touched it this session, which is a strong relevance signal.\nCost: O(catalog size) — typically <20ms for 100s of skills.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller's session UUID (optional, used for the used_in_session flag)." },
                    "context": { "type": "string", "maxLength": 8192, "description": "Current task context — what you're working on, the problem statement, or a natural-language question." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional context embedding. When present, enables semantic matching against skill description_embeddings." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Max results (default 5)." },
                    "min_score": { "type": "number", "minimum": 0.0, "maximum": 2.0, "description": "Minimum score threshold (default 0.0 returns all)." }
                },
                "required": ["context"]
            }),
        },
        ToolDef {
            name: "invoke_skill".into(),
            description: "Fetch the structured steps for a named skill. Returns {description, steps, first_step_prompt, completion_criteria, output_artifacts}.\n\nCALL WHEN: You've decided to apply a skill by name (e.g., after retrieve_skills_for_context returned it, or the user explicitly asked 'use TDD').\n\nThe response is pure data. Execute the steps yourself — invoke_skill does not orchestrate tool calls. Start with first_step_prompt. Check completion_criteria when you finish.\n\nMissed skill returns INVALID_PARAMS with a did_you_mean list of similar skill names (phonetic match). Ingest the skill with ingest_skill if it genuinely doesn't exist yet.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller's session UUID (optional; used for prerequisite-satisfaction tracking)." },
                    "skill_name": { "type": "string", "maxLength": 256, "description": "Exact name of the skill to invoke (case-sensitive)." },
                    "current_context": { "type": "string", "maxLength": 4096, "description": "Optional context hint — what you're working on right now." }
                },
                "required": ["skill_name"]
            }),
        },
        ToolDef {
            name: "ensure_parent_tag".into(),
            description: "Idempotently create a PARENT_TAG edge between two tags in the global taxonomy, resolving tags by name (creating them if missing).\n\nCALL WHEN: Building or extending the tag hierarchy — e.g. declaring that 'tdd' is a sub-category of 'testing', or that 'testing' is a sub-category of 'quality'. forge's fmem-skill-ingest uses this when ingesting `tag-hierarchy.yaml`.\n\nTAG NORMALIZATION: names are normalized to lowercase, alphanumeric + dash only. Any other character (underscore, space, slash, etc.) becomes `-`; consecutive dashes collapse, leading/trailing dashes strip. 'Chaos Engineering' → 'chaos-engineering', 'unit_testing' → 'unit-testing'. Pre-normalize on the caller side if you want full control; otherwise the server's normalization is deterministic.\n\nReturns action=Created on first call, action=Skipped on subsequent identical calls. Cycles are rejected via the graph client's DAG check.\nCost: ~5ms for idempotent re-runs, ~20ms when creating both tags + edge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller session UUID (optional; used for ingested_by_session audit)." },
                    "child_tag": { "type": "string", "maxLength": 256, "description": "Narrower tag name (e.g. 'tdd')." },
                    "parent_tag": { "type": "string", "maxLength": 256, "description": "Broader tag name (e.g. 'testing')." }
                },
                "required": ["child_tag", "parent_tag"]
            }),
        },
        ToolDef {
            name: "verify_skill".into(),
            description: "Verify a skill's graph neighborhood for ingest pipelines and audits. Returns resolved tags, prerequisites (outgoing REQUIRES), required_by (incoming REQUIRES), and missing_prerequisites (raw names declared at ingest that never landed as edges).\n\nCALL WHEN: A bulk ingest finishes and the caller wants to confirm every skill's edges are intact. Safe to call for unknown skill names — returns {exists: false} cleanly, not an error.\n\nThis is an administrative read. For executing a skill, use invoke_skill.\nCost: ~10ms (one entity lookup + two edge scans).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller session UUID (optional)." },
                    "skill_name": { "type": "string", "maxLength": 256, "description": "Exact skill name (case-sensitive)." }
                },
                "required": ["skill_name"]
            }),
        },
        // --- Intention tools (prospective memory, repo-scoped) ---
        ToolDef {
            name: "set_intention".into(),
            description: "Prospective memory — 'remember to do X when Y happens.' Sets a deferred action that auto-triggers on context match.\n\nCALL WHEN you notice something to do later:\n- 'When we touch auth, check the error handling'\n- 'Next time we open database.rs, add that index'\n- 'When user mentions deployment, remind about the TLS cert'\n- 'In 30 minutes, check if the build finished'\n\nTrigger types: Topic (keyword match), FilePattern (file glob), Duration (minutes), Context (flexible condition).\n\nIntentions are repo-scoped and persist across sessions. They trigger automatically when check_intentions runs. Set liberally — they cost nothing until triggered.\nCost: ~1ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "maxLength": 4096, "description": "What to do when triggered" },
                    "repo": { "type": "string", "maxLength": 512, "description": "Repository path for scoping (defaults to server's configured repo)" },
                    "trigger": {
                        "type": "object",
                        "description": "Trigger condition",
                        "properties": {
                            "type": { "type": "string", "enum": ["Topic", "FilePattern", "Duration", "Context"] },
                            "keywords": { "type": "array", "items": { "type": "string" }, "description": "For Topic triggers" },
                            "pattern": { "type": "string", "description": "For FilePattern triggers" },
                            "minutes": { "type": "integer", "minimum": 1, "description": "For Duration triggers" },
                            "condition": { "type": "string", "description": "For Context triggers" }
                        },
                        "required": ["type"]
                    },
                    "priority": { "type": "string", "enum": ["low", "normal", "high", "critical"] }
                },
                "required": ["description", "trigger"]
            }),
        },
        ToolDef {
            name: "set_foresight".into(),
            description: "Time-bounded memory — declare a planned-future fact or temporary constraint with a validity window. Search surfaces it ONLY while valid at the current time; expired and not-yet-active facts are filtered out automatically, so stale deadlines never pollute context.\n\nCALL WHEN a fact only holds for a window:\n- 'Code freeze until 2026-07-01' (valid_until)\n- 'Migration plan goes live on 2026-06-30' (valid_from)\n- 'API v1 is deprecated as of today' (valid_until open-ended past the cutover)\n- 'Use the staging cluster this week' (valid_from + valid_until)\n\nvalid_from/valid_until are optional RFC3339 timestamps; omit either for an open-ended bound. Cost: ~1ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "maxLength": 4096, "description": "The time-bounded fact or constraint" },
                    "valid_from": { "type": "string", "description": "RFC3339 timestamp; the fact becomes active at this time (optional — omit for 'active now')" },
                    "valid_until": { "type": "string", "description": "RFC3339 timestamp; the fact expires at this time (optional — omit for 'no expiry')" },
                    "session_id": { "type": "string", "description": "Session UUID to scope the fact to (defaults to the current session)" }
                },
                "required": ["content"]
            }),
        },
        ToolDef {
            name: "check_intentions".into(),
            description: "Checks pending intentions against current context. Call FREQUENTLY — at every topic change, file open, or new task start. Pass a brief description of what you're doing now as context. Returns triggered intentions you should act on.\n\nIntentions are repo-scoped — only intentions for the current repo are checked.\nCost: ~1ms. Call often — it's free.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "context": { "type": "string", "maxLength": 8192, "description": "Current context to check against" },
                    "repo": { "type": "string", "maxLength": 512, "description": "Repository path (defaults to server's configured repo)" }
                },
                "required": ["context"]
            }),
        },
        ToolDef {
            name: "complete_intention".into(),
            description: "Marks a triggered intention as completed.\n\nCALL WHEN: After you have acted on a triggered intention.\nCost: ~1ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "intention_id": { "type": "string", "format": "uuid" }
                },
                "required": ["intention_id"]
            }),
        },
        ToolDef {
            name: "list_intentions".into(),
            description: "Lists intentions. By default lists current repo's intentions from the in-memory store.\n\nPass all_repos: true to list intentions across ALL repos from durable storage — useful for seeing all threads you're coordinating across projects.\n\nCALL WHEN: User asks about pending intentions, wants a cross-project overview, or for debugging intention state.\nCost: ~1ms (in-memory), ~15ms (all_repos).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "all_repos": { "type": "boolean", "description": "If true, list intentions across ALL repos from storage (not just current session)" }
                }
            }),
        },
        ToolDef {
            name: "snooze_intention".into(),
            description: "Snoozes a triggered intention — resets it to pending so it can trigger again later.\n\nCALL WHEN: An intention triggered but you want to defer action. Resets to pending state.\nCost: ~1ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "intention_id": { "type": "string", "format": "uuid" }
                },
                "required": ["intention_id"]
            }),
        },
        // --- Temporal fact tools ---
        ToolDef {
            name: "write_temporal_fact".into(),
            description: "Records a timestamped fact about an entity. Auto-supersedes the previous fact, preserving history.\n\nCALL WHEN facts change over time — this is how you track evolution:\n- Role changes: 'Alice is now VP' supersedes 'Alice is Director'\n- Status updates: 'deploy succeeded' supersedes 'deploy in progress'\n- Project state: 'using Rust 1.82' supersedes 'using Rust 1.78'\n- Preference changes: 'user prefers dark mode' supersedes 'user likes light mode'\n- Bug status: 'fixed in commit abc' supersedes 'investigating OOM'\n\nFirst call ingest to create the entity, then write_temporal_fact for facts that evolve. The supersession chain is queryable — you can answer 'what was X before?'\n\nReturns: event_id of the new fact.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entity_id": { "type": "string", "format": "uuid" },
                    "fact_text": { "type": "string", "maxLength": 4096, "description": "The fact to record" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1, "description": "Confidence score (default: 1.0)" }
                },
                "required": ["entity_id", "fact_text"]
            }),
        },
        ToolDef {
            name: "get_temporal_chain".into(),
            description: "Returns the current (most recent valid) fact for an entity.\n\nCALL WHEN: You need to check the latest known fact about an entity before writing a new one, or to answer a question about current state.\nReturns: The current fact object, or {\"fact\": null} if no facts exist.\nCost: ~2ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entity_id": { "type": "string", "format": "uuid" }
                },
                "required": ["entity_id"]
            }),
        },
        // --- Graph traversal tool ---
        ToolDef {
            name: "explore_connections".into(),
            description: "Traverses the knowledge graph. Supports 4 traversal types:\n- fold_ancestors: walk the fold hierarchy upward from a fold\n- related_entities: find entities connected within N hops\n- entities_in_fold: list all entities mentioned in a fold\n- supersession_chain: follow temporal supersession links from a fact\n\nCALL WHEN: You need to understand relationships between entities or folds, or trace how facts evolved over time.\nRequires a graph connection to be configured.\nCost: ~10-50ms depending on traversal depth.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "traversal": {
                        "type": "string",
                        "enum": ["fold_ancestors", "related_entities", "entities_in_fold", "supersession_chain"],
                        "description": "The type of graph traversal to perform"
                    },
                    "entity_id": { "type": "string", "format": "uuid", "description": "Entity or event ID (required for related_entities, supersession_chain)" },
                    "fold_id": { "type": "string", "format": "uuid", "description": "Fold ID (required for fold_ancestors, entities_in_fold)" },
                    "session_id": { "type": "string", "description": "Session ID (required for fold_ancestors, related_entities, entities_in_fold)" },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Maximum traversal depth (default: 2)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum results to return (default: 10)" }
                },
                "required": ["traversal"]
            }),
        },
        // --- Hybrid search ---
        ToolDef {
            name: "hybrid_search".into(),
            description: "Search across ALL memory types at once — entities, folds, and facts — using Reciprocal Rank Fusion to merge results.\n\nCALL AT THE START OF EVERY NEW TASK or when the user asks about something that might have prior context. This is your 'what do I already know about this?' tool.\n\nExamples of when to search:\n- User mentions a project, person, or concept → search for prior context\n- Starting implementation → search for related decisions and patterns\n- Debugging → search for prior bugs in the same area\n- User asks 'remember when...' → search for the memory\n\nProvide embedding for ANN strategies; without it only phonetic matching runs.\nCost: ~15ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "query": { "type": "string", "maxLength": 4096, "description": "Search query text (used for phonetic matching)" },
                    "embedding": {
                        "type": "array",
                        "items": { "type": "number" },
                        "description": "Optional embedding vector for ANN search strategies"
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Max results to return (default: 10)" },
                    "offset": { "type": "integer", "minimum": 0, "maximum": 49, "description": "Skip this many fused results for pagination. Use offset=5 after scoring the first 5 as irrelevant." },
                    "candidate_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Per-source candidate fanout before fusion. Defaults to min(limit*2, 50); lower it to reduce retrieval work."
                    },
                    "min_score": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Drop fused results below this score before returning them. Useful for hooks where silence is better than weak recall."
                    },
                    "memory_kinds": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["episodic", "procedural", "semantic"] },
                        "description": "Optional result category filter applied before return."
                    },
                    "datalog_frontier": {
                        "type": "boolean",
                        "description": "Enable bounded Datalog-style graph frontier expansion from entity seeds. Default true when the fusion profile includes datalog_frontier."
                    },
                    "datalog_frontier_seed_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Maximum entity seeds to expand from initial candidates. Defaults to candidate source limit."
                    },
                    "datalog_frontier_edge_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Maximum typed edges considered per frontier node. Defaults to 12."
                    },
                    "datalog_frontier_max_hops": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 3,
                        "description": "Maximum graph hops for inferred recall. Defaults to 2."
                    },
                    "datalog_frontier_min_confidence": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Suppress derived frontier candidates below this edge/derived confidence. Defaults to 0.30."
                    },
                    "fusion_profile": {
                        "type": "string",
                        "enum": ["auto", "default", "all", "bm25-only", "semantic-only", "bm25-semantic", "bm25-semantic-workspace", "bm25-semantic-phonetic", "bm25-semantic-phonetic-workspace"],
                        "description": "Named source-weight profile. Defaults to auto, which cheaply routes query intent to a fast effective profile. Use explicit profiles for deterministic ablations; use all/phonetic profiles for recall-heavy runs."
                    },
                    "fusion_weights": {
                        "type": "object",
                        "description": "Optional numeric source weight overrides, e.g. {\"document_bm25\":2.5,\"document_ann\":1.5,\"document_phonetic\":0}."
                    },
                    "query_decomposition": {
                        "type": "string",
                        "enum": ["none", "heuristic", "llm"],
                        "description": "Generate bounded query variants and RRF-union their candidate sets before reranking. llm uses the configured judge model. Default none."
                    },
                    "query_task": {
                        "type": "string",
                        "enum": ["general", "bright_pro", "memorybench"],
                        "description": "Task hint for query decomposition prompt shaping. Default general."
                    },
                    "query_variants": {
                        "type": "array",
                        "maxItems": 8,
                        "items": {"type": "string", "maxLength": 2048},
                        "description": "Caller-provided extra query variants to union with the primary query."
                    },
                    "query_variant_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 8,
                        "description": "Maximum total query variants, including the original query. Default 5."
                    },
                    "query_embed_variants": {
                        "type": "boolean",
                        "description": "When true and an embedding provider is configured, embed each query variant separately. Default false."
                    },
                    "chunk_expansion": {
                        "type": "string",
                        "enum": ["none", "neighbors"],
                        "description": "Expand document_chunk hits before reranking/returning. neighbors adds bounded prev/next chunks."
                    },
                    "chunk_prev": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 5,
                        "description": "Previous document chunks to include when chunk_expansion=neighbors."
                    },
                    "chunk_next": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 5,
                        "description": "Next document chunks to include when chunk_expansion=neighbors."
                    },
                    "chunk_max_tokens": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 8000,
                        "description": "Approximate added-token budget per result for chunk expansion."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["session", "global", "both"],
                        "description": "session=current session only; global=tenant global plus legacy nil session; both=session+global. Default both, so curated global/skill corpus is retrievable; pass session to restrict to the current session."
                    },
                    "include_cross_session": {
                        "type": "boolean",
                        "description": "Compatibility flag, overridden by an explicit scope. When scope is omitted: the default already spans session+global; pass false to restrict to the current session, true to force both."
                    },
                    "cwd": {
                        "type": "string",
                        "maxLength": 1024,
                        "description": "Current agent working directory. Results learned in the same directory tree receive a bounded reranking boost."
                    },
                    "workspace_cwd": {
                        "type": "string",
                        "maxLength": 1024,
                        "description": "Alias for cwd; explicit workspace path used for reranking affinity."
                    },
                    "rerank": {
                        "type": "boolean",
                        "description": "Override live LLM reranking for this call. Defaults to [judge].enabled."
                    },
                    "rerank_candidates": {
                        "type": "integer",
                        "minimum": 2,
                        "maximum": 50,
                        "description": "Override how many top candidates the judge reranker sees. Keep small for token economy; evals may use 25."
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "manage_authority".into(),
            description: "Set user-managed authority for retrieved memories. Use this to mark curated corpus chunks, skills, or other memory IDs as high reputation/PageRank, or to demote known clutter. Authority is applied to future hybrid_search ranking after normal relevance scoring.\n\nCALL WHEN: The user explicitly says a result/source is curated, authoritative, trusted, or noisy. Prefer global scope for curated corpus/skills and session scope for local one-off preferences.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "target_id": { "type": "string", "description": "Memory result ID to update." },
                    "target_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Multiple memory result IDs to update with the same authority values."
                    },
                    "reputation": {
                        "type": "number",
                        "minimum": -1.0,
                        "maximum": 1.0,
                        "description": "User-managed trust score. 1.0=curated/highest trust, 0=neutral, -1.0=known clutter."
                    },
                    "pagerank": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Authority/PageRank seed. 1.0 strongly boosts authoritative curated material."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["session", "global"],
                        "description": "Where to store this authority. global applies to tenant-global searches; session applies to the current session. Default session unless global=true."
                    },
                    "global": {
                        "type": "boolean",
                        "description": "Compatibility shortcut for scope=global."
                    },
                    "reason": { "type": "string", "maxLength": 2048 }
                }
            }),
        },
        // --- Dream consolidation ---
        ToolDef {
            name: "run_consolidation".into(),
            description: "Dream consolidation — discovers hidden connections between memories. Groups entities by shared context, creates CO_OCCURS graph edges, identifies clusters.\n\nCALL WHEN:\n- At the end of a productive work session\n- When the user says 'wrap up' or 'that's it for now'\n- When you want to force background consolidation for the current session\n\nSmart ingest automatically queues consolidation after enough new entities; you do not need to count memories manually.\nCost: request path only queues work; the background worker does the consolidation.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": []
            }),
        },
        // --- Enrichment pipeline ---
        ToolDef {
            name: "enrich_entities".into(),
            description: "Post-ingest enrichment: generates LLM descriptions for code entities, \
                annotates edge relationships, and lints the knowledge graph.\n\n\
                CALL WHEN: After frg ingest populates the graph with structural entities. \
                Transforms shallow structural facts into searchable semantic knowledge.\n\n\
                Operations: enrich (LLM descriptions), annotate (edge explanations), lint (graph analysis).\n\
                Idempotent — safe to run multiple times. Already-enriched entities are skipped.\n\
                Cost: ~2-5 min for 1000 entities (local LLM). Lint is instant.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "operations": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["enrich", "annotate", "lint"] },
                        "description": "Which operations to run. Default: all three."
                    },
                    "entity_types": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Filter: only enrich entities of these types"
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Re-enrich already-enriched entities. Default: false."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Lint only, don't write changes. Default: false."
                    }
                },
                "required": []
            }),
        },
        // --- Stats tool ---
        ToolDef {
            name: "get_stats".into(),
            description: "Returns memory system statistics for the session: entity count, fold count, memo count, intention count, and latest consolidation status.\n\nCALL WHEN: For health monitoring, debugging, or when the user asks about memory usage.\nCost: ~5ms (runs count queries).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "memory_metrics".into(),
            description: "Returns a compact tenant-wide memory size report: total node/edge counts plus node and edge buckets, including legacy nil-session knowledge in the tenant totals.\n\nCALL WHEN: A user asks how much knowledge is stored, how many nodes/edges memory has, or whether database-backed memory has outgrown flat files.\nCost: ~10-100ms (tenant-scoped count queries).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDef {
            name: "migration_status".into(),
            description: "Returns read-only schema migration status for the connected memory database: db_version, binary_version, pending versions, and last applied timestamp.\n\nCALL WHEN: Startup logs or graph writes suggest schema drift, or an operator asks whether the database schema is current.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDef {
            name: "describe".into(),
            description: "Read-only, management-safe self-description of this ferrosa-memory server (contract ferrosa-memory.system.describe.v1): identity, runtime health, redacted effective config, dependent-store health, live ferrosa cluster info (queried from the CQL system tables), summary memory statistics, schema drift, binary/release state, capabilities, and allowed management actions.\n\nCALL WHEN: A management client (e.g. Ferrosa Workbench) discovers or is pointed at this endpoint and needs the authoritative cluster descriptor instead of inferring it from local files. Secrets are never returned; their key paths appear under configuration.redactedKeys.\nCost: ~10-3000ms (bounded dependency probes).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "include": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": [
                                "identity", "runtime", "configuration", "stores",
                                "schema", "statistics", "binaries", "harnesses",
                                "capabilities", "managementActions"
                            ]
                        },
                        "description": "Optional list of sections to return. Omit for all sections."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session to scope the statistics section to (defaults to the nil session)."
                    },
                    "redaction": {
                        "type": "string",
                        "enum": ["management-safe"],
                        "description": "Redaction mode. Only management-safe is supported; secrets are always redacted."
                    },
                    "caller": {
                        "type": "object",
                        "description": "Optional calling client identity, logged for diagnostics.",
                        "properties": {
                            "name": { "type": "string" },
                            "version": { "type": "string" }
                        }
                    }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "forget".into(),
            description: "Candidate-confirmed forgetting, in two phases. PROPOSE (pass `query`, no token): searches memory across sessions for candidates matching the intent, returns each candidate's blast radius (edges/temporal/derived it references) plus a signed `forget_token` — mutates nothing. CONFIRM (pass `forget_token` + `selected_ids` + `confirm: true`): forgets only the approved ids. Defaults to reversible RETRACT (excluded from recall, audited, restorable via restore_forgotten for `retract_purge_days`); pass `mode: \"hard\"` for permanent deletion. Never forgets without explicit confirmation; skips any candidate that changed since proposal.\n\nCALL WHEN: the user asks to forget/remove specific memories. Always propose first, show the candidates, and only confirm the ids the user approves — never pass confirm:true on the user's behalf.\nCost: propose ~20ms + search; confirm ~10-50ms per item.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Propose phase: natural-language description of what to forget." },
                    "scope": { "type": "array", "items": { "type": "string" }, "description": "Optional candidate filters (entity types, etc.)." },
                    "session_id": { "type": "string" },
                    "limit": { "type": "integer", "description": "Max candidates to propose." },
                    "forget_token": { "type": "string", "description": "Confirm phase: the token returned by a prior propose call." },
                    "selected_ids": { "type": "array", "items": { "type": "string", "format": "uuid" }, "description": "Confirm phase: the candidate ids the user approved." },
                    "mode": { "type": "string", "enum": ["retract", "hard"], "description": "retract (default, reversible) or hard (permanent)." },
                    "acknowledge_high_impact": { "type": "boolean", "description": "Required to forget a high-impact (highly-connected) candidate." },
                    "reason": { "type": "string" },
                    "confirm": { "type": "boolean", "description": "Must be true (with forget_token) to execute the forget." }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "restore_forgotten".into(),
            description: "Reverse a retraction: restore a soft-forgotten entity to its prior memory state so it is recalled again. Works only for retract-mode forgets that have not yet been purged (within retract_purge_days); hard deletes are irreversible. Note: edges removed at forget time are not auto-recreated in v1.\n\nCALL WHEN: the user wants to undo a forget / bring back a retracted memory.\nCost: ~10ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "entity_id": { "type": "string", "format": "uuid", "description": "The entity to restore." }
                },
                "required": ["entity_id"]
            }),
        },
        ToolDef {
            name: "count_entities_by_type".into(),
            description: "Return a per-session entity histogram broken down by entity_type, by state, and by the joint (type,state) buckets.\n\nCALL WHEN: You need status/diagnostic counts like 'how many bugs are active in this session?' without coupling the client to entity_store columns.\nCost: ~5-10ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": []
            }),
        },
        // --- Memory state management ---
        ToolDef {
            name: "promote_memory".into(),
            description: "Promotes an entity's memory state one level: dormant->active, silent->dormant, unavailable->silent. Active stays active.\n\nCALL WHEN: A dormant or silent memory becomes relevant again — e.g., an entity is referenced in new context after a period of inactivity.\nRETURNS: The new memory state after promotion.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entity_id": { "type": "string", "format": "uuid" }
                },
                "required": ["entity_id"]
            }),
        },
        ToolDef {
            name: "demote_memory".into(),
            description: "Demotes an entity's memory state one level: active->dormant, dormant->silent, silent->unavailable. Unavailable stays unavailable.\n\nCALL WHEN: A memory is no longer relevant to the current context, or during periodic decay sweeps. Demoted memories are still retrievable but with lower priority.\nRETURNS: The new memory state after demotion.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entity_id": { "type": "string", "format": "uuid" }
                },
                "required": ["entity_id"]
            }),
        },
        // --- Importance scoring ---
        ToolDef {
            name: "importance_score".into(),
            description: "Computes a 4-channel importance score for a memory entity: novelty (how surprising), arousal (emotional intensity), reward (past retrieval success), attention (recency/frequency).\n\nCALL WHEN: Prioritizing which memories to surface, deciding whether to consolidate or prune, or ranking retrieval results by relevance.\nRETURNS: Per-channel scores (0-1) and a weighted composite score.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entity_id": { "type": "string", "format": "uuid" }
                },
                "required": ["entity_id"]
            }),
        },
        // --- Memory chains ---
        ToolDef {
            name: "find_memory_chain".into(),
            description: "Discovers the shortest path between two entities through the knowledge graph using BFS traversal. Returns the chain of intermediate entities and edge types connecting source to destination.\n\nCALL WHEN: You need to understand HOW two concepts are related — not just whether they are, but the path of connections between them. Useful for explaining reasoning chains, tracing provenance, or finding indirect relationships.\nRETURNS: Ordered list of steps (entity_id + edge_type) forming the shortest path, plus hop count and confidence score.\nCost: ~5-20ms depending on graph density.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "source": { "type": "string", "format": "uuid", "description": "Entity ID to start from" },
                    "destination": { "type": "string", "format": "uuid", "description": "Entity ID to find path to" },
                    "max_hops": { "type": "integer", "minimum": 1, "maximum": 10, "description": "Maximum path length (default: 5)" }
                },
                "required": ["source", "destination"]
            }),
        },
        // --- Speculative retrieval ---
        ToolDef {
            name: "predict_needed".into(),
            description: "Predicts which entities will be needed based on co-access patterns. Analyzes which entities are frequently retrieved together and suggests entities likely to be needed given recent access history.\n\nCALL WHEN: After retrieving entities, to prefetch or surface related memories before they are explicitly requested.\nCost: ~1ms (in-memory co-access analysis).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "threshold": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Minimum confidence threshold (default: 0.3)"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "Maximum predictions to return (default: 10)"
                    }
                },
                "required": []
            }),
        },
        // --- Spreading activation ---
        ToolDef {
            name: "spread_activation".into(),
            description: "Spreading activation search (Collins & Loftus). Propagates activation energy from seed entities through the knowledge graph, decaying at each hop. Returns the most activated non-seed entities.\n\nCALL WHEN: You have one or more known entities and want to discover related entities through graph structure — especially when semantic search alone misses structural relationships.\nPair with retrieve_entities for seeds, then spread to find indirect connections.\nCost: ~10-50ms depending on graph density and max_hops.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "seeds": {
                        "type": "array",
                        "items": { "type": "string", "format": "uuid" },
                        "minItems": 1,
                        "description": "Entity IDs to start activation from"
                    },
                    "max_hops": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Maximum traversal depth (default: 2)" },
                    "decay": { "type": "number", "minimum": 0.01, "maximum": 1.0, "description": "Activation decay per hop (default: 0.7)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Max results to return (default: 10)" }
                },
                "required": ["seeds"]
            }),
        },
        // --- Duplicate detection ---
        ToolDef {
            name: "find_duplicates".into(),
            description: "Scans a session\'s entities for potential duplicates using text similarity (Jaccard coefficient) on context snippets. Returns pairs above the threshold, sorted by similarity descending.\n\nCALL WHEN: After bulk entity ingestion, or when you suspect duplicate entities exist in a session. Useful before consolidation to identify merge candidates.\nDO NOT CALL: On sessions with very few entities (< 3). Use retrieve_entities with phonetic matching for single-entity dedup.\nCost: O(n^2) comparisons -- fast for <1000 entities per session.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "threshold": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1,
                        "description": "Similarity threshold (0-1). Default: 0.7. Higher = fewer, more confident matches."
                    }
                },
                "required": []
            }),
        },
        // --- Recursive exploration ---
        ToolDef {
            name: "recursive_explore".into(),
            description: "Recursive multi-pass query exploration with Datalog-driven discovery.\n\n\
                CALL WHEN:\n\
                - Complex multi-hop queries that need connected knowledge clusters\n\
                - Queries involving relationships between entities\n\
                - When hybrid_search returns too few results\n\n\
                DO NOT CALL:\n\
                - For simple name lookups (use retrieve_entities)\n\
                - For direct entity retrieval by ID\n\n\
                Cost: Multiple passes × hybrid_search cost. Bounded by max_passes.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID" },
                    "query": { "type": "string", "description": "Search query to explore recursively" },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional query embedding vector" },
                    "max_passes": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Max exploration passes (default 3)" },
                    "convergence_threshold": { "type": "number", "minimum": 0.0, "maximum": 1.0, "description": "Novelty ratio for convergence (default 0.1)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Max results (default 20)" }
                },
                "required": ["query"]
            }),
        },
        // --- Datalog query ---
        ToolDef {
            name: "query_derived".into(),
            description: "Query Datalog-derived facts with provenance.\n\n\
                CALL WHEN:\n\
                - You need to explain why entity A relates to entity B\n\
                - You want transitive closure (related, reachable, isa)\n\
                - You need derived facts with explanation chains\n\n\
                DO NOT CALL:\n\
                - For raw entity retrieval (use retrieve_entities)\n\n\
                Cost: Cache hit is free. Cache miss computes Datalog evaluation.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID" },
                    "predicate": { "type": "string", "description": "Derived predicate to query (e.g., 'related', 'reachable', 'isa', 'cluster')" }
                },
                "required": ["predicate"]
            }),
        },
        // --- Datalog rule management ---
        ToolDef {
            name: "manage_rules".into(),
            description: "CRUD for Datalog rule registry.\n\n\
                CALL WHEN:\n\
                - Adding custom inference rules\n\
                - Listing active rules\n\
                - Deprecating old rules\n\n\
                Cost: Low (registry operations).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "put", "deprecate"], "description": "CRUD action" },
                    "rule_id": { "type": "string", "description": "Rule ID (for get/put/deprecate)" },
                    "family": { "type": "string", "description": "Rule family (for list/put)" },
                    "rule_body": { "type": "string", "description": "Datalog rule text (for put)" },
                    "name": { "type": "string", "description": "Human-readable name (for put)" },
                    "rule_weight": { "type": "number", "description": "Rule confidence weight (default 1.0)" }
                },
                "required": ["action"]
            }),
        },
        ToolDef {
            name: "manage_claims".into(),
            description: "Manage expert-system claims stored as entity-backed review artifacts.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "put"] },
                    "claim_id": { "type": "string" },
                    "claim_text": { "type": "string" },
                    "domain": { "type": "string" },
                    "status": { "type": "string", "enum": ["proposed", "approved", "rejected"] },
                    "confidence": { "type": "number" },
                    "source_ref": { "type": "string" },
                    "support_count": { "type": "integer" },
                    "workspace_scope": { "type": "string" },
                    "session_id": { "type": "string" },
                    "include_unapproved": { "type": "boolean" }
                },
                "required": ["action"]
            }),
        },
        ToolDef {
            name: "manage_approvals".into(),
            description: "Append and inspect approval decisions for rules, claims, aliases, and other governed artifacts.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "record", "latest"] },
                    "artifact_kind": { "type": "string", "enum": ["rule", "claim", "alias", "skill"] },
                    "artifact_ref": { "type": "string" },
                    "decision": { "type": "string", "enum": ["proposed", "approved", "rejected"] },
                    "review_note": { "type": "string" },
                    "scope": { "type": "string" },
                    "workspace_scope": { "type": "string" },
                    "session_scope": { "type": "string" },
                    "reviewer": { "type": "string", "description": "Ignored; reviewer is always auth-derived." }
                },
                "required": ["action", "artifact_kind", "artifact_ref"]
            }),
        },
        ToolDef {
            name: "manage_aliases".into(),
            description: "Manage exact-scope tool aliases for deterministic execution-time rewrites.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "put", "resolve"] },
                    "alias_name": { "type": "string" },
                    "scope_kind": { "type": "string", "enum": ["global", "workspace", "session"] },
                    "scope_ref": { "type": "string" },
                    "canonical_tool": { "type": "string" },
                    "parameter_map": { "type": "object" },
                    "fixed_arguments": { "type": "object" },
                    "args_templates": { "type": "object" },
                    "status": { "type": "string", "enum": ["proposed", "approved", "rejected"] },
                    "workspace_scope": { "type": "string" },
                    "session_scope": { "type": "string" }
                },
                "required": ["action", "alias_name"]
            }),
        },
        ToolDef {
            name: "explain_derived".into(),
            description: "Return a bounded explanation for derived facts, including support chain and approval metadata.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "predicate": { "type": "string" },
                    "session_id": { "type": "string" },
                    "src_id": { "type": "string" },
                    "dst_id": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 64 }
                },
                "required": ["predicate"]
            }),
        },
        ToolDef {
            name: "get_effective_rule_set".into(),
            description: "Inspect the merged runtime-effective rule set, including synthetic built-ins and approved registry rules.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "family": { "type": "string" }
                },
                "required": []
            }),
        },
        // --- Predicate promotion ---
        ToolDef {
            name: "promote_predicate".into(),
            description: "Promote a derived predicate to durable materialization.\n\n\
                CALL WHEN:\n\
                - A derived predicate is queried frequently and you want faster access\n\
                - You want to persist inference results beyond the ephemeral cache TTL\n\n\
                Cost: Runs Datalog evaluation + writes to durable tables.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID" },
                    "predicate": { "type": "string", "description": "Predicate to promote (e.g., 'related', 'isa', 'reachable')" }
                },
                "required": ["predicate"]
            }),
        },
        // --- Typed edge tools ---
        ToolDef {
            name: "create_edge".into(),
            description: "Create a typed, labeled edge between two entities.\n\n\
                CALL WHEN:\n\
                - Building a knowledge graph with semantic relationships\n\
                - Recording dependencies (depends_on), containment (contains), inheritance (subclass_of)\n\
                - Any time you discover a specific relationship between entities\n\n\
                Edge types: depends_on, contains, part_of, subclass_of, calls, implements, uses, related_to\n\n\
                Cost: ~5ms per edge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "src_entity_id": { "type": "string", "format": "uuid", "description": "Source entity UUID" },
                    "dst_entity_id": { "type": "string", "format": "uuid", "description": "Destination entity UUID" },
                    "edge_type": { "type": "string", "description": "Relationship type (depends_on, contains, part_of, subclass_of, calls, implements, uses)" },
                    "weight": { "type": "number", "minimum": 0, "maximum": 1, "description": "Edge strength (default 1.0)" },
                    "metadata": { "type": "string", "description": "Optional metadata about the relationship" }
                },
                "required": ["src_entity_id", "dst_entity_id", "edge_type"]
            }),
        },
        ToolDef {
            name: "batch_create_edges".into(),
            description: "Create multiple typed edges in a single call.\n\n\
                Cost: ~5ms + 2ms per edge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "edges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "src_entity_id": { "type": "string", "format": "uuid" },
                                "dst_entity_id": { "type": "string", "format": "uuid" },
                                "edge_type": { "type": "string" },
                                "weight": { "type": "number" }
                            },
                            "required": ["src_entity_id", "dst_entity_id", "edge_type"]
                        },
                        "maxItems": 200
                    }
                },
                "required": ["edges"]
            }),
        },
        ToolDef {
            name: "batch_update_edges".into(),
            description: "Update typed edges in bulk by (src_entity_id, dst_entity_id, edge_type).\n\n\
                Current storage semantics write through `typed_edge_put`; this is treated as upsert/update-compatible where supported."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "edges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "src_entity_id": { "type": "string", "format": "uuid" },
                                "dst_entity_id": { "type": "string", "format": "uuid" },
                                "edge_type": { "type": "string" },
                                "weight": { "type": "number" },
                                "metadata": { "type": "string" }
                            },
                            "required": ["src_entity_id", "dst_entity_id", "edge_type"]
                        },
                        "maxItems": 200
                    }
                },
                "required": ["edges"]
            }),
        },
        ToolDef {
            name: "batch_delete_edges".into(),
            description: "Delete typed edges in bulk.\n\n\
                Uses the current graph-backed delete path and returns structured per-row success/failure results."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "edges": {
                        "type": "array",
                        "description": "Typed edges to delete",
                        "items": {
                            "type": "object",
                            "properties": {
                                "src_entity_id": { "type": "string", "format": "uuid" },
                                "dst_entity_id": { "type": "string", "format": "uuid" },
                                "edge_type": { "type": "string" }
                            },
                            "required": ["src_entity_id", "dst_entity_id", "edge_type"]
                        },
                        "maxItems": 200
                    }
                },
                "required": ["edges"]
            }),
        },
        // --- Derived cache listing ---
        ToolDef {
            name: "list_derived_cache".into(),
            description: "List all derived cache entries for inspection/debugging.\n\n\
                Returns up to `limit` rows sorted by computed_at DESC.\n\n\
                Use for: audit trail, debugging derivation results, reviewing cache state.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Tenant UUID" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Max rows to return (default 100)" }
                },
                "required": ["tenant_id"]
            }),
        },
    ];
    for tool in &mut tools {
        if let Some(short) = short_tool_name(&tool.name) {
            tool.name = short.to_string();
        }
    }
    tools
}

/// Memory guide included in initialize instructions.
/// Teaches the LLM how to use the memory system to build knowledge.
const MEMORY_GUIDE: &str = r#"You have a semantic memory system. Use it BEFORE grep, find, or reading files. It should be your first source of context, not a fallback.

SESSION START: (1) check_intentions with current context, (2) hybrid_search for what you're working on, (3) tell user what you remember. Do this BEFORE reading files.

SEARCHING: hybrid_search first. If it returns what you need, you're done — no need to grep or read files. For document_chunk hits, call chunk_ctx when adjacent chunks or split list items could contain the rest of the answer. If the first page is irrelevant, send compact +1/-1 item feedback with feedback, then request the next page before falling back to grep/find/read.

STORING: Use ingest for new knowledge. It decides CREATE/UPDATE/SUPERSEDE/SKIP. Store insights, decisions, relationships, and facts — not raw file contents.

CONNECTING: After learning 2+ related facts, use create_edge to link them. Types: depends_on, contains, part_of, related_to, calls, implements, uses, references. Connected facts are knowledge; isolated facts are just data.

INTENTIONS: set_intention for deferred actions. check_intentions at session start. Triggers: Topic, FilePattern, Duration, Context.

CONSOLIDATION: The server automatically queues consolidation after enough new ingest-created entities. Use run_consolidation only to force a background consolidation request, such as at wrap-up.

FORGETTING: When the user asks to forget or remove specific memories, call forget with a query to PROPOSE candidates, show them what was found (with blast radius), and only call forget again with confirm=true and the user-approved selected_ids. Never pass confirm=true on the user's behalf. Default mode retract is reversible (restore_forgotten); mode hard is permanent.

FEEDBACK: If you had to use grep, find, or read files to get context that SHOULD have been in memory, call record_outcome with program_type="retrieval_miss" and include what you were looking for. This trains the system to store that kind of information in the future. Every retrieval miss is a signal to improve."#;

/// Server info returned during initialize.
fn server_info() -> Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "ferrosa-memory-mcp",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": MEMORY_GUIDE
    })
}

/// Dispatch an MCP method call. Returns `Ok(result)` or `Err((code, message))`.
///
/// This is the top-level entry point called by the transport layer for each
/// incoming JSON-RPC request.
pub async fn dispatch<S: crate::storage::Storage>(
    method: &str,
    params: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    match method {
        "initialize" => {
            // Extract repo from client roots (MCP spec: roots[].uri).
            if session.repo.get().is_none()
                && let Some(uri) = params
                    .get("roots")
                    .and_then(|v| v.as_array())
                    .and_then(|roots| roots.first())
                    .and_then(|r| r.get("uri"))
                    .and_then(|u| u.as_str())
            {
                let path = uri.strip_prefix("file://").unwrap_or(uri).to_string();
                let _ = session.repo.set(path.clone());
                tracing::info!(repo = %path, "repo set from MCP initialize roots");
            }
            Ok(server_info())
        }
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => {
            let include_all = params
                .get("include_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let all_tools = tool_definitions(&session.entity_types);
            let tools: Vec<ToolDef> = if include_all {
                all_tools
            } else {
                all_tools
                    .into_iter()
                    .filter(|t| is_tier1(&t.name))
                    .collect()
            };
            Ok(serde_json::json!({ "tools": tools }))
        }
        "tools/call" => dispatch_tool(params, storage, ctx, session).await,
        _ => Err((METHOD_NOT_FOUND, format!("unknown method: {method}"))),
    }
}

/// Dispatch a tools/call request to the appropriate handler.
async fn dispatch_tool<S: crate::storage::Storage>(
    params: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((INVALID_PARAMS, "missing tool name".into()))?;
    let canonical_name = canonical_tool_name(name);

    let mut args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    tracing::debug!(
        tool = canonical_name,
        requested_tool = name,
        "dispatching tool call"
    );
    if canonical_name != "configure" {
        resolve_session_id(&mut args, session.effective_default_session_id())?;
    }
    let input_bytes = serde_json::to_string(&args).map(|s| s.len()).unwrap_or(0) as i32;
    let start = std::time::Instant::now();
    tracing::info!(
        tool = canonical_name,
        requested_tool = name,
        input_bytes,
        "tool call started"
    );
    let handler: ToolDispatchFuture<'_> = match canonical_name {
        "check_memo_cache" => Box::pin(handle_check_memo(args, storage, ctx)),
        "all_tools" => Box::pin(async move {
            Ok(serde_json::json!({
                "tools": tool_definitions(&session.entity_types),
                "hint": "Use these short tool names directly. Keep using compact defaults unless you need a specific deeper operation."
            }))
        }),
        "store_memo_result" => Box::pin(handle_store_memo(args, storage, ctx)),
        "write_plan_node" => Box::pin(handle_write_plan(args, storage, ctx)),
        "get_plan_context" => Box::pin(handle_get_plan(args, storage, ctx)),
        "update_plan_node" => Box::pin(handle_update_plan(args, storage, ctx)),
        "session_task_put" => Box::pin(handle_session_task_put(args, storage, ctx)),
        "session_task_get" => Box::pin(handle_session_task_get(args, storage, ctx)),
        "session_task_current" => Box::pin(handle_session_task_current(args, storage, ctx)),
        "session_task_list" => Box::pin(handle_session_task_list(args, storage, ctx)),
        "session_task_complete" => Box::pin(handle_session_task_lifecycle(
            args,
            storage,
            ctx,
            crate::types::SessionTaskStatus::Completed,
        )),
        "session_task_cancel" => Box::pin(handle_session_task_lifecycle(
            args,
            storage,
            ctx,
            crate::types::SessionTaskStatus::Cancelled,
        )),
        "session_task_focus" => Box::pin(handle_session_task_focus(args, storage, ctx)),
        "session_task_observe" => Box::pin(handle_session_task_observe(args, storage, ctx)),
        "start_fold" => Box::pin(handle_start_fold(args, storage, ctx)),
        "append_to_fold" => Box::pin(handle_append_fold(args, storage, ctx)),
        "complete_fold" => Box::pin(handle_complete_fold(args, storage, ctx, session)),
        "retrieve_fold_context" => Box::pin(handle_retrieve_fold(args, storage, ctx, session)),
        "ingest_context_segments" => {
            Box::pin(handle_ingest_context_segments(args, storage, ctx, session))
        }
        "search_context_segments" => {
            Box::pin(handle_search_context_segments(args, storage, ctx, session))
        }
        // --- Remote teacher/learner memory tools ---
        "feedback_record" => Box::pin(handle_feedback_record(args, storage, ctx)),
        "usage_mark" => Box::pin(handle_usage_mark(args, ctx)),
        "trust_update" => Box::pin(handle_trust_update(args, storage, ctx)),
        "teach_query_stream" => Box::pin(handle_teach_query_stream(args, storage, ctx)),
        "pull_preview" => Box::pin(handle_pull_preview(args, storage, ctx)),
        "pull_commit" => Box::pin(handle_pull_commit(args, storage, ctx)),
        "remote_list" => Box::pin(handle_remote_list(args, storage, ctx)),
        "remote_add" => Box::pin(handle_remote_add(args, storage, ctx)),
        "remote_update_policy" => Box::pin(handle_remote_update_policy(args, storage, ctx)),
        "remote_remove" => Box::pin(handle_remote_remove(args, storage, ctx)),
        "remote_health" => Box::pin(handle_remote_health(args, storage, ctx)),
        "remote_detail" => Box::pin(handle_remote_capabilities(args, storage, ctx)),
        "remote_capabilities" => Box::pin(handle_remote_capabilities(args, storage, ctx)),
        "remote_explain_policy" => Box::pin(handle_remote_explain_policy(args, storage, ctx)),
        "get_context_window" => Box::pin(handle_get_context_window(args, storage, ctx)),
        "get_chunk_context" => Box::pin(handle_get_chunk_context(args, storage, ctx)),
        "get_turn_chain" => Box::pin(handle_get_turn_chain(args, storage, ctx)),
        "upsert_entity" => Box::pin(handle_upsert_entity(args, storage, ctx, session)),
        "batch_ingest" => Box::pin(handle_batch_ingest(args, storage, ctx, session)),
        "ingest_entities" => Box::pin(handle_ingest_entities(args, storage, ctx, session)),
        "retrieve_entities" => Box::pin(handle_retrieve_entities(args, storage, ctx, session)),
        "list_entities" => Box::pin(handle_list_entities(args, storage, ctx)),
        "record_outcome" => Box::pin(handle_record_outcome(args, storage, ctx)),
        "record_feedback" | "record_last_retrieval_feedback" => {
            Box::pin(handle_record_feedback(args, storage, ctx, session))
        }
        "configure" => Box::pin(handle_configure(args, session)),
        "delete_session" => Box::pin(handle_delete_session(args, storage, ctx)),
        "smart_ingest" => Box::pin(handle_smart_ingest(args, storage, ctx, session)),
        "ingest_skill" => Box::pin(handle_ingest_skill(args, storage, ctx, session)),
        "retrieve_skills_for_context" => Box::pin(handle_retrieve_skills_for_context(
            args, storage, ctx, session,
        )),
        "invoke_skill" => Box::pin(handle_invoke_skill(args, storage, ctx, session)),
        "ensure_parent_tag" => Box::pin(handle_ensure_parent_tag(args, storage, ctx, session)),
        "verify_skill" => Box::pin(handle_verify_skill(args, storage, ctx, session)),
        "set_intention" => Box::pin(handle_set_intention(args, storage, ctx, session)),
        "set_foresight" => Box::pin(handle_set_foresight(args, storage, ctx, session)),
        "check_intentions" => Box::pin(handle_check_intentions(args, storage, ctx, session)),
        "complete_intention" => Box::pin(handle_complete_intention(args, storage, ctx, session)),
        "list_intentions" => Box::pin(handle_list_intentions(args, storage, ctx, session)),
        "snooze_intention" => Box::pin(handle_snooze_intention(args, storage, ctx, session)),
        "write_temporal_fact" => Box::pin(handle_write_temporal_fact(args, storage, ctx, session)),
        "get_temporal_chain" => Box::pin(handle_get_temporal_chain(args, storage, ctx)),
        "explore_connections" => Box::pin(handle_explore_connections(args, storage, ctx, session)),
        "hybrid_search" => Box::pin(handle_hybrid_search(args, storage, ctx, session)),
        "manage_authority" => Box::pin(handle_manage_authority(args, storage, ctx)),
        "run_consolidation" => Box::pin(handle_run_consolidation(args, storage, ctx, session)),
        "enrich_entities" => Box::pin(handle_enrich_entities(args, storage, ctx, session)),
        "get_stats" => Box::pin(handle_get_stats(args, storage, ctx, session)),
        "memory_metrics" => Box::pin(handle_memory_metrics(storage, ctx, session)),
        "migration_status" => Box::pin(handle_migration_status(args, storage)),
        "describe" => Box::pin(handle_system_describe(args, storage, ctx, session)),
        "forget" => Box::pin(handle_forget(args, storage, ctx, session)),
        "restore_forgotten" => Box::pin(handle_restore_forgotten(args, storage, ctx)),
        "count_entities_by_type" => Box::pin(handle_count_entities_by_type(args, storage, ctx)),
        "promote_memory" => Box::pin(handle_promote_memory(args, storage, ctx, session)),
        "demote_memory" => Box::pin(handle_demote_memory(args, storage, ctx, session)),
        "importance_score" => Box::pin(handle_importance_score(args, storage, ctx, session)),
        "find_memory_chain" => Box::pin(handle_find_memory_chain(args, storage, ctx)),
        "predict_needed" => Box::pin(handle_predict_needed(args, session)),
        "spread_activation" => Box::pin(handle_spread_activation(args, storage, ctx)),
        "find_duplicates" => Box::pin(handle_find_duplicates(args, storage, ctx)),
        "recursive_explore" => Box::pin(handle_recursive_explore(args, storage, ctx, session)),
        "query_derived" => Box::pin(handle_query_derived(args, storage, ctx)),
        "manage_rules" => Box::pin(handle_manage_rules(args, storage, ctx)),
        "manage_claims" => Box::pin(handle_manage_claims(args, storage, ctx)),
        "manage_approvals" => Box::pin(handle_manage_approvals(args, storage, ctx)),
        "manage_aliases" => Box::pin(handle_manage_aliases(args, storage, ctx)),
        "explain_derived" => Box::pin(handle_explain_derived(args, storage, ctx)),
        "get_effective_rule_set" => Box::pin(handle_get_effective_rule_set(args, storage, ctx)),
        "promote_predicate" => Box::pin(handle_promote_predicate(args, storage, ctx)),
        "batch_update_entities" => {
            Box::pin(handle_batch_update_entities(args, storage, ctx, session))
        }
        "batch_delete_entities" => {
            Box::pin(handle_batch_delete_entities(args, storage, ctx, session))
        }
        "create_edge" => Box::pin(handle_create_edge(args, storage, ctx, session)),
        "batch_create_edges" => Box::pin(handle_batch_create_edges(args, storage, ctx, session)),
        "batch_update_edges" => Box::pin(handle_batch_update_edges(args, storage, ctx, session)),
        "batch_delete_edges" => Box::pin(handle_batch_delete_edges(args, storage, ctx, session)),
        "list_derived_cache" => Box::pin(handle_list_derived_cache(args, storage, ctx)),
        _ => Box::pin(async move { Err((METHOD_NOT_FOUND, format!("unknown tool: {name}"))) }),
    };
    let result = handler.await;
    let elapsed = start.elapsed();
    match &result {
        Ok(v) => {
            let bytes = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
            tracing::info!(
                tool = name,
                elapsed_ms = elapsed.as_millis() as u64,
                response_bytes = bytes,
                "tool call completed"
            );
            tracing::debug!(
                tool = name,
                elapsed_ms = elapsed.as_millis() as u64,
                response_bytes = bytes,
                est_tokens = bytes / 4,
                "tool call OK"
            );
        }
        Err((code, msg)) => tracing::warn!(
            tool = name,
            code,
            msg,
            elapsed_ms = elapsed.as_millis() as u64,
            "tool call FAILED"
        ),
    }

    // Signal activity for idle consolidation timer.
    session.last_activity.notify_one();

    // Mark dirty on successful write operations so idle consolidation knows
    // there is new data worth processing.
    if result.is_ok() && is_write_tool(canonical_name) {
        session.dirty.store(true, Ordering::Relaxed);
    }

    // Wrap in MCP CallToolResult format: { content: [{type: "text", text: "..."}] }
    // MCP clients expect this structure; without it, tool output is invisible.
    let is_err = result.is_err();
    let wrapped = result
        .map(|value| wrap_tool_result(canonical_name, name, &value, elapsed.as_millis() as u64));

    // Log tool usage (best-effort).
    let output_bytes = wrapped
        .as_ref()
        .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
        .unwrap_or(0) as i32;
    let estimated_tokens = (input_bytes + output_bytes) / 4;
    let latency_ms = start.elapsed().as_millis() as i32;
    let repo = session.repo.get().map(|s| s.as_str()).unwrap_or("");
    if let Err(e) = storage
        .tool_usage_put(
            ctx,
            canonical_name,
            repo,
            input_bytes,
            output_bytes,
            estimated_tokens,
            latency_ms,
            is_err,
        )
        .await
    {
        tracing::debug!(error = %e, "tool usage logging failed");
    }

    wrapped
}

/// Wrap a tool result in MCP `CallToolResult` shape.
///
/// `structuredContent` carries call metadata plus the tool's result: **object**
/// results are flattened in (back-compat with existing clients/evals); **non-object**
/// results (arrays, strings, scalars) are placed under `result` so their payload is
/// never hidden from clients that render `structuredContent`. `content[0].text` is the
/// textual fallback for clients that don't.
fn wrap_tool_result(
    canonical_name: &str,
    requested_name: &str,
    value: &serde_json::Value,
    duration_ms: u64,
) -> serde_json::Value {
    let text = if let Some(s) = value.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
    };
    let mut structured_content = serde_json::json!({
        "tool": canonical_name,
        "requested_tool": requested_name,
        "duration_ms": duration_ms,
        "is_error": false
    });
    if let Some(structured) = structured_content.as_object_mut() {
        match value.as_object() {
            Some(obj) => {
                for (key, v) in obj {
                    structured.insert(key.clone(), v.clone());
                }
            }
            None => {
                structured.insert("result".to_string(), value.clone());
            }
        }
    }
    serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured_content
    })
}

#[cfg(test)]
mod wrap_tool_result_tests {
    use super::wrap_tool_result;
    use serde_json::json;

    #[test]
    fn object_result_is_flattened_into_structured_content() {
        let r = wrap_tool_result("get_stats", "get_stats", &json!({"entity_count": 1}), 5);
        assert_eq!(r["structuredContent"]["entity_count"], 1);
        assert_eq!(r["structuredContent"]["tool"], "get_stats");
        assert_eq!(r["structuredContent"]["requested_tool"], "get_stats");
    }

    #[test]
    fn array_result_is_surfaced_under_result() {
        let r = wrap_tool_result("all_tools", "all_tools", &json!(["a", "b"]), 5);
        assert_eq!(r["structuredContent"]["result"], json!(["a", "b"]));
        assert_eq!(r["structuredContent"]["tool"], "all_tools");
    }

    #[test]
    fn string_result_is_surfaced_under_result() {
        let r = wrap_tool_result("ping", "ping", &json!("ok"), 5);
        assert_eq!(r["structuredContent"]["result"], "ok");
        assert_eq!(r["content"][0]["text"], "ok");
    }
}

/// Returns true for tier-1 tools (always visible in tools/list).
/// Tier 2 tools are only returned when `include_all: true` is passed.
fn is_tier1(name: &str) -> bool {
    let name = canonical_tool_name(name);
    matches!(
        name,
        "smart_ingest"
            | "all_tools"
            | "hybrid_search"
            | "configure"
            | "get_chunk_context"
            | "get_turn_chain"
            | "record_feedback"
            | "create_edge"
            | "check_intentions"
            | "set_foresight"
            | "session_task_put"
            | "session_task_get"
            | "session_task_current"
            | "session_task_list"
            | "session_task_complete"
            | "session_task_cancel"
            | "session_task_focus"
            | "session_task_observe"
            | "get_stats"
            | "retrieve_entities"
            | "list_entities"
            | "forget"
    )
}

/// Returns true for tools that modify stored data (writes, upserts, deletes).
/// Used to set the dirty flag for idle consolidation.
fn is_write_tool(name: &str) -> bool {
    let name = canonical_tool_name(name);
    matches!(
        name,
        "store_memo_result"
            | "write_plan_node"
            | "update_plan_node"
            | "session_task_put"
            | "session_task_complete"
            | "session_task_cancel"
            | "session_task_focus"
            | "session_task_observe"
            | "start_fold"
            | "append_to_fold"
            | "complete_fold"
            | "ingest_context_segments"
            | "upsert_entity"
            | "batch_ingest"
            | "ingest_entities"
            | "record_outcome"
            | "record_feedback"
            | "manage_authority"
            | "delete_session"
            | "smart_ingest"
            | "set_intention"
            | "complete_intention"
            | "snooze_intention"
            | "write_temporal_fact"
            | "promote_memory"
            | "demote_memory"
            | "manage_rules"
            | "promote_predicate"
            | "create_edge"
            | "batch_create_edges"
            | "batch_update_edges"
            | "batch_delete_edges"
            | "batch_update_entities"
            | "batch_delete_entities"
            | "enrich_entities"
    )
}

async fn queue_session_for_consolidation(
    session: &SessionState,
    session_id: uuid::Uuid,
) -> Result<bool, (i32, String)> {
    let mut queue = session.consolidation_queue.lock().await;
    if queue.contains(&session_id) {
        Ok(false)
    } else if queue.len() >= CONSOLIDATION_QUEUE_CAPACITY {
        Err((
            INTERNAL_ERROR,
            format!(
                "consolidation queue full (capacity {CONSOLIDATION_QUEUE_CAPACITY}); retry after the idle worker drains pending sessions"
            ),
        ))
    } else {
        queue.push_back(session_id);
        Ok(true)
    }
}

pub async fn record_consolidation_queued(session: &SessionState, session_id: uuid::Uuid) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    session.last_consolidation_status.lock().await.insert(
        session_id,
        ConsolidationRunStatus {
            session_id,
            status: "queued".to_string(),
            started_at: now,
            finished_at: None,
            entities_processed: 0,
            connections_created: 0,
            error: None,
        },
    );
}

pub async fn record_consolidation_running(session: &SessionState, session_id: uuid::Uuid) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    session.last_consolidation_status.lock().await.insert(
        session_id,
        ConsolidationRunStatus {
            session_id,
            status: "running".to_string(),
            started_at: now,
            finished_at: None,
            entities_processed: 0,
            connections_created: 0,
            error: None,
        },
    );
}

pub async fn record_consolidation_finished(
    session: &SessionState,
    session_id: uuid::Uuid,
    result: Result<&crate::dream::DreamResult, &str>,
) {
    let finished_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let started_at = session
        .last_consolidation_status
        .lock()
        .await
        .get(&session_id)
        .map(|status| status.started_at.clone())
        .unwrap_or_else(|| finished_at.clone());
    let status = match result {
        Ok(result) => ConsolidationRunStatus {
            session_id,
            status: "success".to_string(),
            started_at,
            finished_at: Some(finished_at),
            entities_processed: result.entities_processed,
            connections_created: result.connections_created,
            error: None,
        },
        Err(error) => ConsolidationRunStatus {
            session_id,
            status: "failed".to_string(),
            started_at,
            finished_at: Some(finished_at),
            entities_processed: 0,
            connections_created: 0,
            error: Some(error.to_string()),
        },
    };
    session
        .last_consolidation_status
        .lock()
        .await
        .insert(session_id, status);
}

async fn mark_smart_ingest_created_for_consolidation(
    session: &SessionState,
    session_id: uuid::Uuid,
) -> Result<bool, (i32, String)> {
    let should_queue = {
        let mut counters = session
            .smart_ingest_created_since_consolidation
            .lock()
            .await;
        let count = counters.entry(session_id).or_insert(0);
        *count += 1;
        *count >= SMART_INGEST_AUTO_CONSOLIDATE_THRESHOLD
    };

    if !should_queue {
        return Ok(false);
    }

    let queued = queue_session_for_consolidation(session, session_id).await?;
    record_consolidation_queued(session, session_id).await;
    session
        .dirty
        .store(true, std::sync::atomic::Ordering::Relaxed);
    session.last_activity.notify_waiters();

    let mut counters = session
        .smart_ingest_created_since_consolidation
        .lock()
        .await;
    counters.insert(session_id, 0);
    Ok(queued)
}

// --- Tool handlers ---

async fn handle_check_memo<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let prompt = require_str(&args, "prompt")?;
    let context_slice = require_str(&args, "context_slice")?;
    let model_version = require_str(&args, "model_version")?;

    let result = crate::memo::check_memo_cache(storage, ctx, prompt, context_slice, model_version)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let mut json = serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    if let Some(obj) = json.as_object_mut() {
        let hint = if result.hit {
            "Cache hit — reusing prior result. Record outcome with record_outcome for routing optimization."
        } else {
            "Cache miss. After completing the sub-call, store the result with store_memo_result."
        };
        obj.insert("hint".into(), Value::String(hint.into()));
    }
    Ok(json)
}

async fn handle_store_memo<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let prompt = require_str(&args, "prompt")?;
    let context_slice = require_str(&args, "context_slice")?;
    let model_version = require_str(&args, "model_version")?;
    let result_text = require_str(&args, "result")?;

    let embedding: Option<Vec<f32>> = args.get("embedding").and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_f64().map(|f| f as f32))
                .collect()
        })
    });

    let ttl_days = args
        .get("ttl_days")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let params = crate::memo::StoreMemoParams {
        prompt,
        context_slice,
        model_version,
        result: result_text,
        embedding,
        ttl_days,
    };

    let result = crate::memo::store_memo_result(storage, ctx, &params)
        .await
        .map_err(map_quota_error)?;

    // Audit log (best-effort, no session_id for memos)
    let content_hash = result.content_hash.clone();
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "store",
        "memo_cache",
        &content_hash,
        uuid::Uuid::nil(),
    )
    .await;

    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_write_plan<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let depth = require_i32(&args, "depth")?;
    let subtask_id = require_str(&args, "subtask_id")?;
    let parent_subtask = args.get("parent_subtask").and_then(|v| v.as_str());
    let goal_text = require_str(&args, "goal_text")?;

    let written = crate::plan::write_plan_node(
        storage,
        ctx,
        session_id,
        depth,
        subtask_id,
        parent_subtask,
        goal_text,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "written": written }))
}

async fn handle_get_plan<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);

    let plan = crate::plan::get_plan_context(storage, ctx, session_id, max_depth)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(plan).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_update_plan<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let depth = require_i32(&args, "depth")?;
    let subtask_id = require_str(&args, "subtask_id")?;
    let status_str = require_str(&args, "status")?;
    let outcome_summary = args.get("outcome_summary").and_then(|v| v.as_str());

    let status: crate::types::PlanStatus =
        serde_json::from_value(Value::String(status_str.to_string()))
            .map_err(|_| (INVALID_PARAMS, format!("invalid status: {status_str}")))?;

    let updated = crate::plan::update_plan_node(
        storage,
        ctx,
        session_id,
        depth,
        subtask_id,
        status,
        outcome_summary,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "updated": updated }))
}

async fn handle_session_task_put<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let title = require_str(&args, "title")?.to_string();
    let status = match args.get("status").and_then(|value| value.as_str()) {
        Some(value) => Some(parse_session_task_status_param(value)?),
        None => None,
    };
    let input = crate::session_task::SessionTaskUpsert {
        session_id,
        task_id: optional_uuid(&args, "task_id")?,
        title,
        description: args
            .get("description")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        status,
        priority: args
            .get("priority")
            .and_then(|value| value.as_i64())
            .map(|value| value as i32),
        tags: optional_string_array(&args, "tags")?,
        parent_task_id: optional_uuid(&args, "parent_task_id")?,
        client: crate::types::SessionTaskClient {
            agent: args
                .get("client_agent")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            workspace: args
                .get("workspace")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            thread_id: args
                .get("thread_id")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            external_session_id: args
                .get("external_session_id")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        },
        alias_scope: args
            .get("alias_scope")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        alias: args
            .get("alias")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        focus: args
            .get("focus")
            .and_then(|value| value.as_bool())
            .unwrap_or(true),
    };
    let result = crate::session_task::put_task(storage, ctx, input)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_session_task_get<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let task = if let Some(task_id) = optional_uuid(&args, "task_id")? {
        crate::session_task::get_task(storage, ctx, session_id, task_id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
    } else if let (Some(alias_scope), Some(alias)) = (
        args.get("alias_scope").and_then(|value| value.as_str()),
        args.get("alias").and_then(|value| value.as_str()),
    ) {
        crate::session_task::resolve_alias(storage, ctx, session_id, alias_scope, alias)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
    } else {
        return Err((
            INVALID_PARAMS,
            "task_id or alias_scope+alias is required".to_string(),
        ));
    };
    Ok(serde_json::json!({ "task": task }))
}

async fn handle_session_task_current<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let snapshot = crate::session_task::current_tasks(storage, ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(snapshot).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_session_task_list<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let status = match args.get("status").and_then(|value| value.as_str()) {
        Some(value) => Some(parse_session_task_status_param(value)?),
        None => None,
    };
    let tasks = crate::session_task::list_tasks(storage, ctx, session_id, status)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    Ok(serde_json::json!({ "tasks": tasks }))
}

async fn handle_session_task_lifecycle<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    status: crate::types::SessionTaskStatus,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let task_id = require_uuid(&args, "task_id")?;
    let outcome_summary = args
        .get("outcome_summary")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let result = crate::session_task::update_status(
        storage,
        ctx,
        session_id,
        task_id,
        status,
        outcome_summary,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_session_task_focus<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let task_id = require_uuid(&args, "task_id")?;
    let reason = args
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("client_focus");
    let snapshot = crate::session_task::focus_task(storage, ctx, session_id, task_id, reason)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(snapshot).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_session_task_observe<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let event_type = require_str(&args, "event_type")?;
    let task_id = optional_uuid(&args, "task_id")?;
    let title = args
        .get("title")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let payload = args
        .get("payload")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let result = crate::session_task::observe(
        storage, ctx, session_id, event_type, title, task_id, payload,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Fold handlers ---

async fn handle_start_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let depth = require_i32(&args, "depth")?;
    let parent_fold_id = optional_uuid(&args, "parent_fold_id")?;
    let initial_context = require_str(&args, "initial_context")?;

    let fold_id = crate::fold::start_fold(
        storage,
        ctx,
        session_id,
        depth,
        parent_fold_id,
        initial_context,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "fold_id": fold_id.to_string() }))
}

async fn handle_append_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let fold_id = require_uuid(&args, "fold_id")?;
    let repl_turn = require_str(&args, "repl_turn")?;

    let (appended, token_count) =
        crate::fold::append_to_fold(storage, ctx, session_id, fold_id, repl_turn)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "appended": appended, "token_count": token_count }))
}

async fn handle_complete_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let fold_id = require_uuid(&args, "fold_id")?;
    let summary = require_str(&args, "summary")?;
    let embedding = require_f32_array(&args, "embedding")?;

    let (folded, compression_ratio) =
        crate::fold::complete_fold(storage, ctx, session_id, fold_id, summary, embedding)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Emit viz event for fold completion
    session.event_bus.emit(crate::viz::VizEvent::FoldCompleted {
        fold_id: fold_id.to_string(),
        summary: summary.to_string(),
        entity_count: 0, // entity count not tracked at fold level
    });

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "complete",
        "trajectory_folds",
        &fold_id.to_string(),
        session_id,
    )
    .await;

    Ok(serde_json::json!({ "folded": folded, "compression_ratio": compression_ratio }))
}

async fn handle_retrieve_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let query_embedding = require_f32_array(&args, "query_embedding")?;
    let query_text = args.get("query").and_then(|v| v.as_str()).unwrap_or("");

    // Use router for strategy selection
    let decision = crate::router::route(&crate::router::RoutingContext {
        query_text,
        has_entity_name: false,
        has_content_hash: false,
        task_complexity: crate::router::TaskComplexity::Simple,
    });

    // User-provided k and include_raw override the router's suggestion
    let k = if args.get("k").is_some() {
        Some(optional_retrieval_limit(&args, &["k"], session)?)
    } else {
        Some(retrieval_default_limit(session))
    };
    let include_raw = args
        .get("include_raw")
        .and_then(|v| v.as_bool())
        .unwrap_or(decision.include_raw);

    let folds = crate::fold::retrieve_fold_context(
        storage,
        ctx,
        session_id,
        &query_embedding,
        k,
        include_raw,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(&folds).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

fn is_explicit_remember_directive(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    [
        "remember ",
        "remember:",
        "please remember ",
        "please remember:",
        "can you remember ",
        "could you remember ",
        "make a note ",
        "make a note:",
        "note that ",
        "note:",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn explicit_remember_turns(messages: &[ContextMessage]) -> HashSet<i32> {
    messages
        .iter()
        .filter(|message| message.role.eq_ignore_ascii_case("user"))
        .filter(|message| is_explicit_remember_directive(&message.content))
        .map(|message| message.turn_index)
        .collect()
}

async fn apply_explicit_remember_authority<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    session_id: uuid::Uuid,
    result: &crate::context_segment::SegmentIngestResult,
    remember_turns: &HashSet<i32>,
) -> Result<usize, (i32, String)> {
    if remember_turns.is_empty() {
        return Ok(0);
    }
    let mut count = 0usize;
    for segment in &result.segments {
        let covers_remember_turn = remember_turns
            .iter()
            .any(|turn| *turn >= segment.start_turn && *turn <= segment.end_turn);
        if !covers_remember_turn {
            continue;
        }
        set_memory_authority(
            storage,
            ctx,
            segment.segment_id,
            session_id,
            Some(1.0),
            Some(0.85),
        )
        .await?;
        count += 1;
    }
    Ok(count)
}

async fn handle_ingest_context_segments<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let conversation_id = require_str(&args, "conversation_id")?.to_string();
    let messages_value = args
        .get("messages")
        .cloned()
        .ok_or((INVALID_PARAMS, "missing required array: messages".into()))?;
    let messages: Vec<ContextMessage> = serde_json::from_value(messages_value)
        .map_err(|e| (INVALID_PARAMS, format!("invalid messages: {e}")))?;
    if messages.is_empty() {
        return Err((INVALID_PARAMS, "messages must not be empty".into()));
    }
    let remember_turns = explicit_remember_turns(&messages);
    let segmentation: SegmentationConfig = match args.get("segmentation") {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| (INVALID_PARAMS, format!("invalid segmentation: {e}")))?,
        _ => SegmentationConfig::default(),
    };
    let embed_missing = args
        .get("embed_missing")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let embeddings = if embed_missing {
        let preview = crate::context_segment::segment_messages(
            session_id,
            &conversation_id,
            &messages,
            &segmentation,
        )
        .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
        if let Some(client) = session_embedding_client(session) {
            let mut vectors = Vec::with_capacity(preview.len());
            let mut failed = false;
            for segment in preview {
                match client.embed(&segment.segment_text).await {
                    Ok(vector) => vectors.push(vector),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "context segment embedding generation failed; storing segments without vectors"
                        );
                        failed = true;
                        break;
                    }
                }
            }
            if failed { None } else { Some(vectors) }
        } else {
            None
        }
    } else {
        None
    };

    let mut result = crate::context_segment::ingest_context_segments(
        storage,
        ctx,
        IngestContextSegmentsParams {
            session_id,
            conversation_id,
            messages,
            segmentation,
            embed_missing,
        },
        embeddings,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let remembered_segments =
        apply_explicit_remember_authority(storage, ctx, session_id, &result, &remember_turns)
            .await?;
    if remembered_segments > 0 {
        result.warnings.push(format!(
            "authority_seeded_for_explicit_remember:{remembered_segments}"
        ));
    }
    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_search_context_segments<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let query = require_str(&args, "query")?.to_string();
    let mut query_embedding = optional_f32_array(&args, "query_embedding")?;
    if query_embedding.is_none()
        && let Some(client) = session_embedding_client(session)
    {
        match client.embed(&query).await {
            Ok(embedding) => query_embedding = Some(embedding),
            Err(e) => tracing::debug!("context segment query embedding skipped: {e}"),
        };
    }
    let expand = args.get("expand").cloned().unwrap_or(Value::Null);
    let limit = optional_retrieval_limit(&args, &["limit"], session)?;
    let expand_prev = expand.get("prev").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let expand_next = expand.get("next").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let max_expanded_tokens = expand
        .get("max_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(8000) as i32;
    let result = crate::context_segment::search_context_segments(
        storage,
        ctx,
        ContextSegmentSearchParams {
            session_id,
            query,
            query_embedding,
            limit,
            expand_prev,
            expand_next,
            max_expanded_tokens,
        },
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_get_turn_chain<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let start_turn_id = require_uuid(&args, "start_turn_id")?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .clamp(1, 50) as usize;

    let turns =
        crate::turn_chain::walk_turn_chain_forward(storage, ctx, session_id, start_turn_id, limit)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "session_id": session_id.to_string(),
        "start_turn_id": start_turn_id.to_string(),
        "count": turns.len(),
        "turns": turns.iter().map(|t| serde_json::json!({
            "entity_id": t.entity_id.to_string(),
            "entity_name": t.entity_name,
            "created_at": t.created_at.to_rfc3339(),
            "context_snippet": t.context_snippet,
            "properties": t.properties,
        })).collect::<Vec<_>>(),
    }))
}

async fn handle_get_context_window<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let segment_id = require_uuid(&args, "segment_id")?;
    let prev = args.get("prev").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let next = args.get("next").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let max_tokens = args
        .get("max_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(8000) as i32;
    let result = crate::context_segment::get_context_window(
        storage,
        ctx,
        ContextWindowParams {
            session_id,
            segment_id,
            prev,
            next,
            max_tokens,
        },
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_get_chunk_context<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let chunk_id = require_uuid(&args, "chunk_id")?;
    let prev = args
        .get("prev")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .min(10) as usize;
    let next = args
        .get("next")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .min(10) as usize;
    let max_tokens = args
        .get("max_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(8000)
        .max(1) as i32;

    let hit = storage
        .document_chunk_get(ctx, session_id, chunk_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        .ok_or((
            INVALID_PARAMS,
            format!("document chunk not found: {chunk_id}"),
        ))?;

    let mut before = Vec::new();
    let mut cursor = hit.prev_chunk_id;
    while before.len() < prev {
        let Some(id) = cursor else { break };
        let Some(chunk) = storage
            .document_chunk_get(ctx, session_id, id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        else {
            break;
        };
        cursor = chunk.prev_chunk_id;
        before.push(chunk);
    }
    before.reverse();
    let has_more_prev = cursor.is_some();

    let mut after = Vec::new();
    cursor = hit.next_chunk_id;
    while after.len() < next {
        let Some(id) = cursor else { break };
        let Some(chunk) = storage
            .document_chunk_get(ctx, session_id, id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        else {
            break;
        };
        cursor = chunk.next_chunk_id;
        after.push(chunk);
    }
    let has_more_next = cursor.is_some();

    let mut chunks = Vec::new();
    chunks.extend(before);
    chunks.push(hit.clone());
    chunks.extend(after);
    chunks.sort_by_key(|chunk| chunk.ordinal);

    let mut total_tokens = 0i32;
    let mut returned = Vec::new();
    for chunk in chunks {
        let is_hit = chunk.chunk_id == hit.chunk_id;
        if !is_hit && total_tokens + chunk.token_count > max_tokens {
            continue;
        }
        total_tokens += chunk.token_count.max(0);
        returned.push(serde_json::json!({
            "chunk_id": chunk.chunk_id,
            "document_id": chunk.document_id,
            "ordinal": chunk.ordinal,
            "source_doc_id": chunk.source_doc_id,
            "title": chunk.title,
            "section_path": chunk.section_path,
            "semantic_kind": chunk.semantic_kind,
            "content": chunk.content,
            "token_count": chunk.token_count,
            "prev_chunk_id": chunk.prev_chunk_id,
            "next_chunk_id": chunk.next_chunk_id,
            "overlap_from_prev": chunk.overlap_from_prev,
            "overlap_to_next": chunk.overlap_to_next,
            "metadata": chunk.metadata,
            "is_hit": is_hit,
        }));
    }

    Ok(serde_json::json!({
        "document_id": hit.document_id,
        "hit_chunk_id": hit.chunk_id,
        "chunks": returned,
        "total_tokens": total_tokens,
        "has_more_prev": has_more_prev,
        "has_more_next": has_more_next,
        "hint": "Chunks are ordered by document ordinal. Increase prev/next or call chunk_ctx on a boundary chunk if the answer continues outside this window."
    }))
}

// --- Entity handlers ---

async fn handle_upsert_entity<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let entity_name = require_str(&args, "entity_name")?;
    let entity_type = require_str(&args, "entity_type")?;
    let context_snippet = require_str(&args, "context_snippet")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let source_fold_id = optional_uuid(&args, "source_fold_id")?;
    let confidence = args.get("confidence").and_then(|v| v.as_f64());

    // Auto-generate embedding if not provided and Ollama is configured.
    if embedding.is_none()
        && let Some(client) = session_embedding_client(session)
    {
        match client.embed(context_snippet).await {
            Ok(emb) => embedding = Some(emb),
            Err(e) => tracing::debug!("embedding generation skipped: {e}"),
        }
    }

    let result = crate::entity::upsert_entity(
        storage,
        ctx,
        session_id,
        entity_name,
        entity_type,
        context_snippet,
        embedding,
        source_fold_id,
        confidence,
    )
    .await
    .map_err(map_quota_error)?;

    let result_json = serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Emit viz event for entity upsert
    let entity_id = result_json
        .get("entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_new = result_json
        .get("is_new")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let action = if is_new { "created" } else { "updated" };

    session.event_bus.emit(crate::viz::VizEvent::EntityChanged {
        node: crate::viz::VizNode {
            id: entity_id.clone(),
            label: entity_name.to_string(),
            node_type: "entity".into(),
            entity_type: entity_type.to_string(),
            state: "active".into(),
            confidence: confidence.unwrap_or(1.0),
            created_at: chrono::Utc::now().to_rfc3339(),
            context: context_snippet.to_string(),
            child_count: None,
            ..Default::default()
        },
        action: action.into(),
    });

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "upsert",
        "entity_store",
        &entity_id,
        session_id,
    )
    .await;

    Ok(result_json)
}

async fn handle_batch_ingest<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let entities = args
        .get("entities")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "entities must be an array".to_string()))?;

    if entities.len() > 100 {
        return Err((
            INVALID_PARAMS,
            format!(
                "entities array length {} exceeds maximum of 100",
                entities.len()
            ),
        ));
    }

    let mut results = Vec::with_capacity(entities.len());
    let mut created: usize = 0;
    let mut skipped: usize = 0;
    let mut errors: usize = 0;

    for (i, entity_json) in entities.iter().enumerate() {
        let name = entity_json
            .get("entity_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let entity_type = entity_json
            .get("entity_type")
            .and_then(|v| v.as_str())
            .unwrap_or("concept");
        let context = entity_json
            .get("context_snippet")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let confidence = entity_json.get("confidence").and_then(|v| v.as_f64());

        if name.is_empty() || context.is_empty() {
            skipped += 1;
            results.push(serde_json::json!({
                "index": i, "status": "skipped", "reason": "empty name or context"
            }));
            continue;
        }

        match crate::entity::upsert_entity(
            storage,
            ctx,
            session_id,
            name,
            entity_type,
            context,
            None,
            None,
            confidence,
        )
        .await
        {
            Ok(result) => {
                let status = if result.is_new { "created" } else { "existing" };
                if result.is_new {
                    created += 1;
                } else {
                    skipped += 1;
                }

                // Emit viz event for each entity
                session.event_bus.emit(crate::viz::VizEvent::EntityChanged {
                    node: crate::viz::VizNode {
                        id: result.entity_id.to_string(),
                        label: name.to_string(),
                        node_type: "entity".into(),
                        entity_type: entity_type.to_string(),
                        state: "active".into(),
                        confidence: confidence.unwrap_or(1.0),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        context: context.to_string(),
                        child_count: None,
                        ..Default::default()
                    },
                    action: status.into(),
                });

                results.push(serde_json::json!({
                    "index": i,
                    "status": status,
                    "entity_id": result.entity_id.to_string(),
                }));
            }
            Err(e) => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": i, "status": "error", "error": e.to_string()
                }));
            }
        }
    }

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "batch_ingest",
        "entity_store",
        &format!("{} entities", entities.len()),
        session_id,
    )
    .await;

    Ok(serde_json::json!({
        "created": created,
        "skipped": skipped,
        "errors": errors,
        "total": entities.len(),
        "results": results,
        "hint": format!(
            "Batch ingested {} entities. Run run_consolidation to build edges.",
            entities.len()
        )
    }))
}

fn default_ingest_confidence() -> f64 {
    0.9
}

fn default_edge_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum IngestOnConflict {
    #[default]
    Update,
    Skip,
    Error,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestEntitiesOptions {
    embed_missing: bool,
    embedding_model: Option<String>,
    on_conflict: IngestOnConflict,
    strict_edges: bool,
    dry_run: bool,
}

impl Default for IngestEntitiesOptions {
    fn default() -> Self {
        Self {
            embed_missing: true,
            embedding_model: None,
            on_conflict: IngestOnConflict::Update,
            strict_edges: true,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestEntityInput {
    id: uuid::Uuid,
    name: String,
    entity_type: String,
    context: String,
    #[serde(default = "default_ingest_confidence")]
    confidence: f64,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    embedding: Option<Vec<f32>>,
    #[serde(default)]
    attrs: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestEdgeInput {
    src_id: uuid::Uuid,
    dst_id: uuid::Uuid,
    edge_type: String,
    #[serde(default = "default_edge_weight")]
    weight: f64,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestEntitiesRequest {
    tenant_id: uuid::Uuid,
    session_id: uuid::Uuid,
    entities: Vec<IngestEntityInput>,
    #[serde(default)]
    edges: Vec<IngestEdgeInput>,
    #[serde(default)]
    options: IngestEntitiesOptions,
}

fn parse_ingest_state(state: Option<&str>) -> Result<crate::types::MemoryState, String> {
    match state {
        Some(raw) => serde_json::from_value::<crate::types::MemoryState>(Value::String(raw.into()))
            .map_err(|_| {
                format!(
                    "invalid_state: expected one of active|dormant|silent|unavailable, got {raw}"
                )
            }),
        None => Ok(crate::types::MemoryState::Active),
    }
}

fn validate_json_object(value: Option<&Value>, field: &str) -> Result<(), String> {
    match value {
        None => Ok(()),
        Some(Value::Object(_)) => Ok(()),
        Some(Value::Null) => Ok(()),
        Some(_) => Err(format!("{field} must be an object")),
    }
}

fn embedding_provider_is_disabled(provider: &str) -> bool {
    let provider = provider.trim();
    provider.is_empty()
        || provider.eq_ignore_ascii_case("disabled")
        || provider.eq_ignore_ascii_case("none")
}

fn embedding_provider_requires_url(provider: &str) -> bool {
    !provider.eq_ignore_ascii_case("synthetic")
}

fn build_ingest_embedding_client(
    session: &SessionState,
    override_model: Option<&str>,
) -> Option<crate::embedding::EmbeddingClient> {
    if embedding_provider_is_disabled(&session.embed_provider) {
        return None;
    }
    if embedding_provider_requires_url(&session.embed_provider)
        && session.ollama_base_url.is_empty()
    {
        return None;
    }
    let model = override_model
        .filter(|m| !m.is_empty())
        .unwrap_or(&session.embed_model)
        .to_string();
    if model.is_empty() && !session.embed_provider.eq_ignore_ascii_case("synthetic") {
        return None;
    }
    Some(crate::embedding::EmbeddingClient::new(
        &crate::config::EmbeddingConfig {
            provider: session.embed_provider.clone(),
            ollama_base_url: session.ollama_base_url.clone(),
            model,
            dimensions: session.embed_dimensions,
            max_input_chars: crate::config::EmbeddingConfig::default().max_input_chars,
            ner_model: String::new(),
        },
    ))
}

fn session_embedding_client(session: &SessionState) -> Option<crate::embedding::EmbeddingClient> {
    build_ingest_embedding_client(session, None)
}

#[derive(Default)]
struct DocumentIndexStats {
    chunks_indexed: usize,
    chunk_embeddings_computed: usize,
    chunk_embeddings_failed: usize,
}

fn document_entity_requires_chunk_index(entity_type: &str) -> bool {
    matches!(entity_type, "document" | "benchmark_document")
}

fn semantic_kind_label(kind: crate::document_chunking::SemanticChunkKind) -> &'static str {
    match kind {
        crate::document_chunking::SemanticChunkKind::Heading => "heading",
        crate::document_chunking::SemanticChunkKind::Paragraph => "paragraph",
        crate::document_chunking::SemanticChunkKind::List => "list",
        crate::document_chunking::SemanticChunkKind::CodeFence => "code",
        crate::document_chunking::SemanticChunkKind::Mixed => "mixed",
    }
}

fn sha256_hex(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    format!("sha256:{}", hex::encode(hash))
}

async fn index_document_entity_chunks<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    request: &IngestEntitiesRequest,
    entity: &IngestEntityInput,
    embedding_client: Option<&crate::embedding::EmbeddingClient>,
) -> anyhow::Result<DocumentIndexStats> {
    if !document_entity_requires_chunk_index(&entity.entity_type) || request.options.dry_run {
        return Ok(DocumentIndexStats::default());
    }

    let config = crate::document_chunking::DocumentChunkConfig {
        max_chars: 2_000,
        overlap_chars: 240,
    };
    let chunks = crate::document_chunking::chunk_markdown_document(&entity.context, &config);
    let chunk_ids: Vec<uuid::Uuid> = chunks
        .iter()
        .map(|chunk| {
            let content_hash = sha256_hex(&chunk.text);
            uuid::Uuid::new_v5(
                &entity.id,
                format!("chunk:{}:{content_hash}", chunk.ordinal).as_bytes(),
            )
        })
        .collect();

    let now = chrono::Utc::now();
    let source_doc_id = entity
        .attrs
        .as_ref()
        .and_then(|attrs| attrs.get("doc_id"))
        .and_then(Value::as_str)
        .unwrap_or(&entity.name)
        .to_string();
    let mut stats = DocumentIndexStats::default();

    for (idx, chunk) in chunks.into_iter().enumerate() {
        let mut embedding = None;
        if let Some(client) = embedding_client {
            match client.embed(&chunk.text).await {
                Ok(value) => {
                    stats.chunk_embeddings_computed += 1;
                    embedding = Some(value);
                }
                Err(err) => {
                    stats.chunk_embeddings_failed += 1;
                    tracing::warn!(
                        document_id = %entity.id,
                        ordinal = chunk.ordinal,
                        error = %err,
                        "document chunk embedding failed; indexing lexical/phonetic signals only"
                    );
                }
            }
        }

        let document_chunk = crate::types::DocumentChunk {
            tenant_id: request.tenant_id,
            session_id: request.session_id,
            document_id: entity.id,
            chunk_id: chunk_ids[idx],
            ordinal: chunk.ordinal as i32,
            source_doc_id: source_doc_id.clone(),
            title: entity.name.clone(),
            section_path: chunk.section_path.join(" > "),
            semantic_kind: semantic_kind_label(chunk.semantic_kind).into(),
            content: chunk.text.clone(),
            bm25_text: chunk.bm25_text.clone(),
            chunk_embedding: embedding,
            token_count: chunk.text.split_whitespace().count() as i32,
            content_hash: sha256_hex(&chunk.text),
            prev_chunk_id: chunk
                .prev_ordinal
                .and_then(|ordinal| chunk_ids.get(ordinal).copied()),
            next_chunk_id: chunk
                .next_ordinal
                .and_then(|ordinal| chunk_ids.get(ordinal).copied()),
            overlap_from_prev: chunk.has_leading_overlap,
            overlap_to_next: chunk.has_trailing_overlap,
            metadata: serde_json::json!({
                "entity_type": entity.entity_type,
                "attrs": entity.attrs,
                "neighbor_hint": "Use chunk_ctx with prev/next when adjacent list items or surrounding context may matter."
            }),
            created_at: now,
            updated_at: now,
        };
        storage.document_chunk_put(ctx, &document_chunk).await?;
        stats.chunks_indexed += 1;
    }

    Ok(stats)
}

async fn handle_ingest_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let request: IngestEntitiesRequest = serde_json::from_value(args).map_err(|e| {
        (
            INVALID_PARAMS,
            format!("invalid ingest_entities request: {e}"),
        )
    })?;
    if request.tenant_id != ctx.tenant_id {
        return Err((
            INVALID_PARAMS,
            format!(
                "tenant_id {} does not match authenticated tenant {}",
                request.tenant_id, ctx.tenant_id
            ),
        ));
    }

    let started = std::time::Instant::now();
    let embedding_client =
        build_ingest_embedding_client(session, request.options.embedding_model.as_deref());
    let progress_total = request.entities.len() + request.edges.len();

    let mut entity_inserted = 0usize;
    let mut entity_updated = 0usize;
    let mut entity_skipped = 0usize;
    let mut entity_failed = Vec::new();
    let mut edge_inserted = 0usize;
    let mut edge_skipped_duplicate = 0usize;
    let mut edge_failed = Vec::new();
    let mut embeddings_computed = 0usize;
    let mut embeddings_received = 0usize;
    let mut embeddings_failed = Vec::new();
    let mut document_chunks_indexed = 0usize;
    let mut document_chunk_embeddings_computed = 0usize;
    let mut document_chunk_embeddings_failed = 0usize;
    let mut document_index_failed = Vec::new();
    let mut turn_chain_edges_created = 0usize;

    let mut available_entities = std::collections::HashSet::new();
    let mut seen_entity_ids = std::collections::HashSet::new();

    for entity in &request.entities {
        if !seen_entity_ids.insert(entity.id) {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": "duplicate_entity_id_in_batch"
            }));
            continue;
        }

        if !(0.0..=1.0).contains(&entity.confidence) {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": format!("invalid_confidence: {}", entity.confidence)
            }));
            continue;
        }
        if entity.name.trim().is_empty() {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": "missing_name"
            }));
            continue;
        }
        if entity.context.trim().is_empty() {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": "missing_context"
            }));
            continue;
        }
        if let Err(reason) = validate_json_object(entity.attrs.as_ref(), "attrs") {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": reason
            }));
            continue;
        }
        let state = match parse_ingest_state(entity.state.as_deref()) {
            Ok(state) => state,
            Err(reason) => {
                entity_failed.push(serde_json::json!({
                    "id": entity.id.to_string(),
                    "reason": reason
                }));
                continue;
            }
        };

        let existing = storage
            .entity_get_by_id(ctx, request.session_id, entity.id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

        if existing.is_some() {
            match request.options.on_conflict {
                IngestOnConflict::Skip => {
                    entity_skipped += 1;
                    available_entities.insert(entity.id);
                    continue;
                }
                IngestOnConflict::Error => {
                    entity_failed.push(serde_json::json!({
                        "id": entity.id.to_string(),
                        "reason": "conflict: entity already exists"
                    }));
                    continue;
                }
                IngestOnConflict::Update => {}
            }
        }

        let resolved_embedding = if let Some(embedding) = entity.embedding.clone() {
            embeddings_received += 1;
            Some(embedding)
        } else if let Some(existing) = existing.as_ref().and_then(|e| e.entity_embedding.clone()) {
            Some(existing)
        } else if request.options.embed_missing {
            match &embedding_client {
                Some(client) => match client.embed(&entity.context).await {
                    Ok(embedding) => {
                        embeddings_computed += 1;
                        Some(embedding)
                    }
                    Err(err) => {
                        let reason = format!("embedding_failed: {err}");
                        embeddings_failed.push(serde_json::json!({
                            "id": entity.id.to_string(),
                            "reason": reason
                        }));
                        entity_failed.push(serde_json::json!({
                            "id": entity.id.to_string(),
                            "reason": "embedding_failed"
                        }));
                        continue;
                    }
                },
                None => {
                    embeddings_failed.push(serde_json::json!({
                        "id": entity.id.to_string(),
                        "reason": "embedding_unavailable: embed_missing requested but embedding endpoint is not configured"
                    }));
                    entity_failed.push(serde_json::json!({
                        "id": entity.id.to_string(),
                        "reason": "embedding_unavailable"
                    }));
                    continue;
                }
            }
        } else {
            None
        };

        let now = chrono::Utc::now();
        let entry = crate::types::EntityEntry {
            tenant_id: request.tenant_id,
            entity_id: entity.id,
            session_id: request.session_id,
            entity_name: entity.name.clone(),
            entity_type: entity.entity_type.clone(),
            source_fold_id: existing.as_ref().and_then(|e| e.source_fold_id),
            context_snippet: entity.context.clone(),
            entity_embedding: resolved_embedding,
            confidence: entity.confidence,
            state: existing
                .as_ref()
                .map(|e| {
                    if entity.state.is_some() {
                        state.clone()
                    } else {
                        e.state.clone()
                    }
                })
                .unwrap_or(state),
            created_at: existing.as_ref().map(|e| e.created_at).unwrap_or(now),
            description: existing.as_ref().and_then(|e| e.description.clone()),
            description_embedding: existing
                .as_ref()
                .and_then(|e| e.description_embedding.clone()),
            tags: existing
                .as_ref()
                .map(|e| e.tags.clone())
                .unwrap_or_default(),
            properties: entity.attrs.clone().unwrap_or_else(|| {
                existing
                    .as_ref()
                    .map(|e| e.properties.clone())
                    .unwrap_or_else(|| serde_json::json!({}))
            }),
            content_hash: existing.as_ref().and_then(|e| e.content_hash.clone()),
            updated_at: Some(now),
            scope: existing
                .as_ref()
                .map(|e| e.scope)
                .unwrap_or(crate::types::EntityScope::Session),
            ingested_by_session: existing
                .as_ref()
                .and_then(|e| e.ingested_by_session)
                .or(Some(request.session_id)),
        };

        if !request.options.dry_run
            && let Err(err) = storage.entity_put(ctx, &entry).await
        {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": err.to_string()
            }));
            continue;
        }
        if !request.options.dry_run {
            let visible = storage
                .entity_get_by_id(ctx, request.session_id, entity.id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            match visible {
                Some(stored)
                    if stored.entity_type == entity.entity_type
                        && stored.entity_name == entity.name
                        && stored.context_snippet == entity.context => {}
                Some(_) => {
                    entity_failed.push(serde_json::json!({
                        "id": entity.id.to_string(),
                        "reason": "entity row visible after write but stored values did not match request"
                    }));
                    continue;
                }
                None => {
                    entity_failed.push(serde_json::json!({
                        "id": entity.id.to_string(),
                        "reason": "entity row not visible after write"
                    }));
                    continue;
                }
            }
        }

        if existing.is_some() {
            entity_updated += 1;
        } else {
            entity_inserted += 1;
            // Auto-chain turn entities: link the new turn to its predecessor
            // in the same session so agent sessions form traversable threads.
            // This mirrors the next/previous_context_segment edge pattern.
            // Only fires for freshly-inserted turns; updates and dry-runs skip
            // this block entirely. Covers every turn-like type (canonical
            // "turn" from the Claude hook and "conversation_turn" from Hermes).
            if !request.options.dry_run && crate::turn_chain::is_turn_type(&entry.entity_type) {
                match crate::turn_chain::link_turn_to_predecessor(
                    storage,
                    ctx,
                    request.session_id,
                    &entry,
                )
                .await
                {
                    Ok(true) => turn_chain_edges_created += 2,
                    Ok(false) => {}
                    Err(err) => {
                        tracing::warn!(
                            entity_id = %entry.entity_id,
                            session_id = %request.session_id,
                            error = %err,
                            "turn chain edge creation failed (non-fatal)"
                        );
                    }
                }
            }
        }
        available_entities.insert(entity.id);

        match index_document_entity_chunks(
            storage,
            ctx,
            &request,
            entity,
            embedding_client.as_ref(),
        )
        .await
        {
            Ok(stats) => {
                document_chunks_indexed += stats.chunks_indexed;
                document_chunk_embeddings_computed += stats.chunk_embeddings_computed;
                document_chunk_embeddings_failed += stats.chunk_embeddings_failed;
            }
            Err(err) => {
                document_index_failed.push(serde_json::json!({
                    "id": entity.id.to_string(),
                    "reason": err.to_string()
                }));
            }
        }
    }

    let mut resident_cache = std::collections::HashMap::<uuid::Uuid, bool>::new();
    let mut existing_edge_cache =
        std::collections::HashMap::<uuid::Uuid, Vec<crate::types::TypedEdge>>::new();
    let mut seen_edges = std::collections::HashSet::<(uuid::Uuid, uuid::Uuid, String)>::new();

    for edge in &request.edges {
        if edge.edge_type.trim().is_empty() {
            edge_failed.push(serde_json::json!({
                "src_id": edge.src_id.to_string(),
                "dst_id": edge.dst_id.to_string(),
                "edge_type": edge.edge_type,
                "reason": "missing_edge_type"
            }));
            continue;
        }
        if let Err(reason) = validate_json_object(edge.metadata.as_ref(), "metadata") {
            edge_failed.push(serde_json::json!({
                "src_id": edge.src_id.to_string(),
                "dst_id": edge.dst_id.to_string(),
                "edge_type": edge.edge_type,
                "reason": reason
            }));
            continue;
        }
        let dedupe_key = (edge.src_id, edge.dst_id, edge.edge_type.clone());
        if !seen_edges.insert(dedupe_key) {
            edge_skipped_duplicate += 1;
            continue;
        }

        if request.options.strict_edges {
            let src_ok = if available_entities.contains(&edge.src_id) {
                true
            } else if let Some(found) = resident_cache.get(&edge.src_id) {
                *found
            } else {
                let found = storage
                    .entity_get_by_id(ctx, request.session_id, edge.src_id)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                    .is_some();
                resident_cache.insert(edge.src_id, found);
                found
            };
            if !src_ok {
                edge_failed.push(serde_json::json!({
                    "src_id": edge.src_id.to_string(),
                    "dst_id": edge.dst_id.to_string(),
                    "edge_type": edge.edge_type,
                    "reason": "endpoint_not_found: src_id not resident and not in batch"
                }));
                continue;
            }

            let dst_ok = if available_entities.contains(&edge.dst_id) {
                true
            } else if let Some(found) = resident_cache.get(&edge.dst_id) {
                *found
            } else {
                let found = storage
                    .entity_get_by_id(ctx, request.session_id, edge.dst_id)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                    .is_some();
                resident_cache.insert(edge.dst_id, found);
                found
            };
            if !dst_ok {
                edge_failed.push(serde_json::json!({
                    "src_id": edge.src_id.to_string(),
                    "dst_id": edge.dst_id.to_string(),
                    "edge_type": edge.edge_type,
                    "reason": "endpoint_not_found: dst_id not resident and not in batch"
                }));
                continue;
            }
        }

        let existing_edges = if let Some(edges) = existing_edge_cache.get(&edge.src_id) {
            edges.clone()
        } else {
            let edges = storage
                .typed_edge_list_from(ctx, request.session_id, edge.src_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            existing_edge_cache.insert(edge.src_id, edges.clone());
            edges
        };
        if existing_edges
            .iter()
            .any(|existing| existing.dst_id == edge.dst_id && existing.edge_type == edge.edge_type)
        {
            edge_skipped_duplicate += 1;
            continue;
        }

        if !request.options.dry_run {
            let metadata = edge.metadata.as_ref().map(|value| value.to_string());
            match crate::graph_write::create_typed_edge(
                storage,
                ctx,
                request.session_id,
                edge.src_id,
                edge.edge_type.clone(),
                edge.dst_id,
                edge.weight,
                metadata,
            )
            .await
            {
                Ok(created) => {
                    existing_edge_cache
                        .entry(edge.src_id)
                        .or_default()
                        .push(created);
                }
                Err(err) => {
                    edge_failed.push(serde_json::json!({
                        "src_id": edge.src_id.to_string(),
                        "dst_id": edge.dst_id.to_string(),
                        "edge_type": edge.edge_type,
                        "reason": err.to_string()
                    }));
                    continue;
                }
            }
        }

        edge_inserted += 1;
    }

    let _ = crate::audit::log_write(
        storage,
        ctx,
        "ingest_entities",
        "entity_store",
        &format!(
            "{} entities, {} edges",
            request.entities.len(),
            request.edges.len()
        ),
        request.session_id,
    )
    .await;

    Ok(serde_json::json!({
        "entities": {
            "inserted": entity_inserted,
            "updated": entity_updated,
            "skipped": entity_skipped,
            "failed": entity_failed,
        },
        "edges": {
            "inserted": edge_inserted,
            "skipped_duplicate": edge_skipped_duplicate,
            "failed": edge_failed,
        },
        "embeddings": {
            "computed": embeddings_computed,
            "received": embeddings_received,
            "failed": embeddings_failed,
        },
        "document_index": {
            "chunks_indexed": document_chunks_indexed,
            "chunk_embeddings_computed": document_chunk_embeddings_computed,
            "chunk_embeddings_failed": document_chunk_embeddings_failed,
            "failed": document_index_failed,
            "hint": "Document chunks are semantic and linked with prev/next IDs. Search results may suggest chunk_ctx expansion when adjacent context matters."
        },
        "turn_chain": {
            "edges_created": turn_chain_edges_created,
            "hint": "next_turn / previous_turn temporal edges link successive turn entities into traversable session threads. Use get_turn_chain to walk."
        },
        "schema_version": "2026-03-01",
        "progress": {
            "bounded": true,
            "total_items": progress_total,
            "events": [
                { "phase": "started", "completed": 0, "total": progress_total },
                { "phase": "entities_done", "completed": request.entities.len(), "total": progress_total },
                { "phase": "edges_done", "completed": progress_total, "total": progress_total },
                { "phase": "complete", "completed": progress_total, "total": progress_total }
            ]
        },
        "duration_ms": started.elapsed().as_millis() as u64,
    }))
}

async fn handle_batch_update_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let entities = args
        .get("entities")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "entities must be an array".to_string()))?;

    if entities.len() > 100 {
        return Err((
            INVALID_PARAMS,
            format!(
                "entities array length {} exceeds maximum of 100",
                entities.len()
            ),
        ));
    }

    let jobs = entities
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, entity_json)| async move {
            let Some(row) = entity_json.as_object() else {
                return BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Error,
                    result: serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": format!("batch_update_entities[{idx}] must be an object")
                    }),
                };
            };

            let entity_id = match row
                .get("entity_id")
                .and_then(|v| v.as_str())
                .and_then(|v| uuid::Uuid::parse_str(v).ok())
            {
                Some(id) => id,
                None => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": format!("batch_update_entities[{idx}] missing/invalid entity_id")
                        }),
                    };
                }
            };

            let mut entity = match storage.entity_get_by_id(ctx, session_id, entity_id).await {
                Ok(Some(entity)) => entity,
                Ok(None) => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::NotFound,
                        result: serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "not_found"
                        }),
                    };
                }
                Err(err) => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": err.to_string()
                        }),
                    };
                }
            };

            let mut mutated = false;
            if let Some(v) = row.get("entity_name") {
                match v.as_str() {
                    Some(v) => {
                        entity.entity_name = v.to_string();
                        mutated = true;
                    }
                    None => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": "entity_name must be a string"
                            }),
                        };
                    }
                }
            }
            if let Some(v) = row.get("entity_type") {
                match v.as_str() {
                    Some(v) => {
                        entity.entity_type = v.to_string();
                        mutated = true;
                    }
                    None => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": "entity_type must be a string"
                            }),
                        };
                    }
                }
            }
            if let Some(v) = row.get("context_snippet") {
                match v.as_str() {
                    Some(v) => {
                        entity.context_snippet = v.to_string();
                        mutated = true;
                    }
                    None => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": "context_snippet must be a string"
                            }),
                        };
                    }
                }
            }

            if let Some(v) = row.get("source_fold_id") {
                if v.is_null() {
                    entity.source_fold_id = None;
                    mutated = true;
                } else if let Some(raw) = v.as_str() {
                    entity.source_fold_id = match uuid::Uuid::parse_str(raw) {
                        Ok(id) => {
                            mutated = true;
                            Some(id)
                        }
                        Err(err) => {
                            return BatchMutationOutcome {
                                index: idx,
                                kind: BatchMutationKind::Error,
                                result: serde_json::json!({
                                "index": idx,
                                "entity_id": entity_id.to_string(),
                                "status": "error",
                                "reason": format!("source_fold_id invalid uuid: {err}")
                                }),
                            };
                        }
                    };
                } else {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": "source_fold_id must be string UUID or null"
                        }),
                    };
                }
            }

            if let Some(v) = row.get("confidence") {
                let confidence = match v.as_f64() {
                    Some(value) => value,
                    None => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": "confidence must be a number"
                            }),
                        };
                    }
                };
                if !(0.0..=1.0).contains(&confidence) {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": "confidence must be between 0 and 1"
                        }),
                    };
                }
                entity.confidence = confidence;
                mutated = true;
            }

            if let Some(v) = row.get("state") {
                let state = match v.as_str() {
                    Some(state) => state,
                    None => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": format!("batch_update_entities[{idx}] state must be a string")
                            }),
                        };
                    }
                };
                let state = match parse_ingest_state(Some(state)) {
                    Ok(state) => state,
                    Err(reason) => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": reason
                            }),
                        };
                    }
                };
                if entity.state != state {
                    entity.state = state;
                    mutated = true;
                }
            }

            if row.contains_key("description") {
                match row.get("description") {
                    Some(value) if value.is_null() => {
                        entity.description = None;
                        mutated = true;
                    }
                    Some(value) => match value.as_str() {
                        Some(value) => {
                            entity.description = Some(value.to_string());
                            mutated = true;
                        }
                        None => {
                            return BatchMutationOutcome {
                                index: idx,
                                kind: BatchMutationKind::Error,
                                result: serde_json::json!({
                                "index": idx,
                                "entity_id": entity_id.to_string(),
                                "status": "error",
                                "reason": "description must be a string or null"
                                }),
                            };
                        }
                    },
                    None => {}
                }
            }

            if row.contains_key("tags") {
                let tags = row.get("tags").unwrap();
                match tags {
                    Value::Array(values) => {
                        let mut parsed_tags = Vec::with_capacity(values.len());
                        for v in values {
                            match v.as_str() {
                                Some(tag) => parsed_tags.push(tag.to_string()),
                                None => {
                                    return BatchMutationOutcome {
                                        index: idx,
                                        kind: BatchMutationKind::Error,
                                        result: serde_json::json!({
                                        "index": idx,
                                        "entity_id": entity_id.to_string(),
                                        "status": "error",
                                        "reason": "tags must be an array of strings"
                                        }),
                                    };
                                }
                            }
                        }
                        entity.tags = parsed_tags;
                        mutated = true;
                    }
                    Value::Null => {
                        entity.tags = Vec::new();
                        mutated = true;
                    }
                    _ => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": "tags must be an array of strings"
                            }),
                        };
                    }
                }
            }

            if row.contains_key("properties") {
                match row.get("properties") {
                    Some(value) if value.is_object() || value.is_null() => {
                        entity.properties = value.clone();
                        mutated = true;
                    }
                    _ => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": "properties must be an object"
                            }),
                        };
                    }
                }
            }

            if row.contains_key("embedding") {
                match row.get("embedding") {
                    Some(value) if value.is_null() => {
                        entity.entity_embedding = None;
                        mutated = true;
                    }
                    Some(value) => match value.as_array() {
                        Some(values) => {
                            let mut embedding = Vec::with_capacity(values.len());
                            let mut invalid = false;
                            for value in values {
                                match value.as_f64() {
                                    Some(v) => embedding.push(v as f32),
                                    None => {
                                        invalid = true;
                                        break;
                                    }
                                }
                            }
                            if invalid {
                                return BatchMutationOutcome {
                                    index: idx,
                                    kind: BatchMutationKind::Error,
                                    result: serde_json::json!({
                                    "index": idx,
                                    "entity_id": entity_id.to_string(),
                                    "status": "error",
                                    "reason": "embedding must be a number array"
                                    }),
                                };
                            }
                            entity.entity_embedding = Some(embedding);
                            mutated = true;
                        }
                        None => {
                            return BatchMutationOutcome {
                                index: idx,
                                kind: BatchMutationKind::Error,
                                result: serde_json::json!({
                                "index": idx,
                                "entity_id": entity_id.to_string(),
                                "status": "error",
                                "reason": "embedding must be an array"
                                }),
                            };
                        }
                    },
                    None => {}
                }
            }

            if !mutated {
                return BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Unchanged,
                    result: serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "unchanged"
                    }),
                };
            }

            entity.updated_at = Some(chrono::Utc::now());
            match storage.entity_put(ctx, &entity).await {
                Ok(_) => {
                    session.dirty.store(true, Ordering::Relaxed);
                    session.last_activity.notify_waiters();
                    BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Updated,
                        result: serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "updated"
                        }),
                    }
                }
                Err(err) => BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Error,
                    result: serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "error",
                    "reason": err.to_string()
                    }),
                },
            }
        });

    let outcomes: Vec<BatchMutationOutcome> = stream::iter(jobs)
        .buffer_unordered(BATCH_MUTATION_CONCURRENCY)
        .collect()
        .await;

    let mut updated: usize = 0;
    let mut unchanged: usize = 0;
    let mut not_found: usize = 0;
    let mut errors: usize = 0;
    for outcome in &outcomes {
        match outcome.kind {
            BatchMutationKind::Updated => updated += 1,
            BatchMutationKind::Unchanged => unchanged += 1,
            BatchMutationKind::NotFound => not_found += 1,
            BatchMutationKind::Error => errors += 1,
            BatchMutationKind::Deleted
            | BatchMutationKind::Missing
            | BatchMutationKind::Invalid
            | BatchMutationKind::Upserted => {}
        }
    }
    let results = ordered_batch_results(outcomes);

    Ok(serde_json::json!({
        "updated": updated,
        "unchanged": unchanged,
        "not_found": not_found,
        "errors": errors,
        "total": entities.len(),
        "results": results,
    }))
}

async fn handle_batch_delete_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let entities = args
        .get("entities")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "entities must be an array".to_string()))?;

    if entities.len() > 100 {
        return Err((
            INVALID_PARAMS,
            format!(
                "entities array length {} exceeds maximum of 100",
                entities.len()
            ),
        ));
    }

    let jobs = entities
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, entity_json)| async move {
            let Some(row) = entity_json.as_object() else {
                return BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Error,
                    result: serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": format!("batch_delete_entities[{idx}] must be an object")
                    }),
                };
            };

            let entity_id = match row
                .get("entity_id")
                .and_then(|v| v.as_str())
                .and_then(|v| uuid::Uuid::parse_str(v).ok())
            {
                Some(id) => id,
                None => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": format!(
                            "batch_delete_entities[{idx}] missing/invalid entity_id"
                        )
                        }),
                    };
                }
            };

            // Remove this entity's edges first — while its :Entity node still
            // exists — so the graph-anchored delete can reach them. Skipping
            // this is what orphaned ~5.5k CO_OCCURS_WITH edges and crashed the
            // viz. Best-effort: a cleanup failure must not block the delete.
            match crate::smart_ingest::delete_typed_edges_referencing_entity_tenant_wide(
                storage, ctx, entity_id,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!(%entity_id, edges = n, "cleaned edges before entity delete"),
                Err(err) => {
                    tracing::warn!(%entity_id, error = %err, "edge cleanup before entity delete failed")
                }
            }

            match storage.entity_delete(ctx, session_id, entity_id).await {
                Ok(true) => {
                    session.dirty.store(true, Ordering::Relaxed);
                    session.last_activity.notify_waiters();
                    BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Deleted,
                        result: serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "deleted"
                        }),
                    }
                }
                Ok(false) => BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::NotFound,
                    result: serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "not_found"
                    }),
                },
                Err(err) => BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Error,
                    result: serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "error",
                    "reason": err.to_string()
                    }),
                },
            }
        });

    let outcomes: Vec<BatchMutationOutcome> = stream::iter(jobs)
        .buffer_unordered(BATCH_MUTATION_CONCURRENCY)
        .collect()
        .await;

    let mut deleted: usize = 0;
    let mut not_found: usize = 0;
    let mut errors: usize = 0;
    for outcome in &outcomes {
        match outcome.kind {
            BatchMutationKind::Deleted => deleted += 1,
            BatchMutationKind::NotFound => not_found += 1,
            BatchMutationKind::Error => errors += 1,
            BatchMutationKind::Updated
            | BatchMutationKind::Unchanged
            | BatchMutationKind::Missing
            | BatchMutationKind::Invalid
            | BatchMutationKind::Upserted => {}
        }
    }
    let results = ordered_batch_results(outcomes);

    Ok(serde_json::json!({
        "deleted": deleted,
        "not_found": not_found,
        "errors": errors,
        "total": entities.len(),
        "results": results,
    }))
}

async fn handle_retrieve_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let query = require_str(&args, "query")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;

    // Auto-generate query embedding for ANN search if Ollama is configured.
    if embedding.is_none()
        && let Some(client) = session_embedding_client(session)
    {
        match client.embed(query).await {
            Ok(emb) => embedding = Some(emb),
            Err(e) => tracing::debug!("query embedding generation skipped: {e}"),
        }
    }

    // Use router for strategy selection when user didn't specify
    let user_strategy = args.get("strategy").and_then(|v| v.as_str());
    let decision = crate::router::route(&crate::router::RoutingContext {
        query_text: query,
        has_entity_name: true, // entity retrieval implies entity context
        has_content_hash: false,
        task_complexity: crate::router::TaskComplexity::Simple,
    });
    let router_strategy = match decision.strategy {
        crate::router::Strategy::Phonetic => "phonetic",
        crate::router::Strategy::HnswAnn => "ann",
        _ => "both",
    };
    let strategy = user_strategy.unwrap_or(router_strategy);
    let k = if args.get("k").is_some() {
        Some(optional_retrieval_limit(&args, &["k"], session)?)
    } else {
        Some(retrieval_default_limit(session))
    };

    let entities = crate::entity::retrieve_entities(
        storage,
        ctx,
        session_id,
        query,
        embedding.as_deref(),
        strategy,
        k,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Track retrieval frequency and check for anomalies (STRIDE T1 / FMEA F19)
    let security_config = crate::config::SecurityConfig::default();
    let mut tracker = session.retrieval_tracker.lock().await;
    let mut co_access = session.co_access.lock().await;
    for entity in &entities {
        tracker.record(entity.entity_id);
        co_access.record(entity.entity_id);
        let count = tracker.count(&entity.entity_id);
        let mean = tracker.mean();
        let stddev = tracker.stddev();
        if crate::audit::check_anomaly(count, mean, stddev, &security_config, None) {
            tracing::warn!(
                entity_id = %entity.entity_id,
                count,
                "anomalous retrieval frequency"
            );
            // Emit anomaly alert via event bus (Sprint 4.9)
            if security_config.anomaly_alerts_enabled {
                session
                    .event_bus
                    .emit(crate::viz::VizEvent::AnomalyDetected {
                        entity_id: entity.entity_id.to_string(),
                        entity_name: entity.entity_name.clone(),
                        retrieval_count: count,
                        session_mean: mean,
                        session_stddev: stddev,
                        sigma_threshold: security_config.anomaly_sigma_threshold,
                    });
            }
        }
    }
    drop(tracker);
    drop(co_access);

    // Fire-and-forget auto outcome for retrieve_entities.
    let auto_query_id = uuid::Uuid::new_v4();
    let _ = crate::feedback::record_outcome(
        storage,
        ctx,
        session_id,
        auto_query_id,
        "retrieve_entities_auto",
        "simple",
        true,
        0,
        0,
    )
    .await;
    for entity in &entities {
        let _ = crate::warmth::apply_outcome_boost(storage, ctx, entity.entity_id, true, 0).await;
    }

    // Warmth boost for retrieved entities (fire-and-forget)
    let rmh_config = crate::config::RmhConfig::default();
    for entity in &entities {
        let _ = crate::warmth::boost_on_access(
            storage,
            ctx,
            entity.entity_id,
            session_id,
            &crate::types::DecayZone::Knowledge,
            &rmh_config,
        )
        .await;
    }

    // Strip embeddings and truncate context to reduce MCP response token cost.
    let slim: Vec<Value> = entities
        .iter()
        .map(|e| {
            let ctx = if e.context_snippet.len() > 200 {
                format!(
                    "{}...",
                    &e.context_snippet[..e.context_snippet.floor_char_boundary(200)]
                )
            } else {
                e.context_snippet.clone()
            };
            serde_json::json!({
                "entity_id": e.entity_id,
                "entity_name": e.entity_name,
                "entity_type": e.entity_type,
                "confidence": e.confidence,
                "state": e.state,
                "created_at": e.created_at,
                "context_snippet": ctx,
            })
        })
        .collect();
    serde_json::to_value(&slim).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

fn parse_entity_list_scope(args: &Value) -> Result<crate::types::EntityListScope, (i32, String)> {
    if let Some(scope) = args.get("scope").and_then(|v| v.as_str()) {
        return match scope {
            "session" | "session_only" => Ok(crate::types::EntityListScope::Session),
            "global" | "global_only" => Ok(crate::types::EntityListScope::Global),
            "both" => Ok(crate::types::EntityListScope::Both),
            "all" => Ok(crate::types::EntityListScope::All),
            other => Err((
                INVALID_PARAMS,
                format!("invalid scope: expected session|global|both|all, got {other}"),
            )),
        };
    }

    match args.get("include_cross_session").and_then(|v| v.as_bool()) {
        Some(true) | None => Ok(crate::types::EntityListScope::All),
        Some(false) => Ok(crate::types::EntityListScope::Session),
    }
}

fn entity_list_response_entry(entity: &crate::types::EntityEntry) -> Value {
    serde_json::json!({
        "entity_id": entity.entity_id,
        "session_id": entity.session_id,
        "entity_name": entity.entity_name,
        "entity_type": entity.entity_type,
        "context_snippet": entity.context_snippet,
        "confidence": entity.confidence,
        "state": entity.state,
        "created_at": entity.created_at,
        "updated_at": entity.updated_at,
        "scope": entity.scope,
        "ingested_by_session": entity.ingested_by_session,
        "tags": entity.tags,
        "properties": entity.properties,
        "content_hash": entity.content_hash,
    })
}

async fn handle_list_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .clamp(1, 500) as usize;
    let filters = match args.get("filters") {
        Some(Value::Object(map)) => map.clone(),
        Some(Value::Null) | None => serde_json::Map::new(),
        Some(_) => {
            return Err((
                INVALID_PARAMS,
                "filters must be an object of equality predicates".into(),
            ));
        }
    };
    let query = crate::types::EntityListQuery {
        session_id,
        entity_type: args
            .get("entity_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        filters,
        scope: parse_entity_list_scope(&args)?,
        limit,
    };

    let scope = query.scope;
    let entities = storage
        .entity_list_matching(ctx, query)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let response_entities: Vec<Value> = entities.iter().map(entity_list_response_entry).collect();
    Ok(serde_json::json!({
        "entities": response_entities,
        "count": response_entities.len(),
        "scope": scope,
    }))
}

// --- Memory state handlers ---

async fn handle_promote_memory<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let entity_id = require_uuid(&args, "entity_id")?;

    let new_state = crate::entity::promote_memory(storage, ctx, session_id, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    session.event_bus.emit(crate::viz::VizEvent::StateChanged {
        entity_id: entity_id.to_string(),
        new_state: new_state.to_string(),
    });

    Ok(serde_json::json!({ "new_state": new_state.to_string() }))
}

async fn handle_demote_memory<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let entity_id = require_uuid(&args, "entity_id")?;

    let new_state = crate::entity::demote_memory(storage, ctx, session_id, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    session.event_bus.emit(crate::viz::VizEvent::StateChanged {
        entity_id: entity_id.to_string(),
        new_state: new_state.to_string(),
    });

    Ok(serde_json::json!({ "new_state": new_state.to_string() }))
}

// --- Importance scoring handler ---

async fn handle_importance_score<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let entity_id = require_uuid(&args, "entity_id")?;

    // Look up the entity to get created_at for recency
    let entities = storage
        .entity_list_session(ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let entity = entities
        .iter()
        .find(|e| e.entity_id == entity_id)
        .ok_or_else(|| (INVALID_PARAMS, format!("entity not found: {entity_id}")))?;

    let last_accessed_seconds_ago = (chrono::Utc::now() - entity.created_at).num_seconds();

    // Use retrieval tracker for attention signal
    let tracker = session.retrieval_tracker.lock().await;
    let retrieval_count = tracker.count(&entity_id);
    drop(tracker);

    // Defaults for channels we cannot yet compute without embedding similarity
    let similarity_to_existing = 0.0;
    let feedback_success_rate = 0.0;

    let score = crate::importance::compute_importance(
        similarity_to_existing,
        retrieval_count,
        last_accessed_seconds_ago,
        feedback_success_rate,
    );

    serde_json::to_value(&score).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Feedback handler ---

fn workspace_feedback_key(cwd: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(cwd.trim().as_bytes());
    format!("cwd:{}", hex::encode(&digest[..8]))
}

struct WorkspaceFeedbackUpdate<'a> {
    session_id: uuid::Uuid,
    entity_id: uuid::Uuid,
    cwd: &'a str,
    source: &'a str,
    judge_source: &'a str,
    score_delta: Option<f64>,
    judgment: Option<i64>,
}

async fn update_workspace_feedback<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    update: WorkspaceFeedbackUpdate<'_>,
) -> anyhow::Result<bool> {
    if update.cwd.trim().is_empty() {
        return Ok(false);
    }
    let Some(mut entity) = storage
        .entity_get_by_id(ctx, update.session_id, update.entity_id)
        .await?
    else {
        return Ok(false);
    };
    let mut properties = entity.properties.clone();
    if !properties.is_object() {
        properties = serde_json::json!({});
    }
    let root = properties.as_object_mut().expect("object set above");
    let feedback = root
        .entry("workspace_feedback")
        .or_insert_with(|| serde_json::json!({}));
    if !feedback.is_object() {
        *feedback = serde_json::json!({});
    }
    let feedback_obj = feedback.as_object_mut().expect("object set above");
    let key = workspace_feedback_key(update.cwd);
    let entry = feedback_obj.entry(key).or_insert_with(|| {
        serde_json::json!({
            "cwd": update.cwd,
            "score": 0.0,
            "positives": 0,
            "negatives": 0,
            "neutrals": 0,
            "abstentions": 0,
            "mechanisms": {}
        })
    });
    if !entry.is_object() {
        *entry = serde_json::json!({
            "cwd": update.cwd,
            "score": 0.0,
            "positives": 0,
            "negatives": 0,
            "neutrals": 0,
            "abstentions": 0,
            "mechanisms": {}
        });
    }
    let entry_obj = entry.as_object_mut().expect("object set above");
    entry_obj.insert("cwd".into(), Value::String(update.cwd.to_string()));
    if let Some(score_delta) = update.score_delta {
        let current_score = entry_obj
            .get("score")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        entry_obj.insert(
            "score".into(),
            serde_json::json!(current_score + score_delta),
        );
    }
    let count_key = match update.judgment {
        Some(score) if score > 0 => "positives",
        Some(score) if score < 0 => "negatives",
        Some(_) => "neutrals",
        None => "abstentions",
    };
    let count = entry_obj
        .get(count_key)
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    entry_obj.insert(count_key.into(), serde_json::json!(count + 1));
    let mechanisms = entry_obj
        .entry("mechanisms")
        .or_insert_with(|| serde_json::json!({}));
    if !mechanisms.is_object() {
        *mechanisms = serde_json::json!({});
    }
    let mechanisms_obj = mechanisms.as_object_mut().expect("object set above");
    let mechanism = mechanisms_obj
        .entry(update.source.to_string())
        .or_insert_with(|| {
            serde_json::json!({
                "score": 0.0,
                "positives": 0,
                "negatives": 0,
                "neutrals": 0,
                "abstentions": 0,
                "judges": {}
            })
        });
    if !mechanism.is_object() {
        *mechanism = serde_json::json!({
            "score": 0.0,
            "positives": 0,
            "negatives": 0,
            "neutrals": 0,
            "abstentions": 0,
            "judges": {}
        });
    }
    let mechanism_obj = mechanism.as_object_mut().expect("object set above");
    if let Some(score_delta) = update.score_delta {
        let mechanism_score = mechanism_obj
            .get("score")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        mechanism_obj.insert(
            "score".into(),
            serde_json::json!(mechanism_score + score_delta),
        );
    }
    let count = mechanism_obj
        .get(count_key)
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    mechanism_obj.insert(count_key.into(), serde_json::json!(count + 1));

    let judges = mechanism_obj
        .entry("judges")
        .or_insert_with(|| serde_json::json!({}));
    if !judges.is_object() {
        *judges = serde_json::json!({});
    }
    let judges_obj = judges.as_object_mut().expect("object set above");
    let judge = judges_obj
        .entry(update.judge_source.to_string())
        .or_insert_with(|| {
            serde_json::json!({
                "score": 0.0,
                "positives": 0,
                "negatives": 0,
                "neutrals": 0,
                "abstentions": 0
            })
        });
    if !judge.is_object() {
        *judge = serde_json::json!({
            "score": 0.0,
            "positives": 0,
            "negatives": 0,
            "neutrals": 0,
            "abstentions": 0
        });
    }
    let judge_obj = judge.as_object_mut().expect("object set above");
    if let Some(score_delta) = update.score_delta {
        let judge_score = judge_obj
            .get("score")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        judge_obj.insert("score".into(), serde_json::json!(judge_score + score_delta));
    }
    let count = judge_obj
        .get(count_key)
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    judge_obj.insert(count_key.into(), serde_json::json!(count + 1));

    entity.properties = properties;
    entity.updated_at = Some(chrono::Utc::now());
    storage.entity_put(ctx, &entity).await?;
    Ok(true)
}

async fn handle_record_outcome<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let query_id = require_uuid(&args, "query_id")?;
    let program_type = require_str(&args, "program_type")?;
    let task_complexity = require_str(&args, "task_complexity")?;
    let succeeded = args
        .get("succeeded")
        .and_then(|v| v.as_bool())
        .ok_or((INVALID_PARAMS, "missing required bool: succeeded".into()))?;
    let latency_ms = require_i32(&args, "latency_ms")?;
    let token_cost = require_i32(&args, "token_cost")?;
    let cwd = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
    let retrieval_sources: Vec<String> = args
        .get("retrieval_sources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec![program_type.to_string()]);

    let recorded = crate::feedback::record_outcome(
        storage,
        ctx,
        session_id,
        query_id,
        program_type,
        task_complexity,
        succeeded,
        latency_ms,
        token_cost,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Warmth modulation: success → boost, failure → penalty.
    // This closes the episodic feedback loop — recorded outcomes change
    // future retrieval ranking via warmth scores.
    let mut entity_ids_updated = Vec::new();
    let mut invalid_entity_ids = Vec::new();
    let mut workspace_feedback_updated = 0usize;
    if let Some(entity_ids) = args.get("entity_ids").and_then(|v| v.as_array()) {
        let mut deltas = std::collections::HashMap::new();
        for id_val in entity_ids {
            match id_val.as_str().map(|s| s.parse::<uuid::Uuid>()) {
                Some(Ok(eid)) => {
                    if let Err(e) =
                        crate::warmth::apply_outcome_boost(storage, ctx, eid, succeeded, latency_ms)
                            .await
                    {
                        tracing::warn!(entity_id = %eid, error = %e, "warmth boost failed");
                    }
                    // Also accumulate reputation delta for batch reputation update.
                    deltas.insert(eid, if succeeded { 0.05 } else { -0.10 });
                    if !cwd.is_empty() {
                        for source in &retrieval_sources {
                            match update_workspace_feedback(
                                storage,
                                ctx,
                                WorkspaceFeedbackUpdate {
                                    session_id,
                                    entity_id: eid,
                                    cwd,
                                    source,
                                    judge_source: "outcome",
                                    score_delta: Some(if succeeded { 0.10 } else { -0.20 }),
                                    judgment: Some(if succeeded { 1 } else { -1 }),
                                },
                            )
                            .await
                            {
                                Ok(true) => workspace_feedback_updated += 1,
                                Ok(false) => {}
                                Err(e) => tracing::warn!(
                                    entity_id = %eid,
                                    error = %e,
                                    "workspace feedback update failed"
                                ),
                            }
                        }
                    }
                    entity_ids_updated.push(eid.to_string());
                }
                Some(Err(_)) => invalid_entity_ids.push(id_val.as_str().unwrap_or("").to_string()),
                None => invalid_entity_ids.push(id_val.to_string()),
            }
        }
        if !deltas.is_empty()
            && let Err(e) =
                crate::pagerank::update_reputation_scores(storage, ctx, session_id, &deltas).await
        {
            tracing::warn!("failed to update entity reputation from outcome: {e}");
        }
    } else if program_type == "retrieval_miss" {
        // Back-compat: the old retrieval_miss path also penalized reputation.
        // If caller didn't supply entity_ids, nothing to penalize directly.
        tracing::debug!("retrieval_miss without entity_ids — no reputation penalty applied");
    }

    let mut response = serde_json::json!({
        "recorded": recorded,
        "warmth_updated": args.get("entity_ids").is_some(),
        "workspace_feedback_updated": workspace_feedback_updated,
        "entity_ids_updated": entity_ids_updated,
        "invalid_entity_ids": invalid_entity_ids
    });
    if program_type == "retrieval_miss" {
        response["_hint"] = serde_json::json!(
            "Retrieval miss logged. The system will learn to store this kind of information. Consider using ingest now to store what you found via grep/read."
        );
    } else {
        response["_hint"] = serde_json::json!(
            "Outcome recorded. This feedback improves retrieval routing over time."
        );
    }
    Ok(response)
}

async fn handle_record_feedback<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let last = {
        let guard = session.last_retrieval.lock().await;
        guard.get(&session_id).cloned()
    }
    .ok_or((
        INVALID_PARAMS,
        "no previous hybrid_search results for this session".into(),
    ))?;

    let requested_subset: Option<std::collections::HashSet<uuid::Uuid>> = args
        .get("entity_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|value| value.as_str()?.parse::<uuid::Uuid>().ok())
                .collect()
        });
    let scores: Vec<Option<i64>> = args
        .get("scores")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_judge_score_value).collect())
        .unwrap_or_default();
    let fallback_score = args
        .get("relevant")
        .and_then(|v| v.as_bool())
        .map(|relevant| if relevant { 1 } else { -1 });
    if scores.is_empty() && fallback_score.is_none() {
        return Err((
            INVALID_PARAMS,
            "pass scores or relevant to record last retrieval feedback".into(),
        ));
    }
    let cwd = args
        .get("cwd")
        .and_then(|v| v.as_str())
        .or(last.cwd.as_deref())
        .unwrap_or("");
    let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    let judge_source = args
        .get("judge")
        .or_else(|| args.get("feedback_source"))
        .or_else(|| args.get("source"))
        .and_then(|v| v.as_str())
        .unwrap_or("caller_llm");

    let mut updated = Vec::new();
    let mut skipped = 0usize;
    let mut abstained = 0usize;
    for (idx, result) in last.results.iter().enumerate() {
        if requested_subset
            .as_ref()
            .is_some_and(|subset| !subset.contains(&result.entity_id))
        {
            skipped += 1;
            continue;
        }
        let score = scores.get(idx).copied().flatten().or(fallback_score);
        let Some(score) = score else {
            abstained += 1;
            if !cwd.is_empty()
                && let Err(e) = update_workspace_feedback(
                    storage,
                    ctx,
                    WorkspaceFeedbackUpdate {
                        session_id,
                        entity_id: result.entity_id,
                        cwd,
                        source: &result.source,
                        judge_source,
                        score_delta: None,
                        judgment: None,
                    },
                )
                .await
            {
                tracing::warn!(entity_id = %result.entity_id, error = %e, "workspace feedback abstention update failed");
            }
            updated.push(serde_json::json!({
                "entity_id": result.entity_id,
                "source": result.source,
                "score": "-"
            }));
            continue;
        };
        let score = score.clamp(-1, 1);
        if score == 0 {
            if !cwd.is_empty()
                && let Err(e) = update_workspace_feedback(
                    storage,
                    ctx,
                    WorkspaceFeedbackUpdate {
                        session_id,
                        entity_id: result.entity_id,
                        cwd,
                        source: &result.source,
                        judge_source,
                        score_delta: Some(0.0),
                        judgment: Some(0),
                    },
                )
                .await
            {
                tracing::warn!(entity_id = %result.entity_id, error = %e, "workspace neutral feedback update failed");
            }
            updated.push(serde_json::json!({
                "entity_id": result.entity_id,
                "source": result.source,
                "score": 0
            }));
            continue;
        }
        let succeeded = score > 0;
        if let Err(e) =
            crate::warmth::apply_outcome_boost(storage, ctx, result.entity_id, succeeded, 1).await
        {
            tracing::warn!(entity_id = %result.entity_id, error = %e, "warmth feedback update failed");
        }
        let mut deltas = std::collections::HashMap::new();
        deltas.insert(result.entity_id, if succeeded { 0.05 } else { -0.10 });
        if let Err(e) =
            crate::pagerank::update_reputation_scores(storage, ctx, session_id, &deltas).await
        {
            tracing::warn!(entity_id = %result.entity_id, error = %e, "reputation feedback update failed");
        }
        if !cwd.is_empty()
            && let Err(e) = update_workspace_feedback(
                storage,
                ctx,
                WorkspaceFeedbackUpdate {
                    session_id,
                    entity_id: result.entity_id,
                    cwd,
                    source: &result.source,
                    judge_source,
                    score_delta: Some(if succeeded { 0.10 } else { -0.20 }),
                    judgment: Some(score),
                },
            )
            .await
        {
            tracing::warn!(entity_id = %result.entity_id, error = %e, "workspace feedback update failed");
        }
        updated.push(serde_json::json!({
            "entity_id": result.entity_id,
            "source": result.source,
            "score": score
        }));
    }

    let _ = crate::feedback::record_outcome(
        storage,
        ctx,
        session_id,
        last.query_id,
        "hybrid_search",
        if reason.is_empty() {
            "simple"
        } else {
            "linear"
        },
        updated
            .iter()
            .any(|entry| entry["score"].as_i64().unwrap_or(0) > 0),
        1,
        0,
    )
    .await;

    Ok(serde_json::json!({
        "recorded": true,
        "query_id": last.query_id,
        "query": last.query,
        "cwd": cwd,
        "judge": judge_source,
        "updated": updated,
        "skipped": skipped,
        "abstained": abstained,
        "hint": "Feedback recorded. Future hybrid_search calls with the same cwd will sum valid judge scores and track '-' abstentions separately."
    }))
}

fn nested_or_flat_str(args: &Value, field: &str) -> Option<String> {
    args.get("session_start")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(field))
        .or_else(|| args.get(field))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn configure_requests_session_start(args: &Value) -> bool {
    matches!(args.get("session_start"), Some(Value::Bool(true)))
        || args
            .get("session_start")
            .and_then(|v| v.as_object())
            .is_some()
        || args.get("session_id").is_some()
        || nested_or_flat_str(args, "agent_session_id").is_some()
        || nested_or_flat_str(args, "external_session_id").is_some()
        || nested_or_flat_str(args, "thread_id").is_some()
}

fn configured_runtime_session_id(
    args: &Value,
) -> Result<Option<(uuid::Uuid, String)>, (i32, String)> {
    if !configure_requests_session_start(args) {
        return Ok(None);
    }

    if let Some(raw) = nested_or_flat_str(args, "session_id") {
        let session_id = uuid::Uuid::parse_str(&raw).map_err(|e| {
            (
                INVALID_PARAMS,
                format!("session_id is not a valid UUID: {e}"),
            )
        })?;
        return Ok(Some((session_id, "explicit_session_id".to_string())));
    }

    let agent = nested_or_flat_str(args, "agent").unwrap_or_else(|| "unknown-agent".to_string());
    let workspace = nested_or_flat_str(args, "workspace")
        .or_else(|| nested_or_flat_str(args, "cwd"))
        .unwrap_or_else(|| "unknown-workspace".to_string());
    let external = nested_or_flat_str(args, "agent_session_id")
        .or_else(|| nested_or_flat_str(args, "external_session_id"))
        .or_else(|| nested_or_flat_str(args, "thread_id"));

    let Some(external) = external else {
        return Ok(Some((
            uuid::Uuid::new_v4(),
            "generated_session_start".to_string(),
        )));
    };

    let key = format!("ferrosa-memory:agent-session:v1:{agent}:{workspace}:{external}");
    Ok(Some((
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, key.as_bytes()),
        "derived_from_agent_session".to_string(),
    )))
}

async fn handle_configure(args: Value, session: &SessionState) -> Result<Value, (i32, String)> {
    let requested = args
        .get("retrieval_limit")
        .or_else(|| args.get("default_limit"))
        .and_then(|v| v.as_u64());
    let mut updated = false;
    if let Some(raw) = requested {
        let value = raw as usize;
        if !(MIN_RETRIEVAL_LIMIT..=MAX_RETRIEVAL_LIMIT).contains(&value) {
            return Err((
                INVALID_PARAMS,
                format!(
                    "retrieval_limit must be between {MIN_RETRIEVAL_LIMIT} and {MAX_RETRIEVAL_LIMIT}"
                ),
            ));
        }
        session
            .retrieval_default_limit
            .store(value, Ordering::Relaxed);
        updated = true;
    }

    let session_update = configured_runtime_session_id(&args)?;
    let session_source = session_update.as_ref().map(|(_, source)| source.clone());
    if let Some((session_id, _)) = session_update {
        session.set_runtime_session_id(session_id)?;
        updated = true;
    }
    let effective_session_id = session.effective_default_session_id();

    Ok(serde_json::json!({
        "updated": updated,
        "retrieval_limit": retrieval_default_limit(session),
        "min_retrieval_limit": MIN_RETRIEVAL_LIMIT,
        "max_retrieval_limit": MAX_RETRIEVAL_LIMIT,
        "session_id": effective_session_id.map(|id| id.to_string()),
        "session_source": session_source,
        "hint": "SessionStart hooks should call configure with session_start metadata once; fmem stores the active session_id. Individual retrieval calls can still override limit/k."
    }))
}

// --- Session lifecycle handler ---

async fn handle_delete_session<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let result = crate::session::delete_session(storage, ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Cognitive memory handler ---

async fn handle_smart_ingest<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let content = require_str(&args, "content")?;
    let entity_type = require_str(&args, "entity_type")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let source_fold_id = optional_uuid(&args, "source_fold_id")?;

    // Auto-generate embedding if not provided and Ollama is configured.
    if embedding.is_none()
        && let Some(client) = session_embedding_client(session)
    {
        match client.embed(content).await {
            Ok(emb) => embedding = Some(emb),
            Err(e) => tracing::debug!("embedding generation skipped: {e}"),
        }
    }

    let entity_name = args
        .get("entity_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let ner_config = crate::smart_ingest::NerConfig {
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default(),
        ollama_base_url: session.ollama_base_url.clone(),
        model: session.ner_model.clone(),
    };

    let config = crate::smart_ingest::IngestConfig::default();
    let decision = crate::smart_ingest::smart_ingest(
        storage,
        ctx,
        session_id,
        content,
        entity_type,
        embedding.as_deref(),
        source_fold_id,
        &config,
        entity_name.as_deref(),
        Some(&ner_config),
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Emit viz event based on ingest decision
    let decision_json =
        serde_json::to_value(&decision).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let action = decision_json
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let entity_id = decision_json
        .get("entity_id")
        .or_else(|| decision_json.get("new_entity_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if !entity_id.is_empty() {
        session.event_bus.emit(crate::viz::VizEvent::EntityChanged {
            node: crate::viz::VizNode {
                id: entity_id.clone(),
                label: content.chars().take(64).collect(),
                node_type: "entity".into(),
                entity_type: entity_type.to_string(),
                state: "active".into(),
                confidence: 1.0,
                created_at: chrono::Utc::now().to_rfc3339(),
                context: content.chars().take(256).collect(),
                child_count: None,
                ..Default::default()
            },
            action: action.clone(),
        });
    }

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "ingest",
        "entity_store",
        &entity_id,
        session_id,
    )
    .await;

    // Penalize reputation of superseded entity — it was wrong/outdated
    if action == "Superseded"
        && let Some(old_id_str) = decision_json.get("old_entity_id").and_then(|v| v.as_str())
        && let Ok(old_id) = old_id_str.parse::<uuid::Uuid>()
    {
        let mut deltas = std::collections::HashMap::new();
        deltas.insert(old_id, -0.2);
        if let Err(e) =
            crate::pagerank::update_reputation_scores(storage, ctx, session_id, &deltas).await
        {
            tracing::warn!("failed to penalize superseded entity reputation: {e}");
        }
    }

    let auto_consolidation_queued = if action == "Created" {
        match mark_smart_ingest_created_for_consolidation(session, session_id).await {
            Ok(queued) => queued,
            Err((_, msg)) => {
                tracing::warn!(error = %msg, "smart_ingest auto-consolidation queue failed");
                false
            }
        }
    } else {
        false
    };

    // Add rotating hint to encourage continued memory formation
    let mut result = decision_json;
    if let Some(obj) = result.as_object_mut() {
        let hint = match action.as_str() {
            "Skipped" => "Content too similar to existing memory. Try a different aspect or more specific insight.".to_string(),
            _ => pick_hint(INGEST_HINTS),
        };
        obj.insert("hint".into(), Value::String(hint));

        // Progressive disclosure hints
        match action.as_str() {
            "Created" => {
                obj.insert("_hint".into(), serde_json::json!(
                    "Entity created. Use create_edge for known relationships; the server automatically queues consolidation after enough new entities."
                ));
                obj.insert(
                    "auto_consolidation_queued".into(),
                    Value::Bool(auto_consolidation_queued),
                );
            }
            "Superseded" => {
                obj.insert("_hint".into(), serde_json::json!(
                    "Previous fact superseded. Use get_temporal_chain to see the full fact evolution for this entity."
                ));
            }
            _ => {}
        }
    }
    Ok(result)
}

// --- Skills handlers ---

async fn handle_ingest_skill<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    // Reject unknown top-level keys. Fail-loud per the project rule: silent
    // field drops hide schema drift for weeks. Every key a caller passes must
    // be one the server recognizes, or the caller needs to learn about the
    // mismatch the first time they make it.
    const KNOWN_KEYS: &[&str] = &[
        "name",
        "category",
        "description",
        "session_id",
        "trigger_keywords",
        "tags",
        "prerequisites",
        "steps",
        "output_artifacts",
        "completion_criteria",
        "content_hash",
    ];
    if let Some(obj) = args.as_object() {
        let unknown: Vec<&str> = obj
            .keys()
            .map(|s| s.as_str())
            .filter(|k| !KNOWN_KEYS.contains(k))
            .collect();
        if !unknown.is_empty() {
            return Err((
                -32602,
                format!(
                    "unknown field(s) on ingest_skill: {}. Known: {}",
                    unknown.join(", "),
                    KNOWN_KEYS.join(", ")
                ),
            ));
        }
    }

    let name = require_str(&args, "name")?.to_string();
    let category = require_str(&args, "category")?.to_string();
    let description = require_str(&args, "description")?.to_string();
    let caller_session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let trigger_keywords = args
        .get("trigger_keywords")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let tags = args
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let prerequisites = args
        .get("prerequisites")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // Propagate serde errors — never default-on-failure. A malformed step
    // array with keys like {title, body} instead of {phase, instruction}
    // must surface as an error, not a silently empty `steps: []`.
    let steps: Vec<crate::skill::Step> = match args.get("steps").cloned() {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| (-32602, format!("invalid `steps` payload: {e}")))?,
        None => Vec::new(),
    };
    let output_artifacts = args
        .get("output_artifacts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let completion_criteria = args
        .get("completion_criteria")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let content_hash = args
        .get("content_hash")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let params = crate::skill::IngestSkillParams {
        name,
        category,
        description,
        trigger_keywords,
        tags,
        prerequisites,
        steps,
        output_artifacts,
        completion_criteria,
        content_hash,
        caller_session_id,
    };

    // Build an embedding client from session config so description_embedding
    // is populated alongside the skill entity.
    let embed_client = session_embedding_client(session);

    let action = crate::skill::ingest_skill(
        storage,
        ctx,
        params,
        embed_client.as_ref(),
        session.graph.as_deref(),
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let mut result = serde_json::to_value(&action).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "_hint".into(),
            Value::String(
                "Skills are global knowledge shared across every session. If you \
                 use this skill and learn something new — a better step, a missing \
                 prerequisite, a clearer description — call ingest_skill again with \
                 the refinement. Your changes persist."
                    .into(),
            ),
        );
    }
    Ok(result)
}

async fn handle_retrieve_skills_for_context<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let context = require_str(&args, "context")?.to_string();
    let caller_session = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(5)
        .clamp(1, 20);
    let min_score = args
        .get("min_score")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let mut query_embedding = args.get("embedding").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|n| n.as_f64().map(|f| f as f32))
            .collect::<Vec<f32>>()
    });
    if query_embedding.is_none()
        && let Some(client) = session_embedding_client(session)
        && let Ok(emb) = client.embed(&context).await
    {
        query_embedding = Some(emb);
    }

    // Used-in-session signal — retrieval_tracker records which entity_ids
    // have been touched in this session.
    let used_ids: std::collections::HashSet<uuid::Uuid> = {
        let tracker = session.retrieval_tracker.lock().await;
        tracker.recent_ids(50).into_iter().collect()
    };

    let hits = crate::skill::retrieve_skills_for_context(
        storage,
        ctx,
        caller_session,
        &context,
        query_embedding.as_deref(),
        limit,
        min_score,
        &used_ids,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let hint = if hits.is_empty() {
        "No skills matched. Ingest one with ingest_skill, or broaden the context."
    } else {
        "These skills are shared across all sessions. If you successfully apply one, \
         remember it. If you discover a refinement, call ingest_skill to persist it."
    };

    Ok(serde_json::json!({
        "results": hits,
        "_hint": hint,
    }))
}

async fn handle_invoke_skill<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    _session: &SessionState,
) -> Result<Value, (i32, String)> {
    let name = require_str(&args, "skill_name")?.to_string();

    let entity = crate::skill::get_skill_by_name(storage, ctx, &name)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let entity = match entity {
        Some(e) => e,
        None => {
            let similar = crate::skill::similar_skill_names(storage, ctx, &name, 3).await;
            let payload = serde_json::json!({
                "error": format!("skill not found: '{}'", name),
                "did_you_mean": similar,
                "hint": "Call retrieve_skills_for_context to discover available skills, \
                        or ingest_skill to add this one.",
            });
            return Err((INVALID_PARAMS, payload.to_string()));
        }
    };

    let result = crate::skill::build_invoke_result(&entity);
    serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_ensure_parent_tag<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let child_tag = require_str(&args, "child_tag")?.to_string();
    let parent_tag = require_str(&args, "parent_tag")?.to_string();
    let caller_session = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let action = crate::skill::ensure_parent_tag(
        storage,
        ctx,
        caller_session,
        &child_tag,
        &parent_tag,
        session.graph.as_deref(),
    )
    .await
    .map_err(|e| (INVALID_PARAMS, e.to_string()))?;

    serde_json::to_value(&action).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_verify_skill<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    _session: &SessionState,
) -> Result<Value, (i32, String)> {
    let name = require_str(&args, "skill_name")?.to_string();
    let result = crate::skill::verify_skill(storage, ctx, &name)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Intention handlers ---

/// Resolve repo from tool args, falling back to session default.
fn resolve_repo<'a>(args: &'a Value, session: &'a SessionState) -> &'a str {
    args.get("repo")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| session.repo.get().map(|s| s.as_str()).unwrap_or(""))
}

async fn handle_set_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let repo = resolve_repo(&args, session);
    if repo.is_empty() {
        return Err((
            INVALID_PARAMS,
            "repo is required (pass explicitly or configure server repo)".into(),
        ));
    }
    let description = require_str(&args, "description")?;
    let trigger_json = args
        .get("trigger")
        .ok_or((INVALID_PARAMS, "missing required object: trigger".into()))?;
    let trigger: crate::intention::IntentionTrigger = serde_json::from_value(trigger_json.clone())
        .map_err(|e| (INVALID_PARAMS, format!("invalid trigger: {e}")))?;
    let priority: crate::intention::Priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .map(|s| {
            serde_json::from_value(Value::String(s.to_string()))
                .unwrap_or(crate::intention::Priority::Normal)
        })
        .unwrap_or(crate::intention::Priority::Normal);

    let mut store = session.intentions.lock().await;
    let intention = store.set(repo, description, trigger, priority);
    let id = intention.id;

    // Persist to storage (best-effort -- in-memory is primary)
    if let Err(e) = storage.intention_put(ctx, &intention).await {
        tracing::warn!(error = %e, "failed to persist intention to storage");
    }

    Ok(serde_json::json!({ "intention_id": id.to_string() }))
}

async fn handle_set_foresight<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let content = require_str(&args, "content")?;
    let session_id = optional_uuid(&args, "session_id")?
        .or_else(|| session.effective_default_session_id())
        .unwrap_or_else(uuid::Uuid::nil);
    let parse_ts = |key: &str| -> Result<Option<chrono::DateTime<chrono::Utc>>, (i32, String)> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .map_err(|e| (INVALID_PARAMS, format!("invalid {key} (want RFC3339): {e}")))
            })
            .transpose()
    };
    let valid_from = parse_ts("valid_from")?;
    let valid_until = parse_ts("valid_until")?;
    if let (Some(from), Some(until)) = (valid_from, valid_until)
        && from > until
    {
        return Err((INVALID_PARAMS, "valid_from must be <= valid_until".into()));
    }
    let fact = crate::types::ForesightFact {
        tenant_id: ctx.tenant_id,
        session_id,
        fact_id: uuid::Uuid::new_v4(),
        content: content.to_string(),
        valid_from,
        valid_until,
        created_at: chrono::Utc::now(),
    };
    storage.foresight_put(ctx, &fact).await.map_err(|e| {
        (
            INTERNAL_ERROR,
            format!("failed to store foresight fact: {e}"),
        )
    })?;
    Ok(serde_json::json!({ "fact_id": fact.fact_id.to_string() }))
}

async fn handle_check_intentions<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let repo = resolve_repo(&args, session);
    let context = require_str(&args, "context")?;
    let mut store = session.intentions.lock().await;
    let triggered = store.check(context, repo);
    let triggered_json: Vec<Value> = triggered
        .iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();

    // Persist status changes for triggered intentions
    for intention in &triggered {
        let status_str = serde_json::to_string(&intention.status)
            .unwrap_or_else(|_| "\"triggered\"".into())
            .trim_matches('"')
            .to_string();
        if let Err(e) = storage
            .intention_update_status(
                ctx,
                repo,
                intention.id,
                &status_str,
                intention.triggered_at,
                None,
            )
            .await
        {
            tracing::warn!(id = %intention.id, error = %e, "failed to persist intention trigger");
        }
    }

    let triggered_count = triggered_json.len();
    let mut response = serde_json::json!({ "triggered": triggered_json });
    if triggered_count > 0 {
        response["_hint"] = serde_json::json!(format!(
            "{triggered_count} intentions triggered. Act on them, then call complete_intention for each."
        ));
    }
    Ok(response)
}

async fn handle_complete_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let repo = resolve_repo(&args, session);
    let id = require_uuid(&args, "intention_id")?;
    let mut store = session.intentions.lock().await;
    let completed = store.complete(id);

    if completed
        && let Err(e) = storage
            .intention_update_status(ctx, repo, id, "completed", None, Some(chrono::Utc::now()))
            .await
    {
        tracing::warn!(%id, error = %e, "failed to persist intention completion");
    }

    Ok(serde_json::json!({ "completed": completed }))
}

async fn handle_list_intentions<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let all_repos = args
        .get("all_repos")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if all_repos {
        // Load from CQL across all repos
        let intentions = storage
            .intention_list_all(ctx)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
        let json: Vec<Value> = intentions
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        Ok(serde_json::json!({ "intentions": json, "source": "all_repos" }))
    } else {
        // In-memory store (current repo only)
        let store = session.intentions.lock().await;
        let intentions = store.list();
        let json: Vec<Value> = intentions
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        Ok(serde_json::json!({ "intentions": json, "source": "session" }))
    }
}

async fn handle_snooze_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let repo = resolve_repo(&args, session);
    let id = require_uuid(&args, "intention_id")?;
    let mut store = session.intentions.lock().await;
    let snoozed = store.snooze(id);

    if snoozed
        && let Err(e) = storage
            .intention_update_status(ctx, repo, id, "pending", None, None)
            .await
    {
        tracing::warn!(%id, error = %e, "failed to persist intention snooze");
    }

    Ok(serde_json::json!({ "snoozed": snoozed }))
}

// --- Temporal fact handlers ---

async fn handle_write_temporal_fact<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let entity_id = require_uuid(&args, "entity_id")?;
    let fact_text = require_str(&args, "fact_text")?;
    let confidence = args
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);

    let event_id = crate::temporal::write_temporal_fact(
        storage, ctx, entity_id, fact_text, session_id, confidence,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Emit viz event for temporal fact
    session.event_bus.emit(crate::viz::VizEvent::FactUpdated {
        entity_id: entity_id.to_string(),
        fact_text: fact_text.to_string(),
        superseded: None,
    });

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "write",
        "temporal_events",
        &event_id.to_string(),
        session_id,
    )
    .await;

    Ok(serde_json::json!({ "event_id": event_id.to_string() }))
}

async fn handle_get_temporal_chain<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let _session_id = optional_uuid(&args, "session_id")?;
    let entity_id = require_uuid(&args, "entity_id")?;

    let fact = crate::temporal::get_current_fact(storage, ctx, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    match fact {
        Some(event) => serde_json::to_value(&event).map_err(|e| (INTERNAL_ERROR, e.to_string())),
        None => Ok(serde_json::json!({ "fact": null })),
    }
}

// --- Graph traversal handler ---

async fn handle_explore_connections<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let traversal = require_str(&args, "traversal")?;
    let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
    let limit = optional_retrieval_limit(&args, &["limit"], session)?;

    let results = match traversal {
        "fold_ancestors" => {
            let graph = session
                .graph
                .as_ref()
                .ok_or((INTERNAL_ERROR, "graph client not configured".into()))?;
            let fold_id = require_uuid(&args, "fold_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            graph
                .get_fold_ancestors(fold_id, session_id, max_depth)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        }
        "related_entities" => {
            let entity_id = require_uuid(&args, "entity_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            // Try graph backend first, fall back to CQL typed_edges if empty.
            // A graph error is a DESIGNED fallback (the graph client may be
            // disabled, unreachable, or temporarily degraded) — but it must
            // be logged so operators can see silent fall-through.
            let graph_results = if let Some(graph) = session.graph.as_ref() {
                match graph
                    .find_related_entities(ctx.tenant_id, entity_id, session_id, max_depth)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            %entity_id,
                            %session_id,
                            error = %e,
                            "related_entities: graph backend failed; falling back to CQL typed_edges"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            if !graph_results.is_empty() {
                let mut r = graph_results;
                r.truncate(limit);
                r
            } else {
                // CQL fallback: query legacy edges plus the canonical typed_edges
                // table used by create_edge / batch_create_edges.
                let mut edges = storage
                    .edge_list_for_entity(ctx, entity_id)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
                let typed_edges = storage
                    .typed_edge_list_from(ctx, session_id, entity_id)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
                edges.extend(
                    typed_edges
                        .into_iter()
                        .map(|edge| (edge.dst_id, edge.edge_type)),
                );
                edges.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                edges.dedup();
                let mut results: Vec<String> = Vec::new();
                for (other_id, edge_type) in edges.into_iter().take(limit) {
                    let name = storage
                        .entity_get_by_id(ctx, session_id, other_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|e| e.entity_name)
                        .unwrap_or_else(|| other_id.to_string());
                    results.push(
                        serde_json::to_string(&serde_json::json!({
                            "entity_id": other_id.to_string(),
                            "entity_name": name,
                            "edge_type": edge_type,
                        }))
                        .unwrap_or_default(),
                    );
                }
                results
            }
        }
        "entities_in_fold" => {
            let graph = session
                .graph
                .as_ref()
                .ok_or((INTERNAL_ERROR, "graph client not configured".into()))?;
            let fold_id = require_uuid(&args, "fold_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            let mut r = graph
                .get_entities_in_fold(fold_id, session_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            r.truncate(limit);
            r
        }
        "supersession_chain" => {
            let graph = session
                .graph
                .as_ref()
                .ok_or((INTERNAL_ERROR, "graph client not configured".into()))?;
            let entity_id = require_uuid(&args, "entity_id")?;
            let event_id = entity_id;
            let fact_entity_id = optional_uuid(&args, "fold_id")?.unwrap_or(event_id);
            graph
                .get_supersession_chain(event_id, fact_entity_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        }
        _ => {
            return Err((
                INVALID_PARAMS,
                format!("unknown traversal type: {traversal}"),
            ));
        }
    };

    let result_count = results.len();
    let mut response = serde_json::json!({
        "traversal": traversal,
        "results": results,
        "count": result_count
    });
    if result_count < 2 {
        response["_hint"] = serde_json::json!(
            "Few connections found. Try spread_activation for broader associative recall, or run_consolidation to discover CO_OCCURS patterns."
        );
    }
    Ok(response)
}

// --- Hybrid search handler ---

#[derive(Debug, Clone)]
struct LlmRerankReport {
    enabled: bool,
    applied: bool,
    mode: String,
    provider: String,
    model: String,
    candidate_count: usize,
    returned_ids: Vec<uuid::Uuid>,
    judged_ids: Vec<uuid::Uuid>,
    judge_scores: Vec<Option<i64>>,
    score_sum: i64,
    abstentions: usize,
    batches: Vec<LlmRerankBatchReport>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct LlmRerankBatchReport {
    start_rank: usize,
    candidate_count: usize,
    returned_ids: Vec<uuid::Uuid>,
    judge_scores: Vec<Option<i64>>,
    score_sum: i64,
    abstentions: usize,
    error: Option<String>,
}

impl LlmRerankReport {
    fn disabled(config: &crate::config::JudgeConfig) -> Self {
        Self {
            enabled: false,
            applied: false,
            mode: "disabled".to_string(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            candidate_count: 0,
            returned_ids: Vec::new(),
            judged_ids: Vec::new(),
            judge_scores: Vec::new(),
            score_sum: 0,
            abstentions: 0,
            batches: Vec::new(),
            error: None,
        }
    }

    fn skipped(config: &crate::config::JudgeConfig, reason: impl Into<String>) -> Self {
        Self {
            enabled: config.enabled,
            applied: false,
            mode: "skipped".to_string(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            candidate_count: 0,
            returned_ids: Vec::new(),
            judged_ids: Vec::new(),
            judge_scores: Vec::new(),
            score_sum: 0,
            abstentions: 0,
            batches: Vec::new(),
            error: Some(reason.into()),
        }
    }

    fn skipped_with_count(
        config: &crate::config::JudgeConfig,
        candidate_count: usize,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            enabled: config.enabled,
            applied: false,
            mode: "skipped".to_string(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            candidate_count,
            returned_ids: Vec::new(),
            judged_ids: Vec::new(),
            judge_scores: vec![None; candidate_count],
            score_sum: 0,
            abstentions: candidate_count,
            batches: Vec::new(),
            error: Some(reason.into()),
        }
    }
}

fn truncate_for_llm(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect::<String>()
}

fn collect_transcript_text(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) if !text.trim().is_empty() => {
            out.push(text.trim().to_string());
        }
        Value::Array(items) => {
            for item in items {
                collect_transcript_text(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["text", "content", "stdout", "stderr"] {
                if let Some(value) = map.get(key) {
                    collect_transcript_text(value, out);
                }
            }
            for key in ["message", "toolUseResult"] {
                if let Some(value) = map.get(key) {
                    collect_transcript_text(value, out);
                }
            }
        }
        _ => {}
    }
}

fn contains_tool_result_block(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_tool_result_block),
        Value::Object(map) => {
            map.get("type").and_then(Value::as_str) == Some("tool_result")
                || map.values().any(contains_tool_result_block)
        }
        _ => false,
    }
}

fn compact_transcript_context_for_rerank(text: &str) -> Option<String> {
    let mut pieces = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some((prefix, payload)) = trimmed.split_once(": ") else {
            continue;
        };
        if !prefix.contains('[') || !prefix.ends_with(']') {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        let mut texts = Vec::new();
        collect_transcript_text(&parsed, &mut texts);
        if texts.is_empty() {
            continue;
        }
        let label = if contains_tool_result_block(&parsed)
            || texts
                .iter()
                .any(|text| text.contains("[This command modified"))
        {
            "tool result"
        } else if prefix.starts_with("assistant") {
            "assistant turn"
        } else {
            "user turn"
        };
        pieces.push(format!("{label}: {}", texts.join(" ")));
    }
    if pieces.is_empty() {
        None
    } else {
        Some(truncate_for_llm(&pieces.join("\n"), 500))
    }
}

fn rerank_candidate_content(result: &crate::hybrid_search::SearchResult) -> String {
    if result.expanded_context.is_empty() {
        if result.memory_kind == "episodic"
            && let Some(compacted) = compact_transcript_context_for_rerank(&result.content)
        {
            return compacted;
        }
        return truncate_for_llm(&result.content, 500);
    }
    let mut text = format!("Hit chunk:\n{}", truncate_for_llm(&result.content, 360));
    text.push_str("\n\nExpanded neighboring chunks:");
    for chunk in &result.expanded_context {
        text.push_str(&format!(
            "\n[{}:{} tokens={}]\n{}",
            chunk.position,
            chunk.distance,
            chunk.token_count,
            truncate_for_llm(&chunk.content, 260)
        ));
    }
    truncate_for_llm(&text, 1200)
}

fn parse_llm_rerank_json(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    let object = trimmed
        .find('{')
        .zip(trimmed.rfind('}'))
        .and_then(|(start, end)| trimmed.get(start..=end))
        .and_then(|slice| serde_json::from_str::<Value>(slice).ok());
    if object.is_some() {
        return object;
    }
    trimmed
        .find('[')
        .zip(trimmed.rfind(']'))
        .and_then(|(start, end)| trimmed.get(start..=end))
        .and_then(|slice| serde_json::from_str::<Value>(slice).ok())
}

fn parse_llm_rerank_order(raw: &str, candidate_ids: &[uuid::Uuid]) -> Vec<uuid::Uuid> {
    let Some(value) = parse_llm_rerank_json(raw) else {
        return Vec::new();
    };
    let candidate_set: std::collections::HashSet<uuid::Uuid> =
        candidate_ids.iter().copied().collect();
    let mut ordered = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect_rerank_ids(
        &value,
        candidate_ids,
        &candidate_set,
        &mut seen,
        &mut ordered,
    );
    ordered
}

fn parse_llm_judge_scores(raw: &str, candidate_count: usize) -> Vec<Option<i64>> {
    let Some(value) = parse_llm_rerank_json(raw) else {
        return vec![None; candidate_count];
    };
    let scores_value = value.get("scores").or_else(|| value.get("judgments"));
    match scores_value {
        Some(Value::Array(values)) => {
            let mut scores = values
                .iter()
                .take(candidate_count)
                .map(parse_judge_score_value)
                .collect::<Vec<_>>();
            scores.resize(candidate_count, None);
            scores
        }
        Some(Value::Object(values)) => {
            let mut scores = vec![None; candidate_count];
            for (key, value) in values {
                let Ok(rank) = key.parse::<usize>() else {
                    continue;
                };
                if rank == 0 || rank > candidate_count {
                    continue;
                }
                scores[rank - 1] = parse_judge_score_value(value);
            }
            scores
        }
        _ => vec![None; candidate_count],
    }
}

fn parse_judge_score_value(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().map(|score| score.clamp(-1, 1)),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed == "-" || trimmed.eq_ignore_ascii_case("abstain") {
                None
            } else {
                trimmed.parse::<i64>().ok().map(|score| score.clamp(-1, 1))
            }
        }
        Value::Bool(relevant) => Some(if *relevant { 1 } else { -1 }),
        Value::Null => None,
        _ => None,
    }
}

fn collect_rerank_ids(
    value: &Value,
    candidate_ids: &[uuid::Uuid],
    candidate_set: &std::collections::HashSet<uuid::Uuid>,
    seen: &mut std::collections::HashSet<uuid::Uuid>,
    ordered: &mut Vec<uuid::Uuid>,
) {
    match value {
        Value::String(raw) => {
            let trimmed = raw.trim().trim_matches('"');
            if let Ok(id) = trimmed.parse::<uuid::Uuid>()
                && candidate_set.contains(&id)
                && seen.insert(id)
            {
                ordered.push(id);
            }
        }
        Value::Number(number) => {
            if let Some(raw) = number.as_u64()
                && raw > 0
                && let Some(id) = candidate_ids.get(raw as usize - 1).copied()
                && seen.insert(id)
            {
                ordered.push(id);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_rerank_ids(value, candidate_ids, candidate_set, seen, ordered);
            }
        }
        Value::Object(map) => {
            for key in ["order", "ranking", "ids", "results", "ranked_ids"] {
                if let Some(value) = map.get(key) {
                    collect_rerank_ids(value, candidate_ids, candidate_set, seen, ordered);
                    return;
                }
            }
            if let Some(value) = map.get("id").or_else(|| map.get("entity_id")) {
                collect_rerank_ids(value, candidate_ids, candidate_set, seen, ordered);
            }
        }
        _ => {}
    }
}

fn apply_llm_rerank_order(
    results: Vec<crate::hybrid_search::SearchResult>,
    order: &[uuid::Uuid],
    candidate_count: usize,
) -> Vec<crate::hybrid_search::SearchResult> {
    if order.is_empty() || candidate_count == 0 {
        return results;
    }
    let split_at = results.len().min(candidate_count);
    let mut top = results[..split_at].to_vec();
    let tail = results[split_at..].to_vec();
    let mut reranked = Vec::with_capacity(results.len());
    for id in order {
        if let Some(pos) = top.iter().position(|result| &result.id == id) {
            reranked.push(top.remove(pos));
        }
    }
    reranked.extend(top);
    reranked.extend(tail);
    reranked
}

/// Runtime snapshot of the `[search]` rerank tunables, read once per rerank call
/// from `SessionState::search` and threaded through the rerank helpers. Defaults
/// preserve the original hardcoded constant behaviour.
#[derive(Clone, Copy)]
struct RerankTunables {
    min_candidates: usize,
    max_candidates: usize,
    min_score_coverage: usize,
    batch_size: usize,
}

impl RerankTunables {
    fn from_search(search: &crate::config::SearchConfig) -> Self {
        let min_candidates = search.rerank_min_candidates.max(1);
        Self {
            min_candidates,
            max_candidates: search.rerank_max_candidates.max(min_candidates),
            min_score_coverage: search.rerank_min_score_coverage.max(1),
            batch_size: search.rerank_batch_size.max(1),
        }
    }
}

fn apply_llm_rerank_decision(
    results: Vec<crate::hybrid_search::SearchResult>,
    order: &[uuid::Uuid],
    judge_scores: &[Option<i64>],
    candidate_count: usize,
    min_score_coverage: usize,
) -> Vec<crate::hybrid_search::SearchResult> {
    if order.is_empty() || candidate_count == 0 {
        return results;
    }
    let split_at = results.len().min(candidate_count);
    let top = results[..split_at].to_vec();
    let tail = results[split_at..].to_vec();
    let order_rank = order
        .iter()
        .enumerate()
        .map(|(idx, id)| (*id, idx))
        .collect::<std::collections::HashMap<_, _>>();
    let score_bucket = |idx: usize| match judge_scores.get(idx).copied().flatten() {
        Some(score) if score > 0 => 0,
        Some(score) if score < 0 => 2,
        _ => 1,
    };
    let mut scored = top
        .into_iter()
        .enumerate()
        .map(|(idx, result)| {
            let bucket = score_bucket(idx);
            let rank = order_rank
                .get(&result.id)
                .copied()
                .unwrap_or(candidate_count + idx);
            (bucket, rank, idx, result)
        })
        .collect::<Vec<_>>();
    let scored_count = judge_scores
        .iter()
        .take(split_at)
        .filter(|score| score.is_some())
        .count();
    let has_negative = judge_scores
        .iter()
        .take(split_at)
        .any(|score| score.is_some_and(|score| score < 0));
    let has_score_contrast = scored
        .iter()
        .map(|(bucket, _, _, _)| *bucket)
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1
        && (has_negative || scored_count >= split_at.min(min_score_coverage));
    if !has_score_contrast {
        return apply_llm_rerank_order(
            scored
                .into_iter()
                .map(|(_, _, _, result)| result)
                .chain(tail)
                .collect(),
            order,
            candidate_count,
        );
    }
    scored.sort_by_key(|(bucket, rank, original_idx, _)| (*bucket, *rank, *original_idx));
    scored
        .into_iter()
        .map(|(_, _, _, result)| result)
        .chain(tail)
        .collect()
}

fn apply_llm_judge_authority(
    results: &mut Vec<crate::hybrid_search::SearchResult>,
    report: &LlmRerankReport,
) {
    if !report.applied {
        return;
    }
    let judgments = report
        .judged_ids
        .iter()
        .copied()
        .zip(report.judge_scores.iter().copied())
        .collect::<HashMap<_, _>>();
    results.retain_mut(
        |result| match judgments.get(&result.id).copied().flatten() {
            Some(score) if score > 0 => {
                result.score += 0.20;
                true
            }
            Some(score) if score < 0 => false,
            Some(_) => true,
            None if judgments.contains_key(&result.id) => {
                result.score = (result.score - 0.02).max(0.0);
                true
            }
            None => true,
        },
    );
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn judge_scores_for_response(scores: &[Option<i64>]) -> Vec<Value> {
    scores
        .iter()
        .map(|score| match score {
            Some(score) => serde_json::json!(score),
            None => serde_json::json!("-"),
        })
        .collect()
}

async fn generate_judge_text(
    config: &crate::config::JudgeConfig,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: usize,
) -> anyhow::Result<String> {
    let provider = config.provider.trim().to_ascii_lowercase();
    if provider == "mock" {
        return Ok(r#"{"order":[2,1],"scores":[1,1]}"#.to_string());
    }
    let timeout = std::time::Duration::from_secs(config.timeout_seconds.clamp(1, 300));
    // Bound connection establishment separately from the (longer) generation
    // timeout: when the judge endpoint is down/unreachable, fail fast so a
    // judge-on-by-default search skips the rerank in ~seconds instead of
    // hanging for the full request timeout. A reachable-but-slow judge still
    // gets the full `timeout` to generate once connected.
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(std::time::Duration::from_secs(
            JUDGE_CONNECT_TIMEOUT_SECONDS,
        ))
        .build()?;
    let base_url = config.base_url.trim_end_matches('/');
    anyhow::ensure!(!base_url.is_empty(), "judge base_url is empty");
    let mut request = match provider.as_str() {
        "ollama" | "ollama.com" => {
            client
                .post(format!("{base_url}/api/generate"))
                .json(&serde_json::json!({
                    "model": config.model,
                    "prompt": format!("{system_prompt}\n\n{user_prompt}"),
                    "stream": false,
                    "format": "json",
                    "options": {
                        "temperature": 0.0,
                        "num_predict": max_tokens.clamp(128, 2048)
                    }
                }))
        }
        "lmstudio" | "openai_compatible" | "openai-compatible" | "openai" => client
            .post(format!("{base_url}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": config.model,
                "temperature": 0.0,
                "max_tokens": max_tokens.clamp(128, 2048),
                "messages": [
                    {"role": "system", "content": system_prompt},
                    {"role": "user", "content": user_prompt}
                ]
            })),
        "disabled" => anyhow::bail!("judge provider is disabled"),
        other => anyhow::bail!("unsupported judge provider: {other}"),
    };
    if let Some(token) = config.token.as_deref()
        && !token.is_empty()
    {
        request = request.bearer_auth(token);
    }
    let value: Value = request.send().await?.error_for_status()?.json().await?;
    let text = value
        .get("response")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
                .and_then(|choice| {
                    choice
                        .get("message")
                        .and_then(|message| message.get("content"))
                        .and_then(Value::as_str)
                        .or_else(|| choice.get("text").and_then(Value::as_str))
                })
        })
        .ok_or_else(|| anyhow::anyhow!("judge provider returned no text"))?;
    Ok(text.to_string())
}

#[derive(Debug)]
struct LlmRerankDecision {
    order: Vec<uuid::Uuid>,
    judge_scores: Vec<Option<i64>>,
}

async fn judge_rerank_candidates(
    config: &crate::config::JudgeConfig,
    query: &str,
    candidates: &[crate::hybrid_search::SearchResult],
) -> anyhow::Result<LlmRerankDecision> {
    let candidate_ids = candidates
        .iter()
        .map(|result| result.id)
        .collect::<Vec<_>>();
    let candidates_json = candidates
        .iter()
        .enumerate()
        .map(|(idx, result)| {
            serde_json::json!({
                "rank": idx + 1,
                "id": result.id,
                "type": result.result_type,
                "source": result.source,
                "memory_kind": result.memory_kind,
                "content": rerank_candidate_content(result),
            })
        })
        .collect::<Vec<_>>();
    let prompt = format!(
        "Query:\n{query}\n\nCandidates JSON:\n{}\n\n\
         Rank the candidates by usefulness for answering the query. \
         Treat memory_kind as a recall category: episodic is prior conversation/tool context, procedural is how-to/decision/process memory, and semantic is durable factual/document memory. \
         Prefer candidates that directly answer the query; raw tool output is irrelevant unless the query asks about that exact prior action. \
         Also judge each candidate's relevance as 1 helpful, 0 neutral/unclear, -1 irrelevant/wrong, or \"-\" if you cannot judge. \
         Return JSON only in this shape: {{\"order\":[rank_number,...],\"scores\":[1|0|-1|\"-\",...]}}. \
         Use the 1-based rank numbers from the input, not UUIDs. \
         The scores array must be in the original candidate order.",
        serde_json::to_string(&candidates_json).unwrap_or_else(|_| "[]".to_string())
    );
    let max_tokens = 256 + candidates.len().saturating_mul(24);
    let raw = generate_judge_text(
        config,
        "You are a retrieval reranker. Return compact JSON only.",
        &prompt,
        max_tokens,
    )
    .await?;
    Ok(LlmRerankDecision {
        order: parse_llm_rerank_order(&raw, &candidate_ids),
        judge_scores: parse_llm_judge_scores(&raw, candidates.len()),
    })
}

fn batch_report(
    start_rank: usize,
    candidate_count: usize,
    order: Vec<uuid::Uuid>,
    judge_scores: Vec<Option<i64>>,
    error: Option<String>,
) -> LlmRerankBatchReport {
    let score_sum = judge_scores.iter().flatten().sum::<i64>();
    let abstentions = judge_scores.iter().filter(|score| score.is_none()).count();
    LlmRerankBatchReport {
        start_rank,
        candidate_count,
        returned_ids: order,
        judge_scores,
        score_sum,
        abstentions,
        error,
    }
}

fn rank_batches_by_winner_order(
    batch_winners: &[uuid::Uuid],
    final_order: &[uuid::Uuid],
) -> Vec<usize> {
    let order_rank = final_order
        .iter()
        .enumerate()
        .map(|(idx, id)| (*id, idx))
        .collect::<std::collections::HashMap<_, _>>();
    let mut batch_indices = (0..batch_winners.len()).collect::<Vec<_>>();
    batch_indices.sort_by_key(|idx| {
        (
            order_rank
                .get(&batch_winners[*idx])
                .copied()
                .unwrap_or(batch_winners.len() + *idx),
            *idx,
        )
    });
    batch_indices
}

async fn batched_llm_rerank_results(
    query: &str,
    results: Vec<crate::hybrid_search::SearchResult>,
    config: &crate::config::JudgeConfig,
    candidate_count: usize,
    tunables: RerankTunables,
) -> (Vec<crate::hybrid_search::SearchResult>, LlmRerankReport) {
    let split_at = results.len().min(candidate_count);
    let top = results[..split_at].to_vec();
    let tail = results[split_at..].to_vec();
    let judged_ids = top.iter().map(|result| result.id).collect::<Vec<_>>();
    let mut aggregate_scores = vec![None; split_at];
    let mut batches = Vec::new();
    let mut ordered_batches = Vec::new();
    let mut batch_winners = Vec::new();
    let mut any_applied = false;

    for (batch_idx, batch) in top.chunks(tunables.batch_size).enumerate() {
        let start_rank = batch_idx * tunables.batch_size + 1;
        let batch_vec = batch.to_vec();
        match judge_rerank_candidates(config, query, &batch_vec).await {
            Ok(decision) if decision.order.len() >= 2 => {
                any_applied = true;
                for (idx, score) in decision.judge_scores.iter().copied().enumerate() {
                    if let Some(slot) = aggregate_scores.get_mut(start_rank - 1 + idx) {
                        *slot = score;
                    }
                }
                let batch_order = apply_llm_rerank_decision(
                    batch_vec,
                    &decision.order,
                    &decision.judge_scores,
                    batch.len(),
                    tunables.min_score_coverage,
                );
                if let Some(winner) = batch_order.first() {
                    batch_winners.push(winner.id);
                }
                ordered_batches.push(batch_order);
                batches.push(batch_report(
                    start_rank,
                    batch.len(),
                    decision.order,
                    decision.judge_scores,
                    None,
                ));
            }
            Ok(decision) => {
                if let Some(winner) = batch_vec.first() {
                    batch_winners.push(winner.id);
                }
                ordered_batches.push(batch_vec);
                batches.push(batch_report(
                    start_rank,
                    batch.len(),
                    decision.order,
                    decision.judge_scores,
                    Some("judge returned fewer than two recognized IDs".to_string()),
                ));
            }
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(
                    provider = %config.provider,
                    model = %config.model,
                    batch_start_rank = start_rank,
                    error = %message,
                    "LLM rerank batch skipped"
                );
                let scores = vec![None; batch.len()];
                if let Some(winner) = batch_vec.first() {
                    batch_winners.push(winner.id);
                }
                ordered_batches.push(batch_vec);
                batches.push(batch_report(
                    start_rank,
                    batch.len(),
                    Vec::new(),
                    scores,
                    Some(message),
                ));
            }
        }
    }

    let final_order = if batch_winners.len() >= 2 {
        let winners = ordered_batches
            .iter()
            .filter_map(|batch| batch.first().cloned())
            .collect::<Vec<_>>();
        match judge_rerank_candidates(config, query, &winners).await {
            Ok(decision) if decision.order.len() >= 2 => {
                any_applied = true;
                batches.push(batch_report(
                    1,
                    winners.len(),
                    decision.order.clone(),
                    decision.judge_scores,
                    None,
                ));
                decision.order
            }
            Ok(decision) => {
                batches.push(batch_report(
                    1,
                    winners.len(),
                    decision.order,
                    decision.judge_scores,
                    Some("final judge returned fewer than two recognized IDs".to_string()),
                ));
                Vec::new()
            }
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(
                    provider = %config.provider,
                    model = %config.model,
                    error = %message,
                    "LLM final rerank batch skipped"
                );
                batches.push(batch_report(
                    1,
                    winners.len(),
                    Vec::new(),
                    vec![None; winners.len()],
                    Some(message),
                ));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    if !any_applied {
        return (
            results,
            LlmRerankReport {
                enabled: true,
                applied: false,
                mode: "batched".to_string(),
                provider: config.provider.clone(),
                model: config.model.clone(),
                candidate_count,
                returned_ids: Vec::new(),
                judged_ids,
                judge_scores: aggregate_scores,
                score_sum: 0,
                abstentions: split_at,
                batches,
                error: Some("all rerank batches failed or returned too few IDs".to_string()),
            },
        );
    }

    let mut reranked_top = Vec::with_capacity(top.len());
    for batch_idx in rank_batches_by_winner_order(&batch_winners, &final_order) {
        if let Some(batch) = ordered_batches.get(batch_idx) {
            reranked_top.extend(batch.iter().cloned());
        }
    }
    let returned_ids = reranked_top
        .iter()
        .map(|result| result.id)
        .collect::<Vec<_>>();
    let mut reranked = reranked_top;
    reranked.extend(tail);
    let score_sum = aggregate_scores.iter().flatten().sum::<i64>();
    let abstentions = aggregate_scores
        .iter()
        .filter(|score| score.is_none())
        .count();
    (
        reranked,
        LlmRerankReport {
            enabled: true,
            applied: true,
            mode: "batched".to_string(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            candidate_count,
            returned_ids,
            judged_ids,
            judge_scores: aggregate_scores,
            score_sum,
            abstentions,
            batches,
            error: None,
        },
    )
}

async fn maybe_llm_rerank_results(
    query: &str,
    results: Vec<crate::hybrid_search::SearchResult>,
    session: &SessionState,
    rerank_override: Option<bool>,
    candidate_override: Option<usize>,
) -> (Vec<crate::hybrid_search::SearchResult>, LlmRerankReport) {
    let config = session.judge_config.lock().await.clone();
    let tunables = RerankTunables::from_search(&session.search.lock().await.clone());
    let enabled = rerank_override.unwrap_or(config.enabled);
    if !enabled {
        return (results, LlmRerankReport::disabled(&config));
    }
    if results.len() < 2 {
        return (
            results,
            LlmRerankReport::skipped(&config, "fewer than two candidates"),
        );
    }
    let configured_candidates = candidate_override
        .unwrap_or(config.max_rerank_candidates)
        .clamp(tunables.min_candidates, tunables.max_candidates);
    let candidate_count = results.len().min(configured_candidates);
    if candidate_count > tunables.batch_size {
        return batched_llm_rerank_results(query, results, &config, candidate_count, tunables)
            .await;
    }
    let candidate_ids = results
        .iter()
        .take(candidate_count)
        .map(|result| result.id)
        .collect::<Vec<_>>();
    let candidates = results
        .iter()
        .take(candidate_count)
        .cloned()
        .collect::<Vec<_>>();
    let decision = match judge_rerank_candidates(&config, query, &candidates).await {
        Ok(decision) => decision,
        Err(error) => {
            let message = error.to_string();
            tracing::warn!(
                provider = %config.provider,
                model = %config.model,
                error = %message,
                "LLM reranking skipped"
            );
            return (results, LlmRerankReport::skipped(&config, message));
        }
    };
    let order = decision.order;
    let judge_scores = decision.judge_scores;
    let score_sum = judge_scores.iter().flatten().sum::<i64>();
    let abstentions = judge_scores.iter().filter(|score| score.is_none()).count();
    if order.len() < 2 {
        return (
            results,
            LlmRerankReport::skipped_with_count(
                &config,
                candidate_count,
                "judge returned fewer than two recognized IDs",
            ),
        );
    }
    let reranked = apply_llm_rerank_decision(
        results,
        &order,
        &judge_scores,
        candidate_count,
        tunables.min_score_coverage,
    );
    (
        reranked,
        LlmRerankReport {
            enabled: true,
            applied: true,
            mode: "single".to_string(),
            provider: config.provider,
            model: config.model,
            candidate_count,
            returned_ids: order,
            judged_ids: candidate_ids,
            judge_scores,
            score_sum,
            abstentions,
            batches: Vec::new(),
            error: None,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutoFusionSelection {
    intent: &'static str,
    profile: &'static str,
}

fn select_auto_fusion_profile(
    query: &str,
    filter: &crate::hybrid_search::SearchFilter,
) -> AutoFusionSelection {
    let lower = query.to_ascii_lowercase();
    let tokens = lower
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let has_token = |candidates: &[&str]| tokens.iter().any(|token| candidates.contains(token));
    let has_workspace = filter
        .workspace_cwd
        .as_deref()
        .is_some_and(|cwd| !cwd.is_empty());

    // 1. Exact error / symbol — a pasted stack trace, file:line, or code
    //    syntax. Exact tokens must match, so route to a lexical-only profile.
    let exact_error_or_symbol = lower.contains("thread '")
        || lower.contains("panicked at")
        || lower.contains("traceback")
        || lower.contains("exception")
        || lower.contains("error[")
        || lower.contains("no such file")
        || lower.contains("undefined symbol")
        || lower.contains("src/")
        || lower.contains(".rs:")
        || lower.contains(".py:")
        || lower.contains(".ex:")
        || lower.contains(".tsx:")
        || lower.contains(".ts:")
        || looks_like_code(&lower);
    if exact_error_or_symbol {
        return AutoFusionSelection {
            intent: "exact_error_or_symbol",
            profile: "bm25-only",
        };
    }

    // 2. Session memory — conversational/temporal recall of recent work. Gated
    //    on explicit deixis, NOT on scope (which defaults to SessionOnly), so
    //    it doesn't swallow ordinary queries.
    let session_recall = has_token(&["yesterday", "earlier", "recently"])
        || lower.contains("last session")
        || lower.contains("working on")
        || lower.contains("we discussed")
        || lower.contains("we decided")
        || lower.contains("what were we")
        || lower.contains("what did we")
        || lower.contains("were we working");
    if session_recall {
        return AutoFusionSelection {
            intent: "session_memory",
            profile: "session-semantic",
        };
    }

    // 3. Project bug / build — a workspace-scoped engineering failure.
    let project_bug_or_build = has_workspace
        && has_token(&[
            "bug",
            "fix",
            "failing",
            "failed",
            "fail",
            "ci",
            "test",
            "tests",
            "build",
            "pr",
            "branch",
            "merge",
            "deploy",
            "regression",
            "flaky",
        ]);
    if project_bug_or_build {
        return AutoFusionSelection {
            intent: "project_bug_or_build",
            profile: "bm25-semantic-workspace",
        };
    }

    // 4. Corpus reference — points at the document corpus (papers, citations).
    let corpus_reference = has_token(&[
        "paper",
        "papers",
        "citation",
        "cite",
        "doi",
        "author",
        "bibliography",
        "corpus",
        "book",
    ]);
    if corpus_reference {
        return AutoFusionSelection {
            intent: "corpus_reference",
            profile: "corpus-reference",
        };
    }

    // 5. Broad semantic — conceptual / explanatory recall. Lean semantic to
    //    avoid the noisy bm25-only candidates the eval flagged.
    let broad_semantic = [
        "architecture",
        "design",
        "explain",
        "compare",
        "overview",
        "summarize",
        "rlm",
        "evermem",
        "memscene",
        "research",
        "reference",
        "why ",
        "how ",
        "what is",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if broad_semantic {
        return AutoFusionSelection {
            intent: "broad_semantic",
            profile: "bm25-semantic",
        };
    }

    AutoFusionSelection {
        intent: "default_balanced",
        profile: "auto",
    }
}

/// Cheap heuristic: does the query contain code-like syntax that prose rarely
/// does? Strengthens exact-error/symbol routing for bare identifiers like
/// `GraphClient::reconnecting_storage()` that carry no stack-trace markers.
fn looks_like_code(lower: &str) -> bool {
    lower.contains("->")
        || lower.contains("=>")
        || lower.contains("::")
        || lower.contains("();")
        // a function-call shape: an identifier char immediately followed by '('
        || lower
            .as_bytes()
            .windows(2)
            .any(|w| (w[0] as char).is_ascii_alphanumeric() && w[1] == b'(')
}

fn parse_hybrid_search_scope(
    args: &Value,
) -> Result<crate::hybrid_search::SearchScope, (i32, String)> {
    if let Some(scope) = args.get("scope").and_then(|v| v.as_str()) {
        return match scope {
            "session" | "session_only" => Ok(crate::hybrid_search::SearchScope::SessionOnly),
            "global" | "global_only" => Ok(crate::hybrid_search::SearchScope::GlobalOnly),
            "both" => Ok(crate::hybrid_search::SearchScope::Both),
            other => Err((
                INVALID_PARAMS,
                format!("invalid scope: expected session|global|both, got {other}"),
            )),
        };
    }
    // No explicit `scope`: default to Both so curated global + nil corpus is
    // visible to normal agent searches (session affinity is handled by ranking
    // weight, not by filtering global knowledge out). An explicit
    // `include_cross_session: false` is still honored as a session-only opt-out.
    match args.get("include_cross_session").and_then(|v| v.as_bool()) {
        Some(true) => Ok(crate::hybrid_search::SearchScope::Both),
        Some(false) => Ok(crate::hybrid_search::SearchScope::SessionOnly),
        None => Ok(crate::hybrid_search::SearchScope::Both),
    }
}

async fn load_authority_score_maps<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    session_id: uuid::Uuid,
    scope: crate::hybrid_search::SearchScope,
) -> anyhow::Result<(HashMap<uuid::Uuid, f64>, HashMap<uuid::Uuid, f64>)> {
    let mut pagerank_scores: HashMap<uuid::Uuid, f64> = HashMap::new();
    let mut reputation_scores: HashMap<uuid::Uuid, f64> = HashMap::new();
    for sid in crate::hybrid_search::sessions_to_query(session_id, ctx.tenant_id, scope) {
        for entry in storage.warmth_list_session(ctx, sid).await? {
            if entry.pagerank != 0.0 {
                pagerank_scores
                    .entry(entry.entity_id)
                    .and_modify(|score| *score = score.max(entry.pagerank))
                    .or_insert(entry.pagerank);
            }
            if entry.reputation != 0.0 {
                reputation_scores
                    .entry(entry.entity_id)
                    .and_modify(|score| *score = (*score + entry.reputation).clamp(-1.0, 1.0))
                    .or_insert(entry.reputation.clamp(-1.0, 1.0));
            }
        }
    }
    Ok((pagerank_scores, reputation_scores))
}

#[derive(Debug, Clone, serde::Serialize)]
struct ChunkExpansionReport {
    mode: String,
    prev: usize,
    next: usize,
    max_tokens: i32,
    expanded_results: usize,
    added_chunks: usize,
    added_tokens: i32,
}

#[derive(Debug, Clone)]
struct ChunkExpansionConfig {
    mode: String,
    prev: usize,
    next: usize,
    max_tokens: i32,
}

fn parse_chunk_expansion(args: &Value) -> Result<ChunkExpansionConfig, (i32, String)> {
    let mode = args
        .get("chunk_expansion")
        .and_then(Value::as_str)
        .unwrap_or("none");
    match mode {
        "none" | "neighbors" => {}
        _ => {
            return Err((INVALID_PARAMS, format!("unknown chunk_expansion: {mode}")));
        }
    }
    let prev = args
        .get("chunk_prev")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .min(5) as usize;
    let next = args
        .get("chunk_next")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .min(5) as usize;
    let max_tokens = args
        .get("chunk_max_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(1600)
        .clamp(1, 8000) as i32;
    Ok(ChunkExpansionConfig {
        mode: mode.to_string(),
        prev,
        next,
        max_tokens,
    })
}

fn expanded_chunk_context(
    chunk: &crate::types::DocumentChunk,
    position: &str,
    distance: usize,
) -> crate::hybrid_search::ExpandedChunkContext {
    crate::hybrid_search::ExpandedChunkContext {
        chunk_id: chunk.chunk_id,
        document_id: chunk.document_id,
        ordinal: chunk.ordinal,
        position: position.to_string(),
        distance,
        token_count: chunk.token_count,
        section_path: chunk.section_path.clone(),
        content: chunk.content.clone(),
    }
}

async fn apply_chunk_expansion<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    sessions: &[uuid::Uuid],
    results: &mut [crate::hybrid_search::SearchResult],
    config: &ChunkExpansionConfig,
) -> Result<ChunkExpansionReport, (i32, String)> {
    let mut report = ChunkExpansionReport {
        mode: config.mode.clone(),
        prev: config.prev,
        next: config.next,
        max_tokens: config.max_tokens,
        expanded_results: 0,
        added_chunks: 0,
        added_tokens: 0,
    };
    if config.mode == "none" || (config.prev == 0 && config.next == 0) {
        return Ok(report);
    }

    for result in results.iter_mut() {
        if result.result_type != "document_chunk" {
            continue;
        }
        let mut added = Vec::new();
        let mut added_tokens = 0i32;

        let mut before = Vec::new();
        let mut cursor = result.prev_chunk_id;
        while before.len() < config.prev {
            let Some(chunk_id) = cursor else { break };
            let Some(chunk) = get_document_chunk_in_scope(storage, ctx, sessions, chunk_id).await?
            else {
                break;
            };
            cursor = chunk.prev_chunk_id;
            before.push(chunk);
        }
        before.reverse();
        for (idx, chunk) in before.iter().enumerate() {
            let distance = before.len().saturating_sub(idx);
            if added_tokens + chunk.token_count.max(0) > config.max_tokens {
                continue;
            }
            added_tokens += chunk.token_count.max(0);
            added.push(expanded_chunk_context(chunk, "prev", distance));
        }

        cursor = result.next_chunk_id;
        let mut distance = 1usize;
        while distance <= config.next {
            let Some(chunk_id) = cursor else { break };
            let Some(chunk) = get_document_chunk_in_scope(storage, ctx, sessions, chunk_id).await?
            else {
                break;
            };
            cursor = chunk.next_chunk_id;
            if added_tokens + chunk.token_count.max(0) <= config.max_tokens {
                added_tokens += chunk.token_count.max(0);
                added.push(expanded_chunk_context(&chunk, "next", distance));
            }
            distance += 1;
        }

        if added.is_empty() {
            continue;
        }

        let mut expanded_text = String::new();
        for chunk in &added {
            expanded_text.push_str(&format!(
                "\n\n[expanded {} chunk distance={} ordinal={}]\n{}",
                chunk.position, chunk.distance, chunk.ordinal, chunk.content
            ));
        }
        result.content.push_str(&expanded_text);
        result.expanded_context = added;
        result.hint = Some(
            "This result includes bounded prev/next chunk expansion. Use chunk_ctx if more neighboring context is needed."
                .into(),
        );
        report.expanded_results += 1;
        report.added_chunks += result.expanded_context.len();
        report.added_tokens += added_tokens;
    }

    Ok(report)
}

async fn get_document_chunk_in_scope<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    sessions: &[uuid::Uuid],
    chunk_id: uuid::Uuid,
) -> Result<Option<crate::types::DocumentChunk>, (i32, String)> {
    for session_id in sessions {
        if let Some(chunk) = storage
            .document_chunk_get(ctx, *session_id, chunk_id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        {
            return Ok(Some(chunk));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, serde::Serialize)]
struct QueryVariantDiagnostic {
    query: String,
    rank: usize,
    result_count: usize,
    candidate_fanout: crate::hybrid_search::SearchDiagnostics,
    embedding_status: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct QueryDecompositionReport {
    mode: String,
    task: String,
    generator_status: String,
    query_count: usize,
    unique_results: usize,
    queries: Vec<QueryVariantDiagnostic>,
}

struct QueryVariantSearchOutput {
    query: String,
    output: crate::hybrid_search::SearchOutput,
    embedding_status: String,
}

struct MergedQueryVariantOutput {
    results: Vec<crate::hybrid_search::SearchResult>,
    diagnostics: QueryDecompositionReport,
}

fn query_decomposition_stopwords(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "can"
            | "does"
            | "for"
            | "from"
            | "how"
            | "in"
            | "into"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "that"
            | "the"
            | "their"
            | "this"
            | "to"
            | "what"
            | "when"
            | "where"
            | "which"
            | "with"
            | "within"
    )
}

fn validate_query_decomposition_task(task: &str) -> Result<&str, (i32, String)> {
    match task {
        "general" | "bright_pro" | "memorybench" => Ok(task),
        _ => Err((INVALID_PARAMS, format!("unknown query_task: {task}"))),
    }
}

fn normalize_query_token(token: &str) -> String {
    token
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
        .to_string()
}

fn push_unique_query(queries: &mut Vec<String>, seen: &mut HashSet<String>, query: String) {
    let query = query.trim();
    if query.is_empty() {
        return;
    }
    let normalized = query.to_ascii_lowercase();
    if seen.insert(normalized) {
        queries.push(query.to_string());
    }
}

fn keyword_query(query: &str, max_terms: usize) -> Option<String> {
    let terms = query
        .split_whitespace()
        .map(normalize_query_token)
        .filter(|token| token.len() >= 3)
        .filter(|token| !query_decomposition_stopwords(&token.to_ascii_lowercase()))
        .take(max_terms)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn entity_alias_query(query: &str, max_terms: usize) -> Option<String> {
    let terms = query
        .split_whitespace()
        .map(normalize_query_token)
        .filter(|token| token.len() >= 2)
        .filter(|token| {
            token.chars().any(|ch| ch.is_ascii_uppercase())
                || token.chars().any(|ch| ch.is_ascii_digit())
                || token.contains('-')
                || token.contains('_')
        })
        .take(max_terms)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn build_query_variants(
    query: &str,
    mode: &str,
    task: &str,
    caller_variants: &[String],
    max_variants: usize,
) -> Result<Vec<String>, (i32, String)> {
    match mode {
        "none" | "heuristic" | "llm" => {}
        _ => {
            return Err((
                INVALID_PARAMS,
                format!("unknown query_decomposition: {mode}"),
            ));
        }
    }
    validate_query_decomposition_task(task)?;
    let max_variants = max_variants.clamp(1, 8);
    let mut variants = Vec::new();
    let mut seen = HashSet::new();
    push_unique_query(&mut variants, &mut seen, query.to_string());

    for variant in caller_variants {
        if variants.len() >= max_variants {
            return Ok(variants);
        }
        push_unique_query(&mut variants, &mut seen, variant.to_string());
    }

    if mode == "heuristic" || mode == "llm" {
        if variants.len() < max_variants
            && let Some(keywords) = keyword_query(query, 14)
        {
            push_unique_query(&mut variants, &mut seen, keywords.clone());
            if variants.len() < max_variants {
                let evidence_query = match task {
                    "bright_pro" => format!("documents explaining {keywords}"),
                    "memorybench" => format!("conversation memory feedback about {keywords}"),
                    _ => format!("supporting evidence for {keywords}"),
                };
                push_unique_query(&mut variants, &mut seen, evidence_query);
            }
        }
        if variants.len() < max_variants
            && let Some(entities) = entity_alias_query(query, 10)
        {
            push_unique_query(&mut variants, &mut seen, entities);
        }
    }

    Ok(variants)
}

fn collect_subquery_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.to_string()),
        Value::Array(values) => {
            for value in values {
                collect_subquery_strings(value, out);
            }
        }
        Value::Object(map) => {
            for key in ["queries", "subqueries", "query_variants", "variants"] {
                if let Some(value) = map.get(key) {
                    collect_subquery_strings(value, out);
                    return;
                }
            }
            if let Some(value) = map.get("query").or_else(|| map.get("text")) {
                collect_subquery_strings(value, out);
            }
        }
        _ => {}
    }
}

fn build_llm_query_variants(query: &str, raw: &str, max_variants: usize) -> Vec<String> {
    let max_variants = max_variants.clamp(1, 8);
    let mut variants = Vec::new();
    let mut seen = HashSet::new();
    push_unique_query(&mut variants, &mut seen, query.to_string());

    let Some(value) = parse_llm_rerank_json(raw) else {
        return variants;
    };
    let mut generated = Vec::new();
    collect_subquery_strings(&value, &mut generated);
    for variant in generated {
        if variants.len() >= max_variants {
            break;
        }
        push_unique_query(&mut variants, &mut seen, variant);
    }
    variants
}

fn query_decomposition_subject(query: &str, task: &str) -> String {
    if task == "memorybench"
        && let Some(question_start) = query.find("[Question]")
    {
        let after_question = &query[question_start + "[Question]".len()..];
        let question = after_question
            .split("[Answer]")
            .next()
            .unwrap_or(after_question)
            .trim();
        if !question.is_empty() {
            return question.to_string();
        }
    }
    query.trim().to_string()
}

fn query_decomposition_system_prompt(task: &str) -> &'static str {
    match task {
        "bright_pro" => {
            "You generate high-precision retrieval subqueries for BRIGHT-Pro reasoning-intensive retrieval. Return compact JSON only."
        }
        "memorybench" => {
            "You generate high-precision natural-language retrieval subqueries for conversation memory and feedback recall. Return compact JSON only."
        }
        _ => "You generate high-precision retrieval subqueries. Return compact JSON only.",
    }
}

fn query_decomposition_user_prompt(query: &str, task: &str, max_variants: usize) -> String {
    let subject = query_decomposition_subject(query, task);
    let task_guidance = match task {
        "bright_pro" => {
            "The goal is to retrieve support documents for a reasoning-heavy question. Generate exact-entity, core-claim, alias/paraphrase, and evidence-focused subqueries. Avoid generic filler and avoid adding unsupported facts."
        }
        "memorybench" => {
            "The goal is to retrieve memories from prior conversations and feedback. Generate natural-language subqueries for entities, dates, user/task context, correction/feedback, and likely answer-bearing memory. Do not write SQL, code, schema names, or database commands."
        }
        _ => {
            "Generate exact-entity, core-concept, alias/paraphrase, and evidence-focused subqueries. Avoid generic filler and avoid adding unsupported facts."
        }
    };
    format!(
        "{task_guidance}\n\nOriginal query:\n{query}\n\nQuery subject:\n{subject}\n\nReturn JSON only: {{\"queries\":[\"subquery\",...]}}. \
         Return at most {} subqueries. Do not repeat the original query or write query languages.",
        max_variants.saturating_sub(1).max(1)
    )
}

async fn generate_llm_query_variants(
    config: &crate::config::JudgeConfig,
    query: &str,
    task: &str,
    max_variants: usize,
) -> anyhow::Result<Vec<String>> {
    let raw = generate_judge_text(
        config,
        query_decomposition_system_prompt(task),
        &query_decomposition_user_prompt(query, task, max_variants),
        384,
    )
    .await?;
    Ok(build_llm_query_variants(query, &raw, max_variants))
}

fn collapse_duplicate_variant_documents(
    results: Vec<crate::hybrid_search::SearchResult>,
) -> Vec<crate::hybrid_search::SearchResult> {
    let mut seen_documents = HashSet::new();
    let mut collapsed = Vec::with_capacity(results.len());
    for result in results {
        if result.result_type == "document_chunk"
            && let Some(document_id) = result.document_id
            && !seen_documents.insert(document_id)
        {
            continue;
        }
        collapsed.push(result);
    }
    collapsed
}

fn apply_result_filters(
    results: &mut Vec<crate::hybrid_search::SearchResult>,
    min_score: Option<f64>,
    memory_kinds: Option<&[String]>,
) {
    let allowed_kinds = memory_kinds.map(|kinds| {
        kinds
            .iter()
            .map(|kind| kind.to_ascii_lowercase())
            .collect::<HashSet<_>>()
    });
    results.retain(|result| {
        min_score.is_none_or(|threshold| result.score >= threshold)
            && allowed_kinds
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&result.memory_kind.to_ascii_lowercase()))
    });
}

fn merge_query_variant_outputs(
    outputs: Vec<QueryVariantSearchOutput>,
    limit: usize,
) -> MergedQueryVariantOutput {
    let mut scores: HashMap<uuid::Uuid, (f64, crate::hybrid_search::SearchResult)> = HashMap::new();
    let mut unique = HashSet::new();
    let mut queries = Vec::with_capacity(outputs.len());

    for (variant_rank, variant) in outputs.into_iter().enumerate() {
        for (rank, result) in variant.output.results.iter().enumerate() {
            unique.insert(result.id);
            let variant_weight = if variant_rank == 0 { 4.0 } else { 0.75 };
            let score = result.score + variant_weight / (60.0 + rank as f64 + 1.0);
            scores
                .entry(result.id)
                .and_modify(|(existing_score, existing_result)| {
                    *existing_score += score;
                    if result.content.len() > existing_result.content.len() {
                        *existing_result = result.clone();
                    }
                })
                .or_insert((score, result.clone()));
        }
        queries.push(QueryVariantDiagnostic {
            rank: variant_rank + 1,
            result_count: variant.output.results.len(),
            candidate_fanout: variant.output.diagnostics,
            embedding_status: variant.embedding_status,
            query: variant.query,
        });
    }

    let mut results = scores
        .into_values()
        .map(|(score, mut result)| {
            result.score = score;
            result
        })
        .collect::<Vec<_>>();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let results = collapse_duplicate_variant_documents(results)
        .into_iter()
        .take(limit)
        .collect::<Vec<_>>();

    MergedQueryVariantOutput {
        results,
        diagnostics: QueryDecompositionReport {
            mode: if queries.len() > 1 {
                "active".into()
            } else {
                "none".into()
            },
            task: "general".into(),
            generator_status: "not_requested".into(),
            query_count: queries.len(),
            unique_results: unique.len(),
            queries,
        },
    }
}

async fn set_memory_authority<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    target_id: uuid::Uuid,
    session_id: uuid::Uuid,
    reputation: Option<f64>,
    pagerank: Option<f64>,
) -> Result<crate::types::WarmthEntry, (i32, String)> {
    let now = chrono::Utc::now();
    let mut entry = storage
        .warmth_get(ctx, target_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        .unwrap_or(crate::types::WarmthEntry {
            tenant_id: ctx.tenant_id,
            entity_id: target_id,
            session_id,
            warmth: 0.0,
            pagerank: 0.0,
            reputation: 0.0,
            last_accessed_at: now,
            access_count: 0,
            decay_zone: crate::types::DecayZone::Knowledge,
            updated_at: now,
        });
    entry.session_id = session_id;
    if let Some(score) = reputation {
        entry.reputation = score.clamp(-1.0, 1.0);
    }
    if let Some(score) = pagerank {
        entry.pagerank = score.clamp(0.0, 1.0);
    }
    entry.updated_at = now;
    storage
        .warmth_put(ctx, &entry)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    Ok(entry)
}

fn authority_target_ids(args: &Value) -> Result<Vec<uuid::Uuid>, (i32, String)> {
    let mut target_ids = Vec::new();
    if let Some(target_id) = args.get("target_id").and_then(Value::as_str) {
        target_ids.push(uuid::Uuid::parse_str(target_id).map_err(|e| {
            (
                INVALID_PARAMS,
                format!("target_id is not a valid UUID: {e}"),
            )
        })?);
    }
    if let Some(values) = args.get("target_ids").and_then(Value::as_array) {
        for value in values {
            let Some(raw) = value.as_str() else {
                return Err((
                    INVALID_PARAMS,
                    "target_ids entries must be UUID strings".into(),
                ));
            };
            target_ids.push(uuid::Uuid::parse_str(raw).map_err(|e| {
                (
                    INVALID_PARAMS,
                    format!("target_ids contains an invalid UUID: {e}"),
                )
            })?);
        }
    }
    target_ids.sort_unstable();
    target_ids.dedup();
    if target_ids.is_empty() {
        return Err((
            INVALID_PARAMS,
            "manage_authority requires target_id or target_ids".into(),
        ));
    }
    Ok(target_ids)
}

fn authority_session_id(
    args: &Value,
    ctx: &crate::types::TenantContext,
) -> Result<(uuid::Uuid, &'static str), (i32, String)> {
    let requested_scope = if args.get("global").and_then(Value::as_bool).unwrap_or(false) {
        "global"
    } else {
        args.get("scope")
            .and_then(Value::as_str)
            .unwrap_or("session")
    };
    match requested_scope {
        "global" => Ok((
            crate::scope::tenant_global_session_uuid(ctx.tenant_id),
            "global",
        )),
        "session" => Ok((
            optional_uuid(args, "session_id")?.unwrap_or(uuid::Uuid::nil()),
            "session",
        )),
        other => Err((
            INVALID_PARAMS,
            format!("invalid authority scope: expected session|global, got {other}"),
        )),
    }
}

async fn handle_manage_authority<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let target_ids = authority_target_ids(&args)?;
    let reputation = args
        .get("reputation")
        .and_then(Value::as_f64)
        .map(|score| score.clamp(-1.0, 1.0));
    let pagerank = args
        .get("pagerank")
        .and_then(Value::as_f64)
        .map(|score| score.clamp(0.0, 1.0));
    if reputation.is_none() && pagerank.is_none() {
        return Err((
            INVALID_PARAMS,
            "manage_authority requires reputation and/or pagerank".into(),
        ));
    }
    let (session_id, scope) = authority_session_id(&args, ctx)?;
    let mut updated = Vec::with_capacity(target_ids.len());
    for target_id in target_ids {
        let entry =
            set_memory_authority(storage, ctx, target_id, session_id, reputation, pagerank).await?;
        updated.push(serde_json::json!({
            "target_id": target_id,
            "session_id": session_id,
            "scope": scope,
            "reputation": entry.reputation,
            "pagerank": entry.pagerank
        }));
    }

    Ok(serde_json::json!({
        "updated": updated,
        "count": updated.len(),
        "scope": scope,
        "session_id": session_id,
        "reason": args.get("reason").and_then(Value::as_str),
        "hint": "Authority updated. Future hybrid_search calls that include this scope will boost trusted IDs and demote negative-reputation IDs after relevance scoring."
    }))
}

async fn handle_hybrid_search<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let query = require_str(&args, "query")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let mut base_embedding_status = if embedding.is_some() {
        "provided".to_string()
    } else {
        "unavailable".to_string()
    };
    let limit = optional_retrieval_limit(&args, &["limit"], session)?;
    let offset = args
        .get("offset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(49) as usize;
    let search_limit = (limit + offset).min(50);
    let candidate_limit = args
        .get("candidate_limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .filter(|value| *value > 0)
        .map(|value| value.min(50));
    let min_score = args
        .get("min_score")
        .and_then(Value::as_f64)
        .filter(|value| *value >= 0.0);
    let memory_kinds = {
        let kinds = optional_string_array(&args, "memory_kinds")?
            .into_iter()
            .map(|kind| kind.to_ascii_lowercase())
            .filter(|kind| matches!(kind.as_str(), "episodic" | "procedural" | "semantic"))
            .collect::<Vec<_>>();
        (!kinds.is_empty()).then_some(kinds)
    };
    let filter = crate::hybrid_search::SearchFilter {
        scope: parse_hybrid_search_scope(&args)?,
        entity_types: None,
        tags: None,
        workspace_cwd: args
            .get("workspace_cwd")
            .or_else(|| args.get("cwd"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        candidate_limit,
        min_score,
        memory_kinds,
        datalog_frontier: args.get("datalog_frontier").and_then(Value::as_bool),
        datalog_frontier_seed_limit: args
            .get("datalog_frontier_seed_limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0),
        datalog_frontier_edge_limit: args
            .get("datalog_frontier_edge_limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0),
        datalog_frontier_max_hops: args
            .get("datalog_frontier_max_hops")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0),
        datalog_frontier_min_confidence: args
            .get("datalog_frontier_min_confidence")
            .and_then(Value::as_f64)
            .filter(|value| (0.0..=1.0).contains(value)),
    };
    let query_decomposition_mode = args
        .get("query_decomposition")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let query_task = args
        .get("query_task")
        .and_then(Value::as_str)
        .unwrap_or("general");
    validate_query_decomposition_task(query_task)?;
    let query_variant_limit = args
        .get("query_variant_limit")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 8) as usize;
    let query_embed_variants = args
        .get("query_embed_variants")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let caller_query_variants = optional_string_array(&args, "query_variants")?;
    let mut query_generator_status = "not_requested".to_string();
    let mut query_variants = build_query_variants(
        query,
        if query_decomposition_mode == "llm" {
            "none"
        } else {
            query_decomposition_mode
        },
        query_task,
        &caller_query_variants,
        query_variant_limit,
    )?;
    if query_decomposition_mode == "llm" {
        let judge_config = session.judge_config.lock().await.clone();
        match generate_llm_query_variants(&judge_config, query, query_task, query_variant_limit)
            .await
        {
            Ok(generated) if generated.len() > 1 => {
                query_generator_status = "generated".into();
                let mut seen = query_variants
                    .iter()
                    .map(|variant| variant.to_ascii_lowercase())
                    .collect::<HashSet<_>>();
                for variant in generated.into_iter().skip(1) {
                    if query_variants.len() >= query_variant_limit {
                        break;
                    }
                    push_unique_query(&mut query_variants, &mut seen, variant);
                }
            }
            Ok(_) => {
                query_generator_status = "empty_fallback_heuristic".into();
                query_variants = build_query_variants(
                    query,
                    "heuristic",
                    query_task,
                    &caller_query_variants,
                    query_variant_limit,
                )?;
            }
            Err(e) => {
                tracing::debug!(error = %e, "LLM query decomposition failed; falling back to heuristic");
                query_generator_status = "failed_fallback_heuristic".into();
                query_variants = build_query_variants(
                    query,
                    "heuristic",
                    query_task,
                    &caller_query_variants,
                    query_variant_limit,
                )?;
            }
        }
    }

    // Auto-generate query embedding for ANN search if Ollama is configured.
    let embedding_client = session_embedding_client(session);
    if embedding.is_none()
        && let Some(client) = embedding_client.as_ref()
    {
        match client.embed(query).await {
            Ok(emb) => {
                embedding = Some(emb);
                base_embedding_status = "generated".into();
            }
            Err(e) => {
                tracing::debug!("query embedding generation skipped: {e}");
                base_embedding_status = "failed".into();
            }
        }
    }

    let requested_fusion_profile = args
        .get("fusion_profile")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let auto_fusion_selection = select_auto_fusion_profile(query, &filter);
    let fusion_profile = if requested_fusion_profile == "auto" {
        auto_fusion_selection.profile
    } else {
        requested_fusion_profile
    };
    let mut fusion_config = crate::hybrid_search::FusionConfig::profile(fusion_profile)
        .ok_or_else(|| {
            (
                INVALID_PARAMS,
                format!("unknown fusion_profile: {requested_fusion_profile}"),
            )
        })?;
    if let Some(weights) = args.get("fusion_weights") {
        let Some(object) = weights.as_object() else {
            return Err((INVALID_PARAMS, "fusion_weights must be an object".into()));
        };
        for (key, value) in object {
            let Some(weight) = value.as_f64() else {
                return Err((
                    INVALID_PARAMS,
                    format!("fusion_weights.{key} must be a number"),
                ));
            };
            if !(0.0..=10.0).contains(&weight) {
                return Err((
                    INVALID_PARAMS,
                    format!("fusion_weights.{key} must be between 0 and 10"),
                ));
            }
            if !fusion_config.set_weight(key, weight) {
                return Err((INVALID_PARAMS, format!("unknown fusion weight key: {key}")));
            }
        }
    }

    // The MCP handler recomputes scores when it merges query variants, so
    // `min_score` must be applied after that final score is known. Keep lower
    // search broad enough to contribute candidates; category filters are safe
    // because they do not depend on score scale.
    let mut pre_merge_filter = filter.clone();
    pre_merge_filter.min_score = None;
    let (pagerank_scores, reputation_scores) = match load_authority_score_maps(
        storage,
        ctx,
        session_id,
        filter.scope,
    )
    .await
    {
        Ok(scores) => scores,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "authority score load failed; continuing hybrid_search without authority ranking signals"
            );
            (HashMap::new(), HashMap::new())
        }
    };
    let pagerank_scores_ref = (!pagerank_scores.is_empty()).then_some(&pagerank_scores);
    let reputation_scores_ref = (!reputation_scores.is_empty()).then_some(&reputation_scores);

    let mut query_outputs = Vec::with_capacity(query_variants.len());
    for (idx, variant_query) in query_variants.iter().enumerate() {
        let mut variant_embedding = embedding.clone();
        let mut embedding_status = if idx == 0 {
            base_embedding_status.clone()
        } else if variant_embedding.is_some() {
            "reused".into()
        } else {
            "unavailable".into()
        };
        if idx > 0
            && query_embed_variants
            && let Some(client) = embedding_client.as_ref()
        {
            match client.embed(variant_query).await {
                Ok(emb) => {
                    variant_embedding = Some(emb);
                    embedding_status = "generated".into();
                }
                Err(e) => {
                    tracing::debug!(query = %variant_query, "query variant embedding skipped: {e}");
                    embedding_status = "failed".into();
                }
            }
        }

        let output = crate::hybrid_search::hybrid_search_with_diagnostics(
            storage,
            ctx,
            session_id,
            variant_query,
            variant_embedding.as_deref(),
            search_limit,
            None,
            pagerank_scores_ref,
            reputation_scores_ref,
            &fusion_config,
            Some(&pre_merge_filter),
        )
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
        query_outputs.push(QueryVariantSearchOutput {
            query: variant_query.clone(),
            output,
            embedding_status,
        });
    }
    let candidate_fanout = query_outputs
        .first()
        .map(|variant| variant.output.diagnostics.clone())
        .unwrap_or(crate::hybrid_search::SearchDiagnostics {
            requested_limit: search_limit,
            source_limit: search_limit,
            total_candidates: 0,
            unique_candidates: 0,
            sources: vec![],
        });
    let mut query_merged_output = merge_query_variant_outputs(query_outputs, search_limit);
    query_merged_output.diagnostics.mode = query_decomposition_mode.to_string();
    query_merged_output.diagnostics.task = query_task.to_string();
    query_merged_output.diagnostics.generator_status = query_generator_status;
    let query_decomposition_report = query_merged_output.diagnostics;
    let mut all_results = query_merged_output.results;
    apply_result_filters(
        &mut all_results,
        filter.min_score,
        filter.memory_kinds.as_deref(),
    );
    let chunk_expansion_config = parse_chunk_expansion(&args)?;
    let scoped_sessions =
        crate::hybrid_search::sessions_to_query(session_id, ctx.tenant_id, filter.scope);
    let chunk_expansion_report = apply_chunk_expansion(
        storage,
        ctx,
        &scoped_sessions,
        &mut all_results,
        &chunk_expansion_config,
    )
    .await?;

    let rerank_override = args.get("rerank").and_then(Value::as_bool);
    let rerank_candidate_override = args
        .get("rerank_candidates")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let (mut all_results, reranker_report) = maybe_llm_rerank_results(
        query,
        all_results,
        session,
        rerank_override,
        rerank_candidate_override,
    )
    .await;
    if let Some(cwd) = filter.workspace_cwd.as_deref() {
        for (idx, entity_id) in reranker_report.judged_ids.iter().enumerate() {
            let Some(result) = all_results.iter().find(|result| result.id == *entity_id) else {
                continue;
            };
            if result.result_type != "entity" {
                continue;
            }
            let judgment = reranker_report.judge_scores.get(idx).copied().flatten();
            let score_delta = judgment.map(|score| {
                if score > 0 {
                    0.05
                } else if score < 0 {
                    -0.10
                } else {
                    0.0
                }
            });
            if let Err(e) = update_workspace_feedback(
                storage,
                ctx,
                WorkspaceFeedbackUpdate {
                    session_id,
                    entity_id: *entity_id,
                    cwd,
                    source: &result.source,
                    judge_source: "judge_model",
                    score_delta,
                    judgment,
                },
            )
            .await
            {
                tracing::warn!(entity_id = %entity_id, error = %e, "judge-model workspace feedback update failed");
            }
        }
    }
    apply_llm_judge_authority(&mut all_results, &reranker_report);
    let results: Vec<_> = all_results.into_iter().skip(offset).take(limit).collect();
    let result_count = results.len();

    // Auto-record outcome for episodic feedback loop.
    // Success if any results were found; failure on empty.
    let auto_outcome_succeeded = result_count > 0;
    let auto_outcome_eids: Vec<String> = results.iter().map(|r| r.id.to_string()).collect();
    let auto_query_id = uuid::Uuid::new_v4();
    let search_start = std::time::Instant::now();
    // We already have the results; compute elapsed since the actual search call.
    // Since we can't retroactively measure, we use a small best-effort value.
    let auto_latency_ms = search_start.elapsed().as_millis() as i32;

    // Fire-and-forget auto outcome — don't fail the search if this errors.
    let _ = crate::feedback::record_outcome(
        storage,
        ctx,
        session_id,
        auto_query_id,
        "hybrid_search_auto",
        "simple",
        auto_outcome_succeeded,
        auto_latency_ms.max(1), // avoid 0
        0,
    )
    .await;
    if !auto_outcome_eids.is_empty() {
        // Best-effort warmth boost for returned entities
        for eid in &auto_outcome_eids {
            if let Ok(id) = eid.parse::<uuid::Uuid>() {
                let _ = crate::warmth::apply_outcome_boost(
                    storage,
                    ctx,
                    id,
                    auto_outcome_succeeded,
                    auto_latency_ms,
                )
                .await;
            }
        }
    }
    {
        let mut last_retrieval = session.last_retrieval.lock().await;
        last_retrieval.insert(
            session_id,
            LastRetrievalCall {
                query_id: auto_query_id,
                query: query.to_string(),
                cwd: filter.workspace_cwd.clone(),
                results: results
                    .iter()
                    .filter(|r| r.result_type == "entity")
                    .map(|r| LastRetrievalResult {
                        entity_id: r.id,
                        source: r.source.clone(),
                    })
                    .collect(),
                recorded_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    let hint = if results.is_empty() {
        pick_hint(&[
            "No matches — this topic is new to memory. Good candidate for ingest.",
            "Empty search. Ingest what you're learning in this conversation with ingest.",
            "Nothing found. Have you captured the key insights from this session yet?",
        ])
    } else {
        pick_hint(&[
            "Found prior context. After judging it, call record_feedback with scores like [1,\"-\",-1,0] so cwd-specific reranking improves.",
            "Prior context found. Use record_feedback: 1=helpful, -1=irrelevant/wrong, 0=neutral, \"-\"=cannot judge.",
            "Check if these memories were useful here; send compact feedback with record_feedback scores in result order, using \"-\" for abstain.",
        ])
    };

    let mut response = serde_json::json!({
        "results": results,
        "count": result_count,
        "offset": offset,
        "next_offset": if result_count == limit && offset + limit < 50 { Some(offset + limit) } else { None },
        "hint": hint,
        "reranker": {
            "enabled": reranker_report.enabled,
            "applied": reranker_report.applied,
            "mode": reranker_report.mode,
            "provider": reranker_report.provider,
            "model": reranker_report.model,
            "candidate_count": reranker_report.candidate_count,
            "returned_ids": reranker_report.returned_ids,
            "judged_ids": reranker_report.judged_ids,
            "judge_scores": judge_scores_for_response(&reranker_report.judge_scores),
            "score_sum": reranker_report.score_sum,
            "abstentions": reranker_report.abstentions,
            "batches": reranker_report.batches.iter().map(|batch| serde_json::json!({
                "start_rank": batch.start_rank,
                "candidate_count": batch.candidate_count,
                "returned_ids": batch.returned_ids,
                "judge_scores": judge_scores_for_response(&batch.judge_scores),
                "score_sum": batch.score_sum,
                "abstentions": batch.abstentions,
                "error": batch.error,
            })).collect::<Vec<_>>(),
            "error": reranker_report.error,
        },
        "candidate_fanout": candidate_fanout,
        "query_decomposition": query_decomposition_report,
        "chunk_expansion": chunk_expansion_report,
        "fusion": {
            "profile": fusion_profile,
            "requested_profile": requested_fusion_profile,
            "auto_intent": if requested_fusion_profile == "auto" { auto_fusion_selection.intent } else { "explicit" },
            "weights": fusion_config,
        },
    });
    if result_count == 0 {
        response["_hint"] = serde_json::json!(
            "No results. Try retrieve_entities with a different name spelling, or check if the information has been stored yet."
        );
    } else if result_count < 3 {
        response["_hint"] = serde_json::json!(
            "Few results found. Try recursive_explore for multi-pass decomposed search, or spread_activation for broader graph-based discovery."
        );
    } else if result_count == limit && offset + limit < 50 {
        response["_hint"] = serde_json::json!(
            "After judging this page, call record_feedback with scores like [1,\"-\",-1,0]. If all useful-looking items are -1, 0, or \"-\", call hybrid_search again with next_offset."
        );
    }
    Ok(response)
}

// --- Dream consolidation handler ---

async fn handle_run_consolidation<S: crate::storage::Storage>(
    args: Value,
    _storage: &S,
    _ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let queued = queue_session_for_consolidation(session, session_id).await?;
    record_consolidation_queued(session, session_id).await;
    session
        .dirty
        .store(true, std::sync::atomic::Ordering::Relaxed);
    session.last_activity.notify_waiters();

    Ok(serde_json::json!({
        "queued": queued,
        "session_id": session_id.to_string(),
        "run_when": "idle_or_nightly",
        "hint": "Consolidation queued for the background idle/nightly worker; request path is non-blocking."
    }))
}

// --- Enrichment handler ---

async fn handle_enrich_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let operations = args
        .get("operations")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["enrich".into(), "annotate".into(), "lint".into()]);
    let entity_type_filter = args
        .get("entity_types")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let enrich_config = crate::enrich::EnrichRunConfig {
        llm_base_url: session.enrich_llm_url.clone(),
        llm_model: session.enrich_llm_model.clone(),
        operations,
        entity_type_filter,
        force,
        dry_run,
        batch_size: 10,
        embed_provider: session.embed_provider.clone(),
        ollama_base_url: session.ollama_base_url.clone(),
        embed_model: session.embed_model.clone(),
        embed_dimensions: session.embed_dimensions,
    };

    let result = crate::enrich::run_enrichment(storage, ctx, session_id, &enrich_config)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Metrics / stats handlers ---

async fn handle_memory_metrics<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    macro_rules! metric_count {
        ($label:literal, $future:expr) => {
            $future.await.map_err(|e| {
                (
                    INTERNAL_ERROR,
                    format!("memory_metrics {} count failed: {e}", $label),
                )
            })?
        };
    }

    let nil_session_id = uuid::Uuid::nil();
    let global_session_id = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
    let tenant_entity_count = metric_count!(
        "entity",
        storage.entity_count_matching(
            ctx,
            crate::types::EntityListQuery {
                scope: crate::types::EntityListScope::All,
                limit: 0,
                ..Default::default()
            },
        )
    );
    let legacy_nil_entity_count = metric_count!(
        "legacy nil entity",
        storage.entity_count(ctx, nil_session_id)
    );

    let document_chunk_count =
        metric_count!("document chunk", storage.document_chunk_count(ctx, None));
    let legacy_nil_document_chunk_count = metric_count!(
        "legacy nil document chunk",
        storage.document_chunk_count(ctx, Some(nil_session_id))
    );
    let context_segment_count =
        metric_count!("context segment", storage.context_segment_count(ctx, None));
    let legacy_nil_context_segment_count = metric_count!(
        "legacy nil context segment",
        storage.context_segment_count(ctx, Some(nil_session_id))
    );

    let active_fold_count = metric_count!(
        "active fold",
        storage.fold_count_by_status(ctx, crate::types::FoldStatus::Active)
    );
    let folded_count = metric_count!(
        "folded fold",
        storage.fold_count_by_status(ctx, crate::types::FoldStatus::Folded)
    );
    let archived_fold_count = metric_count!(
        "archived fold",
        storage.fold_count_by_status(ctx, crate::types::FoldStatus::Archived)
    );
    let fold_count = active_fold_count + folded_count + archived_fold_count;
    let legacy_nil_fold_count =
        metric_count!("legacy nil fold", storage.fold_count(ctx, nil_session_id));

    let memo_count = metric_count!("memo", storage.memo_count(ctx));
    let temporal_fact_count = metric_count!("temporal fact", storage.temporal_count(ctx));
    let edge_counts = metric_count!("edge bucket", storage.edge_counts_by_bucket(ctx));
    let edge_count: usize = edge_counts.values().sum();
    let legacy_nil_temporal_link_count = metric_count!(
        "legacy nil temporal link",
        storage.temporal_edge_count(ctx, Some(nil_session_id))
    );
    let intention_count = session.intentions.lock().await.list().len();

    let node_count = tenant_entity_count
        + document_chunk_count
        + context_segment_count
        + fold_count
        + memo_count
        + temporal_fact_count;
    let legacy_nil_node_count = legacy_nil_entity_count
        + legacy_nil_document_chunk_count
        + legacy_nil_context_segment_count
        + legacy_nil_fold_count;

    Ok(serde_json::json!({
        "scope": "tenant",
        "tenant_id": ctx.tenant_id.to_string(),
        "node_count": node_count,
        "edge_count": edge_count,
        "nodes": {
            "entities": tenant_entity_count,
            "document_chunks": document_chunk_count,
            "context_segments": context_segment_count,
            "folds": fold_count,
            "memos": memo_count,
            "temporal_facts": temporal_fact_count
        },
        "folds": {
            "active": active_fold_count,
            "folded": folded_count,
            "archived": archived_fold_count
        },
        "edges": edge_counts,
        "runtime": {
            "intention_count": intention_count,
            "retrieval_default_limit": retrieval_default_limit(session)
        },
        "legacy_nil_session": {
            "session_id": nil_session_id.to_string(),
            "included_in_tenant_totals": true,
            "migration_mode": "reported_as_tenant_global_legacy",
            "node_count": legacy_nil_node_count,
            "edge_count": legacy_nil_temporal_link_count,
            "nodes": {
                "entities": legacy_nil_entity_count,
                "document_chunks": legacy_nil_document_chunk_count,
                "context_segments": legacy_nil_context_segment_count,
                "folds": legacy_nil_fold_count
            },
            "edges": {
                "temporal_links": legacy_nil_temporal_link_count
            }
        },
        "sessions": {
            "tenant_global_session_id": global_session_id.to_string(),
            "legacy_nil_session_id": nil_session_id.to_string()
        }
    }))
}

async fn handle_get_stats<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    // Default to the server-owned runtime/default session — this matches what
    // smart_ingest / hybrid_search / retrieve_entities use after the
    // SessionStart hook configures fmem. Falling back to nil made live stats
    // look empty even when the active session had correctly persisted rows.
    let session_id = optional_uuid(&args, "session_id")?
        .or_else(|| session.effective_default_session_id())
        .unwrap_or(uuid::Uuid::nil());

    let memo_count = storage.memo_count(ctx).await.unwrap_or(0);
    let memo_total_hits = storage.memo_total_hits(ctx).await.unwrap_or(0);
    let memo_hit_rate = if memo_count > 0 {
        memo_total_hits as f64 / memo_count as f64
    } else {
        0.0
    };

    let entity_count = storage.entity_count(ctx, session_id).await.unwrap_or(0);

    let active_fold_count = storage
        .fold_count_by_status(ctx, crate::types::FoldStatus::Active)
        .await
        .unwrap_or(0);
    let folded_count = storage
        .fold_count_by_status(ctx, crate::types::FoldStatus::Folded)
        .await
        .unwrap_or(0);
    let archived_fold_count = storage
        .fold_count_by_status(ctx, crate::types::FoldStatus::Archived)
        .await
        .unwrap_or(0);

    let temporal_fact_count = storage.temporal_count(ctx).await.unwrap_or(0);
    let edge_count = storage.edge_count(ctx).await.unwrap_or(0);
    let intention_count = session.intentions.lock().await.list().len();
    let last_consolidation_status = session
        .last_consolidation_status
        .lock()
        .await
        .get(&session_id)
        .cloned();

    let hint = if entity_count == 0 {
        "Memory is empty. Start ingesting entities, decisions, and patterns with ingest."
            .to_string()
    } else if edge_count == 0 {
        "Entities exist but no connections. Run run_consolidation to discover relationships."
            .to_string()
    } else {
        pick_hint(&[
            "Memory healthy. Remember to ingest new insights from this conversation.",
            "Have you learned anything about the user's preferences? Ingest with ingest.",
            "Project context, architecture decisions, and debugging insights are worth remembering.",
        ])
    };

    let mut response = serde_json::json!({
        "memo_count": memo_count,
        "memo_total_hits": memo_total_hits,
        "memo_hit_rate": memo_hit_rate,
        "entity_count": entity_count,
        "fold_count": active_fold_count + folded_count + archived_fold_count,
        "active_fold_count": active_fold_count,
        "folded_count": folded_count,
        "archived_fold_count": archived_fold_count,
        "temporal_fact_count": temporal_fact_count,
        "edge_count": edge_count,
        "intention_count": intention_count,
        "retrieval_default_limit": retrieval_default_limit(session),
        "last_consolidation_status": last_consolidation_status,
        "hint": hint
    });
    if entity_count > 0 && edge_count == 0 {
        response["_hint"] = serde_json::json!(
            "Entities exist but no connections. Use create_edge to link related entities — connected facts are knowledge, isolated facts are data."
        );
    } else if entity_count > 20 && edge_count < 5 {
        response["_hint"] = serde_json::json!(
            "Many entities but few connections. Run run_consolidation to discover CO_OCCURS relationships automatically."
        );
    }
    Ok(response)
}

async fn handle_migration_status<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
) -> Result<Value, (i32, String)> {
    if !args.as_object().is_none_or(|obj| obj.is_empty()) {
        return Err((
            INVALID_PARAMS,
            "migration_status does not accept parameters".to_string(),
        ));
    }
    serde_json::to_value(
        storage
            .migration_status()
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?,
    )
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

/// `system_describe` — management-safe self-description of this server.
///
/// Read-only. Combines the immutable startup snapshot (`session.system_info`)
/// with fresh, bounded probes of the dependent stores and schema. Secrets are
/// never returned; only their key paths appear in `configuration.redactedKeys`.
async fn handle_system_describe<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    // Only management-safe redaction is supported in v1.
    if let Some(mode) = args.get("redaction").and_then(|v| v.as_str())
        && mode != "management-safe"
    {
        return Err((
            INVALID_PARAMS,
            format!("unsupported redaction mode '{mode}'; only 'management-safe' is supported"),
        ));
    }

    if let Some(caller) = args.get("caller").and_then(|v| v.as_object()) {
        let name = caller.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let version = caller
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        tracing::info!(caller = name, caller_version = version, "describe called");
    }

    let include: Option<Vec<String>> = args.get("include").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    let sections = crate::system_describe::SectionSet::from_include(include.as_deref());

    let tool_names = if sections.capabilities {
        tool_definitions(&session.entity_types)
            .into_iter()
            .map(|t| t.name)
            .collect()
    } else {
        Vec::new()
    };

    // Statistics are scoped to a session; default to the nil session like
    // get_stats so they report the same data the agent loop sees.
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let intention_count = if sections.statistics {
        session.intentions.lock().await.list().len()
    } else {
        0
    };

    let descriptor =
        crate::system_describe::build_descriptor(crate::system_describe::DescribeRequest {
            info: &session.system_info,
            storage,
            ctx,
            graph: session.graph.as_deref(),
            tool_names,
            session_id,
            intention_count,
            sections,
        })
        .await;

    serde_json::to_value(descriptor).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

/// `forget` — two-phase candidate-confirmed forgetting.
///
/// Propose (no `forget_token`): searches candidates, returns a signed token +
/// blast radius, mutates nothing. Confirm (`forget_token` + `confirm: true`):
/// retracts (default, reversible) or hard-deletes the explicitly selected ids.
async fn collect_forget_dependent_authority_targets<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    token: &crate::forget::ForgetToken,
    selected_ids: &[uuid::Uuid],
) -> Result<HashSet<uuid::Uuid>, (i32, String)> {
    let selected = selected_ids.iter().copied().collect::<HashSet<_>>();
    let mut dependents = HashSet::new();
    for candidate in token
        .candidates
        .iter()
        .filter(|candidate| selected.contains(&candidate.object_id))
    {
        let inbound = storage
            .typed_edge_list_to(ctx, candidate.session_id, candidate.object_id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
        for edge in inbound {
            if !selected.contains(&edge.src_id) {
                dependents.insert(edge.src_id);
            }
        }
    }
    Ok(dependents)
}

async fn apply_forget_authority<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    token: &crate::forget::ForgetToken,
    forgotten: &[crate::forget::ForgottenItem],
    dependent_ids: &HashSet<uuid::Uuid>,
) -> Result<Value, (i32, String)> {
    let sessions = token
        .candidates
        .iter()
        .map(|candidate| (candidate.object_id, candidate.session_id))
        .collect::<HashMap<_, _>>();
    let mut forgotten_updated = Vec::new();
    for item in forgotten {
        let session_id = sessions
            .get(&item.id)
            .copied()
            .unwrap_or_else(uuid::Uuid::nil);
        set_memory_authority(storage, ctx, item.id, session_id, Some(-1.0), Some(0.0)).await?;
        forgotten_updated.push(item.id);
    }

    let mut dependent_updated = Vec::new();
    for dependent_id in dependent_ids {
        set_memory_authority(
            storage,
            ctx,
            *dependent_id,
            uuid::Uuid::nil(),
            Some(-0.35),
            None,
        )
        .await?;
        dependent_updated.push(*dependent_id);
    }
    dependent_updated.sort_unstable();

    Ok(serde_json::json!({
        "forgotten_hard_negative": forgotten_updated,
        "dependents_demoted": dependent_updated,
        "dependent_reputation": -0.35
    }))
}

async fn handle_forget<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let now = chrono::Utc::now();
    let key = session.forget_token_key.as_slice();
    let cfg = session.forget.lock().await.clone();

    let token = args.get("forget_token").and_then(|v| v.as_str());
    let confirming = args
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Some(token_str) = token.filter(|_| confirming) {
        let selected: Vec<uuid::Uuid> = args
            .get("selected_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(|s| uuid::Uuid::parse_str(s).ok())
                    .collect()
            })
            .unwrap_or_default();
        if selected.is_empty() {
            return Err((
                INVALID_PARAMS,
                "confirm requires a non-empty selected_ids drawn from the proposed candidates"
                    .into(),
            ));
        }
        let mode = match args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("retract")
        {
            "retract" => crate::forget::ForgetMode::Retract,
            "hard" => crate::forget::ForgetMode::Hard,
            other => {
                return Err((
                    INVALID_PARAMS,
                    format!("invalid mode '{other}'; use 'retract' (default) or 'hard'"),
                ));
            }
        };
        let ack = args
            .get("acknowledge_high_impact")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        let actor = args
            .get("actor")
            .and_then(|v| v.as_str())
            .unwrap_or(ctx.session_origin.as_str());
        let authority_token = crate::forget::decode(token_str, key, cfg.token_ttl_seconds, now)
            .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
        let dependent_authority_targets =
            collect_forget_dependent_authority_targets(storage, ctx, &authority_token, &selected)
                .await?;
        let result = crate::forget::confirm(
            storage,
            ctx,
            token_str,
            &selected,
            mode,
            ack,
            reason,
            actor,
            key,
            cfg.token_ttl_seconds,
            cfg.retract_purge_days,
            cfg.high_impact_edge_threshold,
            now,
        )
        .await
        .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
        let authority = apply_forget_authority(
            storage,
            ctx,
            &authority_token,
            &result.forgotten,
            &dependent_authority_targets,
        )
        .await?;
        let mut value =
            serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("authority".into(), authority);
            obj.insert(
                "hint".into(),
                Value::String(
                    "Forget confirmed. Forgotten IDs are now hard-negative authority; inbound dependents were demoted for review."
                        .into(),
                ),
            );
        }
        return Ok(value);
    }

    // Propose phase (read-only).
    let query = args.get("query").and_then(|v| v.as_str()).ok_or((
        INVALID_PARAMS,
        "forget requires 'query' to propose candidates, or 'forget_token' + 'confirm: true' to \
         execute a prior proposal"
            .to_string(),
    ))?;
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let scope: Vec<String> = args
        .get("scope")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(cfg.candidate_limit);
    let result = crate::forget::propose(
        storage,
        ctx,
        session_id,
        query,
        &scope,
        limit,
        cfg.candidate_max,
        cfg.high_impact_edge_threshold,
        key,
        now,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let mut value = serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "hint".into(),
            Value::String(
                "Review candidates and blast radius with the user. Only call forget again with confirm:true for user-approved selected_ids."
                    .into(),
            ),
        );
    }
    Ok(value)
}

/// `restore_forgotten` — reverse a retraction (un-retract an entity).
async fn handle_restore_forgotten<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let entity_id = optional_uuid(&args, "entity_id")?.ok_or((
        INVALID_PARAMS,
        "restore_forgotten requires 'entity_id'".into(),
    ))?;
    let restored = crate::forget::restore(storage, ctx, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    Ok(serde_json::json!({
        "restored": restored,
        "entity_id": entity_id.to_string(),
        "hint": if restored {
            "Entity restored to its prior state. Note: edges removed at forget time are not auto-recreated in v1."
        } else {
            "No active retraction found for this entity (already restored, never retracted, or purged)."
        }
    }))
}

async fn handle_count_entities_by_type<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let start = std::time::Instant::now();
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let rows = storage
        .entity_counts_by_type_and_state(ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let mut total = 0usize;
    let mut by_entity_type = serde_json::Map::new();
    let mut by_state = serde_json::Map::new();
    let mut by_type_and_state: std::collections::BTreeMap<String, serde_json::Map<String, Value>> =
        std::collections::BTreeMap::new();

    for row in rows {
        total += row.count;

        let type_entry = by_entity_type
            .entry(row.entity_type.clone())
            .or_insert_with(|| serde_json::json!(0));
        let type_total = type_entry.as_u64().unwrap_or(0) + row.count as u64;
        *type_entry = serde_json::json!(type_total);

        let state_key = row.state.to_string();
        let state_entry = by_state
            .entry(state_key.clone())
            .or_insert_with(|| serde_json::json!(0));
        let state_total = state_entry.as_u64().unwrap_or(0) + row.count as u64;
        *state_entry = serde_json::json!(state_total);

        by_type_and_state
            .entry(row.entity_type)
            .or_default()
            .insert(state_key, serde_json::json!(row.count));
    }

    let sum_by_type: usize = by_entity_type
        .values()
        .map(|v| v.as_u64().unwrap_or(0) as usize)
        .sum();
    let sum_by_state: usize = by_state
        .values()
        .map(|v| v.as_u64().unwrap_or(0) as usize)
        .sum();
    let sum_joint: usize = by_type_and_state
        .values()
        .flat_map(|inner| inner.values())
        .map(|v| v.as_u64().unwrap_or(0) as usize)
        .sum();

    if total != sum_by_type || total != sum_by_state || total != sum_joint {
        tracing::error!(
            total,
            sum_by_type,
            sum_by_state,
            sum_joint,
            "count_entities_by_type invariant mismatch"
        );
        return Err((
            INTERNAL_ERROR,
            "count_entities_by_type invariant mismatch".to_string(),
        ));
    }
    debug_assert_eq!(total, sum_by_type);
    debug_assert_eq!(total, sum_by_state);
    debug_assert_eq!(total, sum_joint);

    Ok(serde_json::json!({
        "total": total,
        "by_entity_type": by_entity_type,
        "by_state": by_state,
        "by_type_and_state": by_type_and_state,
        "duration_ms": start.elapsed().as_millis() as u64,
    }))
}

// --- Spreading activation handler ---

async fn handle_find_memory_chain<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let source = require_uuid(&args, "source")?;
    let destination = require_uuid(&args, "destination")?;
    let max_hops = args
        .get("max_hops")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(5);

    let chain = crate::chains::find_chain(storage, ctx, session_id, source, destination, max_hops)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    match chain {
        Some(ref c) => {
            // Warmth boost for entities on the path (fire-and-forget)
            let rmh_config = crate::config::RmhConfig::default();
            for step in &c.steps {
                let _ = crate::warmth::boost_on_access(
                    storage,
                    ctx,
                    step.entity_id,
                    session_id,
                    &crate::types::DecayZone::Knowledge,
                    &rmh_config,
                )
                .await;
            }
            serde_json::to_value(c).map_err(|e| (INTERNAL_ERROR, e.to_string()))
        }
        None => Ok(serde_json::json!({
            "found": false,
            "source": source.to_string(),
            "destination": destination.to_string(),
            "message": "No path found within max_hops"
        })),
    }
}

async fn handle_predict_needed(
    args: Value,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let _session_id = optional_uuid(&args, "session_id")?;
    let threshold = args
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.3);
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    if !(0.0..=1.0).contains(&threshold) {
        return Err((
            INVALID_PARAMS,
            "threshold must be between 0.0 and 1.0".into(),
        ));
    }
    if !(1..=20).contains(&limit) {
        return Err((INVALID_PARAMS, "limit must be between 1 and 20".into()));
    }

    let tracker = session.retrieval_tracker.lock().await;
    let recent = tracker.recent_ids(10);
    drop(tracker);

    let co_access = session.co_access.lock().await;
    let predictions = co_access.predict(&recent, threshold, limit);
    drop(co_access);

    Ok(serde_json::json!({
        "predictions": predictions,
        "count": predictions.len(),
        "recent_entity_count": recent.len(),
    }))
}

async fn handle_spread_activation<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let seeds_arr = args
        .get("seeds")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "missing required array: seeds".into()))?;
    let seeds: Vec<uuid::Uuid> = seeds_arr
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or((INVALID_PARAMS, "seed must be a uuid string".into()))
                .and_then(|s| {
                    uuid::Uuid::parse_str(s)
                        .map_err(|e| (INVALID_PARAMS, format!("invalid seed uuid: {e}")))
                })
        })
        .collect::<Result<_, _>>()?;

    let max_hops = args
        .get("max_hops")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(2);
    let decay = args.get("decay").and_then(|v| v.as_f64()).unwrap_or(0.7);
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(10);

    let results = crate::spreading::spread(
        storage,
        ctx,
        &seeds,
        Some(session_id),
        max_hops,
        decay,
        limit,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Warmth boost for seeds and top activated results (fire-and-forget)
    let rmh_config = crate::config::RmhConfig::default();
    for seed_id in &seeds {
        let _ = crate::warmth::boost_on_access(
            storage,
            ctx,
            *seed_id,
            session_id,
            &crate::types::DecayZone::Knowledge,
            &rmh_config,
        )
        .await;
    }
    for activated in &results {
        let _ = crate::warmth::boost_on_access(
            storage,
            ctx,
            activated.entity_id,
            session_id,
            &crate::types::DecayZone::Knowledge,
            &rmh_config,
        )
        .await;
    }

    Ok(serde_json::json!({
        "activated": results,
        "count": results.len()
    }))
}

// --- Recursive explore / Datalog / Rule handlers ---

async fn handle_recursive_explore<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let query = require_str(&args, "query")?;
    let session_id = optional_uuid(&args, "session_id")?
        .or_else(|| session.effective_default_session_id())
        .unwrap_or(uuid::Uuid::nil());

    // Parse optional embedding
    let embedding: Option<Vec<f32>> = args.get("embedding").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect()
    });
    let embedding_ref = embedding.as_deref();

    let mut rmh_config = crate::config::RmhConfig::default();
    if let Some(mp) = args.get("max_passes").and_then(|v| v.as_u64()) {
        rmh_config.max_explore_passes = mp as usize;
    }
    if let Some(ct) = args.get("convergence_threshold").and_then(|v| v.as_f64()) {
        rmh_config.convergence_threshold = ct;
    }
    let datalog_config = crate::config::DatalogConfig::default();

    let result = crate::recursive_explore::explore(
        storage,
        ctx,
        session_id,
        query,
        embedding_ref,
        &rmh_config,
        &datalog_config,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
    let results: Vec<Value> = result
        .results
        .iter()
        .take(limit)
        .map(|r| {
            serde_json::json!({
                "id": r.id.to_string(),
                "source": r.source,
                "content": r.content,
                "score": r.score,
                "result_type": r.result_type,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "results": results,
        "count": results.len(),
        "sub_queries": result.sub_queries.iter().map(|sq| {
            serde_json::json!({ "query": sq.query_text, "reasoning": sq.reasoning })
        }).collect::<Vec<_>>(),
        "passes": result.passes,
        "converged": result.converged,
        "derived_facts_count": result.derived_facts_count,
        "hint": if results.is_empty() {
            "No results found. Try ingest to add entities first."
        } else {
            "Recursive exploration found connected knowledge clusters."
        }
    }))
}

async fn handle_query_derived<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let predicate = require_str(&args, "predicate")?;
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let config = crate::config::DatalogConfig::default();
    let facts = crate::datalog::query_predicate(storage, ctx, session_id, predicate, &config)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let results: Vec<Value> = facts
        .iter()
        .map(|f| {
            serde_json::json!({
                "src_id": f.src_id,
                "predicate": f.pred,
                "dst_id": f.dst_id,
                "confidence": f.confidence,
                "rule_id": f.rule_id,
                "support_count": f.support_count,
                "provenance": f.provenance.iter().map(|p| {
                    serde_json::json!({
                        "parent_src": p.parent_src,
                        "parent_pred": p.parent_pred,
                        "parent_dst": p.parent_dst,
                        "parent_kind": p.parent_kind,
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "predicate": predicate,
        "derived_facts": results,
        "count": results.len(),
    }))
}

async fn handle_manage_rules<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let action = require_str(&args, "action")?;

    match action {
        "list" => {
            let family = args.get("family").and_then(|v| v.as_str()).unwrap_or("*");
            let source = args
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or(if family == "*" { "builtin" } else { "registry" });

            let results: Vec<Value> = match source {
                "builtin" => crate::datalog::synthetic_builtin_rule_entries(ctx.tenant_id)
                    .into_iter()
                    .filter(|r| family == "*" || r.family == family)
                    .map(|r| {
                        serde_json::json!({
                            "source": "builtin",
                            "rule_id": r.rule_id,
                            "version": r.version,
                            "name": r.name,
                            "family": r.family,
                            "state": r.state.to_string(),
                            "rule_body": r.rule_body,
                            "rule_weight": r.rule_weight,
                        })
                    })
                    .collect(),
                "registry" => {
                    let stored_rules = if family == "*" {
                        storage
                            .rule_list_active(ctx, crate::types::RuleState::Active)
                            .await
                            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                    } else {
                        storage
                            .rule_list_family(ctx, family, crate::types::RuleState::Active)
                            .await
                            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                    };
                    let mut rows = Vec::with_capacity(stored_rules.len());
                    for r in stored_rules {
                        let approval_state = crate::expert_system::approval_state(
                            storage,
                            ctx,
                            crate::types::ArtifactKind::Rule,
                            &r.rule_id,
                        )
                        .await
                        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
                        rows.push(serde_json::json!({
                            "source": "registry",
                            "rule_id": r.rule_id,
                            "version": r.version,
                            "name": r.name,
                            "family": r.family,
                            "state": r.state.to_string(),
                            "approval_state": approval_state.map(|state| state.to_string()),
                            "rule_body": r.rule_body,
                            "rule_weight": r.rule_weight,
                        }));
                    }
                    rows
                }
                "effective" => {
                    crate::datalog::load_effective_rule_entries(storage, ctx, Some(family))
                        .await
                        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                        .into_iter()
                        .map(|rule| {
                            serde_json::json!({
                                "source": match rule.source {
                                    crate::datalog::RuleSource::Builtin => "builtin",
                                    crate::datalog::RuleSource::Registry => "registry",
                                },
                                "rule_id": rule.entry.rule_id,
                                "version": rule.entry.version,
                                "name": rule.entry.name,
                                "family": rule.entry.family,
                                "state": rule.entry.state.to_string(),
                                "rule_body": rule.entry.rule_body,
                                "rule_weight": rule.entry.rule_weight,
                            })
                        })
                        .collect()
                }
                _ => {
                    return Err((
                        INVALID_PARAMS,
                        "Unknown source. Use builtin, registry, or effective.".into(),
                    ));
                }
            };

            Ok(serde_json::json!({
                "action": "list",
                "source": source,
                "rules": results,
                "count": results.len()
            }))
        }
        "get" => {
            let rule_id = require_str(&args, "rule_id")?;
            let rule = storage
                .rule_get(ctx, rule_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            match rule {
                Some(r) => Ok(serde_json::json!({
                    "action": "get",
                    "rule": {
                        "rule_id": r.rule_id, "version": r.version, "name": r.name,
                        "family": r.family, "rule_body": r.rule_body, "rule_weight": r.rule_weight
                    }
                })),
                None => Ok(serde_json::json!({ "action": "get", "rule": null })),
            }
        }
        "put" => {
            let rule_id = require_str(&args, "rule_id")?;
            let rule_body = require_str(&args, "rule_body")?;

            let parsed = crate::datalog::parse_rule(rule_body)
                .map_err(|e| (INVALID_PARAMS, format!("Invalid rule syntax: {e}")))?;
            let family = args
                .get("family")
                .and_then(|v| v.as_str())
                .unwrap_or(&parsed.head.predicate);
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or(rule_id);
            let weight = args
                .get("rule_weight")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0);

            // Get current version to auto-increment
            let version = match storage
                .rule_get(ctx, rule_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
            {
                Some(existing) => existing.version + 1,
                None => 1,
            };

            let entry = crate::types::RuleEntry {
                tenant_id: ctx.tenant_id,
                rule_id: rule_id.to_string(),
                version,
                name: name.to_string(),
                family: family.to_string(),
                state: crate::types::RuleState::Active,
                rule_body: rule_body.to_string(),
                rule_weight: weight,
                incremental: false,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };

            storage
                .rule_put(ctx, &entry)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            let approval = crate::expert_system::record_approval(
                storage,
                ctx,
                crate::types::ArtifactKind::Rule,
                rule_id,
                crate::types::ApprovalDecision::Proposed,
                Some("rule submitted for review".to_string()),
                "rule".to_string(),
                None,
                None,
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            invalidate_predicate_cache(storage, ctx, &parsed.head.predicate).await;

            Ok(serde_json::json!({
                "action": "put",
                "rule_id": rule_id,
                "version": version,
                "approval": approval_json(&approval),
                "hint": "Rule stored. Derived cache invalidated for affected predicate."
            }))
        }
        "deprecate" => {
            let rule_id = require_str(&args, "rule_id")?;
            match storage
                .rule_get(ctx, rule_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
            {
                Some(mut rule) => {
                    rule.state = crate::types::RuleState::Deprecated;
                    rule.updated_at = chrono::Utc::now();
                    storage
                        .rule_put(ctx, &rule)
                        .await
                        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
                    Ok(
                        serde_json::json!({ "action": "deprecate", "rule_id": rule_id, "deprecated": true }),
                    )
                }
                None => Ok(
                    serde_json::json!({ "action": "deprecate", "rule_id": rule_id, "deprecated": false, "error": "Rule not found" }),
                ),
            }
        }
        _ => Err((
            INVALID_PARAMS,
            format!("Unknown action: {action}. Use list/get/put/deprecate."),
        )),
    }
}

async fn invalidate_predicate_cache<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    predicate: &str,
) {
    let mut keys_to_clear = std::collections::BTreeSet::new();
    keys_to_clear.insert(predicate.to_string());

    if let Ok(rows) = storage.derived_cache_list_all(ctx, 10_000).await {
        for row in rows {
            if row.predicate == predicate
                && let Some(key) = row.cache_key
            {
                keys_to_clear.insert(key);
            }
        }
    }

    for key in keys_to_clear {
        let _ = storage.derived_cache_clear(ctx, &key).await;
    }
}

fn approval_json(entry: &crate::types::ApprovalEntry) -> Value {
    serde_json::json!({
        "approval_id": entry.approval_id,
        "artifact_kind": entry.artifact_kind,
        "artifact_ref": entry.artifact_ref,
        "decision": entry.decision,
        "review_note": entry.review_note,
        "reviewer": entry.reviewer,
        "scope": entry.scope,
        "workspace_scope": entry.workspace_scope,
        "session_scope": entry.session_scope,
        "mirror_entity_id": entry.mirror_entity_id,
        "created_at": entry.created_at,
    })
}

fn claim_json(entry: &crate::types::EntityEntry) -> Value {
    serde_json::json!({
        "claim_id": entry.entity_name,
        "claim_entity_id": entry.entity_id,
        "claim_text": entry.properties.get("claim_text").and_then(|v| v.as_str()).unwrap_or(&entry.context_snippet),
        "domain": entry.properties.get("domain").and_then(|v| v.as_str()),
        "status": crate::expert_system::claim_status_from_entity(entry).to_string(),
        "confidence": entry.properties.get("confidence").and_then(|v| v.as_f64()).unwrap_or(entry.confidence),
        "source_ref": entry.properties.get("source_ref"),
        "support_count": entry.properties.get("support_count").and_then(|v| v.as_i64()).unwrap_or_default(),
        "workspace_scope": entry.properties.get("workspace_scope"),
        "session_scope": entry.properties.get("session_scope"),
        "updated_at": entry.updated_at.unwrap_or(entry.created_at),
    })
}

fn alias_json(entry: &crate::types::AliasEntry) -> Value {
    serde_json::json!({
        "alias_id": entry.alias_id,
        "alias_name": entry.alias_name,
        "scope_kind": entry.scope_kind,
        "scope_ref": entry.scope_ref,
        "canonical_tool": entry.canonical_tool,
        "parameter_map": entry.parameter_map,
        "fixed_arguments": entry.fixed_arguments,
        "args_templates": entry.args_templates,
        "status": entry.status,
        "created_at": entry.created_at,
        "updated_at": entry.updated_at,
    })
}

async fn handle_manage_claims<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let action = require_str(&args, "action")?;
    match action {
        "list" => {
            let include_unapproved = args
                .get("include_unapproved")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let session_id = optional_uuid(&args, "session_id")?;
            let mut claims: Vec<crate::types::EntityEntry> = storage
                .entity_list_all(ctx)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                .into_iter()
                .filter(|entry| entry.entity_type == crate::expert_system::CLAIM_ENTITY_TYPE)
                .filter(|entry| session_id.is_none_or(|session_id| entry.session_id == session_id))
                .filter(|entry| {
                    include_unapproved
                        || matches!(
                            crate::expert_system::claim_status_from_entity(entry),
                            crate::types::ClaimStatus::Approved
                        )
                })
                .collect();
            claims.sort_by(|left, right| {
                right
                    .updated_at
                    .unwrap_or(right.created_at)
                    .cmp(&left.updated_at.unwrap_or(left.created_at))
            });
            let rows: Vec<Value> = claims.iter().map(claim_json).collect();
            Ok(serde_json::json!({
                "action": "list",
                "claims": rows,
                "count": rows.len(),
            }))
        }
        "get" => {
            let claim_id = require_str(&args, "claim_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            let claim = storage
                .entity_get_by_id(
                    ctx,
                    session_id,
                    crate::expert_system::claim_entity_id(claim_id),
                )
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({
                "action": "get",
                "claim": claim.as_ref().map(claim_json),
            }))
        }
        "put" => {
            let claim_id = require_str(&args, "claim_id")?;
            let claim_text = require_str(&args, "claim_text")?;
            let domain = args
                .get("domain")
                .and_then(|value| value.as_str())
                .unwrap_or("general");
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            let status = args
                .get("status")
                .and_then(|value| value.as_str())
                .map(crate::expert_system::parse_claim_status)
                .transpose()
                .map_err(|e| (INVALID_PARAMS, e.to_string()))?
                .unwrap_or(crate::types::ClaimStatus::Proposed);
            let confidence = args
                .get("confidence")
                .and_then(|value| value.as_f64())
                .unwrap_or(1.0);
            let support_count = args
                .get("support_count")
                .and_then(|value| value.as_i64())
                .unwrap_or(0) as i32;
            let source_ref = args.get("source_ref").and_then(|value| value.as_str());
            let workspace_scope = args.get("workspace_scope").and_then(|value| value.as_str());

            let claim = crate::expert_system::claim_entity(
                ctx,
                claim_id,
                session_id,
                claim_text,
                domain,
                status,
                confidence,
                source_ref,
                support_count,
                workspace_scope,
            );

            storage
                .entity_put(ctx, &claim)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            let decision = match status {
                crate::types::ClaimStatus::Proposed => crate::types::ApprovalDecision::Proposed,
                crate::types::ClaimStatus::Approved => crate::types::ApprovalDecision::Approved,
                crate::types::ClaimStatus::Rejected => crate::types::ApprovalDecision::Rejected,
            };
            let approval = crate::expert_system::record_approval(
                storage,
                ctx,
                crate::types::ArtifactKind::Claim,
                claim_id,
                decision,
                Some(format!("claim status set to {status}")),
                "claim".to_string(),
                workspace_scope.map(str::to_string),
                Some(session_id),
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            Ok(serde_json::json!({
                "action": "put",
                "claim": claim_json(&claim),
                "approval": approval_json(&approval),
            }))
        }
        _ => Err((
            INVALID_PARAMS,
            format!("Unknown action: {action}. Use list/get/put."),
        )),
    }
}

async fn handle_manage_approvals<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let action = require_str(&args, "action")?;
    let artifact_kind =
        crate::expert_system::parse_artifact_kind(require_str(&args, "artifact_kind")?)
            .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
    let artifact_ref = require_str(&args, "artifact_ref")?;

    match action {
        "list" => {
            let rows = storage
                .approval_list(ctx, &artifact_kind.to_string(), artifact_ref)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            let rows: Vec<Value> = rows.iter().map(approval_json).collect();
            Ok(serde_json::json!({
                "action": "list",
                "approvals": rows,
                "count": rows.len(),
            }))
        }
        "latest" => {
            let latest = storage
                .approval_latest(ctx, &artifact_kind.to_string(), artifact_ref)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({
                "action": "latest",
                "approval": latest.as_ref().map(approval_json),
            }))
        }
        "record" => {
            let decision = args
                .get("decision")
                .and_then(|value| value.as_str())
                .ok_or((INVALID_PARAMS, "missing required string: decision".into()))
                .and_then(|value| {
                    crate::expert_system::parse_approval_decision(value)
                        .map_err(|e| (INVALID_PARAMS, e.to_string()))
                })?;
            let review_note = args
                .get("review_note")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let scope = args
                .get("scope")
                .and_then(|value| value.as_str())
                .unwrap_or("operator")
                .to_string();
            let workspace_scope = args
                .get("workspace_scope")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let session_scope = optional_uuid(&args, "session_scope")?;

            let approval = crate::expert_system::record_approval(
                storage,
                ctx,
                artifact_kind,
                artifact_ref,
                decision,
                review_note,
                scope,
                workspace_scope,
                session_scope,
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            if matches!(artifact_kind, crate::types::ArtifactKind::Claim)
                && let Some(session_id) = session_scope
                && let Some(mut claim) = storage
                    .entity_get_by_id(
                        ctx,
                        session_id,
                        crate::expert_system::claim_entity_id(artifact_ref),
                    )
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
            {
                if let Some(properties) = claim.properties.as_object_mut() {
                    properties.insert("status".into(), Value::String(decision.to_string()));
                }
                claim.updated_at = Some(chrono::Utc::now());
                storage
                    .entity_put(ctx, &claim)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            }

            Ok(serde_json::json!({
                "action": "record",
                "approval": approval_json(&approval),
            }))
        }
        _ => Err((
            INVALID_PARAMS,
            format!("Unknown action: {action}. Use list/latest/record."),
        )),
    }
}

async fn handle_manage_aliases<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let action = require_str(&args, "action")?;
    let alias_name = require_str(&args, "alias_name")?;
    match action {
        "list" => {
            let aliases = storage
                .alias_list(ctx, alias_name)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            let rows: Vec<Value> = aliases.iter().map(alias_json).collect();
            Ok(serde_json::json!({
                "action": "list",
                "aliases": rows,
                "count": rows.len(),
            }))
        }
        "resolve" => {
            let workspace_scope = args.get("workspace_scope").and_then(|value| value.as_str());
            let session_scope = optional_uuid(&args, "session_scope")?;
            let alias = crate::expert_system::resolve_alias(
                storage,
                ctx,
                alias_name,
                workspace_scope,
                session_scope,
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({
                "action": "resolve",
                "alias": alias.as_ref().map(alias_json),
            }))
        }
        "put" => {
            let canonical_tool = require_str(&args, "canonical_tool")?;
            let scope_kind = crate::expert_system::parse_alias_scope_kind(
                args.get("scope_kind")
                    .and_then(|value| value.as_str())
                    .unwrap_or("global"),
            )
            .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
            let session_scope = optional_uuid(&args, "session_scope")?;
            let workspace_scope = args
                .get("workspace_scope")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let scope_ref = match scope_kind {
                crate::types::AliasScopeKind::Global => args
                    .get("scope_ref")
                    .and_then(|value| value.as_str())
                    .unwrap_or("*")
                    .to_string(),
                crate::types::AliasScopeKind::Workspace => workspace_scope
                    .clone()
                    .or_else(|| {
                        args.get("scope_ref")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                    .ok_or((
                        INVALID_PARAMS,
                        "workspace alias requires workspace_scope".into(),
                    ))?,
                crate::types::AliasScopeKind::Session => session_scope
                    .map(|value| value.to_string())
                    .or_else(|| {
                        args.get("scope_ref")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                    .ok_or((
                        INVALID_PARAMS,
                        "session alias requires session_scope".into(),
                    ))?,
            };
            let status = args
                .get("status")
                .and_then(|value| value.as_str())
                .map(crate::expert_system::parse_claim_status)
                .transpose()
                .map_err(|e| (INVALID_PARAMS, e.to_string()))?
                .unwrap_or(crate::types::ClaimStatus::Proposed);
            let now = chrono::Utc::now();
            let alias = crate::types::AliasEntry {
                tenant_id: ctx.tenant_id,
                alias_id: uuid::Uuid::now_v7(),
                alias_name: alias_name.to_string(),
                scope_kind,
                scope_ref,
                canonical_tool: canonical_tool.to_string(),
                parameter_map: args
                    .get("parameter_map")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                fixed_arguments: args
                    .get("fixed_arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                args_templates: args
                    .get("args_templates")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                status,
                created_at: now,
                updated_at: now,
            };

            storage
                .alias_put(ctx, &alias)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            storage
                .entity_put(
                    ctx,
                    &crate::expert_system::alias_mirror_entity(&alias, session_scope),
                )
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            let artifact_ref = format!("{alias_name}:{}:{}", alias.scope_kind, alias.scope_ref);
            let decision = match status {
                crate::types::ClaimStatus::Proposed => crate::types::ApprovalDecision::Proposed,
                crate::types::ClaimStatus::Approved => crate::types::ApprovalDecision::Approved,
                crate::types::ClaimStatus::Rejected => crate::types::ApprovalDecision::Rejected,
            };
            let approval = crate::expert_system::record_approval(
                storage,
                ctx,
                crate::types::ArtifactKind::Alias,
                &artifact_ref,
                decision,
                Some(format!("alias status set to {status}")),
                "alias".to_string(),
                workspace_scope.clone(),
                session_scope,
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            Ok(serde_json::json!({
                "action": "put",
                "alias": alias_json(&alias),
                "approval": approval_json(&approval),
            }))
        }
        _ => Err((
            INVALID_PARAMS,
            format!("Unknown action: {action}. Use list/put/resolve."),
        )),
    }
}

async fn handle_explain_derived<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let predicate = require_str(&args, "predicate")?;
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let src_id = args.get("src_id").and_then(|value| value.as_str());
    let dst_id = args.get("dst_id").and_then(|value| value.as_str());
    let limit = args
        .get("limit")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or(16)
        .clamp(1, 64);

    let start = std::time::Instant::now();
    let derived = crate::datalog::query_predicate(
        storage,
        ctx,
        session_id,
        predicate,
        &crate::config::DatalogConfig::default(),
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let explanations: Vec<Value> = derived
        .iter()
        .filter(|fact| src_id.is_none_or(|src| fact.src_id == src))
        .filter(|fact| dst_id.is_none_or(|dst| fact.dst_id == dst))
        .map(|fact| {
            let truncated = fact.provenance.len() > limit;
            let chain: Vec<Value> = fact
                .provenance
                .iter()
                .take(limit)
                .map(|step| {
                    serde_json::json!({
                        "parent_src": step.parent_src,
                        "parent_pred": step.parent_pred,
                        "parent_dst": step.parent_dst,
                        "parent_kind": step.parent_kind,
                    })
                })
                .collect();
            serde_json::json!({
                "predicate": fact.pred,
                "src_id": fact.src_id,
                "dst_id": fact.dst_id,
                "rule_id": fact.rule_id,
                "support_count": fact.support_count,
                "support_chain": chain,
                "approval_state": Value::Null,
                "fanout": fact.provenance.len(),
                "truncated": truncated,
            })
        })
        .collect();
    let elapsed_ms = start.elapsed().as_millis() as i64;
    let metric_predicate = format!("explain:{predicate}");
    storage
        .heat_record(ctx, &metric_predicate, false, Some(elapsed_ms))
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "predicate": predicate,
        "explanations": explanations,
        "count": explanations.len(),
        "latency_ms": elapsed_ms,
        "limit": limit,
    }))
}

async fn handle_get_effective_rule_set<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let family = args.get("family").and_then(|value| value.as_str());
    let rules = crate::datalog::load_effective_rule_entries(storage, ctx, family)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let rows: Vec<Value> = rules
        .iter()
        .map(|entry| {
            serde_json::json!({
                "source": match entry.source {
                    crate::datalog::RuleSource::Builtin => "builtin",
                    crate::datalog::RuleSource::Registry => "registry",
                },
                "rule_id": entry.entry.rule_id,
                "family": entry.entry.family,
                "name": entry.entry.name,
                "version": entry.entry.version,
                "state": entry.entry.state,
                "rule_body": entry.entry.rule_body,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "family": family,
        "rules": rows,
        "count": rows.len(),
    }))
}

async fn handle_promote_predicate<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let predicate = require_str(&args, "predicate")?;
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let config = crate::config::PromotionConfig::default();

    let count = crate::promotion::batch_materialize(storage, ctx, session_id, predicate, &config)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "predicate": predicate,
        "materialized_count": count,
        "status": if count > 0 { "promoted" } else { "no_facts_to_materialize" },
        "hint": if count > 0 {
            "Predicate promoted to durable storage. Future queries will use materialized facts."
        } else {
            "No derived facts found for this predicate. Run recursive_explore first to populate the graph."
        }
    }))
}

// --- Typed edge handlers ---

fn edge_write_timeout_message(operation: &str, budget: std::time::Duration) -> String {
    format!(
        "{operation} timed out after {}s while writing typed_edges. \
         Ferrosa may still be warming ANN indexes and blocking CQL; retry after \
         /healthz/ready is healthy or after a successful get_stats call.",
        budget.as_secs()
    )
}

async fn edge_write_with_timeout<T, F>(operation: &str, fut: F) -> Result<T, (i32, String)>
where
    F: Future<Output = anyhow::Result<T>>,
{
    edge_write_with_timeout_budget(operation, EDGE_WRITE_TIMEOUT, fut).await
}

async fn edge_write_with_timeout_budget<T, F>(
    operation: &str,
    budget: std::time::Duration,
    fut: F,
) -> Result<T, (i32, String)>
where
    F: Future<Output = anyhow::Result<T>>,
{
    match tokio::time::timeout(budget, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err((INTERNAL_ERROR, e.to_string())),
        Err(_) => Err((
            INTERNAL_ERROR,
            edge_write_timeout_message(operation, budget),
        )),
    }
}

async fn handle_create_edge<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let src_id = require_uuid(&args, "src_entity_id")?;
    let dst_id = require_uuid(&args, "dst_entity_id")?;
    let edge_type = require_str(&args, "edge_type")?;
    let weight = args.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0);
    let metadata = args
        .get("metadata")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    edge_write_with_timeout(
        "create_edge",
        crate::graph_write::create_typed_edge(
            storage, ctx, session_id, src_id, edge_type, dst_id, weight, metadata,
        ),
    )
    .await?;

    session.dirty.store(true, Ordering::Relaxed);
    session.last_activity.notify_waiters();

    Ok(serde_json::json!({
        "created": true,
        "src_id": src_id.to_string(),
        "edge_type": edge_type,
        "dst_id": dst_id.to_string(),
        "weight": weight,
    }))
}

async fn handle_batch_create_edges<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let edges = args
        .get("edges")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "edges must be an array".to_string()))?;

    if edges.len() > 200 {
        return Err((
            INVALID_PARAMS,
            format!("edges array length {} exceeds maximum of 200", edges.len()),
        ));
    }

    let mut created: usize = 0;
    let mut errors: usize = 0;
    let mut parsed_edges = Vec::with_capacity(edges.len());

    for edge_json in edges {
        let src_id = match edge_json
            .get("src_entity_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        {
            Some(id) => id,
            None => {
                errors += 1;
                continue;
            }
        };
        let dst_id = match edge_json
            .get("dst_entity_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        {
            Some(id) => id,
            None => {
                errors += 1;
                continue;
            }
        };
        let edge_type = match edge_json.get("edge_type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                errors += 1;
                continue;
            }
        };
        let weight = edge_json
            .get("weight")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0);

        parsed_edges.push((src_id, edge_type.to_string(), dst_id, weight));
    }

    let writes = parsed_edges
        .into_iter()
        .map(|(src_id, edge_type, dst_id, weight)| async move {
            edge_write_with_timeout(
                "batch_create_edges",
                crate::graph_write::create_typed_edge(
                    storage, ctx, session_id, src_id, edge_type, dst_id, weight, None,
                ),
            )
            .await
        });

    let mut timeout_errors = 0usize;
    let mut last_error: Option<String> = None;
    for result in join_all(writes).await {
        match result {
            Ok(_) => created += 1,
            Err((_, message)) => {
                if message.contains("timed out after") {
                    timeout_errors += 1;
                }
                if last_error.is_none() {
                    last_error = Some(message);
                }
                errors += 1;
            }
        }
    }

    session.dirty.store(true, Ordering::Relaxed);
    session.last_activity.notify_waiters();

    Ok(serde_json::json!({
        "created": created,
        "errors": errors,
        "timeout_errors": timeout_errors,
        "total": edges.len(),
        "last_error": last_error,
        "_hint": if timeout_errors > 0 {
            "Some edge writes timed out while CQL was blocked. Ferrosa may still be warming ANN indexes; retry after /healthz/ready is healthy or get_stats succeeds."
        } else {
            "Batch edge creation completed."
        },
    }))
}

async fn handle_batch_update_edges<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let edges = args
        .get("edges")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "edges must be an array".to_string()))?;

    if edges.len() > 200 {
        return Err((
            INVALID_PARAMS,
            format!("edges array length {} exceeds maximum of 200", edges.len()),
        ));
    }

    let jobs = edges
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, edge_json)| async move {
            let Some(edge_row) = edge_json.as_object() else {
                return BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Error,
                    result: serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": format!("batch_update_edges[{idx}] must be an object")
                    }),
                };
            };

            let src_id = match edge_row
                .get("src_entity_id")
                .and_then(|v| v.as_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
            {
                Some(id) => id,
                None => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": "invalid or missing src_entity_id"
                        }),
                    };
                }
            };
            let dst_id = match edge_json
                .get("dst_entity_id")
                .and_then(|v| v.as_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
            {
                Some(id) => id,
                None => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": "invalid or missing dst_entity_id"
                        }),
                    };
                }
            };
            let edge_type = match edge_row.get("edge_type").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": "missing edge_type"
                        }),
                    };
                }
            };

            let metadata_override_set = edge_row.contains_key("metadata");
            let weight_override_set = edge_row.contains_key("weight");

            let weight_override = if weight_override_set {
                match edge_row.get("weight").and_then(|v| v.as_f64()) {
                    Some(weight) => Some(weight),
                    None => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "status": "error",
                            "reason": "weight must be a number"
                            }),
                        };
                    }
                }
            } else {
                None
            };

            let metadata_override = if metadata_override_set {
                match edge_row.get("metadata").unwrap() {
                    Value::String(value) => Some(value.to_string()),
                    Value::Null => None,
                    _ => {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                            "index": idx,
                            "status": "error",
                            "reason": "metadata must be a string"
                            }),
                        };
                    }
                }
            } else {
                None
            };

            if weight_override_set && !weight_override.unwrap().is_finite() {
                return BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Error,
                    result: serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": "weight must be finite"
                    }),
                };
            }

            let existing_edges = match storage.typed_edge_list_from(ctx, session_id, src_id).await {
                Ok(edges) => edges,
                Err(err) => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Error,
                        result: serde_json::json!({
                            "index": idx,
                            "status": "error",
                            "reason": err.to_string()
                        }),
                    };
                }
            };
            let existing = existing_edges
                .into_iter()
                .find(|edge| edge.dst_id == dst_id && edge.edge_type == edge_type);

            match existing {
                None => {
                    let final_weight = weight_override.unwrap_or(1.0);
                    if !weight_override_set && !metadata_override_set {
                        // No existing edge and no replacement data => we can only upsert defaults.
                        // Preserve current semantics and allow this path to act as a create.
                    }
                    let final_metadata = metadata_override;

                    match crate::graph_write::create_typed_edge(
                        storage,
                        ctx,
                        session_id,
                        src_id,
                        edge_type,
                        dst_id,
                        final_weight,
                        final_metadata,
                    )
                    .await
                    {
                        Ok(created) => {
                            session.dirty.store(true, Ordering::Relaxed);
                            session.last_activity.notify_waiters();
                            BatchMutationOutcome {
                                index: idx,
                                kind: BatchMutationKind::Upserted,
                                result: serde_json::json!({
                                    "index": idx,
                                    "status": "upserted",
                                    "created_at": created.created_at.to_rfc3339(),
                                    "weight": created.weight,
                                }),
                            }
                        }
                        Err(err) => BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                                "index": idx,
                                "status": "error",
                                "reason": err.to_string()
                            }),
                        },
                    }
                }
                Some(existing_edge) => {
                    let final_weight = weight_override.unwrap_or(existing_edge.weight);
                    let final_metadata = if metadata_override_set {
                        metadata_override
                    } else {
                        existing_edge.metadata.clone()
                    };

                    let unchanged_weight =
                        !weight_override_set || final_weight == existing_edge.weight;
                    let unchanged_metadata =
                        !metadata_override_set || final_metadata == existing_edge.metadata;
                    if unchanged_weight && unchanged_metadata {
                        return BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Unchanged,
                            result: serde_json::json!({
                                "index": idx,
                                "status": "unchanged"
                            }),
                        };
                    }

                    match crate::graph_write::create_typed_edge(
                        storage,
                        ctx,
                        session_id,
                        src_id,
                        edge_type,
                        dst_id,
                        final_weight,
                        final_metadata,
                    )
                    .await
                    {
                        Ok(updated) => {
                            session.dirty.store(true, Ordering::Relaxed);
                            session.last_activity.notify_waiters();
                            BatchMutationOutcome {
                                index: idx,
                                kind: BatchMutationKind::Upserted,
                                result: serde_json::json!({
                                    "index": idx,
                                    "status": "updated",
                                    "weight": updated.weight
                                }),
                            }
                        }
                        Err(err) => BatchMutationOutcome {
                            index: idx,
                            kind: BatchMutationKind::Error,
                            result: serde_json::json!({
                                "index": idx,
                                "status": "error",
                                "reason": err.to_string()
                            }),
                        },
                    }
                }
            }
        });

    let outcomes: Vec<BatchMutationOutcome> = stream::iter(jobs)
        .buffer_unordered(BATCH_MUTATION_CONCURRENCY)
        .collect()
        .await;

    let mut upserted: usize = 0;
    let mut unchanged: usize = 0;
    let mut errors: usize = 0;
    for outcome in &outcomes {
        match outcome.kind {
            BatchMutationKind::Upserted => upserted += 1,
            BatchMutationKind::Unchanged => unchanged += 1,
            BatchMutationKind::Error => errors += 1,
            BatchMutationKind::Updated
            | BatchMutationKind::NotFound
            | BatchMutationKind::Deleted
            | BatchMutationKind::Missing
            | BatchMutationKind::Invalid => {}
        }
    }
    let results = ordered_batch_results(outcomes);

    Ok(serde_json::json!({
        "upserted": upserted,
        "unchanged": unchanged,
        "errors": errors,
        "total": edges.len(),
        "results": results,
    }))
}

async fn handle_batch_delete_edges<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let edges = args
        .get("edges")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "edges must be an array".to_string()))?;

    if edges.len() > 200 {
        return Err((
            INVALID_PARAMS,
            format!("edges array length {} exceeds maximum of 200", edges.len()),
        ));
    }

    let jobs = edges
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, edge_json)| async move {
            let Some(edge_row) = edge_json.as_object() else {
                return BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Invalid,
                    result: serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": format!("batch_delete_edges[{idx}] must be an object")
                    }),
                };
            };

            let src_id = match edge_row
                .get("src_entity_id")
                .and_then(|v| v.as_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
            {
                Some(id) => id,
                None => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Invalid,
                        result: serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": "invalid or missing src_entity_id"
                        }),
                    };
                }
            };
            let dst_id = match edge_row
                .get("dst_entity_id")
                .and_then(|v| v.as_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
            {
                Some(id) => id,
                None => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Invalid,
                        result: serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": "invalid or missing dst_entity_id"
                        }),
                    };
                }
            };
            let edge_type = match edge_row.get("edge_type").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => {
                    return BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Invalid,
                        result: serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": "missing edge_type"
                        }),
                    };
                }
            };

            match storage
                .typed_edge_delete(ctx, session_id, src_id, &edge_type, dst_id)
                .await
            {
                Ok(true) => {
                    session.dirty.store(true, Ordering::Relaxed);
                    session.last_activity.notify_waiters();
                    BatchMutationOutcome {
                        index: idx,
                        kind: BatchMutationKind::Deleted,
                        result: serde_json::json!({
                        "index": idx,
                        "src_id": src_id.to_string(),
                        "dst_id": dst_id.to_string(),
                        "edge_type": edge_type,
                        "status": "deleted"
                        }),
                    }
                }
                Ok(false) => BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Missing,
                    result: serde_json::json!({
                    "index": idx,
                    "src_id": src_id.to_string(),
                    "dst_id": dst_id.to_string(),
                    "edge_type": edge_type,
                    "status": "not_found"
                    }),
                },
                Err(err) => BatchMutationOutcome {
                    index: idx,
                    kind: BatchMutationKind::Error,
                    result: serde_json::json!({
                    "index": idx,
                    "src_id": src_id.to_string(),
                    "dst_id": dst_id.to_string(),
                    "edge_type": edge_type,
                    "status": "error",
                    "reason": err.to_string()
                    }),
                },
            }
        });

    let outcomes: Vec<BatchMutationOutcome> = stream::iter(jobs)
        .buffer_unordered(BATCH_MUTATION_CONCURRENCY)
        .collect()
        .await;

    let mut deleted = 0usize;
    let mut missing = 0usize;
    let mut invalid = 0usize;
    let mut errors = 0usize;
    for outcome in &outcomes {
        match outcome.kind {
            BatchMutationKind::Deleted => deleted += 1,
            BatchMutationKind::Missing => missing += 1,
            BatchMutationKind::Invalid => invalid += 1,
            BatchMutationKind::Error => errors += 1,
            BatchMutationKind::Updated
            | BatchMutationKind::Unchanged
            | BatchMutationKind::NotFound
            | BatchMutationKind::Upserted => {}
        }
    }
    let results = ordered_batch_results(outcomes);

    Ok(serde_json::json!({
        "deleted": deleted,
        "missing": missing,
        "invalid": invalid,
        "errors": errors,
        "total": edges.len(),
        "results": results,
    }))
}

async fn handle_list_derived_cache<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize)
        .unwrap_or(100)
        .clamp(1, 500);

    let rows = storage
        .derived_cache_list_all(ctx, limit)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "count": rows.len(),
        "limit": limit,
        "entries": rows,
    }))
}

// --- Error mapping helpers ---

/// Map anyhow errors to JSON-RPC error codes, using INVALID_PARAMS for
/// quota violations (FMEA D1) and INTERNAL_ERROR for everything else.
fn map_quota_error(e: anyhow::Error) -> (i32, String) {
    if e.downcast_ref::<crate::quota::QuotaExceeded>().is_some() {
        (INVALID_PARAMS, e.to_string())
    } else {
        (INTERNAL_ERROR, e.to_string())
    }
}

async fn handle_find_duplicates<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let threshold = args
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.7);

    let pairs = crate::dedup::find_duplicates(storage, ctx, session_id, threshold)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(&pairs).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Parameter extraction helpers ---

/// Resolve `session_id` in tool-call arguments using the configured default.
///
/// Without this, the dispatcher only treated a *missing* `session_id` as a
/// fallback trigger — so callers passing `"default"`, `""`, `null`, or an
/// invalid UUID would either hit MCP client-side schema rejection or land in
/// the nil-UUID scope (silent wrong-session bug).
///
/// Rules:
/// - missing / null / empty / `"default"` / non-UUID string → inject the
///   configured default when one exists
/// - valid UUID string → passthrough
/// - non-UUID input AND no configured default → `INVALID_PARAMS` with a
///   message naming the field and the offending value (fail loud)
/// - missing AND no configured default → leave alone; downstream handler
///   decides whether the field was required
fn resolve_session_id(args: &mut Value, default: Option<uuid::Uuid>) -> Result<(), (i32, String)> {
    let Some(obj) = args.as_object_mut() else {
        return Ok(());
    };

    let caller_value = obj.get("session_id").cloned();
    let needs_fallback = match &caller_value {
        None => true,
        Some(Value::Null) => true,
        Some(Value::String(s)) => {
            s.is_empty() || s.eq_ignore_ascii_case("default") || uuid::Uuid::parse_str(s).is_err()
        }
        Some(_) => true,
    };

    if !needs_fallback {
        return Ok(());
    }

    // Observability: if the caller provided a non-empty value that failed to
    // parse, warn. Missing/null is the common, intentional path and stays
    // silent. Per the fail-loud rules, fallbacks must be observable.
    let caller_explicitly_bad = matches!(
        &caller_value,
        Some(Value::String(s)) if !s.is_empty()
    ) || matches!(&caller_value, Some(v) if !v.is_null() && !v.is_string());

    if let Some(sid) = default {
        if caller_explicitly_bad {
            tracing::warn!(
                provided = %caller_value.as_ref().unwrap(),
                default = %sid,
                "substituted configured default for caller-provided session_id"
            );
        }
        obj.insert("session_id".into(), Value::String(sid.to_string()));
        return Ok(());
    }

    match caller_value {
        None | Some(Value::Null) => Ok(()),
        Some(v) => Err((
            INVALID_PARAMS,
            format!(
                "session_id {} is not a valid UUID and no default session is configured; \
                 pass a valid UUID or configure server.default_session_id",
                v
            ),
        )),
    }
}

fn optional_uuid(args: &Value, field: &str) -> Result<Option<uuid::Uuid>, (i32, String)> {
    match args.get(field).and_then(|v| v.as_str()) {
        Some(s) => {
            // For optional UUID fields, treat invalid UUIDs as None rather than erroring.
            // This handles cases where callers pass non-UUID values (e.g., file paths) for
            // optional parameters - they're simply ignored with a debug log.
            match uuid::Uuid::parse_str(s) {
                Ok(u) => Ok(Some(u)),
                Err(_) => {
                    tracing::debug!(field = %field, value = %s, "optional_uuid: ignoring invalid UUID, treating as None");
                    Ok(None)
                }
            }
        }
        None => Ok(None),
    }
}

fn require_f32_array(args: &Value, field: &str) -> Result<Vec<f32>, (i32, String)> {
    args.get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_f64().map(|f| f as f32))
                .collect()
        })
        .ok_or((INVALID_PARAMS, format!("missing required array: {field}")))
}

fn optional_f32_array(args: &Value, field: &str) -> Result<Option<Vec<f32>>, (i32, String)> {
    match args.get(field) {
        Some(v) if v.is_array() => Ok(Some(
            v.as_array()
                .unwrap()
                .iter()
                .filter_map(|n| n.as_f64().map(|f| f as f32))
                .collect(),
        )),
        Some(_) => Err((INVALID_PARAMS, format!("{field} must be an array"))),
        None => Ok(None),
    }
}

fn optional_string_array(args: &Value, field: &str) -> Result<Vec<String>, (i32, String)> {
    let Some(value) = args.get(field) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err((INVALID_PARAMS, format!("{field} must be an array")));
    };
    let mut parsed = Vec::with_capacity(values.len());
    for (idx, value) in values.iter().enumerate() {
        let Some(text) = value.as_str() else {
            return Err((INVALID_PARAMS, format!("{field}[{idx}] must be a string")));
        };
        parsed.push(text.to_string());
    }
    Ok(parsed)
}

fn parse_session_task_status_param(
    status: &str,
) -> Result<crate::types::SessionTaskStatus, (i32, String)> {
    serde_json::from_value(Value::String(status.to_string())).map_err(|_| {
        (
            INVALID_PARAMS,
            format!("invalid session task status: {status}"),
        )
    })
}

fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, (i32, String)> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or((INVALID_PARAMS, format!("missing required string: {field}")))
}

fn require_uuid(args: &Value, field: &str) -> Result<uuid::Uuid, (i32, String)> {
    let s = require_str(args, field)?;
    uuid::Uuid::parse_str(s).map_err(|e| (INVALID_PARAMS, format!("invalid uuid {field}: {e}")))
}

fn require_i32(args: &Value, field: &str) -> Result<i32, (i32, String)> {
    args.get(field)
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or((INVALID_PARAMS, format!("missing required integer: {field}")))
}

// --- Remote teacher/learner memory tool handlers ---

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackRecordRequest {
    tenant_id: uuid::Uuid,
    remote_id: uuid::Uuid,
    target_id: uuid::Uuid,
    source_namespace: String,
    scope: String,
    feedback: String,
}

async fn handle_feedback_record<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: FeedbackRecordRequest = serde_json::from_value(args).map_err(|e| {
        (
            INVALID_PARAMS,
            format!("invalid feedback_record request: {e}"),
        )
    })?;
    if request.tenant_id != ctx.tenant_id {
        return Err((
            INVALID_PARAMS,
            format!(
                "tenant_id {} does not match authenticated tenant {}",
                request.tenant_id, ctx.tenant_id
            ),
        ));
    }
    if request.feedback.trim().is_empty() {
        return Err((INVALID_PARAMS, "feedback must not be empty".into()));
    }
    if request.source_namespace.trim().is_empty() || request.scope.trim().is_empty() {
        return Err((
            INVALID_PARAMS,
            "source_namespace and scope must not be empty".into(),
        ));
    }

    let signal = crate::remotes::feedback::FeedbackSignal::classify(&request.feedback);
    let note = format!(
        "Packet H feedback for namespace={} scope={}: {}; raw={}",
        request.source_namespace,
        request.scope,
        signal.explanation,
        request.feedback.trim()
    );
    let feedback = crate::remotes::types::MemoryFeedback {
        feedback_id: uuid::Uuid::new_v4(),
        remote_id: request.remote_id,
        target_id: request.target_id,
        feedback_type: signal.feedback_type,
        note: Some(note.clone()),
        created_at: chrono::Utc::now(),
    };
    storage
        .memory_feedback_put(ctx, &feedback)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "feedback_id": feedback.feedback_id,
        "remote_id": request.remote_id,
        "target_id": request.target_id,
        "source_namespace": request.source_namespace,
        "scope": request.scope,
        "feedback_type": signal.feedback_type,
        "weight": signal.weight,
        "requires_review": signal.requires_review,
        "applicability_correction": signal.applicability_correction,
        "explanation": signal.explanation,
        "halt_current_chain": signal.halt_current_chain,
        "guidance": if signal.halt_current_chain {
            "halt the current remote-memory chain and require an explicit next retrieval"
        } else {
            "persist feedback and apply only scoped trust updates"
        },
        "stored_note": note,
    }))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageMarkRequest {
    tenant_id: uuid::Uuid,
    remote_id: uuid::Uuid,
    target_id: uuid::Uuid,
    source_namespace: String,
    scope: String,
    usage: String,
}

async fn handle_usage_mark(
    args: Value,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: UsageMarkRequest = serde_json::from_value(args)
        .map_err(|e| (INVALID_PARAMS, format!("invalid usage_mark request: {e}")))?;
    ensure_remote_tenant(request.tenant_id, ctx)?;
    if request.source_namespace.trim().is_empty() || request.scope.trim().is_empty() {
        return Err((
            INVALID_PARAMS,
            "source_namespace and scope must not be empty".into(),
        ));
    }
    let reinforcement = usage_reinforcement(&request.usage)?;
    let key = crate::remotes::feedback::TrustKey::new(
        request.remote_id,
        request.source_namespace.clone(),
        request.scope.clone(),
    );
    let mut ledger = crate::remotes::feedback::TrustLedger::default();
    let update = ledger.apply(&key, reinforcement);
    Ok(serde_json::json!({
        "remote_id": request.remote_id,
        "target_id": request.target_id,
        "source_namespace": request.source_namespace,
        "scope": request.scope,
        "usage": request.usage,
        "reinforcement": reinforcement,
        "delta": update.delta,
        "score": update.score,
        "policy_persisted": false,
        "guidance": "apply this reinforcement only to the returned remote_id/source_namespace/scope key",
    }))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustUpdateRequest {
    tenant_id: uuid::Uuid,
    remote_id: uuid::Uuid,
    source_namespace: String,
    scope: String,
    reinforcements: Vec<String>,
}

async fn handle_trust_update<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: TrustUpdateRequest = serde_json::from_value(args)
        .map_err(|e| (INVALID_PARAMS, format!("invalid trust_update request: {e}")))?;
    ensure_remote_tenant(request.tenant_id, ctx)?;
    if request.source_namespace.trim().is_empty() || request.scope.trim().is_empty() {
        return Err((
            INVALID_PARAMS,
            "source_namespace and scope must not be empty".into(),
        ));
    }
    if request.reinforcements.is_empty() {
        return Err((
            INVALID_PARAMS,
            "reinforcements must contain at least one entry".into(),
        ));
    }
    let key = crate::remotes::feedback::TrustKey::new(
        request.remote_id,
        request.source_namespace.clone(),
        request.scope.clone(),
    );
    let mut ledger = crate::remotes::feedback::TrustLedger::default();
    let mut updates = Vec::with_capacity(request.reinforcements.len());
    for raw in &request.reinforcements {
        let reinforcement = trust_reinforcement(raw)?;
        updates.push(ledger.apply(&key, reinforcement));
    }
    let policy = ledger.not_trusted_for_fact(&key);
    let policy_persisted = policy.is_some();
    if let Some(fact) = &policy {
        storage
            .remote_policy_put(ctx, fact)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    }
    let final_score = ledger.score(&key);
    Ok(serde_json::json!({
        "remote_id": request.remote_id,
        "source_namespace": request.source_namespace,
        "scope": request.scope,
        "score": final_score,
        "updates": updates,
        "policy_persisted": policy_persisted,
        "policy_fact": policy,
        "guidance": "trust updates are scoped by remote_id, source_namespace, and scope; do not apply globally",
    }))
}

fn ensure_remote_tenant(
    tenant_id: uuid::Uuid,
    ctx: &crate::types::TenantContext,
) -> Result<(), (i32, String)> {
    if tenant_id == ctx.tenant_id {
        return Ok(());
    }
    Err((
        INVALID_PARAMS,
        format!(
            "tenant_id {} does not match authenticated tenant {}",
            tenant_id, ctx.tenant_id
        ),
    ))
}

fn usage_reinforcement(
    usage: &str,
) -> Result<crate::remotes::feedback::Reinforcement, (i32, String)> {
    match usage.trim().to_ascii_lowercase().as_str() {
        "chosen" => Ok(crate::remotes::feedback::Reinforcement::PolicyChosen),
        "confirmed" | "success" => Ok(crate::remotes::feedback::Reinforcement::UserConfirmed),
        other => Err((INVALID_PARAMS, format!("unknown usage mark: {other}"))),
    }
}

fn trust_reinforcement(
    raw: &str,
) -> Result<crate::remotes::feedback::Reinforcement, (i32, String)> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "chosen" | "policy_chosen" => Ok(crate::remotes::feedback::Reinforcement::PolicyChosen),
        "confirmed" | "user_confirmed" | "success" => {
            Ok(crate::remotes::feedback::Reinforcement::UserConfirmed)
        }
        "wrong_scope" => Ok(crate::remotes::feedback::Reinforcement::WrongScope),
        "strong_negative" => Ok(crate::remotes::feedback::Reinforcement::StrongNegative),
        other => Err((
            INVALID_PARAMS,
            format!("unknown trust reinforcement: {other}"),
        )),
    }
}

async fn handle_teach_query_stream<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let request = crate::remotes::teach::TeachQueryRequest {
        remote_id: require_uuid(&args, "remote_id")?,
        learner_instance_id: crate::remote_identity::InstanceId(
            optional_uuid(&args, "learner_instance_id")?.unwrap_or(uuid::Uuid::nil()),
        ),
        query: require_str(&args, "query")?.to_string(),
        namespaces: optional_string_array(&args, "namespaces")?,
        max_items: args
            .get("max_items")
            .and_then(|v| v.as_i64())
            .unwrap_or(8)
            .clamp(1, 50) as i32,
        query_embedding: optional_f32_array(&args, "query_embedding")?,
        grants: optional_string_array(&args, "grants")?,
        include_raw_context: args
            .get("include_raw_context")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        include_detail: args
            .get("include_detail")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        include_skill: args
            .get("include_skill")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    };
    let events = crate::remotes::teach::teach_query_stream(storage, ctx, session_id, request)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(events).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_pull_preview<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let mut request = crate::remotes::pull::PullPreviewRequest::new(
        require_uuid(&args, "remote_id")?,
        require_str(&args, "remote_name")?.to_string(),
        require_str(&args, "query")?.to_string(),
    )
    .with_public_identity(
        serde_json::from_value(
            args.get("public_identity")
                .cloned()
                .ok_or((INVALID_PARAMS, "missing public_identity".into()))?,
        )
        .map_err(|e| (INVALID_PARAMS, format!("invalid public_identity: {e}")))?,
    );
    if let Some(value) = args.get("local_applicability") {
        request = request.with_local_applicability(
            serde_json::from_value(value.clone())
                .map_err(|e| (INVALID_PARAMS, format!("invalid local_applicability: {e}")))?,
        );
    }
    if let Some(ttl) = args.get("preview_ttl_seconds").and_then(|v| v.as_i64()) {
        request.preview_ttl = chrono::Duration::seconds(ttl.clamp(1, 86400));
    }
    if let Some(remote) = storage
        .remote_get(ctx, request.remote_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
    {
        let policy_rows = storage
            .remote_policy_list(ctx, request.remote_id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
        let mut facts = vec![crate::remotes::policy::PolicyFact::remote(
            remote.name.clone(),
        )];
        for row in &policy_rows {
            if let Some(fact) = policy_fact_from_row(&remote.name, row)? {
                facts.push(fact);
            }
        }
        request.remote_name = remote.name;
        request = request.with_policy(crate::remotes::policy::RemotePolicy::from_facts(facts));
    }
    let client = crate::remotes::pull::json_remote_client_from_args(&args)
        .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
    let preview = crate::remotes::pull::pull_preview(&client, storage, ctx, request)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(preview).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_pull_commit<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let preview: crate::remotes::pull::PullPreviewPlan = serde_json::from_value(
        args.get("preview")
            .cloned()
            .ok_or((INVALID_PARAMS, "missing preview".into()))?,
    )
    .map_err(|e| (INVALID_PARAMS, format!("invalid preview: {e}")))?;
    let learner_decision = serde_json::from_value(
        args.get("learner_decision")
            .cloned()
            .ok_or((INVALID_PARAMS, "missing learner_decision".into()))?,
    )
    .map_err(|e| (INVALID_PARAMS, format!("invalid learner_decision: {e}")))?;
    let receipt = crate::remotes::pull::pull_commit(
        storage,
        ctx,
        crate::remotes::pull::PullCommitRequest {
            preview,
            learner_decision,
        },
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(receipt).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteListRequest {
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteAddRequest {
    tenant_id: uuid::Uuid,
    remote_id: Option<uuid::Uuid>,
    instance_id: uuid::Uuid,
    name: String,
    endpoint: String,
    trust_class: crate::remotes::types::RemoteTrustClass,
    public_key_fingerprint: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemotePolicyInputFact {
    kind: String,
    namespace: String,
    action: String,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteUpdatePolicyRequest {
    tenant_id: uuid::Uuid,
    remote_id: uuid::Uuid,
    facts: Vec<RemotePolicyInputFact>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteIdRequest {
    tenant_id: Option<uuid::Uuid>,
    remote_id: uuid::Uuid,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteExplainPolicyRequest {
    remote_id: uuid::Uuid,
    action: String,
    namespace: String,
}

async fn handle_remote_list<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: RemoteListRequest = serde_json::from_value(args)
        .map_err(|e| (INVALID_PARAMS, format!("invalid remote_list request: {e}")))?;
    let limit = request.limit.unwrap_or(100).clamp(1, 100);
    let mut remotes = storage
        .remote_list(ctx, limit)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    remotes.sort_by(|a, b| a.name.cmp(&b.name).then(a.remote_id.cmp(&b.remote_id)));
    remotes.truncate(limit);
    Ok(serde_json::json!({ "remotes": remotes, "count": remotes.len() }))
}

async fn handle_remote_add<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: RemoteAddRequest = serde_json::from_value(args)
        .map_err(|e| (INVALID_PARAMS, format!("invalid remote_add request: {e}")))?;
    ensure_remote_tenant(request.tenant_id, ctx)?;
    let name = request.name.trim();
    let endpoint = request.endpoint.trim();
    let fingerprint = request.public_key_fingerprint.trim();
    if name.is_empty() || endpoint.is_empty() || fingerprint.is_empty() {
        return Err((
            INVALID_PARAMS,
            "name, endpoint, and public_key_fingerprint must not be empty".into(),
        ));
    }
    validate_remote_endpoint(endpoint)?;
    let now = chrono::Utc::now();
    let remote = crate::remotes::types::MemoryRemote {
        remote_id: request.remote_id.unwrap_or_else(uuid::Uuid::new_v4),
        instance_id: crate::remote_identity::InstanceId(request.instance_id),
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        trust_class: request.trust_class,
        public_key_fingerprint: crate::remote_identity::PublicKeyFingerprint(
            fingerprint.to_string(),
        ),
        enabled: true,
        created_at: now,
        updated_at: now,
    };
    storage
        .remote_put(ctx, &remote)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    Ok(serde_json::json!({ "action": "upserted", "remote": remote }))
}

async fn handle_remote_update_policy<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: RemoteUpdatePolicyRequest = serde_json::from_value(args).map_err(|e| {
        (
            INVALID_PARAMS,
            format!("invalid remote_update_policy request: {e}"),
        )
    })?;
    ensure_remote_tenant(request.tenant_id, ctx)?;
    if request.facts.is_empty() {
        return Err((INVALID_PARAMS, "facts must not be empty".into()));
    }
    let now = chrono::Utc::now();
    let mut stored = Vec::with_capacity(request.facts.len());
    for fact in request.facts {
        let namespace = fact.namespace.trim();
        let action = fact.action.trim();
        if namespace.is_empty() || action.is_empty() {
            return Err((
                INVALID_PARAMS,
                "policy namespace and action must not be empty".into(),
            ));
        }
        let kind = match fact.kind.as_str() {
            "grant" => {
                crate::remotes::types::RemotePolicyKind::Grant(crate::remotes::types::RemoteGrant {
                    namespace: namespace.to_string(),
                    grant: action.to_string(),
                })
            }
            "deny" => {
                crate::remotes::types::RemotePolicyKind::Deny(crate::remotes::types::RemoteDeny {
                    namespace: namespace.to_string(),
                    deny: action.to_string(),
                })
            }
            other => {
                return Err((
                    INVALID_PARAMS,
                    format!("unsupported policy fact kind: {other}"),
                ));
            }
        };
        let row = crate::remotes::types::RemotePolicyFact {
            fact_id: uuid::Uuid::new_v4(),
            remote_id: request.remote_id,
            kind,
            created_at: now,
            expires_at: fact.expires_at,
        };
        storage
            .remote_policy_put(ctx, &row)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
        stored.push(row);
    }
    Ok(
        serde_json::json!({ "remote_id": request.remote_id, "facts": stored, "policy_count": stored.len() }),
    )
}

async fn handle_remote_remove<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: RemoteIdRequest = serde_json::from_value(args).map_err(|e| {
        (
            INVALID_PARAMS,
            format!("invalid remote_remove request: {e}"),
        )
    })?;
    if let Some(tenant_id) = request.tenant_id {
        ensure_remote_tenant(tenant_id, ctx)?;
    }
    let mut remote = storage
        .remote_get(ctx, request.remote_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                INVALID_PARAMS,
                format!("unknown remote_id: {}", request.remote_id),
            )
        })?;
    remote.enabled = false;
    remote.updated_at = chrono::Utc::now();
    storage
        .remote_put(ctx, &remote)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    Ok(
        serde_json::json!({ "remote_id": request.remote_id, "enabled": false, "disabled": true, "removed": false, "preserved_provenance": true }),
    )
}

async fn handle_remote_health<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: RemoteIdRequest = serde_json::from_value(args).map_err(|e| {
        (
            INVALID_PARAMS,
            format!("invalid remote_health request: {e}"),
        )
    })?;
    let remote = storage
        .remote_get(ctx, request.remote_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                INVALID_PARAMS,
                format!("unknown remote_id: {}", request.remote_id),
            )
        })?;
    let endpoint_valid = validate_remote_endpoint(&remote.endpoint).is_ok();
    Ok(serde_json::json!({
        "remote_id": remote.remote_id,
        "name": remote.name,
        "enabled": remote.enabled,
        "configured": true,
        "endpoint_valid": endpoint_valid,
        "status": if remote.enabled && endpoint_valid { "configured" } else { "disabled_or_invalid" },
        "checked_at": chrono::Utc::now(),
    }))
}

async fn handle_remote_capabilities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: RemoteIdRequest = serde_json::from_value(args).map_err(|e| {
        (
            INVALID_PARAMS,
            format!("invalid remote_capabilities request: {e}"),
        )
    })?;
    let remote = storage
        .remote_get(ctx, request.remote_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                INVALID_PARAMS,
                format!("unknown remote_id: {}", request.remote_id),
            )
        })?;
    Ok(serde_json::json!({
        "remote_id": remote.remote_id,
        "instance_id": remote.instance_id,
        "enabled": remote.enabled,
        "capabilities": ["teach_query_stream", "pull_preview", "pull_commit", "remote_detail", "archive_detail"],
        "details": {
            "supports_signed_packets": true,
            "supports_stub_activation": true,
            "supports_policy_explain": true,
            "supports_provenance": true
        }
    }))
}

async fn handle_remote_explain_policy<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let request: RemoteExplainPolicyRequest = serde_json::from_value(args).map_err(|e| {
        (
            INVALID_PARAMS,
            format!("invalid remote_explain_policy request: {e}"),
        )
    })?;
    let remote = storage
        .remote_get(ctx, request.remote_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                INVALID_PARAMS,
                format!("unknown remote_id: {}", request.remote_id),
            )
        })?;
    let policy_rows = storage
        .remote_policy_list(ctx, request.remote_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let mut facts = vec![crate::remotes::policy::PolicyFact::remote(
        remote.name.clone(),
    )];
    for row in &policy_rows {
        if let Some(fact) = policy_fact_from_row(&remote.name, row)? {
            facts.push(fact);
        }
    }
    let policy = crate::remotes::policy::RemotePolicy::from_facts(facts);
    let decision = match request.action.as_str() {
        "read" => policy.can_query(&remote.name, &request.namespace),
        "detail_fetch" => policy.can_fetch_detail(&remote.name, &policy_item(&request.namespace)),
        "autocommit" => policy.can_autocommit(&remote.name, &policy_item(&request.namespace)),
        "requires_activation" => {
            policy.requires_activation(&remote.name, &policy_item(&request.namespace))
        }
        "should_consult" => policy.should_consult(&remote.name, &request.namespace),
        other => {
            return Err((
                INVALID_PARAMS,
                format!("unsupported policy action: {other}"),
            ));
        }
    };
    let reasons: Vec<_> = decision
        .reasons
        .iter()
        .map(|reason| serde_json::json!({ "code": reason.code, "fact": reason.fact, "message": reason.message }))
        .collect();
    Ok(serde_json::json!({
        "remote_id": request.remote_id,
        "remote_name": remote.name,
        "action": request.action,
        "namespace": request.namespace,
        "allowed": decision.allowed,
        "explanation": decision.explanation,
        "reasons": reasons,
        "policy_fact_count": policy_rows.len()
    }))
}

fn validate_remote_endpoint(endpoint: &str) -> Result<(), (i32, String)> {
    if endpoint.starts_with("https://") || endpoint.starts_with("http://") {
        if endpoint.contains('@') {
            return Err((
                INVALID_PARAMS,
                "remote endpoint must not contain credentials".into(),
            ));
        }
        return Ok(());
    }
    Err((
        INVALID_PARAMS,
        "remote endpoint must be an http(s) URL".into(),
    ))
}

fn policy_action(action: &str) -> Result<crate::remotes::policy::PolicyAction, (i32, String)> {
    match action {
        "read" => Ok(crate::remotes::policy::PolicyAction::Read),
        "detail_fetch" => Ok(crate::remotes::policy::PolicyAction::DetailFetch),
        "autocommit" => Ok(crate::remotes::policy::PolicyAction::Autocommit),
        "requires_activation" => Ok(crate::remotes::policy::PolicyAction::RequiresActivation),
        "should_consult" => Ok(crate::remotes::policy::PolicyAction::ShouldConsult),
        other => Err((
            INVALID_PARAMS,
            format!("unsupported policy action: {other}"),
        )),
    }
}

fn policy_fact_from_row(
    remote_name: &str,
    row: &crate::remotes::types::RemotePolicyFact,
) -> Result<Option<crate::remotes::policy::PolicyFact>, (i32, String)> {
    use crate::remotes::policy::PolicyFact;
    match &row.kind {
        crate::remotes::types::RemotePolicyKind::Grant(grant) => match grant.grant.as_str() {
            "trusted_for" => Ok(Some(PolicyFact::trusted_for(
                remote_name,
                grant.namespace.clone(),
            ))),
            "fallback_enabled" => Ok(Some(PolicyFact::fallback_enabled(
                remote_name,
                grant.namespace.clone(),
            ))),
            action => Ok(Some(PolicyFact::grant(
                remote_name,
                policy_action(action)?,
                grant.namespace.clone(),
            ))),
        },
        crate::remotes::types::RemotePolicyKind::Deny(deny) => match deny.deny.as_str() {
            "not_trusted_for" => Ok(Some(PolicyFact::not_trusted_for(
                remote_name,
                deny.namespace.clone(),
            ))),
            action => Ok(Some(PolicyFact::deny(
                remote_name,
                policy_action(action)?,
                deny.namespace.clone(),
            ))),
        },
    }
}

fn policy_item(namespace: &str) -> crate::remotes::policy::PolicyItem {
    crate::remotes::policy::PolicyItem::new("packet_k_probe", namespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use crate::storage::mock::MockStorage;
    use crate::types::TenantContext;
    use uuid::Uuid;

    #[test]
    fn hybrid_search_scope_defaults_to_both_so_global_corpus_is_visible() {
        use crate::hybrid_search::SearchScope;
        // Regression (t_ae8613ff): curated global/nil corpus was invisible to
        // normal agent searches because hybrid_search defaulted to SessionOnly.
        // With no `scope` and no `include_cross_session`, a default search must
        // span the caller's session PLUS the global + nil curated partitions.
        assert_eq!(
            parse_hybrid_search_scope(&serde_json::json!({})).unwrap(),
            SearchScope::Both,
        );
        // An explicit cross-session opt-out is still honored.
        assert_eq!(
            parse_hybrid_search_scope(&serde_json::json!({"include_cross_session": false}))
                .unwrap(),
            SearchScope::SessionOnly,
        );
        // An explicit scope always wins over the default.
        assert_eq!(
            parse_hybrid_search_scope(&serde_json::json!({"scope": "session"})).unwrap(),
            SearchScope::SessionOnly,
        );
        assert_eq!(
            parse_hybrid_search_scope(&serde_json::json!({"scope": "global"})).unwrap(),
            SearchScope::GlobalOnly,
        );
    }

    #[test]
    fn auto_fusion_routes_exact_errors_to_lexical_profile() {
        let filter = crate::hybrid_search::SearchFilter::default();
        let selected = select_auto_fusion_profile(
            "thread 'main' panicked at src/lib.rs:42: unwrap on None",
            &filter,
        );
        assert_eq!(selected.intent, "exact_error_or_symbol");
        assert_eq!(selected.profile, "bm25-only");
    }

    #[test]
    fn auto_fusion_routes_project_bug_queries_to_workspace_profile() {
        let filter = crate::hybrid_search::SearchFilter {
            workspace_cwd: Some("/Users/bkearns/src/ferrosa-suite/ferrosa-memory".into()),
            ..Default::default()
        };
        let selected =
            select_auto_fusion_profile("why did the hybrid search CI test fail?", &filter);
        assert_eq!(selected.intent, "project_bug_or_build");
        assert_eq!(selected.profile, "bm25-semantic-workspace");
    }

    #[test]
    fn auto_fusion_routes_broad_recall_to_clean_semantic_profile() {
        let filter = crate::hybrid_search::SearchFilter::default();
        let selected = select_auto_fusion_profile(
            "explain the RLM and EverMemOS memory architecture",
            &filter,
        );
        assert_eq!(selected.intent, "broad_semantic");
        assert_eq!(selected.profile, "bm25-semantic");
    }

    #[test]
    fn auto_fusion_routes_session_recall_to_session_profile() {
        let filter = crate::hybrid_search::SearchFilter::default();
        let selected =
            select_auto_fusion_profile("what were we working on earlier in this session", &filter);
        assert_eq!(selected.intent, "session_memory");
        assert_eq!(selected.profile, "session-semantic");
        // The named profile must resolve to real weights.
        assert!(crate::hybrid_search::FusionConfig::profile("session-semantic").is_some());
    }

    #[test]
    fn auto_fusion_separates_corpus_reference_from_broad_semantic() {
        let filter = crate::hybrid_search::SearchFilter::default();

        let corpus = select_auto_fusion_profile("find the EverMemOS paper citation", &filter);
        assert_eq!(corpus.intent, "corpus_reference");
        assert_eq!(corpus.profile, "corpus-reference");
        assert!(crate::hybrid_search::FusionConfig::profile("corpus-reference").is_some());

        let broad = select_auto_fusion_profile("explain how reciprocal rank fusion works", &filter);
        assert_eq!(broad.intent, "broad_semantic");
        assert_eq!(broad.profile, "bm25-semantic");
    }

    #[test]
    fn auto_fusion_code_token_density_triggers_exact_error() {
        let filter = crate::hybrid_search::SearchFilter::default();
        // No stack-trace markers, but `::` + a call shape => code.
        let selected = select_auto_fusion_profile("GraphClient::reconnecting_storage()", &filter);
        assert_eq!(selected.intent, "exact_error_or_symbol");
        assert_eq!(selected.profile, "bm25-only");
    }

    #[test]
    fn auto_fusion_project_bug_requires_workspace() {
        // Same bug-flavored query: workspace present => project_bug; absent =>
        // it must NOT be misrouted to the workspace profile.
        let bug_query = "the integration build keeps failing";
        let with_ws = select_auto_fusion_profile(
            bug_query,
            &crate::hybrid_search::SearchFilter {
                workspace_cwd: Some("/repo".into()),
                ..Default::default()
            },
        );
        assert_eq!(with_ws.intent, "project_bug_or_build");

        let without_ws =
            select_auto_fusion_profile(bug_query, &crate::hybrid_search::SearchFilter::default());
        assert_ne!(without_ws.intent, "project_bug_or_build");
    }

    /// Extract the inner tool result from MCP CallToolResult wrapper.
    /// Dispatch wraps results as {"content": [{"type": "text", "text": "..."}]}.
    fn unwrap_tool_result(result: Value) -> Value {
        let text = result["content"][0]["text"]
            .as_str()
            .expect("CallToolResult missing content[0].text");
        serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
    }

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    // --- Remote teacher/learner dispatch tests ---

    #[tokio::test]
    async fn remote_add_rejects_invalid_endpoint_config() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        let err = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_add",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "name": "bad",
                    "endpoint": "localhost:18765",
                    "trust_class": "personal",
                    "instance_id": Uuid::new_v4().to_string(),
                    "public_key_fingerprint": "ed25519:abc"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, INVALID_PARAMS);
        assert!(err.1.contains("endpoint"));
        assert_eq!(store.remote_list(&ctx, 10).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn remote_add_list_update_policy_explain_and_remove_round_trip() {
        use crate::remote_identity::{ContentHash, InstanceId, PublicKeyFingerprint};
        use crate::remotes::types::{MemoryProvenance, RemotePolicyFact, RemotePolicyKind};

        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let remote_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let local_entity_id = Uuid::new_v4();
        let packet_id = Uuid::new_v4();
        let item_id = Uuid::new_v4();

        let add = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_add",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "name": "team-memory",
                    "endpoint": "https://remote.example/mcp",
                    "trust_class": "team",
                    "instance_id": instance_id.to_string(),
                    "public_key_fingerprint": "ed25519:team"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(add);
        assert_eq!(body["remote"]["remote_id"], remote_id.to_string());
        assert_eq!(body["remote"]["enabled"], true);

        let list = dispatch(
            "tools/call",
            serde_json::json!({"name": "remote_list", "arguments": {"limit": 10}}),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(list);
        assert_eq!(body["remotes"].as_array().unwrap().len(), 1);

        let policy = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_update_policy",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "facts": [
                        {"kind": "grant", "namespace": "knowledge", "action": "read"},
                        {"kind": "grant", "namespace": "knowledge", "action": "autocommit"},
                        {"kind": "grant", "namespace": "gpu_builds", "action": "trusted_for"},
                        {"kind": "grant", "namespace": "gpu_builds", "action": "fallback_enabled"},
                        {"kind": "grant", "namespace": "gpu_builds", "action": "should_consult"}
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(policy);
        assert_eq!(body["policy_count"], 5);

        let explain = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_explain_policy",
                "arguments": {
                    "remote_id": remote_id.to_string(),
                    "action": "read",
                    "namespace": "knowledge"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(explain);
        assert_eq!(body["allowed"], true);
        assert!(body["explanation"].as_str().unwrap().contains("Allowed"));

        let explain = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_explain_policy",
                "arguments": {
                    "remote_id": remote_id.to_string(),
                    "action": "should_consult",
                    "namespace": "gpu_builds"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(explain);
        assert_eq!(body["allowed"], false);
        assert_eq!(body["action"], "should_consult");

        store
            .memory_provenance_put(
                &ctx,
                &MemoryProvenance {
                    provenance_id: Uuid::new_v4(),
                    local_entity_id,
                    remote_id,
                    packet_id,
                    item_id,
                    content_hash: ContentHash("content".into()),
                    signature_hash: ContentHash("sig".into()),
                    imported_at: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();

        let remove = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_remove",
                "arguments": {"tenant_id": ctx.tenant_id.to_string(), "remote_id": remote_id.to_string()}
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(remove);
        assert_eq!(body["removed"], false);
        assert_eq!(body["disabled"], true);

        let remote = store.remote_get(&ctx, remote_id).await.unwrap().unwrap();
        assert_eq!(remote.instance_id, InstanceId(instance_id));
        assert_eq!(
            remote.public_key_fingerprint,
            PublicKeyFingerprint("ed25519:team".into())
        );
        assert!(!remote.enabled);
        let provenance = store
            .memory_provenance_list_by_entity(&ctx, local_entity_id)
            .await
            .unwrap();
        assert_eq!(
            provenance.len(),
            1,
            "remote_remove must not delete import provenance"
        );
        let facts = store.remote_policy_list(&ctx, remote_id).await.unwrap();
        assert!(
            facts
                .iter()
                .any(|fact| matches!(fact.kind, RemotePolicyKind::Grant(_)))
        );
        assert!(
            facts
                .iter()
                .all(|fact: &RemotePolicyFact| fact.remote_id == remote_id)
        );
    }

    #[tokio::test]
    async fn remote_capabilities_and_health_reflect_registered_remote() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let remote_id = Uuid::new_v4();

        dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_add",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "name": "archive",
                    "endpoint": "https://archive.example/mcp",
                    "trust_class": "archive",
                    "instance_id": Uuid::new_v4().to_string(),
                    "public_key_fingerprint": "ed25519:archive"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();

        let health = dispatch(
            "tools/call",
            serde_json::json!({"name": "remote_health", "arguments": {"remote_id": remote_id.to_string()}}),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(health);
        assert_eq!(body["status"], "configured");
        assert_eq!(body["enabled"], true);

        let capabilities = dispatch(
            "tools/call",
            serde_json::json!({"name": "remote_capabilities", "arguments": {"remote_id": remote_id.to_string()}}),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(capabilities);
        assert!(
            body["capabilities"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("teach_query_stream"))
        );
        assert!(
            body["capabilities"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("archive_detail"))
        );
    }

    #[tokio::test]
    async fn packet_l_tools_list_exposes_remote_smoke_surface_and_rejects_credentialed_endpoints() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let tools = dispatch(
            "tools/list",
            serde_json::json!({"include_all": true}),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let tool_names: Vec<&str> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();

        for name in [
            "remote_add",
            "remote_list",
            "remote_detail",
            "remote_explain_policy",
            "teach_query_stream",
            "pull_preview",
            "pull_commit",
        ] {
            assert!(
                tool_names.contains(&name),
                "missing remote smoke tool: {name}"
            );
        }

        let err = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_add",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": Uuid::new_v4().to_string(),
                    "name": "credentialed",
                    "endpoint": "https://user:pass@archive.example/mcp",
                    "trust_class": "archive",
                    "instance_id": Uuid::new_v4().to_string(),
                    "public_key_fingerprint": "ed25519:archive"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS);
        assert_eq!(err.1, "remote endpoint must not contain credentials");
    }

    #[tokio::test]
    async fn packet_l_remote_detail_and_policy_explain_smoke_cover_capabilities_and_deny_override()
    {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let remote_id = Uuid::new_v4();

        dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_add",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "name": "gpu",
                    "endpoint": "https://gpu.example/mcp",
                    "trust_class": "team",
                    "instance_id": Uuid::new_v4().to_string(),
                    "public_key_fingerprint": "ed25519:gpu"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();

        let listed = dispatch(
            "tools/call",
            serde_json::json!({"name": "remote_list", "arguments": {"limit": 10}}),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let listed = unwrap_tool_result(listed);
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["remotes"][0]["remote_id"], remote_id.to_string());

        let detail = dispatch(
            "tools/call",
            serde_json::json!({"name": "remote_detail", "arguments": {"remote_id": remote_id.to_string()}}),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let detail = unwrap_tool_result(detail);
        assert!(
            detail["capabilities"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("pull_preview"))
        );
        assert_eq!(detail["details"]["supports_signed_packets"], true);
        assert_eq!(detail["details"]["supports_policy_explain"], true);
        assert_eq!(detail["details"]["supports_provenance"], true);

        dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_update_policy",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "facts": [
                        {"kind": "grant", "namespace": "knowledge", "action": "autocommit"},
                        {"kind": "grant", "namespace": "gpu_builds", "action": "trusted_for"},
                        {"kind": "deny", "namespace": "gpu_builds", "action": "autocommit"}
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();

        let explained = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_explain_policy",
                "arguments": {
                    "remote_id": remote_id.to_string(),
                    "action": "autocommit",
                    "namespace": "gpu_builds"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let explained = unwrap_tool_result(explained);
        assert_eq!(explained["allowed"], false);
        assert_eq!(explained["policy_fact_count"], 3);
        assert!(
            explained["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| {
                    reason["code"] == "deny"
                        && reason["message"] == "explicit deny overrides any derived grant"
                })
        );
    }

    #[tokio::test]
    async fn packet_l_dispatch_pull_preview_and_commit_write_provenance_and_stub() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let remote_id = Uuid::new_v4();
        let teacher = crate::remote_identity::InstanceSigningIdentity::generate(
            crate::remote_identity::InstanceId(Uuid::new_v4()),
        );
        let learner = crate::remote_identity::InstanceSigningIdentity::generate(
            crate::remote_identity::InstanceId(Uuid::new_v4()),
        );
        let packet_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let frame = |namespace: &str| crate::remotes::types::ApplicabilityFrame {
            namespaces: vec![namespace.into()],
            host_os: Some("linux".into()),
            container_runtime: Some("docker".into()),
            hardware: vec!["gpu".into()],
            required_tags: vec![format!("namespace:{namespace}")],
            excluded_tags: vec![],
            confidence: 0.91,
        };
        let item =
            |title: &str, namespace: &str, summary: &str| crate::remotes::types::TeachingItem {
                item_id: Uuid::new_v4(),
                packet_id,
                kind: crate::remotes::types::TeachingKind::Decision,
                title: title.into(),
                summary: summary.into(),
                body: Some(summary.into()),
                content_hash: crate::remote_identity::ContentHash::sha256_bytes(
                    format!("{namespace}:{title}:{summary}").as_bytes(),
                ),
                applicability: frame(namespace),
                safety: crate::remotes::types::SafetyClassification {
                    risk: crate::remotes::types::SafetyRisk::Low,
                    reasons: vec!["safe packet l smoke".into()],
                    redacted: false,
                    requires_human: false,
                },
                detail_ref: None,
                metadata: serde_json::json!({}),
                created_at: now,
            };
        let packet = crate::remotes::types::TeachingPacket {
            packet_id,
            teacher_instance_id: teacher.instance_id,
            request_id: Some(Uuid::new_v4()),
            source_namespace: "gpu_builds".into(),
            query: "gpu build".into(),
            items: vec![
                item("GPU build", "gpu_builds", "Use pinned CUDA image"),
                item("Team note", "team_notes", "Keep as remote stub"),
            ],
            expires_at: Some(now + chrono::Duration::hours(1)),
            created_at: now,
        };
        let signed_packet = teacher.sign(packet).unwrap();

        dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_add",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "name": "gpu",
                    "endpoint": "https://gpu.example/mcp",
                    "trust_class": "team",
                    "instance_id": teacher.instance_id.0.to_string(),
                    "public_key_fingerprint": teacher.public_identity().public_key_fingerprint.0
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        dispatch(
            "tools/call",
            serde_json::json!({
                "name": "remote_update_policy",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "facts": [
                        {"kind": "grant", "namespace": "knowledge", "action": "autocommit"},
                        {"kind": "grant", "namespace": "gpu_builds", "action": "trusted_for"}
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();

        let preview = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "pull_preview",
                "arguments": {
                    "remote_id": remote_id.to_string(),
                    "remote_name": "gpu",
                    "query": "gpu build",
                    "public_identity": teacher.public_identity(),
                    "signed_packet": signed_packet
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let preview = unwrap_tool_result(preview);
        assert_eq!(preview["items"][0]["state"], "active");
        assert_eq!(preview["items"][1]["state"], "needs_activation");

        let preview_plan: crate::remotes::pull::PullPreviewPlan =
            serde_json::from_value(preview).unwrap();
        let commit_request =
            crate::remotes::pull::PullCommitRequest::from_preview(preview_plan.clone(), &learner);
        let receipt = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "pull_commit",
                "arguments": {
                    "preview": preview_plan,
                    "learner_decision": commit_request.learner_decision
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let receipt = unwrap_tool_result(receipt);
        assert_eq!(receipt["imported_count"], 1);
        assert_eq!(receipt["stub_count"], 1);
        assert_eq!(store.memory_provenance.lock().await.len(), 1);
        assert_eq!(store.remote_stubs.lock().await.len(), 1);
        assert_eq!(store.import_batches.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn remote_feedback_record_rejects_forged_tenant() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let remote_id = Uuid::new_v4();
        let target_id = Uuid::new_v4();
        let forged = Uuid::new_v4();

        let err = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "feedback_record",
                "arguments": {
                    "tenant_id": forged.to_string(),
                    "remote_id": remote_id.to_string(),
                    "target_id": target_id.to_string(),
                    "source_namespace": "gpu_builds",
                    "scope": "linux",
                    "feedback": "no"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, INVALID_PARAMS);
        assert!(err.1.contains("authenticated tenant"));
    }

    #[tokio::test]
    async fn remote_feedback_negative_creates_queryable_explanation() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let remote_id = Uuid::new_v4();
        let target_id = Uuid::new_v4();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "feedback_record",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "target_id": target_id.to_string(),
                    "source_namespace": "gpu_builds",
                    "scope": "linux",
                    "feedback": "WTF"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(result);

        assert_eq!(body["feedback_type"], "wrong_fact");
        assert_eq!(body["requires_review"], true);
        assert!(
            body["explanation"]
                .as_str()
                .unwrap()
                .contains("strong negative")
        );

        let rows = store
            .memory_feedback_list_by_target(&ctx, remote_id, target_id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].note.as_ref().unwrap().contains("strong negative"));
    }

    #[tokio::test]
    async fn remote_feedback_stop_signal_surfaces_halt_guidance() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let remote_id = Uuid::new_v4();
        let target_id = Uuid::new_v4();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "feedback_record",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "target_id": target_id.to_string(),
                    "source_namespace": "gpu_builds",
                    "scope": "linux",
                    "feedback": "stop"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(result);

        assert_eq!(body["feedback_type"], "stop_signal");
        assert_eq!(body["halt_current_chain"], true);
        assert!(body["guidance"].as_str().unwrap().contains("halt"));
    }

    #[tokio::test]
    async fn remote_usage_mark_rejects_forged_tenant() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        let err = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "usage_mark",
                "arguments": {
                    "tenant_id": Uuid::new_v4().to_string(),
                    "remote_id": Uuid::new_v4().to_string(),
                    "target_id": Uuid::new_v4().to_string(),
                    "source_namespace": "gpu_builds",
                    "scope": "linux",
                    "usage": "confirmed"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, INVALID_PARAMS);
        assert!(err.1.contains("authenticated tenant"));
    }

    #[tokio::test]
    async fn remote_trust_update_repeated_strong_negative_persists_scoped_policy() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let remote_id = Uuid::new_v4();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "trust_update",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "remote_id": remote_id.to_string(),
                    "source_namespace": "deployment_info",
                    "scope": "linux",
                    "reinforcements": ["strong_negative", "strong_negative"]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let body = unwrap_tool_result(result);

        assert_eq!(body["source_namespace"], "deployment_info");
        assert_eq!(body["scope"], "linux");
        assert_eq!(body["policy_persisted"], true);

        let facts = store.remote_policy_list(&ctx, remote_id).await.unwrap();
        assert_eq!(facts.len(), 1);
        match &facts[0].kind {
            crate::remotes::types::RemotePolicyKind::Deny(deny) => {
                assert_eq!(deny.namespace, "deployment_info");
                assert_eq!(deny.deny, "not_trusted_for:linux");
            }
            other => panic!("expected deny policy, got {other:?}"),
        }
    }

    #[test]
    fn batch_mutation_handlers_use_bounded_concurrency() {
        let source = include_str!("dispatch.rs");
        assert!(
            source.contains("const BATCH_MUTATION_CONCURRENCY: usize"),
            "batch mutation handlers need one named concurrency budget"
        );

        for handler in [
            "handle_batch_update_entities",
            "handle_batch_delete_entities",
            "handle_batch_update_edges",
            "handle_batch_delete_edges",
        ] {
            let start = source
                .find(&format!("async fn {handler}"))
                .unwrap_or_else(|| panic!("{handler} should exist"));
            let tail = &source[start..];
            let end = tail.find("\nasync fn ").unwrap_or(tail.len());
            let body = &tail[..end];
            assert!(
                body.contains(".buffer_unordered(BATCH_MUTATION_CONCURRENCY)"),
                "{handler} should run storage mutations with bounded concurrency"
            );
        }
    }

    fn test_entity(
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
        entity_type: &str,
        properties: Value,
    ) -> crate::types::EntityEntry {
        crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id,
            entity_name: name.into(),
            entity_type: entity_type.into(),
            context_snippet: format!("{name} context"),
            confidence: 1.0,
            created_at: chrono::Utc::now(),
            properties,
            ..Default::default()
        }
    }

    // T-FORGET-003 (wiring): the forget tool routes propose → confirm through
    // dispatch, and refuses a call with neither query nor token.
    #[tokio::test]
    async fn forget_tool_propose_confirm_roundtrip_via_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::nil();

        // Seed an entity to forget (direct put, like the module tests).
        let entry = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "outbound port exhaustion".to_string(),
            entity_type: "concept".to_string(),
            source_fold_id: None,
            context_snippet: "sockets held on sleep".to_string(),
            entity_embedding: None,
            confidence: 1.0,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            description: None,
            description_embedding: None,
            tags: Vec::new(),
            properties: serde_json::Value::Null,
            content_hash: None,
            updated_at: None,
            scope: Default::default(),
            ingested_by_session: None,
        };
        store.entity_put(&ctx, &entry).await.unwrap();

        // Propose (read-only) returns a token + candidates.
        let propose = unwrap_tool_result(
            dispatch(
                "tools/call",
                serde_json::json!({
                    "name": "forget",
                    "arguments": { "query": "outbound port exhaustion", "session_id": sid.to_string() }
                }),
                &store,
                &ctx,
                &session,
            )
            .await
            .unwrap(),
        );
        let token = propose["forget_token"].as_str().expect("forget_token");
        let candidates = propose["candidates"].as_array().expect("candidates");
        assert!(!candidates.is_empty(), "expected a candidate to forget");
        let object_id = candidates[0]["object_id"].as_str().unwrap().to_string();

        // Confirm (retract) the selected candidate.
        let confirm = unwrap_tool_result(
            dispatch(
                "tools/call",
                serde_json::json!({
                    "name": "forget",
                    "arguments": {
                        "forget_token": token,
                        "selected_ids": [object_id],
                        "confirm": true
                    }
                }),
                &store,
                &ctx,
                &session,
            )
            .await
            .unwrap(),
        );
        let forgotten = confirm["forgotten"].as_array().expect("forgotten");
        assert_eq!(forgotten.len(), 1);
        assert_eq!(forgotten[0]["outcome"], "retracted");

        // A forget call with neither query nor token is rejected.
        let err = dispatch(
            "tools/call",
            serde_json::json!({ "name": "forget", "arguments": {} }),
            &store,
            &ctx,
            &session,
        )
        .await;
        assert!(err.is_err(), "forget with no query/token must error");
    }

    #[tokio::test]
    async fn list_entities_filters_properties_across_sessions_by_default() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let live_session = Uuid::new_v4();
        let session = SessionState::default();

        let nil_task = test_entity(
            &ctx,
            Uuid::nil(),
            "nil ready task",
            "task",
            serde_json::json!({"status": "ready", "assignee": "claude"}),
        );
        let live_task = test_entity(
            &ctx,
            live_session,
            "live ready task",
            "task",
            serde_json::json!({"status": "ready", "assignee": "claude"}),
        );
        let blocked_task = test_entity(
            &ctx,
            Uuid::nil(),
            "blocked task",
            "task",
            serde_json::json!({"status": "blocked", "assignee": "claude"}),
        );
        store.entity_put(&ctx, &nil_task).await.unwrap();
        store.entity_put(&ctx, &live_task).await.unwrap();
        store.entity_put(&ctx, &blocked_task).await.unwrap();

        let params = serde_json::json!({
            "name": "list_entities",
            "arguments": {
                "session_id": live_session.to_string(),
                "entity_type": "task",
                "filters": { "status": "ready", "assignee": "claude" },
                "limit": 50
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let names: std::collections::HashSet<_> = result["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entity| entity["entity_name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(result["count"], 2);
        assert!(names.contains("nil ready task"));
        assert!(names.contains("live ready task"));
        assert!(!names.contains("blocked task"));
    }

    #[tokio::test]
    async fn list_entities_can_stay_session_scoped() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let live_session = Uuid::new_v4();
        let session = SessionState::default();

        store
            .entity_put(
                &ctx,
                &test_entity(
                    &ctx,
                    Uuid::nil(),
                    "nil ready task",
                    "task",
                    serde_json::json!({"status": "ready"}),
                ),
            )
            .await
            .unwrap();
        store
            .entity_put(
                &ctx,
                &test_entity(
                    &ctx,
                    live_session,
                    "live ready task",
                    "task",
                    serde_json::json!({"status": "ready"}),
                ),
            )
            .await
            .unwrap();

        let params = serde_json::json!({
            "name": "list_entities",
            "arguments": {
                "session_id": live_session.to_string(),
                "entity_type": "task",
                "filters": { "status": "ready" },
                "include_cross_session": false
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 1);
        assert_eq!(result["entities"][0]["entity_name"], "live ready task");
        assert_eq!(result["scope"], "session");
    }

    #[tokio::test]
    async fn hybrid_search_cross_session_includes_legacy_nil_session() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let live_session = Uuid::new_v4();
        let session = SessionState::default();

        store
            .entity_put(
                &ctx,
                &test_entity(
                    &ctx,
                    Uuid::nil(),
                    "beam actor model task",
                    "task",
                    serde_json::json!({"status": "ready"}),
                ),
            )
            .await
            .unwrap();

        // Default search (no scope, no flag) now spans the global + nil corpus,
        // so the legacy nil-session entity is retrievable by a normal agent
        // search (regression fix t_ae8613ff: curated global corpus must be
        // visible by default).
        let default_params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": live_session.to_string(),
                "query": "beam actor model task"
            }
        });
        let default_result = dispatch("tools/call", default_params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(unwrap_tool_result(default_result)["count"], 1);

        // An explicit session-only opt-out still scopes the search to the
        // caller's (empty) session, excluding the nil-session entity.
        let session_only_params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": live_session.to_string(),
                "query": "beam actor model task",
                "include_cross_session": false
            }
        });
        let session_only = dispatch("tools/call", session_only_params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(unwrap_tool_result(session_only)["count"], 0);
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let result = dispatch("initialize", Value::Null, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["serverInfo"]["name"], "ferrosa-memory-mcp");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn run_consolidation_tool_queues_idle_work_instead_of_running_inline() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let session_id = Uuid::new_v4();
        let params = serde_json::json!({
            "name": "run_consolidation",
            "arguments": {"session_id": session_id.to_string()}
        });

        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let body = unwrap_tool_result(result);

        assert_eq!(body["queued"], true);
        assert_eq!(body["session_id"], session_id.to_string());
        assert_eq!(body["run_when"], "idle_or_nightly");
        assert!(session.dirty.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            session
                .consolidation_queue
                .lock()
                .await
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![session_id]
        );
    }

    #[tokio::test]
    async fn consolidation_queue_is_bounded() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        for _ in 0..CONSOLIDATION_QUEUE_CAPACITY {
            let session_id = Uuid::new_v4();
            let params = serde_json::json!({
                "name": "run_consolidation",
                "arguments": {"session_id": session_id.to_string()}
            });
            dispatch("tools/call", params, &store, &ctx, &session)
                .await
                .unwrap();
        }

        let duplicate = session.consolidation_queue.lock().await[0];
        let duplicate_params = serde_json::json!({
            "name": "run_consolidation",
            "arguments": {"session_id": duplicate.to_string()}
        });
        dispatch("tools/call", duplicate_params, &store, &ctx, &session)
            .await
            .unwrap();

        let overflow = Uuid::new_v4();
        let overflow_params = serde_json::json!({
            "name": "run_consolidation",
            "arguments": {"session_id": overflow.to_string()}
        });
        let err = dispatch("tools/call", overflow_params, &store, &ctx, &session)
            .await
            .unwrap_err();

        assert_eq!(err.0, INTERNAL_ERROR);
        assert!(
            err.1.contains("consolidation queue full"),
            "error must name bounded queue backpressure: {err:?}"
        );
        assert_eq!(
            session.consolidation_queue.lock().await.len(),
            CONSOLIDATION_QUEUE_CAPACITY
        );
    }

    #[tokio::test]
    async fn tools_list_returns_tier1_by_default() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let result = dispatch("tools/list", Value::Null, &store, &ctx, &session)
            .await
            .unwrap();
        let tools = result["tools"].as_array().unwrap();
        let expected_tier1 = tool_definitions(&session.entity_types)
            .iter()
            .filter(|tool| is_tier1(&tool.name))
            .count();
        assert_eq!(
            tools.len(),
            expected_tier1,
            "default tools/list should return all tier-1 tools"
        );
        assert_eq!(tools.len(), 22, "tier-1 tool surface should stay compact");
        assert!(
            tools.iter().any(|t| t["name"].as_str() == Some("forget")),
            "forget is a tier-1 tool"
        );

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // All default tools must be compact public names.
        assert!(names.contains(&"all_tools"));
        assert!(names.contains(&"ingest"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"chunk_ctx"));
        assert!(names.contains(&"turn_chain"));
        assert!(names.contains(&"task_current"));
        assert!(names.contains(&"task_put"));
        assert!(names.contains(&"task_done"));
        assert!(names.contains(&"feedback"));
        assert!(names.contains(&"config"));
        assert!(names.contains(&"edge"));
        assert!(names.contains(&"check"));
        assert!(names.contains(&"stats"));
        assert!(names.contains(&"find"));
        assert!(names.contains(&"list"));
        assert!(!names.contains(&"full_list"));

        // Tier-2 tools must NOT be present
        assert!(!names.contains(&"memo"));
        assert!(!names.contains(&"ctx_ingest"));
        assert!(!names.contains(&"edges_add"));
        assert!(!names.contains(&"spread"));
        assert!(!names.contains(&"recurse"));
        assert!(!names.contains(&"promote"));
        assert!(!names.contains(&"derived_cache"));
    }

    #[test]
    fn memory_guide_uses_default_ingest_tool_name() {
        assert!(MEMORY_GUIDE.contains("Use ingest for new knowledge"));
        assert!(!MEMORY_GUIDE.contains("Use smart_ingest"));
    }

    #[test]
    fn default_session_entity_type_schema_includes_eval_and_knowledge_artifacts() {
        let session = SessionState::default();
        for expected in [
            "document",
            "section",
            "benchmark_document",
            "feedback",
            "procedure",
            "policy_preference",
            "eval_run",
            "eval_failure",
            "corpus_manifest",
            "conversation",
            "message",
            "turn",
            "workspace",
            "knowledge_artifact",
        ] {
            assert!(
                session
                    .entity_types
                    .iter()
                    .any(|entity_type| entity_type == expected),
                "default MCP schema must include {expected}"
            );
        }
    }

    #[test]
    fn tool_dispatch_boxes_selected_handler_future() {
        let source = include_str!("dispatch.rs");
        let start = source
            .find("async fn dispatch_tool")
            .expect("dispatch_tool must exist");
        let tail = &source[start..];
        let end = tail
            .find("/// Returns true for tier-1 tools")
            .expect("dispatch_tool section must end before tier-1 helper");
        let dispatch_tool = &tail[..end];

        assert!(
            source.contains("type ToolDispatchFuture"),
            "dispatcher should have an explicit boxed future type alias"
        );
        assert!(
            dispatch_tool.contains("let handler: ToolDispatchFuture<'_> = match canonical_name"),
            "dispatch_tool must box exactly the selected handler future"
        );
        assert!(
            dispatch_tool.contains("Box::pin(handle_check_intentions"),
            "check_intentions must go through the boxed handler path"
        );
        assert!(
            dispatch_tool.contains("let result = handler.await;"),
            "dispatch_tool should await only the boxed selected handler"
        );
    }

    #[tokio::test]
    async fn context_segment_tools_round_trip_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let ingest = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_context_segments",
                "arguments": {
                    "session_id": sid.to_string(),
                    "conversation_id": "discord-thread-123",
                    "embed_missing": false,
                    "segmentation": {"target_tokens": 4, "max_tokens": 8, "time_gap_seconds": 900, "strategy": "deterministic_v1", "semantic_drift_threshold": 0.72},
                    "messages": [
                        {"role": "user", "content": "alpha beta", "turn_index": 0},
                        {"role": "assistant", "content": "gamma delta", "turn_index": 1},
                        {"role": "user", "content": "memory segment retrieval", "turn_index": 2}
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let ingest = unwrap_tool_result(ingest);
        assert!(ingest["segments_created"].as_u64().unwrap() >= 2);

        let search = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "search_context_segments",
                "arguments": {
                    "session_id": sid.to_string(),
                    "query": "memory retrieval",
                    "limit": 1,
                    "expand": {"prev": 1, "next": 1, "max_tokens": 100}
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let search = unwrap_tool_result(search);
        let hits = search["results"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0]["expanded_context"].as_array().unwrap().is_empty());

        let segment_id = hits[0]["segment"]["segment_id"].as_str().unwrap();
        let window = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "get_context_window",
                "arguments": {
                    "session_id": sid.to_string(),
                    "segment_id": segment_id,
                    "prev": 1,
                    "next": 1,
                    "max_tokens": 100
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let window = unwrap_tool_result(window);
        assert!(!window["segments"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn chunk_ctx_expands_document_chunk_neighbors_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let document_id = Uuid::new_v4();
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let now = chrono::Utc::now();

        for (ordinal, id) in ids.iter().enumerate() {
            store
                .document_chunk_put(
                    &ctx,
                    &crate::types::DocumentChunk {
                        tenant_id: ctx.tenant_id,
                        session_id: sid,
                        document_id,
                        chunk_id: *id,
                        ordinal: ordinal as i32,
                        source_doc_id: "doc-1".into(),
                        title: "Chunk Context Test".into(),
                        section_path: "Root".into(),
                        semantic_kind: "paragraph".into(),
                        content: format!("chunk {ordinal} retrieval text"),
                        bm25_text: format!("chunk {ordinal} retrieval text"),
                        chunk_embedding: None,
                        token_count: 4,
                        content_hash: format!("sha256:{ordinal}"),
                        prev_chunk_id: ordinal.checked_sub(1).map(|i| ids[i]),
                        next_chunk_id: ids.get(ordinal + 1).copied(),
                        overlap_from_prev: false,
                        overlap_to_next: false,
                        metadata: serde_json::json!({"test": true}),
                        created_at: now,
                        updated_at: now,
                    },
                )
                .await
                .unwrap();
        }

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "chunk_ctx",
                "arguments": {
                    "session_id": sid.to_string(),
                    "chunk_id": ids[1].to_string(),
                    "prev": 1,
                    "next": 1,
                    "max_tokens": 100
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        let chunks = result["chunks"].as_array().unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0]["ordinal"], 0);
        assert_eq!(chunks[1]["ordinal"], 1);
        assert_eq!(chunks[1]["is_hit"], true);
        assert_eq!(chunks[2]["ordinal"], 2);
        assert_eq!(result["document_id"], document_id.to_string());
    }

    #[tokio::test]
    async fn hybrid_search_expands_global_document_neighbors_with_scope_both() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let live_session = Uuid::new_v4();
        let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let document_id = Uuid::new_v4();
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let now = chrono::Utc::now();

        for (ordinal, id) in ids.iter().enumerate() {
            store
                .document_chunk_put(
                    &ctx,
                    &crate::types::DocumentChunk {
                        tenant_id: ctx.tenant_id,
                        session_id: global_session,
                        document_id,
                        chunk_id: *id,
                        ordinal: ordinal as i32,
                        source_doc_id: "global-doc-1".into(),
                        title: "Global Recall Test".into(),
                        section_path: "Root".into(),
                        semantic_kind: "paragraph".into(),
                        content: match ordinal {
                            0 => "previous chunk has setup context".into(),
                            1 => "needle global corpus target".into(),
                            _ => "next chunk has consequence context".into(),
                        },
                        bm25_text: match ordinal {
                            0 => "previous chunk has setup context".into(),
                            1 => "needle global corpus target".into(),
                            _ => "next chunk has consequence context".into(),
                        },
                        chunk_embedding: None,
                        token_count: 5,
                        content_hash: format!("sha256:global:{ordinal}"),
                        prev_chunk_id: ordinal.checked_sub(1).map(|i| ids[i]),
                        next_chunk_id: ids.get(ordinal + 1).copied(),
                        overlap_from_prev: false,
                        overlap_to_next: false,
                        metadata: serde_json::json!({"test": true}),
                        created_at: now,
                        updated_at: now,
                    },
                )
                .await
                .unwrap();
        }

        // Explicit session-only scope excludes the global corpus (negative
        // control). NOTE: the default scope now spans global (t_ae8613ff), so
        // this must opt out explicitly to demonstrate the session boundary.
        let scoped = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "hybrid_search",
                "arguments": {
                    "session_id": live_session.to_string(),
                    "query": "needle global corpus target",
                    "scope": "session",
                    "chunk_expansion": "neighbors",
                    "chunk_prev": 1,
                    "chunk_next": 1,
                    "chunk_max_tokens": 100
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        assert_eq!(unwrap_tool_result(scoped)["count"], 0);

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "hybrid_search",
                "arguments": {
                    "session_id": live_session.to_string(),
                    "query": "needle global corpus target",
                    "scope": "both",
                    "chunk_expansion": "neighbors",
                    "chunk_prev": 1,
                    "chunk_next": 1,
                    "chunk_max_tokens": 100
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 1);
        assert_eq!(result["chunk_expansion"]["expanded_results"], 1);
        assert_eq!(result["chunk_expansion"]["added_chunks"], 2);

        let hit = &result["results"][0];
        assert_eq!(hit["result_type"], "document_chunk");
        assert_eq!(hit["id"], ids[1].to_string());
        let expanded = hit["expanded_context"].as_array().unwrap();
        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[0]["position"], "prev");
        assert_eq!(expanded[0]["ordinal"], 0);
        assert_eq!(expanded[1]["position"], "next");
        assert_eq!(expanded[1]["ordinal"], 2);
    }

    #[tokio::test]
    async fn tools_list_returns_all_with_include_all() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let params = serde_json::json!({ "include_all": true });
        let result = dispatch("tools/list", params, &store, &ctx, &session)
            .await
            .unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            tool_definitions(&session.entity_types).len(),
            "include_all should return all tools"
        );

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Check tier-1 tools still present
        assert!(names.contains(&"all_tools"));
        assert!(names.contains(&"ingest"));
        assert!(!names.contains(&"full_list"));
        assert!(names.contains(&"ctx_ingest"));
        assert!(names.contains(&"ctx_search"));
        assert!(names.contains(&"ctx_window"));
        assert!(names.contains(&"search"));
        // Check tier-2 tools now included
        assert!(names.contains(&"memo"));
        assert!(names.contains(&"ingest_batch"));
        assert!(names.contains(&"ingest_many"));
        assert!(names.contains(&"migrations"));
        assert!(names.contains(&"memo_store"));
        assert!(names.contains(&"plan_write"));
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"plan_update"));
        assert!(names.contains(&"intentions"));
        assert!(names.contains(&"snooze"));
        assert!(names.contains(&"promote"));
        assert!(names.contains(&"demote"));
        assert!(names.contains(&"importance"));
        assert!(names.contains(&"predict"));
        assert!(names.contains(&"spread"));
        assert!(names.contains(&"derived_cache"));
        assert!(names.contains(&"duplicates"));
        assert!(names.contains(&"recurse"));
        assert!(names.contains(&"derive"));
        assert!(names.contains(&"rules"));
        assert!(names.contains(&"claims"));
        assert!(names.contains(&"approvals"));
        assert!(names.contains(&"aliases"));
        assert!(names.contains(&"explain"));
        assert!(names.contains(&"ruleset"));
        assert!(names.contains(&"pred_promote"));
        assert!(names.contains(&"edge"));
        assert!(names.contains(&"edges_add"));
        assert!(names.contains(&"entities_update"));
        assert!(names.contains(&"entities_delete"));
        assert!(names.contains(&"edges_update"));
        assert!(names.contains(&"edges_delete"));
        assert!(names.contains(&"type_counts"));
    }

    #[tokio::test]
    async fn batch_update_entities_updates_rows_and_reports_counts() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let update_id = Uuid::new_v4();
        let unchanged_id = Uuid::new_v4();
        let missing_id = Uuid::new_v4();

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: update_id,
            session_id: sid,
            entity_name: "Original".into(),
            entity_type: "person".into(),
            source_fold_id: Some(Uuid::new_v4()),
            context_snippet: "original context".into(),
            entity_embedding: Some(vec![0.2, 0.4]),
            confidence: 0.7,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            description: Some("old desc".into()),
            tags: vec!["old".into()],
            properties: serde_json::json!({"legacy": true}),
            ..Default::default()
        });

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: unchanged_id,
            session_id: sid,
            entity_name: "NoChange".into(),
            entity_type: "person".into(),
            context_snippet: "same".into(),
            confidence: 0.5,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "batch_update_entities",
                "arguments": {
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "entity_id": update_id.to_string(),
                            "entity_name": "Updated Name",
                            "context_snippet": "updated context",
                            "confidence": 0.9,
                            "state": "silent",
                            "description": "new description",
                            "tags": ["tag-a", "tag-b"],
                            "properties": {"score": 42},
                            "embedding": [0.9, 0.8, 0.7],
                            "source_fold_id": null
                        },
                        {
                            "entity_id": unchanged_id.to_string()
                        },
                        {
                            "entity_id": "not-a-uuid"
                        },
                        {
                            "entity_id": missing_id.to_string()
                        }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["updated"], 1);
        assert_eq!(result["unchanged"], 1);
        assert_eq!(result["not_found"], 1);
        assert_eq!(result["errors"], 1);

        let updated = store
            .entity_get_by_id(&ctx, sid, update_id)
            .await
            .unwrap()
            .expect("updated entity should exist");
        assert_eq!(updated.entity_name, "Updated Name");
        assert_eq!(updated.state, crate::types::MemoryState::Silent);
        assert_eq!(updated.description.as_deref(), Some("new description"));
        assert_eq!(updated.tags, vec!["tag-a", "tag-b"]);
        assert_eq!(updated.properties["score"], 42);
        assert_eq!(updated.source_fold_id, None);
    }

    #[tokio::test]
    async fn batch_delete_entities_hard_deletes_and_reports_counts() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let deleted_id = Uuid::new_v4();
        let already_unavailable_id = Uuid::new_v4();
        let missing_id = Uuid::new_v4();

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: deleted_id,
            session_id: sid,
            entity_name: "ToDelete".into(),
            entity_type: "person".into(),
            context_snippet: "to delete".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: already_unavailable_id,
            session_id: sid,
            entity_name: "AlreadyGone".into(),
            entity_type: "person".into(),
            context_snippet: "already gone".into(),
            confidence: 0.8,
            state: crate::types::MemoryState::Unavailable,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "batch_delete_entities",
                "arguments": {
                    "session_id": sid.to_string(),
                    "entities": [
                        { "entity_id": deleted_id.to_string() },
                        { "entity_id": already_unavailable_id.to_string() },
                        { "entity_id": missing_id.to_string() },
                        { "entity_id": "invalid" }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["deleted"], 2);
        assert_eq!(result["not_found"], 1);
        assert_eq!(result["errors"], 1);

        let after_delete = store.entity_get_by_id(&ctx, sid, deleted_id).await.unwrap();
        assert!(after_delete.is_none());
        let after_unavailable_delete = store
            .entity_get_by_id(&ctx, sid, already_unavailable_id)
            .await
            .unwrap();
        assert!(after_unavailable_delete.is_none());
    }

    #[tokio::test]
    async fn batch_update_edges_reports_upsert_and_update_statuses() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let existing_src = Uuid::new_v4();
        let existing_dst = Uuid::new_v4();
        let missing_src = Uuid::new_v4();
        let missing_dst = Uuid::new_v4();

        let _ = crate::graph_write::create_typed_edge(
            &store,
            &ctx,
            sid,
            existing_src,
            "references",
            existing_dst,
            1.5,
            Some("start".into()),
        )
        .await
        .unwrap();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "batch_update_edges",
                "arguments": {
                    "session_id": sid.to_string(),
                    "edges": [
                        {
                            "src_entity_id": existing_src.to_string(),
                            "dst_entity_id": existing_dst.to_string(),
                            "edge_type": "references",
                            "weight": 1.5,
                            "metadata": "start"
                        },
                        {
                            "src_entity_id": existing_src.to_string(),
                            "dst_entity_id": existing_dst.to_string(),
                            "edge_type": "references",
                            "weight": 2.2
                        },
                        {
                            "src_entity_id": missing_src.to_string(),
                            "dst_entity_id": missing_dst.to_string(),
                            "edge_type": "references",
                            "weight": 0.5
                        },
                        {
                            "src_entity_id": "not-a-uuid",
                            "dst_entity_id": missing_dst.to_string(),
                            "edge_type": "references"
                        }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["upserted"], 2);
        assert_eq!(result["unchanged"], 1);
        assert_eq!(result["errors"], 1);

        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["status"], "unchanged");
        assert_eq!(results[1]["status"], "updated");
        assert_eq!(results[2]["status"], "upserted");
        assert_eq!(results[3]["status"], "error");
    }

    #[tokio::test]
    async fn batch_delete_edges_hard_deletes_and_reports_missing() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let existing_src = Uuid::new_v4();
        let existing_dst = Uuid::new_v4();

        let _ = crate::graph_write::create_typed_edge(
            &store,
            &ctx,
            sid,
            existing_src,
            "references",
            existing_dst,
            1.0,
            None,
        )
        .await
        .unwrap();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "batch_delete_edges",
                "arguments": {
                    "session_id": sid.to_string(),
                    "edges": [
                        {
                            "src_entity_id": existing_src.to_string(),
                            "dst_entity_id": existing_dst.to_string(),
                            "edge_type": "references"
                        },
                        {
                            "src_entity_id": Uuid::new_v4().to_string(),
                            "dst_entity_id": Uuid::new_v4().to_string(),
                            "edge_type": "references"
                        },
                        {
                            "src_entity_id": "invalid",
                            "dst_entity_id": existing_dst.to_string(),
                            "edge_type": "references"
                        }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["deleted"], 1);
        assert_eq!(result["missing"], 1);
        assert_eq!(result["invalid"], 1);
        assert_eq!(result["errors"], 0);

        let remaining = store
            .typed_edge_list_from(&ctx, sid, existing_src)
            .await
            .unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn ingest_entities_upserts_entities_and_skips_duplicate_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["bug".into(), "document".into()],
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let first = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "id": a.to_string(),
                            "name": "Bug A",
                            "entity_type": "bug",
                            "context": "first bug",
                            "attrs": {"severity": "high"}
                        },
                        {
                            "id": b.to_string(),
                            "name": "Doc B",
                            "entity_type": "document",
                            "context": "supporting doc"
                        }
                    ],
                    "edges": [
                        {
                            "src_id": a.to_string(),
                            "dst_id": b.to_string(),
                            "edge_type": "references",
                            "metadata": {"source": "batch"}
                        }
                    ],
                    "options": {
                        "embed_missing": false
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let first = unwrap_tool_result(first);
        assert_eq!(first["entities"]["inserted"], 2);
        assert_eq!(first["entities"]["updated"], 0);
        assert_eq!(first["edges"]["inserted"], 1);
        assert_eq!(first["progress"]["bounded"], true);
        assert_eq!(first["progress"]["total_items"], 3);
        assert_eq!(
            first["progress"]["events"]
                .as_array()
                .expect("progress events must be an array")
                .len(),
            4
        );
        assert_eq!(store.entities.lock().await.len(), 2);
        assert_eq!(store.typed_edges.lock().await.len(), 1);

        let second = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "id": a.to_string(),
                            "name": "Bug A",
                            "entity_type": "bug",
                            "context": "updated bug context",
                            "confidence": 0.5,
                            "attrs": {"severity": "critical"}
                        },
                        {
                            "id": b.to_string(),
                            "name": "Doc B",
                            "entity_type": "document",
                            "context": "supporting doc updated"
                        }
                    ],
                    "edges": [
                        {
                            "src_id": a.to_string(),
                            "dst_id": b.to_string(),
                            "edge_type": "references"
                        }
                    ],
                    "options": {
                        "embed_missing": false,
                        "on_conflict": "update"
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let second = unwrap_tool_result(second);
        assert_eq!(second["entities"]["inserted"], 0);
        assert_eq!(second["entities"]["updated"], 2);
        assert_eq!(second["edges"]["inserted"], 0);
        assert_eq!(second["edges"]["skipped_duplicate"], 1);

        let stored = store
            .entity_get_by_id(&ctx, sid, a)
            .await
            .unwrap()
            .expect("entity a should exist");
        assert_eq!(stored.context_snippet, "updated bug context");
        assert_eq!(stored.properties["severity"], "critical");
        assert_eq!(store.typed_edges.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn ingest_entities_chains_conversation_turns() {
        // The Hermes harness ingests turns as entity_type="conversation_turn".
        // handle_ingest_entities must auto-chain them into next_turn temporal
        // edges, exactly like canonical "turn" entities (t_9c78b122).
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["conversation_turn".into()],
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();

        // Two separate ingests in the same session (as the real harness does),
        // so the second turn's created_at is strictly after the first's.
        for (id, name) in [(t1, "turn one"), (t2, "turn two")] {
            dispatch(
                "tools/call",
                serde_json::json!({
                    "name": "ingest_entities",
                    "arguments": {
                        "tenant_id": ctx.tenant_id.to_string(),
                        "session_id": sid.to_string(),
                        "entities": [{
                            "id": id.to_string(),
                            "name": name,
                            "entity_type": "conversation_turn",
                            "context": name
                        }],
                        "options": { "embed_missing": false }
                    }
                }),
                &store,
                &ctx,
                &session,
            )
            .await
            .unwrap();
        }

        let edges = store.temporal_edges.lock().await;
        let next = edges
            .iter()
            .find(|e| e.edge_type == "next_turn")
            .expect("ingesting two conversation_turns must create a next_turn edge");
        assert_eq!(
            next.src_id, t1,
            "next_turn must point from the earlier turn"
        );
        assert_eq!(next.dst_id, t2, "...to the later turn");
    }

    #[tokio::test]
    async fn ingest_entities_fails_loudly_when_section_write_is_not_visible() {
        let store = MockStorage::new();
        store
            .silently_drop_entity_put_types
            .lock()
            .await
            .push("section".into());
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["section".into()],
            ..SessionState::default()
        };
        let sid = Uuid::nil();
        let section_id = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "id": section_id.to_string(),
                            "name": "Test Section",
                            "entity_type": "section",
                            "context": "This is a test section entity."
                        }
                    ],
                    "options": {
                        "embed_missing": false,
                        "on_conflict": "update"
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);

        assert_eq!(result["entities"]["inserted"], 0);
        assert_eq!(result["entities"]["updated"], 0);
        let failed = result["entities"]["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["id"], section_id.to_string());
        assert!(
            failed[0]["reason"]
                .as_str()
                .unwrap()
                .contains("not visible after write"),
            "unexpected failure reason: {}",
            failed[0]
        );
    }

    #[tokio::test]
    async fn ingest_entities_persists_section_entities() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["section".into()],
            ..SessionState::default()
        };
        let sid = Uuid::nil();
        let section_id = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "id": section_id.to_string(),
                            "name": "Test Section",
                            "entity_type": "section",
                            "context": "This is a test section entity."
                        }
                    ],
                    "options": {
                        "embed_missing": false,
                        "on_conflict": "update"
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);

        assert_eq!(result["entities"]["inserted"], 1);
        assert_eq!(result["entities"]["failed"], serde_json::json!([]));
        let stored = store
            .entity_get_by_id(&ctx, sid, section_id)
            .await
            .unwrap()
            .expect("section row should be visible after ingest_entities success");
        assert_eq!(stored.entity_type, "section");
        assert_eq!(stored.entity_name, "Test Section");
    }

    #[tokio::test]
    async fn ingest_entities_dry_run_validates_without_writing() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["bug".into()],
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "id": a.to_string(),
                            "name": "Bug A",
                            "entity_type": "bug",
                            "context": "dry run entity"
                        },
                        {
                            "id": b.to_string(),
                            "name": "Bug B",
                            "entity_type": "bug",
                            "context": "dry run entity two"
                        }
                    ],
                    "edges": [
                        {
                            "src_id": a.to_string(),
                            "dst_id": b.to_string(),
                            "edge_type": "references"
                        }
                    ],
                    "options": {
                        "embed_missing": false,
                        "dry_run": true
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["entities"]["inserted"], 2);
        assert_eq!(result["edges"]["inserted"], 1);
        assert_eq!(store.entities.lock().await.len(), 0);
        assert_eq!(store.typed_edges.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn ingest_entities_strict_edges_fail_loudly_for_missing_endpoints() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["bug".into()],
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [],
                    "edges": [
                        {
                            "src_id": Uuid::new_v4().to_string(),
                            "dst_id": Uuid::new_v4().to_string(),
                            "edge_type": "references"
                        }
                    ],
                    "options": {
                        "embed_missing": false,
                        "strict_edges": true
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        let failed = result["edges"]["failed"].as_array().unwrap();
        assert_eq!(result["edges"]["inserted"], 0);
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0]["reason"]
                .as_str()
                .unwrap()
                .contains("endpoint_not_found"),
            "expected endpoint_not_found failure, got: {}",
            failed[0]
        );
    }

    #[tokio::test]
    async fn ingest_entities_rejects_tenant_mismatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let err = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": Uuid::new_v4().to_string(),
                    "session_id": Uuid::new_v4().to_string(),
                    "entities": []
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS);
        assert!(err.1.contains("tenant_id"));
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let err = dispatch("bogus/method", Value::Null, &store, &ctx, &session)
            .await
            .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_call_unknown_tool() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let params = serde_json::json!({ "name": "nonexistent_tool" });
        let err = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_call_missing_params() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let params = serde_json::json!({
            "name": "check_memo_cache",
            "arguments": {}
        });
        let err = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn memo_round_trip_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        // Store
        let store_params = serde_json::json!({
            "name": "store_memo_result",
            "arguments": {
                "prompt": "test prompt",
                "context_slice": "ctx",
                "model_version": "v1",
                "result": "cached answer"
            }
        });
        let result = dispatch("tools/call", store_params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["stored"], true);

        // Check
        let check_params = serde_json::json!({
            "name": "check_memo_cache",
            "arguments": {
                "prompt": "test prompt",
                "context_slice": "ctx",
                "model_version": "v1"
            }
        });
        let result = dispatch("tools/call", check_params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["hit"], true);
        assert_eq!(result["result"], "cached answer");
    }

    #[tokio::test]
    async fn plan_round_trip_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Write
        let write_params = serde_json::json!({
            "name": "write_plan_node",
            "arguments": {
                "session_id": sid.to_string(),
                "depth": 0,
                "subtask_id": "root",
                "goal_text": "solve the problem"
            }
        });
        let result = dispatch("tools/call", write_params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["written"], true);

        // Get
        let get_params = serde_json::json!({
            "name": "get_plan_context",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", get_params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["nodes"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn session_task_round_trip_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let put_params = serde_json::json!({
            "name": "task_put",
            "arguments": {
                "session_id": sid.to_string(),
                "title": "Keep active task durable",
                "description": "Recover foreground task after compaction",
                "alias_scope": "thread:test",
                "alias": "current",
                "tags": ["continuity"]
            }
        });
        let result = dispatch("tools/call", put_params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["generated_task_id"], true);
        let task_id = result["task"]["task_id"].as_str().unwrap().to_string();

        let current_params = serde_json::json!({
            "name": "task_current",
            "arguments": { "session_id": sid.to_string() }
        });
        let current = dispatch("tools/call", current_params, &store, &ctx, &session)
            .await
            .unwrap();
        let current = unwrap_tool_result(current);
        assert_eq!(current["foreground"]["task_id"], task_id);

        let done_params = serde_json::json!({
            "name": "task_done",
            "arguments": {
                "session_id": sid.to_string(),
                "task_id": task_id,
                "outcome_summary": "done"
            }
        });
        let done = dispatch("tools/call", done_params, &store, &ctx, &session)
            .await
            .unwrap();
        let done = unwrap_tool_result(done);
        assert_eq!(done["task"]["status"], "completed");
    }

    #[tokio::test]
    async fn smart_ingest_creates_on_new_content() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Ferrosa uses LSM-tree storage with S3 tiering",
                "entity_type": "concept"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["action"], "Created");
        assert!(result["entity_id"].is_string());
    }

    #[tokio::test]
    async fn set_intention_and_check() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            repo: {
                let l = std::sync::OnceLock::new();
                let _ = l.set("/test/repo".to_string());
                l
            },
            ..SessionState::default()
        };

        // Set
        let params = serde_json::json!({
            "name": "set_intention",
            "arguments": {
                "description": "Review auth error handling",
                "trigger": { "type": "Topic", "keywords": ["auth", "authentication"] },
                "priority": "high"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["intention_id"].is_string());

        // Check — no match
        let params = serde_json::json!({
            "name": "check_intentions",
            "arguments": { "context": "working on database" }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["triggered"].as_array().unwrap().len(), 0);

        // Check — match
        let params = serde_json::json!({
            "name": "check_intentions",
            "arguments": { "context": "now looking at auth middleware" }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["triggered"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_foresight_stores_a_time_bounded_fact() {
        use crate::storage::Storage;
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = uuid::Uuid::new_v4();

        let params = serde_json::json!({
            "name": "set_foresight",
            "arguments": {
                "content": "code freeze until release",
                "valid_until": "2099-01-01T00:00:00Z",
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["fact_id"].is_string());

        let facts = store.foresight_list_session(&ctx, sid).await.unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "code freeze until release");
        assert!(facts[0].valid_until.is_some());
        // Valid until 2099 -> active now, so retrieval would surface it.
        assert!(facts[0].is_valid_at(chrono::Utc::now()));

        // A malformed timestamp is rejected.
        let bad = serde_json::json!({
            "name": "set_foresight",
            "arguments": { "content": "x", "valid_until": "not-a-date" }
        });
        let bad_result = dispatch("tools/call", bad, &store, &ctx, &session).await;
        assert!(
            bad_result.is_err()
                || unwrap_tool_result(bad_result.unwrap())
                    .get("isError")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            "malformed valid_until should be an error"
        );
    }

    #[tokio::test]
    async fn temporal_fact_round_trip() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let entity_id = Uuid::new_v4();

        // Write a temporal fact
        let params = serde_json::json!({
            "name": "write_temporal_fact",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_id": entity_id.to_string(),
                "fact_text": "Alice is VP of Engineering",
                "confidence": 0.95
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["event_id"].is_string());

        // Get the current fact
        let params = serde_json::json!({
            "name": "get_temporal_chain",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_id": entity_id.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["fact_text"], "Alice is VP of Engineering");
        assert_eq!(result["confidence"], 0.95);

        // Get for unknown entity returns null fact
        let unknown = Uuid::new_v4();
        let params = serde_json::json!({
            "name": "get_temporal_chain",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_id": unknown.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["fact"].is_null());
    }

    #[tokio::test]
    async fn explore_connections_related_entities_cql_fallback() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default(); // graph is None — uses CQL fallback
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "traversal": "related_entities",
                "entity_id": Uuid::new_v4().to_string()
            }
        });
        // Should succeed with empty results (CQL fallback, no edges in mock)
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 0);
    }

    #[tokio::test]
    async fn explore_connections_cql_fallback_returns_typed_edge_neighbors() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let source = test_entity(&ctx, session_id, "source", "concept", serde_json::json!({}));
        let destination = test_entity(
            &ctx,
            session_id,
            "destination",
            "concept",
            serde_json::json!({}),
        );
        store.entity_put(&ctx, &source).await.unwrap();
        store.entity_put(&ctx, &destination).await.unwrap();
        crate::graph_write::create_typed_edge(
            &store,
            &ctx,
            session_id,
            source.entity_id,
            "references",
            destination.entity_id,
            0.75,
            None,
        )
        .await
        .unwrap();

        let session = SessionState::default(); // graph is None — uses CQL fallback
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "traversal": "related_entities",
                "session_id": session_id.to_string(),
                "entity_id": source.entity_id.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 1);
        let first = result["results"][0].as_str().unwrap();
        let first: Value = serde_json::from_str(first).unwrap();
        assert_eq!(first["entity_id"], destination.entity_id.to_string());
        assert_eq!(first["entity_name"], "destination");
        assert_eq!(first["edge_type"], "references");
    }

    #[tokio::test]
    async fn find_memory_chain_returns_typed_edge_path() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let source = Uuid::new_v4();
        let destination = Uuid::new_v4();
        crate::graph_write::create_typed_edge(
            &store,
            &ctx,
            session_id,
            source,
            "references",
            destination,
            0.75,
            None,
        )
        .await
        .unwrap();

        let session = SessionState::default();
        let params = serde_json::json!({
            "name": "find_memory_chain",
            "arguments": {
                "session_id": session_id.to_string(),
                "source": source.to_string(),
                "destination": destination.to_string(),
                "max_hops": 2
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["source"], source.to_string());
        assert_eq!(result["destination"], destination.to_string());
        assert_eq!(result["hop_count"], 1);
        assert_eq!(result["steps"][0]["entity_id"], destination.to_string());
        assert_eq!(result["steps"][0]["edge_type"], "references");
    }

    #[tokio::test]
    async fn explore_connections_fold_requires_graph() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default(); // graph is None
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "traversal": "fold_ancestors",
                "fold_id": Uuid::new_v4().to_string()
            }
        });
        let err = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap_err();
        assert_eq!(err.0, INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn hybrid_search_phonetic_only() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Seed an entity
        let entity = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Alice".into(),
            entity_type: "person".into(),
            source_fold_id: None,
            context_snippet: "Alice is the project lead".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        store.entities.lock().await.push(entity.clone());

        // Search without embedding — only phonetic strategy runs
        let params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Alice",
                "limit": 5,
                "fusion_profile": "all",
                "fusion_weights": {
                    "entity_phonetic": 1.0,
                    "entity_ann": 0.0,
                    "fold_ann": 0.0,
                    "context_bm25": 0.0,
                    "context_ann": 0.0,
                    "document_bm25": 0.0,
                    "document_ann": 0.0,
                    "document_phonetic": 0.0,
                    "datalog_frontier": 0.0
                }
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["count"].as_u64().unwrap() >= 1);
        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["source"], "entity_phonetic");
        assert_eq!(results[0]["result_type"], "entity");
    }

    #[tokio::test]
    async fn hybrid_search_with_embedding() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Seed an entity
        let entity = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Bob".into(),
            entity_type: "person".into(),
            source_fold_id: None,
            context_snippet: "Bob is the CTO".into(),
            entity_embedding: Some(vec![0.1, 0.2, 0.3]),
            confidence: 0.95,
            state: Default::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        store.entities.lock().await.push(entity);

        // Seed a completed fold
        let fold = crate::types::FoldEntry {
            session_id: sid,
            fold_id: Uuid::new_v4(),
            tenant_id: ctx.tenant_id,
            depth: 0,
            parent_fold_id: None,
            raw_trajectory: "Bob discussed architecture".into(),
            fold_summary: Some("Bob architecture discussion summary".into()),
            fold_embedding: Some(vec![0.1, 0.2, 0.3]),
            token_count: 100,
            compression_ratio: Some(0.5),
            status: crate::types::FoldStatus::Folded,
            created_at: chrono::Utc::now(),
            folded_at: Some(chrono::Utc::now()),
        };
        store.folds.lock().await.push(fold);

        // Search with embedding — all 3 strategies can run
        let params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Bob",
                "embedding": [0.1, 0.2, 0.3],
                "limit": 10
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        // Should have results from multiple strategies
        let count = result["count"].as_u64().unwrap();
        assert!(count >= 2, "expected at least 2 results, got {count}");

        // Bob entity should appear (boosted by phonetic + ann fusion)
        let results = result["results"].as_array().unwrap();
        let sources: Vec<&str> = results
            .iter()
            .map(|r| r["source"].as_str().unwrap())
            .collect();
        assert!(
            sources.contains(&"entity_phonetic") || sources.contains(&"entity_ann"),
            "expected entity results"
        );
        assert!(sources.contains(&"fold_ann"), "expected fold results");
    }

    #[tokio::test]
    async fn get_stats_returns_zeroes_for_empty_session() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "get_stats",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["entity_count"], 0);
        assert_eq!(result["fold_count"], 0);
        assert_eq!(result["memo_count"], 0);
        assert_eq!(result["memo_total_hits"], 0);
        assert_eq!(result["memo_hit_rate"], 0.0);
        assert_eq!(result["active_fold_count"], 0);
        assert_eq!(result["folded_count"], 0);
        assert_eq!(result["archived_fold_count"], 0);
        assert_eq!(result["temporal_fact_count"], 0);
        assert_eq!(result["edge_count"], 0);
        assert_eq!(result["intention_count"], 0);
        assert!(result["last_consolidation_status"].is_null());
    }

    #[tokio::test]
    async fn get_stats_reports_last_consolidation_status() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        record_consolidation_queued(&session, sid).await;

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "get_stats",
                "arguments": {
                    "session_id": sid.to_string()
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);

        assert_eq!(
            result["last_consolidation_status"]["session_id"],
            sid.to_string()
        );
        assert_eq!(result["last_consolidation_status"]["status"], "queued");
    }

    #[tokio::test]
    async fn migration_status_returns_binary_schema_status() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "migration_status",
                "arguments": {}
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        let expected = crate::migration::MIGRATIONS
            .iter()
            .map(|m| m.version)
            .max()
            .unwrap_or(crate::migration::PRE_VERSIONING_BASELINE) as u64;

        assert_eq!(result["db_version"], expected);
        assert_eq!(result["binary_version"], expected);
        assert_eq!(result["pending"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn get_stats_without_session_id_counts_default_session_entities() {
        // Regression: get_stats previously returned entity_count=0 hardcoded
        // when session_id was omitted, instead of querying the server-owned
        // runtime/default session that ordinary tools use when session_id is
        // omitted from their arguments. The tools must agree on the
        // implicit session — otherwise ingested entities look like phantoms.
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let session = SessionState {
            default_session_id: Some(sid),
            ..SessionState::default()
        };

        for i in 0..2 {
            let entity = crate::types::EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: Uuid::new_v4(),
                session_id: sid,
                entity_name: format!("DefaultSessionEntity{i}"),
                entity_type: "concept".into(),
                source_fold_id: None,
                context_snippet: format!("entity {i}"),
                entity_embedding: None,
                confidence: 0.9,
                state: Default::default(),
                created_at: chrono::Utc::now(),
                ..Default::default()
            };
            store.entities.lock().await.push(entity);
        }

        let params = serde_json::json!({
            "name": "get_stats",
            "arguments": {}
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(
            result["entity_count"], 2,
            "get_stats with no session_id should count default-session entities, not return 0"
        );
    }

    #[tokio::test]
    async fn memory_metrics_reports_tenant_wide_nodes_edges_and_legacy_nil() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let global_sid = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let nil_sid = Uuid::nil();
        let now = chrono::Utc::now();
        let session = SessionState {
            default_session_id: Some(sid),
            ..SessionState::default()
        };

        for (session_id, name) in [
            (sid, "session entity"),
            (global_sid, "global entity"),
            (nil_sid, "legacy nil entity"),
        ] {
            store.entities.lock().await.push(crate::types::EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: Uuid::new_v4(),
                session_id,
                entity_name: name.into(),
                entity_type: "concept".into(),
                context_snippet: name.into(),
                confidence: 0.9,
                created_at: now,
                ..Default::default()
            });
        }
        store
            .document_chunks
            .lock()
            .await
            .push(crate::types::DocumentChunk {
                tenant_id: ctx.tenant_id,
                session_id: nil_sid,
                document_id: Uuid::new_v4(),
                chunk_id: Uuid::new_v4(),
                ordinal: 0,
                source_doc_id: "legacy-doc".into(),
                title: "legacy doc".into(),
                section_path: String::new(),
                semantic_kind: "text".into(),
                content: "legacy document chunk".into(),
                bm25_text: "legacy document chunk".into(),
                chunk_embedding: None,
                token_count: 3,
                content_hash: "legacy-doc-chunk".into(),
                prev_chunk_id: None,
                next_chunk_id: None,
                overlap_from_prev: false,
                overlap_to_next: false,
                metadata: serde_json::Value::Null,
                created_at: now,
                updated_at: now,
            });
        store
            .context_segments
            .lock()
            .await
            .push(crate::context_segment::ContextSegment {
                tenant_id: ctx.tenant_id,
                session_id: sid,
                segment_id: Uuid::new_v4(),
                source_session: sid,
                source_fold_id: None,
                conversation_id: "metrics-test".into(),
                segment_index: 0,
                start_turn: 0,
                end_turn: 1,
                start_time: Some(now),
                end_time: Some(now),
                segment_text: "session context segment".into(),
                segment_summary: None,
                bm25_text: "session context segment".into(),
                segment_embedding: None,
                token_count: 3,
                content_hash: "session-context-segment".into(),
                prev_segment_id: None,
                next_segment_id: None,
                created_at: now,
            });
        store.folds.lock().await.push(crate::types::FoldEntry {
            session_id: nil_sid,
            fold_id: Uuid::new_v4(),
            tenant_id: ctx.tenant_id,
            depth: 0,
            parent_fold_id: None,
            raw_trajectory: "legacy fold".into(),
            fold_summary: Some("legacy fold".into()),
            fold_embedding: None,
            token_count: 2,
            compression_ratio: None,
            status: crate::types::FoldStatus::Active,
            created_at: now,
            folded_at: None,
        });
        store.memos.lock().await.push(crate::types::MemoEntry {
            content_hash: "memo".into(),
            model_version: "test".into(),
            result: "cached result".into(),
            result_embedding: None,
            hit_count: 0,
            created_at: now,
            last_hit_at: None,
            expires_at: None,
        });
        store
            .temporal_edges
            .lock()
            .await
            .push(crate::context_segment::TemporalEdge {
                tenant_id: ctx.tenant_id,
                session_id: nil_sid,
                src_id: Uuid::new_v4(),
                edge_type: "next".into(),
                dst_id: Uuid::new_v4(),
                relation_time: now,
                ordinal: 0,
                metadata: "{}".into(),
                created_at: now,
            });
        store
            .edge_co_occurs(&ctx, Uuid::new_v4(), Uuid::new_v4(), sid, 0.5)
            .await
            .unwrap();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "memory_metrics",
                "arguments": {}
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);

        assert_eq!(result["scope"], "tenant");
        assert_eq!(result["nodes"]["entities"], 3);
        assert_eq!(result["nodes"]["document_chunks"], 1);
        assert_eq!(result["nodes"]["context_segments"], 1);
        assert_eq!(result["nodes"]["folds"], 1);
        assert_eq!(result["nodes"]["memos"], 1);
        assert_eq!(result["node_count"], 7);
        assert_eq!(result["edges"]["co_occurs"], 1);
        assert_eq!(result["edges"]["temporal_links"], 1);
        assert_eq!(result["edge_count"], 2);
        assert_eq!(
            result["legacy_nil_session"]["included_in_tenant_totals"],
            true
        );
        assert_eq!(result["legacy_nil_session"]["nodes"]["entities"], 1);
        assert_eq!(result["legacy_nil_session"]["nodes"]["document_chunks"], 1);
        assert_eq!(result["legacy_nil_session"]["nodes"]["folds"], 1);
        assert_eq!(result["legacy_nil_session"]["edge_count"], 1);
    }

    #[tokio::test]
    async fn get_stats_structured_content_serializes_stats_payload() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let session = SessionState {
            default_session_id: Some(sid),
            ..SessionState::default()
        };
        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "StructuredStatsEntity".into(),
            entity_type: "concept".into(),
            context_snippet: "entity visible in structured stats".into(),
            confidence: 0.9,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "get_stats",
                "arguments": {}
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();

        assert_eq!(result["structuredContent"]["entity_count"], 1);
        assert_eq!(result["structuredContent"]["tool"], "get_stats");
        assert_eq!(result["structuredContent"]["requested_tool"], "get_stats");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("entity_count")
        );
    }

    #[tokio::test]
    async fn count_entities_by_type_returns_empty_breakdowns_for_empty_session() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "count_entities_by_type",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["total"], 0);
        assert_eq!(result["by_entity_type"], serde_json::json!({}));
        assert_eq!(result["by_state"], serde_json::json!({}));
        assert_eq!(result["by_type_and_state"], serde_json::json!({}));
        assert!(result["duration_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn count_entities_by_type_buckets_known_fixture() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let mut entities = store.entities.lock().await;
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Bug A".into(),
            entity_type: "bug".into(),
            context_snippet: "a".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Bug B".into(),
            entity_type: "bug".into(),
            context_snippet: "b".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Dormant,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Doc".into(),
            entity_type: "document".into(),
            context_snippet: "c".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Fn".into(),
            entity_type: "function".into(),
            context_snippet: "d".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Fn 2".into(),
            entity_type: "function".into(),
            context_snippet: "e".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Silent,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Fn 3".into(),
            entity_type: "function".into(),
            context_snippet: "f".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        drop(entities);

        let params = serde_json::json!({
            "name": "count_entities_by_type",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);

        assert_eq!(result["total"], 6);
        assert_eq!(result["by_entity_type"]["bug"], 2);
        assert_eq!(result["by_entity_type"]["document"], 1);
        assert_eq!(result["by_entity_type"]["function"], 3);
        assert_eq!(result["by_state"]["active"], 4);
        assert_eq!(result["by_state"]["dormant"], 1);
        assert_eq!(result["by_state"]["silent"], 1);
        assert_eq!(result["by_type_and_state"]["bug"]["active"], 1);
        assert_eq!(result["by_type_and_state"]["bug"]["dormant"], 1);
        assert_eq!(result["by_type_and_state"]["document"]["active"], 1);
        assert_eq!(result["by_type_and_state"]["function"]["active"], 2);
        assert_eq!(result["by_type_and_state"]["function"]["silent"], 1);
    }

    #[tokio::test]
    async fn count_entities_by_type_respects_tenant_isolation() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let other_ctx = crate::types::TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "other".into(),
        };
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let mut entities = store.entities.lock().await;
        entities.push(crate::types::EntityEntry {
            tenant_id: other_ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Other Tenant Bug".into(),
            entity_type: "bug".into(),
            context_snippet: "other".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        drop(entities);

        let params = serde_json::json!({
            "name": "count_entities_by_type",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["total"], 0);
        assert_eq!(result["by_entity_type"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn count_entities_by_type_defaults_to_nil_session_without_dirtying_session() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            entity_name: "Nil Session Bug".into(),
            entity_type: "bug".into(),
            context_snippet: "nil".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });

        let params = serde_json::json!({
            "name": "count_entities_by_type",
            "arguments": {}
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["total"], 1);
        assert_eq!(result["by_entity_type"]["bug"], 1);
        assert!(!session.dirty.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn intention_persists_to_storage() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            repo: {
                let l = std::sync::OnceLock::new();
                let _ = l.set("/test/repo".to_string());
                l
            },
            ..SessionState::default()
        };

        // Set an intention — should persist to mock storage
        let params = serde_json::json!({
            "name": "set_intention",
            "arguments": {
                "description": "Check SQL injection patterns",
                "trigger": { "type": "Topic", "keywords": ["sql", "injection"] },
                "priority": "high"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let intention_id = result["intention_id"].as_str().unwrap().to_string();

        // Verify it was persisted to storage
        let stored = store.intentions.lock().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id.to_string(), intention_id);
        assert_eq!(stored[0].description, "Check SQL injection patterns");
        assert_eq!(stored[0].repo, "/test/repo");
        drop(stored);

        // Read back from storage trait (repo-scoped)
        use crate::storage::Storage as _;
        let loaded = store.intention_list(&ctx, "/test/repo").await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id.to_string(), intention_id);

        // Different repo returns empty
        let other = store.intention_list(&ctx, "/other/repo").await.unwrap();
        assert!(other.is_empty());
    }

    #[tokio::test]
    async fn intention_complete_persists_status() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            repo: {
                let l = std::sync::OnceLock::new();
                let _ = l.set("/test/repo".to_string());
                l
            },
            ..SessionState::default()
        };

        // Set an intention
        let params = serde_json::json!({
            "name": "set_intention",
            "arguments": {
                "description": "Review auth module",
                "trigger": { "type": "Topic", "keywords": ["auth"] },
                "priority": "normal"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let intention_id = result["intention_id"].as_str().unwrap().to_string();

        // Trigger it
        let params = serde_json::json!({
            "name": "check_intentions",
            "arguments": { "context": "working on auth middleware" }
        });
        dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();

        // Verify triggered status was persisted
        let stored = store.intentions.lock().await;
        assert_eq!(
            stored[0].status,
            crate::intention::IntentionStatus::Triggered
        );
        drop(stored);

        // Complete it
        let params = serde_json::json!({
            "name": "complete_intention",
            "arguments": { "intention_id": intention_id }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["completed"], true);

        // Verify completed status was persisted
        let stored = store.intentions.lock().await;
        assert_eq!(
            stored[0].status,
            crate::intention::IntentionStatus::Completed
        );
        assert!(stored[0].completed_at.is_some());
    }

    #[tokio::test]
    async fn intention_load_from_storage() {
        use crate::intention::*;
        use crate::storage::Storage as _;

        let store = MockStorage::new();
        let ctx = test_ctx();

        // Simulate pre-existing intentions in storage (from previous session)
        let intention = Intention {
            id: Uuid::new_v4(),
            repo: "/test/repo".into(),
            description: "Previously stored intention".into(),
            trigger: IntentionTrigger::Topic {
                keywords: vec!["rust".into()],
            },
            priority: Priority::High,
            status: IntentionStatus::Pending,
            created_at: chrono::Utc::now(),
            triggered_at: None,
            completed_at: None,
        };
        store.intention_put(&ctx, &intention).await.unwrap();

        // Load from storage into IntentionStore (repo-scoped)
        let loaded = store.intention_list(&ctx, "/test/repo").await.unwrap();
        let mut intention_store = IntentionStore::new();
        intention_store.load(loaded);

        // Verify the loaded intention triggers correctly
        let triggered = intention_store.check("writing rust code", "/test/repo");
        assert_eq!(triggered.len(), 1);
        assert_eq!(triggered[0].description, "Previously stored intention");
    }

    #[test]
    fn retrieval_tracker_stats() {
        let mut tracker = RetrievalTracker::new();
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        // Empty tracker
        assert_eq!(tracker.mean(), 0.0);
        assert_eq!(tracker.stddev(), 0.0);

        // Single entity — stddev stays 0 (need 2+ for variance)
        tracker.record(id_a);
        assert_eq!(tracker.count(&id_a), 1);
        assert_eq!(tracker.mean(), 1.0);
        assert_eq!(tracker.stddev(), 0.0);

        // Two entities with divergent counts
        tracker.record(id_b);
        for _ in 0..9 {
            tracker.record(id_a);
        }
        assert_eq!(tracker.count(&id_a), 10);
        assert_eq!(tracker.count(&id_b), 1);
        assert_eq!(tracker.mean(), 5.5);
        assert!(tracker.stddev() > 0.0);
    }

    #[test]
    fn retrieval_tracker_anomaly_detection() {
        // Simulate a realistic scenario: many baseline entities at low counts,
        // one entity with anomalously high retrieval count.
        let mut tracker = RetrievalTracker::new();
        let config = crate::config::SecurityConfig::default();

        // 20 baseline entities each retrieved once
        let baseline_ids: Vec<Uuid> = (0..20).map(|_| Uuid::new_v4()).collect();
        for &id in &baseline_ids {
            tracker.record(id);
        }

        // One outlier entity retrieved 100 times
        let outlier_id = Uuid::new_v4();
        for _ in 0..100 {
            tracker.record(outlier_id);
        }

        let count = tracker.count(&outlier_id);
        let mean = tracker.mean();
        let stddev = tracker.stddev();

        // Baseline entities should NOT trigger anomaly
        let baseline_count = tracker.count(&baseline_ids[0]);
        assert!(
            !crate::audit::check_anomaly(baseline_count, mean, stddev, &config, None),
            "baseline entity should not be anomalous"
        );

        // Outlier SHOULD trigger anomaly
        assert!(
            crate::audit::check_anomaly(count, mean, stddev, &config, None),
            "outlier count {count} with mean={mean:.1} stddev={stddev:.1} should be anomalous"
        );
    }

    #[tokio::test]
    async fn retrieve_entities_records_to_tracker() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Upsert an entity
        let params = serde_json::json!({
            "name": "upsert_entity",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_name": "TrackedEntity",
                "entity_type": "concept",
                "context_snippet": "testing retrieval tracking",
                "confidence": 0.9
            }
        });
        dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();

        // Retrieve it several times
        let retrieve_params = serde_json::json!({
            "name": "retrieve_entities",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "TrackedEntity",
                "strategy": "phonetic"
            }
        });
        for _ in 0..5 {
            dispatch(
                "tools/call",
                retrieve_params.clone(),
                &store,
                &ctx,
                &session,
            )
            .await
            .unwrap();
        }

        // Verify the tracker recorded all retrievals
        let tracker = session.retrieval_tracker.lock().await;
        let entities = store.entities.lock().await;
        let entity_id = entities
            .iter()
            .find(|e| e.entity_name == "TrackedEntity")
            .expect("entity should exist")
            .entity_id;

        assert_eq!(
            tracker.count(&entity_id),
            5,
            "tracker should record each retrieval"
        );
    }

    #[tokio::test]
    async fn hybrid_search_hint_few_results() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Seed one entity so we get 1 result (< 3)
        let entity = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "HintTest".into(),
            entity_type: "concept".into(),
            source_fold_id: None,
            context_snippet: "testing hint triggers".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        store.entities.lock().await.push(entity);

        let params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "HintTest"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let count = result["count"].as_u64().unwrap();
        assert!((1..3).contains(&count), "expected 1-2 results, got {count}");
        assert!(
            result["_hint"]
                .as_str()
                .unwrap()
                .contains("recursive_explore"),
            "expected _hint suggesting recursive_explore"
        );
    }

    #[tokio::test]
    async fn hybrid_search_hint_no_results() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "NonexistentTopic"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 0);
        assert!(
            result["_hint"]
                .as_str()
                .unwrap()
                .contains("retrieve_entities"),
            "expected _hint suggesting retrieve_entities for zero results"
        );
    }

    #[tokio::test]
    async fn configure_updates_default_retrieval_limit_for_omitted_search_limit() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            retrieval_default_limit: Arc::new(AtomicUsize::new(3)),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        for idx in 0..3 {
            store.entities.lock().await.push(crate::types::EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: Uuid::new_v4(),
                session_id: sid,
                entity_name: format!("LimitTest::{idx}"),
                entity_type: "concept".into(),
                context_snippet: format!("limit test context {idx}"),
                confidence: 0.9,
                created_at: chrono::Utc::now(),
                ..Default::default()
            });
        }

        let initial = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "hybrid_search",
                "arguments": {
                    "session_id": sid.to_string(),
                    "query": "LimitTest"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let initial = unwrap_tool_result(initial);
        assert_eq!(initial["count"], 3);

        let updated = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "config",
                "arguments": {
                    "retrieval_limit": 1
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let updated = unwrap_tool_result(updated);
        assert_eq!(updated["retrieval_limit"], 1);

        let narrowed = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "hybrid_search",
                "arguments": {
                    "session_id": sid.to_string(),
                    "query": "LimitTest"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let narrowed = unwrap_tool_result(narrowed);
        assert_eq!(narrowed["count"], 1);
    }

    #[tokio::test]
    async fn configure_session_start_sets_runtime_session_without_llm_supplied_uuid() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };

        let configured = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "config",
                "arguments": {
                    "session_start": {
                        "agent": "claude",
                        "agent_session_id": "claude-runtime-session-1",
                        "workspace": "/repo/ferrosa-memory"
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let configured = unwrap_tool_result(configured);
        let sid = Uuid::parse_str(configured["session_id"].as_str().unwrap()).unwrap();
        let expected = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            b"ferrosa-memory:agent-session:v1:claude:/repo/ferrosa-memory:claude-runtime-session-1",
        );
        assert_eq!(sid, expected);

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "HookSessionMemory".into(),
            entity_type: "concept".into(),
            context_snippet: "memory written under the hook-configured session".into(),
            confidence: 0.9,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });

        let found = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "hybrid_search",
                "arguments": {
                    "query": "HookSessionMemory"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let found = unwrap_tool_result(found);
        assert_eq!(found["count"], 1);
    }

    #[tokio::test]
    async fn configure_session_start_derives_session_even_when_default_exists() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let default_sid = Uuid::new_v4();
        let session = SessionState {
            ollama_base_url: String::new(),
            default_session_id: Some(default_sid),
            ..SessionState::default()
        };

        let configured = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "config",
                "arguments": {
                    "session_start": {
                        "agent": "codex",
                        "agent_session_id": "codex-ferrosa-suite-leak-check",
                        "workspace": "/Users/bkearns/src/ferrosa-suite"
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let configured = unwrap_tool_result(configured);
        let sid = Uuid::parse_str(configured["session_id"].as_str().unwrap()).unwrap();
        let expected = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            b"ferrosa-memory:agent-session:v1:codex:/Users/bkearns/src/ferrosa-suite:codex-ferrosa-suite-leak-check",
        );

        assert_eq!(sid, expected);
        assert_ne!(sid, default_sid);
        assert_eq!(configured["session_source"], "derived_from_agent_session");
    }

    #[tokio::test]
    async fn hybrid_search_min_score_uses_post_merge_score_for_session_context() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };
        let configured = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "config",
                "arguments": {
                    "session_start": {
                        "agent": "codex",
                        "agent_session_id": "session-context-score-probe",
                        "workspace": "/repo/ferrosa-memory"
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let configured = unwrap_tool_result(configured);
        let sid = Uuid::parse_str(configured["session_id"].as_str().unwrap()).unwrap();
        let segment_id = Uuid::new_v4();
        store
            .context_segments
            .lock()
            .await
            .push(crate::context_segment::ContextSegment {
                tenant_id: ctx.tenant_id,
                session_id: sid,
                segment_id,
                source_session: sid,
                source_fold_id: None,
                conversation_id: "codex:score-probe".into(),
                segment_index: 0,
                start_turn: 0,
                end_turn: 1,
                start_time: None,
                end_time: None,
                segment_text:
                    "SESSION_CONTEXT_SCORE_PROBE should survive final min_score filtering".into(),
                segment_summary: None,
                bm25_text: "SESSION_CONTEXT_SCORE_PROBE should survive final min_score filtering"
                    .into(),
                segment_embedding: None,
                token_count: 12,
                content_hash: segment_id.to_string(),
                prev_segment_id: None,
                next_segment_id: None,
                created_at: chrono::Utc::now(),
            });

        let found = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "hybrid_search",
                "arguments": {
                    "query": "SESSION_CONTEXT_SCORE_PROBE",
                    "scope": "session",
                    "min_score": 0.065
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let found = unwrap_tool_result(found);

        assert_eq!(found["count"], 1);
        assert_eq!(found["results"][0]["id"], segment_id.to_string());
        assert!(found["results"][0]["score"].as_f64().unwrap() >= 0.065);
    }

    #[tokio::test]
    async fn manage_authority_sets_global_reputation_and_pagerank() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let target_id = Uuid::new_v4();

        let updated = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "manage_authority",
                "arguments": {
                    "target_id": target_id.to_string(),
                    "reputation": 1.0,
                    "pagerank": 0.8,
                    "scope": "global",
                    "reason": "curated corpus"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let updated = unwrap_tool_result(updated);
        assert_eq!(updated["count"], 1);
        assert_eq!(updated["scope"], "global");

        let entry = store.warmth_get(&ctx, target_id).await.unwrap().unwrap();
        assert_eq!(
            entry.session_id,
            crate::scope::tenant_global_session_uuid(ctx.tenant_id)
        );
        assert_eq!(entry.reputation, 1.0);
        assert_eq!(entry.pagerank, 0.8);
    }

    #[tokio::test]
    async fn hybrid_search_applies_persisted_global_authority_to_document_chunks() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };
        let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let noisy_id = Uuid::new_v4();
        let curated_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let chunk = |chunk_id, ordinal, content: &str| crate::types::DocumentChunk {
            tenant_id: ctx.tenant_id,
            session_id: global_session,
            document_id: chunk_id,
            chunk_id,
            ordinal,
            source_doc_id: format!("authority-doc-{ordinal}"),
            title: format!("authority doc {ordinal}"),
            section_path: String::new(),
            semantic_kind: "text".into(),
            content: content.into(),
            bm25_text: content.into(),
            chunk_embedding: None,
            token_count: 16,
            content_hash: chunk_id.to_string(),
            prev_chunk_id: None,
            next_chunk_id: None,
            overlap_from_prev: false,
            overlap_to_next: false,
            metadata: serde_json::Value::Null,
            created_at: now,
            updated_at: now,
        };
        store.document_chunks.lock().await.extend([
            chunk(
                noisy_id,
                0,
                "curated authority corpus stale transcript clutter",
            ),
            chunk(
                curated_id,
                1,
                "curated authority corpus trusted skill documentation",
            ),
        ]);

        dispatch(
            "tools/call",
            serde_json::json!({
                "name": "manage_authority",
                "arguments": {
                    "target_ids": [curated_id.to_string()],
                    "reputation": 1.0,
                    "pagerank": 1.0,
                    "scope": "global"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        dispatch(
            "tools/call",
            serde_json::json!({
                "name": "manage_authority",
                "arguments": {
                    "target_id": noisy_id.to_string(),
                    "reputation": -1.0,
                    "scope": "global"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();

        let found = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "hybrid_search",
                "arguments": {
                    "query": "curated authority corpus",
                    "scope": "global",
                    "limit": 2
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let found = unwrap_tool_result(found);
        assert_eq!(found["count"], 1);
        assert_eq!(found["results"][0]["id"], curated_id.to_string());
        let curated_score = found["results"][0]["score"].as_f64().unwrap();
        assert!(curated_score > 0.5);
    }

    #[tokio::test]
    async fn context_ingest_marks_explicit_remember_turns_as_authoritative() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_context_segments",
                "arguments": {
                    "session_id": sid.to_string(),
                    "conversation_id": "remember-authority-test",
                    "embed_missing": false,
                    "messages": [
                        {
                            "role": "user",
                            "content": "Please remember PROJECT_AUTHORITY_SIGNAL_42: curated corpus should win.",
                            "turn_index": 0
                        },
                        {
                            "role": "assistant",
                            "content": "I will remember PROJECT_AUTHORITY_SIGNAL_42.",
                            "turn_index": 1
                        }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        let segment_id =
            Uuid::parse_str(result["segments"][0]["segment_id"].as_str().unwrap()).unwrap();

        let entry = store.warmth_get(&ctx, segment_id).await.unwrap().unwrap();
        assert_eq!(entry.session_id, sid);
        assert_eq!(entry.reputation, 1.0);
        assert_eq!(entry.pagerank, 0.85);
        assert!(
            result["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning
                    .as_str()
                    .unwrap()
                    .contains("authority_seeded_for_explicit_remember"))
        );
    }

    #[tokio::test]
    async fn forget_confirm_applies_negative_authority_to_forgotten_and_dependents() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let victim = test_entity(
            &ctx,
            sid,
            "obsolete endpoint secret",
            "concept",
            serde_json::json!({}),
        );
        let dependent = test_entity(
            &ctx,
            sid,
            "client behavior depending on obsolete endpoint secret",
            "concept",
            serde_json::json!({}),
        );
        let victim_id = victim.entity_id;
        let dependent_id = dependent.entity_id;
        store.entity_put(&ctx, &victim).await.unwrap();
        store.entity_put(&ctx, &dependent).await.unwrap();
        store
            .typed_edges
            .lock()
            .await
            .push(crate::types::TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id: sid,
                src_id: dependent_id,
                edge_type: "depends_on".into(),
                dst_id: victim_id,
                weight: 1.0,
                metadata: None,
                created_at: chrono::Utc::now(),
            });

        let propose = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "forget",
                "arguments": {
                    "session_id": sid.to_string(),
                    "query": "obsolete endpoint secret"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let propose = unwrap_tool_result(propose);
        let token = propose["forget_token"].as_str().unwrap();

        let confirmed = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "forget",
                "arguments": {
                    "forget_token": token,
                    "selected_ids": [victim_id.to_string()],
                    "confirm": true,
                    "reason": "user said forget obsolete endpoint secret"
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let confirmed = unwrap_tool_result(confirmed);

        assert_eq!(
            confirmed["authority"]["forgotten_hard_negative"][0],
            victim_id.to_string()
        );
        assert_eq!(
            confirmed["authority"]["dependents_demoted"][0],
            dependent_id.to_string()
        );
        let victim_authority = store.warmth_get(&ctx, victim_id).await.unwrap().unwrap();
        let dependent_authority = store.warmth_get(&ctx, dependent_id).await.unwrap().unwrap();
        assert_eq!(victim_authority.reputation, -1.0);
        assert_eq!(victim_authority.pagerank, 0.0);
        assert_eq!(dependent_authority.reputation, -0.35);
    }

    #[tokio::test]
    async fn explore_connections_hint_few_results() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "traversal": "related_entities",
                "entity_id": Uuid::new_v4().to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 0);
        assert!(
            result["_hint"]
                .as_str()
                .unwrap()
                .contains("spread_activation"),
            "expected _hint suggesting spread_activation"
        );
    }

    #[tokio::test]
    async fn smart_ingest_hint_on_created() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Progressive disclosure reduces cognitive load for LLM tool selection",
                "entity_type": "concept"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["action"], "Created");
        assert!(
            result["_hint"].as_str().unwrap().contains("create_edge"),
            "expected _hint suggesting create_edge after Created"
        );
    }

    #[tokio::test]
    async fn smart_ingest_auto_queues_consolidation_after_ten_creates() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        for i in 0..SMART_INGEST_AUTO_CONSOLIDATE_THRESHOLD {
            let params = serde_json::json!({
                "name": "smart_ingest",
                "arguments": {
                    "session_id": sid.to_string(),
                    "content": format!("auto consolidation unique memory {i} for subsystem {}", Uuid::new_v4()),
                    "entity_type": "concept",
                    "entity_name": format!("auto-consolidation-{i}")
                }
            });
            let result = dispatch("tools/call", params, &store, &ctx, &session)
                .await
                .unwrap();
            let body = unwrap_tool_result(result);
            assert_eq!(body["action"], "Created");
            assert_eq!(
                body["auto_consolidation_queued"].as_bool().unwrap(),
                i + 1 == SMART_INGEST_AUTO_CONSOLIDATE_THRESHOLD
            );
        }

        assert!(session.dirty.load(Ordering::Relaxed));
        assert_eq!(
            session
                .consolidation_queue
                .lock()
                .await
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![sid]
        );
        assert_eq!(
            session
                .smart_ingest_created_since_consolidation
                .lock()
                .await
                .get(&sid)
                .copied(),
            Some(0)
        );
    }

    #[tokio::test]
    async fn get_stats_hint_entities_no_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Seed entities but no edges
        for i in 0..3 {
            let entity = crate::types::EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: Uuid::new_v4(),
                session_id: sid,
                entity_name: format!("Entity{i}"),
                entity_type: "concept".into(),
                source_fold_id: None,
                context_snippet: format!("entity {i}"),
                entity_embedding: None,
                confidence: 0.9,
                state: Default::default(),
                created_at: chrono::Utc::now(),
                ..Default::default()
            };
            store.entities.lock().await.push(entity);
        }

        let params = serde_json::json!({
            "name": "get_stats",
            "arguments": { "session_id": sid.to_string() }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["entity_count"].as_u64().unwrap() > 0);
        assert_eq!(result["edge_count"], 0);
        assert!(
            result["_hint"].as_str().unwrap().contains("create_edge"),
            "expected _hint suggesting create_edge when entities exist but no edges"
        );
    }

    #[tokio::test]
    async fn check_intentions_hint_on_triggered() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            repo: {
                let l = std::sync::OnceLock::new();
                let _ = l.set("/test/repo".to_string());
                l
            },
            ..SessionState::default()
        };

        // Set an intention
        let params = serde_json::json!({
            "name": "set_intention",
            "arguments": {
                "description": "Review hint implementation",
                "trigger": { "type": "Topic", "keywords": ["hint", "disclosure"] },
                "priority": "normal"
            }
        });
        dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();

        // Trigger it
        let params = serde_json::json!({
            "name": "check_intentions",
            "arguments": { "context": "implementing progressive disclosure hints" }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["triggered"].as_array().unwrap().len(), 1);
        let hint = result["_hint"].as_str().unwrap();
        assert!(
            hint.contains("complete_intention"),
            "expected _hint suggesting complete_intention"
        );
        assert!(
            hint.contains("1 intentions triggered"),
            "expected _hint to include triggered count"
        );
    }

    #[tokio::test]
    async fn record_outcome_hint_always_present() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "record_outcome",
            "arguments": {
                "session_id": sid.to_string(),
                "query_id": Uuid::new_v4().to_string(),
                "program_type": "hnsw_ann",
                "task_complexity": "simple",
                "succeeded": true,
                "latency_ms": 10,
                "token_cost": 50
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(
            result["_hint"]
                .as_str()
                .unwrap()
                .contains("feedback improves retrieval routing"),
            "expected _hint about routing improvement"
        );
    }

    #[tokio::test]
    async fn record_outcome_hint_retrieval_miss() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Note: record_outcome requires program_type to be one of the enum values.
        // "retrieval_miss" is not in the enum, so we test the regular path here
        // and validate the _hint field is present regardless.
        let params = serde_json::json!({
            "name": "record_outcome",
            "arguments": {
                "session_id": sid.to_string(),
                "query_id": Uuid::new_v4().to_string(),
                "program_type": "phonetic",
                "task_complexity": "simple",
                "succeeded": false,
                "latency_ms": 5,
                "token_cost": 20
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(
            result["_hint"].is_string(),
            "expected _hint field on record_outcome"
        );
    }

    #[tokio::test]
    async fn smart_ingest_embedding_fallback_when_ollama_unavailable() {
        // When ollama_base_url is set but Ollama is not running, smart_ingest
        // should gracefully fall back to phonetic search (no embedding).
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: "http://127.0.0.1:1".to_string(),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Ferrosa uses LSM-tree storage with S3 tiering",
                "entity_type": "concept"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["action"], "Created");
    }

    #[tokio::test]
    async fn smart_ingest_skips_embedding_when_ollama_empty() {
        // When ollama_base_url is empty, no embedding request is attempted
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Test content for embedding skip",
                "entity_type": "concept"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["action"], "Created");
    }

    #[tokio::test]
    async fn smart_ingest_uses_synthetic_embeddings_without_provider_url() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            embed_provider: "synthetic".to_string(),
            ollama_base_url: String::new(),
            embed_dimensions: 8,
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Synthetic embeddings keep CI semantic paths covered",
                "entity_type": "concept",
                "entity_name": "Synthetic Embeddings"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["action"], "Created");

        let entity_id = Uuid::parse_str(result["entity_id"].as_str().unwrap()).unwrap();
        let entity = store
            .entity_get_by_id(&ctx, sid, entity_id)
            .await
            .unwrap()
            .expect("entity should be persisted");
        assert_eq!(entity.entity_embedding.as_ref().unwrap().len(), 8);
    }

    #[tokio::test]
    async fn hybrid_search_embedding_fallback_when_ollama_unavailable() {
        // When ollama_base_url is set but unreachable, hybrid_search falls back
        // to phonetic-only search.
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: "http://127.0.0.1:1".to_string(),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        // First ingest something
        let ingest_params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Ferrosa is a database engine",
                "entity_type": "concept",
                "entity_name": "Ferrosa"
            }
        });
        dispatch("tools/call", ingest_params, &store, &ctx, &session)
            .await
            .unwrap();

        // Search should still work via phonetic even without embeddings
        let search_params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Ferrosa"
            }
        });
        let result = dispatch("tools/call", search_params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(
            result["count"].as_u64().unwrap() > 0,
            "should find via phonetic fallback"
        );
    }

    #[tokio::test]
    async fn record_feedback_reranks_future_searches_for_same_workspace() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();
        let cwd = "/repo/ferrosa-memory";
        let first = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Atlas Cache".into(),
            entity_type: "concept".into(),
            context_snippet: "Atlas cache stores reusable retrieval hints".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        let second = crate::types::EntityEntry {
            entity_id: Uuid::new_v4(),
            context_snippet: "Atlas cache stores reusable retrieval hints".into(),
            ..first.clone()
        };
        store.entities.lock().await.push(first);
        store.entities.lock().await.push(second);

        let search = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Atlas Cache",
                "cwd": cwd,
                "limit": 2,
                // This test asserts feedback accounting/reranking over a fixed
                // two-result entity set. Pin live judge rerank off so host-local
                // judge availability cannot drop or reorder candidates before
                // record_feedback sees them.
                "rerank": false
            }
        });
        let result = dispatch("tools/call", search.clone(), &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        let demoted_id = Uuid::parse_str(results[0]["id"].as_str().unwrap()).unwrap();
        let promoted_id = Uuid::parse_str(results[1]["id"].as_str().unwrap()).unwrap();

        let feedback = serde_json::json!({
            "name": "record_feedback",
            "arguments": {
                "session_id": sid.to_string(),
                "scores": [-1, 1],
                "cwd": cwd
            }
        });
        let feedback = dispatch("tools/call", feedback, &store, &ctx, &session)
            .await
            .unwrap();
        let feedback = unwrap_tool_result(feedback);
        assert_eq!(feedback["updated"].as_array().unwrap().len(), 2);

        let demoted = store
            .entity_get_by_id(&ctx, sid, demoted_id)
            .await
            .unwrap()
            .expect("demoted entity should exist");
        assert!(
            demoted
                .properties
                .get("workspace_feedback")
                .and_then(|value| value.as_object())
                .is_some(),
            "feedback should be persisted on the entity"
        );

        let reranked = dispatch("tools/call", search, &store, &ctx, &session)
            .await
            .unwrap();
        let reranked = unwrap_tool_result(reranked);
        let reranked_results = reranked["results"].as_array().unwrap();
        assert_eq!(
            reranked_results[0]["id"].as_str().unwrap(),
            promoted_id.to_string(),
            "positive feedback should lift this entity for the same cwd"
        );
    }

    #[tokio::test]
    async fn record_feedback_records_abstentions_separately_from_neutral_scores() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();
        let cwd = "/repo/ferrosa-memory";
        let first = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Judge Abstention".into(),
            entity_type: "concept".into(),
            context_snippet: "Judge abstention should be tracked separately".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        let second = crate::types::EntityEntry {
            entity_id: Uuid::new_v4(),
            entity_name: "Judge Neutral".into(),
            context_snippet: "Judge neutral should not be treated as abstention".into(),
            ..first.clone()
        };
        store.entities.lock().await.push(first.clone());
        store.entities.lock().await.push(second.clone());

        let search = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Judge",
                "cwd": cwd,
                "limit": 2,
                // This test asserts feedback accounting over a fixed two-result
                // set. With judge rerank enabled by default, hybrid_search would
                // make a live LLM call to the configured judge (config.base_url),
                // which can reorder/veto candidates — non-hermetic and host-
                // dependent. Pin rerank off so the setup is deterministic; the
                // judge-authority drop behavior is covered by its own tests.
                "rerank": false
            }
        });
        let result = dispatch("tools/call", search, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        let abstained_id = Uuid::parse_str(results[0]["id"].as_str().unwrap()).unwrap();
        let neutral_id = Uuid::parse_str(results[1]["id"].as_str().unwrap()).unwrap();

        let feedback = serde_json::json!({
            "name": "record_feedback",
            "arguments": {
                "session_id": sid.to_string(),
                "scores": ["-", 0],
                "judge": "test_judge",
                "cwd": cwd
            }
        });
        let feedback = dispatch("tools/call", feedback, &store, &ctx, &session)
            .await
            .unwrap();
        let feedback = unwrap_tool_result(feedback);
        assert_eq!(feedback["abstained"], 1);
        assert_eq!(feedback["updated"].as_array().unwrap()[0]["score"], "-");
        assert_eq!(feedback["updated"].as_array().unwrap()[1]["score"], 0);

        let abstained = store
            .entity_get_by_id(&ctx, sid, abstained_id)
            .await
            .unwrap()
            .expect("entity should exist");
        let feedback = abstained
            .properties
            .pointer("/workspace_feedback")
            .and_then(Value::as_object)
            .expect("workspace feedback should exist");
        let entry = feedback.values().next().unwrap();
        assert_eq!(entry["abstentions"], 1);
        assert_eq!(entry["neutrals"].as_i64().unwrap_or(0), 0);
        assert_eq!(
            entry["mechanisms"]["entity_phonetic"]["judges"]["test_judge"]["abstentions"],
            1
        );

        let neutral = store
            .entity_get_by_id(&ctx, sid, neutral_id)
            .await
            .unwrap()
            .expect("entity should exist");
        let feedback = neutral
            .properties
            .pointer("/workspace_feedback")
            .and_then(Value::as_object)
            .expect("workspace feedback should exist");
        let entry = feedback.values().next().unwrap();
        assert_eq!(entry["neutrals"], 1);
        assert_eq!(entry["abstentions"].as_i64().unwrap_or(0), 0);
        assert_eq!(
            entry["mechanisms"]["entity_phonetic"]["judges"]["test_judge"]["neutrals"],
            1
        );
    }

    #[tokio::test]
    async fn hybrid_search_preserves_all_results_when_judge_unavailable() {
        // Judge rerank is on by default. If the judge endpoint can't be reached,
        // hybrid_search must SKIP the rerank and return every candidate — it must
        // never drop results just because the judge is down. Regression guard for
        // the judge-default-on flip (the judge has authority to remove results, so
        // an unreachable judge must not be allowed to silently shrink the set).
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };
        {
            let mut judge = session.judge_config.lock().await;
            judge.enabled = true;
            judge.base_url = String::new(); // unreachable judge -> rerank errors -> skip
        }
        let sid = Uuid::new_v4();
        let first = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Judge One".into(),
            entity_type: "concept".into(),
            context_snippet: "Judge one candidate".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        let second = crate::types::EntityEntry {
            entity_id: Uuid::new_v4(),
            entity_name: "Judge Two".into(),
            context_snippet: "Judge two candidate".into(),
            ..first.clone()
        };
        store.entities.lock().await.push(first);
        store.entities.lock().await.push(second);

        let search = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Judge",
                "limit": 2
            }
        });
        let result = dispatch("tools/call", search, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let results = result["results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            2,
            "judge unavailable must skip rerank and preserve all candidates, not drop any"
        );
    }

    #[test]
    fn parse_llm_rerank_order_accepts_object_array_and_indices() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let ids = vec![first, second, third];

        let raw = format!(
            r#"Here is the JSON: {{"order":[{{"id":"{second}"}}, "not-a-candidate", 1, "{third}"]}}"#
        );
        let parsed = parse_llm_rerank_order(&raw, &ids);

        assert_eq!(parsed, vec![second, first, third]);
    }

    #[test]
    fn parse_llm_judge_scores_preserves_abstentions() {
        let raw = r#"{"order":[1,2,3,4],"scores":[1,"-",0,-1]}"#;
        let parsed = parse_llm_judge_scores(raw, 5);
        assert_eq!(parsed, vec![Some(1), None, Some(0), Some(-1), None]);
        assert_eq!(
            Value::Array(judge_scores_for_response(&parsed)),
            serde_json::json!([1, "-", 0, -1, "-"])
        );
    }

    #[test]
    fn parse_llm_judge_scores_accepts_sparse_rank_object() {
        let raw = r#"{"order":[3,1],"scores":{"1":1,"3":-1,"5":"-"}}"#;
        let parsed = parse_llm_judge_scores(raw, 5);

        assert_eq!(parsed, vec![Some(1), None, Some(-1), None, None]);
    }

    #[test]
    fn rerank_candidate_content_includes_expanded_chunks() {
        let id = Uuid::new_v4();
        let result = crate::hybrid_search::SearchResult {
            id,
            source: "document_bm25".into(),
            memory_kind: "semantic".into(),
            content: "hit chunk".into(),
            score: 1.0,
            result_type: "document_chunk".into(),
            document_id: Some(id),
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: vec![crate::hybrid_search::ExpandedChunkContext {
                chunk_id: Uuid::new_v4(),
                document_id: id,
                ordinal: 2,
                position: "next".into(),
                distance: 1,
                token_count: 5,
                section_path: "section".into(),
                content: "neighbor answer text".into(),
            }],
        };

        let content = rerank_candidate_content(&result);

        assert!(content.contains("hit chunk"));
        assert!(content.contains("Expanded neighboring chunks"));
        assert!(content.contains("neighbor answer text"));
    }

    #[test]
    fn rerank_candidate_content_compacts_episodic_transcript_json() {
        let id = Uuid::new_v4();
        let raw = r#"user[0]: {"parentUuid":"noise","message":{"role":"user","content":[{"type":"tool_result","content":"FMT_NOW_CLEAN\n M ferrosa/src/main.rs"}]},"toolUseResult":{"stdout":"FMT_NOW_CLEAN\n M ferrosa/src/main.rs"}}"#;
        let result = crate::hybrid_search::SearchResult {
            id,
            source: "context_bm25".into(),
            memory_kind: "episodic".into(),
            content: raw.into(),
            score: 1.0,
            result_type: "context_segment".into(),
            document_id: None,
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        };

        let content = rerank_candidate_content(&result);

        assert!(content.contains("tool result: FMT_NOW_CLEAN"));
        assert!(!content.contains("parentUuid"));
        assert!(!content.contains("user[0]"));
    }

    #[test]
    fn build_query_variants_heuristic_adds_bounded_keyword_and_evidence_queries() {
        let variants = build_query_variants(
            "How does BRCA1 repair DNA damage in breast cancer cells?",
            "heuristic",
            "bright_pro",
            &[],
            5,
        )
        .unwrap();

        assert_eq!(
            variants.first().map(String::as_str),
            Some("How does BRCA1 repair DNA damage in breast cancer cells?")
        );
        assert!(variants.len() <= 5);
        assert!(variants.iter().any(|query| query.contains("BRCA1")));
        assert!(
            variants
                .iter()
                .any(|query| query.starts_with("documents explaining "))
        );
        let unique = variants.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), variants.len());
    }

    #[test]
    fn parse_llm_subqueries_accepts_compact_json_and_dedups_original() {
        let raw = r#"{
            "queries": [
                "BRCA1 DNA repair breast cancer",
                "support documents for homologous recombination",
                "BRCA1 DNA repair breast cancer"
            ]
        }"#;
        let variants = build_llm_query_variants(
            "How does BRCA1 repair DNA damage in breast cancer cells?",
            raw,
            4,
        );

        assert_eq!(
            variants,
            vec![
                "How does BRCA1 repair DNA damage in breast cancer cells?".to_string(),
                "BRCA1 DNA repair breast cancer".to_string(),
                "support documents for homologous recombination".to_string(),
            ]
        );
    }

    #[test]
    fn memorybench_query_decomposition_prompt_extracts_question_subject() {
        let query = "You are Sheldon.\n\n[Question] Which online game hooked Penny?\n[Answer]";

        assert_eq!(
            query_decomposition_subject(query, "memorybench"),
            "Which online game hooked Penny?"
        );

        let prompt = query_decomposition_user_prompt(query, "memorybench", 5);
        assert!(prompt.contains("Query subject:\nWhich online game hooked Penny?"));
        assert!(prompt.contains("Do not write SQL"));
    }

    #[test]
    fn build_query_variants_rejects_unknown_task_and_mode() {
        assert!(build_query_variants("query", "unknown", "general", &[], 5).is_err());
        assert!(build_query_variants("query", "heuristic", "unknown", &[], 5).is_err());
    }

    #[test]
    fn merge_query_variant_outputs_promotes_hits_seen_by_multiple_variants() {
        let shared = Uuid::new_v4();
        let original_only = Uuid::new_v4();
        let variant_only = Uuid::new_v4();
        let result = |id, label: &str| crate::hybrid_search::SearchResult {
            id,
            source: "document_bm25".into(),
            memory_kind: "semantic".into(),
            content: label.into(),
            score: 1.0,
            result_type: "document_chunk".into(),
            document_id: Some(id),
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        };
        let original = crate::hybrid_search::SearchOutput {
            results: vec![
                result(original_only, "original only"),
                result(shared, "shared"),
            ],
            diagnostics: crate::hybrid_search::SearchDiagnostics {
                requested_limit: 2,
                source_limit: 2,
                total_candidates: 2,
                unique_candidates: 2,
                sources: vec![],
            },
        };
        let variant = crate::hybrid_search::SearchOutput {
            results: vec![
                result(variant_only, "variant only"),
                result(shared, "shared"),
            ],
            diagnostics: crate::hybrid_search::SearchDiagnostics {
                requested_limit: 2,
                source_limit: 2,
                total_candidates: 2,
                unique_candidates: 2,
                sources: vec![],
            },
        };

        let merged = merge_query_variant_outputs(
            vec![
                QueryVariantSearchOutput {
                    query: "original".into(),
                    output: original,
                    embedding_status: "provided".into(),
                },
                QueryVariantSearchOutput {
                    query: "variant".into(),
                    output: variant,
                    embedding_status: "provided".into(),
                },
            ],
            3,
        );

        assert_eq!(merged.results.first().map(|result| result.id), Some(shared));
        assert_eq!(merged.diagnostics.queries.len(), 2);
        assert_eq!(merged.diagnostics.unique_results, 3);
    }

    #[test]
    fn apply_result_filters_drops_post_merge_low_scores_and_wrong_kinds() {
        let keep = Uuid::new_v4();
        let low = Uuid::new_v4();
        let episodic = Uuid::new_v4();
        let result = |id, score, kind: &str| crate::hybrid_search::SearchResult {
            id,
            source: "document_bm25".into(),
            memory_kind: kind.into(),
            content: "candidate".into(),
            score,
            result_type: "document_chunk".into(),
            document_id: Some(id),
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        };
        let mut results = vec![
            result(keep, 0.07, "semantic"),
            result(low, 0.065574, "semantic"),
            result(episodic, 0.08, "episodic"),
        ];

        apply_result_filters(
            &mut results,
            Some(0.067),
            Some(&["semantic".into(), "procedural".into()]),
        );

        assert_eq!(
            results.iter().map(|result| result.id).collect::<Vec<_>>(),
            vec![keep]
        );
    }

    #[test]
    fn apply_llm_judge_authority_boosts_positive_and_removes_negative() {
        let positive = Uuid::new_v4();
        let negative = Uuid::new_v4();
        let abstain = Uuid::new_v4();
        let result = |id, score| crate::hybrid_search::SearchResult {
            id,
            source: "document_bm25".into(),
            memory_kind: "semantic".into(),
            content: "candidate".into(),
            score,
            result_type: "document_chunk".into(),
            document_id: Some(id),
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        };
        let mut results = vec![
            result(negative, 0.50),
            result(positive, 0.40),
            result(abstain, 0.30),
        ];
        let report = LlmRerankReport {
            enabled: true,
            applied: true,
            mode: "single".into(),
            provider: "mock".into(),
            model: "mock".into(),
            candidate_count: 3,
            returned_ids: vec![positive, negative, abstain],
            judged_ids: vec![positive, negative, abstain],
            judge_scores: vec![Some(1), Some(-1), None],
            score_sum: 0,
            abstentions: 1,
            batches: Vec::new(),
            error: None,
        };

        apply_llm_judge_authority(&mut results, &report);

        assert_eq!(
            results.iter().map(|result| result.id).collect::<Vec<_>>(),
            vec![positive, abstain]
        );
        assert!(results[0].score > 0.59);
        assert!(results[1].score < 0.30);
    }

    #[test]
    fn apply_llm_rerank_order_preserves_omitted_candidates_and_tail() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let fourth = Uuid::new_v4();
        let result = |id, content: &str| crate::hybrid_search::SearchResult {
            id,
            source: "entity_ann".into(),
            memory_kind: "semantic".into(),
            content: content.into(),
            score: 1.0,
            result_type: "entity".into(),
            document_id: None,
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        };
        let reranked = apply_llm_rerank_order(
            vec![
                result(first, "first"),
                result(second, "second"),
                result(third, "third"),
                result(fourth, "fourth"),
            ],
            &[third, first],
            3,
        );
        let ids = reranked.into_iter().map(|r| r.id).collect::<Vec<_>>();

        assert_eq!(ids, vec![third, first, second, fourth]);
    }

    #[test]
    fn apply_llm_rerank_decision_promotes_positive_and_demotes_negative_scores() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let fourth = Uuid::new_v4();
        let result = |id, content: &str| crate::hybrid_search::SearchResult {
            id,
            source: "document_bm25".into(),
            memory_kind: "semantic".into(),
            content: content.into(),
            score: 1.0,
            result_type: "document_chunk".into(),
            document_id: Some(id),
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        };

        let reranked = apply_llm_rerank_decision(
            vec![
                result(first, "first"),
                result(second, "second"),
                result(third, "third"),
                result(fourth, "fourth"),
            ],
            &[third, first, second],
            &[Some(0), Some(-1), Some(1)],
            3,
            5,
        );
        let ids = reranked.into_iter().map(|r| r.id).collect::<Vec<_>>();

        assert_eq!(ids, vec![third, first, second, fourth]);
    }

    #[test]
    fn apply_llm_rerank_decision_keeps_abstentions_below_positive_candidates() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let result = |id, content: &str| crate::hybrid_search::SearchResult {
            id,
            source: "document_bm25".into(),
            memory_kind: "semantic".into(),
            content: content.into(),
            score: 1.0,
            result_type: "document_chunk".into(),
            document_id: Some(id),
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        };

        let reranked = apply_llm_rerank_decision(
            vec![
                result(first, "first"),
                result(second, "second"),
                result(third, "third"),
            ],
            &[second, third, first],
            &[None, Some(1), Some(0)],
            3,
            5,
        );
        let ids = reranked.into_iter().map(|r| r.id).collect::<Vec<_>>();

        assert_eq!(ids, vec![second, third, first]);
    }

    #[test]
    fn apply_llm_rerank_decision_uses_order_when_positive_scores_are_sparse() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let result = |id, content: &str| crate::hybrid_search::SearchResult {
            id,
            source: "document_bm25".into(),
            memory_kind: "semantic".into(),
            content: content.into(),
            score: 1.0,
            result_type: "document_chunk".into(),
            document_id: Some(id),
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        };

        let reranked = apply_llm_rerank_decision(
            vec![
                result(first, "first"),
                result(second, "second"),
                result(third, "third"),
            ],
            &[third, second],
            &[None, Some(1), None],
            3,
            5,
        );
        let ids = reranked.into_iter().map(|r| r.id).collect::<Vec<_>>();

        assert_eq!(ids, vec![third, second, first]);
    }

    #[test]
    fn rank_batches_by_winner_order_keeps_unranked_batches_stable() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let fourth = Uuid::new_v4();

        let ranked = rank_batches_by_winner_order(&[first, second, third, fourth], &[third, first]);

        assert_eq!(ranked, vec![2, 0, 1, 3]);
    }

    #[tokio::test]
    async fn record_outcome_retrieval_miss_penalizes_entity_reputation() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let eid1 = Uuid::new_v4();
        let eid2 = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "record_outcome",
            "arguments": {
                "session_id": sid.to_string(),
                "query_id": Uuid::new_v4().to_string(),
                "program_type": "retrieval_miss",
                "task_complexity": "simple",
                "succeeded": false,
                "latency_ms": 100,
                "token_cost": 50,
                "entity_ids": [eid1.to_string(), eid2.to_string()]
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        serde_json::to_string(&result)
            .expect("record_outcome CallToolResult should remain JSON-serializable");
        let result = unwrap_tool_result(result);
        assert_eq!(result["recorded"], true);
        assert_eq!(result["warmth_updated"], true);
        assert_eq!(result["entity_ids_updated"].as_array().unwrap().len(), 2);
        assert!(result["invalid_entity_ids"].as_array().unwrap().is_empty());

        // Both entities should have received -0.10 reputation penalty (failure with entity_ids)
        let w1 = store.warmth_get(&ctx, eid1).await.unwrap().unwrap();
        assert!(
            (w1.reputation - (-0.10)).abs() < 0.001,
            "expected -0.10 reputation, got {}",
            w1.reputation
        );
        let w2 = store.warmth_get(&ctx, eid2).await.unwrap().unwrap();
        assert!(
            (w2.reputation - (-0.10)).abs() < 0.001,
            "expected -0.10 reputation, got {}",
            w2.reputation
        );
    }
}

#[cfg(test)]
mod speculative_tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use crate::types::TenantContext;
    use uuid::Uuid;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    fn unwrap_tool_result(result: Value) -> Value {
        let text = result["content"][0]["text"]
            .as_str()
            .expect("CallToolResult missing content[0].text");
        serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
    }

    #[tokio::test]
    async fn predict_needed_returns_empty_with_no_history() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "predict_needed",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 0);
        assert_eq!(result["recent_entity_count"], 0);
        assert!(result["predictions"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn predict_needed_with_targeted_recent() {
        let session = SessionState::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        // Build co-access history: a-b are strongly co-accessed
        {
            let mut co = session.co_access.lock().await;
            for _ in 0..5 {
                co.record(a);
                co.record(b);
            }
            // a-c weakly co-accessed
            co.record(a);
            co.record(c);
        }

        // Only mark 'a' as recently retrieved
        {
            let mut tracker = session.retrieval_tracker.lock().await;
            tracker.record(a);
        }

        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "predict_needed",
            "arguments": {
                "session_id": sid.to_string(),
                "threshold": 0.0,
                "limit": 10
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let predictions = result["predictions"].as_array().unwrap();
        assert!(!predictions.is_empty(), "should predict b and/or c");
        // b should be predicted with highest confidence
        let first = &predictions[0];
        assert_eq!(first["entity_id"].as_str().unwrap(), b.to_string());
        assert!((first["confidence"].as_f64().unwrap() - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn predict_needed_validates_params() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        // session_id is now optional — calling without it should succeed (empty predictions)
        let params = serde_json::json!({
            "name": "predict_needed",
            "arguments": {}
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["predictions"].is_array());
    }

    #[tokio::test]
    async fn anomaly_emits_event_on_bus() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Subscribe to event bus before triggering anomaly
        let mut rx = session.event_bus.subscribe();

        // Create 20 distinct entities to establish a wide baseline
        for i in 0..20 {
            let params = serde_json::json!({
                "name": "upsert_entity",
                "arguments": {
                    "session_id": sid.to_string(),
                    "entity_name": format!("Baseline{i:02}"),
                    "entity_type": "concept",
                    "context_snippet": "baseline entity",
                    "confidence": 0.9
                }
            });
            dispatch("tools/call", params, &store, &ctx, &session)
                .await
                .unwrap();
        }

        // Retrieve all baseline entities once each to seed tracker
        for i in 0..20 {
            let params = serde_json::json!({
                "name": "retrieve_entities",
                "arguments": {
                    "session_id": sid.to_string(),
                    "query": format!("Baseline{i:02}"),
                    "strategy": "phonetic"
                }
            });
            dispatch("tools/call", params, &store, &ctx, &session)
                .await
                .unwrap();
        }

        // Now retrieve one entity many more times to create a clear outlier.
        // With 20 entities at count 1 and outlier at count N, mean ~= N/21,
        // stddev driven mostly by the outlier. We need count > mean + 3*stddev.
        // At count=50 with 20 baselines at 1: mean=50+20/21=3.33, variance high
        // enough that the outlier itself will exceed 3-sigma once the distribution
        // has enough data points at the baseline.
        let outlier_params = serde_json::json!({
            "name": "retrieve_entities",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Baseline00",
                "strategy": "phonetic"
            }
        });
        for _ in 0..60 {
            dispatch("tools/call", outlier_params.clone(), &store, &ctx, &session)
                .await
                .unwrap();
        }

        // Drain events and check for at least one AnomalyDetected
        let mut found_anomaly = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, crate::viz::VizEvent::AnomalyDetected { .. }) {
                found_anomaly = true;
                let json = serde_json::to_string(&event).unwrap();
                assert!(json.contains("AnomalyDetected"));
                assert!(json.contains("Baseline00"));
                break;
            }
        }
        assert!(found_anomaly, "expected AnomalyDetected event on bus");
    }

    // --- Session ID resolution (bug: explore_connections rejects non-UUID session_id) ---
    //
    // The dispatcher injects the configured default_session_id when the caller
    // passes a placeholder (missing, null, empty, literal "default") or an
    // invalid UUID. Callers should never silently get Uuid::nil() scope.

    #[test]
    fn resolve_session_id_injects_default_when_absent() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({});
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_injects_default_when_null() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": null });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_injects_default_when_empty_string() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": "" });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_injects_default_when_literal_default() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": "default" });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_injects_default_when_invalid_uuid() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": "not-a-uuid" });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_passes_through_valid_uuid() {
        let default_sid = Uuid::new_v4();
        let caller_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": caller_sid.to_string() });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], caller_sid.to_string());
    }

    #[test]
    fn resolve_session_id_fails_loud_when_invalid_and_no_default() {
        let mut args = serde_json::json!({ "session_id": "default" });
        let err = resolve_session_id(&mut args, None).unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS);
        assert!(
            err.1.contains("session_id") && err.1.contains("default"),
            "error should name the field and bad value, got: {}",
            err.1
        );
    }

    #[test]
    fn resolve_session_id_leaves_absent_alone_when_no_default() {
        // No default and no caller value — let the handler decide whether to
        // error on missing session_id. (Some tools don't need one.)
        let mut args = serde_json::json!({});
        resolve_session_id(&mut args, None).unwrap();
        assert!(args.get("session_id").is_none());
    }

    #[test]
    fn resolve_session_id_handles_non_object_args() {
        // Defensive: some tool calls arrive with Value::Null as arguments.
        let mut args = Value::Null;
        resolve_session_id(&mut args, Some(Uuid::new_v4())).unwrap();
    }

    // Schema invariants: after the refactor-session-id-schemas sweep, only
    // delete_session (destructive) may keep `format:uuid` and `required: session_id`.
    // Every other tool must advertise session_id as a plain optional string so
    // the dispatcher's resolve_session_id fallback can fire for "default", "",
    // or omitted values.

    const STRICT_SESSION_TOOLS: &[&str] = &["delete_session"];

    #[test]
    fn tool_schemas_do_not_require_uuid_format_on_session_id() {
        let tools = tool_definitions(&["person".to_string()]);
        for tool in &tools {
            if STRICT_SESSION_TOOLS.contains(&tool.name.as_str()) {
                continue;
            }
            let sid = &tool.input_schema["properties"]["session_id"];
            if sid.is_null() {
                continue;
            }
            assert_ne!(
                sid.get("format"),
                Some(&serde_json::json!("uuid")),
                "tool {}: session_id must not carry format:uuid — blocks config fallback in strict MCP clients",
                tool.name
            );
        }
    }

    #[test]
    fn tool_schemas_do_not_list_session_id_as_required() {
        let tools = tool_definitions(&["person".to_string()]);
        for tool in &tools {
            if STRICT_SESSION_TOOLS.contains(&tool.name.as_str()) {
                continue;
            }
            let required = tool
                .input_schema
                .get("required")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            assert!(
                !required.iter().any(|v| v == "session_id"),
                "tool {}: session_id must not be in required — server falls back to default_session_id",
                tool.name
            );
        }
    }

    #[test]
    fn delete_session_schema_stays_strict() {
        let tools = tool_definitions(&["person".to_string()]);
        let tool = tools.iter().find(|t| t.name == "delete_session").unwrap();
        let sid = &tool.input_schema["properties"]["session_id"];
        assert_eq!(
            sid.get("format"),
            Some(&serde_json::json!("uuid")),
            "delete_session must keep format:uuid to prevent accidental fallback to default on typo"
        );
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v == "session_id"),
            "delete_session must keep session_id as required"
        );
    }

    #[tokio::test]
    async fn explore_connections_accepts_default_string_when_session_configured() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let default_sid = Uuid::new_v4();
        let session = SessionState {
            default_session_id: Some(default_sid),
            ..SessionState::default()
        };
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "session_id": "default",
                "traversal": "related_entities",
                "entity_id": Uuid::new_v4().to_string(),
                "max_depth": 1
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session).await;
        assert!(
            result.is_ok(),
            "expected 'default' to resolve to configured session, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn edge_write_timeout_reports_ann_warmup_hint() {
        let result: Result<(), (i32, String)> = edge_write_with_timeout_budget(
            "create_edge",
            std::time::Duration::from_millis(1),
            std::future::pending::<anyhow::Result<()>>(),
        )
        .await;

        let err = result.expect_err("pending edge write should time out");
        assert_eq!(err.0, INTERNAL_ERROR);
        assert!(
            err.1.contains("Ferrosa may still be warming ANN indexes"),
            "timeout should explain likely ANN warmup cause: {}",
            err.1
        );
        assert!(
            err.1.contains("get_stats"),
            "timeout should include an actionable readiness probe: {}",
            err.1
        );
    }
}
