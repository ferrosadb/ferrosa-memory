//! Plan state tool handlers.
//!
//! Implements ReCAP's structured re-injection of parent plans. The plan tree
//! is stored in CQL with `(session_id, tenant_id)` as partition key and
//! `(depth, subtask_id)` as clustering columns, giving O(depth) range scans.
//!
//! ## Tools
//!
//! - `write_plan_node` — create a new node in the plan hierarchy
//! - `get_plan_context` — retrieve the full plan tree (or up to max_depth)
//! - `update_plan_node` — mark a node complete/failed with outcome summary
//!
//! ## Active path
//!
//! `get_plan_context` returns both the full node list and the "active path" —
//! the chain of nodes from root to the deepest currently-active node. This is
//! what the LLM injects into its prompt preamble to maintain plan awareness.

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{PlanNode, PlanStatus, TenantContext};

/// Result of `get_plan_context`.
#[derive(Debug, serde::Serialize)]
pub struct PlanContext {
    pub nodes: Vec<PlanNode>,
    pub active_path: Vec<String>,
}

/// Write a new plan node to storage.
pub async fn write_plan_node(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    depth: i32,
    subtask_id: &str,
    parent_subtask: Option<&str>,
    goal_text: &str,
) -> anyhow::Result<bool> {
    let node = PlanNode {
        session_id,
        depth,
        subtask_id: subtask_id.to_string(),
        parent_subtask: parent_subtask.map(String::from),
        goal_text: goal_text.to_string(),
        status: PlanStatus::Pending,
        outcome_summary: None,
        created_at: chrono::Utc::now(),
        completed_at: None,
    };

    storage.plan_put(ctx, &node).await?;
    Ok(true)
}

/// Retrieve the plan tree for a session, optionally limited to max_depth.
///
/// Returns all nodes sorted by (depth, subtask_id) plus the active path —
/// the chain of `Active` status nodes from root to leaf.
pub async fn get_plan_context(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    max_depth: Option<i32>,
) -> anyhow::Result<PlanContext> {
    let nodes = storage.plan_get(ctx, session_id, max_depth).await?;

    // Compute active path: find the chain of Active nodes from depth 0 down
    let active_path = compute_active_path(&nodes);

    Ok(PlanContext { nodes, active_path })
}

/// Update a plan node's status and optional outcome summary.
pub async fn update_plan_node(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    depth: i32,
    subtask_id: &str,
    status: PlanStatus,
    outcome_summary: Option<&str>,
) -> anyhow::Result<bool> {
    storage
        .plan_update_status(ctx, session_id, depth, subtask_id, status, outcome_summary)
        .await?;
    Ok(true)
}

/// Compute the active path through the plan tree.
///
/// Walks from depth 0 down, following `Active` nodes. At each depth, picks
/// the first active node whose `parent_subtask` matches the previous level's
/// `subtask_id`.
fn compute_active_path(nodes: &[PlanNode]) -> Vec<String> {
    let mut path = Vec::new();
    let mut current_parent: Option<&str> = None;

    // Nodes should already be sorted by depth, but group by depth to be safe
    let max_depth = nodes.iter().map(|n| n.depth).max().unwrap_or(-1);

    for d in 0..=max_depth {
        let active_at_depth = nodes.iter().find(|n| {
            n.depth == d
                && n.status == PlanStatus::Active
                && match (current_parent, &n.parent_subtask) {
                    (None, None) => true,      // root level
                    (None, Some(_)) => d == 0, // root level with explicit parent
                    (Some(p), Some(np)) => p == np.as_str(),
                    (Some(_), None) => false,
                }
        });

        match active_at_depth {
            Some(node) => {
                path.push(node.subtask_id.clone());
                current_parent = Some(&node.subtask_id);
            }
            None => break,
        }
    }

    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    #[tokio::test]
    async fn write_and_get_plan_node() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        write_plan_node(&store, &ctx, sid, 0, "root", None, "do the thing")
            .await
            .unwrap();

        let plan = get_plan_context(&store, &ctx, sid, None).await.unwrap();
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].subtask_id, "root");
        assert_eq!(plan.nodes[0].status, PlanStatus::Pending);
    }

    #[tokio::test]
    async fn update_plan_status() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        write_plan_node(&store, &ctx, sid, 0, "root", None, "do the thing")
            .await
            .unwrap();

        update_plan_node(
            &store,
            &ctx,
            sid,
            0,
            "root",
            PlanStatus::Complete,
            Some("it was done"),
        )
        .await
        .unwrap();

        let plan = get_plan_context(&store, &ctx, sid, None).await.unwrap();
        assert_eq!(plan.nodes[0].status, PlanStatus::Complete);
        assert_eq!(
            plan.nodes[0].outcome_summary.as_deref(),
            Some("it was done")
        );
        assert!(plan.nodes[0].completed_at.is_some());
    }

    #[tokio::test]
    async fn active_path_follows_active_chain() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Build a small tree:
        // depth=0: root (Active)
        //   depth=1: sub-a (Complete), sub-b (Active)
        //     depth=2: sub-b-1 (Active)
        write_plan_node(&store, &ctx, sid, 0, "root", None, "root goal")
            .await
            .unwrap();
        update_plan_node(&store, &ctx, sid, 0, "root", PlanStatus::Active, None)
            .await
            .unwrap();

        write_plan_node(&store, &ctx, sid, 1, "sub-a", Some("root"), "sub-a goal")
            .await
            .unwrap();
        update_plan_node(
            &store,
            &ctx,
            sid,
            1,
            "sub-a",
            PlanStatus::Complete,
            Some("done"),
        )
        .await
        .unwrap();

        write_plan_node(&store, &ctx, sid, 1, "sub-b", Some("root"), "sub-b goal")
            .await
            .unwrap();
        update_plan_node(&store, &ctx, sid, 1, "sub-b", PlanStatus::Active, None)
            .await
            .unwrap();

        write_plan_node(&store, &ctx, sid, 2, "sub-b-1", Some("sub-b"), "leaf goal")
            .await
            .unwrap();
        update_plan_node(&store, &ctx, sid, 2, "sub-b-1", PlanStatus::Active, None)
            .await
            .unwrap();

        let plan = get_plan_context(&store, &ctx, sid, None).await.unwrap();
        assert_eq!(plan.active_path, vec!["root", "sub-b", "sub-b-1"]);
    }

    #[tokio::test]
    async fn max_depth_limits_results() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        write_plan_node(&store, &ctx, sid, 0, "root", None, "root")
            .await
            .unwrap();
        write_plan_node(&store, &ctx, sid, 1, "sub", Some("root"), "sub")
            .await
            .unwrap();
        write_plan_node(&store, &ctx, sid, 2, "leaf", Some("sub"), "leaf")
            .await
            .unwrap();

        let plan = get_plan_context(&store, &ctx, sid, Some(1)).await.unwrap();
        assert_eq!(plan.nodes.len(), 2); // depth 0 and 1 only
        assert!(plan.nodes.iter().all(|n| n.depth <= 1));
    }
}
