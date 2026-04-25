//! Duplicate detection -- find semantically similar entities that may be duplicates.

use serde::Serialize;
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

#[derive(Debug, Clone, Serialize)]
pub struct DuplicatePair {
    pub entity_a: Uuid,
    pub entity_b: Uuid,
    pub name_a: String,
    pub name_b: String,
    pub similarity: f64,
}

/// Find potential duplicate entities in a session.
/// Uses text similarity (Jaccard coefficient) on context snippets.
pub async fn find_duplicates(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    threshold: f64,
) -> anyhow::Result<Vec<DuplicatePair>> {
    anyhow::ensure!(
        (0.0..=1.0).contains(&threshold),
        "threshold must be between 0.0 and 1.0, got {threshold}"
    );

    let entities = storage.entity_list_session(ctx, session_id).await?;
    let mut pairs = Vec::new();

    // O(n^2) -- acceptable for <1000 entities per session
    for i in 0..entities.len() {
        for j in (i + 1)..entities.len() {
            let sim = crate::smart_ingest::compute_text_similarity(
                &entities[i].context_snippet,
                &entities[j].context_snippet,
            );
            if sim >= threshold {
                pairs.push(DuplicatePair {
                    entity_a: entities[i].entity_id,
                    entity_b: entities[j].entity_id,
                    name_a: entities[i].entity_name.clone(),
                    name_b: entities[j].entity_name.clone(),
                    similarity: sim,
                });
            }
        }
    }

    pairs.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use crate::types::{EntityEntry, MemoryState, TenantContext};

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    fn make_entity(session_id: Uuid, tenant_id: Uuid, name: &str, snippet: &str) -> EntityEntry {
        EntityEntry {
            tenant_id,
            entity_id: Uuid::new_v4(),
            session_id,
            entity_name: name.to_string(),
            entity_type: "concept".to_string(),
            source_fold_id: None,
            context_snippet: snippet.to_string(),
            entity_embedding: None,
            confidence: 1.0,
            state: MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn finds_duplicates_above_threshold() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(
            session_id,
            ctx.tenant_id,
            "Rust language",
            "Rust is a systems programming language focused on safety and performance",
        );
        let e2 = make_entity(
            session_id,
            ctx.tenant_id,
            "Rust lang",
            "Rust is a systems programming language focused on safety and concurrency",
        );

        store.entities.lock().await.push(e1.clone());
        store.entities.lock().await.push(e2.clone());

        let pairs = find_duplicates(&store, &ctx, session_id, 0.5)
            .await
            .unwrap();
        assert_eq!(pairs.len(), 1, "should find one duplicate pair");
        assert!(pairs[0].similarity >= 0.5);
        assert_eq!(pairs[0].name_a, "Rust language");
        assert_eq!(pairs[0].name_b, "Rust lang");
    }

    #[tokio::test]
    async fn no_duplicates_below_threshold() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(
            session_id,
            ctx.tenant_id,
            "Rust language",
            "Rust is a systems programming language focused on safety",
        );
        let e2 = make_entity(
            session_id,
            ctx.tenant_id,
            "Japanese cuisine",
            "Sushi is a traditional Japanese dish made with vinegared rice",
        );

        store.entities.lock().await.push(e1);
        store.entities.lock().await.push(e2);

        let pairs = find_duplicates(&store, &ctx, session_id, 0.5)
            .await
            .unwrap();
        assert!(
            pairs.is_empty(),
            "unrelated entities should not be duplicates"
        );
    }

    #[tokio::test]
    async fn empty_session_returns_empty() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let pairs = find_duplicates(&store, &ctx, session_id, 0.5)
            .await
            .unwrap();
        assert!(pairs.is_empty());
    }

    #[tokio::test]
    async fn single_entity_returns_empty() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(
            session_id,
            ctx.tenant_id,
            "Rust",
            "Rust programming language",
        );
        store.entities.lock().await.push(e1);

        let pairs = find_duplicates(&store, &ctx, session_id, 0.5)
            .await
            .unwrap();
        assert!(pairs.is_empty());
    }

    #[tokio::test]
    async fn results_sorted_by_similarity_descending() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        // Three entities: e1 and e2 are very similar, e1 and e3 are somewhat similar
        let e1 = make_entity(
            session_id,
            ctx.tenant_id,
            "Entity A",
            "the quick brown fox jumps over the lazy dog",
        );
        let e2 = make_entity(
            session_id,
            ctx.tenant_id,
            "Entity B",
            "the quick brown fox jumps over the lazy cat",
        );
        let e3 = make_entity(
            session_id,
            ctx.tenant_id,
            "Entity C",
            "the quick red fox runs over the lazy dog",
        );

        store.entities.lock().await.push(e1);
        store.entities.lock().await.push(e2);
        store.entities.lock().await.push(e3);

        let pairs = find_duplicates(&store, &ctx, session_id, 0.3)
            .await
            .unwrap();
        assert!(pairs.len() >= 2, "should find at least two pairs");

        // Verify descending order
        for window in pairs.windows(2) {
            assert!(
                window[0].similarity >= window[1].similarity,
                "pairs should be sorted by similarity descending"
            );
        }
    }

    #[tokio::test]
    async fn threshold_boundary() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        // Identical snippets should have similarity 1.0
        let e1 = make_entity(
            session_id,
            ctx.tenant_id,
            "Entity A",
            "identical content here",
        );
        let e2 = make_entity(
            session_id,
            ctx.tenant_id,
            "Entity B",
            "identical content here",
        );

        store.entities.lock().await.push(e1);
        store.entities.lock().await.push(e2);

        // threshold=1.0 should still find exact matches
        let pairs = find_duplicates(&store, &ctx, session_id, 1.0)
            .await
            .unwrap();
        assert_eq!(pairs.len(), 1);
        assert!((pairs[0].similarity - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn invalid_threshold_rejected() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let result = find_duplicates(&store, &ctx, session_id, 1.5).await;
        assert!(result.is_err(), "threshold > 1.0 should be rejected");

        let result = find_duplicates(&store, &ctx, session_id, -0.1).await;
        assert!(result.is_err(), "threshold < 0.0 should be rejected");
    }
}
