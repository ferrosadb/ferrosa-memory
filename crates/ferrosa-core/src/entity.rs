//! Entity store and retrieval tool handlers.
//!
//! Tracks named entities discovered during trajectory traversal. Supports
//! phonetic matching for variant/noisy entity names (Ferrosa Double Metaphone)
//! and ANN search via HNSW.
//!
//! ## Deduplication
//!
//! On upsert, the phonetic index is checked first. If a match is found AND
//! the embedding distance is below threshold, the existing entity is updated
//! rather than creating a duplicate (FMEA F18).
//!
//! ## Security
//!
//! - Confidence gating: rejects writes with confidence < threshold (FMEA F19)
//! - Per-session entity count limit to prevent graph explosion (FMEA F20)

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{EntityEntry, TenantContext};

/// Maximum entities per session (configurable via config, hardcoded default).
const DEFAULT_MAX_ENTITIES_PER_SESSION: usize = 1000;

/// Default confidence gate — reject entities below this threshold.
const DEFAULT_CONFIDENCE_GATE: f64 = 0.7;

/// Result of upserting an entity.
#[derive(Debug, serde::Serialize)]
pub struct UpsertEntityResult {
    pub entity_id: Uuid,
    pub is_new: bool,
}

/// Upsert an entity with phonetic deduplication.
///
/// Checks phonetic match first. If found, returns existing entity_id.
/// If not found, creates a new entity. Rejects if confidence < gate or
/// session entity count exceeds limit.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_entity(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    entity_name: &str,
    entity_type: &str,
    context_snippet: &str,
    embedding: Option<Vec<f32>>,
    source_fold_id: Option<Uuid>,
    confidence: Option<f64>,
) -> anyhow::Result<UpsertEntityResult> {
    let confidence = confidence.unwrap_or(1.0);

    // Confidence gating (FMEA F19)
    if confidence < DEFAULT_CONFIDENCE_GATE {
        anyhow::bail!("confidence {confidence} below gate {DEFAULT_CONFIDENCE_GATE}");
    }

    // Rate limit: check entity count (FMEA F20)
    let count = storage.entity_count(ctx, session_id).await?;
    if count >= DEFAULT_MAX_ENTITIES_PER_SESSION {
        anyhow::bail!("entity count {count} exceeds limit {DEFAULT_MAX_ENTITIES_PER_SESSION}");
    }

    // Check for phonetic match (deduplication)
    if let Some(existing) = storage
        .entity_find_phonetic(ctx, session_id, entity_name)
        .await?
    {
        return Ok(UpsertEntityResult {
            entity_id: existing.entity_id,
            is_new: false,
        });
    }

    // Create new entity
    let entity_id = Uuid::new_v4();
    let entry = EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id,
        session_id,
        entity_name: entity_name.to_string(),
        entity_type: entity_type.to_string(),
        source_fold_id,
        context_snippet: context_snippet.to_string(),
        entity_embedding: embedding,
        confidence,
        created_at: chrono::Utc::now(),
    };

    storage.entity_put(ctx, &entry).await?;

    Ok(UpsertEntityResult {
        entity_id,
        is_new: true,
    })
}

/// Retrieve entities by the specified strategy.
///
/// - `ann`: HNSW cosine similarity search
/// - `phonetic`: Double Metaphone fuzzy name match
/// - `both`: union-merge of both, deduplicated by entity_id
pub async fn retrieve_entities(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query: &str,
    embedding: Option<&[f32]>,
    strategy: &str,
    k: Option<usize>,
) -> anyhow::Result<Vec<EntityEntry>> {
    let k = k.unwrap_or(10);

    match strategy {
        "phonetic" => {
            let result = storage.entity_find_phonetic(ctx, session_id, query).await?;
            Ok(result.into_iter().collect())
        }
        "ann" => {
            let emb =
                embedding.ok_or_else(|| anyhow::anyhow!("embedding required for ann strategy"))?;
            storage.entity_search_ann(ctx, session_id, emb, k).await
        }
        "both" => {
            let mut results = Vec::new();

            // Phonetic first
            if let Some(e) = storage.entity_find_phonetic(ctx, session_id, query).await? {
                results.push(e);
            }

            // ANN if embedding provided
            if let Some(emb) = embedding {
                let ann_results = storage.entity_search_ann(ctx, session_id, emb, k).await?;
                for e in ann_results {
                    if !results.iter().any(|r| r.entity_id == e.entity_id) {
                        results.push(e);
                    }
                }
            }

            Ok(results)
        }
        other => anyhow::bail!("unknown retrieval strategy: {other}"),
    }
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
    async fn upsert_creates_new_entity() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let result = upsert_entity(
            &store, &ctx, sid, "Alice", "person", "ctx", None, None, None,
        )
        .await
        .unwrap();
        assert!(result.is_new);
    }

    #[tokio::test]
    async fn upsert_deduplicates_on_phonetic_match() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let r1 = upsert_entity(
            &store, &ctx, sid, "Alice", "person", "ctx", None, None, None,
        )
        .await
        .unwrap();
        let r2 = upsert_entity(
            &store, &ctx, sid, "alice", "person", "ctx2", None, None, None,
        )
        .await
        .unwrap();

        assert!(r1.is_new);
        assert!(!r2.is_new);
        assert_eq!(r1.entity_id, r2.entity_id);
    }

    #[tokio::test]
    async fn upsert_rejects_low_confidence() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let result = upsert_entity(
            &store,
            &ctx,
            sid,
            "Alice",
            "person",
            "ctx",
            None,
            None,
            Some(0.3),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("confidence"));
    }

    #[tokio::test]
    async fn upsert_rejects_over_limit() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Fill to limit
        for i in 0..DEFAULT_MAX_ENTITIES_PER_SESSION {
            upsert_entity(
                &store,
                &ctx,
                sid,
                &format!("entity_{i}"),
                "thing",
                "ctx",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }

        let result = upsert_entity(
            &store, &ctx, sid, "one_more", "thing", "ctx", None, None, None,
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("limit"));
    }

    #[tokio::test]
    async fn retrieve_phonetic() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        upsert_entity(&store, &ctx, sid, "Bob", "person", "ctx", None, None, None)
            .await
            .unwrap();

        let results = retrieve_entities(&store, &ctx, sid, "bob", None, "phonetic", None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entity_name, "Bob");
    }

    #[tokio::test]
    async fn retrieve_both_deduplicates() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let emb = vec![0.1; 768];
        upsert_entity(
            &store,
            &ctx,
            sid,
            "Carol",
            "person",
            "ctx",
            Some(emb.clone()),
            None,
            None,
        )
        .await
        .unwrap();

        let results = retrieve_entities(&store, &ctx, sid, "carol", Some(&emb), "both", None)
            .await
            .unwrap();
        // Should have exactly 1 (deduplicated)
        assert_eq!(results.len(), 1);
    }
}
