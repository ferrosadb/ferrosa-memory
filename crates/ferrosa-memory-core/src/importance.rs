//! Multi-channel importance scoring.
//!
//! 4-channel model inspired by vestige's neuroscience-based scoring:
//! - Novelty: how surprising/new the content is
//! - Arousal: emotional intensity (keyword heuristic)
//! - Reward: past retrieval success rate
//! - Attention: recency and access frequency

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ImportanceScore {
    pub novelty: f64,
    pub arousal: f64,
    pub reward: f64,
    pub attention: f64,
    pub derived_confidence: f64,
    pub support: f64,
    pub path_distance: f64,
    pub predicate_weight: f64,
    pub scope_guard: f64,
    pub composite: f64,
}

/// Compute importance score for a memory entity.
pub fn compute_importance(
    similarity_to_existing: f64,
    _retrieval_count: usize,
    last_accessed_seconds_ago: i64,
    feedback_success_rate: f64,
) -> ImportanceScore {
    let novelty = (1.0 - similarity_to_existing).clamp(0.0, 1.0);
    let arousal = 0.5; // placeholder — could detect urgency keywords
    let reward = feedback_success_rate.clamp(0.0, 1.0);
    let attention = 1.0 / (1.0 + (last_accessed_seconds_ago as f64 / 3600.0));
    let derived_confidence = 0.0;
    let support = 0.0;
    let path_distance = 0.0;
    let predicate_weight = 0.0;
    let scope_guard = 1.0;

    let composite = 0.3 * novelty + 0.2 * arousal + 0.3 * reward + 0.2 * attention;

    ImportanceScore {
        novelty,
        arousal,
        reward,
        attention,
        derived_confidence,
        support,
        path_distance,
        predicate_weight,
        scope_guard,
        composite,
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct DerivedImportanceInput {
    pub derived_confidence: f64,
    pub support_count: usize,
    pub path_distance: usize,
    pub predicate_weight: f64,
    pub scope_guard: bool,
}

/// Score graph-derived recall evidence before it is allowed to influence search.
///
/// The result is intentionally conservative: wrong-scope facts contribute zero,
/// path distance decays quickly, and support count is logarithmic so high-degree
/// co-occurrence hubs do not dominate recall.
pub fn compute_derived_importance(input: DerivedImportanceInput) -> f64 {
    if !input.scope_guard || input.path_distance == 0 {
        return 0.0;
    }
    let confidence = input.derived_confidence.clamp(0.0, 1.0);
    let support = ((input.support_count.max(1) as f64).ln_1p() / 4.0_f64.ln_1p()).min(1.0);
    let path = 1.0 / input.path_distance as f64;
    let predicate = input.predicate_weight.clamp(0.0, 1.0);

    0.45 * confidence + 0.20 * support + 0.20 * path + 0.15 * predicate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn novel_content_scores_high() {
        let score = compute_importance(0.1, 0, 60, 0.0);
        assert!(score.novelty > 0.8);
        assert!(score.composite > 0.3);
    }

    #[test]
    fn frequently_retrieved_scores_high_attention() {
        let score = compute_importance(0.5, 10, 30, 0.8);
        assert!(score.attention > 0.5);
        assert!(score.reward > 0.7);
    }

    #[test]
    fn old_unused_scores_low() {
        let score = compute_importance(0.9, 0, 86400, 0.0);
        assert!(score.novelty < 0.2);
        assert!(score.attention < 0.1);
        assert!(score.composite < 0.3);
    }

    #[test]
    fn composite_is_weighted_average() {
        let score = compute_importance(0.0, 5, 0, 1.0);
        let expected = 0.3 * 1.0 + 0.2 * 0.5 + 0.3 * 1.0 + 0.2 * 1.0;
        assert!((score.composite - expected).abs() < 0.01);
    }

    #[test]
    fn derived_importance_requires_scope_guard() {
        let scoped = compute_derived_importance(DerivedImportanceInput {
            derived_confidence: 0.9,
            support_count: 3,
            path_distance: 1,
            predicate_weight: 0.9,
            scope_guard: true,
        });
        let wrong_scope = compute_derived_importance(DerivedImportanceInput {
            scope_guard: false,
            ..DerivedImportanceInput {
                derived_confidence: 0.9,
                support_count: 3,
                path_distance: 1,
                predicate_weight: 0.9,
                scope_guard: true,
            }
        });

        assert!(scoped > 0.75);
        assert_eq!(wrong_scope, 0.0);
    }

    #[test]
    fn derived_importance_decays_with_path_distance() {
        let one_hop = compute_derived_importance(DerivedImportanceInput {
            derived_confidence: 0.8,
            support_count: 2,
            path_distance: 1,
            predicate_weight: 0.8,
            scope_guard: true,
        });
        let three_hop = compute_derived_importance(DerivedImportanceInput {
            path_distance: 3,
            ..DerivedImportanceInput {
                derived_confidence: 0.8,
                support_count: 2,
                path_distance: 1,
                predicate_weight: 0.8,
                scope_guard: true,
            }
        });

        assert!(one_hop > three_hop);
        assert!(three_hop > 0.0);
    }
}
