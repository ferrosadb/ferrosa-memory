//! Storage trait — abstract CQL operations for testability.
//!
//! All tool modules depend on this trait, not a concrete CQL client. This is
//! the critical abstraction identified by the DSM analysis: `cql_client` has
//! 86% propagation cost, so a stable trait interface prevents cascading changes.
//!
//! ## Design
//!
//! The trait defines typed operations at the domain level (memo, plan) rather
//! than raw CQL. This keeps the CQL dialect inside the concrete implementation
//! and lets us swap backends (mock, embedded, real Ferrosa) without changing
//! tool handler code.

use uuid::Uuid;

use crate::types::{MemoEntry, PlanNode, PlanStatus, TenantContext};

/// Core storage operations for the memory system.
///
/// All methods are async and take `&self` (shared reference) because the
/// underlying CQL client manages its own connection pool.
///
/// Every method that accesses tenant data requires a [`TenantContext`] to
/// enforce tenant isolation at the trait boundary.
#[allow(async_fn_in_trait)]
pub trait Storage: Send + Sync {
    /// Check memo cache by content hash.
    async fn memo_get(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<Option<MemoEntry>>;

    /// Increment hit count and update last_hit_at on cache hit.
    async fn memo_touch(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<()>;

    /// Store a new memo cache entry.
    async fn memo_put(&self, ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()>;

    /// Write a plan node.
    async fn plan_put(&self, ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()>;

    /// Get all plan nodes for a session up to max_depth.
    async fn plan_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        max_depth: Option<i32>,
    ) -> anyhow::Result<Vec<PlanNode>>;

    /// Update a plan node's status and optional outcome summary.
    async fn plan_update_status(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        depth: i32,
        subtask_id: &str,
        status: PlanStatus,
        outcome_summary: Option<&str>,
    ) -> anyhow::Result<()>;
}

/// In-memory mock storage for unit tests.
///
/// Stores data in `Vec`s behind a `tokio::sync::Mutex`. Not for production use.
#[cfg(any(test, feature = "mock-storage"))]
pub mod mock {
    use super::*;
    use tokio::sync::Mutex;

    #[derive(Default)]
    pub struct MockStorage {
        pub memos: Mutex<Vec<MemoEntry>>,
        pub plans: Mutex<Vec<PlanNode>>,
    }

    impl MockStorage {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl Storage for MockStorage {
        async fn memo_get(
            &self,
            ctx: &TenantContext,
            content_hash: &str,
            model_version: &str,
        ) -> anyhow::Result<Option<MemoEntry>> {
            let memos = self.memos.lock().await;
            let _ = ctx; // tenant scoping would filter here in real impl
            Ok(memos
                .iter()
                .find(|m| m.content_hash == content_hash && m.model_version == model_version)
                .cloned())
        }

        async fn memo_touch(
            &self,
            _ctx: &TenantContext,
            content_hash: &str,
            model_version: &str,
        ) -> anyhow::Result<()> {
            let mut memos = self.memos.lock().await;
            if let Some(m) = memos
                .iter_mut()
                .find(|m| m.content_hash == content_hash && m.model_version == model_version)
            {
                m.hit_count += 1;
                m.last_hit_at = Some(chrono::Utc::now());
            }
            Ok(())
        }

        async fn memo_put(&self, _ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()> {
            let mut memos = self.memos.lock().await;
            memos.push(entry.clone());
            Ok(())
        }

        async fn plan_put(&self, _ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()> {
            let mut plans = self.plans.lock().await;
            plans.push(node.clone());
            Ok(())
        }

        async fn plan_get(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            max_depth: Option<i32>,
        ) -> anyhow::Result<Vec<PlanNode>> {
            let plans = self.plans.lock().await;
            Ok(plans
                .iter()
                .filter(|p| p.session_id == session_id && max_depth.is_none_or(|d| p.depth <= d))
                .cloned()
                .collect())
        }

        async fn plan_update_status(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            depth: i32,
            subtask_id: &str,
            status: PlanStatus,
            outcome_summary: Option<&str>,
        ) -> anyhow::Result<()> {
            let mut plans = self.plans.lock().await;
            if let Some(p) = plans.iter_mut().find(|p| {
                p.session_id == session_id && p.depth == depth && p.subtask_id == subtask_id
            }) {
                p.status = status;
                if let Some(summary) = outcome_summary {
                    p.outcome_summary = Some(summary.to_string());
                }
                if p.status == PlanStatus::Complete || p.status == PlanStatus::Failed {
                    p.completed_at = Some(chrono::Utc::now());
                }
            }
            Ok(())
        }
    }
}
