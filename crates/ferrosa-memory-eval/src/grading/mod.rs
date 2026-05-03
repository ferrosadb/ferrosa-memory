//! Grading pipeline for ferrosa-memory-eval.
//!
//! Submodules:
//! - `programmatic`: Schema, sequence, field assertion grading (T-005)
//! - `claim_rubric`: Claim-based rubric grading with anti-false-pass (T-004)
//! - `tool_usage`: Tool usage efficiency grading (T-033)
//!
//! This module also provides MCP quality score computation (T-034).

pub mod claim_rubric;
pub mod programmatic;
pub mod tool_usage;

use crate::report::McpQualityScores;

/// Default dimension weights for the MCP quality composite score.
/// Order: accuracy, completeness, relevance, clarity, reasoning.
const DEFAULT_WEIGHTS: [f64; 5] = [0.25, 0.20, 0.20, 0.15, 0.20];

/// Compute MCP quality scores from grading component results (T-034).
///
/// Maps grading results to 0.0-1.0 internal scale:
/// - Accuracy = programmatic score (0.0-1.0)
/// - Completeness = claim score (0.0-1.0)
/// - Relevance = tool usage efficiency (0.0-1.0)
/// - Clarity = schema_valid as f64 (1.0 if valid, 0.0 otherwise)
/// - Reasoning = judge confidence if available, else average of programmatic + claim scores
/// - Composite = weighted mean of all 5 dimensions
///
/// All inputs are expected in 0.0-1.0 range. Use `McpQualityScores::to_display_scale`
/// to convert to the 1-5 display range (EF25).
pub fn compute_mcp_quality(
    programmatic_score: f64,
    claim_score: Option<f64>,
    tool_efficiency: f64,
    schema_valid: bool,
    judge_confidence: Option<f64>,
) -> McpQualityScores {
    let accuracy = programmatic_score.clamp(0.0, 1.0);
    let completeness = claim_score.unwrap_or(programmatic_score).clamp(0.0, 1.0);
    let relevance = tool_efficiency.clamp(0.0, 1.0);
    let clarity = if schema_valid { 1.0 } else { 0.0 };
    let reasoning = match judge_confidence {
        Some(conf) => conf.clamp(0.0, 1.0),
        None => {
            // Average of available scores: programmatic + claim (if present)
            match claim_score {
                Some(cs) => ((programmatic_score + cs) / 2.0).clamp(0.0, 1.0),
                None => programmatic_score.clamp(0.0, 1.0),
            }
        }
    };

    let dimensions = [accuracy, completeness, relevance, clarity, reasoning];
    let composite = weighted_mean(&dimensions, &DEFAULT_WEIGHTS);

    McpQualityScores {
        accuracy,
        completeness,
        relevance,
        clarity,
        reasoning,
        composite,
    }
}

/// Compute MCP quality scores with custom weights.
///
/// Weights order: `[accuracy, completeness, relevance, clarity, reasoning]`.
/// Weights are normalized internally (do not need to sum to 1.0).
pub fn compute_mcp_quality_weighted(
    programmatic_score: f64,
    claim_score: Option<f64>,
    tool_efficiency: f64,
    schema_valid: bool,
    judge_confidence: Option<f64>,
    weights: &[f64; 5],
) -> McpQualityScores {
    let mut scores = compute_mcp_quality(
        programmatic_score,
        claim_score,
        tool_efficiency,
        schema_valid,
        judge_confidence,
    );
    let dimensions = [
        scores.accuracy,
        scores.completeness,
        scores.relevance,
        scores.clarity,
        scores.reasoning,
    ];
    scores.composite = weighted_mean(&dimensions, weights);
    scores
}

/// Weighted mean of values with weights. Normalizes weights internally.
fn weighted_mean(values: &[f64], weights: &[f64]) -> f64 {
    assert_eq!(
        values.len(),
        weights.len(),
        "values and weights must have same length"
    );

    let total_weight: f64 = weights.iter().sum();
    if total_weight == 0.0 {
        return 0.0;
    }

    let weighted_sum: f64 = values.iter().zip(weights.iter()).map(|(v, w)| v * w).sum();

    weighted_sum / total_weight
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f64 = 1e-10;

    // ── T-034: MCP quality score mapping ─────────────────────────

    #[test]
    fn mcp_quality_perfect_scores() {
        let scores = compute_mcp_quality(1.0, Some(1.0), 1.0, true, Some(1.0));

        assert!((scores.accuracy - 1.0).abs() < EPSILON);
        assert!((scores.completeness - 1.0).abs() < EPSILON);
        assert!((scores.relevance - 1.0).abs() < EPSILON);
        assert!((scores.clarity - 1.0).abs() < EPSILON);
        assert!((scores.reasoning - 1.0).abs() < EPSILON);
        assert!(
            (scores.composite - 1.0).abs() < EPSILON,
            "Perfect inputs should give composite 1.0, got {}",
            scores.composite
        );
    }

    #[test]
    fn mcp_quality_zero_scores() {
        let scores = compute_mcp_quality(0.0, Some(0.0), 0.0, false, Some(0.0));

        assert!((scores.accuracy - 0.0).abs() < EPSILON);
        assert!((scores.completeness - 0.0).abs() < EPSILON);
        assert!((scores.relevance - 0.0).abs() < EPSILON);
        assert!((scores.clarity - 0.0).abs() < EPSILON);
        assert!((scores.reasoning - 0.0).abs() < EPSILON);
        assert!(
            (scores.composite - 0.0).abs() < EPSILON,
            "Zero inputs should give composite 0.0, got {}",
            scores.composite
        );
    }

    #[test]
    fn mcp_quality_accuracy_maps_from_programmatic() {
        let scores = compute_mcp_quality(0.75, Some(0.5), 0.8, true, Some(0.6));
        assert!(
            (scores.accuracy - 0.75).abs() < EPSILON,
            "Accuracy should be programmatic_score"
        );
    }

    #[test]
    fn mcp_quality_completeness_maps_from_claim() {
        let scores = compute_mcp_quality(0.75, Some(0.5), 0.8, true, Some(0.6));
        assert!(
            (scores.completeness - 0.5).abs() < EPSILON,
            "Completeness should be claim_score"
        );
    }

    #[test]
    fn mcp_quality_relevance_maps_from_efficiency() {
        let scores = compute_mcp_quality(0.75, Some(0.5), 0.8, true, Some(0.6));
        assert!(
            (scores.relevance - 0.8).abs() < EPSILON,
            "Relevance should be tool_efficiency"
        );
    }

    #[test]
    fn mcp_quality_clarity_maps_from_schema() {
        let valid = compute_mcp_quality(0.75, Some(0.5), 0.8, true, Some(0.6));
        assert!(
            (valid.clarity - 1.0).abs() < EPSILON,
            "Clarity should be 1.0 when schema valid"
        );

        let invalid = compute_mcp_quality(0.75, Some(0.5), 0.8, false, Some(0.6));
        assert!(
            (invalid.clarity - 0.0).abs() < EPSILON,
            "Clarity should be 0.0 when schema invalid"
        );
    }

    #[test]
    fn mcp_quality_reasoning_uses_judge_when_available() {
        let scores = compute_mcp_quality(0.75, Some(0.5), 0.8, true, Some(0.9));
        assert!(
            (scores.reasoning - 0.9).abs() < EPSILON,
            "Reasoning should be judge_confidence when available"
        );
    }

    #[test]
    fn mcp_quality_reasoning_falls_back_to_average() {
        // No judge: reasoning = (programmatic + claim) / 2
        let scores = compute_mcp_quality(0.8, Some(0.6), 0.9, true, None);
        let expected = (0.8 + 0.6) / 2.0; // 0.7
        assert!(
            (scores.reasoning - expected).abs() < EPSILON,
            "Reasoning should be (prog+claim)/2 = {}, got {}",
            expected,
            scores.reasoning
        );
    }

    #[test]
    fn mcp_quality_reasoning_falls_back_to_programmatic_when_no_claims() {
        // No judge, no claims: reasoning = programmatic
        let scores = compute_mcp_quality(0.85, None, 0.9, true, None);
        assert!(
            (scores.reasoning - 0.85).abs() < EPSILON,
            "Reasoning should fall back to programmatic_score when no judge or claims"
        );
    }

    #[test]
    fn mcp_quality_composite_is_weighted_mean() {
        // Known inputs: accuracy=0.8, completeness=0.6, relevance=0.9, clarity=1.0, reasoning=0.7
        let scores = compute_mcp_quality(0.8, Some(0.6), 0.9, true, Some(0.7));

        // Default weights: [0.25, 0.20, 0.20, 0.15, 0.20]
        let expected = (0.8 * 0.25 + 0.6 * 0.20 + 0.9 * 0.20 + 1.0 * 0.15 + 0.7 * 0.20) / 1.0;
        assert!(
            (scores.composite - expected).abs() < EPSILON,
            "Composite should be weighted mean = {}, got {}",
            expected,
            scores.composite
        );
    }

    #[test]
    fn mcp_quality_display_scale_mapping() {
        // Verify internal 0-1 maps correctly to display 1-5
        let scores = compute_mcp_quality(0.8, Some(0.6), 0.9, true, Some(0.7));

        let display_accuracy = McpQualityScores::to_display_scale(scores.accuracy);
        // 0.8 -> 1.0 + 0.8*4.0 = 4.2
        assert!(
            (display_accuracy - 4.2).abs() < EPSILON,
            "0.8 should display as 4.2, got {}",
            display_accuracy
        );

        let display_composite = McpQualityScores::to_display_scale(scores.composite);
        assert!(
            (1.0..=5.0).contains(&display_composite),
            "Display composite should be in 1-5 range, got {}",
            display_composite
        );
    }

    #[test]
    fn mcp_quality_passing_threshold() {
        // Target threshold: 3.5/5.0 = 0.625 internal
        // Create a scenario that should pass
        let passing = compute_mcp_quality(0.9, Some(0.8), 0.85, true, Some(0.7));
        let display = McpQualityScores::to_display_scale(passing.composite);
        assert!(
            display >= 3.5,
            "Good scores should pass 3.5/5.0 threshold, got {:.1}/5.0",
            display
        );

        // Create a scenario that should fail
        let failing = compute_mcp_quality(0.3, Some(0.2), 0.4, false, Some(0.1));
        let display = McpQualityScores::to_display_scale(failing.composite);
        assert!(
            display < 3.5,
            "Poor scores should fail 3.5/5.0 threshold, got {:.1}/5.0",
            display
        );
    }

    #[test]
    fn mcp_quality_completeness_defaults_to_programmatic_when_no_claims() {
        let scores = compute_mcp_quality(0.7, None, 0.8, true, Some(0.6));
        assert!(
            (scores.completeness - 0.7).abs() < EPSILON,
            "Completeness should default to programmatic_score when no claims, got {}",
            scores.completeness
        );
    }

    #[test]
    fn mcp_quality_clamps_out_of_range() {
        // Input scores should be clamped to 0-1
        let scores = compute_mcp_quality(1.5, Some(1.2), 1.1, true, Some(1.3));
        assert!((scores.accuracy - 1.0).abs() < EPSILON);
        assert!((scores.completeness - 1.0).abs() < EPSILON);
        assert!((scores.relevance - 1.0).abs() < EPSILON);
        assert!((scores.reasoning - 1.0).abs() < EPSILON);
    }

    #[test]
    fn mcp_quality_custom_weights() {
        let scores = compute_mcp_quality_weighted(
            0.8,
            Some(0.6),
            0.9,
            true,
            Some(0.7),
            &[1.0, 1.0, 1.0, 1.0, 1.0], // equal weights
        );

        // Equal weights: simple average
        let expected = (0.8 + 0.6 + 0.9 + 1.0 + 0.7) / 5.0;
        assert!(
            (scores.composite - expected).abs() < EPSILON,
            "Equal weights should give simple average = {}, got {}",
            expected,
            scores.composite
        );
    }

    // ── Weighted mean utility ────────────────────────────────────

    #[test]
    fn mcp_quality_weighted_mean_normalizes() {
        // Weights that don't sum to 1.0
        let result = weighted_mean(&[0.8, 0.6], &[2.0, 3.0]);
        // (0.8*2 + 0.6*3) / (2+3) = (1.6 + 1.8) / 5 = 0.68
        assert!(
            (result - 0.68).abs() < EPSILON,
            "Weighted mean should normalize weights, got {}",
            result
        );
    }

    #[test]
    fn mcp_quality_weighted_mean_zero_weights() {
        let result = weighted_mean(&[0.5, 0.5], &[0.0, 0.0]);
        assert!(
            (result - 0.0).abs() < EPSILON,
            "Zero total weight should return 0.0"
        );
    }
}
