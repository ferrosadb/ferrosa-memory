//! Batch job logic for routing guideline refinement (ADR-002).
//!
//! Reads feedback outcomes, computes strategy accuracy per
//! (program_type, task_complexity), and produces updated routing guidelines.

use std::collections::HashMap;

use crate::types::FeedbackOutcome;

/// Strategy accuracy statistics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StrategyStats {
    pub program_type: String,
    pub task_complexity: String,
    pub total: usize,
    pub succeeded: usize,
    pub accuracy: f64,
    pub avg_latency_ms: f64,
}

/// Compute strategy accuracy from a set of feedback outcomes.
pub fn compute_strategy_accuracy(outcomes: &[FeedbackOutcome]) -> Vec<StrategyStats> {
    let mut groups: HashMap<(String, String), (usize, usize, i64)> = HashMap::new();

    for o in outcomes {
        let key = (o.program_type.clone(), o.task_complexity.clone());
        let entry = groups.entry(key).or_insert((0, 0, 0));
        entry.0 += 1; // total
        if o.succeeded {
            entry.1 += 1; // succeeded
        }
        entry.2 += i64::from(o.latency_ms); // total latency
    }

    let mut stats: Vec<StrategyStats> = groups
        .into_iter()
        .map(|((pt, tc), (total, succeeded, latency))| StrategyStats {
            program_type: pt,
            task_complexity: tc,
            total,
            succeeded,
            accuracy: if total > 0 {
                succeeded as f64 / total as f64
            } else {
                0.0
            },
            avg_latency_ms: if total > 0 {
                latency as f64 / total as f64
            } else {
                0.0
            },
        })
        .collect();

    stats.sort_by(|a, b| {
        a.program_type
            .cmp(&b.program_type)
            .then(a.task_complexity.cmp(&b.task_complexity))
    });
    stats
}

/// Generate routing guideline text from strategy stats.
pub fn generate_guidelines(stats: &[StrategyStats], version: &str) -> String {
    let mut lines = vec![format!("# Routing Guidelines {version}")];
    lines.push(String::new());

    for s in stats {
        let status = if s.accuracy >= 0.8 {
            "preferred"
        } else if s.accuracy >= 0.5 {
            "acceptable"
        } else {
            "avoid"
        };
        lines.push(format!(
            "- {}/{}: accuracy={:.0}% latency={:.0}ms status={}",
            s.program_type,
            s.task_complexity,
            s.accuracy * 100.0,
            s.avg_latency_ms,
            status
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn outcome(pt: &str, tc: &str, succeeded: bool, latency: i32) -> FeedbackOutcome {
        FeedbackOutcome {
            tenant_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            query_id: Uuid::new_v4(),
            program_type: pt.into(),
            task_complexity: tc.into(),
            succeeded,
            latency_ms: latency,
            token_cost: 0,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn empty_outcomes_returns_empty_stats() {
        let stats = compute_strategy_accuracy(&[]);
        assert!(stats.is_empty());
    }

    #[test]
    fn single_strategy_accuracy() {
        let outcomes = vec![
            outcome("hnsw_ann", "simple", true, 10),
            outcome("hnsw_ann", "simple", true, 20),
            outcome("hnsw_ann", "simple", false, 50),
        ];
        let stats = compute_strategy_accuracy(&outcomes);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].total, 3);
        assert_eq!(stats[0].succeeded, 2);
        assert!((stats[0].accuracy - 0.6667).abs() < 0.01);
    }

    #[test]
    fn multiple_strategies_grouped() {
        let outcomes = vec![
            outcome("hnsw_ann", "simple", true, 10),
            outcome("phonetic", "simple", true, 5),
            outcome("hnsw_ann", "linear", false, 100),
            outcome("phonetic", "simple", false, 8),
        ];
        let stats = compute_strategy_accuracy(&outcomes);
        assert_eq!(stats.len(), 3);
    }

    #[test]
    fn guidelines_marks_high_accuracy_preferred() {
        let stats = vec![StrategyStats {
            program_type: "hnsw_ann".into(),
            task_complexity: "simple".into(),
            total: 10,
            succeeded: 9,
            accuracy: 0.9,
            avg_latency_ms: 15.0,
        }];
        let text = generate_guidelines(&stats, "v2");
        assert!(text.contains("preferred"));
        assert!(text.contains("v2"));
    }

    #[test]
    fn guidelines_marks_low_accuracy_avoid() {
        let stats = vec![StrategyStats {
            program_type: "cypher_hop".into(),
            task_complexity: "quadratic".into(),
            total: 10,
            succeeded: 2,
            accuracy: 0.2,
            avg_latency_ms: 500.0,
        }];
        let text = generate_guidelines(&stats, "v1");
        assert!(text.contains("avoid"));
    }

    #[test]
    fn full_cycle_outcomes_to_guidelines() {
        let outcomes = vec![
            outcome("hnsw_ann", "simple", true, 10),
            outcome("hnsw_ann", "simple", true, 12),
            outcome("hnsw_ann", "simple", true, 8),
            outcome("phonetic", "simple", true, 3),
            outcome("phonetic", "simple", false, 50),
            outcome("btree_range", "linear", false, 200),
            outcome("btree_range", "linear", false, 300),
        ];
        let stats = compute_strategy_accuracy(&outcomes);
        let guidelines = generate_guidelines(&stats, "v2");

        // hnsw_ann: 100% → preferred
        assert!(guidelines.contains("hnsw_ann/simple: accuracy=100%"));
        // btree_range: 0% → avoid
        assert!(guidelines.contains("btree_range/linear: accuracy=0%"));
    }
}
