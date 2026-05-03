//! Temporal / decay validation metrics.
//!
//! Measures whether fmem's warmth decay follows the Ebbinghaus curve,
//! whether content-type-specific decay rates behave correctly, and
//! whether threshold-based forgetting preserves the right entities.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Decay curve accuracy
// ---------------------------------------------------------------------------

/// Expected Ebbinghaus warmth values at fixed checkpoints.
pub const EBINGHAUS_CHECKPOINTS: &[(f64, f64)] = &[
    (0.0, 1.0),       // t=0h
    (1.0, 0.55),      // 1h
    (24.0, 0.21),     // 24h
    (168.0, 0.05),    // 7d
];

/// Decay profile for a content category.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct DecayProfile {
    pub category: &'static str,
    pub half_life_hours: f64,
}

/// Pre-defined decay profiles used by fmem.
pub const DECAY_PROFILES: &[DecayProfile] = &[
    DecayProfile {
        category: "bug",
        half_life_hours: 24.0,
    },
    DecayProfile {
        category: "architecture",
        half_life_hours: 168.0,
    },
    DecayProfile {
        category: "decision",
        half_life_hours: 72.0,
    },
    DecayProfile {
        category: "transient",
        half_life_hours: 12.0,
    },
];

/// Find the decay profile for a category.
pub fn profile_for(category: &str) -> Option<&DecayProfile> {
    DECAY_PROFILES.iter().find(|p| p.category == category)
}

/// Score for decay curve accuracy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecayAccuracy {
    pub mse: f64,
    pub max_deviation: f64,
    pub passed: bool,
}

/// Validate that measured warmth values track the Ebbinghaus curve.
///
/// `measured` is a Vec of (elapsed_hours, warmth) pairs.
/// `expected` defaults to `EBINGHAUS_CHECKPOINTS`.
pub fn decay_curve_accuracy(
    measured: &[(f64, f64)],
    expected: Option<&[(f64, f64)]>,
) -> DecayAccuracy {
    let expected = expected.unwrap_or(EBINGHAUS_CHECKPOINTS);

    let mut sse: f64 = 0.0;
    let mut max_dev: f64 = 0.0;

    for ((_elapsed, actual), (_, exp)) in measured.iter().zip(expected.iter()) {
        let dev: f64 = (actual - exp).abs();
        sse += dev * dev;
        if dev > max_dev {
            max_dev = dev;
        }
    }

    let n = measured.len().max(1) as f64;
    let mse = sse / n;

    DecayAccuracy {
        mse,
        max_deviation: max_dev,
        passed: max_dev < 0.05,
    }
}

// ---------------------------------------------------------------------------
// Forgetting validation
// ---------------------------------------------------------------------------

/// Score for threshold-based forgetting correctness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForgettingScore {
    pub expected_retained: usize,
    pub actual_retained: usize,
    pub expected_pruned: usize,
    pub actual_pruned: usize,
    pub precision: f64, // fraction of retained that should have been retained
    pub passed: bool,
}

/// Validate that pruning at a threshold removes exactly the right entities.
///
/// `entities`: Vec of (id, current_warmth) for all seeded entities.
/// `threshold`: warmth threshold for pruning.
/// `pruned_ids`: IDs that the system actually removed.
pub fn forgetting_validation(
    entities: &[(String, f64)],
    threshold: f64,
    pruned_ids: &[String],
) -> ForgettingScore {
    let entity_set: HashSet<&str> = entities.iter().map(|(id, _)| id.as_str()).collect();
    let pruned_set: HashSet<&str> = pruned_ids.iter().map(|s| s.as_str()).collect();

    let expected_pruned: HashSet<&str> = entities
        .iter()
        .filter(|(_, w)| *w < threshold)
        .map(|(id, _)| id.as_str())
        .collect();
    let expected_retained: HashSet<&str> = entities
        .iter()
        .filter(|(_, w)| *w >= threshold)
        .map(|(id, _)| id.as_str())
        .collect();

    let actual_pruned = pruned_set.len();
    let expected_pruned_count = expected_pruned.len();
    let actual_retained = entity_set.len() - actual_pruned;
    let expected_retained_count = expected_retained.len();

    let precision = if actual_retained == 0 {
        1.0
    } else {
        let correctly_retained = expected_retained
            .intersection(
                &entity_set
                    .difference(&pruned_set)
                    .copied()
                    .collect::<HashSet<_>>(),
            )
            .count() as f64;
        correctly_retained / actual_retained as f64
    };

    ForgettingScore {
        expected_retained: expected_retained_count,
        actual_retained,
        expected_pruned: expected_pruned_count,
        actual_pruned,
        precision,
        passed: pruned_set == expected_pruned && actual_retained == expected_retained_count,
    }
}

// ---------------------------------------------------------------------------
// Content-type-specific decay comparison
// ---------------------------------------------------------------------------

/// Compare decay rates across content categories at a fixed elapsed time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CategoryDecayComparison {
    pub category: String,
    pub half_life_hours: f64,
    pub elapsed_hours: f64,
    pub expected_warmth: f64,
    pub actual_warmth: f64,
    pub deviation: f64,
}

/// Compute expected warmth for a single decay step using exponential decay.
pub fn expected_warmth(initial: f64, half_life: f64, elapsed: f64) -> f64 {
    let lambda = half_life.ln() / half_life; // approximate
    initial * (-lambda * elapsed).exp()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ebbinghaus_perfect_match() {
        let measured = vec![(0.0, 1.0), (1.0, 0.55), (24.0, 0.21), (168.0, 0.05)];
        let score = decay_curve_accuracy(&measured, None);
        assert!(score.mse < 1e-10);
        assert!(score.passed);
    }

    #[test]
    fn ebbinghaus_slight_drift_still_passes() {
        let measured = vec![(0.0, 1.0), (1.0, 0.56), (24.0, 0.22), (168.0, 0.06)];
        let score = decay_curve_accuracy(&measured, None);
        assert!(score.max_deviation <= 0.05);
        assert!(score.passed);
    }

    #[test]
    fn ebbinghaus_large_drift_fails() {
        let measured = vec![(0.0, 1.0), (1.0, 0.9), (24.0, 0.8), (168.0, 0.7)];
        let score = decay_curve_accuracy(&measured, None);
        assert!(!score.passed);
    }

    #[test]
    fn profile_lookup_hits() {
        assert_eq!(profile_for("bug").unwrap().half_life_hours, 24.0);
        assert_eq!(profile_for("architecture").unwrap().half_life_hours, 168.0);
    }

    #[test]
    fn profile_lookup_miss() {
        assert!(profile_for("nonexistent").is_none());
    }

    #[test]
    fn forgetting_perfect_threshold() {
        let entities = vec![
            ("a".to_string(), 0.9),
            ("b".to_string(), 0.85),
            ("c".to_string(), 0.1),
            ("d".to_string(), 0.05),
        ];
        let pruned = vec!["c".to_string(), "d".to_string()];
        let score = forgetting_validation(&entities, 0.5, &pruned);
        assert!(score.passed);
        assert_eq!(score.expected_retained, 2);
        assert_eq!(score.actual_retained, 2);
    }

    #[test]
    fn forgetting_false_positive_fails() {
        let entities = vec![
            ("a".to_string(), 0.9),
            ("b".to_string(), 0.1),
        ];
        let pruned = vec!["a".to_string()]; // wrong: a is above threshold
        let score = forgetting_validation(&entities, 0.5, &pruned);
        assert!(!score.passed);
    }

    #[test]
    fn category_decay_bug_faster_than_arch() {
        let bug = expected_warmth(1.0, 24.0, 24.0);
        let arch = expected_warmth(1.0, 168.0, 24.0);
        assert!(bug < arch, "bug should decay faster than architecture over 24h");
    }

    #[test]
    fn category_decay_transient_fastest() {
        let transient = expected_warmth(1.0, 12.0, 12.0);
        let bug = expected_warmth(1.0, 24.0, 12.0);
        assert!(transient < bug, "transient should decay faster than bug over 12h");
    }
}
