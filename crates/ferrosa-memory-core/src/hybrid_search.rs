//! Hybrid search — multi-strategy retrieval with Reciprocal Rank Fusion.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: Uuid,
    pub source: String,
    pub content: String,
    pub score: f64,
    pub result_type: String,
}

/// Configuration for 5-signal RRF fusion weights.
/// Default weight 1.0 for all signals. Set to 0.0 to disable a signal.
#[derive(Debug, Clone)]
pub struct FusionConfig {
    pub phonetic_weight: f64,
    pub ann_weight: f64,
    pub fold_weight: f64,
    pub warmth_weight: f64,
    pub pagerank_weight: f64,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            phonetic_weight: 1.0,
            ann_weight: 1.0,
            fold_weight: 1.0,
            warmth_weight: 1.0,
            pagerank_weight: 1.0,
        }
    }
}

/// Reciprocal Rank Fusion: merge ranked lists with per-signal weights.
///
/// Each item's RRF score is `sum(weight_i / (k + rank + 1))` across all lists
/// where it appears. The `k` parameter (typically 60) controls how much
/// lower-ranked items are penalized. Each list's contribution is scaled by
/// the corresponding entry in `weights` (defaults to 1.0 if not provided).
fn rrf_merge(lists: Vec<Vec<SearchResult>>, k: f64, weights: &[f64]) -> Vec<SearchResult> {
    assert!(k >= 0.0, "RRF k parameter must be non-negative");

    let mut scores: HashMap<Uuid, (f64, SearchResult)> = HashMap::new();
    for (list_idx, list) in lists.iter().enumerate() {
        let weight = weights.get(list_idx).copied().unwrap_or(1.0);
        for (rank, item) in list.iter().enumerate() {
            let rrf_score = weight / (k + rank as f64 + 1.0);
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

/// Run a hybrid search combining up to 5 signals: phonetic entity lookup,
/// ANN entity search, ANN fold search, warmth scores, and pagerank scores.
/// Results are fused via weighted Reciprocal Rank Fusion.
#[allow(clippy::too_many_arguments)]
pub async fn hybrid_search(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query: &str,
    embedding: Option<&[f32]>,
    limit: usize,
    warmth_scores: Option<&HashMap<Uuid, f64>>,
    pagerank_scores: Option<&HashMap<Uuid, f64>>,
    config: &FusionConfig,
) -> anyhow::Result<Vec<SearchResult>> {
    anyhow::ensure!(!query.is_empty(), "query must not be empty");
    anyhow::ensure!(limit > 0 && limit <= 50, "limit must be between 1 and 50");

    let mut lists = Vec::new();

    // Strategy 1: Phonetic entity search (ranked by match quality)
    if let Ok(entities) = storage.entity_find_phonetic(ctx, session_id, query).await
        && !entities.is_empty()
    {
        lists.push(
            entities
                .into_iter()
                .take(limit)
                .enumerate()
                .map(|(i, e)| SearchResult {
                    id: e.entity_id,
                    source: "entity_phonetic".into(),
                    content: e.context_snippet.clone(),
                    score: 1.0 - (i as f64 * 0.1), // rank decay
                    result_type: "entity".into(),
                })
                .collect(),
        );
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

    // Build weights for the initial 3 signals
    let mut weights = vec![
        config.phonetic_weight,
        config.ann_weight,
        config.fold_weight,
    ];

    // Strategy 4: Warmth signal — rank existing candidates by warmth score
    if let Some(warmth) = warmth_scores {
        let mut warmth_ranked: Vec<SearchResult> = lists
            .iter()
            .flatten()
            .filter_map(|r: &SearchResult| {
                warmth.get(&r.id).map(|score| SearchResult {
                    id: r.id,
                    source: "warmth".to_string(),
                    content: r.content.clone(),
                    score: *score,
                    result_type: r.result_type.clone(),
                })
            })
            .collect();
        warmth_ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        warmth_ranked.dedup_by_key(|r| r.id);
        lists.push(warmth_ranked);
        weights.push(config.warmth_weight);
    }

    // Strategy 5: PageRank signal — same approach
    if let Some(pagerank) = pagerank_scores {
        let mut pr_ranked: Vec<SearchResult> = lists
            .iter()
            .flatten()
            .filter_map(|r| {
                pagerank.get(&r.id).map(|score| SearchResult {
                    id: r.id,
                    source: "pagerank".to_string(),
                    content: r.content.clone(),
                    score: *score,
                    result_type: r.result_type.clone(),
                })
            })
            .collect();
        pr_ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        pr_ranked.dedup_by_key(|r| r.id);
        lists.push(pr_ranked);
        weights.push(config.pagerank_weight);
    }

    let merged = rrf_merge(lists, 60.0, &weights);
    Ok(merged.into_iter().take(limit).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(id: Uuid, source: &str, score: f64) -> SearchResult {
        SearchResult {
            id,
            source: source.into(),
            content: format!("content-{source}"),
            score,
            result_type: "entity".into(),
        }
    }

    #[test]
    fn rrf_merge_empty_lists() {
        let result = rrf_merge(vec![], 60.0, &[]);
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
        let merged = rrf_merge(vec![list], 60.0, &[1.0]);
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

        let merged = rrf_merge(vec![list_a, list_b], 60.0, &[1.0, 1.0]);
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

        let merged = rrf_merge(vec![list], 60.0, &[1.0]);
        // Should be in descending score order (rank 0 has highest RRF score)
        for i in 0..merged.len() - 1 {
            assert!(merged[i].score >= merged[i + 1].score);
        }
    }

    #[test]
    fn rrf_merge_with_weights_boosts_higher_weighted_list() {
        let uuid1 = Uuid::new_v4();
        let list1 = vec![make_result(uuid1, "a", 1.0)];
        let list2 = vec![make_result(uuid1, "b", 1.0)];

        // Equal weights: score = 1/61 + 1/61 = 2/61
        let merged_equal = rrf_merge(vec![list1.clone(), list2.clone()], 60.0, &[1.0, 1.0]);
        let score_equal = merged_equal[0].score;

        // Higher weight on list2: score = 1/61 + 2/61 = 3/61
        let merged_weighted = rrf_merge(vec![list1, list2], 60.0, &[1.0, 2.0]);
        let score_weighted = merged_weighted[0].score;

        assert!(score_weighted > score_equal);
        // Verify exact values
        assert!((score_equal - 2.0 / 61.0).abs() < 1e-10);
        assert!((score_weighted - 3.0 / 61.0).abs() < 1e-10);
    }

    #[test]
    fn rrf_merge_zero_weight_disables_signal() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        let list1 = vec![make_result(id1, "a", 1.0)];
        let list2 = vec![make_result(id2, "b", 1.0)];

        // Zero weight on list1 means only list2 contributes
        let merged = rrf_merge(vec![list1, list2], 60.0, &[0.0, 1.0]);
        assert_eq!(merged.len(), 2);

        // id1 should have score 0 (disabled), id2 should have 1/61
        let id1_result = merged.iter().find(|r| r.id == id1).unwrap();
        let id2_result = merged.iter().find(|r| r.id == id2).unwrap();
        assert!((id1_result.score - 0.0).abs() < 1e-10);
        assert!((id2_result.score - 1.0 / 61.0).abs() < 1e-10);

        // id2 should rank first
        assert_eq!(merged[0].id, id2);
    }

    #[test]
    fn rrf_merge_missing_weights_default_to_one() {
        let id1 = Uuid::new_v4();
        let list1 = vec![make_result(id1, "a", 1.0)];
        let list2 = vec![make_result(id1, "b", 1.0)];

        // Only provide weight for first list; second defaults to 1.0
        let merged = rrf_merge(vec![list1, list2], 60.0, &[1.0]);
        assert_eq!(merged.len(), 1);
        // score = 1.0/61 + 1.0/61 = 2/61
        assert!((merged[0].score - 2.0 / 61.0).abs() < 1e-10);
    }

    #[test]
    fn rrf_merge_five_signal_fusion() {
        let id1 = Uuid::new_v4();
        let lists = vec![
            vec![make_result(id1, "phonetic", 1.0)],
            vec![make_result(id1, "ann", 1.0)],
            vec![make_result(id1, "fold", 1.0)],
            vec![make_result(id1, "warmth", 1.0)],
            vec![make_result(id1, "pagerank", 1.0)],
        ];
        let weights = [1.0, 1.0, 1.0, 1.0, 1.0];

        let merged = rrf_merge(lists, 60.0, &weights);
        assert_eq!(merged.len(), 1);
        // All 5 signals at rank 0: score = 5 * (1/61)
        assert!((merged[0].score - 5.0 / 61.0).abs() < 1e-10);
    }

    #[test]
    fn fusion_config_default_all_ones() {
        let config = FusionConfig::default();
        assert!((config.phonetic_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.ann_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.fold_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.warmth_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.pagerank_weight - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn backward_compatible_three_signals_with_default_config() {
        // Verify that 3 lists with default weights [1.0, 1.0, 1.0]
        // produce identical results to the old unweighted behavior.
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        let list1 = vec![make_result(id1, "phonetic", 1.0)];
        let list2 = vec![make_result(id1, "ann", 1.0), make_result(id2, "ann", 0.9)];
        let list3 = vec![make_result(id2, "fold", 0.8)];

        let merged = rrf_merge(vec![list1, list2, list3], 60.0, &[1.0, 1.0, 1.0]);

        // id1: rank 0 in list1 + rank 0 in list2 = 1/61 + 1/61 = 2/61
        let id1_result = merged.iter().find(|r| r.id == id1).unwrap();
        assert!((id1_result.score - 2.0 / 61.0).abs() < 1e-10);

        // id2: rank 1 in list2 + rank 0 in list3 = 1/62 + 1/61
        let id2_result = merged.iter().find(|r| r.id == id2).unwrap();
        assert!((id2_result.score - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-10);
    }
}
