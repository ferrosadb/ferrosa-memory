//! Shared domain types used across modules.
//!
//! These types represent the core data structures that flow through the system:
//! tool parameters, tool results, and internal representations of stored data.

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
