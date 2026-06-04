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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_chunk_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_chunk_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Configuration for 6-signal RRF fusion weights.
/// Default weight 1.0 for all signals. Set to 0.0 to disable a signal.
#[derive(Debug, Clone)]
pub struct FusionConfig {
    pub phonetic_weight: f64,
    pub ann_weight: f64,
    pub fold_weight: f64,
    pub context_bm25_weight: f64,
    pub context_ann_weight: f64,
    pub document_bm25_weight: f64,
    pub document_ann_weight: f64,
    pub document_phonetic_weight: f64,
    pub warmth_weight: f64,
    pub pagerank_weight: f64,
    /// Reputation signal weight. Moderate (0.5) to demote bad entities
    /// without a single negative event burying good information.
    pub reputation_weight: f64,
    /// Workspace affinity signal weight. Boosts entities learned in or near
    /// the caller's working directory without filtering out global knowledge.
    pub workspace_weight: f64,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            phonetic_weight: 1.0,
            ann_weight: 1.0,
            fold_weight: 1.0,
            context_bm25_weight: 1.5,
            context_ann_weight: 1.0,
            document_bm25_weight: 2.5,
            document_ann_weight: 1.5,
            document_phonetic_weight: 1.0,
            warmth_weight: 1.0,
            pagerank_weight: 1.0,
            reputation_weight: 0.5,
            workspace_weight: 2.0,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_cwd: Option<String>,
}

fn normalize_workspace_path(path: &str) -> String {
    let mut normalized = path.trim().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        return String::new();
    }
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    normalized
}

fn workspace_affinity_score(query_cwd: &str, candidate_cwd: &str) -> Option<f64> {
    let query = normalize_workspace_path(query_cwd);
    let candidate = normalize_workspace_path(candidate_cwd);
    if query.is_empty() || candidate.is_empty() {
        return None;
    }
    if query == candidate {
        return Some(3.0);
    }
    let query_child = query
        .strip_prefix(&candidate)
        .is_some_and(|rest| rest.starts_with('/'));
    let candidate_child = candidate
        .strip_prefix(&query)
        .is_some_and(|rest| rest.starts_with('/'));
    if query_child || candidate_child {
        Some(2.0)
    } else {
        None
    }
}

fn workspace_candidate_paths(properties: &serde_json::Value) -> Vec<&str> {
    [
        "cwd",
        "workspace",
        "working_directory",
        "repo",
        "repository",
    ]
    .iter()
    .filter_map(|key| properties.get(*key).and_then(|v| v.as_str()))
    .collect()
}

fn workspace_feedback_adjustment(query_cwd: &str, properties: &serde_json::Value) -> f64 {
    let Some(feedback) = properties
        .get("workspace_feedback")
        .and_then(|value| value.as_object())
    else {
        return 0.0;
    };
    let mut total = 0.0;
    for entry in feedback.values() {
        let Some(cwd) = entry.get("cwd").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(affinity) = workspace_affinity_score(query_cwd, cwd) else {
            continue;
        };
        let factor = if affinity >= 3.0 { 1.0 } else { 0.67 };
        if let Some(score) = entry.get("score").and_then(|value| value.as_f64()) {
            total += score * factor;
        }
        if let Some(mechanisms) = entry.get("mechanisms").and_then(|value| value.as_object()) {
            for mechanism in mechanisms.values() {
                if let Some(score) = mechanism.get("score").and_then(|value| value.as_f64()) {
                    total += score * factor;
                }
            }
        }
    }
    total.clamp(-1.0, 1.0)
}

/// Resolve the list of session partitions to query given the caller's session
/// and the filter scope.
fn sessions_to_query(caller_session: Uuid, tenant_id: Uuid, scope: SearchScope) -> Vec<Uuid> {
    let global = crate::scope::tenant_global_session_uuid(tenant_id);
    let nil = Uuid::nil();
    let mut sessions = match scope {
        SearchScope::SessionOnly => vec![caller_session],
        SearchScope::GlobalOnly => vec![global, nil],
        SearchScope::Both => vec![caller_session, global, nil],
    };
    sessions.sort_unstable();
    sessions.dedup();
    sessions
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
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: None,
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
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: None,
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
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: None,
                    })
                    .collect(),
            );
            weights.push(config.fold_weight);
        }

        // Strategy 4: raw context lexical/BM25 search over semantic segments.
        if let Ok(segments) = storage
            .context_segment_search_bm25(ctx, sid, query, limit)
            .await
            && !segments.is_empty()
        {
            lists.push(
                segments
                    .into_iter()
                    .enumerate()
                    .map(|(i, segment)| SearchResult {
                        id: segment.segment_id,
                        source: "context_bm25".into(),
                        content: segment.segment_text,
                        score: 1.0 - (i as f64 * 0.1),
                        result_type: "context_segment".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: Some("This is a raw context segment. Use ctx_window with this segment_id when adjacent turns may contain the rest of the answer.".into()),
                    })
                    .collect(),
            );
            weights.push(config.context_bm25_weight);
        }

        // Strategy 5: raw context ANN search over semantic segment embeddings.
        if let Some(emb) = embedding
            && let Ok(segments) = storage
                .context_segment_search_ann(ctx, sid, emb, limit)
                .await
            && !segments.is_empty()
        {
            lists.push(
                segments
                    .into_iter()
                    .map(|segment| SearchResult {
                        id: segment.segment_id,
                        source: "context_ann".into(),
                        content: segment.segment_text,
                        score: 1.0,
                        result_type: "context_segment".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: Some("This vector-matched context segment has temporal neighbors. Use ctx_window for bounded prev/next expansion.".into()),
                    })
                    .collect(),
            );
            weights.push(config.context_ann_weight);
        }

        // Strategy 6: document lexical/BM25 search over semantic chunks.
        if let Ok(chunks) = storage
            .document_chunk_search_bm25(ctx, sid, query, limit)
            .await
            && !chunks.is_empty()
        {
            lists.push(
                chunks
                    .into_iter()
                    .enumerate()
                    .map(|(i, chunk)| SearchResult {
                        id: chunk.chunk_id,
                        source: "document_bm25".into(),
                        content: chunk.content,
                        score: 1.0 - (i as f64 * 0.1),
                        result_type: "document_chunk".into(),
                        document_id: Some(chunk.document_id),
                        prev_chunk_id: chunk.prev_chunk_id,
                        next_chunk_id: chunk.next_chunk_id,
                        hint: Some("This is a semantic document chunk. If surrounding list items or adjacent context may matter, call chunk_ctx with prev/next expansion.".into()),
                    })
                    .collect(),
            );
            weights.push(config.document_bm25_weight);
        }

        // Strategy 7: document phonetic term search. This helps doc IDs,
        // titles, and spelling variants contribute candidates before RRF.
        if let Ok(chunks) = storage
            .document_chunk_search_phonetic(ctx, sid, query, limit)
            .await
            && !chunks.is_empty()
        {
            lists.push(
                chunks
                    .into_iter()
                    .enumerate()
                    .map(|(i, chunk)| SearchResult {
                        id: chunk.chunk_id,
                        source: "document_phonetic".into(),
                        content: chunk.content,
                        score: 1.0 - (i as f64 * 0.1),
                        result_type: "document_chunk".into(),
                        document_id: Some(chunk.document_id),
                        prev_chunk_id: chunk.prev_chunk_id,
                        next_chunk_id: chunk.next_chunk_id,
                        hint: Some("This document chunk has linked neighbors. Use chunk_ctx when the answer depends on adjacent context.".into()),
                    })
                    .collect(),
            );
            weights.push(config.document_phonetic_weight);
        }

        // Strategy 8: document ANN search over chunk embeddings.
        if let Some(emb) = embedding
            && let Ok(chunks) = storage
                .document_chunk_search_ann(ctx, sid, emb, limit)
                .await
            && !chunks.is_empty()
        {
            lists.push(
                chunks
                    .into_iter()
                    .map(|chunk| SearchResult {
                        id: chunk.chunk_id,
                        source: "document_ann".into(),
                        content: chunk.content,
                        score: 1.0,
                        result_type: "document_chunk".into(),
                        document_id: Some(chunk.document_id),
                        prev_chunk_id: chunk.prev_chunk_id,
                        next_chunk_id: chunk.next_chunk_id,
                        hint: Some("This is a vector-matched semantic document chunk. Use chunk_ctx for neighboring context.".into()),
                    })
                    .collect(),
            );
            weights.push(config.document_ann_weight);
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
                    document_id: r.document_id,
                    prev_chunk_id: r.prev_chunk_id,
                    next_chunk_id: r.next_chunk_id,
                    hint: r.hint.clone(),
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
                    document_id: r.document_id,
                    prev_chunk_id: r.prev_chunk_id,
                    next_chunk_id: r.next_chunk_id,
                    hint: r.hint.clone(),
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
                    document_id: r.document_id,
                    prev_chunk_id: r.prev_chunk_id,
                    next_chunk_id: r.next_chunk_id,
                    hint: r.hint.clone(),
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

    // Strategy 7: Workspace affinity — if the caller supplies cwd/repo context,
    // rank candidates learned in that same tree higher. This is intentionally
    // a boost rather than a filter because cross-repo facts can still be useful.
    if let Some(workspace_cwd) = filter.and_then(|f| f.workspace_cwd.as_deref())
        && !workspace_cwd.trim().is_empty()
    {
        let mut workspace_ranked = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for candidate in lists.iter().flatten().filter(|r| r.result_type == "entity") {
            if !seen.insert(candidate.id) {
                continue;
            }
            for sid in &sessions {
                if let Ok(Some(entity)) = storage.entity_get_by_id(ctx, *sid, candidate.id).await {
                    let score = workspace_candidate_paths(&entity.properties)
                        .into_iter()
                        .filter_map(|path| workspace_affinity_score(workspace_cwd, path))
                        .fold(None, |best: Option<f64>, score| {
                            Some(best.map_or(score, |b| b.max(score)))
                        });
                    if let Some(score) = score {
                        workspace_ranked.push(SearchResult {
                            id: candidate.id,
                            source: "workspace".to_string(),
                            content: candidate.content.clone(),
                            score,
                            result_type: candidate.result_type.clone(),
                            document_id: candidate.document_id,
                            prev_chunk_id: candidate.prev_chunk_id,
                            next_chunk_id: candidate.next_chunk_id,
                            hint: candidate.hint.clone(),
                        });
                    }
                    break;
                }
            }
        }
        workspace_ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if !workspace_ranked.is_empty() {
            lists.push(workspace_ranked);
            weights.push(config.workspace_weight);
        }
    }

    let mut merged = rrf_merge(lists, 60.0, &weights);
    if let Some(workspace_cwd) = filter.and_then(|f| f.workspace_cwd.as_deref())
        && !workspace_cwd.trim().is_empty()
    {
        for result in &mut merged {
            if result.result_type != "entity" {
                continue;
            }
            for sid in &sessions {
                if let Ok(Some(entity)) = storage.entity_get_by_id(ctx, *sid, result.id).await {
                    result.score +=
                        workspace_feedback_adjustment(workspace_cwd, &entity.properties) * 0.02;
                    break;
                }
            }
        }
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
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
            document_id: None,
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
        }
    }

    #[test]
    fn workspace_affinity_scores_exact_and_tree_matches() {
        assert_eq!(
            workspace_affinity_score("/repo/project", "/repo/project"),
            Some(3.0)
        );
        assert_eq!(
            workspace_affinity_score("/repo/project/crate", "/repo/project"),
            Some(2.0)
        );
        assert_eq!(
            workspace_affinity_score("/repo/project", "/repo/project/crate"),
            Some(2.0)
        );
        assert_eq!(workspace_affinity_score("/repo/a", "/repo/b"), None);
    }

    #[test]
    fn workspace_candidate_paths_reads_supported_property_names() {
        let properties = serde_json::json!({
            "cwd": "/repo/current",
            "workspace": "/repo/workspace",
            "working_directory": "/repo/wd",
            "repo": "/repo/root",
            "repository": "/repo/name",
            "other": "/ignored"
        });
        let paths = workspace_candidate_paths(&properties);
        assert_eq!(paths.len(), 5);
        assert!(paths.contains(&"/repo/current"));
        assert!(paths.contains(&"/repo/root"));
    }

    #[test]
    fn workspace_feedback_adjustment_demotes_current_workspace_only() {
        let properties = serde_json::json!({
            "workspace_feedback": {
                "local": {
                    "cwd": "/repo/project",
                    "score": -0.2,
                    "mechanisms": {
                        "hybrid_search": {"score": -0.4},
                        "phonetic": {"score": 0.1}
                    }
                },
                "other": {
                    "cwd": "/repo/other",
                    "score": -1.0
                }
            }
        });
        let local = workspace_feedback_adjustment("/repo/project", &properties);
        let child = workspace_feedback_adjustment("/repo/project/crate", &properties);
        let other = workspace_feedback_adjustment("/repo/unrelated", &properties);
        assert!(local < -0.49 && local > -0.51);
        assert!(child < -0.32 && child > -0.35);
        assert_eq!(other, 0.0);
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
        assert!(sessions.contains(&crate::scope::tenant_global_session_uuid(tenant)));
        assert!(sessions.contains(&Uuid::nil()));
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn sessions_to_query_both_returns_caller_sentinel_and_legacy_nil() {
        let caller = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let sessions = sessions_to_query(caller, tenant, SearchScope::Both);
        assert_eq!(sessions.len(), 3);
        assert!(sessions.contains(&caller));
        assert!(sessions.contains(&crate::scope::tenant_global_session_uuid(tenant)));
        assert!(sessions.contains(&Uuid::nil()));
    }

    #[test]
    fn sessions_to_query_both_dedups_when_caller_is_sentinel() {
        let tenant = Uuid::new_v4();
        let sentinel = crate::scope::tenant_global_session_uuid(tenant);
        let sessions = sessions_to_query(sentinel, tenant, SearchScope::Both);
        assert!(sessions.contains(&sentinel));
        assert!(sessions.contains(&Uuid::nil()));
        assert_eq!(sessions.len(), 2);
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
            workspace_cwd: Some("/repo/project".into()),
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.scope, SearchScope::Both);
        assert_eq!(back.workspace_cwd.as_deref(), Some("/repo/project"));
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
        let mut first = make_result(id1, "a", 1.0);
        first.content = "first".into();
        let mut second = make_result(id2, "a", 1.0);
        second.content = "second".into();
        let list = vec![first, second];
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

        let mut shared_a = make_result(shared_id, "a", 1.0);
        shared_a.content = "shared".into();
        let list_a = vec![shared_a];
        let mut unique_b = make_result(unique_id, "b", 1.0);
        unique_b.content = "unique".into();
        unique_b.result_type = "fold".into();
        let mut shared_b = make_result(shared_id, "b", 1.0);
        shared_b.content = "shared".into();
        let list_b = vec![unique_b, shared_b];

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
            .map(|(i, &id)| {
                let mut result = make_result(id, "test", 1.0);
                result.content = format!("item {i}");
                result
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
