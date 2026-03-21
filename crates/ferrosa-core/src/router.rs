//! SRLM-inspired tool routing layer.
//!
//! Selects the optimal retrieval strategy before invoking storage. Implements
//! the finding that program selection is the primary performance driver (SRLM).
//!
//! ## Decision tree
//!
//! 1. Content hash in memo_cache? -> MemoHit
//! 2. Named entity search? -> Phonetic + ANN on entity_store
//! 3. Plan hierarchy traversal? -> BTreeRange on plan_state
//! 4. Fold-level semantic search? -> HnswAnn on trajectory_folds
//! 5. Graph multi-hop? -> CypherTraversal
//! 6. Default -> HnswAnn on trajectory_folds
//!
//! ## Routing signals
//!
//! - Entity name presence (triggers phonetic path)
//! - Task complexity classification (simple/linear/quadratic)
//! - Query keywords (plan/fold/entity hints)

use serde::Serialize;

/// Retrieval strategy selected by the router.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    MemoHit,
    Phonetic,
    BTreeRange,
    HnswAnn,
    CypherTraversal,
}

/// Task complexity classification per RLM paper taxonomy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    Simple,
    Linear,
    Quadratic,
}

/// Context available to the router for making a routing decision.
pub struct RoutingContext<'a> {
    pub query_text: &'a str,
    pub has_entity_name: bool,
    pub has_content_hash: bool,
    pub task_complexity: TaskComplexity,
}

/// Result of a routing decision.
#[derive(Debug)]
pub struct RoutingDecision {
    pub strategy: Strategy,
    pub fallback: Option<Strategy>,
    pub k: usize,
    pub include_raw: bool,
}

/// Route a query to the optimal retrieval strategy.
///
/// This is a lightweight in-memory decision tree (< 1ms overhead).
/// The decision is based on query characteristics, not on database lookups.
pub fn route(ctx: &RoutingContext) -> RoutingDecision {
    // 1. Memo hit path (caller already checked hash)
    if ctx.has_content_hash {
        return RoutingDecision {
            strategy: Strategy::MemoHit,
            fallback: Some(Strategy::HnswAnn),
            k: 1,
            include_raw: false,
        };
    }

    // 2. Entity name detected
    if ctx.has_entity_name {
        return RoutingDecision {
            strategy: Strategy::Phonetic,
            fallback: Some(Strategy::HnswAnn),
            k: default_k(&ctx.task_complexity),
            include_raw: false,
        };
    }

    // 3. Plan-related keywords
    let lower = ctx.query_text.to_lowercase();
    if lower.contains("plan")
        || lower.contains("subtask")
        || lower.contains("goal")
        || lower.contains("depth")
    {
        return RoutingDecision {
            strategy: Strategy::BTreeRange,
            fallback: Some(Strategy::HnswAnn),
            k: default_k(&ctx.task_complexity),
            include_raw: false,
        };
    }

    // 4. Graph traversal hints
    if lower.contains("related to")
        || lower.contains("connected")
        || lower.contains("co-occurs")
        || lower.contains("mentioned in")
    {
        return RoutingDecision {
            strategy: Strategy::CypherTraversal,
            fallback: Some(Strategy::HnswAnn),
            k: default_k(&ctx.task_complexity),
            include_raw: false,
        };
    }

    // 5. Default: HNSW ANN on trajectory folds
    RoutingDecision {
        strategy: Strategy::HnswAnn,
        fallback: None,
        k: default_k(&ctx.task_complexity),
        include_raw: matches!(ctx.task_complexity, TaskComplexity::Quadratic),
    }
}

/// Default `k` (number of results) scaled by task complexity.
/// Per Fractured CoT axis defaults.
fn default_k(complexity: &TaskComplexity) -> usize {
    match complexity {
        TaskComplexity::Simple => 3,
        TaskComplexity::Linear => 5,
        TaskComplexity::Quadratic => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_memo_hit() {
        let decision = route(&RoutingContext {
            query_text: "anything",
            has_entity_name: false,
            has_content_hash: true,
            task_complexity: TaskComplexity::Simple,
        });
        assert_eq!(decision.strategy, Strategy::MemoHit);
        assert_eq!(decision.fallback, Some(Strategy::HnswAnn));
    }

    #[test]
    fn route_entity_name() {
        let decision = route(&RoutingContext {
            query_text: "find Alice",
            has_entity_name: true,
            has_content_hash: false,
            task_complexity: TaskComplexity::Simple,
        });
        assert_eq!(decision.strategy, Strategy::Phonetic);
    }

    #[test]
    fn route_plan_keywords() {
        let decision = route(&RoutingContext {
            query_text: "get the current plan for this subtask",
            has_entity_name: false,
            has_content_hash: false,
            task_complexity: TaskComplexity::Linear,
        });
        assert_eq!(decision.strategy, Strategy::BTreeRange);
    }

    #[test]
    fn route_graph_traversal() {
        let decision = route(&RoutingContext {
            query_text: "what entities are related to this concept",
            has_entity_name: false,
            has_content_hash: false,
            task_complexity: TaskComplexity::Linear,
        });
        assert_eq!(decision.strategy, Strategy::CypherTraversal);
    }

    #[test]
    fn route_default_hnsw() {
        let decision = route(&RoutingContext {
            query_text: "search for relevant context about the API",
            has_entity_name: false,
            has_content_hash: false,
            task_complexity: TaskComplexity::Simple,
        });
        assert_eq!(decision.strategy, Strategy::HnswAnn);
        assert_eq!(decision.k, 3);
        assert!(!decision.include_raw);
    }

    #[test]
    fn quadratic_complexity_includes_raw() {
        let decision = route(&RoutingContext {
            query_text: "deep analysis of this problem",
            has_entity_name: false,
            has_content_hash: false,
            task_complexity: TaskComplexity::Quadratic,
        });
        assert_eq!(decision.strategy, Strategy::HnswAnn);
        assert_eq!(decision.k, 10);
        assert!(decision.include_raw);
    }

    #[test]
    fn k_scales_with_complexity() {
        assert_eq!(default_k(&TaskComplexity::Simple), 3);
        assert_eq!(default_k(&TaskComplexity::Linear), 5);
        assert_eq!(default_k(&TaskComplexity::Quadratic), 10);
    }
}
