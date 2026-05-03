//! DIKW Emergence Analyzer (T-021).
//!
//! Evaluates emergent relationship formation:
//! - Before/after graph snapshots (entity count, edge count, derived fact count)
//! - Edge provenance filtering: only count consolidation/datalog/spread edges
//!   as emergent; explicit edges are excluded (ET-E2)
//! - Base fact exclusion: derived facts matching base facts excluded (EF13)
//! - Graph density delta
//! - Edge quality sampling: flag if >30% of sampled edges are meaningless (EF02)
//!
//! Risk mitigation:
//! - EF02 (RPN 245): Edge correctness unverified — sample and validate
//! - EF13 (RPN 150): Base facts counted as derived — exclude matching pairs
//! - ET-E2: Manufacturing emergent relationships — provenance filtering

use serde_json::Value;

use crate::report::EmergenceScore;
use crate::runner::GraphSnapshot;
use crate::scenario::ToolCallTrace;

/// Provenance values that qualify an edge as emergent.
const EMERGENT_PROVENANCES: &[&str] = &["consolidation", "datalog", "spread"];

/// Threshold above which edge quality is considered poor (EF02).
const POOR_QUALITY_THRESHOLD: f64 = 0.30;

// ---------------------------------------------------------------------------
// Edge provenance filtering (ET-E2)
// ---------------------------------------------------------------------------

/// A parsed edge with provenance annotation.
#[derive(Debug, Clone)]
pub struct AnnotatedEdge {
    pub source_id: String,
    pub target_id: String,
    pub edge_type: String,
    pub created_by: String,
}

/// Parse annotated edges from an explore_connections response that includes
/// edge annotations (created_by provenance).
///
/// Expected shape:
/// ```json
/// {"edges": [{"source_id": "A", "target_id": "B", "edge_type": "co_occurs",
///             "annotations": {"created_by": "consolidation"}}]}
/// ```
pub fn parse_annotated_edges(response: &Value) -> Vec<AnnotatedEdge> {
    let inner = extract_inner_json(response);
    let edges_arr = inner
        .get("edges")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    edges_arr
        .iter()
        .filter_map(|e| {
            let source_id = e
                .get("source_id")
                .or_else(|| e.get("from_entity_id"))
                .and_then(|v| v.as_str())?
                .to_string();
            let target_id = e
                .get("target_id")
                .or_else(|| e.get("to_entity_id"))
                .and_then(|v| v.as_str())?
                .to_string();
            let edge_type = e
                .get("edge_type")
                .or_else(|| e.get("relation_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let created_by = extract_created_by(e);
            Some(AnnotatedEdge {
                source_id,
                target_id,
                edge_type,
                created_by,
            })
        })
        .collect()
}

/// Extract the created_by annotation from an edge JSON object.
///
/// Tries multiple paths:
/// 1. `annotations.created_by`
/// 2. `created_by` (top-level)
/// 3. `provenance` (alternate name)
fn extract_created_by(edge: &Value) -> String {
    // Try annotations object
    if let Some(annotations) = edge.get("annotations")
        && let Some(cb) = annotations.get("created_by").and_then(|v| v.as_str()) {
            return cb.to_string();
        }
    // Try top-level
    if let Some(cb) = edge.get("created_by").and_then(|v| v.as_str()) {
        return cb.to_string();
    }
    // Try provenance
    if let Some(p) = edge.get("provenance").and_then(|v| v.as_str()) {
        return p.to_string();
    }
    "explicit".to_string()
}

/// Filter edges to only emergent ones (consolidation, datalog, spread).
pub fn filter_emergent_edges(edges: &[AnnotatedEdge]) -> Vec<&AnnotatedEdge> {
    edges
        .iter()
        .filter(|e| {
            EMERGENT_PROVENANCES
                .iter()
                .any(|p| e.created_by.to_lowercase() == *p)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Base fact exclusion (EF13)
// ---------------------------------------------------------------------------

/// A derived fact that may or may not match a base fact.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FactKey {
    pub predicate: String,
    pub args: Vec<String>,
}

/// Parse derived facts from query_derived or consolidation results.
pub fn parse_derived_facts(response: &Value) -> Vec<FactKey> {
    let inner = extract_inner_json(response);
    let facts_arr = inner
        .get("derived_facts")
        .or_else(|| inner.get("facts"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    facts_arr
        .iter()
        .filter_map(|f| {
            let predicate = f.get("predicate").and_then(|v| v.as_str())?.to_string();
            let args = f
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Some(FactKey { predicate, args })
        })
        .collect()
}

/// Parse base facts for exclusion comparison.
pub fn parse_base_facts(response: &Value) -> Vec<FactKey> {
    let inner = extract_inner_json(response);
    let facts_arr = inner
        .get("base_facts")
        .or_else(|| inner.get("facts"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    facts_arr
        .iter()
        .filter_map(|f| {
            let predicate = f.get("predicate").and_then(|v| v.as_str())?.to_string();
            let args = f
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Some(FactKey { predicate, args })
        })
        .collect()
}

/// Exclude derived facts that exactly match base facts (EF13).
///
/// Returns the count of truly novel derived facts.
pub fn count_novel_derived_facts(derived: &[FactKey], base: &[FactKey]) -> usize {
    let base_set: std::collections::HashSet<&FactKey> = base.iter().collect();
    derived.iter().filter(|d| !base_set.contains(d)).count()
}

// ---------------------------------------------------------------------------
// Graph density
// ---------------------------------------------------------------------------

/// Compute directed graph density: edges / (nodes * (nodes - 1)).
///
/// Returns 0.0 if fewer than 2 nodes.
pub fn graph_density(entity_count: usize, edge_count: usize) -> f64 {
    if entity_count < 2 {
        return 0.0;
    }
    let max_edges = entity_count * (entity_count - 1);
    edge_count as f64 / max_edges as f64
}

/// Compute density delta between before and after snapshots.
pub fn density_delta(before: &GraphSnapshot, after: &GraphSnapshot) -> f64 {
    let density_before = graph_density(before.entity_count, before.edge_count);
    let density_after = graph_density(after.entity_count, after.edge_count);
    density_after - density_before
}

// ---------------------------------------------------------------------------
// Edge quality sampling (EF02)
// ---------------------------------------------------------------------------

/// An edge quality sample result.
#[derive(Debug, Clone)]
pub struct EdgeQualitySample {
    pub total_sampled: usize,
    pub meaningful_count: usize,
    pub quality_ratio: f64,
    pub poor_quality: bool,
}

/// Sample edge quality from emergent edges.
///
/// In unit tests, we use a simple heuristic: an edge is "meaningful" if
/// source and target are different entities. In integration tests with a
/// live cluster, this would use cosine similarity of embeddings.
///
/// Flags poor quality if > 30% of sampled edges are meaningless (EF02).
pub fn sample_edge_quality(edges: &[AnnotatedEdge], sample_size: usize) -> EdgeQualitySample {
    if edges.is_empty() {
        return EdgeQualitySample {
            total_sampled: 0,
            meaningful_count: 0,
            quality_ratio: 1.0, // no edges = trivially fine
            poor_quality: false,
        };
    }

    let sample_count = sample_size.min(edges.len());
    // Deterministic sampling: take first N (for reproducibility in tests).
    // In production, this would use a seeded RNG.
    let sample = &edges[..sample_count];

    let meaningful = sample.iter().filter(|e| e.source_id != e.target_id).count();

    let quality_ratio = meaningful as f64 / sample_count as f64;
    let meaningless_ratio = 1.0 - quality_ratio;

    EdgeQualitySample {
        total_sampled: sample_count,
        meaningful_count: meaningful,
        quality_ratio,
        poor_quality: meaningless_ratio > POOR_QUALITY_THRESHOLD,
    }
}

/// Extract inner JSON from MCP content wrapper.
fn extract_inner_json(response: &Value) -> Value {
    if let Some(content) = response.get("content").and_then(|c| c.as_array())
        && let Some(first) = content.first()
            && let Some(text) = first.get("text").and_then(|t| t.as_str())
                && let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    return parsed;
                }
    response.clone()
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Analyze emergence from before/after snapshots and traces.
///
/// Returns an EmergenceScore with all fields per overview.md section 6.2.
pub fn analyze(
    before: &GraphSnapshot,
    after: &GraphSnapshot,
    traces: &[ToolCallTrace],
    base_facts: &[FactKey],
) -> EmergenceScore {
    // Collect all annotated edges from traces
    let mut all_edges: Vec<AnnotatedEdge> = Vec::new();
    let mut all_derived: Vec<FactKey> = Vec::new();

    for trace in traces {
        match trace.tool.as_str() {
            "explore_connections" | "run_consolidation" => {
                all_edges.extend(parse_annotated_edges(&trace.response));
            }
            "query_derived" => {
                all_derived.extend(parse_derived_facts(&trace.response));
            }
            _ => {}
        }
    }

    // Edge provenance filtering (ET-E2)
    let emergent_edges = filter_emergent_edges(&all_edges);
    let emergent_edge_count = emergent_edges.len();

    // Collect new edge types
    let new_edge_types: Vec<String> = {
        let mut types: std::collections::HashSet<String> = std::collections::HashSet::new();
        for edge in &emergent_edges {
            types.insert(edge.edge_type.clone());
        }
        let mut sorted: Vec<String> = types.into_iter().collect();
        sorted.sort();
        sorted
    };

    // Base fact exclusion (EF13)
    let novel_derived = count_novel_derived_facts(&all_derived, base_facts);

    // Graph density
    let density_after = graph_density(after.entity_count, after.edge_count);
    let delta = density_delta(before, after);

    // Edge quality sampling (EF02)
    let emergent_owned: Vec<AnnotatedEdge> = emergent_edges.iter().map(|e| (*e).clone()).collect();
    let quality = sample_edge_quality(&emergent_owned, 10);

    // Compute composite score
    // Components:
    // - Edge growth: ratio of emergent edges to total edge growth (30%)
    // - Novel derived facts ratio (25%)
    // - Density delta positive (20%)
    // - Edge quality (25%)
    let edge_growth = after.edge_count.saturating_sub(before.edge_count);
    let edge_growth_score = if edge_growth == 0 {
        0.0
    } else {
        (emergent_edge_count as f64 / edge_growth as f64).min(1.0)
    };

    let derived_score = if all_derived.is_empty() {
        0.0
    } else {
        novel_derived as f64 / all_derived.len() as f64
    };

    let density_score = if delta > 0.0 {
        (delta * 10.0).min(1.0)
    } else {
        0.0
    };

    let quality_score = if quality.poor_quality {
        quality.quality_ratio * 0.5 // penalize poor quality
    } else {
        quality.quality_ratio
    };

    let composite = edge_growth_score * 0.30
        + derived_score * 0.25
        + density_score * 0.20
        + quality_score * 0.25;

    EmergenceScore {
        entities_before: before.entity_count,
        entities_after: after.entity_count,
        edges_before: before.edge_count,
        edges_after: after.edge_count,
        derived_facts_created: novel_derived,
        new_edge_types,
        graph_density: density_after,
        density_delta: delta,
        score: composite,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use std::collections::HashMap;

    fn make_trace(tool: &str, response: Value) -> ToolCallTrace {
        ToolCallTrace {
            tool: tool.to_string(),
            arguments: HashMap::new(),
            response,
            latency_ms: 50,
            success: true,
        }
    }

    fn make_snapshot(entities: usize, edges: usize, derived: usize) -> GraphSnapshot {
        GraphSnapshot {
            entity_count: entities,
            edge_count: edges,
            derived_fact_count: derived,
            timestamp: Utc::now(),
        }
    }

    fn make_edge_response(edges: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "edges": edges })).unwrap()
            }]
        })
    }

    fn make_derived_response(facts: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "derived_facts": facts })).unwrap()
            }]
        })
    }

    // ---------------------------------------------------------------
    // Edge provenance filtering (ET-E2)
    // ---------------------------------------------------------------

    #[test]
    fn only_emergent_provenance_edges_counted() {
        let edges = vec![
            AnnotatedEdge {
                source_id: "A".into(),
                target_id: "B".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
            AnnotatedEdge {
                source_id: "A".into(),
                target_id: "C".into(),
                edge_type: "related_to".into(),
                created_by: "explicit".into(),
            },
            AnnotatedEdge {
                source_id: "B".into(),
                target_id: "C".into(),
                edge_type: "derived".into(),
                created_by: "datalog".into(),
            },
            AnnotatedEdge {
                source_id: "C".into(),
                target_id: "D".into(),
                edge_type: "spread_edge".into(),
                created_by: "spread".into(),
            },
        ];

        let emergent = filter_emergent_edges(&edges);
        assert_eq!(
            emergent.len(),
            3,
            "should count consolidation + datalog + spread, not explicit"
        );
    }

    #[test]
    fn explicit_edges_excluded() {
        let edges = vec![
            AnnotatedEdge {
                source_id: "A".into(),
                target_id: "B".into(),
                edge_type: "manual".into(),
                created_by: "explicit".into(),
            },
            AnnotatedEdge {
                source_id: "A".into(),
                target_id: "C".into(),
                edge_type: "user_defined".into(),
                created_by: "explicit".into(),
            },
        ];

        let emergent = filter_emergent_edges(&edges);
        assert_eq!(
            emergent.len(),
            0,
            "explicit edges should be excluded entirely"
        );
    }

    #[test]
    fn provenance_case_insensitive() {
        let edges = vec![AnnotatedEdge {
            source_id: "A".into(),
            target_id: "B".into(),
            edge_type: "co_occurs".into(),
            created_by: "Consolidation".into(),
        }];

        let emergent = filter_emergent_edges(&edges);
        assert_eq!(
            emergent.len(),
            1,
            "provenance check should be case-insensitive"
        );
    }

    // ---------------------------------------------------------------
    // Base fact exclusion (EF13)
    // ---------------------------------------------------------------

    #[test]
    fn derived_facts_matching_base_excluded() {
        let base = vec![FactKey {
            predicate: "knows".into(),
            args: vec!["Alice".into(), "Bob".into()],
        }];
        let derived = vec![
            FactKey {
                predicate: "knows".into(),
                args: vec!["Alice".into(), "Bob".into()],
            },
            FactKey {
                predicate: "works_with".into(),
                args: vec!["Alice".into(), "Carol".into()],
            },
        ];

        let novel = count_novel_derived_facts(&derived, &base);
        assert_eq!(
            novel, 1,
            "derived fact matching base should be excluded, leaving 1 novel"
        );
    }

    #[test]
    fn all_derived_are_novel() {
        let base = vec![FactKey {
            predicate: "knows".into(),
            args: vec!["Alice".into(), "Bob".into()],
        }];
        let derived = vec![FactKey {
            predicate: "works_with".into(),
            args: vec!["Carol".into(), "Dave".into()],
        }];

        let novel = count_novel_derived_facts(&derived, &base);
        assert_eq!(novel, 1, "non-matching derived should all be novel");
    }

    #[test]
    fn empty_derived_returns_zero() {
        let novel = count_novel_derived_facts(&[], &[]);
        assert_eq!(novel, 0);
    }

    // ---------------------------------------------------------------
    // Graph density
    // ---------------------------------------------------------------

    #[test]
    fn graph_density_complete_graph() {
        // Complete directed graph with 4 nodes: 4 * 3 = 12 edges
        let density = graph_density(4, 12);
        assert!(
            (density - 1.0).abs() < 0.01,
            "complete graph should have density 1.0, got {density}"
        );
    }

    #[test]
    fn graph_density_sparse_graph() {
        // 4 nodes, 2 edges: 2 / 12 = 0.167
        let density = graph_density(4, 2);
        assert!(
            (density - 2.0 / 12.0).abs() < 0.01,
            "sparse graph density incorrect, got {density}"
        );
    }

    #[test]
    fn graph_density_single_node_is_zero() {
        let density = graph_density(1, 0);
        assert!(
            density < 0.01,
            "single node should have density 0, got {density}"
        );
    }

    #[test]
    fn graph_density_zero_nodes_is_zero() {
        let density = graph_density(0, 0);
        assert!(density < 0.01);
    }

    #[test]
    fn density_delta_positive_growth() {
        let before = make_snapshot(3, 2, 0);
        let after = make_snapshot(5, 10, 3);
        let delta = density_delta(&before, &after);
        assert!(
            delta > 0.0,
            "growing graph should have positive delta, got {delta}"
        );
    }

    #[test]
    fn density_delta_no_change() {
        let before = make_snapshot(3, 2, 0);
        let after = make_snapshot(3, 2, 0);
        let delta = density_delta(&before, &after);
        assert!(
            delta.abs() < 0.001,
            "no change should give zero delta, got {delta}"
        );
    }

    // ---------------------------------------------------------------
    // Edge quality sampling (EF02)
    // ---------------------------------------------------------------

    #[test]
    fn good_quality_edges_pass() {
        let edges = vec![
            AnnotatedEdge {
                source_id: "A".into(),
                target_id: "B".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
            AnnotatedEdge {
                source_id: "B".into(),
                target_id: "C".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
        ];
        let quality = sample_edge_quality(&edges, 10);
        assert!(
            !quality.poor_quality,
            "good edges should not flag poor quality"
        );
        assert!(
            (quality.quality_ratio - 1.0).abs() < 0.01,
            "all distinct src/dst should have ratio 1.0"
        );
    }

    #[test]
    fn self_loop_edges_flag_poor_quality() {
        // All edges are self-loops (meaningless)
        let edges = vec![
            AnnotatedEdge {
                source_id: "A".into(),
                target_id: "A".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
            AnnotatedEdge {
                source_id: "B".into(),
                target_id: "B".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
            AnnotatedEdge {
                source_id: "C".into(),
                target_id: "C".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
        ];
        let quality = sample_edge_quality(&edges, 10);
        assert!(
            quality.poor_quality,
            "all self-loops should flag poor quality (>30% meaningless)"
        );
        assert!(
            quality.quality_ratio < 0.01,
            "self-loops should have 0.0 quality ratio"
        );
    }

    #[test]
    fn mixed_quality_below_threshold() {
        // 2 good, 1 self-loop => 1/3 meaningless = 33% > 30% threshold
        let edges = vec![
            AnnotatedEdge {
                source_id: "A".into(),
                target_id: "B".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
            AnnotatedEdge {
                source_id: "B".into(),
                target_id: "C".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
            AnnotatedEdge {
                source_id: "D".into(),
                target_id: "D".into(),
                edge_type: "co_occurs".into(),
                created_by: "consolidation".into(),
            },
        ];
        let quality = sample_edge_quality(&edges, 10);
        assert!(
            quality.poor_quality,
            "33% meaningless should flag poor quality (threshold 30%)"
        );
    }

    #[test]
    fn empty_edges_not_poor_quality() {
        let quality = sample_edge_quality(&[], 10);
        assert!(
            !quality.poor_quality,
            "empty edges should not flag poor quality"
        );
        assert_eq!(quality.total_sampled, 0);
    }

    // ---------------------------------------------------------------
    // Parse helpers
    // ---------------------------------------------------------------

    #[test]
    fn parse_annotated_edges_from_mcp_response() {
        let response = make_edge_response(vec![
            json!({
                "source_id": "A",
                "target_id": "B",
                "edge_type": "co_occurs",
                "annotations": {"created_by": "consolidation"}
            }),
            json!({
                "source_id": "C",
                "target_id": "D",
                "edge_type": "related_to",
                "created_by": "explicit"
            }),
        ]);
        let edges = parse_annotated_edges(&response);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].created_by, "consolidation");
        assert_eq!(edges[1].created_by, "explicit");
    }

    #[test]
    fn parse_annotated_edges_default_to_explicit() {
        let response = json!({
            "edges": [{"source_id": "A", "target_id": "B", "edge_type": "unknown"}]
        });
        let edges = parse_annotated_edges(&response);
        assert_eq!(
            edges[0].created_by, "explicit",
            "missing provenance defaults to explicit"
        );
    }

    #[test]
    fn parse_derived_facts_from_response() {
        let response = make_derived_response(vec![
            json!({"predicate": "knows", "args": ["Alice", "Bob"]}),
            json!({"predicate": "works_with", "args": ["Carol", "Dave"]}),
        ]);
        let facts = parse_derived_facts(&response);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].predicate, "knows");
        assert_eq!(facts[0].args, vec!["Alice", "Bob"]);
    }

    // ---------------------------------------------------------------
    // Full analyzer integration
    // ---------------------------------------------------------------

    #[test]
    fn analyze_with_emergent_edges_produces_score() {
        let before = make_snapshot(3, 1, 0);
        let after = make_snapshot(5, 6, 2);

        let edge_response = make_edge_response(vec![
            json!({
                "source_id": "A",
                "target_id": "B",
                "edge_type": "co_occurs",
                "annotations": {"created_by": "consolidation"}
            }),
            json!({
                "source_id": "B",
                "target_id": "C",
                "edge_type": "derived_edge",
                "annotations": {"created_by": "datalog"}
            }),
            json!({
                "source_id": "C",
                "target_id": "D",
                "edge_type": "spread_edge",
                "annotations": {"created_by": "spread"}
            }),
            json!({
                "source_id": "A",
                "target_id": "D",
                "edge_type": "manual",
                "created_by": "explicit"
            }),
        ]);

        let derived_response = make_derived_response(vec![
            json!({"predicate": "works_with", "args": ["Alice", "Carol"]}),
            json!({"predicate": "knows", "args": ["Alice", "Bob"]}),
        ]);

        let traces = vec![
            make_trace("explore_connections", edge_response),
            make_trace("query_derived", derived_response),
        ];

        let base_facts = vec![FactKey {
            predicate: "knows".into(),
            args: vec!["Alice".into(), "Bob".into()],
        }];

        let result = analyze(&before, &after, &traces, &base_facts);

        assert_eq!(result.entities_before, 3);
        assert_eq!(result.entities_after, 5);
        assert_eq!(result.edges_before, 1);
        assert_eq!(result.edges_after, 6);
        assert_eq!(
            result.derived_facts_created, 1,
            "only 1 novel derived fact (knows excluded by EF13)"
        );
        assert!(
            !result.new_edge_types.is_empty(),
            "should have emergent edge types"
        );
        assert!(result.density_delta > 0.0, "density should increase");
        assert!(
            result.score > 0.0,
            "should have positive emergence score, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_only_explicit_edges_low_emergence() {
        let before = make_snapshot(2, 0, 0);
        let after = make_snapshot(4, 3, 0);

        let edge_response = make_edge_response(vec![
            json!({
                "source_id": "A",
                "target_id": "B",
                "edge_type": "manual",
                "created_by": "explicit"
            }),
            json!({
                "source_id": "B",
                "target_id": "C",
                "edge_type": "manual",
                "created_by": "explicit"
            }),
        ]);

        let traces = vec![make_trace("explore_connections", edge_response)];
        let result = analyze(&before, &after, &traces, &[]);

        // No emergent edges => edge_growth_score = 0
        // No derived facts => derived_score = 0
        // Density delta positive => some score
        // Quality: empty emergent => 1.0 (trivial)
        assert!(
            result.score < 0.5,
            "only explicit edges should produce low emergence, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_no_growth_zero_score() {
        let before = make_snapshot(3, 3, 0);
        let after = make_snapshot(3, 3, 0);
        let result = analyze(&before, &after, &[], &[]);

        assert!(
            result.score < 0.3,
            "no growth should produce near-zero emergence, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_preserves_snapshot_values() {
        let before = make_snapshot(10, 20, 5);
        let after = make_snapshot(15, 40, 12);
        let result = analyze(&before, &after, &[], &[]);

        assert_eq!(result.entities_before, 10);
        assert_eq!(result.entities_after, 15);
        assert_eq!(result.edges_before, 20);
        assert_eq!(result.edges_after, 40);
    }
}
