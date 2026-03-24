//! Dream consolidation — periodic memory processing.
//!
//! Inspired by vestige's 5-phase dream cycle. Simplified for v1:
//! 1. Triage — list entities for the session
//! 2. Connection Discovery — find co-occurring entities (same source fold), create CO_OCCURS edges
//! 3. Insight Generation — identify clusters (3+ entities in same fold)

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
}

/// Run consolidation over a session's entities.
///
/// Groups entities by source fold, creates CO_OCCURS edges between
/// entities sharing a fold, and identifies clusters of 3+ co-occurring
/// entities as insights.
pub async fn run_consolidation(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<DreamResult> {
    let entities = storage.entity_list_session(ctx, session_id).await?;
    let entity_count = entities.len();

    // Group by source_fold_id
    let mut fold_groups: HashMap<Uuid, Vec<&crate::types::EntityEntry>> = HashMap::new();
    for entity in &entities {
        if let Some(fold_id) = entity.source_fold_id {
            fold_groups.entry(fold_id).or_default().push(entity);
        }
    }

    // Create CO_OCCURS edges between pairs in same fold that share context.
    // Uses text similarity to avoid linking unrelated entities in large folds.
    let mut connections_created = 0;
    for group in fold_groups.values() {
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let sim = crate::smart_ingest::compute_text_similarity(
                    &group[i].context_snippet,
                    &group[j].context_snippet,
                );
                if sim >= 0.05 {
                    let _ = storage
                        .edge_co_occurs(ctx, group[i].entity_id, group[j].entity_id, session_id)
                        .await;
                    connections_created += 1;
                }
            }
        }
    }

    // Identify clusters (3+ entities in same fold)
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

    Ok(DreamResult {
        entities_processed: entity_count,
        connections_created,
        insights,
    })
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
}
