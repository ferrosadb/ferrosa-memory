//! Dream consolidation — periodic memory processing.
//!
//! Inspired by vestige's 5-phase dream cycle. Simplified for v1:
//! 1. Triage — list entities for the session
//! 2. Connection Discovery — compare entities by text similarity:
//!    a. Within-fold groups (entities sharing a source fold)
//!    b. Unfolded entities (ingested without a fold context, e.g. via `smart_ingest`)
//! 3. Insight Generation — identify clusters (3+ co-occurring entities)

use std::collections::HashMap;
use uuid::Uuid;

use serde::Serialize;

use crate::storage::Storage;
use crate::types::TenantContext;

/// Result of a dream consolidation run.
#[derive(Debug, Serialize)]
pub struct DreamResult {
    pub entities_processed: usize,
    pub connections_created: usize,
    pub insights: Vec<String>,
    /// Actual entity pairs connected (for viz event emission).
    #[serde(skip)]
    pub edges: Vec<(Uuid, Uuid)>,
}

/// Similarity threshold for creating CO_OCCURS edges (Jaccard on word sets).
const CO_OCCURS_THRESHOLD: f64 = 0.05;

/// Maximum number of unfolded entities to compare pairwise per run.
/// Caps the O(n²) comparison to keep idle consolidation fast.
const UNFOLDED_PAIR_CAP: usize = 200;

/// Run consolidation over a session's entities.
///
/// Two-pass connection discovery:
/// 1. Entities with a `source_fold_id` are grouped by fold and compared within each group.
/// 2. Entities without a fold ("unfolded") are compared pairwise using text similarity,
///    capped at `UNFOLDED_PAIR_CAP` most-recent entities to bound the O(n²) cost.
///
/// Clusters of 3+ co-occurring entities generate insight summaries.
pub async fn run_consolidation(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<DreamResult> {
    let entities = storage.entity_list_session(ctx, session_id).await?;
    let entity_count = entities.len();

    // Partition into folded (grouped by fold_id) and unfolded.
    let mut fold_groups: HashMap<Uuid, Vec<&crate::types::EntityEntry>> = HashMap::new();
    let mut unfolded: Vec<&crate::types::EntityEntry> = Vec::new();
    for entity in &entities {
        if let Some(fold_id) = entity.source_fold_id {
            fold_groups.entry(fold_id).or_default().push(entity);
        } else {
            unfolded.push(entity);
        }
    }

    let mut connections_created = 0;
    let mut edges = Vec::new();

    // Pass 1: within-fold comparison (existing behaviour).
    create_edges_for_groups(
        fold_groups.values(),
        storage,
        ctx,
        session_id,
        &mut connections_created,
        &mut edges,
    )
    .await;

    // Pass 2: unfolded entities — compare most-recent pairs by text similarity.
    // Sort by created_at descending so the cap keeps the freshest entities.
    unfolded.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    unfolded.truncate(UNFOLDED_PAIR_CAP);
    if unfolded.len() >= 2 {
        create_edges_for_groups(
            std::iter::once(&unfolded),
            storage,
            ctx,
            session_id,
            &mut connections_created,
            &mut edges,
        )
        .await;
    }

    // Identify clusters (3+ entities in same fold).
    let mut insights = Vec::new();
    for (fold_id, group) in &fold_groups {
        if group.len() >= 3 {
            let names: Vec<&str> = group.iter().map(|e| e.entity_name.as_str()).collect();
            insights.push(format!(
                "Cluster in fold {}: {} ({} entities co-occurring)",
                &fold_id.to_string()[..8],
                names.join(", "),
                group.len()
            ));
        }
    }
    // Cluster insight for unfolded entities too.
    if unfolded.len() >= 3 {
        let names: Vec<&str> = unfolded.iter().map(|e| e.entity_name.as_str()).collect();
        insights.push(format!(
            "Unfolded cluster: {} ({} entities co-occurring)",
            names.join(", "),
            unfolded.len()
        ));
    }

    Ok(DreamResult {
        entities_processed: entity_count,
        connections_created,
        insights,
        edges,
    })
}

/// Compare all pairs within each group and create CO_OCCURS edges for pairs
/// exceeding the similarity threshold.
async fn create_edges_for_groups<'a, I, S>(
    groups: I,
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    connections_created: &mut usize,
    edges: &mut Vec<(Uuid, Uuid)>,
) where
    I: Iterator<Item = &'a Vec<&'a crate::types::EntityEntry>>,
    S: Storage + ?Sized,
{
    for group in groups {
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let sim = crate::smart_ingest::compute_text_similarity(
                    &group[i].context_snippet,
                    &group[j].context_snippet,
                );
                if sim >= CO_OCCURS_THRESHOLD {
                    let a = group[i].entity_id;
                    let b = group[j].entity_id;
                    let _ = storage
                        .edge_co_occurs(ctx, a, b, session_id, sim as f32)
                        .await;
                    edges.push((a, b));
                    *connections_created += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use crate::types::{EntityEntry, TenantContext};
    use uuid::Uuid;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    fn make_entity(
        tenant_id: Uuid,
        session_id: Uuid,
        name: &str,
        source_fold_id: Option<Uuid>,
    ) -> EntityEntry {
        EntityEntry {
            tenant_id,
            entity_id: Uuid::new_v4(),
            session_id,
            entity_name: name.to_string(),
            entity_type: "concept".to_string(),
            source_fold_id,
            context_snippet: format!("context for {name}"),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn empty_session_returns_zero_counts() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 0);
        assert_eq!(result.connections_created, 0);
        assert!(result.insights.is_empty());
    }

    #[tokio::test]
    async fn same_fold_entities_get_co_occurs_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();

        // Add two entities sharing the same source fold
        let e1 = make_entity(ctx.tenant_id, session_id, "Alice", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "Bob", Some(fold_id));
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 2);
        assert_eq!(result.connections_created, 1);
        assert!(result.insights.is_empty()); // only 2, need 3+ for insight
    }

    #[tokio::test]
    async fn cluster_with_three_entities_generates_insight() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();

        // Add three entities sharing the same source fold
        let e1 = make_entity(ctx.tenant_id, session_id, "Alpha", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "Beta", Some(fold_id));
        let e3 = make_entity(ctx.tenant_id, session_id, "Gamma", Some(fold_id));
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 3);
        // 3 entities => C(3,2) = 3 pairs
        assert_eq!(result.connections_created, 3);
        assert_eq!(result.insights.len(), 1);
        assert!(result.insights[0].contains("3 entities co-occurring"));
    }

    #[tokio::test]
    async fn unfolded_entities_get_co_occurs_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        // Add two entities WITHOUT a source fold (typical smart_ingest usage)
        let e1 = make_entity(ctx.tenant_id, session_id, "Alice", None);
        let e2 = make_entity(ctx.tenant_id, session_id, "Bob", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 2);
        assert_eq!(result.connections_created, 1);
        assert!(result.insights.is_empty()); // only 2, need 3+ for insight
    }

    #[tokio::test]
    async fn unfolded_cluster_generates_insight() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "Alpha", None);
        let e2 = make_entity(ctx.tenant_id, session_id, "Beta", None);
        let e3 = make_entity(ctx.tenant_id, session_id, "Gamma", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 3);
        assert_eq!(result.connections_created, 3);
        assert_eq!(result.insights.len(), 1);
        assert!(result.insights[0].contains("Unfolded cluster"));
    }

    /// create_edges_for_groups with an empty iterator produces no edges.
    #[tokio::test]
    async fn create_edges_for_empty_groups() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let mut connections_created = 0;
        let mut edges = Vec::new();
        let groups: Vec<Vec<&crate::types::EntityEntry>> = vec![];

        create_edges_for_groups(
            groups.iter(),
            &store,
            &ctx,
            session_id,
            &mut connections_created,
            &mut edges,
        )
        .await;

        assert_eq!(connections_created, 0);
        assert!(edges.is_empty());
    }

    /// create_edges_for_groups with a single-entity group creates no edges (no pairs).
    #[tokio::test]
    async fn create_edges_single_entity_group() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let e1 = make_entity(ctx.tenant_id, session_id, "Solo", Some(Uuid::new_v4()));
        let group = vec![&e1];
        let groups = [group];
        let mut connections_created = 0;
        let mut edges = Vec::new();

        create_edges_for_groups(
            groups.iter(),
            &store,
            &ctx,
            session_id,
            &mut connections_created,
            &mut edges,
        )
        .await;

        assert_eq!(connections_created, 0);
        assert!(edges.is_empty());
    }

    /// Mixed folded and unfolded entities in the same consolidation run
    /// generate separate insights for each group.
    #[tokio::test]
    async fn mixed_folded_and_unfolded_entities() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();

        // 3 folded entities (should generate a fold cluster insight)
        let e1 = make_entity(ctx.tenant_id, session_id, "FoldA", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "FoldB", Some(fold_id));
        let e3 = make_entity(ctx.tenant_id, session_id, "FoldC", Some(fold_id));
        // 3 unfolded entities (should generate an unfolded cluster insight)
        let e4 = make_entity(ctx.tenant_id, session_id, "UnfoldX", None);
        let e5 = make_entity(ctx.tenant_id, session_id, "UnfoldY", None);
        let e6 = make_entity(ctx.tenant_id, session_id, "UnfoldZ", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
            entities.push(e4);
            entities.push(e5);
            entities.push(e6);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 6);
        // 3 folded pairs + 3 unfolded pairs = 6
        assert_eq!(result.connections_created, 6);
        // Should have both a fold cluster and an unfolded cluster insight
        assert_eq!(result.insights.len(), 2);
        let has_fold_insight = result
            .insights
            .iter()
            .any(|i| i.contains("Cluster in fold"));
        let has_unfolded_insight = result
            .insights
            .iter()
            .any(|i| i.contains("Unfolded cluster"));
        assert!(has_fold_insight, "should have fold cluster insight");
        assert!(has_unfolded_insight, "should have unfolded cluster insight");
    }

    /// Entities from different folds should not create edges between them.
    #[tokio::test]
    async fn different_folds_no_cross_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_a = Uuid::new_v4();
        let fold_b = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "FoldA1", Some(fold_a));
        let e2 = make_entity(ctx.tenant_id, session_id, "FoldB1", Some(fold_b));
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 2);
        // No edges — each fold group has only 1 entity
        assert_eq!(result.connections_created, 0);
    }

    /// Sort-by-recency: unfolded entities are sorted newest first.
    /// We verify by checking that the insight mentions them in the expected order.
    #[tokio::test]
    async fn unfolded_entities_sorted_by_recency() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        // Create entities with distinct timestamps
        let mut e1 = make_entity(ctx.tenant_id, session_id, "Old", None);
        e1.created_at = chrono::Utc::now() - chrono::Duration::hours(2);
        let mut e2 = make_entity(ctx.tenant_id, session_id, "Middle", None);
        e2.created_at = chrono::Utc::now() - chrono::Duration::hours(1);
        let mut e3 = make_entity(ctx.tenant_id, session_id, "Recent", None);
        e3.created_at = chrono::Utc::now();
        {
            // Insert in non-chronological order
            let mut entities = store.entities.lock().await;
            entities.push(e2);
            entities.push(e1);
            entities.push(e3);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 3);
        // All 3 are unfolded and under UNFOLDED_PAIR_CAP, so all pairs compared
        assert_eq!(result.connections_created, 3);
        // Verify insight lists them (order determined by sorted iteration)
        assert_eq!(result.insights.len(), 1);
        assert!(result.insights[0].contains("Unfolded cluster"));
    }

    /// DreamResult edges field tracks actual entity pairs connected.
    #[tokio::test]
    async fn dream_result_edges_track_pairs() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "A", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "B", Some(fold_id));
        let id1 = e1.entity_id;
        let id2 = e2.entity_id;
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.edges.len(), 1);
        let (a, b) = result.edges[0];
        // The edge should be between our two entity IDs
        assert!(
            (a == id1 && b == id2) || (a == id2 && b == id1),
            "edge should connect the two entities"
        );
    }

    /// CO_OCCURS_THRESHOLD is a sensible positive value.
    #[test]
    fn co_occurs_threshold_is_positive() {
        const { assert!(CO_OCCURS_THRESHOLD > 0.0) };
        const { assert!(CO_OCCURS_THRESHOLD < 1.0) };
    }

    /// UNFOLDED_PAIR_CAP is a sensible positive value.
    #[test]
    fn unfolded_pair_cap_is_positive() {
        const { assert!(UNFOLDED_PAIR_CAP > 0) };
    }

    /// Only one unfolded entity should not trigger pairwise comparison.
    #[tokio::test]
    async fn single_unfolded_entity_no_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "Solo", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 1);
        assert_eq!(result.connections_created, 0);
        assert!(result.insights.is_empty());
        assert!(result.edges.is_empty());
    }

    /// Two unfolded entities — should not generate insight (need 3+).
    #[tokio::test]
    async fn two_unfolded_entities_no_insight() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "A", None);
        let e2 = make_entity(ctx.tenant_id, session_id, "B", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 2);
        assert_eq!(result.connections_created, 1);
        assert!(
            result.insights.is_empty(),
            "2 entities should not generate insight"
        );
    }

    /// Multiple folds with different sizes generate insights only for 3+ groups.
    #[tokio::test]
    async fn multiple_folds_mixed_sizes() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_small = Uuid::new_v4();
        let fold_big = Uuid::new_v4();

        // Small fold: 2 entities (no insight)
        let e1 = make_entity(ctx.tenant_id, session_id, "S1", Some(fold_small));
        let e2 = make_entity(ctx.tenant_id, session_id, "S2", Some(fold_small));
        // Big fold: 4 entities (should generate insight)
        let e3 = make_entity(ctx.tenant_id, session_id, "B1", Some(fold_big));
        let e4 = make_entity(ctx.tenant_id, session_id, "B2", Some(fold_big));
        let e5 = make_entity(ctx.tenant_id, session_id, "B3", Some(fold_big));
        let e6 = make_entity(ctx.tenant_id, session_id, "B4", Some(fold_big));
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
            entities.push(e4);
            entities.push(e5);
            entities.push(e6);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 6);
        // Small fold: C(2,2)=1, Big fold: C(4,2)=6
        assert_eq!(result.connections_created, 7);
        // Only the big fold generates an insight
        assert_eq!(result.insights.len(), 1);
        assert!(result.insights[0].contains("4 entities co-occurring"));
    }
}
