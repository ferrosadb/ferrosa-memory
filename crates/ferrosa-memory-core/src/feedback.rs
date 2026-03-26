//! Feedback outcome recording (ACON/SRLM pattern).
//!
//! Records (strategy, complexity, outcome) triples for offline guideline
//! refinement. Write-only via MCP — no read path exposed (STRIDE E2).

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{FeedbackOutcome, TenantContext};

/// Record a feedback outcome.
#[allow(clippy::too_many_arguments)]
pub async fn record_outcome(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query_id: Uuid,
    program_type: &str,
    task_complexity: &str,
    succeeded: bool,
    latency_ms: i32,
    token_cost: i32,
) -> anyhow::Result<bool> {
    let outcome = FeedbackOutcome {
        tenant_id: ctx.tenant_id,
        session_id,
        query_id,
        program_type: program_type.to_string(),
        task_complexity: task_complexity.to_string(),
        succeeded,
        latency_ms,
        token_cost,
        created_at: chrono::Utc::now(),
    };

    storage.feedback_put(ctx, &outcome).await?;
    tracing::info!(
        program_type,
        task_complexity,
        succeeded,
        latency_ms,
        "feedback recorded"
    );
    Ok(true)
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
    async fn record_feedback() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        let ok = record_outcome(
            &store,
            &ctx,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "hnsw_ann",
            "simple",
            true,
            42,
            100,
        )
        .await
        .unwrap();
        assert!(ok);

        let feedback = store.feedback.lock().await;
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].program_type, "hnsw_ann");
        assert!(feedback[0].succeeded);
    }
}
