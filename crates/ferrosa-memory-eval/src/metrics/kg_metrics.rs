//! KG Metrics — graph quality metrics against ground truth.
//!
//! Measures edge precision/recall, microstructure fidelity (triangles, 2-paths, stars),
//! and deduplication accuracy using a ground-truth benchmark approach.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Ground truth
// ---------------------------------------------------------------------------

/// Ground-truth benchmark for a knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KgGroundTruth {
    /// (name, type) pairs for expected entities.
    pub entities: Vec<(String, String)>,
    /// (src_name, dst_name, edge_type) triples for expected edges.
    pub edges: Vec<(String, String, String)>,
}

impl KgGroundTruth {
    /// Create an empty ground truth.
    pub fn empty() -> Self {
        Self {
            entities: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Number of triangles in the ground-truth graph.
    pub fn triangle_count(&self) -> usize {
        count_triangles(&self.edges)
    }

    /// Number of 2-paths (A-B-C structures).
    pub fn two_path_count(&self) -> usize {
        count_two_paths(&self.edges)
    }

    /// Number of star centers (nodes with degree >= 3).
    pub fn star_count(&self) -> usize {
        count_stars(&self.edges)
    }
}

// ---------------------------------------------------------------------------
// Edge precision / recall
// ---------------------------------------------------------------------------

/// Compute edge precision and recall against ground truth.
///
/// `found_edges`: (src_name, dst_name, edge_type) from storage.
/// `gt`: ground truth edges.
///
/// Returns (precision, recall) where:
/// - precision = TP / |found|
/// - recall = TP / |expected|
pub fn edge_precision_recall(
    found_edges: &[(String, String, String)],
    gt: &KgGroundTruth,
) -> (f64, f64) {
    let found_set: HashSet<&(String, String, String)> = found_edges.iter().collect();
    let expected_set: HashSet<&(String, String, String)> = gt.edges.iter().collect();

    let tp = found_set.intersection(&expected_set).count() as f64;

    let precision = if found_set.is_empty() {
        1.0
    } else {
        tp / found_set.len() as f64
    };

    let recall = if expected_set.is_empty() {
        1.0
    } else {
        tp / expected_set.len() as f64
    };

    (precision, recall)
}

// ---------------------------------------------------------------------------
// Microstructure fidelity (ERGM-lite)
// ---------------------------------------------------------------------------

/// Microstructure fidelity score comparing graph motifs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MicrostructureScore {
    /// Ratio of found triangles / expected triangles.
    pub triangle_ratio: f64,
    /// Ratio of found 2-paths / expected 2-paths.
    pub two_path_ratio: f64,
    /// Ratio of found stars / expected stars.
    pub star_ratio: f64,
}

impl MicrostructureScore {
    /// Overall fidelity: average of all ratios (clamped to [0, 2]).
    pub fn composite(&self) -> f64 {
        let avg = (self.triangle_ratio + self.two_path_ratio + self.star_ratio) / 3.0;
        avg.clamp(0.0, 2.0)
    }
}

/// Compute microstructure fidelity between found graph and ground truth.
pub fn microstructure_fidelity(
    found_edges: &[(String, String, String)],
    gt: &KgGroundTruth,
) -> MicrostructureScore {
    let found_tri = count_triangles(found_edges);
    let gt_tri = gt.triangle_count();
    let triangle_ratio = if gt_tri == 0 {
        1.0
    } else {
        (found_tri as f64) / (gt_tri as f64)
    };

    let found_2p = count_two_paths(found_edges);
    let gt_2p = gt.two_path_count();
    let two_path_ratio = if gt_2p == 0 {
        1.0
    } else {
        (found_2p as f64) / (gt_2p as f64)
    };

    let found_star = count_stars(found_edges);
    let gt_star = gt.star_count();
    let star_ratio = if gt_star == 0 {
        1.0
    } else {
        (found_star as f64) / (gt_star as f64)
    };

    MicrostructureScore {
        triangle_ratio,
        two_path_ratio,
        star_ratio,
    }
}

// ---------------------------------------------------------------------------
// Motif counting
// ---------------------------------------------------------------------------

/// Count undirected triangles in an edge list.
fn count_triangles(edges: &[(String, String, String)]) -> usize {
    // Build adjacency per edge type
    let mut adj: HashMap<&str, HashMap<&str, HashSet<&str>>> = HashMap::new();

    for (src, dst, etype) in edges {
        adj.entry(etype.as_str())
            .or_default()
            .entry(src.as_str())
            .or_default()
            .insert(dst.as_str());
        adj.entry(etype.as_str())
            .or_default()
            .entry(dst.as_str())
            .or_default()
            .insert(src.as_str());
    }

    let mut triangles = 0usize;
    for neighbors in adj.values() {
        let nodes: Vec<&str> = neighbors.keys().copied().collect();
        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                let a = nodes[i];
                let b = nodes[j];
                if neighbors.get(a).is_some_and(|s| s.contains(b)) {
                    // Check common neighbors of a and b
                    let na = neighbors.get(a).unwrap();
                    let nb = neighbors.get(b).unwrap();
                    let common = na.intersection(nb).count();
                    triangles += common;
                }
            }
        }
    }

    // Each triangle counted 3 times (once per edge), divide by 3
    triangles / 3
}

/// Count 2-paths (A-B-C where A != C).
fn count_two_paths(edges: &[(String, String, String)]) -> usize {
    let mut adj: HashMap<&str, HashMap<&str, HashSet<&str>>> = HashMap::new();

    for (src, dst, etype) in edges {
        adj.entry(etype.as_str())
            .or_default()
            .entry(src.as_str())
            .or_default()
            .insert(dst.as_str());
        adj.entry(etype.as_str())
            .or_default()
            .entry(dst.as_str())
            .or_default()
            .insert(src.as_str());
    }

    let mut two_paths = 0usize;
    for neighbors in adj.values() {
        for nbrs in neighbors.values() {
            let d = nbrs.len();
            if d >= 2 {
                // Each pair of neighbors forms a 2-path through this node
                two_paths += d * (d - 1) / 2;
            }
        }
    }

    two_paths
}

/// Count star centers (nodes with degree >= 3).
fn count_stars(edges: &[(String, String, String)]) -> usize {
    let mut degree: HashMap<&str, usize> = HashMap::new();

    for (src, dst, _etype) in edges {
        *degree.entry(src.as_str()).or_insert(0) += 1;
        *degree.entry(dst.as_str()).or_insert(0) += 1;
    }

    degree.values().filter(|d| **d >= 3).count()
}

// ---------------------------------------------------------------------------
// Deduplication benchmark
// ---------------------------------------------------------------------------

/// Result of a deduplication benchmark.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DedupScore {
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

/// Run deduplication benchmark on entity pairs.
///
/// `intentional_dups`: set of (id_a, id_b) pairs that are true duplicates.
/// `found_dup_pairs`: set of (id_a, id_b) pairs reported by the system.
pub fn dedup_benchmark(
    intentional_dups: &HashSet<(Uuid, Uuid)>,
    found_dup_pairs: &HashSet<(Uuid, Uuid)>,
) -> DedupScore {
    let tp = found_dup_pairs.intersection(intentional_dups).count();
    let fp = found_dup_pairs.difference(intentional_dups).count();
    let fn_ = intentional_dups.difference(found_dup_pairs).count();

    let precision = if tp + fp == 0 {
        1.0
    } else {
        tp as f64 / (tp + fp) as f64
    };

    let recall = if tp + fn_ == 0 {
        1.0
    } else {
        tp as f64 / (tp + fn_) as f64
    };

    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };

    DedupScore {
        true_positives: tp,
        false_positives: fp,
        false_negatives: fn_,
        precision,
        recall,
        f1,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Edge precision / recall ────────────────────────────────────

    #[test]
    fn perfect_graph_precision_recall() {
        let gt = KgGroundTruth {
            entities: vec![
                ("Alice".to_string(), "person".to_string()),
                ("Bob".to_string(), "person".to_string()),
                ("Carol".to_string(), "person".to_string()),
            ],
            edges: vec![
                ("Alice".to_string(), "Bob".to_string(), "knows".to_string()),
                ("Bob".to_string(), "Carol".to_string(), "knows".to_string()),
            ],
        };

        let found = vec![
            ("Alice".to_string(), "Bob".to_string(), "knows".to_string()),
            ("Bob".to_string(), "Carol".to_string(), "knows".to_string()),
        ];

        let (precision, recall) = edge_precision_recall(&found, &gt);
        assert!((precision - 1.0).abs() < 1e-10);
        assert!((recall - 1.0).abs() < 1e-10);
    }

    #[test]
    fn missing_edge_drops_recall() {
        let gt = KgGroundTruth {
            entities: vec![],
            edges: vec![
                ("A".to_string(), "B".to_string(), "x".to_string()),
                ("B".to_string(), "C".to_string(), "x".to_string()),
            ],
        };

        let found = vec![("A".to_string(), "B".to_string(), "x".to_string())];

        let (precision, recall) = edge_precision_recall(&found, &gt);
        assert!((precision - 1.0).abs() < 1e-10);
        assert!((recall - 0.5).abs() < 1e-10);
    }

    #[test]
    fn extra_edge_drops_precision() {
        let gt = KgGroundTruth {
            entities: vec![],
            edges: vec![("A".to_string(), "B".to_string(), "x".to_string())],
        };

        let found = vec![
            ("A".to_string(), "B".to_string(), "x".to_string()),
            ("B".to_string(), "C".to_string(), "x".to_string()),
        ];

        let (precision, recall) = edge_precision_recall(&found, &gt);
        assert!((precision - 0.5).abs() < 1e-10);
        assert!((recall - 1.0).abs() < 1e-10);
    }

    #[test]
    fn empty_ground_truth_is_perfect() {
        let gt = KgGroundTruth::empty();
        let found = vec![];
        let (p, r) = edge_precision_recall(&found, &gt);
        assert!((p - 1.0).abs() < 1e-10);
        assert!((r - 1.0).abs() < 1e-10);
    }

    // ── Triangle counting ─────────────────────────────────────────

    #[test]
    fn count_triangles_three_node_clique() {
        let edges = vec![
            ("A".to_string(), "B".to_string(), "knows".to_string()),
            ("B".to_string(), "C".to_string(), "knows".to_string()),
            ("C".to_string(), "A".to_string(), "knows".to_string()),
        ];
        assert_eq!(super::count_triangles(&edges), 1);
    }

    #[test]
    fn count_triangles_line_has_zero() {
        let edges = vec![
            ("A".to_string(), "B".to_string(), "knows".to_string()),
            ("B".to_string(), "C".to_string(), "knows".to_string()),
        ];
        assert_eq!(super::count_triangles(&edges), 0);
    }

    #[test]
    fn count_triangles_two_triangles() {
        let edges = vec![
            ("A".to_string(), "B".to_string(), "x".to_string()),
            ("B".to_string(), "C".to_string(), "x".to_string()),
            ("C".to_string(), "A".to_string(), "x".to_string()),
            ("A".to_string(), "D".to_string(), "x".to_string()),
            ("D".to_string(), "E".to_string(), "x".to_string()),
            ("E".to_string(), "A".to_string(), "x".to_string()),
        ];
        assert_eq!(super::count_triangles(&edges), 2);
    }

    // ── 2-path counting ────────────────────────────────────────────

    #[test]
    fn count_two_paths_line() {
        let edges = vec![
            ("A".to_string(), "B".to_string(), "x".to_string()),
            ("B".to_string(), "C".to_string(), "x".to_string()),
        ];
        // B is center of one 2-path (A-B-C)
        assert_eq!(super::count_two_paths(&edges), 1);
    }

    #[test]
    fn count_two_paths_star() {
        let edges = vec![
            ("Center".to_string(), "A".to_string(), "x".to_string()),
            ("Center".to_string(), "B".to_string(), "x".to_string()),
            ("Center".to_string(), "C".to_string(), "x".to_string()),
        ];
        // Center has 3 neighbors → 3 choose 2 = 3 two-paths
        assert_eq!(super::count_two_paths(&edges), 3);
    }

    // ── Star counting ──────────────────────────────────────────────

    #[test]
    fn count_stars_degree_three() {
        let edges = vec![
            ("Center".to_string(), "A".to_string(), "x".to_string()),
            ("Center".to_string(), "B".to_string(), "x".to_string()),
            ("Center".to_string(), "C".to_string(), "x".to_string()),
        ];
        // Center has degree 3, leaves have degree 1 → 1 star center
        assert_eq!(super::count_stars(&edges), 1);
    }

    #[test]
    fn count_stars_line_no_stars() {
        let edges = vec![
            ("A".to_string(), "B".to_string(), "x".to_string()),
            ("B".to_string(), "C".to_string(), "x".to_string()),
        ];
        // Max degree is 2 → no stars
        assert_eq!(super::count_stars(&edges), 0);
    }

    // ── Microstructure fidelity ───────────────────────────────────

    #[test]
    fn microstructure_perfect_match() {
        let gt = KgGroundTruth {
            entities: vec![],
            edges: vec![
                ("A".to_string(), "B".to_string(), "x".to_string()),
                ("B".to_string(), "C".to_string(), "x".to_string()),
                ("C".to_string(), "A".to_string(), "x".to_string()),
            ],
        };

        let found = vec![
            ("A".to_string(), "B".to_string(), "x".to_string()),
            ("B".to_string(), "C".to_string(), "x".to_string()),
            ("C".to_string(), "A".to_string(), "x".to_string()),
        ];

        let score = microstructure_fidelity(&found, &gt);
        assert!((score.triangle_ratio - 1.0).abs() < 1e-10);
        assert!((score.two_path_ratio - 1.0).abs() < 1e-10);
        assert!((score.star_ratio - 1.0).abs() < 1e-10);
    }

    #[test]
    fn triangle_bias_detects_false_triangles() {
        // GT has 3 edges forming 1 triangle
        let gt = KgGroundTruth {
            entities: vec![],
            edges: vec![
                ("A".to_string(), "B".to_string(), "x".to_string()),
                ("B".to_string(), "C".to_string(), "x".to_string()),
                ("C".to_string(), "A".to_string(), "x".to_string()),
            ],
        };

        // Found adds a false edge creating an extra triangle
        let found = vec![
            ("A".to_string(), "B".to_string(), "x".to_string()),
            ("B".to_string(), "C".to_string(), "x".to_string()),
            ("C".to_string(), "A".to_string(), "x".to_string()),
            ("A".to_string(), "D".to_string(), "x".to_string()),
            ("D".to_string(), "B".to_string(), "x".to_string()),
        ];

        let score = microstructure_fidelity(&found, &gt);
        // Triangle ratio = 2 found / 1 expected = 2.0
        assert!((score.triangle_ratio - 2.0).abs() < 1e-10);
        // Precision drops even though ratio > 1
        let (precision, _recall) = edge_precision_recall(&found, &gt);
        assert!(precision < 1.0);
    }

    // ── Dedup benchmark ───────────────────────────────────────────

    #[test]
    fn dedup_perfect_detection() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let _id3 = Uuid::new_v4();

        let intentional: HashSet<(Uuid, Uuid)> = [(id1, id2)].into_iter().collect();
        let found: HashSet<(Uuid, Uuid)> = [(id1, id2)].into_iter().collect();

        let score = dedup_benchmark(&intentional, &found);
        assert_eq!(score.true_positives, 1);
        assert_eq!(score.false_positives, 0);
        assert_eq!(score.false_negatives, 0);
        assert!((score.precision - 1.0).abs() < 1e-10);
        assert!((score.recall - 1.0).abs() < 1e-10);
        assert!((score.f1 - 1.0).abs() < 1e-10);
    }

    #[test]
    fn dedup_half_duplicates() {
        // 10 intentional duplicate pairs, system finds 7
        let ids: Vec<Uuid> = (0..20).map(|_| Uuid::new_v4()).collect();
        let mut intentional = HashSet::new();
        for i in 0..10 {
            intentional.insert((ids[i * 2], ids[i * 2 + 1]));
        }

        let mut found = HashSet::new();
        for i in 0..7 {
            found.insert((ids[i * 2], ids[i * 2 + 1]));
        }

        let score = dedup_benchmark(&intentional, &found);
        assert_eq!(score.true_positives, 7);
        assert_eq!(score.false_negatives, 3);
        assert_eq!(score.false_positives, 0);
        assert!((score.recall - 0.7).abs() < 1e-10);
    }

    #[test]
    fn dedup_with_false_positives() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        let intentional: HashSet<(Uuid, Uuid)> = [(id1, id2)].into_iter().collect();
        let found: HashSet<(Uuid, Uuid)> = [(id1, id2), (id2, id3)].into_iter().collect();

        let score = dedup_benchmark(&intentional, &found);
        assert_eq!(score.true_positives, 1);
        assert_eq!(score.false_positives, 1);
        assert_eq!(score.false_negatives, 0);
        assert!((score.precision - 0.5).abs() < 1e-10);
        assert!((score.recall - 1.0).abs() < 1e-10);
    }
}
