//! Shared domain types used across modules.
//!
//! These types represent the core data structures that flow through the system:
//! tool parameters, tool results, and internal representations of stored data.

use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Tenant-scoped session context. Threaded through every tool handler.
/// `tenant_id` is extracted from authentication — never client-supplied.
#[derive(Debug, Clone)]
pub struct TenantContext {
    pub tenant_id: Uuid,
    pub session_origin: String,
}

/// Status of a plan node in the hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    Active,
    Complete,
    Failed,
}

/// A node in the plan tree, as stored and returned by plan tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanNode {
    pub session_id: Uuid,
    pub depth: i32,
    pub subtask_id: String,
    pub parent_subtask: Option<String>,
    pub goal_text: String,
    pub status: PlanStatus,
    pub outcome_summary: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Durable lifecycle state for a session task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionTaskStatus {
    Pending,
    InProgress,
    Blocked,
    Completed,
    Cancelled,
    Superseded,
}

impl SessionTaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionTaskStatus::Completed
                | SessionTaskStatus::Cancelled
                | SessionTaskStatus::Superseded
        )
    }
}

/// Client identity carried with a task for scoped aliases and cross-client
/// recovery. These fields are descriptive; tenant/session remain authoritative.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTaskClient {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_session_id: Option<String>,
}

/// A durable, fmem-owned work item for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTask {
    pub session_id: Uuid,
    pub task_id: Uuid,
    pub title: String,
    pub description: String,
    pub status: SessionTaskStatus,
    pub priority: i32,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<Uuid>,
    pub focus_rank: i32,
    pub client: SessionTaskClient,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_summary: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A scoped, caller-friendly alias that resolves to a canonical fmem task id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTaskAlias {
    pub session_id: Uuid,
    pub alias_scope: String,
    pub alias: String,
    pub task_id: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Explicit focus stack entry. Lower `stack_index` is closer to the foreground.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTaskFocusEntry {
    pub session_id: Uuid,
    pub stack_index: i32,
    pub task_id: Uuid,
    pub reason: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Append-only audit/recovery event for task lifecycle changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTaskEvent {
    pub session_id: Uuid,
    pub event_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Per-session behavior knobs for deterministic v1 task observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTaskPolicy {
    pub session_id: Uuid,
    pub auto_task_detection: String,
    pub auto_resume: String,
    pub max_active_before_subagents: i32,
    pub max_children_before_subagents: i32,
    pub confidence_threshold: f64,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Default for SessionTaskPolicy {
    fn default() -> Self {
        Self {
            session_id: Uuid::nil(),
            auto_task_detection: "suggest".to_string(),
            auto_resume: "ask".to_string(),
            max_active_before_subagents: 5,
            max_children_before_subagents: 4,
            confidence_threshold: 0.72,
            updated_at: chrono::Utc::now(),
        }
    }
}

/// A memoized sub-call result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoEntry {
    pub content_hash: String,
    pub model_version: String,
    pub result: String,
    pub result_embedding: Option<Vec<f32>>,
    pub hit_count: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_hit_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Result of checking the memo cache.
#[derive(Debug, Serialize, Deserialize)]
pub struct MemoCheckResult {
    pub hit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_count: Option<i64>,
}

/// Result of storing a memo entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct MemoStoreResult {
    pub stored: bool,
    pub content_hash: String,
}

// --- Fold types (Sprint 2) ---

/// Status of a trajectory fold.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FoldStatus {
    Active,
    Folded,
    Archived,
}

/// A trajectory fold entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldEntry {
    pub session_id: Uuid,
    pub fold_id: Uuid,
    pub tenant_id: Uuid,
    pub depth: i32,
    pub parent_fold_id: Option<Uuid>,
    pub raw_trajectory: String,
    pub fold_summary: Option<String>,
    pub fold_embedding: Option<Vec<f32>>,
    pub token_count: i32,
    pub compression_ratio: Option<f64>,
    pub status: FoldStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub folded_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Summary returned by fold retrieval (without raw trajectory by default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldSummary {
    pub fold_id: Uuid,
    pub depth: i32,
    pub fold_summary: String,
    pub token_count: i32,
    pub similarity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_trajectory: Option<String>,
}

// --- Entity types (Sprint 3) ---

/// Memory lifecycle state inspired by vestige's cognitive memory model.
///
/// Memories transition through states based on usage:
/// - Active: frequently accessed, returned in searches
/// - Dormant: still available but lower priority
/// - Silent: not returned in normal searches, available on explicit request
/// - Unavailable: logically deleted, retained for audit trail only
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryState {
    #[default]
    Active,
    Dormant,
    Silent,
    Unavailable,
}

impl std::fmt::Display for MemoryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Dormant => write!(f, "dormant"),
            Self::Silent => write!(f, "silent"),
            Self::Unavailable => write!(f, "unavailable"),
        }
    }
}

impl MemoryState {
    /// Whether an entity in this state may be returned from recall/search.
    ///
    /// `Unavailable` is "logically deleted, audit-only" — the state a retracted
    /// (forgotten) entity is moved to — and is excluded from all recall. The
    /// other states remain retrievable (`Dormant`/`Silent` only affect ranking
    /// today), so this filter is the single guard that hides forgotten memory
    /// without changing demotion semantics.
    pub fn is_retrievable(&self) -> bool {
        !matches!(self, Self::Unavailable)
    }
}

/// A retraction record: audit + restore metadata for a soft-forgotten object.
/// Persisted to the `retraction` table; mirrors ddl/038_retraction_record.cql.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetractionRecord {
    pub object_id: Uuid,
    pub object_type: String,
    pub session_id: Uuid,
    pub retracted_at: chrono::DateTime<chrono::Utc>,
    pub reason: String,
    pub actor: String,
    /// State the object was in before retraction, so restore can put it back.
    pub prior_state: String,
    pub restorable_until: chrono::DateTime<chrono::Utc>,
    pub forget_id: Uuid,
}

/// A forget-journal entry: the durable, replayable record of a single forget
/// operation and its per-step progress. Mirrors `ddl/037_forget_journal.cql`.
///
/// The journal is the source of truth for atomicity: a multi-target /
/// multi-store forget writes this entry FIRST (single partition, the one place
/// durability is required) with `status = "in_progress"`, then drives
/// disposition of the item + edges + temporal links + derived rows, advancing
/// `step_states` and finally marking `status = "completed"`. A crash mid-forget
/// leaves an unfinished journal that a resumable sweep can finish or roll back.
///
/// `target_ids` and `step_states` are stored as JSON text columns (CQL has no
/// rich nested types here); the typed accessors below parse them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgetJournalEntry {
    pub tenant_id: Uuid,
    pub forget_id: Uuid,
    /// JSON array of `{object_type, object_id, session_id}` — the forget targets.
    pub target_ids: String,
    /// `"retract"` | `"hard"`.
    pub mode: String,
    /// JSON map `step_name -> "pending"|"done"|"failed"`.
    pub step_states: String,
    /// `"in_progress"` | `"completed"` | `"failed"`.
    pub status: String,
    pub reason: String,
    pub actor: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Scope of an entity: session-local (default) or global to the tenant.
///
/// Session-scoped entities live in their session's partition and are
/// invisible to other sessions. Global-scoped entities live in the tenant's
/// global sentinel partition and are visible to every session.
///
/// Used for shared knowledge (skills, tags, concepts, decisions, code
/// symbols) that should cross session boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EntityScope {
    #[default]
    Session,
    Global,
}

/// A "scene": a durable, summarized cluster of related entities produced by
/// dream consolidation. Retrieval can surface a scene as a single coherent
/// semantic unit (with its member entities) instead of loose individual hits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemScene {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub scene_id: Uuid,
    /// Entities that make up the scene (the cluster members).
    pub member_ids: Vec<Uuid>,
    /// Human-readable summary of what the scene is about.
    pub summary: String,
    /// Centroid of the member entity embeddings (mean vector), enabling semantic
    /// scene matching. `None` when no member carried an embedding.
    #[serde(default)]
    pub scene_embedding: Option<Vec<f32>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl MemScene {
    pub fn member_count(&self) -> i32 {
        self.member_ids.len() as i32
    }
}

/// A time-bounded prospective fact or temporary constraint. Retrieval surfaces
/// it only while it is valid at the query's as-of time, so expired facts and
/// not-yet-active plans don't pollute context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForesightFact {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub fact_id: Uuid,
    pub content: String,
    pub valid_from: Option<chrono::DateTime<chrono::Utc>>,
    pub valid_until: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Grace window kept after a foresight fact's `valid_until` before its storage
/// row is reclaimed, so a just-expired fact stays inspectable (e.g. for audit /
/// `foresight_list_session`) for a short while. Retrieval already hides it via
/// `is_valid_at`, so this only affects raw row lifetime.
pub const FORESIGHT_TTL_GRACE_SECS: i64 = 7 * 24 * 3600; // 7 days

/// Upper bound on the per-row TTL we will set. Beyond this we set no TTL and let
/// the row persist — both to stay within engine TTL limits and to never risk
/// reaping a fact while it is still valid. 20 years is the conservative
/// Cassandra-compatible maximum.
pub const FORESIGHT_TTL_MAX_SECS: i64 = 630_720_000; // 20 years

impl ForesightFact {
    /// True when `now` is within `[valid_from, valid_until]` (either bound open).
    pub fn is_valid_at(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.valid_from.is_none_or(|from| from <= now)
            && self.valid_until.is_none_or(|until| now <= until)
    }

    /// Per-row storage TTL (seconds) to set on write so an expired fact is
    /// reclaimed by the engine instead of accumulating forever. Returns:
    /// - `None` for an open-ended fact (no `valid_until`) — it must persist, and
    ///   for a window so far out the TTL would exceed [`FORESIGHT_TTL_MAX_SECS`]
    ///   (capping there would risk reaping a still-valid fact);
    /// - `Some(secs > 0)` otherwise: time until `valid_until`, plus the grace
    ///   window, clamped to at least 1s (CQL requires a positive TTL, so an
    ///   already-long-expired fact is reaped almost immediately).
    ///
    /// This is a storage-reclamation optimization layered under the read-time
    /// `is_valid_at` filter, which remains the correctness guarantee.
    pub fn storage_ttl_seconds(&self, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
        let until = self.valid_until?;
        let secs = (until - now).num_seconds() + FORESIGHT_TTL_GRACE_SECS;
        if secs > FORESIGHT_TTL_MAX_SECS {
            return None;
        }
        Some(secs.max(1))
    }
}

/// A durable record of one hybrid_search: query, which sources produced
/// candidates, and the returned results. Enables offline learning (tune fusion
/// weights, detect regressions) from successful vs unhelpful retrievals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalTrace {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub trace_id: Uuid,
    pub query: String,
    /// Compact map of candidate source -> count (e.g. {"entity_ann": 18}).
    pub source_counts: std::collections::BTreeMap<String, usize>,
    pub result_ids: Vec<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A compact, durable per-session profile / workspace-state summary derived
/// from consolidation scenes. Injected into retrieval so an agent always gets
/// the session's gist (active entities, repo/branch/task context).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemProfile {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub summary: String,
    pub scene_count: i32,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A named entity discovered during trajectory traversal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityEntry {
    pub tenant_id: Uuid,
    pub entity_id: Uuid,
    pub session_id: Uuid,
    pub entity_name: String,
    pub entity_type: String,
    pub source_fold_id: Option<Uuid>,
    pub context_snippet: String,
    pub entity_embedding: Option<Vec<f32>>,
    pub confidence: f64,
    #[serde(default)]
    pub state: MemoryState,
    pub created_at: chrono::DateTime<chrono::Utc>,

    // --- Richer entity model (Sprint 1) ---
    /// Curated, retrieval-optimized description (distinct from `context_snippet`,
    /// which is the extraction source).
    #[serde(default)]
    pub description: Option<String>,
    /// Embedding of `description` (same model/dimensions as `entity_embedding`).
    #[serde(default)]
    pub description_embedding: Option<Vec<f32>>,
    /// Denormalized tag names — direct tags plus all ancestor tags. Source of
    /// truth is the `TAGGED_AS` / `PARENT_TAG` edge graph; this column is a
    /// materialized filter index.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Type-specific structured data. Shape is validated per `entity_type` at
    /// ingest time (e.g. skills carry `category`, `steps`, `trigger_keywords`).
    #[serde(default)]
    pub properties: serde_json::Value,
    /// Content-hash for idempotent re-ingest. Populated by callers that care
    /// about update semantics (e.g. forge's skill ingester).
    #[serde(default)]
    pub content_hash: Option<String>,
    /// Last modification timestamp. `None` for legacy rows — callers should
    /// fall back to `created_at`.
    #[serde(default)]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Session vs global storage partition.
    #[serde(default)]
    pub scope: EntityScope,
    /// The session that most recently ingested or refreshed this entity.
    /// Global entities need this because their physical `session_id` is a
    /// tenant-global sentinel; session-scoped entities retain their original
    /// provenance in `session_id` and may record a later refresh here.
    #[serde(default)]
    pub ingested_by_session: Option<Uuid>,
}

/// Flat histogram row for `(entity_type, state) -> count`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityTypeStateCount {
    pub entity_type: String,
    pub state: MemoryState,
    pub count: usize,
}

/// Session partition scope for structured entity listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EntityListScope {
    /// Only the caller's session partition.
    Session,
    /// Tenant global partitions: the deterministic global sentinel plus the
    /// legacy nil-session partition used by older bulk scripts.
    Global,
    /// Caller session plus tenant global partitions.
    Both,
    /// Every session partition for this tenant. Uses a tenant-wide scan.
    #[default]
    All,
}

/// Structured entity list request used by storage backends.
#[derive(Debug, Clone, Default)]
pub struct EntityListQuery {
    pub session_id: Uuid,
    pub entity_type: Option<String>,
    pub filters: serde_json::Map<String, serde_json::Value>,
    pub scope: EntityListScope,
    pub limit: usize,
}

/// Retrieval-optimized semantic chunk from a source document.
///
/// `entity_store` keeps the durable document identity; document chunks keep the
/// ordered, searchable content plane. Neighbor IDs let callers expand around a
/// hit when the answer depends on surrounding list items or adjacent sections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChunk {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub document_id: Uuid,
    pub chunk_id: Uuid,
    pub ordinal: i32,
    pub source_doc_id: String,
    pub title: String,
    pub section_path: String,
    pub semantic_kind: String,
    pub content: String,
    pub bm25_text: String,
    pub chunk_embedding: Option<Vec<f32>>,
    pub token_count: i32,
    pub content_hash: String,
    pub prev_chunk_id: Option<Uuid>,
    pub next_chunk_id: Option<Uuid>,
    pub overlap_from_prev: bool,
    pub overlap_to_next: bool,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Default for EntityEntry {
    fn default() -> Self {
        Self {
            tenant_id: Uuid::nil(),
            entity_id: Uuid::nil(),
            session_id: Uuid::nil(),
            entity_name: String::new(),
            entity_type: String::new(),
            source_fold_id: None,
            context_snippet: String::new(),
            entity_embedding: None,
            confidence: 0.0,
            state: MemoryState::default(),
            created_at: chrono::DateTime::<chrono::Utc>::default(),
            description: None,
            description_embedding: None,
            tags: Vec::new(),
            properties: serde_json::Value::Null,
            content_hash: None,
            updated_at: None,
            scope: EntityScope::default(),
            ingested_by_session: None,
        }
    }
}

/// A temporal event/fact for an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalEvent {
    pub tenant_id: Uuid,
    pub entity_id: Uuid,
    pub event_time: chrono::DateTime<chrono::Utc>,
    pub event_id: Uuid,
    pub fact_text: String,
    pub supersedes_id: Option<Uuid>,
    pub valid_until: Option<chrono::DateTime<chrono::Utc>>,
    pub source_session: Uuid,
    pub confidence: f64,
}

/// A feedback outcome record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackOutcome {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub query_id: Uuid,
    pub program_type: String,
    pub task_complexity: String,
    pub succeeded: bool,
    pub latency_ms: i32,
    pub token_cost: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// An audit log entry recording a write operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub tenant_id: Uuid,
    pub audit_id: Uuid,
    pub operation: String,
    pub target_table: String,
    pub target_id: String,
    pub session_id: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ─── Tool usage logging ────────────────────────────────────────

/// A row from the tool_usage_log table for token analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsageRow {
    pub tool_name: String,
    pub repo: String,
    pub input_bytes: i32,
    pub output_bytes: i32,
    pub estimated_tokens: i32,
    pub latency_ms: i32,
    pub error: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ─── Sprint 5: Warmth types ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DecayZone {
    Identity,
    Knowledge,
    Operational,
}

impl DecayZone {
    pub fn decay_multiplier(&self) -> f64 {
        match self {
            Self::Identity => 0.1,
            Self::Knowledge => 1.0,
            Self::Operational => 3.0,
        }
    }
}

impl std::fmt::Display for DecayZone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity => write!(f, "identity"),
            Self::Knowledge => write!(f, "knowledge"),
            Self::Operational => write!(f, "operational"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmthEntry {
    pub tenant_id: Uuid,
    pub entity_id: Uuid,
    pub session_id: Uuid,
    pub warmth: f64,
    pub pagerank: f64,
    /// Reputation/trust score. Positive = verified/useful, negative = contradicted/missed.
    /// Accumulated via supersede events (-0.2) and retrieval misses (-0.05).
    #[serde(default)]
    pub reputation: f64,
    pub last_accessed_at: chrono::DateTime<chrono::Utc>,
    pub access_count: i64,
    pub decay_zone: DecayZone,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// ─── Sprint 5: Datalog types ───────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Term {
    Var(String),
    Const(Uuid),
    ConstStr(String),
    ConstFloat(OrderedFloat<f64>),
    /// A known-absent value, written `null`.
    ///
    /// Distinct from a variable that is unbound, which means "not decided
    /// yet", and from an evaluation error, which means "no answer at all".
    /// Comparing anything to it is `Unknown`, which propagates by Kleene —
    /// unlike an error, which poisons.
    ConstNull,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Atom {
    pub predicate: String,
    pub args: Vec<Term>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BuiltinFilter {
    /// Legacy: variable greater than a literal float.
    /// No longer emitted by the parser; preserved so already-persisted
    /// `RuleEntry` rows in CQL still deserialize.
    GreaterThan(String, f64),
    /// Legacy. See `GreaterThan` doc.
    LessThan(String, f64),
    /// Legacy. See `GreaterThan` doc.
    NotEqual(String, String),
    /// Full comparison filter.
    Compare {
        op: CmpOp,
        lhs: FilterExpr,
        rhs: FilterExpr,
    },
    /// `is_null(expr)`. Answers true or false and never `Unknown`, which is
    /// what makes it the only way to actually ask: `V == null` is `Unknown`
    /// and therefore never fires.
    IsNull(FilterExpr),
    /// A boolean-valued question about the shape of a string, written
    /// `str_starts_with(S, P)`.
    StrPred {
        op: StrOp,
        subject: FilterExpr,
        arg: FilterExpr,
    },
    /// Disjunction, written `||`. True when any branch is true.
    Any(Vec<BuiltinFilter>),
    /// Conjunction, written `&&`. Comma-separated body filters are already an
    /// implicit `All`; this is the explicit, groupable form.
    All(Vec<BuiltinFilter>),
    /// Negation, written `!`. The single mechanism for negating a filter —
    /// `StrPred` deliberately does not carry its own negated flag.
    Not(Box<BuiltinFilter>),
}

/// The string-shape predicates.
///
/// These carry a reserved `str_` prefix rather than the bare names, because
/// `contains` is already an edge type in this system — `contains(X, Y)` is a
/// legitimate stored relation, and taking the name would have silently changed
/// the meaning of rules already written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StrOp {
    StartsWith,
    EndsWith,
    Contains,
}

impl StrOp {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::StartsWith => "str_starts_with",
            Self::EndsWith => "str_ends_with",
            Self::Contains => "str_contains",
        }
    }

    pub const ALL: [StrOp; 3] = [StrOp::StartsWith, StrOp::EndsWith, StrOp::Contains];

    pub fn apply(self, subject: &str, arg: &str) -> bool {
        match self {
            Self::StartsWith => subject.starts_with(arg),
            Self::EndsWith => subject.ends_with(arg),
            Self::Contains => subject.contains(arg),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FilterExpr {
    Var(String),
    LitNum(ordered_float::OrderedFloat<f64>),
    LitStr(String),
    BinOp {
        op: ArithOp,
        lhs: Box<FilterExpr>,
        rhs: Box<FilterExpr>,
    },
    Neg(Box<FilterExpr>),
    /// The `null` literal.
    Null,
    /// A call to one of a closed set of pure functions.
    ///
    /// Closed on purpose: an open extension point would mean an unknown name
    /// had to be read as something, and the only other reading — a variable —
    /// is unbound and therefore matches every row.
    Call {
        func: Func,
        args: Vec<FilterExpr>,
    },
}

/// The functions callable from an expression. Pure, total on their declared
/// types, and cheap — nothing here allocates unboundedly or can loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Func {
    Abs,
    Floor,
    Ceil,
    Round,
    Len,
    Lower,
    Upper,
    Concat,
}

impl Func {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Abs => "abs",
            Self::Floor => "floor",
            Self::Ceil => "ceil",
            Self::Round => "round",
            Self::Len => "len",
            Self::Lower => "lower",
            Self::Upper => "upper",
            Self::Concat => "concat",
        }
    }

    /// How many arguments the function takes.
    pub fn arity(self) -> usize {
        match self {
            Self::Concat => 2,
            _ => 1,
        }
    }

    pub const ALL: [Func; 8] = [
        Func::Abs,
        Func::Floor,
        Func::Ceil,
        Func::Round,
        Func::Len,
        Func::Lower,
        Func::Upper,
        Func::Concat,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.keyword() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    /// Remainder, written `%`. Binds as tightly as `*` and `/`.
    Rem,
    /// Exponentiation, written `**`. Binds tighter than `*`, and associates to
    /// the right, so `2 ** 3 ** 2` is `2 ** (3 ** 2)`.
    Pow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AggregateKind {
    /// Folds *rows*: how many complete unifications the inner conjunction has.
    Count,
    /// Folds *values* of the aggregate's `value_var`.
    Sum,
    Min,
    Max,
    Avg,
    /// Folds distinct *values* of `value_var`.
    ///
    /// The one fold that cannot stream: distinctness needs a set, and the set
    /// grows with the answer. It is therefore bounded — see
    /// `datalog::DISTINCT_VALUE_CAP`.
    CountDistinct,
    /// Population standard deviation. Streams: Welford's method computes
    /// variance in one pass with constant memory, so this is a fold like
    /// `sum` and `avg`, not a member of the bounded family below.
    StdDev,
    /// The middle value. Needs the whole group ordered before an answer
    /// exists, so it is bounded — see `datalog::RETAINED_VALUE_CAP`.
    Median,
    /// The value at a fraction of the way through the ordered group. `Median`
    /// is this with `P = 0.5` rather than a separate mechanism.
    Percentile,
    /// The values themselves, joined by a separator.
    ///
    /// Every other aggregate reduces a group to one number; this is the only
    /// one that answers "which ones". It returns a string rather than a list
    /// because `DerivedFact` carries its endpoints as strings — a list-valued
    /// argument would be flattened at that boundary anyway.
    GroupConcat,
}

impl AggregateKind {
    /// The keyword this kind is written with.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
            Self::CountDistinct => "count_distinct",
            Self::StdDev => "stddev",
            Self::Median => "median",
            Self::Percentile => "percentile",
            Self::GroupConcat => "group_concat",
        }
    }

    /// Every kind but `Count` folds the values of a named variable, so every
    /// kind but `Count` requires one.
    pub fn needs_value_var(self) -> bool {
        !matches!(self, Self::Count)
    }

    /// Whether the aggregate takes a literal parameter between its value
    /// variable and its output — the fraction for `percentile`, the separator
    /// for `group_concat`.
    pub fn needs_param(self) -> bool {
        matches!(self, Self::Percentile | Self::GroupConcat)
    }

    /// Whether the fold must retain the whole group rather than an
    /// accumulator. These are the aggregates that cannot stream.
    pub fn retains_group(self) -> bool {
        matches!(self, Self::Median | Self::Percentile | Self::GroupConcat)
    }

    /// Whether an empty group still produces a value.
    ///
    /// `Count` and `Sum` have a well-defined identity over no rows — nothing
    /// happened zero times and cost zero. `Min`, `Max` and `Avg` do not: there
    /// is no minimum of nothing, and emitting a sentinel would be a fabricated
    /// value the caller cannot distinguish from a real one. Those rules simply
    /// do not fire.
    pub fn identity_over_empty(self) -> Option<Term> {
        match self {
            Self::Count | Self::Sum | Self::CountDistinct => {
                Some(Term::ConstFloat(OrderedFloat(0.0)))
            }
            // Joining no values is the empty string, which is a real answer
            // rather than a stand-in for one.
            Self::GroupConcat => Some(Term::ConstStr(String::new())),
            // There is no middle, extreme, mean or spread of nothing, and a
            // sentinel would be a fabricated value.
            Self::Min | Self::Max | Self::Avg | Self::StdDev | Self::Median | Self::Percentile => {
                None
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub kind: AggregateKind,
    /// The literal parameter, for the kinds that take one.
    ///
    /// Additive: rows written before `percentile` and `group_concat` existed
    /// carry no such field and deserialize to `None`, which is what every
    /// other kind wants.
    #[serde(default)]
    pub param: Option<Term>,
    pub inner: Atom,
    #[serde(default)]
    pub inner_conjunction: Vec<Atom>,
    pub group_vars: Vec<String>,
    pub output_var: String,
    /// The variable whose values are folded, for every kind but `Count`.
    ///
    /// Additive: rows written when `count` was the only aggregate carry no
    /// such field and deserialize to `None`, which is what `Count` wants.
    #[serde(default)]
    pub value_var: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StratifyError {
    RecursionThroughAggregate {
        cycle: Vec<String>,
    },
    /// A predicate's derivation transitively requires its own negation.
    /// Such a rule set has no stratified model, so it is rejected rather
    /// than evaluated to an arbitrary fixpoint.
    RecursionThroughNegation {
        cycle: Vec<String>,
    },
    /// A rule computes a head argument and that head can reach its own body,
    /// so each round produces a new value and the fixpoint never closes.
    ///
    /// The `max_facts` budget would stop it, but by truncation — the caller
    /// would get an arbitrary prefix with no signal it was cut short.
    /// Rejecting is the only answer that cannot be mistaken for an answer.
    RecursionThroughHeadExpression {
        cycle: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatalogRule {
    pub head: Atom,
    /// Positive body atoms. A binding must match every one of them.
    pub body: Vec<Atom>,
    pub filters: Vec<BuiltinFilter>,
    #[serde(default)]
    pub aggregates: Vec<Aggregate>,
    /// Negated body atoms (`not p(X)`). A binding survives only if it
    /// matches *none* of them.
    ///
    /// Additive on purpose: `RuleEntry` rows written before negation
    /// existed carry no such field and deserialize to an empty set, which
    /// means exactly what those rules meant before. Retyping `body` to
    /// carry polarity would have been a breaking stored-format change.
    #[serde(default)]
    pub negated: Vec<Atom>,
    /// Head arguments that are computed rather than repeated.
    ///
    /// Kept beside the head instead of retyping `Atom.args`, because `Atom` is
    /// shared by the head, body atoms, negated atoms and aggregate inner atoms,
    /// and only the head may compute — a body atom is a pattern to unify
    /// against, not something to evaluate.
    #[serde(default)]
    pub head_exprs: Vec<HeadExpr>,
    /// Computed values the body names, written `D := expr`.
    ///
    /// A distinct operator rather than `=`, which already parses to
    /// `CmpOp::Eq`. Redefining `=` would silently change the meaning of rules
    /// already stored, and a silent change of meaning is worse than a new
    /// symbol to learn.
    ///
    /// Evaluated in order after the positive atoms, so a binding may use
    /// anything the body bound and anything an earlier binding named.
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

/// A named value computed from what the body already bound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub var: String,
    pub expr: FilterExpr,
}

/// A computed head argument: which position it fills, and how to compute it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadExpr {
    pub index: usize,
    pub expr: FilterExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleEntry {
    pub tenant_id: Uuid,
    pub rule_id: String,
    pub version: i32,
    pub name: String,
    pub family: String,
    pub state: RuleState,
    pub rule_body: String,
    pub rule_weight: f64,
    pub incremental: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleState {
    Active,
    Deprecated,
    Superseded,
}

impl std::fmt::Display for RuleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Deprecated => write!(f, "deprecated"),
            Self::Superseded => write!(f, "superseded"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Proposed,
    Approved,
    Rejected,
}

impl std::fmt::Display for ClaimStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proposed => write!(f, "proposed"),
            Self::Approved => write!(f, "approved"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Rule,
    Claim,
    Alias,
    Skill,
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rule => write!(f, "rule"),
            Self::Claim => write!(f, "claim"),
            Self::Alias => write!(f, "alias"),
            Self::Skill => write!(f, "skill"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Proposed,
    Approved,
    Rejected,
}

impl std::fmt::Display for ApprovalDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proposed => write!(f, "proposed"),
            Self::Approved => write!(f, "approved"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalEntry {
    pub tenant_id: Uuid,
    pub approval_id: Uuid,
    pub artifact_kind: ArtifactKind,
    pub artifact_ref: String,
    pub decision: ApprovalDecision,
    pub review_note: Option<String>,
    pub reviewer: String,
    pub scope: String,
    pub workspace_scope: Option<String>,
    pub session_scope: Option<Uuid>,
    pub mirror_entity_id: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AliasScopeKind {
    Global,
    Workspace,
    Session,
}

impl std::fmt::Display for AliasScopeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => write!(f, "global"),
            Self::Workspace => write!(f, "workspace"),
            Self::Session => write!(f, "session"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasEntry {
    pub tenant_id: Uuid,
    pub alias_id: Uuid,
    pub alias_name: String,
    pub scope_kind: AliasScopeKind,
    pub scope_ref: String,
    pub canonical_tool: String,
    pub parameter_map: serde_json::Value,
    pub fixed_arguments: serde_json::Value,
    pub args_templates: serde_json::Value,
    pub status: ClaimStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplanationNode {
    pub parent_src: String,
    pub parent_pred: String,
    pub parent_dst: String,
    pub parent_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedExplanation {
    pub predicate: String,
    pub src_id: String,
    pub dst_id: String,
    pub rule_id: String,
    pub support_count: i32,
    pub support_chain: Vec<ExplanationNode>,
    pub approval_state: Option<ApprovalDecision>,
    pub latency_ms: i64,
    pub fanout: usize,
    pub truncated: bool,
}

// ─── Sprint 5: Fact set and derived facts ──────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FactSet {
    pub facts: std::collections::HashMap<String, std::collections::HashSet<Vec<Term>>>,
}

impl FactSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, predicate: &str, args: Vec<Term>) -> bool {
        self.facts
            .entry(predicate.to_string())
            .or_default()
            .insert(args)
    }

    pub fn contains(&self, predicate: &str, args: &[Term]) -> bool {
        self.facts
            .get(predicate)
            .is_some_and(|set| set.contains(args))
    }

    pub fn len(&self) -> usize {
        self.facts.values().map(|s| s.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, predicate: &str) -> Option<&std::collections::HashSet<Vec<Term>>> {
        self.facts.get(predicate)
    }

    pub fn predicates(&self) -> impl Iterator<Item = &String> {
        self.facts.keys()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedFact {
    pub src_id: String,
    pub pred: String,
    pub dst_id: String,
    pub confidence: f64,
    pub rule_id: String,
    pub support_count: i32,
    pub provenance: Vec<ProvenanceStep>,
}

impl DerivedFact {
    /// True iff both endpoints parse as UUIDs.
    ///
    /// The Datalog engine legitimately derives taxonomy facts whose object is an
    /// entity *type* string rather than an entity UUID — e.g.
    /// `isa(<entity-uuid>, "conversation_turn")`. The `derived_cache_by_query`
    /// table is UUID-keyed, so only UUID↔UUID facts can be cached there; callers
    /// use this to skip the rest instead of failing the whole batch (issue #129).
    pub fn has_uuid_endpoints(&self) -> bool {
        Uuid::parse_str(&self.src_id).is_ok() && Uuid::parse_str(&self.dst_id).is_ok()
    }

    /// True iff any step in this fact's derivation was an *absence* — a
    /// negated literal that held because no row matched it.
    pub fn rests_on_absence(&self) -> bool {
        self.provenance
            .iter()
            .any(|step| step.parent_kind == PROVENANCE_KIND_ABSENCE)
    }

    /// True iff this fact may be written to the persisted derived-fact store.
    ///
    /// Two independent reasons a derivation must not be cached:
    ///
    /// - its endpoints are not both UUIDs, so the UUID-keyed cache table
    ///   cannot hold it (issue #129); or
    /// - it rests on an absence. The engine is monotonic only for positive
    ///   rules: a later base fact can make a negated derivation **false**,
    ///   and an append-only cache would go on serving the stale derivation
    ///   forever. For a permission rule that is access which should have
    ///   been revoked. Negated derivations are therefore evaluated live and
    ///   never persisted.
    pub fn is_cacheable(&self) -> bool {
        self.has_uuid_endpoints() && !self.rests_on_absence()
    }
}

/// `ProvenanceStep::parent_kind` for a step that records an *absence*
/// rather than a matched row — the reason a negated literal held.
pub const PROVENANCE_KIND_ABSENCE: &str = "absence";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceStep {
    pub parent_src: String,
    pub parent_pred: String,
    pub parent_dst: String,
    pub parent_kind: String,
}

/// A single row from the derived cache (bulk listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedFactRow {
    pub source_id: String,
    pub predicate: String,
    pub target_id: String,
    pub confidence: f64,
    pub rule_id: String,
    pub cache_key: Option<String>,
    pub computed_at: String,
}

/// Entry for TTL tracking table (maps a cache row to its TTL rule).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtlTrackEntry {
    pub seq: i32,
    pub src_id: String,
    pub pred: String,
    pub dst_id: String,
    pub ttl_seconds: i32,
    pub rule_id: String,
    pub next_maintenance: String,
}

// ─── Sprint 5: Recursive exploration types ─────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecursiveExploreResult {
    pub sub_queries: Vec<SubQuery>,
    pub results: Vec<crate::hybrid_search::SearchResult>,
    pub passes: usize,
    pub converged: bool,
    pub derived_facts_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubQuery {
    pub query_text: String,
    pub reasoning: String,
}

// ─── Typed edges ───────────────────────────────────────────────

/// A typed, labeled edge between two entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedEdge {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub src_id: Uuid,
    pub edge_type: String,
    pub dst_id: Uuid,
    pub weight: f64,
    pub metadata: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ─── Sprint 6: Confidence, Contradiction, Consolidation, Schema ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceScore {
    pub entity_id: Uuid,
    pub fact_hash: String,
    pub confidence: f64,
    pub source_count: i32,
    pub last_confirmed_at: chrono::DateTime<chrono::Utc>,
    pub contradiction_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionEntry {
    pub tenant_id: Uuid,
    pub entity_id: Uuid,
    pub old_fact_hash: String,
    pub new_fact_hash: String,
    pub old_fact_text: String,
    pub new_fact_text: String,
    pub detected_at: chrono::DateTime<chrono::Utc>,
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
    pub resolution: Option<String>,
    pub resolver: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationStage {
    FoldRaw,
    FoldCompressed,
    EntityExtracted,
    SkillCandidate,
}

impl std::fmt::Display for ConsolidationStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FoldRaw => write!(f, "fold_raw"),
            Self::FoldCompressed => write!(f, "fold_compressed"),
            Self::EntityExtracted => write!(f, "entity_extracted"),
            Self::SkillCandidate => write!(f, "skill_candidate"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainSchema {
    pub schema_id: Uuid,
    pub schema_name: String,
    pub version: i32,
    pub description: Option<String>,
    pub skill_names: Vec<String>,
    pub routing_guidelines: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// ─── B10: Materialization + Promotion types ────────────────────

/// A durably materialized derived edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedEdge {
    pub tenant_id: Uuid,
    pub src_id: String,
    pub shard: i16,
    pub pred: String,
    pub dst_id: String,
    pub rule_id: String,
    pub support_count: i32,
    pub confidence: f64,
    pub batch_id: String,
    pub materialized_at: chrono::DateTime<chrono::Utc>,
}

/// Promotion status for a predicate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromotionStatus {
    Candidate,
    Promoted,
    Demoted,
}

impl std::fmt::Display for PromotionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Candidate => write!(f, "candidate"),
            Self::Promoted => write!(f, "promoted"),
            Self::Demoted => write!(f, "demoted"),
        }
    }
}

/// A promoted predicate registry entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotedPredicate {
    pub tenant_id: Uuid,
    pub pred: String,
    pub promotion_score: f64,
    pub estimated_rows: i32,
    pub materialized_at: Option<chrono::DateTime<chrono::Utc>>,
    pub batch_id: Option<String>,
    pub status: PromotionStatus,
}

/// Heat data for a predicate over a time window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredicateHeat {
    pub pred: String,
    pub total_hits: i64,
    pub total_compute_ms: i64,
    pub total_requests: i64,
    pub days_observed: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decay_zone_multipliers() {
        assert!((DecayZone::Identity.decay_multiplier() - 0.1).abs() < f64::EPSILON);
        assert!((DecayZone::Knowledge.decay_multiplier() - 1.0).abs() < f64::EPSILON);
        assert!((DecayZone::Operational.decay_multiplier() - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_fact_set_operations() {
        let mut fs = FactSet::new();
        assert!(fs.is_empty());

        let args = vec![
            Term::Const(Uuid::new_v4()),
            Term::ConstStr("co_occurs".into()),
            Term::Const(Uuid::new_v4()),
        ];
        assert!(fs.insert("edge", args.clone()));
        assert!(!fs.insert("edge", args.clone())); // duplicate
        assert_eq!(fs.len(), 1);
        assert!(fs.contains("edge", &args));
        assert!(!fs.contains("node", &args));
    }

    #[test]
    fn test_decay_zone_serde() {
        let zone = DecayZone::Identity;
        let json = serde_json::to_string(&zone).unwrap();
        assert_eq!(json, "\"identity\"");
        let back: DecayZone = serde_json::from_str(&json).unwrap();
        assert_eq!(back, zone);
    }

    #[test]
    fn test_rule_state_serde() {
        let state = RuleState::Active;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"active\"");
        let back: RuleState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn test_term_serde_round_trip() {
        let terms = vec![
            Term::Var("X".into()),
            Term::Const(Uuid::nil()),
            Term::ConstStr("hello".into()),
            Term::ConstFloat(OrderedFloat(2.72)),
        ];
        for term in terms {
            let json = serde_json::to_string(&term).unwrap();
            let back: Term = serde_json::from_str(&json).unwrap();
            assert_eq!(back, term);
        }
    }

    #[test]
    fn test_promotion_status_serde() {
        let status = PromotionStatus::Promoted;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"promoted\"");
        let back: PromotionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn test_promotion_status_display() {
        assert_eq!(PromotionStatus::Candidate.to_string(), "candidate");
        assert_eq!(PromotionStatus::Promoted.to_string(), "promoted");
        assert_eq!(PromotionStatus::Demoted.to_string(), "demoted");
    }

    // --- Richer entity model (Sprint 1 slice 1a) ---

    #[test]
    fn entity_scope_default_is_session() {
        assert_eq!(EntityScope::default(), EntityScope::Session);
    }

    #[test]
    fn entity_scope_serde_as_lowercase_strings() {
        assert_eq!(
            serde_json::to_string(&EntityScope::Session).unwrap(),
            "\"session\""
        );
        assert_eq!(
            serde_json::to_string(&EntityScope::Global).unwrap(),
            "\"global\""
        );

        let back: EntityScope = serde_json::from_str("\"global\"").unwrap();
        assert_eq!(back, EntityScope::Global);
    }

    #[test]
    fn entity_entry_deserializes_from_legacy_json_with_defaults() {
        // Old rows on disk won't have the new fields. Deserialization must
        // succeed and populate sensible defaults so existing data is readable.
        let now = chrono::Utc::now();
        let legacy = serde_json::json!({
            "tenant_id": Uuid::nil(),
            "entity_id": Uuid::nil(),
            "session_id": Uuid::nil(),
            "entity_name": "x",
            "entity_type": "concept",
            "source_fold_id": null,
            "context_snippet": "",
            "entity_embedding": null,
            "confidence": 1.0,
            "created_at": now,
        });
        let entry: EntityEntry = serde_json::from_value(legacy).unwrap();
        assert_eq!(entry.description, None);
        assert_eq!(entry.description_embedding, None);
        assert!(entry.tags.is_empty());
        assert_eq!(entry.properties, serde_json::Value::Null);
        assert_eq!(entry.content_hash, None);
        assert_eq!(entry.updated_at, None);
        assert_eq!(entry.scope, EntityScope::Session);
        assert_eq!(entry.ingested_by_session, None);
    }

    #[test]
    fn entity_entry_round_trip_preserves_new_fields() {
        let now = chrono::Utc::now();
        let ingester = Uuid::new_v4();
        let original = EntityEntry {
            tenant_id: Uuid::new_v4(),
            entity_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            entity_name: "tdd".into(),
            entity_type: "skill".into(),
            source_fold_id: None,
            context_snippet: "original source".into(),
            entity_embedding: Some(vec![0.1, 0.2]),
            confidence: 0.9,
            state: MemoryState::default(),
            created_at: now,
            description: Some("Guides red-green-refactor cycles.".into()),
            description_embedding: Some(vec![0.3, 0.4]),
            tags: vec!["testing".into(), "quality".into()],
            properties: serde_json::json!({"category": "task-level"}),
            content_hash: Some("sha256:abc".into()),
            updated_at: Some(now),
            scope: EntityScope::Global,
            ingested_by_session: Some(ingester),
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: EntityEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entity_name, "tdd");
        assert_eq!(
            back.description.as_deref(),
            Some("Guides red-green-refactor cycles.")
        );
        assert_eq!(back.tags, vec!["testing".to_string(), "quality".into()]);
        assert_eq!(
            back.properties,
            serde_json::json!({"category": "task-level"})
        );
        assert_eq!(back.content_hash.as_deref(), Some("sha256:abc"));
        assert_eq!(back.scope, EntityScope::Global);
        assert_eq!(back.ingested_by_session, Some(ingester));
    }

    #[test]
    fn builtin_filter_compare_round_trips_through_json() {
        let f = BuiltinFilter::Compare {
            op: CmpOp::Ge,
            lhs: FilterExpr::Var("S".into()),
            rhs: FilterExpr::BinOp {
                op: ArithOp::Add,
                lhs: Box::new(FilterExpr::Var("T".into())),
                rhs: Box::new(FilterExpr::LitNum(ordered_float::OrderedFloat(1.0))),
            },
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: BuiltinFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn legacy_builtin_filter_variants_still_round_trip() {
        let g = BuiltinFilter::GreaterThan("X".into(), 0.5);
        let l = BuiltinFilter::LessThan("X".into(), 0.5);
        let n = BuiltinFilter::NotEqual("X".into(), "Y".into());
        for f in [g, l, n] {
            let json = serde_json::to_string(&f).unwrap();
            let back: BuiltinFilter = serde_json::from_str(&json).unwrap();
            assert_eq!(back, f);
        }
    }

    #[test]
    fn aggregate_round_trips_through_json() {
        let a = Aggregate {
            kind: AggregateKind::Count,
            inner: Atom {
                predicate: "user_corrected".into(),
                args: vec![Term::Var("S".into()), Term::Var("X".into())],
            },
            inner_conjunction: vec![],
            group_vars: vec!["X".into()],
            param: None,
            value_var: None,
            output_var: "N".into(),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: Aggregate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn aggregate_v2_round_trips_through_json() {
        let a = Aggregate {
            kind: AggregateKind::Count,
            inner: Atom {
                predicate: "worked_well".into(),
                args: vec![Term::Var("S".into()), Term::Var("Tool".into())],
            },
            inner_conjunction: vec![
                Atom {
                    predicate: "worked_well".into(),
                    args: vec![Term::Var("S".into()), Term::Var("Tool".into())],
                },
                Atom {
                    predicate: "session_context".into(),
                    args: vec![Term::Var("S".into()), Term::Var("Ctx".into())],
                },
            ],
            group_vars: vec!["Ctx".into(), "Tool".into()],
            param: None,
            value_var: None,
            output_var: "N".into(),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: Aggregate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn aggregate_v1_legacy_deserializes_with_empty_conjunction() {
        // Use whatever Term::Var wire shape the existing tests use. Look
        // at `datalog_rule_without_aggregates_field_deserializes_with_default`
        // in this same file for the canonical Term tag format.
        // What matters is `inner_conjunction` is absent and defaults to vec![].
        // Build the JSON by serializing a v1-shaped aggregate, then strip the
        // `inner_conjunction` key before deserializing — that guarantees the
        // wire format matches what's actually persisted.
        let v1_shape = Aggregate {
            kind: AggregateKind::Count,
            inner: Atom {
                predicate: "user_corrected".into(),
                args: vec![Term::Var("S".into()), Term::Var("X".into())],
            },
            inner_conjunction: vec![],
            group_vars: vec!["X".into()],
            param: None,
            value_var: None,
            output_var: "N".into(),
        };
        let mut json: serde_json::Value = serde_json::to_value(&v1_shape).unwrap();
        // Simulate a pre-v2 wire row: drop the inner_conjunction key entirely.
        json.as_object_mut().unwrap().remove("inner_conjunction");
        let back: Aggregate = serde_json::from_value(json).unwrap();
        assert!(back.inner_conjunction.is_empty());
        assert_eq!(back.inner.predicate, "user_corrected");
    }

    #[test]
    fn stratify_error_round_trips() {
        let e = StratifyError::RecursionThroughAggregate {
            cycle: vec!["a".into(), "b".into()],
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: StratifyError = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn datalog_rule_without_aggregates_field_deserializes_with_default() {
        let json = r#"{
            "head": {"predicate": "foo", "args": [{"type": "Var", "value": "X"}]},
            "body": [{"predicate": "bar", "args": [{"type": "Var", "value": "X"}]}],
            "filters": []
        }"#;
        let rule: DatalogRule = serde_json::from_str(json).unwrap();
        assert!(rule.aggregates.is_empty());
    }

    #[test]
    fn foresight_storage_ttl_seconds_covers_each_window() {
        let now = chrono::Utc::now();
        let mk = |valid_from, valid_until| ForesightFact {
            tenant_id: Uuid::nil(),
            session_id: Uuid::nil(),
            fact_id: Uuid::nil(),
            content: String::new(),
            valid_from,
            valid_until,
            created_at: now,
        };

        // Open-ended (no valid_until) -> no TTL (persists).
        assert_eq!(mk(None, None).storage_ttl_seconds(now), None);

        // Bounded, in the future -> time-to-expiry + grace.
        let until = now + chrono::Duration::days(3);
        let ttl = mk(None, Some(until)).storage_ttl_seconds(now).unwrap();
        let expected = 3 * 24 * 3600 + FORESIGHT_TTL_GRACE_SECS;
        assert!((ttl - expected).abs() <= 1, "ttl={ttl} expected≈{expected}");

        // A future valid_from doesn't shorten the TTL (it lives until valid_until).
        let ttl_future_from = mk(Some(now + chrono::Duration::days(2)), Some(until))
            .storage_ttl_seconds(now)
            .unwrap();
        assert_eq!(ttl_future_from, ttl);

        // Already long-expired (past the grace) -> clamped to a positive minimum.
        let past = now - chrono::Duration::days(100);
        assert_eq!(mk(None, Some(past)).storage_ttl_seconds(now), Some(1));

        // Too far out to TTL safely -> None (persist; never risk early reaping).
        let far = now + chrono::Duration::days(365 * 30);
        assert_eq!(mk(None, Some(far)).storage_ttl_seconds(now), None);
    }
}

/// Durable coordination state for a single (tenant, session) consolidation
/// request. Exactly one replica may hold the lease at a time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationRequestState {
    Pending,
    Leased,
    Completed,
    Failed,
}

/// Durable consolidation coordination row keyed by (tenant_id, session_id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationRequest {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub state: ConsolidationRequestState,
    pub requested_at: chrono::DateTime<chrono::Utc>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub attempt_count: i32,
    pub last_error: Option<String>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Durable consolidation run audit record. `run_id` is a timeuuid so rows
/// cluster newest-first under each (tenant, session).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationRun {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub run_id: Uuid,
    pub lease_owner: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: String,
    pub entities_processed: i32,
    pub connections_created: i32,
    pub error: Option<String>,
}

/// Outcome passed to `Storage::consolidation_request_complete` to release the
/// lease and write the run log in one call.
#[derive(Debug, Clone)]
pub struct ConsolidationResult {
    pub status: String,
    pub entities_processed: i32,
    pub connections_created: i32,
    pub error: Option<String>,
}
