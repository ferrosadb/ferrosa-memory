//! Cross-Domain Personalization metrics.
//!
//! Measures whether fmem transfers knowledge across domains and infers latent
//! user preferences from varied phrasings.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Latent preference inference
// ---------------------------------------------------------------------------

/// Score for latent preference inference quality.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreferenceScore {
    pub jaccard: f64,
    pub retrieved_count: usize,
    pub expected_count: usize,
}

/// Ingest preference facts across varied phrasings, then query canonical form.
///
/// `preference_facts`: list of (phrasing, canonical_topic) pairs.
/// `retrieved`: list of phrasings returned by the query.
/// `canonical_topic`: the canonical query form.
///
/// Score: Jaccard of retrieved set vs all phrasings for that topic.
pub fn latent_preference_score(
    preference_facts: &[(String, String)],
    retrieved: &[String],
    canonical_topic: &str,
) -> PreferenceScore {
    // Collect all phrasings for this canonical topic
    let expected: HashSet<&str> = preference_facts
        .iter()
        .filter(|(_, topic)| topic == canonical_topic)
        .map(|(phrasing, _)| phrasing.as_str())
        .collect();

    let retrieved_set: HashSet<&str> = retrieved.iter().map(|s| s.as_str()).collect();

    let intersection = expected.intersection(&retrieved_set).count() as f64;
    let union = expected.union(&retrieved_set).count() as f64;

    let jaccard = if union == 0.0 {
        1.0
    } else {
        intersection / union
    };

    PreferenceScore {
        jaccard,
        retrieved_count: retrieved.len(),
        expected_count: expected.len(),
    }
}

// ---------------------------------------------------------------------------
// Style drift tracking
// ---------------------------------------------------------------------------

/// Score for cross-domain style/pattern transfer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyleDriftScore {
    /// Fraction of edge types shared across domains (higher = better transfer).
    pub shared_edge_ratio: f64,
    /// Jaccard of entity types used in each domain.
    pub entity_type_overlap: f64,
    /// Overall transfer score (average of the above).
    pub composite: f64,
}

/// Compare edge patterns between two project domains.
///
/// `old_domain_edges`: edge types from the source domain session.
/// `new_domain_edges`: edge types from the target domain session.
/// `old_entity_types`: entity types from source domain.
/// `new_entity_types`: entity types from target domain.
pub fn style_drift_score(
    old_domain_edges: &[String],
    new_domain_edges: &[String],
    old_entity_types: &[String],
    new_entity_types: &[String],
) -> StyleDriftScore {
    let old_edge_set: HashSet<&str> = old_domain_edges.iter().map(|s| s.as_str()).collect();
    let new_edge_set: HashSet<&str> = new_domain_edges.iter().map(|s| s.as_str()).collect();

    let shared_edges = old_edge_set.intersection(&new_edge_set).count() as f64;
    let union_edges = old_edge_set.union(&new_edge_set).count() as f64;
    let shared_edge_ratio = if union_edges == 0.0 {
        1.0
    } else {
        shared_edges / union_edges
    };

    let old_type_set: HashSet<&str> = old_entity_types.iter().map(|s| s.as_str()).collect();
    let new_type_set: HashSet<&str> = new_entity_types.iter().map(|s| s.as_str()).collect();

    let shared_types = old_type_set.intersection(&new_type_set).count() as f64;
    let union_types = old_type_set.union(&new_type_set).count() as f64;
    let entity_type_overlap = if union_types == 0.0 {
        1.0
    } else {
        shared_types / union_types
    };

    let composite = (shared_edge_ratio + entity_type_overlap) / 2.0;

    StyleDriftScore {
        shared_edge_ratio,
        entity_type_overlap,
        composite,
    }
}

// ---------------------------------------------------------------------------
// Cross-domain transfer scoring
// ---------------------------------------------------------------------------

/// Score for cross-domain knowledge transfer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransferScore {
    /// Fraction of source-domain findings retrieved in target domain.
    pub retrieval_ratio: f64,
    /// Fraction of source-domain patterns successfully mapped.
    pub mapping_ratio: f64,
    /// Overall composite score.
    pub composite: f64,
}

/// Score cross-domain knowledge transfer.
///
/// `source_findings`: findings from the source domain.
/// `retrieved_in_target`: findings from source domain that were retrieved when
///   queried in the target domain.
/// `mapped_patterns`: number of patterns explicitly mapped to the new domain.
pub fn cross_domain_transfer_score(
    source_findings: &[String],
    retrieved_in_target: &[String],
    mapped_patterns: usize,
) -> TransferScore {
    let source_set: HashSet<&str> = source_findings.iter().map(|s| s.as_str()).collect();
    let retrieved_set: HashSet<&str> = retrieved_in_target.iter().map(|s| s.as_str()).collect();

    let retrieval_ratio = if source_set.is_empty() {
        1.0
    } else {
        retrieved_set.intersection(&source_set).count() as f64 / source_set.len() as f64
    };

    let mapping_ratio = if source_set.is_empty() {
        1.0
    } else {
        (mapped_patterns as f64 / source_set.len() as f64).min(1.0)
    };

    let composite = (retrieval_ratio + mapping_ratio) / 2.0;

    TransferScore {
        retrieval_ratio,
        mapping_ratio,
        composite,
    }
}

// ---------------------------------------------------------------------------
// Preference cluster builder (for testing)
// ---------------------------------------------------------------------------

/// Build a synthetic preference cluster for testing.
pub fn build_preference_cluster(topic: &str, phrasings: &[&str]) -> Vec<(String, String)> {
    phrasings
        .iter()
        .map(|p| (p.to_string(), topic.to_string()))
        .collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Latent preference ─────────────────────────────────────────

    #[test]
    fn preference_cluster_perfect_retrieval() {
        let facts = vec![
            (
                "I like concise output".to_string(),
                "output_style".to_string(),
            ),
            (
                "prefer short answers".to_string(),
                "output_style".to_string(),
            ),
            ("hate verbosity".to_string(), "output_style".to_string()),
        ];

        let retrieved = vec![
            "I like concise output".to_string(),
            "prefer short answers".to_string(),
            "hate verbosity".to_string(),
        ];

        let score = latent_preference_score(&facts, &retrieved, "output_style");
        assert!((score.jaccard - 1.0).abs() < 1e-10);
        assert_eq!(score.expected_count, 3);
        assert_eq!(score.retrieved_count, 3);
    }

    #[test]
    fn preference_cluster_partial_retrieval() {
        let facts = vec![
            (
                "I like concise output".to_string(),
                "output_style".to_string(),
            ),
            (
                "prefer short answers".to_string(),
                "output_style".to_string(),
            ),
            ("hate verbosity".to_string(), "output_style".to_string()),
            ("keep it brief".to_string(), "output_style".to_string()),
        ];

        let retrieved = vec![
            "I like concise output".to_string(),
            "hate verbosity".to_string(),
        ];

        let score = latent_preference_score(&facts, &retrieved, "output_style");
        // Jaccard = 2 / 4 = 0.5
        assert!((score.jaccard - 0.5).abs() < 1e-10);
    }

    #[test]
    fn preference_cluster_different_topic_ignored() {
        let facts = vec![
            (
                "I like concise output".to_string(),
                "output_style".to_string(),
            ),
            ("prefer dark mode".to_string(), "theme".to_string()),
            ("hate verbosity".to_string(), "output_style".to_string()),
        ];

        let retrieved = vec![
            "I like concise output".to_string(),
            "hate verbosity".to_string(),
        ];

        let score = latent_preference_score(&facts, &retrieved, "output_style");
        // Only output_style facts count
        assert_eq!(score.expected_count, 2);
        assert!((score.jaccard - 1.0).abs() < 1e-10);
    }

    #[test]
    fn preference_cluster_empty_expected() {
        let facts: Vec<(String, String)> = vec![];
        let retrieved: Vec<String> = vec![];
        let score = latent_preference_score(&facts, &retrieved, "nonexistent");
        assert_eq!(score.expected_count, 0);
        assert!((score.jaccard - 1.0).abs() < 1e-10);
    }

    // ── Style drift ───────────────────────────────────────────────

    #[test]
    fn style_drift_identical_domains() {
        let old_edges = vec!["knows".to_string(), "works_at".to_string()];
        let new_edges = vec!["knows".to_string(), "works_at".to_string()];
        let old_types = vec!["person".to_string(), "org".to_string()];
        let new_types = vec!["person".to_string(), "org".to_string()];

        let score = style_drift_score(&old_edges, &new_edges, &old_types, &new_types);
        assert!((score.shared_edge_ratio - 1.0).abs() < 1e-10);
        assert!((score.entity_type_overlap - 1.0).abs() < 1e-10);
        assert!((score.composite - 1.0).abs() < 1e-10);
    }

    #[test]
    fn style_drift_no_overlap() {
        let old_edges = vec!["knows".to_string()];
        let new_edges = vec!["purchased".to_string()];
        let old_types = vec!["person".to_string()];
        let new_types = vec!["product".to_string()];

        let score = style_drift_score(&old_edges, &new_edges, &old_types, &new_types);
        assert!((score.shared_edge_ratio - 0.0).abs() < 1e-10);
        assert!((score.entity_type_overlap - 0.0).abs() < 1e-10);
        assert!((score.composite - 0.0).abs() < 1e-10);
    }

    #[test]
    fn style_drift_partial_overlap() {
        let old_edges = vec![
            "knows".to_string(),
            "works_at".to_string(),
            "lives_in".to_string(),
        ];
        let new_edges = vec!["knows".to_string(), "purchased".to_string()];
        let old_types = vec!["person".to_string(), "org".to_string()];
        let new_types = vec!["person".to_string(), "product".to_string()];

        let score = style_drift_score(&old_edges, &new_edges, &old_types, &new_types);
        // Edge overlap: 1 shared / 4 union = 0.25
        assert!((score.shared_edge_ratio - 0.25).abs() < 1e-10);
        // Type overlap: 1 shared / 3 union ≈ 0.333
        assert!((score.entity_type_overlap - (1.0 / 3.0)).abs() < 1e-10);
    }

    // ── Cross-domain transfer ──────────────────────────────────────

    #[test]
    fn transfer_perfect() {
        let source = vec![
            "token expiry".to_string(),
            "redis lock contention".to_string(),
        ];
        let retrieved = vec![
            "token expiry".to_string(),
            "redis lock contention".to_string(),
        ];

        let score = cross_domain_transfer_score(&source, &retrieved, 2);
        assert!((score.retrieval_ratio - 1.0).abs() < 1e-10);
        assert!((score.mapping_ratio - 1.0).abs() < 1e-10);
        assert!((score.composite - 1.0).abs() < 1e-10);
    }

    #[test]
    fn transfer_partial() {
        let source = vec![
            "token expiry".to_string(),
            "redis lock contention".to_string(),
        ];
        let retrieved = vec!["token expiry".to_string()];

        let score = cross_domain_transfer_score(&source, &retrieved, 1);
        assert!((score.retrieval_ratio - 0.5).abs() < 1e-10);
        assert!((score.mapping_ratio - 0.5).abs() < 1e-10);
        assert!((score.composite - 0.5).abs() < 1e-10);
    }

    #[test]
    fn transfer_empty_source() {
        let source: Vec<String> = vec![];
        let retrieved: Vec<String> = vec![];

        let score = cross_domain_transfer_score(&source, &retrieved, 0);
        assert!((score.retrieval_ratio - 1.0).abs() < 1e-10);
        assert!((score.mapping_ratio - 1.0).abs() < 1e-10);
    }

    // ── Preference cluster builder ───────────────────────────────

    #[test]
    fn build_cluster_creates_pairs() {
        let cluster = build_preference_cluster("brevity", &["short", "concise", "terse"]);
        assert_eq!(cluster.len(), 3);
        assert_eq!(cluster[0], ("short".to_string(), "brevity".to_string()));
        assert_eq!(cluster[1], ("concise".to_string(), "brevity".to_string()));
        assert_eq!(cluster[2], ("terse".to_string(), "brevity".to_string()));
    }
}
