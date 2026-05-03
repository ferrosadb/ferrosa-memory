//! Contradiction detection for temporal facts (STUB — Task 4 partially implemented).
//!
//! The full implementation requires Storage trait methods `contradiction_put` and
//! CQL prepared statements. This stub provides the core logic; wiring is tracked
//! in specs/in-process/feat-kg-evolution.md Task 4.
//!
//! Two facts contradict if:
//! - Same entity
//! - Similar semantic content (token overlap > 0.6)
//! - Opposite polarity (detected via negation keywords)

use std::collections::HashSet;
use sha2::{Sha256, Digest};

/// Check if `new_fact` contradicts `old_fact`.
/// Returns true if negation polarity flips and semantic overlap is high.
pub fn is_contradiction(old: &str, new: &str) -> bool {
    let old_negated = has_negation(old);
    let new_negated = has_negation(new);
    if old_negated == new_negated {
        return false; // same polarity
    }
    token_overlap(old, new) > 0.6
}

fn has_negation(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "not", "no longer", "deprecated", "removed", "false",
        "does not", "isn't", "wasn't", "don't", "won't",
    ]
    .iter()
    .any(|w| lower.contains(w))
}

fn token_overlap(a: &str, b: &str) -> f64 {
    let normalize = |s: &str| {
        s.to_lowercase()
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
            .filter(|w| !w.is_empty() && w.len() > 2)
            .collect::<HashSet<String>>()
    };

    let a_tokens = normalize(a);
    let b_tokens = normalize(b);

    if a_tokens.is_empty() || b_tokens.is_empty() {
        return 0.0;
    }

    let intersection = a_tokens.intersection(&b_tokens).count();
    let union = a_tokens.union(&b_tokens).count();
    intersection as f64 / union as f64
}

/// Hash a fact text for deduplication.
pub fn hash_fact(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_negation() {
        assert!(has_negation("does not use port 8080"));
        assert!(!has_negation("uses port 8080"));
        assert!(has_negation("deprecated API v1"));
        assert!(has_negation("Server isn't running"));
    }

    #[test]
    fn test_token_overlap_similar() {
        let a = "Server uses port 8080 for HTTP";
        let b = "Server uses port 8080 for HTTPS";
        assert!(token_overlap(a, b) > 0.5);
    }

    #[test]
    fn test_token_overlap_different() {
        let a = "Server uses port 8080";
        let c = "Database runs on PostgreSQL";
        assert!(token_overlap(a, c) < 0.3);
    }

    #[test]
    fn test_is_contradiction_negation_flip() {
        assert!(is_contradiction("Uses port 8080", "Does not use port 8080"));
    }

    #[test]
    fn test_is_contradiction_same_polarity() {
        assert!(!is_contradiction("Uses port 8080", "Uses port 9090"));
        assert!(!is_contradiction("Uses port 8080", "Server uses port 8080"));
    }

    #[test]
    fn test_is_contradiction_no_overlap() {
        assert!(!is_contradiction("Database is PostgreSQL", "Server is not running"));
    }

    #[test]
    fn test_hash_fact_deterministic() {
        let h1 = hash_fact("test fact");
        let h2 = hash_fact("test fact");
        assert_eq!(h1, h2);
        let h3 = hash_fact("different fact");
        assert_ne!(h1, h3);
    }
}
