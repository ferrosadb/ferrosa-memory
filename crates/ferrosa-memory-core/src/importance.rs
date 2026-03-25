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

    let composite = 0.3 * novelty + 0.2 * arousal + 0.3 * reward + 0.2 * attention;

    ImportanceScore {
        novelty,
        arousal,
        reward,
        attention,
        composite,
    }
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
}
