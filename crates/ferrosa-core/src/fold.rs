//! Trajectory fold tool handlers (Context-Folding pattern).
//!
//! Manages the fold lifecycle: start -> append turns -> complete with summary.
//! Each fold is a Cypher vertex; `FOLDED_INTO` edges connect child to parent,
//! enabling multi-hop traversal of the fold hierarchy.
//!
//! ## Tools
//!
//! - `start_fold` — create a new active trajectory fold
//! - `append_to_fold` — add a REPL turn to an active fold
//! - `complete_fold` — seal fold, write summary + embedding, create graph edge
//! - `retrieve_fold_context` — ANN search over fold embeddings

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{FoldEntry, FoldStatus, FoldSummary, TenantContext};

/// Start a new trajectory fold.
///
/// Creates an active fold entry. Returns the generated `fold_id`.
pub async fn start_fold(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    depth: i32,
    parent_fold_id: Option<Uuid>,
    initial_context: &str,
) -> anyhow::Result<Uuid> {
    let fold_id = Uuid::now_v7();
    let token_count = initial_context.split_whitespace().count() as i32;

    let entry = FoldEntry {
        session_id,
        fold_id,
        tenant_id: ctx.tenant_id,
        depth,
        parent_fold_id,
        raw_trajectory: initial_context.to_string(),
        fold_summary: None,
        fold_embedding: None,
        token_count,
        compression_ratio: None,
        status: FoldStatus::Active,
        created_at: chrono::Utc::now(),
        folded_at: None,
    };

    storage.fold_put(ctx, &entry).await?;
    Ok(fold_id)
}

/// Append a REPL turn to an active fold.
///
/// Returns the updated token count so the caller can decide whether to
/// initiate a nested fold. Rejects appends to non-active folds (FMEA F15).
pub async fn append_to_fold(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    fold_id: Uuid,
    repl_turn: &str,
) -> anyhow::Result<(bool, i32)> {
    // Check fold exists and is active
    let fold = storage
        .fold_get(ctx, session_id, fold_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("fold not found: {fold_id}"))?;

    if fold.status != FoldStatus::Active {
        anyhow::bail!("cannot append to fold with status {:?}", fold.status);
    }

    storage
        .fold_append(ctx, session_id, fold_id, repl_turn)
        .await?;

    let new_count = fold.token_count + repl_turn.split_whitespace().count() as i32;
    Ok((true, new_count))
}

/// Complete a fold: seal it, write summary and embedding.
///
/// After completion, the fold is marked as `Folded` and background compression
/// is queued (handled by caller). Creates a `FOLDED_INTO` graph edge to the
/// parent fold if one exists.
pub async fn complete_fold(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    fold_id: Uuid,
    summary: &str,
    embedding: Vec<f32>,
) -> anyhow::Result<(bool, f64)> {
    let fold = storage
        .fold_get(ctx, session_id, fold_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("fold not found: {fold_id}"))?;

    if fold.status != FoldStatus::Active {
        anyhow::bail!("cannot complete fold with status {:?}", fold.status);
    }

    let summary_tokens = summary.split_whitespace().count();
    let raw_tokens = fold.token_count.max(1) as f64;
    let compression_ratio = summary_tokens as f64 / raw_tokens;

    storage
        .fold_complete(
            ctx,
            session_id,
            fold_id,
            summary,
            embedding,
            compression_ratio,
        )
        .await?;

    Ok((true, compression_ratio))
}

/// Retrieve fold summaries by semantic similarity.
///
/// Searches fold embeddings via ANN, returning the top-k most relevant
/// fold summaries. If `include_raw` is true, also loads the full trajectory
/// (may be slow for archived folds — FMEA F16).
pub async fn retrieve_fold_context(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query_embedding: &[f32],
    k: Option<usize>,
    include_raw: bool,
) -> anyhow::Result<Vec<FoldSummary>> {
    let k = k.unwrap_or(5);
    storage
        .fold_search(ctx, session_id, query_embedding, k, include_raw)
        .await
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
    async fn fold_lifecycle() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Start
        let fold_id = start_fold(&store, &ctx, sid, 0, None, "initial context here")
            .await
            .unwrap();

        // Append
        let (ok, count) = append_to_fold(&store, &ctx, sid, fold_id, "turn 1: did something")
            .await
            .unwrap();
        assert!(ok);
        assert!(count > 3);

        // Append more
        let (_, count2) = append_to_fold(&store, &ctx, sid, fold_id, "turn 2: more work")
            .await
            .unwrap();
        assert!(count2 > count);

        // Complete
        let embedding = vec![0.1; 768];
        let (folded, ratio) =
            complete_fold(&store, &ctx, sid, fold_id, "summary of work", embedding)
                .await
                .unwrap();
        assert!(folded);
        assert!(ratio > 0.0 && ratio < 1.0);
    }

    #[tokio::test]
    async fn append_to_completed_fold_fails() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let fold_id = start_fold(&store, &ctx, sid, 0, None, "ctx").await.unwrap();
        complete_fold(&store, &ctx, sid, fold_id, "done", vec![0.1; 768])
            .await
            .unwrap();

        let result = append_to_fold(&store, &ctx, sid, fold_id, "more").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn complete_already_folded_fails() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let fold_id = start_fold(&store, &ctx, sid, 0, None, "ctx").await.unwrap();
        complete_fold(&store, &ctx, sid, fold_id, "done", vec![0.1; 768])
            .await
            .unwrap();

        let result = complete_fold(&store, &ctx, sid, fold_id, "again", vec![0.1; 768]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn retrieve_fold_context_returns_summaries() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let f1 = start_fold(&store, &ctx, sid, 0, None, "fold 1 context")
            .await
            .unwrap();
        complete_fold(&store, &ctx, sid, f1, "fold 1 summary", vec![0.1; 768])
            .await
            .unwrap();

        let f2 = start_fold(&store, &ctx, sid, 1, Some(f1), "fold 2 context")
            .await
            .unwrap();
        complete_fold(&store, &ctx, sid, f2, "fold 2 summary", vec![0.2; 768])
            .await
            .unwrap();

        let results = retrieve_fold_context(&store, &ctx, sid, &[0.15; 768], Some(10), false)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].raw_trajectory.is_none());
    }

    #[tokio::test]
    async fn retrieve_with_include_raw() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let f = start_fold(&store, &ctx, sid, 0, None, "raw content here")
            .await
            .unwrap();
        complete_fold(&store, &ctx, sid, f, "summary", vec![0.1; 768])
            .await
            .unwrap();

        let results = retrieve_fold_context(&store, &ctx, sid, &[0.1; 768], Some(5), true)
            .await
            .unwrap();
        assert!(results[0].raw_trajectory.is_some());
    }

    #[tokio::test]
    async fn nested_folds_with_parent() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let parent = start_fold(&store, &ctx, sid, 0, None, "parent context")
            .await
            .unwrap();
        let child = start_fold(&store, &ctx, sid, 1, Some(parent), "child context")
            .await
            .unwrap();

        // Verify child has correct parent
        let fold = store.fold_get(&ctx, sid, child).await.unwrap().unwrap();
        assert_eq!(fold.parent_fold_id, Some(parent));
        assert_eq!(fold.depth, 1);
    }
}
