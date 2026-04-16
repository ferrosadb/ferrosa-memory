//! Recursive query decomposition with multi-pass retrieval.
//!
//! Decomposes complex queries into sub-queries, uses the Datalog engine
//! for transitive closure and convergence detection, and fuses results
//! from multiple retrieval passes.

use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::config::{DatalogConfig, RmhConfig};
use crate::datalog;
use crate::hybrid_search::{self, FusionConfig, SearchResult};
use crate::storage::Storage;
use crate::types::*;
use crate::warmth;

/// Decompose a query into sub-queries using heuristics (no LLM).
///
/// Strategy:
/// 1. Always include the original query first
/// 2. Split on conjunctions ("and", "or", "as well as")
/// 3. Extract quoted phrases as separate sub-queries
/// 4. Cap at 5 sub-queries total
pub fn decompose_query(query: &str) -> Vec<SubQuery> {
    assert!(!query.is_empty(), "query must not be empty");

    let mut sub_queries = Vec::new();

    // Always include original
    sub_queries.push(SubQuery {
        query_text: query.to_string(),
        reasoning: "original query".to_string(),
    });

    // Split on conjunctions
    let conjunctions = [" and ", " or ", " as well as ", " along with "];
    for conj in &conjunctions {
        if query.to_lowercase().contains(conj) {
            for part in query.to_lowercase().split(conj) {
                let trimmed = part.trim().to_string();
                if !trimmed.is_empty()
                    && trimmed.len() > 3
                    && !sub_queries
                        .iter()
                        .any(|sq| sq.query_text.to_lowercase() == trimmed)
                {
                    sub_queries.push(SubQuery {
                        query_text: trimmed,
                        reasoning: format!("conjunction split on '{}'", conj.trim()),
                    });
                }
            }
            break; // Only split on first matching conjunction
        }
    }

    // Extract quoted phrases as separate sub-queries
    let mut start = 0;
    while let Some(open) = query[start..].find('"') {
        let open_abs = start + open + 1;
        if let Some(close) = query[open_abs..].find('"') {
            let phrase = query[open_abs..open_abs + close].trim().to_string();
            if phrase.len() > 3
                && !sub_queries
                    .iter()
                    .any(|sq| sq.query_text.to_lowercase() == phrase.to_lowercase())
            {
                sub_queries.push(SubQuery {
                    query_text: phrase,
                    reasoning: "quoted phrase extraction".to_string(),
                });
            }
            start = open_abs + close + 1;
        } else {
            break;
        }
    }

    // Cap at 5
    sub_queries.truncate(5);
    sub_queries
}

/// Recursive multi-pass exploration with Datalog-driven discovery.
///
/// Pass 1: Decompose query -> hybrid_search per sub-query -> collect seeds
/// Pass 2+: Evaluate Datalog rules -> discover related/reachable entities ->
///          hybrid_search on new discoveries
/// Convergence: Datalog fixpoint OR novelty < threshold OR max passes
#[allow(clippy::too_many_arguments)]
pub async fn explore(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query: &str,
    embedding: Option<&[f32]>,
    rmh_config: &RmhConfig,
    datalog_config: &DatalogConfig,
) -> anyhow::Result<RecursiveExploreResult> {
    anyhow::ensure!(!query.is_empty(), "query must not be empty");

    let sub_queries = decompose_query(query);
    let mut all_results: HashMap<Uuid, SearchResult> = HashMap::new();
    let mut seen_entity_ids: HashSet<Uuid> = HashSet::new();
    let mut passes: usize = 0;
    let mut converged = false;
    let mut derived_facts_count: usize = 0;
    let fusion_config = FusionConfig::default();

    // Get warmth scores for fusion
    let warmth_scores = warmth::get_warmth_scores(storage, ctx, session_id, rmh_config)
        .await
        .ok();
    let warmth_ref = warmth_scores.as_ref();

    // Pass 1: Search each sub-query
    for sq in &sub_queries {
        let results = hybrid_search::hybrid_search(
            storage,
            ctx,
            session_id,
            &sq.query_text,
            embedding,
            rmh_config.max_explore_entities.min(50), // hybrid_search caps at 50
            warmth_ref,
            None,
            None,
            &fusion_config,
            None,
        )
        .await?;

        for r in results {
            seen_entity_ids.insert(r.id);
            all_results.entry(r.id).or_insert(r);
        }
    }
    passes += 1;

    // Pass 2+: Datalog-driven discovery
    for pass_idx in 1..rmh_config.max_explore_passes {
        if seen_entity_ids.len() >= rmh_config.max_explore_entities {
            break;
        }

        // Load session facts and evaluate Datalog rules
        let facts = datalog::load_session_facts(storage, ctx, session_id).await?;
        let rules = datalog::builtin_rules();
        let (all_facts, derived) = datalog::evaluate(
            &rules,
            &facts,
            datalog_config.max_iterations,
            datalog_config.max_facts,
        );
        derived_facts_count = derived.len();

        // Find entities related to our seed set via derived facts
        let mut new_entity_ids: HashSet<Uuid> = HashSet::new();

        // Check "related" and "reachable" derived predicates
        for predicate in &["related", "reachable"] {
            if let Some(fact_set) = all_facts.get(predicate) {
                for args in fact_set {
                    if args.len() >= 2
                        && let (Term::Const(src), Term::Const(dst)) = (&args[0], &args[1])
                        && seen_entity_ids.contains(src)
                        && !seen_entity_ids.contains(dst)
                    {
                        new_entity_ids.insert(*dst);
                    }
                }
            }
        }

        // Convergence check: novelty ratio
        let novelty = if seen_entity_ids.is_empty() {
            0.0
        } else {
            new_entity_ids.len() as f64 / seen_entity_ids.len() as f64
        };

        if new_entity_ids.is_empty() || novelty < rmh_config.convergence_threshold {
            converged = true;
            break;
        }

        // Add newly discovered entities as search results with decay score
        for new_id in &new_entity_ids {
            if all_results.len() >= rmh_config.max_explore_entities {
                break;
            }
            if !all_results.contains_key(new_id) {
                all_results.insert(
                    *new_id,
                    SearchResult {
                        id: *new_id,
                        source: "datalog_discovery".to_string(),
                        content: String::new(), // Will be enriched by caller
                        score: 0.5 * (1.0 / (1.0 + pass_idx as f64)), // Decay score by pass
                        result_type: "entity".to_string(),
                    },
                );
            }
            seen_entity_ids.insert(*new_id);
        }

        passes += 1;
    }

    // Boost warmth for all returned entities
    for eid in all_results.keys() {
        let _ = warmth::boost_on_access(
            storage,
            ctx,
            *eid,
            session_id,
            &DecayZone::Knowledge,
            rmh_config,
        )
        .await;
    }

    // Sort results by score descending
    let mut results: Vec<SearchResult> = all_results.into_values().collect();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(RecursiveExploreResult {
        sub_queries,
        results,
        passes,
        converged,
        derived_facts_count,
    })
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

    #[test]
    fn test_decompose_simple() {
        let subs = decompose_query("authentication system");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].query_text, "authentication system");
        assert_eq!(subs[0].reasoning, "original query");
    }

    #[test]
    fn test_decompose_conjunction() {
        let subs = decompose_query("authentication and authorization");
        assert!(subs.len() >= 2);
        assert_eq!(subs[0].query_text, "authentication and authorization"); // original
        // Should have split parts
        assert!(subs.iter().any(|s| s.query_text.contains("authentication")));
        assert!(subs.iter().any(|s| s.query_text.contains("authorization")));
    }

    #[test]
    fn test_decompose_or_conjunction() {
        let subs = decompose_query("caching or indexing strategies");
        assert!(subs.len() >= 2);
        assert_eq!(subs[0].query_text, "caching or indexing strategies"); // original
        assert!(
            subs.iter()
                .any(|s| s.query_text == "caching" || s.query_text.contains("caching"))
        );
    }

    #[test]
    fn test_decompose_caps_at_5() {
        let subs = decompose_query("a and b and c and d and e and f and g");
        assert!(subs.len() <= 5);
    }

    #[test]
    fn test_decompose_short_fragments_skipped() {
        let subs = decompose_query("the and fox");
        // "the" is 3 chars which is not > 3, so it should be skipped
        let non_original: Vec<_> = subs.iter().skip(1).collect();
        for sq in non_original {
            assert!(sq.query_text.len() > 3);
        }
    }

    #[test]
    fn test_decompose_quoted_phrases() {
        let subs = decompose_query(r#"search for "memory system" and "graph traversal""#);
        // Should have original + conjunction splits + quoted phrases
        assert!(
            subs.iter()
                .any(|s| s.query_text == "memory system" || s.query_text.contains("memory system"))
        );
    }

    #[test]
    fn test_decompose_no_duplicates() {
        let subs = decompose_query("authentication and authentication");
        // Original + only one split (second is duplicate of first split)
        let texts: Vec<_> = subs.iter().map(|s| s.query_text.to_lowercase()).collect();
        let unique: HashSet<_> = texts.iter().collect();
        assert_eq!(
            texts.len(),
            unique.len(),
            "should not have duplicate sub-queries"
        );
    }

    #[test]
    #[should_panic(expected = "query must not be empty")]
    fn test_decompose_empty_panics() {
        decompose_query("");
    }

    #[tokio::test]
    async fn test_explore_empty_session() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let rmh = RmhConfig::default();
        let dl = DatalogConfig::default();

        let result = explore(&storage, &ctx, sid, "test query", None, &rmh, &dl)
            .await
            .unwrap();
        assert_eq!(result.passes, 1);
        assert!(result.results.is_empty());
        assert_eq!(result.sub_queries.len(), 1);
    }

    #[tokio::test]
    async fn test_explore_with_entities() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Add some entities
        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();

        storage
            .entity_put(
                &ctx,
                &EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: e1,
                    session_id: sid,
                    entity_name: "auth module".into(),
                    entity_type: "concept".into(),
                    source_fold_id: None,
                    context_snippet: "authentication module".into(),
                    entity_embedding: None,
                    confidence: 0.9,
                    state: MemoryState::Active,
                    created_at: chrono::Utc::now(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Add edge: e1 -> e2
        storage
            .edge_co_occurs(&ctx, e1, e2, sid, 1.0)
            .await
            .unwrap();

        let rmh = RmhConfig::default();
        let dl = DatalogConfig::default();

        let result = explore(&storage, &ctx, sid, "auth", None, &rmh, &dl)
            .await
            .unwrap();
        // Should have at least 1 pass
        assert!(result.passes >= 1);
    }

    #[tokio::test]
    async fn test_explore_converges_on_empty_graph() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let rmh = RmhConfig {
            max_explore_passes: 5,
            ..Default::default()
        };
        let dl = DatalogConfig::default();

        // Empty graph should converge quickly
        let result = explore(&storage, &ctx, sid, "test", None, &rmh, &dl)
            .await
            .unwrap();
        assert!(
            result.passes <= 2,
            "expected at most 2 passes on empty graph, got {}",
            result.passes
        );
    }

    #[tokio::test]
    async fn test_explore_returns_sorted_by_score() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let rmh = RmhConfig::default();
        let dl = DatalogConfig::default();

        let result = explore(&storage, &ctx, sid, "anything", None, &rmh, &dl)
            .await
            .unwrap();
        // Verify results are sorted by score descending
        for pair in result.results.windows(2) {
            assert!(
                pair[0].score >= pair[1].score,
                "results not sorted: {} < {}",
                pair[0].score,
                pair[1].score
            );
        }
    }

    #[tokio::test]
    async fn test_explore_rejects_empty_query() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let rmh = RmhConfig::default();
        let dl = DatalogConfig::default();

        let result = explore(&storage, &ctx, sid, "", None, &rmh, &dl).await;
        assert!(result.is_err(), "empty query should produce an error");
    }

    #[tokio::test]
    async fn test_explore_sub_queries_match_decomposition() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let rmh = RmhConfig::default();
        let dl = DatalogConfig::default();

        let query = "authentication and authorization";
        let result = explore(&storage, &ctx, sid, query, None, &rmh, &dl)
            .await
            .unwrap();

        // The sub_queries in the result should match decompose_query output
        let expected = decompose_query(query);
        assert_eq!(result.sub_queries.len(), expected.len());
        for (actual, exp) in result.sub_queries.iter().zip(expected.iter()) {
            assert_eq!(actual.query_text, exp.query_text);
        }
    }
}
