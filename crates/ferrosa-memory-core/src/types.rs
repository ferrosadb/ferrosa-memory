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
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl MemScene {
    pub fn member_count(&self) -> i32 {
        self.member_ids.len() as i32
    }
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
    /// For global-scope entities, the session that originally ingested them
    /// (audit + session-affinity re-rank signal).
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
    /// Full comparison filter — the only variant the parser emits.
    Compare {
        op: CmpOp,
        lhs: FilterExpr,
        rhs: FilterExpr,
    },
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AggregateKind {
    Count,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub kind: AggregateKind,
    pub inner: Atom,
    #[serde(default)]
    pub inner_conjunction: Vec<Atom>,
    pub group_vars: Vec<String>,
    pub output_var: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StratifyError {
    RecursionThroughAggregate { cycle: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatalogRule {
    pub head: Atom,
    pub body: Vec<Atom>,
    pub filters: Vec<BuiltinFilter>,
    #[serde(default)]
    pub aggregates: Vec<Aggregate>,
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
}
