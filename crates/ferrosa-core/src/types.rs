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
