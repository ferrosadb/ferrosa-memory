//! Session lifecycle management.
//!
//! Implements right-to-deletion: cascade delete all memory objects for a
//! session across all tables.

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

/// Result of deleting a session's data.
#[derive(Debug, serde::Serialize)]
pub struct DeleteSessionResult {
    pub deleted: bool,
    pub objects_removed: usize,
}

/// Delete all memory objects for a session.
///
/// Cascades across plan_state, trajectory_folds, entity_store,
/// temporal_events, and feedback_outcomes.
pub async fn delete_session(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<DeleteSessionResult> {
    let count = storage.delete_session(ctx, session_id).await?;
    tracing::info!(%session_id, objects_removed = count, "session deleted");
    Ok(DeleteSessionResult {
        deleted: true,
        objects_removed: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity;
    use crate::feedback;
    use crate::memo::{self, StoreMemoParams};
    use crate::plan;
    use crate::storage::mock::MockStorage;

    fn ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    #[tokio::test]
    async fn delete_session_clears_all_tables() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        // Populate
        memo::store_memo_result(
            &store,
            &ctx,
            &StoreMemoParams {
                prompt: "test",
                context_slice: "ctx",
                model_version: "v1",
                result: "answer",
                embedding: None,
                ttl_days: None,
            },
        )
        .await
        .unwrap();

        plan::write_plan_node(&store, &ctx, sid, 0, "root", None, "goal")
            .await
            .unwrap();

        entity::upsert_entity(
            &store,
            &ctx,
            sid,
            "Alice",
            "person",
            "ctx",
            None,
            None,
            Some(0.9),
        )
        .await
        .unwrap();

        feedback::record_outcome(
            &store,
            &ctx,
            sid,
            Uuid::new_v4(),
            "phonetic",
            "simple",
            true,
            5,
            0,
        )
        .await
        .unwrap();

        // Delete session
        let result = delete_session(&store, &ctx, sid).await.unwrap();
        assert!(result.deleted);
        assert!(result.objects_removed >= 3); // plan + entity + feedback

        // Verify plan is empty for this session
        let plan = plan::get_plan_context(&store, &ctx, sid, None)
            .await
            .unwrap();
        assert!(plan.nodes.is_empty());
    }

    #[tokio::test]
    async fn delete_session_doesnt_affect_other_sessions() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid1 = Uuid::new_v4();
        let sid2 = Uuid::new_v4();

        plan::write_plan_node(&store, &ctx, sid1, 0, "root1", None, "goal1")
            .await
            .unwrap();
        plan::write_plan_node(&store, &ctx, sid2, 0, "root2", None, "goal2")
            .await
            .unwrap();

        delete_session(&store, &ctx, sid1).await.unwrap();

        // sid1 gone
        let p1 = plan::get_plan_context(&store, &ctx, sid1, None)
            .await
            .unwrap();
        assert!(p1.nodes.is_empty());

        // sid2 still there
        let p2 = plan::get_plan_context(&store, &ctx, sid2, None)
            .await
            .unwrap();
        assert_eq!(p2.nodes.len(), 1);
    }
}
