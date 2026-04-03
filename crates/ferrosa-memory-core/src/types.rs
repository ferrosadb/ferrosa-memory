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
    GreaterThan(String, f64),
    LessThan(String, f64),
    NotEqual(String, String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatalogRule {
    pub head: Atom,
    pub body: Vec<Atom>,
    pub filters: Vec<BuiltinFilter>,
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
}
