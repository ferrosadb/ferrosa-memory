//! Hybrid search — multi-strategy retrieval with Reciprocal Rank Fusion.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::importance::{DerivedImportanceInput, compute_derived_importance};
use crate::storage::Storage;
use crate::types::TenantContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandedChunkContext {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub ordinal: i32,
    pub position: String,
    pub distance: usize,
    pub token_count: i32,
    pub section_path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: Uuid,
    pub source: String,
    pub memory_kind: String,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expanded_context: Vec<ExpandedChunkContext>,
}

pub fn classify_memory_kind(
    result_type: &str,
    source: &str,
    entity_type: Option<&str>,
) -> &'static str {
    match result_type {
        "context_segment" | "fold" => "episodic",
        "entity" => match entity_type.unwrap_or_default() {
            "procedure" | "policy_preference" | "decision" | "pattern" | "skill" => "procedural",
            "conversation" | "message" | "turn" => "episodic",
            _ => "semantic",
        },
        "document_chunk" if source.contains("procedure") => "procedural",
        "document_chunk" => "semantic",
        _ => "semantic",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateSourceStats {
    pub source: String,
    pub candidates: usize,
    pub unique_candidates: usize,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchDiagnostics {
    pub requested_limit: usize,
    pub source_limit: usize,
    pub total_candidates: usize,
    pub unique_candidates: usize,
    pub sources: Vec<CandidateSourceStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub diagnostics: SearchDiagnostics,
}

/// Configuration for 6-signal RRF fusion weights.
/// Default weight 1.0 for all signals. Set to 0.0 to disable a signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionConfig {
    pub phonetic_weight: f64,
    /// Weight for entity CONTENT-body lexical hits (native FTS on
    /// context_snippet) — the embeddings-free content recall path.
    pub entity_content_fts_weight: f64,
    pub ann_weight: f64,
    pub fold_weight: f64,
    pub context_bm25_weight: f64,
    pub context_ann_weight: f64,
    pub document_bm25_weight: f64,
    pub document_ann_weight: f64,
    pub document_phonetic_weight: f64,
    pub datalog_frontier_weight: f64,
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
            entity_content_fts_weight: 1.5,
            ann_weight: 1.0,
            fold_weight: 1.0,
            context_bm25_weight: 1.5,
            context_ann_weight: 1.0,
            document_bm25_weight: 2.5,
            document_ann_weight: 1.5,
            document_phonetic_weight: 1.0,
            datalog_frontier_weight: 4.0,
            warmth_weight: 1.0,
            pagerank_weight: 1.0,
            reputation_weight: 0.5,
            workspace_weight: 2.0,
        }
    }
}

impl FusionConfig {
    pub fn profile(name: &str) -> Option<Self> {
        let mut config = Self::default();
        match name {
            "default" | "all" => Some(config),
            "auto" => {
                config.zero_all();
                config.context_bm25_weight = 1.5;
                config.document_bm25_weight = 2.5;
                config.phonetic_weight = 1.0;
                config.ann_weight = 1.0;
                config.fold_weight = 1.0;
                config.context_ann_weight = 1.0;
                config.document_ann_weight = 1.5;
                Some(config)
            }
            "bm25-only" => {
                config.zero_all();
                config.context_bm25_weight = 1.5;
                config.document_bm25_weight = 2.5;
                Some(config)
            }
            "semantic-only" => {
                config.zero_all();
                config.ann_weight = 1.0;
                config.fold_weight = 1.0;
                config.context_ann_weight = 1.0;
                config.document_ann_weight = 1.5;
                Some(config)
            }
            // Session-memory intent: conversational/temporal recall of recent
            // work. Weight session context (segments + folds) and warmth up and
            // leave document_* at zero — the target is the current trajectory,
            // not the document corpus.
            "session-semantic" => {
                config.zero_all();
                config.context_bm25_weight = 2.0;
                config.context_ann_weight = 1.5;
                config.phonetic_weight = 1.0;
                config.ann_weight = 1.0;
                config.fold_weight = 1.5;
                config.warmth_weight = 2.0;
                config.workspace_weight = 1.0;
                Some(config)
            }
            // Corpus-reference intent: the query points at the document corpus
            // (papers, citations). Boost document sources; keep a little entity
            // context so a referenced concept's entity can still surface.
            "corpus-reference" => {
                config.zero_all();
                config.document_bm25_weight = 3.0;
                config.document_ann_weight = 2.0;
                config.context_bm25_weight = 1.0;
                config.context_ann_weight = 1.0;
                config.ann_weight = 1.0;
                config.fold_weight = 0.5;
                Some(config)
            }
            "bm25-semantic" => {
                config.zero_all();
                config.context_bm25_weight = 1.5;
                config.document_bm25_weight = 2.5;
                config.ann_weight = 1.0;
                config.fold_weight = 1.0;
                config.context_ann_weight = 1.0;
                config.document_ann_weight = 1.5;
                Some(config)
            }
            "bm25-semantic-workspace" => {
                config.zero_all();
                config.context_bm25_weight = 1.5;
                config.document_bm25_weight = 2.5;
                config.ann_weight = 1.0;
                config.fold_weight = 1.0;
                config.context_ann_weight = 1.0;
                config.document_ann_weight = 1.5;
                config.workspace_weight = 2.0;
                config.datalog_frontier_weight = 4.0;
                Some(config)
            }
            "bm25-semantic-phonetic" => {
                config.zero_all();
                config.context_bm25_weight = 1.5;
                config.document_bm25_weight = 2.5;
                config.ann_weight = 1.0;
                config.fold_weight = 1.0;
                config.context_ann_weight = 1.0;
                config.document_ann_weight = 1.5;
                config.phonetic_weight = 1.0;
                config.document_phonetic_weight = 1.0;
                config.datalog_frontier_weight = 4.0;
                Some(config)
            }
            "bm25-semantic-phonetic-workspace" => {
                config.zero_all();
                config.context_bm25_weight = 1.5;
                config.document_bm25_weight = 2.5;
                config.ann_weight = 1.0;
                config.fold_weight = 1.0;
                config.context_ann_weight = 1.0;
                config.document_ann_weight = 1.5;
                config.phonetic_weight = 1.0;
                config.document_phonetic_weight = 1.0;
                config.workspace_weight = 2.0;
                config.datalog_frontier_weight = 4.0;
                Some(config)
            }
            _ => None,
        }
    }

    fn zero_all(&mut self) {
        self.phonetic_weight = 0.0;
        self.ann_weight = 0.0;
        self.fold_weight = 0.0;
        self.context_bm25_weight = 0.0;
        self.context_ann_weight = 0.0;
        self.document_bm25_weight = 0.0;
        self.document_ann_weight = 0.0;
        self.document_phonetic_weight = 0.0;
        self.datalog_frontier_weight = 0.0;
        self.warmth_weight = 0.0;
        self.pagerank_weight = 0.0;
        self.reputation_weight = 0.0;
        self.workspace_weight = 0.0;
    }

    pub fn set_weight(&mut self, key: &str, weight: f64) -> bool {
        match key {
            "phonetic" | "entity_phonetic" | "phonetic_weight" => {
                self.phonetic_weight = weight;
                true
            }
            "ann" | "entity_ann" | "ann_weight" => {
                self.ann_weight = weight;
                true
            }
            "fold" | "fold_ann" | "fold_weight" => {
                self.fold_weight = weight;
                true
            }
            "context_bm25" | "context_bm25_weight" => {
                self.context_bm25_weight = weight;
                true
            }
            "context_ann" | "context_ann_weight" => {
                self.context_ann_weight = weight;
                true
            }
            "document_bm25" | "document_bm25_weight" => {
                self.document_bm25_weight = weight;
                true
            }
            "document_ann" | "document_ann_weight" => {
                self.document_ann_weight = weight;
                true
            }
            "document_phonetic" | "document_phonetic_weight" => {
                self.document_phonetic_weight = weight;
                true
            }
            "datalog_frontier" | "datalog" | "datalog_frontier_weight" => {
                self.datalog_frontier_weight = weight;
                true
            }
            "warmth" | "warmth_weight" => {
                self.warmth_weight = weight;
                true
            }
            "pagerank" | "graph" | "pagerank_weight" => {
                self.pagerank_weight = weight;
                true
            }
            "reputation" | "reputation_weight" => {
                self.reputation_weight = weight;
                true
            }
            "workspace" | "workspace_weight" => {
                self.workspace_weight = weight;
                true
            }
            _ => false,
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
        if weight <= 0.0 {
            continue;
        }
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

fn prune_disabled_source_lists(
    lists: Vec<Vec<SearchResult>>,
    weights: Vec<f64>,
) -> (Vec<Vec<SearchResult>>, Vec<f64>) {
    lists
        .into_iter()
        .zip(weights)
        .filter(|(_, weight)| *weight > 0.0)
        .unzip()
}

fn metadata_stub(content: &str) -> bool {
    content.trim_start().starts_with("BOOK METADATA |")
}

fn bibliographic_query(query_terms: &HashSet<String>) -> bool {
    query_terms.iter().any(|term| {
        matches!(
            term.as_str(),
            "book"
                | "books"
                | "author"
                | "authors"
                | "metadata"
                | "publisher"
                | "published"
                | "edition"
                | "title"
                | "titles"
                | "corpus"
        )
    })
}

fn document_representative_better(candidate: &SearchResult, incumbent: &SearchResult) -> bool {
    let candidate_metadata = metadata_stub(&candidate.content);
    let incumbent_metadata = metadata_stub(&incumbent.content);
    if candidate_metadata != incumbent_metadata {
        return !candidate_metadata;
    }
    candidate.score > incumbent.score
}

fn collapse_duplicate_document_chunks(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut document_slots: HashMap<Uuid, usize> = HashMap::new();
    let mut collapsed = Vec::with_capacity(results.len());
    for result in results {
        if result.result_type == "document_chunk"
            && let Some(document_id) = result.document_id
        {
            if let Some(slot) = document_slots.get(&document_id).copied() {
                if document_representative_better(&result, &collapsed[slot]) {
                    collapsed[slot] = result;
                }
                continue;
            }
            document_slots.insert(document_id, collapsed.len());
        }
        collapsed.push(result);
    }
    collapsed.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    collapsed
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_kinds: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datalog_frontier: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datalog_frontier_seed_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datalog_frontier_edge_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datalog_frontier_max_hops: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datalog_frontier_min_confidence: Option<f64>,
}

fn source_limit(limit: usize, filter: Option<&SearchFilter>) -> usize {
    filter
        .and_then(|f| f.candidate_limit)
        .unwrap_or_else(|| limit.saturating_mul(2))
        .clamp(limit, 50)
}

fn candidate_source_stats(
    lists: &[Vec<SearchResult>],
    weights: &[f64],
    requested_limit: usize,
    source_limit: usize,
) -> SearchDiagnostics {
    let mut sources = Vec::new();
    let mut all_unique = HashSet::new();
    let mut total_candidates = 0usize;
    for (idx, list) in lists.iter().enumerate() {
        let source = list
            .first()
            .map(|result| result.source.clone())
            .unwrap_or_else(|| format!("source_{idx}"));
        let unique_candidates = list
            .iter()
            .map(|result| result.id)
            .collect::<HashSet<_>>()
            .len();
        total_candidates += list.len();
        all_unique.extend(list.iter().map(|result| result.id));
        sources.push(CandidateSourceStats {
            source,
            candidates: list.len(),
            unique_candidates,
            weight: weights.get(idx).copied().unwrap_or(1.0),
        });
    }
    SearchDiagnostics {
        requested_limit,
        source_limit,
        total_candidates,
        unique_candidates: all_unique.len(),
        sources,
    }
}

fn relevance_stopword(token: &str) -> bool {
    matches!(
        token,
        "about"
            | "after"
            | "again"
            | "also"
            | "and"
            | "are"
            | "because"
            | "been"
            | "before"
            | "but"
            | "can"
            | "did"
            | "does"
            | "for"
            | "from"
            | "get"
            | "had"
            | "has"
            | "have"
            | "how"
            | "into"
            | "its"
            | "just"
            | "like"
            | "now"
            | "our"
            | "out"
            | "should"
            | "that"
            | "the"
            | "then"
            | "there"
            | "these"
            | "this"
            | "those"
            | "through"
            | "was"
            | "were"
            | "what"
            | "when"
            | "where"
            | "with"
            | "would"
            | "you"
            | "your"
    )
}

fn relevance_terms(text: &str) -> HashSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .filter_map(|part| {
            let token = part.trim().to_ascii_lowercase();
            (token.len() >= 3 && !relevance_stopword(&token)).then_some(token)
        })
        .collect()
}

fn source_is_lexical(source: &str) -> bool {
    source.contains("bm25") || source.contains("phonetic")
}

fn source_is_ann(source: &str) -> bool {
    source.contains("ann")
}

fn source_is_datalog_frontier(source: &str) -> bool {
    source.starts_with("datalog_frontier:")
}

fn source_family_prior(result: &SearchResult) -> f64 {
    match result.source.as_str() {
        source if source_is_datalog_frontier(source) => 0.08,
        "context_bm25" => 0.07,
        "context_ann" => 0.04,
        "entity_phonetic" => 0.04,
        "entity_ann" => 0.03,
        "fold_ann" => 0.03,
        "document_bm25" => 0.0,
        "document_ann" => -0.01,
        "document_phonetic" => -0.07,
        _ => 0.0,
    }
}

#[derive(Debug, Clone, Default)]
struct CandidateEvidence {
    sources: HashSet<String>,
    lexical_sources: usize,
    ann_sources: usize,
    datalog_frontier_sources: usize,
    lexical_hits: usize,
    lexical_coverage: f64,
}

impl CandidateEvidence {
    fn source_count(&self) -> usize {
        self.sources.len()
    }

    fn ann_only_without_terms(&self) -> bool {
        self.ann_sources > 0
            && self.lexical_sources == 0
            && self.datalog_frontier_sources == 0
            && self.lexical_hits == 0
    }
}

fn collect_candidate_evidence(
    query: &str,
    lists: &[Vec<SearchResult>],
) -> HashMap<Uuid, CandidateEvidence> {
    let query_terms = relevance_terms(query);
    let query_term_count = query_terms.len().max(1);
    let mut evidence: HashMap<Uuid, CandidateEvidence> = HashMap::new();
    for result in lists.iter().flatten() {
        let result_terms = relevance_terms(&result.content);
        let lexical_hits = query_terms.intersection(&result_terms).count();
        let entry = evidence.entry(result.id).or_default();
        entry.sources.insert(result.source.clone());
        if source_is_lexical(&result.source) {
            entry.lexical_sources += 1;
        }
        if source_is_ann(&result.source) {
            entry.ann_sources += 1;
        }
        if source_is_datalog_frontier(&result.source) {
            entry.datalog_frontier_sources += 1;
        }
        entry.lexical_hits = entry.lexical_hits.max(lexical_hits);
        entry.lexical_coverage = entry
            .lexical_coverage
            .max(lexical_hits as f64 / query_term_count as f64);
    }
    evidence
}

fn apply_source_aware_scoring(
    query: &str,
    results: Vec<SearchResult>,
    evidence: &HashMap<Uuid, CandidateEvidence>,
) -> Vec<SearchResult> {
    let query_terms = relevance_terms(query);
    if query_terms.is_empty() {
        return results;
    }
    let bibliographic = bibliographic_query(&query_terms);
    let mut scored = Vec::with_capacity(results.len());
    for mut result in results {
        let Some(ev) = evidence.get(&result.id) else {
            scored.push(result);
            continue;
        };
        if ev.ann_only_without_terms() {
            continue;
        }
        let source_bonus = (ev.source_count().saturating_sub(1) as f64 * 0.035).min(0.14);
        let lexical_bonus = ev.lexical_coverage * 0.30;
        let lexical_source_bonus = if ev.lexical_sources > 0 { 0.04 } else { 0.0 };
        let ann_only_penalty =
            if ev.ann_sources > 0 && ev.lexical_sources == 0 && ev.datalog_frontier_sources == 0 {
                0.04
            } else {
                0.0
            };
        let metadata_penalty = if result.result_type == "document_chunk"
            && metadata_stub(&result.content)
            && !bibliographic
        {
            0.18
        } else {
            0.0
        };
        result.score = (result.score
            + source_bonus
            + lexical_bonus
            + lexical_source_bonus
            + source_family_prior(&result)
            - ann_only_penalty
            - metadata_penalty)
            .max(0.0);
        scored.push(result);
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored
}

fn apply_authority_adjustments(
    results: Vec<SearchResult>,
    pagerank_scores: Option<&HashMap<Uuid, f64>>,
    reputation_scores: Option<&HashMap<Uuid, f64>>,
) -> Vec<SearchResult> {
    if pagerank_scores.is_none() && reputation_scores.is_none() {
        return results;
    }

    let mut adjusted = Vec::with_capacity(results.len());
    for mut result in results {
        if let Some(score) = pagerank_scores.and_then(|scores| scores.get(&result.id)) {
            result.score += score.clamp(0.0, 1.0) * 0.20;
        }
        if let Some(score) = reputation_scores.and_then(|scores| scores.get(&result.id)) {
            let score = score.clamp(-1.0, 1.0);
            if score <= -1.0 {
                continue;
            }
            result.score = (result.score + score * 0.45).max(0.0);
        }
        adjusted.push(result);
    }
    adjusted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    adjusted
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DerivedSearchSignals {
    derived_confidence: f64,
    support_count: usize,
    path_distance: usize,
    predicate_weight: f64,
    scope_guard: bool,
}

impl DerivedSearchSignals {
    fn importance(self) -> f64 {
        compute_derived_importance(DerivedImportanceInput {
            derived_confidence: self.derived_confidence,
            support_count: self.support_count,
            path_distance: self.path_distance,
            predicate_weight: self.predicate_weight,
            scope_guard: self.scope_guard,
        })
    }
}

fn predicate_weight(predicate: &str) -> f64 {
    match predicate {
        "remembered" | "authoritative" | "curated" | "implements" | "depends_on" => 1.0,
        "uses" | "references" | "part_of" | "contains" | "current" | "task_relevant" => 0.85,
        "related_to" | "bridge_memory" | "reachable" => 0.65,
        "co_occurs" | "co_occurs_with" | "CO_OCCURS" | "CO_OCCURS_WITH" => 0.25,
        "supersedes" | "contradicts" | "stale" => 0.0,
        _ => 0.55,
    }
}

fn derived_metadata_usize(metadata: Option<&str>, key: &str) -> Option<usize> {
    let value = serde_json::from_str::<serde_json::Value>(metadata?).ok()?;
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as usize)
}

fn derived_metadata_f64(metadata: Option<&str>, key: &str) -> Option<f64> {
    let value = serde_json::from_str::<serde_json::Value>(metadata?).ok()?;
    value.get(key).and_then(serde_json::Value::as_f64)
}

fn should_suppress_derived_predicate(predicate: &str) -> bool {
    matches!(
        predicate,
        "supersedes" | "contradicts" | "stale" | "wrong_workspace" | "negative_reputation"
    )
}

fn apply_derived_importance_adjustments(
    results: Vec<SearchResult>,
    derived_signals: &HashMap<Uuid, DerivedSearchSignals>,
) -> Vec<SearchResult> {
    if derived_signals.is_empty() {
        return results;
    }
    let mut adjusted = Vec::with_capacity(results.len());
    for mut result in results {
        if let Some(signals) = derived_signals.get(&result.id) {
            result.score += signals.importance() * 0.08;
        }
        adjusted.push(result);
    }
    adjusted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    adjusted
}

fn filter_relevant_results(
    results: Vec<SearchResult>,
    min_score: Option<f64>,
    memory_kinds: Option<&[String]>,
) -> Vec<SearchResult> {
    let allowed_kinds = memory_kinds.map(|kinds| {
        kinds
            .iter()
            .map(|kind| kind.to_ascii_lowercase())
            .collect::<HashSet<_>>()
    });
    results
        .into_iter()
        .filter(|result| min_score.is_none_or(|threshold| result.score >= threshold))
        .filter(|result| {
            allowed_kinds
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&result.memory_kind.to_ascii_lowercase()))
        })
        .collect()
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

#[allow(clippy::too_many_arguments)]
async fn datalog_frontier_candidates<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    sessions: &[Uuid],
    seed_candidates: &[SearchResult],
    source_limit: usize,
    filter: Option<&SearchFilter>,
) -> anyhow::Result<(Vec<SearchResult>, HashMap<Uuid, DerivedSearchSignals>)> {
    if filter.and_then(|f| f.datalog_frontier) == Some(false) {
        return Ok((Vec::new(), HashMap::new()));
    }

    let seed_limit = filter
        .and_then(|f| f.datalog_frontier_seed_limit)
        .unwrap_or(source_limit)
        .clamp(1, 50);
    let edge_limit = filter
        .and_then(|f| f.datalog_frontier_edge_limit)
        .unwrap_or(12)
        .clamp(1, 50);
    let max_hops = filter
        .and_then(|f| f.datalog_frontier_max_hops)
        .unwrap_or(2)
        .clamp(1, 3);
    let min_confidence = filter
        .and_then(|f| f.datalog_frontier_min_confidence)
        .unwrap_or(0.30)
        .clamp(0.0, 1.0);

    let seeds = seed_candidates
        .iter()
        .filter(|candidate| candidate.result_type == "entity")
        .map(|candidate| candidate.id)
        .take(seed_limit)
        .collect::<Vec<_>>();
    if seeds.is_empty() {
        return Ok((Vec::new(), HashMap::new()));
    }

    let mut best: HashMap<Uuid, (SearchResult, DerivedSearchSignals)> = HashMap::new();
    for &sid in sessions {
        let mut frontier = seeds
            .iter()
            .copied()
            .map(|id| (id, 0usize))
            .collect::<Vec<_>>();
        let mut visited = seeds.iter().copied().collect::<HashSet<_>>();
        let mut offset = 0usize;
        while offset < frontier.len() {
            let (current, depth) = frontier[offset];
            offset += 1;
            if depth >= max_hops {
                continue;
            }

            let mut edges = storage.typed_edge_list_from(ctx, sid, current).await?;
            edges.extend(storage.typed_edge_list_to(ctx, sid, current).await?);
            edges.sort_by(|left, right| {
                right
                    .weight
                    .partial_cmp(&left.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            edges.truncate(edge_limit);

            for edge in edges {
                if should_suppress_derived_predicate(&edge.edge_type) {
                    continue;
                }
                let neighbor = if edge.src_id == current {
                    edge.dst_id
                } else {
                    edge.src_id
                };
                if neighbor == current {
                    continue;
                }
                let path_distance = depth + 1;
                let derived_confidence =
                    derived_metadata_f64(edge.metadata.as_deref(), "confidence")
                        .unwrap_or(edge.weight)
                        .clamp(0.0, 1.0);
                if derived_confidence < min_confidence {
                    continue;
                }
                let support_count =
                    derived_metadata_usize(edge.metadata.as_deref(), "support_count").unwrap_or(1);
                let signals = DerivedSearchSignals {
                    derived_confidence,
                    support_count,
                    path_distance,
                    predicate_weight: derived_metadata_f64(
                        edge.metadata.as_deref(),
                        "predicate_weight",
                    )
                    .unwrap_or_else(|| predicate_weight(&edge.edge_type)),
                    scope_guard: edge.session_id == sid,
                };
                if signals.importance() <= 0.0 {
                    continue;
                }

                let Some(entity) = storage.entity_get_by_id(ctx, sid, neighbor).await? else {
                    continue;
                };
                if !entity.state.is_retrievable() {
                    continue;
                }
                if !visited.contains(&neighbor) && path_distance < max_hops {
                    visited.insert(neighbor);
                    frontier.push((neighbor, path_distance));
                }

                let content = if entity.context_snippet.trim().is_empty() {
                    entity.entity_name.clone()
                } else {
                    entity.context_snippet.clone()
                };
                let candidate = SearchResult {
                    id: entity.entity_id,
                    source: format!("datalog_frontier:{}", edge.edge_type),
                    memory_kind: classify_memory_kind(
                        "entity",
                        "datalog_frontier",
                        Some(&entity.entity_type),
                    )
                    .into(),
                    content,
                    score: signals.importance(),
                    result_type: "entity".into(),
                    document_id: None,
                    prev_chunk_id: None,
                    next_chunk_id: None,
                    hint: Some(format!(
                        "Derived via {} at {} hop(s), confidence {:.2}, support {}.",
                        edge.edge_type, path_distance, derived_confidence, support_count
                    )),
                    expanded_context: Vec::new(),
                };

                best.entry(entity.entity_id)
                    .and_modify(|(existing, existing_signals)| {
                        let combined = DerivedSearchSignals {
                            derived_confidence: existing_signals
                                .derived_confidence
                                .max(signals.derived_confidence),
                            support_count: existing_signals
                                .support_count
                                .saturating_add(signals.support_count),
                            path_distance: existing_signals
                                .path_distance
                                .min(signals.path_distance),
                            predicate_weight: existing_signals
                                .predicate_weight
                                .max(signals.predicate_weight),
                            scope_guard: existing_signals.scope_guard && signals.scope_guard,
                        };
                        if combined.importance() > existing_signals.importance() {
                            *existing = candidate.clone();
                        }
                        *existing_signals = combined;
                    })
                    .or_insert((candidate, signals));
            }
        }
    }

    let mut candidates = best
        .values()
        .map(|(candidate, signals)| {
            let mut candidate = candidate.clone();
            candidate.score = signals.importance();
            candidate
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(source_limit);
    let signals = best
        .into_iter()
        .map(|(id, (_candidate, signals))| (id, signals))
        .collect();
    Ok((candidates, signals))
}

fn context_segment_workspace(segment: &crate::context_segment::ContextSegment) -> Option<&str> {
    // Hook-ingested context uses "<harness>:<session_id>:<cwd>". Preserve
    // compatibility with existing rows by deriving workspace from that stable
    // conversation id until context segments carry structured metadata.
    segment
        .conversation_id
        .rsplit_once(':')
        .map(|(_, workspace)| workspace)
        .filter(|workspace| !workspace.trim().is_empty())
}

fn context_segment_allowed_for_workspace(
    caller_session: Uuid,
    queried_session: Uuid,
    filter: Option<&SearchFilter>,
    segment: &crate::context_segment::ContextSegment,
) -> bool {
    if segment.session_id == caller_session || segment.source_session == caller_session {
        return true;
    }
    if queried_session == caller_session {
        return true;
    }
    let Some(workspace_cwd) = filter.and_then(|f| f.workspace_cwd.as_deref()) else {
        return true;
    };
    if workspace_cwd.trim().is_empty() {
        return true;
    }
    context_segment_workspace(segment)
        .and_then(|workspace| workspace_affinity_score(workspace_cwd, workspace))
        .is_some()
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
pub(crate) fn sessions_to_query(
    caller_session: Uuid,
    tenant_id: Uuid,
    scope: SearchScope,
) -> Vec<Uuid> {
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
    hybrid_search_with_diagnostics(
        storage,
        ctx,
        session_id,
        query,
        embedding,
        limit,
        warmth_scores,
        pagerank_scores,
        reputation_scores,
        config,
        filter,
    )
    .await
    .map(|output| output.results)
}

#[allow(clippy::too_many_arguments)]
pub async fn hybrid_search_with_diagnostics(
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
) -> anyhow::Result<SearchOutput> {
    anyhow::ensure!(!query.is_empty(), "query must not be empty");
    anyhow::ensure!(limit > 0 && limit <= 50, "limit must be between 1 and 50");

    let scope = filter.map(|f| f.scope).unwrap_or_default();
    let sessions = sessions_to_query(session_id, ctx.tenant_id, scope);
    let source_limit = source_limit(limit, filter);

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
                    .take(source_limit)
                    .enumerate()
                    .map(|(i, e)| SearchResult {
                        id: e.entity_id,
                        source: "entity_phonetic".into(),
                        memory_kind: classify_memory_kind(
                            "entity",
                            "entity_phonetic",
                            Some(&e.entity_type),
                        )
                        .into(),
                        content: if e.context_snippet.trim().is_empty() {
                            e.entity_name.clone()
                        } else {
                            e.context_snippet.clone()
                        },
                        score: 1.0 - (i as f64 * 0.1), // rank decay
                        result_type: "entity".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: None,
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.phonetic_weight);
        }

        // Strategy 1b: Entity CONTENT-body lexical search via native FTS.
        // Embeddings-free: makes a content-term `search` return a plain-ingested
        // entity even with no ANN — the gap that entity_text_scan_bounded only
        // patched as a labeled last-resort scan. Needs idx_entity_context_snippet_fts
        // (ddl/043); no-op against clusters/older schema where the index is absent.
        if let Ok(entities) = storage
            .entity_find_content_fts(ctx, sid, query, source_limit)
            .await
            && !entities.is_empty()
        {
            lists.push(
                entities
                    .into_iter()
                    .take(source_limit)
                    .enumerate()
                    .map(|(i, e)| SearchResult {
                        id: e.entity_id,
                        source: "entity_content_fts".into(),
                        memory_kind: classify_memory_kind(
                            "entity",
                            "entity_content_fts",
                            Some(&e.entity_type),
                        )
                        .into(),
                        content: if e.context_snippet.trim().is_empty() {
                            e.entity_name.clone()
                        } else {
                            e.context_snippet.clone()
                        },
                        score: 1.0 - (i as f64 * 0.1), // rank decay
                        result_type: "entity".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: None,
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.entity_content_fts_weight);
        }

        // Strategy 2: ANN entity search
        if let Some(emb) = embedding
            && let Ok(entities) = storage.entity_search_ann(ctx, sid, emb, source_limit).await
            && !entities.is_empty()
        {
            lists.push(
                entities
                    .into_iter()
                    .map(|e| SearchResult {
                        id: e.entity_id,
                        source: "entity_ann".into(),
                        memory_kind: classify_memory_kind(
                            "entity",
                            "entity_ann",
                            Some(&e.entity_type),
                        )
                        .into(),
                        content: e.context_snippet,
                        score: 1.0,
                        result_type: "entity".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: None,
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.ann_weight);
        }

        // Strategy 3: ANN fold search
        if let Some(emb) = embedding
            && let Ok(folds) = storage
                .fold_search(ctx, sid, emb, source_limit, false)
                .await
            && !folds.is_empty()
        {
            lists.push(
                folds
                    .into_iter()
                    .map(|f| SearchResult {
                        id: f.fold_id,
                        source: "fold_ann".into(),
                        memory_kind: classify_memory_kind("fold", "fold_ann", None).into(),
                        content: f.fold_summary,
                        score: f.similarity.unwrap_or(0.0),
                        result_type: "fold".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: None,
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.fold_weight);
        }

        // Strategy 4: raw context lexical/BM25 search over semantic segments.
        if let Ok(segments) = storage
            .context_segment_search_bm25(ctx, sid, query, source_limit)
            .await
            && !segments.is_empty()
        {
            lists.push(
                segments
                    .into_iter()
                    .filter(|segment| {
                        context_segment_allowed_for_workspace(session_id, sid, filter, segment)
                    })
                    .enumerate()
                    .map(|(i, segment)| SearchResult {
                        id: segment.segment_id,
                        source: "context_bm25".into(),
                        memory_kind: classify_memory_kind("context_segment", "context_bm25", None).into(),
                        content: segment.segment_text,
                        score: 1.0 - (i as f64 * 0.1),
                        result_type: "context_segment".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: Some("This is a raw context segment. Use ctx_window with this segment_id when adjacent turns may contain the rest of the answer.".into()),
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.context_bm25_weight);
        }

        // Strategy 5: raw context ANN search over semantic segment embeddings.
        if let Some(emb) = embedding
            && let Ok(segments) = storage
                .context_segment_search_ann(ctx, sid, emb, source_limit)
                .await
            && !segments.is_empty()
        {
            lists.push(
                segments
                    .into_iter()
                    .filter(|segment| {
                        context_segment_allowed_for_workspace(session_id, sid, filter, segment)
                    })
                    .map(|segment| SearchResult {
                        id: segment.segment_id,
                        source: "context_ann".into(),
                        memory_kind: classify_memory_kind("context_segment", "context_ann", None).into(),
                        content: segment.segment_text,
                        score: 1.0,
                        result_type: "context_segment".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: Some("This vector-matched context segment has temporal neighbors. Use ctx_window for bounded prev/next expansion.".into()),
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.context_ann_weight);
        }

        // Strategy 6: document lexical/BM25 search over semantic chunks.
        if let Ok(chunks) = storage
            .document_chunk_search_bm25(ctx, sid, query, source_limit)
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
                        memory_kind: classify_memory_kind("document_chunk", "document_bm25", None).into(),
                        content: chunk.content,
                        score: 1.0 - (i as f64 * 0.1),
                        result_type: "document_chunk".into(),
                        document_id: Some(chunk.document_id),
                        prev_chunk_id: chunk.prev_chunk_id,
                        next_chunk_id: chunk.next_chunk_id,
                        hint: Some("This is a semantic document chunk. If surrounding list items or adjacent context may matter, call chunk_ctx with prev/next expansion.".into()),
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.document_bm25_weight);
        }

        // Strategy 7: document phonetic term search. This helps doc IDs,
        // titles, and spelling variants contribute candidates before RRF.
        if let Ok(chunks) = storage
            .document_chunk_search_phonetic(ctx, sid, query, source_limit)
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
                        memory_kind: classify_memory_kind("document_chunk", "document_phonetic", None)
                            .into(),
                        content: chunk.content,
                        score: 1.0 - (i as f64 * 0.1),
                        result_type: "document_chunk".into(),
                        document_id: Some(chunk.document_id),
                        prev_chunk_id: chunk.prev_chunk_id,
                        next_chunk_id: chunk.next_chunk_id,
                        hint: Some("This document chunk has linked neighbors. Use chunk_ctx when the answer depends on adjacent context.".into()),
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.document_phonetic_weight);
        }

        // Strategy 8: document ANN search over chunk embeddings.
        if let Some(emb) = embedding
            && let Ok(chunks) = storage
                .document_chunk_search_ann(ctx, sid, emb, source_limit)
                .await
            && !chunks.is_empty()
        {
            lists.push(
                chunks
                    .into_iter()
                    .map(|chunk| SearchResult {
                        id: chunk.chunk_id,
                        source: "document_ann".into(),
                        memory_kind: classify_memory_kind("document_chunk", "document_ann", None).into(),
                        content: chunk.content,
                        score: 1.0,
                        result_type: "document_chunk".into(),
                        document_id: Some(chunk.document_id),
                        prev_chunk_id: chunk.prev_chunk_id,
                        next_chunk_id: chunk.next_chunk_id,
                        hint: Some("This is a vector-matched semantic document chunk. Use chunk_ctx for neighboring context.".into()),
                        expanded_context: Vec::new(),
                    })
                    .collect(),
            );
            weights.push(config.document_ann_weight);
        }
    }

    let mut derived_signals = HashMap::new();
    if config.datalog_frontier_weight > 0.0 {
        let mut seen_seed = HashSet::new();
        let seed_candidates = lists
            .iter()
            .flatten()
            .filter(|candidate| candidate.result_type == "entity")
            .filter(|candidate| seen_seed.insert(candidate.id))
            .cloned()
            .collect::<Vec<_>>();
        let (frontier, signals) = datalog_frontier_candidates(
            storage,
            ctx,
            &sessions,
            &seed_candidates,
            source_limit,
            filter,
        )
        .await?;
        if !frontier.is_empty() {
            lists.push(frontier);
            weights.push(config.datalog_frontier_weight);
            derived_signals = signals;
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
                    memory_kind: r.memory_kind.clone(),
                    content: r.content.clone(),
                    score: *score,
                    result_type: r.result_type.clone(),
                    document_id: r.document_id,
                    prev_chunk_id: r.prev_chunk_id,
                    next_chunk_id: r.next_chunk_id,
                    hint: r.hint.clone(),
                    expanded_context: r.expanded_context.clone(),
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
                    memory_kind: r.memory_kind.clone(),
                    content: r.content.clone(),
                    score: *score,
                    result_type: r.result_type.clone(),
                    document_id: r.document_id,
                    prev_chunk_id: r.prev_chunk_id,
                    next_chunk_id: r.next_chunk_id,
                    hint: r.hint.clone(),
                    expanded_context: r.expanded_context.clone(),
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
                    memory_kind: r.memory_kind.clone(),
                    content: r.content.clone(),
                    score: score + 1.0, // shift [-1,1] to [0,2] for ranking
                    result_type: r.result_type.clone(),
                    document_id: r.document_id,
                    prev_chunk_id: r.prev_chunk_id,
                    next_chunk_id: r.next_chunk_id,
                    hint: r.hint.clone(),
                    expanded_context: r.expanded_context.clone(),
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
                            memory_kind: candidate.memory_kind.clone(),
                            content: candidate.content.clone(),
                            score,
                            result_type: candidate.result_type.clone(),
                            document_id: candidate.document_id,
                            prev_chunk_id: candidate.prev_chunk_id,
                            next_chunk_id: candidate.next_chunk_id,
                            hint: candidate.hint.clone(),
                            expanded_context: candidate.expanded_context.clone(),
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

    // Zero-candidate fallback. On a fresh install without embeddings, ANN is
    // skipped and an entity is lexically findable only by its NAME (its content
    // body is not fts-indexed), so a content-body query against a plain-ingested
    // entity returns nothing even though find/list can see it. When EVERY
    // strategy came back empty, do a bounded, labeled scan of the same
    // entity_store that find/list read, so search isn't silently empty. Gated on
    // zero candidates so it never runs when normal retrieval works — no scan
    // cost on a large, healthy store.
    if lists.iter().all(|l| l.is_empty()) {
        const FALLBACK_CAP: usize = 25;
        let mut fallback: Vec<SearchResult> = Vec::new();
        for &sid in &sessions {
            let remaining = FALLBACK_CAP - fallback.len();
            if remaining == 0 {
                break;
            }
            if let Ok(entities) = storage
                .entity_text_scan_bounded(ctx, sid, query, remaining)
                .await
            {
                for (i, e) in entities.into_iter().enumerate() {
                    fallback.push(SearchResult {
                        id: e.entity_id,
                        source: "entity_store_fallback".into(),
                        memory_kind: classify_memory_kind(
                            "entity",
                            "entity_store_fallback",
                            Some(&e.entity_type),
                        )
                        .into(),
                        content: if e.context_snippet.trim().is_empty() {
                            e.entity_name.clone()
                        } else {
                            e.context_snippet.clone()
                        },
                        score: 1.0 - (i as f64 * 0.1),
                        result_type: "entity".into(),
                        document_id: None,
                        prev_chunk_id: None,
                        next_chunk_id: None,
                        hint: None,
                        expanded_context: Vec::new(),
                    });
                }
            }
        }
        if !fallback.is_empty() {
            lists.push(fallback);
            // Low weight: any genuine candidate from a real source always
            // outranks the last-resort fallback when retrieval recovers.
            weights.push(0.5);
        }
    }

    let (lists, weights) = prune_disabled_source_lists(lists, weights);
    let diagnostics = candidate_source_stats(&lists, &weights, limit, source_limit);
    let evidence = collect_candidate_evidence(query, &lists);
    let mut merged = apply_authority_adjustments(
        apply_derived_importance_adjustments(
            apply_source_aware_scoring(query, rrf_merge(lists, 60.0, &weights), &evidence),
            &derived_signals,
        ),
        pagerank_scores,
        reputation_scores,
    );
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
    let results = filter_relevant_results(
        collapse_duplicate_document_chunks(merged),
        filter.and_then(|f| f.min_score),
        filter.and_then(|f| f.memory_kinds.as_deref()),
    )
    .into_iter()
    .take(limit)
    .collect();
    Ok(SearchOutput {
        results,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(id: Uuid, source: &str, score: f64) -> SearchResult {
        SearchResult {
            id,
            source: source.into(),
            memory_kind: classify_memory_kind("entity", source, None).into(),
            content: format!("content-{source}"),
            score,
            result_type: "entity".into(),
            document_id: None,
            prev_chunk_id: None,
            next_chunk_id: None,
            hint: None,
            expanded_context: Vec::new(),
        }
    }

    #[test]
    fn classify_memory_kind_maps_recall_categories() {
        assert_eq!(
            classify_memory_kind("context_segment", "context_bm25", None),
            "episodic"
        );
        assert_eq!(classify_memory_kind("fold", "fold_ann", None), "episodic");
        assert_eq!(
            classify_memory_kind("entity", "entity_phonetic", Some("procedure")),
            "procedural"
        );
        assert_eq!(
            classify_memory_kind("entity", "entity_ann", Some("decision")),
            "procedural"
        );
        assert_eq!(
            classify_memory_kind("entity", "entity_ann", Some("turn")),
            "episodic"
        );
        assert_eq!(
            classify_memory_kind("document_chunk", "document_bm25", None),
            "semantic"
        );
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
    fn context_segment_workspace_reads_hook_conversation_id() {
        let segment = crate::context_segment::ContextSegment {
            tenant_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            segment_id: Uuid::new_v4(),
            source_session: Uuid::new_v4(),
            source_fold_id: None,
            conversation_id:
                "claude:11111111-1111-1111-1111-111111111111:/Users/bkearns/src/research".into(),
            segment_index: 0,
            start_turn: 0,
            end_turn: 0,
            start_time: None,
            end_time: None,
            segment_text: "marketing context".into(),
            segment_summary: None,
            bm25_text: "marketing context".into(),
            segment_embedding: None,
            token_count: 2,
            content_hash: "hash".into(),
            prev_segment_id: None,
            next_segment_id: None,
            created_at: chrono::Utc::now(),
        };

        assert_eq!(
            context_segment_workspace(&segment),
            Some("/Users/bkearns/src/research")
        );
    }

    #[test]
    fn context_segment_workspace_guard_blocks_unrelated_legacy_raw_context() {
        let caller = Uuid::new_v4();
        let segment = crate::context_segment::ContextSegment {
            tenant_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            segment_id: Uuid::new_v4(),
            source_session: Uuid::nil(),
            source_fold_id: None,
            conversation_id:
                "claude:11111111-1111-1111-1111-111111111111:/Users/bkearns/src/research".into(),
            segment_index: 0,
            start_turn: 0,
            end_turn: 0,
            start_time: None,
            end_time: None,
            segment_text: "LinkedIn marketing context".into(),
            segment_summary: None,
            bm25_text: "linkedin marketing context".into(),
            segment_embedding: None,
            token_count: 3,
            content_hash: "hash".into(),
            prev_segment_id: None,
            next_segment_id: None,
            created_at: chrono::Utc::now(),
        };
        let filter = SearchFilter {
            scope: SearchScope::Both,
            workspace_cwd: Some("/Users/bkearns/src/ferrosa-suite".into()),
            ..SearchFilter::default()
        };

        assert!(!context_segment_allowed_for_workspace(
            caller,
            Uuid::nil(),
            Some(&filter),
            &segment
        ));
    }

    #[test]
    fn context_segment_workspace_guard_allows_same_tree_raw_context() {
        let caller = Uuid::new_v4();
        let segment = crate::context_segment::ContextSegment {
            tenant_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            segment_id: Uuid::new_v4(),
            source_session: Uuid::nil(),
            source_fold_id: None,
            conversation_id:
                "claude:11111111-1111-1111-1111-111111111111:/Users/bkearns/src/ferrosa-suite"
                    .into(),
            segment_index: 0,
            start_turn: 0,
            end_turn: 0,
            start_time: None,
            end_time: None,
            segment_text: "memory hook context".into(),
            segment_summary: None,
            bm25_text: "memory hook context".into(),
            segment_embedding: None,
            token_count: 3,
            content_hash: "hash".into(),
            prev_segment_id: None,
            next_segment_id: None,
            created_at: chrono::Utc::now(),
        };
        let filter = SearchFilter {
            scope: SearchScope::Both,
            workspace_cwd: Some("/Users/bkearns/src/ferrosa-suite/crate".into()),
            ..SearchFilter::default()
        };

        assert!(context_segment_allowed_for_workspace(
            caller,
            Uuid::nil(),
            Some(&filter),
            &segment
        ));
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
            candidate_limit: Some(25),
            min_score: Some(0.062),
            memory_kinds: Some(vec!["procedural".into(), "semantic".into()]),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.scope, SearchScope::Both);
        assert_eq!(back.workspace_cwd.as_deref(), Some("/repo/project"));
        assert_eq!(back.candidate_limit, Some(25));
        assert_eq!(back.min_score, Some(0.062));
        assert_eq!(
            back.memory_kinds,
            Some(vec!["procedural".into(), "semantic".into()])
        );
        assert_eq!(back.entity_types, Some(vec!["skill".into()]));
        assert_eq!(back.tags, Some(vec!["testing".into(), "quality".into()]));
    }

    #[test]
    fn filter_relevant_results_drops_low_score_and_wrong_memory_kind() {
        let keep = Uuid::new_v4();
        let low = Uuid::new_v4();
        let episodic = Uuid::new_v4();
        let mut keep_result = make_result(keep, "document_bm25", 0.08);
        keep_result.memory_kind = "semantic".into();
        let mut low_result = make_result(low, "context_bm25", 0.03);
        low_result.memory_kind = "semantic".into();
        let mut episodic_result = make_result(episodic, "context_ann", 0.09);
        episodic_result.memory_kind = "episodic".into();

        let results = filter_relevant_results(
            vec![low_result, keep_result, episodic_result],
            Some(0.062),
            Some(&["semantic".to_string(), "procedural".to_string()]),
        );

        assert_eq!(
            results.iter().map(|result| result.id).collect::<Vec<_>>(),
            vec![keep]
        );
    }

    #[test]
    fn source_aware_scoring_drops_ann_only_without_lexical_overlap() {
        let ann_id = Uuid::new_v4();
        let lexical_id = Uuid::new_v4();
        let mut ann = make_result(ann_id, "context_ann", 0.07);
        ann.content = "unrelated previous workflow output".into();
        let mut lexical = make_result(lexical_id, "context_bm25", 0.065);
        lexical.content = "memory hook installer search cleanup".into();
        let lists = vec![vec![ann.clone()], vec![lexical.clone()]];
        let evidence = collect_candidate_evidence("memory hook installer", &lists);

        let scored =
            apply_source_aware_scoring("memory hook installer", vec![ann, lexical], &evidence);

        assert_eq!(
            scored.iter().map(|result| result.id).collect::<Vec<_>>(),
            vec![lexical_id]
        );
        assert!(scored[0].score > 0.20);
    }

    #[test]
    fn source_aware_scoring_boosts_bm25_ann_corroboration() {
        let shared = Uuid::new_v4();
        let ann_only = Uuid::new_v4();
        let mut shared_bm25 = make_result(shared, "context_bm25", 0.06);
        shared_bm25.content = "session context sentinel retrieval".into();
        let mut shared_ann = make_result(shared, "context_ann", 0.06);
        shared_ann.content = "session context sentinel retrieval".into();
        let mut weak_ann = make_result(ann_only, "document_ann", 0.07);
        weak_ann.content = "corpus metadata unrelated topic".into();
        let lists = vec![
            vec![shared_bm25.clone()],
            vec![shared_ann.clone()],
            vec![weak_ann.clone()],
        ];
        let evidence = collect_candidate_evidence("session context sentinel", &lists);

        let scored = apply_source_aware_scoring(
            "session context sentinel",
            vec![weak_ann, shared_bm25],
            &evidence,
        );

        assert_eq!(scored.first().map(|result| result.id), Some(shared));
        assert!(scored.iter().all(|result| result.id != ann_only));
    }

    #[test]
    fn source_aware_scoring_keeps_datalog_corroborated_ann_candidate() {
        let candidate_id = Uuid::new_v4();
        let mut ann = make_result(candidate_id, "entity_ann", 0.03);
        ann.content = "graph connected but lexically distant memory".into();
        let mut frontier = make_result(candidate_id, "datalog_frontier:uses", 0.04);
        frontier.content = "graph connected but lexically distant memory".into();
        let lists = vec![vec![ann.clone()], vec![frontier]];
        let evidence = collect_candidate_evidence("lexical seed phrase", &lists);

        let scored = apply_source_aware_scoring("lexical seed phrase", vec![ann], &evidence);

        assert_eq!(scored.first().map(|result| result.id), Some(candidate_id));
        assert!(scored[0].score > 0.03);
    }

    #[test]
    fn source_aware_scoring_demotes_metadata_stubs_for_task_queries() {
        let metadata_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();
        let mut metadata = make_result(metadata_id, "document_bm25", 0.20);
        metadata.result_type = "document_chunk".into();
        metadata.memory_kind = "semantic".into();
        metadata.content =
            "BOOK METADATA | Title: Rust Web Development | Key topics: memory search cleanup"
                .into();
        let mut content = make_result(content_id, "context_bm25", 0.12);
        content.result_type = "context_segment".into();
        content.memory_kind = "episodic".into();
        content.content = "memory search cleanup candidate pool source aware scoring".into();
        let lists = vec![vec![metadata.clone()], vec![content.clone()]];
        let evidence = collect_candidate_evidence("memory search cleanup", &lists);

        let scored =
            apply_source_aware_scoring("memory search cleanup", vec![metadata, content], &evidence);

        assert_eq!(scored.first().map(|result| result.id), Some(content_id));
    }

    #[test]
    fn source_aware_scoring_keeps_metadata_for_bibliographic_queries() {
        let metadata_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();
        let mut metadata = make_result(metadata_id, "document_bm25", 0.20);
        metadata.result_type = "document_chunk".into();
        metadata.content =
            "BOOK METADATA | Title: Rust Web Development | Author: Karuna Murti".into();
        let mut content = make_result(content_id, "context_bm25", 0.12);
        content.content = "notes mention rust web development author reference".into();
        let lists = vec![vec![metadata.clone()], vec![content.clone()]];
        let evidence = collect_candidate_evidence("book author rust web development", &lists);

        let scored = apply_source_aware_scoring(
            "book author rust web development",
            vec![metadata, content],
            &evidence,
        );

        assert_eq!(scored.first().map(|result| result.id), Some(metadata_id));
    }

    #[test]
    fn duplicate_document_collapse_prefers_content_chunk_over_metadata_stub() {
        let document_id = Uuid::new_v4();
        let metadata_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();
        let mut metadata = make_result(metadata_id, "document_bm25", 0.40);
        metadata.result_type = "document_chunk".into();
        metadata.document_id = Some(document_id);
        metadata.content = "BOOK METADATA | Title: Noisy Metadata".into();
        let mut content = make_result(content_id, "document_bm25", 0.20);
        content.result_type = "document_chunk".into();
        content.document_id = Some(document_id);
        content.content = "actual implementation detail for candidate cleanup".into();

        let collapsed = collapse_duplicate_document_chunks(vec![metadata, content]);

        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].id, content_id);
    }

    #[test]
    fn authority_adjustments_boost_positive_and_penalize_negative_reputation() {
        let trusted = Uuid::new_v4();
        let distrusted = Uuid::new_v4();
        let neutral = Uuid::new_v4();
        let mut reputation = HashMap::new();
        reputation.insert(trusted, 1.0);
        reputation.insert(distrusted, -1.0);

        let scored = apply_authority_adjustments(
            vec![
                make_result(distrusted, "document_bm25", 0.20),
                make_result(neutral, "document_bm25", 0.20),
                make_result(trusted, "document_bm25", 0.20),
            ],
            None,
            Some(&reputation),
        );

        assert_eq!(scored.first().map(|result| result.id), Some(trusted));
        let trusted_score = scored
            .iter()
            .find(|result| result.id == trusted)
            .unwrap()
            .score;
        let neutral_score = scored
            .iter()
            .find(|result| result.id == neutral)
            .unwrap()
            .score;
        assert!(trusted_score > neutral_score);
        assert!(scored.iter().all(|result| result.id != distrusted));
    }

    #[test]
    fn authority_adjustments_boost_pagerank_without_negative_values() {
        let authority = Uuid::new_v4();
        let ordinary = Uuid::new_v4();
        let mut pagerank = HashMap::new();
        pagerank.insert(authority, 1.0);

        let scored = apply_authority_adjustments(
            vec![
                make_result(ordinary, "document_bm25", 0.20),
                make_result(authority, "document_bm25", 0.20),
            ],
            Some(&pagerank),
            None,
        );

        assert_eq!(scored.first().map(|result| result.id), Some(authority));
        assert!(scored[0].score > scored[1].score);
    }

    #[test]
    fn derived_importance_adjustments_promote_confident_scoped_frontier_hits() {
        let inferred = Uuid::new_v4();
        let wrong_scope = Uuid::new_v4();
        let mut signals = HashMap::new();
        signals.insert(
            inferred,
            DerivedSearchSignals {
                derived_confidence: 0.9,
                support_count: 3,
                path_distance: 1,
                predicate_weight: 0.85,
                scope_guard: true,
            },
        );
        signals.insert(
            wrong_scope,
            DerivedSearchSignals {
                derived_confidence: 0.9,
                support_count: 3,
                path_distance: 1,
                predicate_weight: 0.85,
                scope_guard: false,
            },
        );

        let adjusted = apply_derived_importance_adjustments(
            vec![
                make_result(inferred, "datalog_frontier:task_relevant", 0.02),
                make_result(wrong_scope, "datalog_frontier:task_relevant", 0.02),
            ],
            &signals,
        );

        let inferred_score = adjusted.iter().find(|r| r.id == inferred).unwrap().score;
        let wrong_scope_score = adjusted.iter().find(|r| r.id == wrong_scope).unwrap().score;
        assert!(inferred_score > 0.08);
        assert_eq!(wrong_scope_score, 0.02);
    }

    #[test]
    fn rrf_merge_ignores_zero_weight_source_lists() {
        let enabled = Uuid::new_v4();
        let disabled = Uuid::new_v4();
        let merged = rrf_merge(
            vec![
                vec![make_result(disabled, "document_phonetic", 1.0)],
                vec![make_result(enabled, "document_bm25", 1.0)],
            ],
            60.0,
            &[0.0, 1.0],
        );

        assert_eq!(
            merged.iter().map(|result| result.id).collect::<Vec<_>>(),
            vec![enabled]
        );
    }

    #[test]
    fn prune_disabled_source_lists_removes_zero_weight_evidence_sources() {
        let enabled = Uuid::new_v4();
        let disabled = Uuid::new_v4();
        let (lists, weights) = prune_disabled_source_lists(
            vec![
                vec![make_result(disabled, "document_phonetic", 1.0)],
                vec![make_result(enabled, "document_bm25", 1.0)],
            ],
            vec![0.0, 1.0],
        );

        assert_eq!(weights, vec![1.0]);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0][0].id, enabled);
    }

    #[test]
    fn source_limit_defaults_to_bounded_double_limit() {
        assert_eq!(source_limit(10, None), 20);
        assert_eq!(source_limit(25, None), 50);
        assert_eq!(source_limit(50, None), 50);
    }

    #[test]
    fn fusion_profiles_select_expected_source_families() {
        let bm25 = FusionConfig::profile("bm25-only").unwrap();
        assert!(bm25.document_bm25_weight > 0.0);
        assert!(bm25.context_bm25_weight > 0.0);
        assert_eq!(bm25.document_ann_weight, 0.0);
        assert_eq!(bm25.document_phonetic_weight, 0.0);

        let auto = FusionConfig::profile("auto").unwrap();
        assert!(auto.document_bm25_weight > 0.0);
        assert!(auto.context_bm25_weight > 0.0);
        assert!(auto.document_ann_weight > 0.0);
        assert!(auto.context_ann_weight > 0.0);
        assert_eq!(auto.document_phonetic_weight, 0.0);

        let workspace = FusionConfig::profile("bm25-semantic-workspace").unwrap();
        assert!(workspace.document_bm25_weight > 0.0);
        assert!(workspace.document_ann_weight > 0.0);
        assert!(workspace.workspace_weight > 0.0);
        assert!(workspace.datalog_frontier_weight > 0.0);
        assert_eq!(workspace.document_phonetic_weight, 0.0);

        let semantic = FusionConfig::profile("semantic-only").unwrap();
        assert!(semantic.document_ann_weight > 0.0);
        assert_eq!(semantic.document_bm25_weight, 0.0);
        assert_eq!(semantic.document_phonetic_weight, 0.0);

        let combined = FusionConfig::profile("bm25-semantic-phonetic-workspace").unwrap();
        assert!(combined.document_bm25_weight > 0.0);
        assert!(combined.document_ann_weight > 0.0);
        assert!(combined.document_phonetic_weight > 0.0);
        assert!(combined.workspace_weight > 0.0);
        assert!(FusionConfig::profile("not-a-profile").is_none());
    }

    #[test]
    fn fusion_weight_overrides_accept_supported_keys() {
        let mut config = FusionConfig::profile("bm25-only").unwrap();
        assert!(config.set_weight("document_ann", 3.0));
        assert_eq!(config.document_ann_weight, 3.0);
        assert!(config.set_weight("graph", 1.25));
        assert_eq!(config.pagerank_weight, 1.25);
        assert!(!config.set_weight("unknown_source", 1.0));
    }

    #[test]
    fn source_limit_respects_explicit_candidate_limit_floor_and_ceiling() {
        let low = SearchFilter {
            candidate_limit: Some(5),
            ..Default::default()
        };
        let high = SearchFilter {
            candidate_limit: Some(100),
            ..Default::default()
        };
        assert_eq!(source_limit(10, Some(&low)), 10);
        assert_eq!(source_limit(10, Some(&high)), 50);
    }

    #[test]
    fn candidate_source_stats_reports_source_counts_and_uniques() {
        let shared = Uuid::new_v4();
        let unique = Uuid::new_v4();
        let lists = vec![
            vec![
                make_result(shared, "document_bm25", 1.0),
                make_result(unique, "document_bm25", 0.9),
            ],
            vec![make_result(shared, "document_ann", 1.0)],
        ];

        let stats = candidate_source_stats(&lists, &[2.5, 1.5], 10, 20);

        assert_eq!(stats.requested_limit, 10);
        assert_eq!(stats.source_limit, 20);
        assert_eq!(stats.total_candidates, 3);
        assert_eq!(stats.unique_candidates, 2);
        assert_eq!(stats.sources.len(), 2);
        assert_eq!(stats.sources[0].source, "document_bm25");
        assert_eq!(stats.sources[0].candidates, 2);
        assert_eq!(stats.sources[0].unique_candidates, 2);
        assert_eq!(stats.sources[0].weight, 2.5);
        assert_eq!(stats.sources[1].source, "document_ann");
        assert_eq!(stats.sources[1].candidates, 1);
        assert_eq!(stats.sources[1].unique_candidates, 1);
        assert_eq!(stats.sources[1].weight, 1.5);
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
    fn collapse_duplicate_document_chunks_keeps_best_chunk_per_document() {
        let doc_a = Uuid::new_v4();
        let doc_b = Uuid::new_v4();
        let mut first_a = make_result(Uuid::new_v4(), "document_bm25", 0.9);
        first_a.result_type = "document_chunk".into();
        first_a.document_id = Some(doc_a);
        let mut second_a = make_result(Uuid::new_v4(), "document_bm25", 0.8);
        second_a.result_type = "document_chunk".into();
        second_a.document_id = Some(doc_a);
        let mut first_b = make_result(Uuid::new_v4(), "document_bm25", 0.7);
        first_b.result_type = "document_chunk".into();
        first_b.document_id = Some(doc_b);

        let collapsed =
            collapse_duplicate_document_chunks(vec![first_a.clone(), second_a, first_b.clone()]);

        assert_eq!(collapsed.len(), 2);
        assert_eq!(collapsed[0].id, first_a.id);
        assert_eq!(collapsed[1].id, first_b.id);
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

        // Zero weight on list1 means only list2 contributes; disabled-source
        // candidates must not survive as score-zero rows that later source-aware
        // priors can resurrect.
        let merged = rrf_merge(vec![list1, list2], 60.0, &[0.0, 1.0]);
        assert_eq!(merged.len(), 1);
        assert!(merged.iter().all(|r| r.id != id1));
        assert_eq!(merged[0].id, id2);
        assert!((merged[0].score - 1.0 / 61.0).abs() < 1e-10);
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

    /// Fresh-install regression (t_8b9583b7): a plain-ingested entity (no
    /// embedding) is visible to find/list but its CONTENT body is not lexically
    /// indexed — only its name is. A content-body query whose terms don't appear
    /// in the name would return zero candidates. The bounded entity_store
    /// fallback must surface it (labeled `entity_store_fallback`), while a query
    /// with no token overlap must still return nothing (fallback stays bounded
    /// by relevance, not a blind dump).
    #[tokio::test]
    async fn hybrid_search_falls_back_to_entity_store_when_all_sources_empty() {
        use crate::storage::mock::MockStorage;
        use crate::types::{EntityEntry, MemoryState, TenantContext};

        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let sid = Uuid::new_v4();
        let id = Uuid::new_v4();

        // Name does NOT contain the query terms; the content snippet does.
        // No embedding => ANN is skipped.
        storage
            .entity_put(
                &ctx,
                &EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: id,
                    session_id: sid,
                    entity_name: "Project Phoenix".into(),
                    entity_type: "note".into(),
                    source_fold_id: None,
                    context_snippet: "the migration runbook for the billing service".into(),
                    entity_embedding: None,
                    confidence: 1.0,
                    state: MemoryState::Active,
                    created_at: chrono::Utc::now(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let config = FusionConfig::default();

        // Content-body query (no name overlap, no embedding) must still find it
        // via the fallback.
        let results = hybrid_search(
            &storage,
            &ctx,
            sid,
            "billing service runbook",
            None,
            10,
            None,
            None,
            None,
            &config,
            None,
        )
        .await
        .unwrap();
        assert!(
            results.iter().any(|r| r.id == id),
            "fresh-install content query must surface the entity via fallback, got {} results",
            results.len()
        );
        assert!(
            results.iter().any(|r| r.source == "entity_store_fallback"),
            "the recovered candidate must be labeled entity_store_fallback for auditability"
        );

        // Negative control: a query with no token overlap returns nothing —
        // the fallback is bounded by relevance, not a blind dump of the store.
        let none = hybrid_search(
            &storage,
            &ctx,
            sid,
            "kubernetes networking",
            None,
            10,
            None,
            None,
            None,
            &config,
            None,
        )
        .await
        .unwrap();
        assert!(
            none.is_empty(),
            "irrelevant query must not trigger a blind fallback dump, got {} results",
            none.len()
        );
    }

    #[tokio::test]
    async fn hybrid_search_scope_both_drops_raw_context_from_unrelated_workspace() {
        use crate::storage::mock::MockStorage;
        use crate::types::TenantContext;

        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let sid = Uuid::new_v4();
        let leaked_id = Uuid::new_v4();
        let local_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        storage.context_segments.lock().await.extend([
            crate::context_segment::ContextSegment {
                tenant_id: ctx.tenant_id,
                session_id: Uuid::nil(),
                segment_id: leaked_id,
                source_session: Uuid::nil(),
                source_fold_id: None,
                conversation_id: format!("claude:{}:/Users/bkearns/src/research", Uuid::new_v4()),
                segment_index: 0,
                start_turn: 0,
                end_turn: 0,
                start_time: None,
                end_time: None,
                segment_text: "FERROSA_LEAK_SENTINEL LinkedIn marketing context".into(),
                segment_summary: None,
                bm25_text: "FERROSA_LEAK_SENTINEL LinkedIn marketing context".to_ascii_lowercase(),
                segment_embedding: None,
                token_count: 5,
                content_hash: leaked_id.to_string(),
                prev_segment_id: None,
                next_segment_id: None,
                created_at: now,
            },
            crate::context_segment::ContextSegment {
                tenant_id: ctx.tenant_id,
                session_id: Uuid::nil(),
                segment_id: local_id,
                source_session: Uuid::nil(),
                source_fold_id: None,
                conversation_id: format!(
                    "claude:{}:/Users/bkearns/src/ferrosa-suite",
                    Uuid::new_v4()
                ),
                segment_index: 1,
                start_turn: 0,
                end_turn: 0,
                start_time: None,
                end_time: None,
                segment_text: "FERROSA_LEAK_SENTINEL memory hook context".into(),
                segment_summary: None,
                bm25_text: "FERROSA_LEAK_SENTINEL memory hook context".to_ascii_lowercase(),
                segment_embedding: None,
                token_count: 5,
                content_hash: local_id.to_string(),
                prev_segment_id: None,
                next_segment_id: None,
                created_at: now,
            },
        ]);
        let filter = SearchFilter {
            scope: SearchScope::Both,
            workspace_cwd: Some("/Users/bkearns/src/ferrosa-suite".into()),
            ..SearchFilter::default()
        };

        let results = hybrid_search(
            &storage,
            &ctx,
            sid,
            "FERROSA_LEAK_SENTINEL",
            None,
            10,
            None,
            None,
            None,
            &FusionConfig::profile("all").unwrap(),
            Some(&filter),
        )
        .await
        .unwrap();

        assert!(results.iter().any(|result| result.id == local_id));
        assert!(!results.iter().any(|result| result.id == leaked_id));
    }

    #[tokio::test]
    async fn hybrid_search_min_score_keeps_corroborated_hit_and_drops_singleton_noise() {
        use crate::storage::mock::MockStorage;
        use crate::types::{DocumentChunk, TenantContext};

        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let sid = Uuid::new_v4();
        let relevant_id = Uuid::new_v4();
        let stale_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let chunk = |chunk_id, ordinal, content: &str, embedding| DocumentChunk {
            tenant_id: ctx.tenant_id,
            session_id: sid,
            document_id: chunk_id,
            chunk_id,
            ordinal,
            source_doc_id: format!("doc-{ordinal}"),
            title: format!("doc {ordinal}"),
            section_path: String::new(),
            semantic_kind: "text".into(),
            content: content.into(),
            bm25_text: content.into(),
            chunk_embedding: embedding,
            token_count: 16,
            content_hash: chunk_id.to_string(),
            prev_chunk_id: None,
            next_chunk_id: None,
            overlap_from_prev: false,
            overlap_to_next: false,
            metadata: serde_json::Value::Null,
            created_at: now,
            updated_at: now,
        };
        storage.document_chunks.lock().await.extend([
            chunk(
                relevant_id,
                0,
                "memory hook installer should pass min_score and memory_kinds to search",
                Some(vec![1.0, 0.0]),
            ),
            chunk(
                stale_id,
                1,
                "release notes from an unrelated stale workspace",
                None,
            ),
        ]);
        let filter = SearchFilter {
            scope: SearchScope::SessionOnly,
            min_score: Some(0.062),
            memory_kinds: Some(vec!["semantic".into()]),
            ..Default::default()
        };

        let results = hybrid_search(
            &storage,
            &ctx,
            sid,
            "memory hook installer search",
            Some(&[1.0, 0.0]),
            5,
            None,
            None,
            None,
            &FusionConfig::default(),
            Some(&filter),
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, relevant_id);
        assert!(results[0].score >= 0.062);
    }

    #[tokio::test]
    async fn hybrid_search_scope_both_retrieves_global_and_legacy_nil_corpus_chunks() {
        use crate::storage::mock::MockStorage;
        use crate::types::{DocumentChunk, TenantContext};

        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let caller_session = Uuid::new_v4();
        let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let nil_session = Uuid::nil();
        let now = chrono::Utc::now();
        let global_chunk_id = Uuid::new_v4();
        let nil_chunk_id = Uuid::new_v4();
        let make_chunk = |session_id, chunk_id, content: &str| DocumentChunk {
            tenant_id: ctx.tenant_id,
            session_id,
            document_id: chunk_id,
            chunk_id,
            ordinal: 0,
            source_doc_id: format!("doc-{chunk_id}"),
            title: "curated corpus".into(),
            section_path: String::new(),
            semantic_kind: "text".into(),
            content: content.into(),
            bm25_text: content.to_ascii_lowercase(),
            chunk_embedding: None,
            token_count: 16,
            content_hash: chunk_id.to_string(),
            prev_chunk_id: None,
            next_chunk_id: None,
            overlap_from_prev: false,
            overlap_to_next: false,
            metadata: serde_json::Value::Null,
            created_at: now,
            updated_at: now,
        };
        storage.document_chunks.lock().await.extend([
            make_chunk(
                global_session,
                global_chunk_id,
                "Curated corpus explains RLM trace persistence.",
            ),
            make_chunk(
                nil_session,
                nil_chunk_id,
                "Legacy nil corpus explains MemScene consolidation.",
            ),
        ]);
        let both_filter = SearchFilter {
            scope: SearchScope::Both,
            ..Default::default()
        };

        let both_results = hybrid_search(
            &storage,
            &ctx,
            caller_session,
            "curated corpus explains",
            None,
            10,
            None,
            None,
            None,
            &FusionConfig::default(),
            Some(&both_filter),
        )
        .await
        .unwrap();
        let both_ids = both_results
            .iter()
            .map(|result| result.id)
            .collect::<HashSet<_>>();

        assert!(both_ids.contains(&global_chunk_id));
        assert!(both_ids.contains(&nil_chunk_id));

        let session_filter = SearchFilter {
            scope: SearchScope::SessionOnly,
            ..Default::default()
        };
        let session_results = hybrid_search(
            &storage,
            &ctx,
            caller_session,
            "curated corpus explains",
            None,
            10,
            None,
            None,
            None,
            &FusionConfig::default(),
            Some(&session_filter),
        )
        .await
        .unwrap();

        assert!(session_results.is_empty());
    }

    #[tokio::test]
    async fn hybrid_search_returns_memory_reachable_through_datalog_frontier() {
        use crate::storage::mock::MockStorage;
        use crate::types::{EntityEntry, MemoryState, TenantContext, TypedEdge};

        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let sid = Uuid::new_v4();
        let seed_id = Uuid::new_v4();
        let inferred_id = Uuid::new_v4();
        let now = chrono::Utc::now();

        storage
            .entity_put(
                &ctx,
                &EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: seed_id,
                    session_id: sid,
                    entity_name: "Atlas cache".into(),
                    entity_type: "concept".into(),
                    context_snippet: "Atlas cache routes durable recall requests".into(),
                    confidence: 1.0,
                    state: MemoryState::Active,
                    created_at: now,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        storage
            .entity_put(
                &ctx,
                &EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: inferred_id,
                    session_id: sid,
                    entity_name: "Borealis routing note".into(),
                    entity_type: "decision".into(),
                    context_snippet:
                        "Use the durable graph note when the routing cache cannot answer directly."
                            .into(),
                    confidence: 1.0,
                    state: MemoryState::Active,
                    created_at: now,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        storage
            .typed_edge_put(
                &ctx,
                &TypedEdge {
                    tenant_id: ctx.tenant_id,
                    session_id: sid,
                    src_id: seed_id,
                    edge_type: "task_relevant".into(),
                    dst_id: inferred_id,
                    weight: 0.9,
                    metadata: Some(r#"{"support_count":3}"#.into()),
                    created_at: now,
                },
            )
            .await
            .unwrap();

        let filter = SearchFilter {
            scope: SearchScope::SessionOnly,
            min_score: Some(0.062),
            datalog_frontier_min_confidence: Some(0.3),
            ..Default::default()
        };
        let results = hybrid_search(
            &storage,
            &ctx,
            sid,
            "Atlas cache",
            None,
            10,
            None,
            None,
            None,
            &FusionConfig::default(),
            Some(&filter),
        )
        .await
        .unwrap();

        let inferred = results
            .iter()
            .find(|result| result.id == inferred_id)
            .expect("graph-reachable memory should be returned");
        assert!(inferred.source.starts_with("datalog_frontier:"));
        assert!(inferred.score >= 0.062);
        assert!(
            inferred
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("Derived via task_relevant")
        );
    }

    #[tokio::test]
    async fn datalog_frontier_can_corroborate_existing_weak_seed_candidates() {
        use crate::storage::mock::MockStorage;
        use crate::types::{EntityEntry, MemoryState, TenantContext, TypedEdge};

        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let sid = Uuid::new_v4();
        let lexical_seed_id = Uuid::new_v4();
        let weak_ann_id = Uuid::new_v4();
        let now = chrono::Utc::now();

        for (entity_id, entity_name, context_snippet) in [
            (
                lexical_seed_id,
                "Datalog frontier seed",
                "The lexical seed is directly query visible.",
            ),
            (
                weak_ann_id,
                "Datalog frontier neighbor",
                "This memory is useful only because the graph connects it.",
            ),
        ] {
            storage
                .entity_put(
                    &ctx,
                    &EntityEntry {
                        tenant_id: ctx.tenant_id,
                        entity_id,
                        session_id: sid,
                        entity_name: entity_name.into(),
                        entity_type: "concept".into(),
                        context_snippet: context_snippet.into(),
                        confidence: 1.0,
                        state: MemoryState::Active,
                        created_at: now,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        storage
            .typed_edge_put(
                &ctx,
                &TypedEdge {
                    tenant_id: ctx.tenant_id,
                    session_id: sid,
                    src_id: lexical_seed_id,
                    edge_type: "uses".into(),
                    dst_id: weak_ann_id,
                    weight: 0.9,
                    metadata: Some(r#"{"support_count":2}"#.into()),
                    created_at: now,
                },
            )
            .await
            .unwrap();

        let seed_candidates = vec![
            make_result(lexical_seed_id, "entity_phonetic", 1.0),
            make_result(weak_ann_id, "entity_ann", 1.0),
        ];
        let filter = SearchFilter {
            scope: SearchScope::SessionOnly,
            datalog_frontier_min_confidence: Some(0.3),
            ..Default::default()
        };
        let (frontier, signals) = datalog_frontier_candidates(
            &storage,
            &ctx,
            &[sid],
            &seed_candidates,
            10,
            Some(&filter),
        )
        .await
        .unwrap();

        let corroborated = frontier
            .iter()
            .find(|candidate| candidate.id == weak_ann_id)
            .expect("frontier should corroborate a weak candidate already seen by ANN");
        assert_eq!(corroborated.source, "datalog_frontier:uses");
        assert!(signals.contains_key(&weak_ann_id));
    }

    #[tokio::test]
    async fn datalog_frontier_suppresses_weak_candidates_before_hook_min_score() {
        use crate::storage::mock::MockStorage;
        use crate::types::{EntityEntry, MemoryState, TenantContext, TypedEdge};

        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let sid = Uuid::new_v4();
        let seed_id = Uuid::new_v4();
        let weak_id = Uuid::new_v4();
        let now = chrono::Utc::now();

        storage
            .entity_put(
                &ctx,
                &EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: weak_id,
                    session_id: sid,
                    entity_name: "Low confidence derived note".into(),
                    entity_type: "decision".into(),
                    context_snippet: "This should not clutter hook context.".into(),
                    confidence: 1.0,
                    state: MemoryState::Active,
                    created_at: now,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        storage
            .typed_edge_put(
                &ctx,
                &TypedEdge {
                    tenant_id: ctx.tenant_id,
                    session_id: sid,
                    src_id: seed_id,
                    edge_type: "task_relevant".into(),
                    dst_id: weak_id,
                    weight: 0.1,
                    metadata: None,
                    created_at: now,
                },
            )
            .await
            .unwrap();

        let seed = make_result(seed_id, "entity_phonetic", 1.0);
        let filter = SearchFilter {
            datalog_frontier_min_confidence: Some(0.3),
            ..Default::default()
        };
        let (frontier, signals) =
            datalog_frontier_candidates(&storage, &ctx, &[sid], &[seed], 10, Some(&filter))
                .await
                .unwrap();

        assert!(frontier.is_empty());
        assert!(signals.is_empty());
    }
}
