//! Hybrid search — multi-strategy retrieval with Reciprocal Rank Fusion.

use serde::Serialize;
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub id: Uuid,
    pub source: String,
    pub content: String,
    pub score: f64,
    pub result_type: String,
}

/// Reciprocal Rank Fusion: merge ranked lists.
///
/// Each item's RRF score is `sum(1 / (k + rank + 1))` across all lists
/// where it appears. The `k` parameter (typically 60) controls how much
/// lower-ranked items are penalized.
fn rrf_merge(lists: Vec<Vec<SearchResult>>, k: f64) -> Vec<SearchResult> {
    use std::collections::HashMap;

    assert!(k >= 0.0, "RRF k parameter must be non-negative");

    let mut scores: HashMap<Uuid, (f64, SearchResult)> = HashMap::new();
    for list in &lists {
        for (rank, item) in list.iter().enumerate() {
            let rrf_score = 1.0 / (k + rank as f64 + 1.0);
            scores
                .entry(item.id)
                .and_modify(|(s, _)| *s += rrf_score)
                .or_insert((rrf_score, item.clone()));
        }
    }
    let mut merged: Vec<SearchResult> = scores
        .into_values()
        .map(|(score, mut r)| {
            r.score = score;
            r
        })
        .collect();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged
}

/// Run a hybrid search combining phonetic entity lookup, ANN entity search,
/// and ANN fold search. Results are fused via Reciprocal Rank Fusion.
pub async fn hybrid_search(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query: &str,
    embedding: Option<&[f32]>,
    limit: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    anyhow::ensure!(!query.is_empty(), "query must not be empty");
    anyhow::ensure!(limit > 0 && limit <= 50, "limit must be between 1 and 50");

    let mut lists = Vec::new();

    // Strategy 1: Phonetic entity search
    if let Ok(Some(entity)) = storage.entity_find_phonetic(ctx, session_id, query).await {
        lists.push(vec![SearchResult {
            id: entity.entity_id,
            source: "entity_phonetic".into(),
            content: entity.context_snippet.clone(),
            score: 1.0,
            result_type: "entity".into(),
        }]);
    }

    // Strategy 2: ANN entity search
    if let Some(emb) = embedding
        && let Ok(entities) = storage.entity_search_ann(ctx, session_id, emb, limit).await
    {
        lists.push(
            entities
                .into_iter()
                .map(|e| SearchResult {
                    id: e.entity_id,
                    source: "entity_ann".into(),
                    content: e.context_snippet,
                    score: 1.0,
                    result_type: "entity".into(),
                })
                .collect(),
        );
    }

    // Strategy 3: ANN fold search
    if let Some(emb) = embedding
        && let Ok(folds) = storage
            .fold_search(ctx, session_id, emb, limit, false)
            .await
    {
        lists.push(
            folds
                .into_iter()
                .map(|f| SearchResult {
                    id: f.fold_id,
                    source: "fold_ann".into(),
                    content: f.fold_summary,
                    score: f.similarity.unwrap_or(0.0),
                    result_type: "fold".into(),
                })
                .collect(),
        );
    }

    let merged = rrf_merge(lists, 60.0);
    Ok(merged.into_iter().take(limit).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_merge_empty_lists() {
        let result = rrf_merge(vec![], 60.0);
        assert!(result.is_empty());
    }

    #[test]
    fn rrf_merge_single_list() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let list = vec![
            SearchResult {
                id: id1,
                source: "a".into(),
                content: "first".into(),
                score: 1.0,
                result_type: "entity".into(),
            },
            SearchResult {
                id: id2,
                source: "a".into(),
                content: "second".into(),
                score: 1.0,
                result_type: "entity".into(),
            },
        ];
        let merged = rrf_merge(vec![list], 60.0);
        assert_eq!(merged.len(), 2);
        // Rank 0 score: 1/(60+0+1) = 1/61
        assert!((merged[0].score - 1.0 / 61.0).abs() < 1e-10);
        // Rank 1 score: 1/(60+1+1) = 1/62
        assert!((merged[1].score - 1.0 / 62.0).abs() < 1e-10);
        assert_eq!(merged[0].id, id1);
        assert_eq!(merged[1].id, id2);
    }

    #[test]
    fn rrf_merge_overlapping_lists_boosts_shared_items() {
        let shared_id = Uuid::new_v4();
        let unique_id = Uuid::new_v4();

        let list_a = vec![SearchResult {
            id: shared_id,
            source: "a".into(),
            content: "shared".into(),
            score: 1.0,
            result_type: "entity".into(),
        }];
        let list_b = vec![
            SearchResult {
                id: unique_id,
                source: "b".into(),
                content: "unique".into(),
                score: 1.0,
                result_type: "fold".into(),
            },
            SearchResult {
                id: shared_id,
                source: "b".into(),
                content: "shared".into(),
                score: 1.0,
                result_type: "entity".into(),
            },
        ];

        let merged = rrf_merge(vec![list_a, list_b], 60.0);
        assert_eq!(merged.len(), 2);

        // shared_id appears at rank 0 in list_a and rank 1 in list_b
        // score = 1/61 + 1/62
        let expected_shared = 1.0 / 61.0 + 1.0 / 62.0;
        // unique_id appears at rank 0 in list_b only
        let expected_unique = 1.0 / 61.0;

        // Shared item should rank higher due to fusion boost
        assert_eq!(merged[0].id, shared_id);
        assert!((merged[0].score - expected_shared).abs() < 1e-10);
        assert_eq!(merged[1].id, unique_id);
        assert!((merged[1].score - expected_unique).abs() < 1e-10);
    }

    #[test]
    fn rrf_merge_preserves_ordering() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let list: Vec<SearchResult> = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| SearchResult {
                id,
                source: "test".into(),
                content: format!("item {i}"),
                score: 1.0,
                result_type: "entity".into(),
            })
            .collect();

        let merged = rrf_merge(vec![list], 60.0);
        // Should be in descending score order (rank 0 has highest RRF score)
        for i in 0..merged.len() - 1 {
            assert!(merged[i].score >= merged[i + 1].score);
        }
    }
}
