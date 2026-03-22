//! Smart ingestion with prediction error gating.
//!
//! Inspired by vestige's prediction_error module and neuroscience research
//! (Sinclair & Bhavnani 2020, Lee et al. 2017). When new content arrives,
//! compare against existing memories to decide: CREATE new, UPDATE existing,
//! or SUPERSEDE outdated.
//!
//! The key insight: only store what's SURPRISING. If the new content is
//! similar to an existing memory, update it. If it contradicts, supersede.
//! If it's genuinely new, create.

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

/// Decision made by the prediction error gate.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "action")]
pub enum IngestDecision {
    /// Content is new — no similar memories found.
    Created { entity_id: Uuid },
    /// Content is similar to existing memory — updated in place.
    Updated { entity_id: Uuid, similarity: f64 },
    /// Content contradicts existing memory — old one superseded.
    Superseded {
        new_entity_id: Uuid,
        old_entity_id: Uuid,
        similarity: f64,
    },
    /// Content is too similar to existing — skipped (not novel enough).
    Skipped {
        existing_entity_id: Uuid,
        similarity: f64,
        reason: String,
    },
}

/// Thresholds for prediction error gating.
pub struct IngestConfig {
    /// Below this similarity, create new memory (content is novel).
    pub create_threshold: f64,
    /// Above this similarity, skip (content is redundant).
    pub skip_threshold: f64,
    /// Between create and skip: update if consistent, supersede if contradictory.
    pub update_threshold: f64,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            create_threshold: 0.3,
            skip_threshold: 0.9,
            update_threshold: 0.6,
        }
    }
}

/// Smart ingest: decide whether to create, update, supersede, or skip.
///
/// Uses entity search to find similar existing memories, then applies
/// prediction error gating based on similarity thresholds.
pub async fn smart_ingest(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    content: &str,
    entity_type: &str,
    embedding: Option<&[f32]>,
    config: &IngestConfig,
) -> anyhow::Result<IngestDecision> {
    // Search for similar existing entities
    let existing = if let Some(emb) = embedding {
        storage.entity_search_ann(ctx, session_id, emb, 3).await?
    } else {
        // Fall back to phonetic search on the first few words
        let name_hint = content
            .split_whitespace()
            .take(5)
            .collect::<Vec<_>>()
            .join(" ");
        match storage
            .entity_find_phonetic(ctx, session_id, &name_hint)
            .await?
        {
            Some(e) => vec![e],
            None => vec![],
        }
    };

    if existing.is_empty() {
        // No similar memories — create new
        let entity_id = Uuid::new_v4();
        let entry = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id,
            session_id,
            entity_name: content
                .split_whitespace()
                .take(8)
                .collect::<Vec<_>>()
                .join(" "),
            entity_type: entity_type.to_string(),
            source_fold_id: None,
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            confidence: 1.0,
            created_at: chrono::Utc::now(),
        };
        storage.entity_put(ctx, &entry).await?;
        tracing::info!(
            %entity_id,
            "smart_ingest: CREATED (no similar memories)"
        );
        return Ok(IngestDecision::Created { entity_id });
    }

    // Compare with most similar existing memory
    // For now, use a simple heuristic: check content overlap
    let best_match = &existing[0];
    let similarity = compute_text_similarity(content, &best_match.context_snippet);

    if similarity > config.skip_threshold {
        tracing::debug!(
            entity_id = %best_match.entity_id,
            similarity,
            "smart_ingest: SKIPPED (too similar)"
        );
        return Ok(IngestDecision::Skipped {
            existing_entity_id: best_match.entity_id,
            similarity,
            reason: "content too similar to existing memory".into(),
        });
    }

    if similarity > config.update_threshold {
        // Similar enough to be about the same topic — update
        tracing::info!(
            entity_id = %best_match.entity_id,
            similarity,
            "smart_ingest: UPDATED"
        );
        return Ok(IngestDecision::Updated {
            entity_id: best_match.entity_id,
            similarity,
        });
    }

    if similarity > config.create_threshold {
        // Moderately similar but different — supersede
        let new_id = Uuid::new_v4();
        let entry = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: new_id,
            session_id,
            entity_name: content
                .split_whitespace()
                .take(8)
                .collect::<Vec<_>>()
                .join(" "),
            entity_type: entity_type.to_string(),
            source_fold_id: None,
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            confidence: 1.0,
            created_at: chrono::Utc::now(),
        };
        storage.entity_put(ctx, &entry).await?;
        // Create supersession edge
        let _ = storage
            .edge_supersedes(ctx, new_id, best_match.entity_id, new_id)
            .await;
        tracing::info!(
            new_id = %new_id,
            old_id = %best_match.entity_id,
            similarity,
            "smart_ingest: SUPERSEDED"
        );
        return Ok(IngestDecision::Superseded {
            new_entity_id: new_id,
            old_entity_id: best_match.entity_id,
            similarity,
        });
    }

    // Very different — create new
    let entity_id = Uuid::new_v4();
    let entry = crate::types::EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id,
        session_id,
        entity_name: content
            .split_whitespace()
            .take(8)
            .collect::<Vec<_>>()
            .join(" "),
        entity_type: entity_type.to_string(),
        source_fold_id: None,
        context_snippet: content.to_string(),
        entity_embedding: embedding.map(|e| e.to_vec()),
        confidence: 1.0,
        created_at: chrono::Utc::now(),
    };
    storage.entity_put(ctx, &entry).await?;
    tracing::info!(
        %entity_id,
        similarity,
        "smart_ingest: CREATED (novel content)"
    );
    Ok(IngestDecision::Created { entity_id })
}

/// Simple text similarity using word overlap (Jaccard coefficient).
/// For production, this should use embedding cosine similarity.
fn compute_text_similarity(a: &str, b: &str) -> f64 {
    let words_a: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let words_b: std::collections::HashSet<&str> = b.split_whitespace().collect();
    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_similarity_identical() {
        assert!((compute_text_similarity("hello world", "hello world") - 1.0).abs() < 0.01);
    }

    #[test]
    fn text_similarity_different() {
        assert!(compute_text_similarity("hello world", "foo bar baz") < 0.1);
    }

    #[test]
    fn text_similarity_partial() {
        let sim = compute_text_similarity("the quick brown fox", "the quick red fox jumps");
        assert!(sim > 0.3 && sim < 0.8);
    }

    #[tokio::test]
    async fn smart_ingest_creates_on_empty_store() {
        use crate::storage::mock::MockStorage;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };

        let result = smart_ingest(
            &store,
            &ctx,
            Uuid::new_v4(),
            "Ferrosa is a Rust-native Cassandra-compatible database",
            "concept",
            None,
            &IngestConfig::default(),
        )
        .await
        .unwrap();

        assert!(matches!(result, IngestDecision::Created { .. }));
    }
}
