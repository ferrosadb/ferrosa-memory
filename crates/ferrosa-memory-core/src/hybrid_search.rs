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

/// Configuration for 6-signal RRF fusion weights.
/// Default weight 1.0 for all signals. Set to 0.0 to disable a signal.
#[derive(Debug, Clone)]
pub struct FusionConfig {
    pub phonetic_weight: f64,
    pub ann_weight: f64,
    pub fold_weight: f64,
    pub warmth_weight: f64,
    pub pagerank_weight: f64,
    /// Reputation signal weight. Moderate (0.5) to demote bad entities
    /// without a single negative event burying good information.
    pub reputation_weight: f64,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            phonetic_weight: 1.0,
            ann_weight: 1.0,
            fold_weight: 1.0,
            warmth_weight: 1.0,
            pagerank_weight: 1.0,
            reputation_weight: 0.5,
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

/// Which partitions to query. Defaults to `SessionOnly` for backward compat
/// with existing callers that pass `None` for the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    #[default]
    SessionOnly,
    GlobalOnly,
    Both,
}

/// Optional filter applied to hybrid search.
///
/// - `scope`: which partitions to query (session, global, or both)
/// - `entity_types`: reserved for post-filter; to be applied by callers that
///   also enrich candidates with entity metadata (Sprint 2 work will add
///   automatic enrichment + filtering inside hybrid_search)
/// - `tags`: reserved for post-filter, same treatment
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchFilter {
    #[serde(default)]
    pub scope: SearchScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// Resolve the list of session partitions to query given the caller's session
/// and the filter scope.
fn sessions_to_query(
    caller_session: Uuid,
    tenant_id: Uuid,
    scope: SearchScope,
) -> Vec<Uuid> {
    match scope {
        SearchScope::SessionOnly => vec![caller_session],
        SearchScope::GlobalOnly => vec![crate::scope::tenant_global_session_uuid(tenant_id)],
        SearchScope::Both => {
            let global = crate::scope::tenant_global_session_uuid(tenant_id);
            if caller_session == global {
                vec![global]
            } else {
                vec![caller_session, global]
            }
        }
    }
}

/// Run a hybrid search combining up to 6 signals: phonetic entity lookup,
/// ANN entity search, ANN fold search, warmth scores, pagerank scores,
/// and reputation scores.
/// Results are fused via weighted Reciprocal Rank Fusion.
///
/// The optional `filter` argument controls partition scope (session vs
/// global vs both) and reserves fields for downstream entity-type / tag
/// filtering. When `None`, behavior matches the pre-filter API: query the
/// caller's session only.
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
    reputation_scores: Option<&HashMap<Uuid, f64>>,
    config: &FusionConfig,
    filter: Option<&SearchFilter>,
) -> anyhow::Result<Vec<SearchResult>> {
    anyhow::ensure!(!query.is_empty(), "query must not be empty");
    anyhow::ensure!(limit > 0 && limit <= 50, "limit must be between 1 and 50");

    let scope = filter.map(|f| f.scope).unwrap_or_default();
    let sessions = sessions_to_query(session_id, ctx.tenant_id, scope);

    let mut lists: Vec<Vec<SearchResult>> = Vec::new();
    let mut weights: Vec<f64> = Vec::new();

    for &sid in &sessions {
        // Strategy 1: Phonetic entity search (ranked by match quality)
        if let Ok(entities) = storage.entity_find_phonetic(ctx, sid, query).await
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
            weights.push(config.phonetic_weight);
        }

        // Strategy 2: ANN entity search
        if let Some(emb) = embedding
            && let Ok(entities) = storage.entity_search_ann(ctx, sid, emb, limit).await
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
            weights.push(config.ann_weight);
        }

        // Strategy 3: ANN fold search
        if let Some(emb) = embedding
            && let Ok(folds) = storage.fold_search(ctx, sid, emb, limit, false).await
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
            weights.push(config.fold_weight);
        }
    }

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

    // Strategy 6: Reputation signal — boost trusted entities, demote penalized ones.
    // Reputation ranges [-1.0, 1.0]. We shift to [0.0, 2.0] for RRF ranking so
    // that negative reputation maps to low rank and positive to high rank.
    if let Some(reputation) = reputation_scores {
        let mut rep_ranked: Vec<SearchResult> = lists
            .iter()
            .flatten()
            .filter_map(|r| {
                reputation.get(&r.id).map(|score| SearchResult {
                    id: r.id,
                    source: "reputation".to_string(),
                    content: r.content.clone(),
                    score: score + 1.0, // shift [-1,1] to [0,2] for ranking
                    result_type: r.result_type.clone(),
                })
            })
            .collect();
        rep_ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rep_ranked.dedup_by_key(|r| r.id);
        lists.push(rep_ranked);
        weights.push(config.reputation_weight);
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
    fn sessions_to_query_session_only_returns_caller() {
        let caller = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let sessions = sessions_to_query(caller, tenant, SearchScope::SessionOnly);
        assert_eq!(sessions, vec![caller]);
    }

    #[test]
    fn sessions_to_query_global_only_returns_sentinel() {
        let caller = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let sessions = sessions_to_query(caller, tenant, SearchScope::GlobalOnly);
        assert_eq!(
            sessions,
            vec![crate::scope::tenant_global_session_uuid(tenant)]
        );
    }

    #[test]
    fn sessions_to_query_both_returns_caller_and_sentinel() {
        let caller = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let sessions = sessions_to_query(caller, tenant, SearchScope::Both);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.contains(&caller));
        assert!(sessions.contains(&crate::scope::tenant_global_session_uuid(tenant)));
    }

    #[test]
    fn sessions_to_query_both_dedups_when_caller_is_sentinel() {
        // If a caller happens to pass the global sentinel as their session,
        // don't query the same partition twice.
        let tenant = Uuid::new_v4();
        let sentinel = crate::scope::tenant_global_session_uuid(tenant);
        let sessions = sessions_to_query(sentinel, tenant, SearchScope::Both);
        assert_eq!(sessions, vec![sentinel]);
    }

    #[test]
    fn search_scope_default_is_session_only() {
        assert_eq!(SearchScope::default(), SearchScope::SessionOnly);
    }

    #[test]
    fn search_filter_serde_round_trip() {
        let filter = SearchFilter {
            scope: SearchScope::Both,
            entity_types: Some(vec!["skill".into()]),
            tags: Some(vec!["testing".into(), "quality".into()]),
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.scope, SearchScope::Both);
        assert_eq!(back.entity_types, Some(vec!["skill".into()]));
        assert_eq!(back.tags, Some(vec!["testing".into(), "quality".into()]));
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
    fn fusion_config_default_weights() {
        let config = FusionConfig::default();
        assert!((config.phonetic_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.ann_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.fold_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.warmth_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.pagerank_weight - 1.0).abs() < f64::EPSILON);
        assert!((config.reputation_weight - 0.5).abs() < f64::EPSILON);
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

    #[test]
    fn rrf_merge_six_signal_fusion_with_reputation() {
        let id1 = Uuid::new_v4();
        let lists = vec![
            vec![make_result(id1, "phonetic", 1.0)],
            vec![make_result(id1, "ann", 1.0)],
            vec![make_result(id1, "fold", 1.0)],
            vec![make_result(id1, "warmth", 1.0)],
            vec![make_result(id1, "pagerank", 1.0)],
            vec![make_result(id1, "reputation", 1.0)],
        ];
        let weights = [1.0, 1.0, 1.0, 1.0, 1.0, 0.5];

        let merged = rrf_merge(lists, 60.0, &weights);
        assert_eq!(merged.len(), 1);
        // 5 signals at weight 1.0 + reputation at weight 0.5, all rank 0
        // score = 5 * (1/61) + 0.5 * (1/61) = 5.5/61
        assert!((merged[0].score - 5.5 / 61.0).abs() < 1e-10);
    }

    #[test]
    fn reputation_boosts_trusted_entity_in_ranking() {
        let good_id = Uuid::new_v4();
        let bad_id = Uuid::new_v4();

        // Both appear at rank 0 in phonetic (separate lists)
        let phonetic = vec![
            make_result(good_id, "phonetic", 1.0),
            make_result(bad_id, "phonetic", 0.9),
        ];

        // Reputation: good_id has positive (shifted: 1.5), bad_id has negative (shifted: 0.2)
        let mut rep_list = vec![
            make_result(good_id, "reputation", 1.5), // 0.5 + 1.0 shift
            make_result(bad_id, "reputation", 0.2),  // -0.8 + 1.0 shift
        ];
        rep_list.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());

        let merged = rrf_merge(vec![phonetic, rep_list], 60.0, &[1.0, 0.5]);

        // good_id: rank 0 in phonetic + rank 0 in reputation (it has higher score)
        // bad_id: rank 1 in phonetic + rank 1 in reputation
        // good_id should rank higher
        assert_eq!(merged[0].id, good_id);
        assert!(merged[0].score > merged[1].score);
    }

    #[test]
    fn negative_reputation_demotes_entity() {
        let trusted_id = Uuid::new_v4();
        let penalized_id = Uuid::new_v4();

        // Without reputation, both would tie at rank 0 in their own list
        let list1 = vec![make_result(trusted_id, "ann", 1.0)];
        let list2 = vec![make_result(penalized_id, "ann", 1.0)];

        // Reputation signal: trusted=1.0 (shifted: 2.0), penalized=-0.8 (shifted: 0.2)
        let rep = vec![
            make_result(trusted_id, "rep", 2.0),
            make_result(penalized_id, "rep", 0.2),
        ];

        // Without reputation: both get 1/61
        let merged_no_rep = rrf_merge(vec![list1.clone(), list2.clone()], 60.0, &[1.0, 1.0]);
        let no_rep_trusted = merged_no_rep
            .iter()
            .find(|r| r.id == trusted_id)
            .unwrap()
            .score;
        let no_rep_penalized = merged_no_rep
            .iter()
            .find(|r| r.id == penalized_id)
            .unwrap()
            .score;
        assert!(
            (no_rep_trusted - no_rep_penalized).abs() < 1e-10,
            "without reputation they should tie"
        );

        // With reputation: trusted gets boosted, penalized gets demoted
        let merged_with_rep = rrf_merge(vec![list1, list2, rep], 60.0, &[1.0, 1.0, 0.5]);
        assert_eq!(
            merged_with_rep[0].id, trusted_id,
            "trusted entity should rank first"
        );
        assert!(merged_with_rep[0].score > merged_with_rep[1].score);
    }

    #[tokio::test]
    async fn hybrid_search_uses_reputation_scores() {
        use crate::storage::mock::MockStorage;
        use crate::types::{EntityEntry, MemoryState, TenantContext};

        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let sid = Uuid::new_v4();

        let good_id = Uuid::new_v4();
        let bad_id = Uuid::new_v4();

        // Insert two entities with similar names
        for (id, name) in [(good_id, "Ferrosa DB"), (bad_id, "Ferrosa Cache")] {
            storage
                .entity_put(
                    &ctx,
                    &EntityEntry {
                        tenant_id: ctx.tenant_id,
                        entity_id: id,
                        session_id: sid,
                        entity_name: name.into(),
                        entity_type: "tool".into(),
                        source_fold_id: None,
                        context_snippet: format!("{name} is a database component"),
                        entity_embedding: None,
                        confidence: 1.0,
                        state: MemoryState::Active,
                        created_at: chrono::Utc::now(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }

        // Reputation: good entity is trusted, bad entity is penalized
        let mut reputation = HashMap::new();
        reputation.insert(good_id, 0.8);
        reputation.insert(bad_id, -0.5);

        let config = FusionConfig::default();
        let results = hybrid_search(
            &storage,
            &ctx,
            sid,
            "Ferrosa",
            None,
            10,
            None,
            None,
            Some(&reputation),
            &config,
            None,
        )
        .await
        .unwrap();

        assert!(!results.is_empty());

        // good_id should rank above bad_id due to reputation boost
        let good_pos = results.iter().position(|r| r.id == good_id);
        let bad_pos = results.iter().position(|r| r.id == bad_id);
        if let (Some(gp), Some(bp)) = (good_pos, bad_pos) {
            assert!(
                gp < bp,
                "trusted entity should rank higher than penalized entity"
            );
        }
    }
}
