//! Speculative retrieval — predict needed memories from access patterns.
//!
//! When entities A and B are frequently retrieved together, retrieving A
//! should suggest B. Based on co-access frequency analysis.

use serde::Serialize;
use std::collections::HashMap;
use uuid::Uuid;

/// A predicted entity that may be needed based on co-access patterns.
#[derive(Debug, Clone, Serialize)]
pub struct Prediction {
    pub entity_id: Uuid,
    pub confidence: f64,
    pub reason: String,
}

/// Track which entities are accessed together in the same session.
#[derive(Debug, Default)]
pub struct CoAccessTracker {
    /// For each entity pair, how many times they were co-accessed.
    co_access: HashMap<(Uuid, Uuid), usize>,
    /// Recent access window (last N entity IDs accessed).
    window: Vec<Uuid>,
    /// Maximum window size.
    window_size: usize,
}

impl CoAccessTracker {
    pub fn new(window_size: usize) -> Self {
        assert!(window_size > 0, "window_size must be at least 1");
        Self {
            co_access: HashMap::new(),
            window: Vec::new(),
            window_size,
        }
    }

    /// Record an entity access. Creates co-access pairs with recent window.
    pub fn record(&mut self, entity_id: Uuid) {
        for &prev in &self.window {
            if prev != entity_id {
                let key = if prev < entity_id {
                    (prev, entity_id)
                } else {
                    (entity_id, prev)
                };
                *self.co_access.entry(key).or_insert(0) += 1;
            }
        }
        self.window.push(entity_id);
        if self.window.len() > self.window_size {
            self.window.remove(0);
        }
    }

    /// Given recently accessed entities, predict what else will be needed.
    pub fn predict(&self, recent: &[Uuid], threshold: f64, limit: usize) -> Vec<Prediction> {
        assert!(
            (0.0..=1.0).contains(&threshold),
            "threshold must be 0.0..=1.0"
        );
        assert!(limit >= 1, "limit must be at least 1");

        let mut scores: HashMap<Uuid, f64> = HashMap::new();

        for &accessed in recent {
            for (&(a, b), &count) in &self.co_access {
                if a == accessed && !recent.contains(&b) {
                    *scores.entry(b).or_insert(0.0) += count as f64;
                }
                if b == accessed && !recent.contains(&a) {
                    *scores.entry(a).or_insert(0.0) += count as f64;
                }
            }
        }

        let max_score = scores.values().cloned().fold(0.0_f64, f64::max);
        if max_score == 0.0 {
            return Vec::new();
        }

        let mut predictions: Vec<Prediction> = scores
            .into_iter()
            .map(|(id, score)| Prediction {
                entity_id: id,
                confidence: score / max_score,
                reason: "co-access pattern".into(),
            })
            .filter(|p| p.confidence >= threshold)
            .collect();

        predictions.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        predictions.truncate(limit);
        predictions
    }

    /// Number of co-access pairs tracked.
    pub fn pair_count(&self) -> usize {
        self.co_access.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_creates_co_access_pairs() {
        let mut tracker = CoAccessTracker::new(5);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        tracker.record(a);
        tracker.record(b);
        tracker.record(c);

        // a-b, a-c, b-c should all have count 1
        assert_eq!(tracker.pair_count(), 3);
    }

    #[test]
    fn repeated_co_access_increases_count() {
        let mut tracker = CoAccessTracker::new(5);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        tracker.record(a);
        tracker.record(b);
        tracker.record(a);
        tracker.record(b);

        // a-b pair should have count > 1
        let predictions = tracker.predict(&[a], 0.0, 10);
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].entity_id, b);
        assert!((predictions[0].confidence - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn predict_returns_empty_for_unknown_entity() {
        let tracker = CoAccessTracker::new(5);
        let unknown = Uuid::new_v4();

        let predictions = tracker.predict(&[unknown], 0.0, 10);
        assert!(predictions.is_empty());
    }

    #[test]
    fn predict_excludes_recent_entities() {
        let mut tracker = CoAccessTracker::new(5);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        tracker.record(a);
        tracker.record(b);

        // b is in recent, so should not be predicted
        let predictions = tracker.predict(&[a, b], 0.0, 10);
        assert!(predictions.is_empty());
    }

    #[test]
    fn predict_respects_threshold() {
        let mut tracker = CoAccessTracker::new(5);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        // a-b accessed 3 times
        for _ in 0..3 {
            tracker.record(a);
            tracker.record(b);
        }
        // a-c accessed 1 time
        tracker.record(a);
        tracker.record(c);

        // With high threshold, only strong co-access should appear
        let predictions = tracker.predict(&[a], 0.8, 10);
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].entity_id, b);
    }

    #[test]
    fn predict_respects_limit() {
        let mut tracker = CoAccessTracker::new(10);
        let seed = Uuid::new_v4();

        // Create many co-access pairs
        for _ in 0..5 {
            let other = Uuid::new_v4();
            tracker.record(seed);
            tracker.record(other);
        }

        let predictions = tracker.predict(&[seed], 0.0, 2);
        assert!(predictions.len() <= 2);
    }

    #[test]
    fn window_evicts_old_entries() {
        let mut tracker = CoAccessTracker::new(1);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        tracker.record(a);
        // window: [a]
        tracker.record(b);
        // b pairs with a; window becomes [a, b], evicts a -> [b]
        tracker.record(c);
        // c pairs with b; window becomes [b, c], evicts b -> [c]

        // c is co-accessed with b only (a was evicted before c was recorded)
        let predictions = tracker.predict(&[c], 0.0, 10);
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].entity_id, b);
    }

    #[test]
    fn confidence_normalized_to_max() {
        let mut tracker = CoAccessTracker::new(10);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        // a-b: 4 co-accesses
        for _ in 0..4 {
            tracker.record(a);
            tracker.record(b);
        }
        // a-c: 2 co-accesses
        for _ in 0..2 {
            tracker.record(a);
            tracker.record(c);
        }

        let predictions = tracker.predict(&[a], 0.0, 10);
        assert_eq!(predictions.len(), 2);
        // b should have confidence 1.0, c should have ~0.5
        assert_eq!(predictions[0].entity_id, b);
        assert!((predictions[0].confidence - 1.0).abs() < f64::EPSILON);
        assert!((predictions[1].confidence - 0.5).abs() < 0.1);
    }
}
