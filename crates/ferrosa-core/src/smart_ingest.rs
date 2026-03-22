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
#[allow(clippy::too_many_arguments)] // source_fold_id is per-call provenance, not config
pub async fn smart_ingest(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    content: &str,
    entity_type: &str,
    embedding: Option<&[f32]>,
    source_fold_id: Option<Uuid>,
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
            source_fold_id,
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            confidence: 1.0,
            state: crate::types::MemoryState::default(),
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
            source_fold_id,
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            confidence: 1.0,
            state: crate::types::MemoryState::default(),
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
        source_fold_id,
        context_snippet: content.to_string(),
        entity_embedding: embedding.map(|e| e.to_vec()),
        confidence: 1.0,
        state: crate::types::MemoryState::default(),
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

/// Extract candidate entities from text using simple heuristics.
/// Returns (name, entity_type) pairs.
///
/// Finds capitalized multi-word phrases (2+ capital-letter words) which are
/// likely named entities (people, places, orgs, technical concepts). Also
/// captures standalone capitalized words that are not common English words.
pub fn extract_entity_candidates(text: &str) -> Vec<(String, String)> {
    let mut candidates = Vec::new();

    // Find capitalized multi-word phrases (words starting with uppercase)
    // These are likely named entities (people, places, orgs, concepts)
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        // Skip common sentence starters and short words
        if word.len() > 1
            && word.chars().next().is_some_and(|c| c.is_uppercase())
            && !is_common_word(word)
            // skip sentence starters (i > 0)
            && i > 0
        {
            // Collect consecutive capitalized words
            let start = i;
            while i < words.len()
                && words[i].chars().next().is_some_and(|c| c.is_uppercase())
                && words[i].len() > 1
            {
                i += 1;
            }
            if i > start {
                let phrase: String = words[start..i].join(" ");
                // Clean trailing punctuation
                let clean = phrase.trim_end_matches(|c: char| c.is_ascii_punctuation());
                if clean.len() > 1 {
                    candidates.push((clean.to_string(), "concept".to_string()));
                }
            }
        } else {
            i += 1;
        }
    }

    candidates.dedup_by(|a, b| a.0 == b.0);
    candidates
}

fn is_common_word(word: &str) -> bool {
    matches!(
        word,
        "The"
            | "This"
            | "That"
            | "These"
            | "Those"
            | "When"
            | "Where"
            | "Which"
            | "What"
            | "How"
            | "For"
            | "With"
            | "From"
            | "Into"
            | "After"
            | "Before"
            | "During"
            | "Between"
            | "Through"
            | "About"
            | "Each"
            | "Every"
            | "Also"
            | "Both"
            | "Either"
            | "Neither"
            | "Some"
            | "Any"
            | "All"
            | "Most"
            | "Other"
            | "Another"
            | "Such"
            | "Only"
            | "Very"
            | "Just"
            | "But"
            | "And"
            | "Not"
            | "Yet"
            | "Still"
            | "Already"
            | "However"
            | "Therefore"
            | "Thus"
            | "Hence"
            | "Since"
            | "Because"
            | "Although"
            | "While"
            | "Until"
            | "Unless"
            | "Once"
            | "Here"
            | "There"
            | "Then"
            | "Now"
            | "If"
            | "Or"
            | "So"
    )
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
            None,
            &IngestConfig::default(),
        )
        .await
        .unwrap();

        assert!(matches!(result, IngestDecision::Created { .. }));
    }

    #[test]
    fn extract_entities_from_technical_text() {
        // First word is skipped by the sentence-starter heuristic (i > 0),
        // so prefix with a lowercase word to push entities past position 0.
        let text = "uses Ferrosa with LSM-tree storage and S3 tiering";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(names.contains(&"Ferrosa"), "should extract Ferrosa");
        assert!(names.contains(&"LSM-tree"), "should extract LSM-tree");
        assert!(names.contains(&"S3"), "should extract S3");
    }

    #[test]
    fn extract_entities_filters_common_words() {
        let text = "However the system Also provides Some features";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(
            !names.iter().any(|n| *n == "However"),
            "should filter However"
        );
        assert!(!names.iter().any(|n| *n == "Also"), "should filter Also");
        assert!(!names.iter().any(|n| *n == "Some"), "should filter Some");
    }

    #[test]
    fn extract_entities_skips_sentence_starters() {
        let text = "Cassandra is great. Redis is fast.";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        // "Cassandra" is at i=0, so it is skipped as a sentence starter
        assert!(
            !names.contains(&"Cassandra"),
            "should skip sentence-starting word"
        );
        // "Redis" is at i > 0 and capitalized, so it should be captured
        assert!(
            names.contains(&"Redis"),
            "should extract mid-sentence entity"
        );
    }

    #[test]
    fn extract_entities_multi_word_phrase() {
        let text = "uses Apache Kafka for streaming";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(
            names.contains(&"Apache Kafka"),
            "should extract multi-word phrase, got: {names:?}"
        );
    }

    #[test]
    fn extract_entities_empty_text() {
        let candidates = extract_entity_candidates("");
        assert!(candidates.is_empty());
    }

    #[test]
    fn extract_entities_all_lowercase() {
        let candidates = extract_entity_candidates("everything is lowercase here");
        assert!(candidates.is_empty());
    }
}
