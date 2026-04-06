//! DIKW Information-to-Knowledge Analyzer (T-019).
//!
//! Evaluates transformation from structured information to connected knowledge:
//! - Consolidation edge counting (CO_OCCURS) with symmetric dedup
//! - Search recall@k: run hybrid_search for known entities, compute recall
//! - Spread activation reach: how many hops from seed entity
//!
//! Risk mitigation:
//! - EF11 (RPN 180): Symmetric edge double-counting — edges A->B and B->A
//!   counted as ONE via (min(src,dst), max(src,dst)) dedup.

use std::collections::HashSet;

use serde_json::Value;

use crate::report::TransitionScore;
use crate::scenario::ToolCallTrace;

// ---------------------------------------------------------------------------
// Edge analysis
// ---------------------------------------------------------------------------

/// A parsed edge from an explore_connections response.
#[derive(Debug, Clone)]
pub struct EdgeEntry {
    pub source_id: String,
    pub target_id: String,
    pub edge_type: String,
}

/// Canonical key for symmetric dedup (EF11).
///
/// Sorts source/target so A->B and B->A produce the same key.
fn canonical_edge_key(src: &str, dst: &str) -> (String, String) {
    if src <= dst {
        (src.to_string(), dst.to_string())
    } else {
        (dst.to_string(), src.to_string())
    }
}

/// Parse edges from an MCP explore_connections response.
///
/// Expected shape (MCP content wrapper):
/// ```json
/// {"content": [{"type": "text", "text": "{\"edges\": [...]}"}]}
/// ```
/// Or direct: `{"edges": [{"source_id": "...", "target_id": "...", "edge_type": "..."}]}`
pub fn parse_edges(response: &Value) -> Vec<EdgeEntry> {
    let inner = extract_inner_json(response);
    let edges_arr = inner
        .get("edges")
        .or_else(|| inner.get("connections"))
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
            Some(EdgeEntry {
                source_id,
                target_id,
                edge_type,
            })
        })
        .collect()
}

/// Count unique consolidation edges with symmetric dedup (EF11).
///
/// Returns (deduped_count, raw_count) so callers can see the difference.
pub fn count_consolidation_edges(edges: &[EdgeEntry]) -> (usize, usize) {
    let consolidation_edges: Vec<&EdgeEntry> = edges
        .iter()
        .filter(|e| {
            let t = e.edge_type.to_lowercase();
            t == "co_occurs" || t == "consolidation" || t == "cooccurs"
        })
        .collect();

    let raw_count = consolidation_edges.len();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for edge in &consolidation_edges {
        let key = canonical_edge_key(&edge.source_id, &edge.target_id);
        seen.insert(key);
    }

    (seen.len(), raw_count)
}

// ---------------------------------------------------------------------------
// Search recall
// ---------------------------------------------------------------------------

/// Parse search results from a hybrid_search response.
///
/// Returns a list of entity names/ids found.
pub fn parse_search_results(response: &Value) -> Vec<String> {
    let inner = extract_inner_json(response);
    let results_arr = inner
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    results_arr
        .iter()
        .filter_map(|r| {
            r.get("entity_name")
                .or_else(|| r.get("name"))
                .or_else(|| r.get("entity_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Compute recall@k: fraction of known entities found in search results.
///
/// `known_entities` is the ground truth set. `found_entities` is from the search.
pub fn recall_at_k(known_entities: &[String], found_entities: &[String]) -> f64 {
    if known_entities.is_empty() {
        return 1.0; // trivially complete
    }

    let found_set: HashSet<&str> = found_entities.iter().map(|s| s.as_str()).collect();
    let found_count = known_entities
        .iter()
        .filter(|e| found_set.contains(e.as_str()))
        .count();

    found_count as f64 / known_entities.len() as f64
}

// ---------------------------------------------------------------------------
// Spread activation reach
// ---------------------------------------------------------------------------

/// Parse spread activation results to count unique nodes reached.
pub fn parse_spread_activation_reach(response: &Value) -> usize {
    let inner = extract_inner_json(response);

    // Try "activations" array first, then "nodes"
    let nodes_arr = inner
        .get("activations")
        .or_else(|| inner.get("nodes"))
        .or_else(|| inner.get("results"))
        .and_then(|v| v.as_array());

    match nodes_arr {
        Some(arr) => {
            let unique: HashSet<&str> = arr
                .iter()
                .filter_map(|n| {
                    n.get("entity_id")
                        .or_else(|| n.get("node_id"))
                        .and_then(|v| v.as_str())
                })
                .collect();
            unique.len()
        }
        None => 0,
    }
}

/// Extract inner JSON from MCP content wrapper.
fn extract_inner_json(response: &Value) -> Value {
    if let Some(content) = response.get("content").and_then(|c| c.as_array()) {
        if let Some(first) = content.first() {
            if let Some(text) = first.get("text").and_then(|t| t.as_str()) {
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    return parsed;
                }
            }
        }
    }
    response.clone()
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Analyze scenario traces to produce an Information-to-Knowledge TransitionScore.
///
/// Weights: edge counting 40%, search recall 35%, spread reach 25%.
pub fn analyze(
    traces: &[ToolCallTrace],
    known_entities: &[String],
    max_expected_reach: usize,
) -> TransitionScore {
    let mut all_edges: Vec<EdgeEntry> = Vec::new();
    let mut search_found: Vec<String> = Vec::new();
    let mut spread_reach: usize = 0;

    for trace in traces {
        match trace.tool.as_str() {
            "explore_connections" => {
                all_edges.extend(parse_edges(&trace.response));
            }
            "hybrid_search" => {
                search_found.extend(parse_search_results(&trace.response));
            }
            "spread_activation" => {
                let reach = parse_spread_activation_reach(&trace.response);
                if reach > spread_reach {
                    spread_reach = reach;
                }
            }
            _ => {}
        }
    }

    let (deduped, raw) = count_consolidation_edges(&all_edges);
    let edge_score = if raw == 0 {
        0.0
    } else {
        // Penalize double-counting: score is 1.0 when deduped == raw,
        // decreases as duplicates increase. Also require at least 1 edge.
        let dedup_ratio = if raw > 0 {
            deduped as f64 / raw as f64
        } else {
            1.0
        };
        // Combine existence (at least 1 edge) with dedup quality
        dedup_ratio.min(1.0)
    };
    let edge_detail = format!("{deduped} unique consolidation edges ({raw} raw, deduped)");

    let recall = recall_at_k(known_entities, &search_found);
    let recall_detail = format!(
        "recall@k: {recall:.2} ({}/{} known entities found)",
        (recall * known_entities.len() as f64).round() as usize,
        known_entities.len()
    );

    let reach_score = if max_expected_reach == 0 {
        if spread_reach > 0 {
            1.0
        } else {
            0.0
        }
    } else {
        (spread_reach as f64 / max_expected_reach as f64).min(1.0)
    };
    let reach_detail = format!(
        "spread reach: {spread_reach} nodes (expected up to {max_expected_reach})"
    );

    // Weighted composite
    let composite = edge_score * 0.40 + recall * 0.35 + reach_score * 0.25;

    let detail = format!("{edge_detail}; {recall_detail}; {reach_detail}");

    TransitionScore {
        label: "info_to_knowledge".to_string(),
        score: composite,
        detail,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
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

    fn make_edge_response(edges: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "edges": edges })).unwrap()
            }]
        })
    }

    fn make_search_response(results: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "results": results })).unwrap()
            }]
        })
    }

    fn make_spread_response(activations: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "activations": activations })).unwrap()
            }]
        })
    }

    // ---------------------------------------------------------------
    // Symmetric edge dedup (EF11)
    // ---------------------------------------------------------------

    #[test]
    fn canonical_edge_key_sorts_min_max() {
        let (a, b) = canonical_edge_key("B", "A");
        assert_eq!(a, "A");
        assert_eq!(b, "B");

        let (a, b) = canonical_edge_key("A", "B");
        assert_eq!(a, "A");
        assert_eq!(b, "B");
    }

    #[test]
    fn symmetric_edges_counted_once() {
        let edges = vec![
            EdgeEntry {
                source_id: "A".into(),
                target_id: "B".into(),
                edge_type: "co_occurs".into(),
            },
            EdgeEntry {
                source_id: "B".into(),
                target_id: "A".into(),
                edge_type: "co_occurs".into(),
            },
        ];
        let (deduped, raw) = count_consolidation_edges(&edges);
        assert_eq!(deduped, 1, "A->B and B->A should count as 1");
        assert_eq!(raw, 2, "raw count should be 2");
    }

    #[test]
    fn distinct_edges_counted_separately() {
        let edges = vec![
            EdgeEntry {
                source_id: "A".into(),
                target_id: "B".into(),
                edge_type: "co_occurs".into(),
            },
            EdgeEntry {
                source_id: "A".into(),
                target_id: "C".into(),
                edge_type: "co_occurs".into(),
            },
            EdgeEntry {
                source_id: "B".into(),
                target_id: "C".into(),
                edge_type: "co_occurs".into(),
            },
        ];
        let (deduped, raw) = count_consolidation_edges(&edges);
        assert_eq!(deduped, 3, "3 distinct pairs should count as 3");
        assert_eq!(raw, 3);
    }

    #[test]
    fn non_consolidation_edges_excluded() {
        let edges = vec![
            EdgeEntry {
                source_id: "A".into(),
                target_id: "B".into(),
                edge_type: "co_occurs".into(),
            },
            EdgeEntry {
                source_id: "A".into(),
                target_id: "C".into(),
                edge_type: "related_to".into(),
            },
        ];
        let (deduped, raw) = count_consolidation_edges(&edges);
        assert_eq!(deduped, 1, "only co_occurs edge should count");
        assert_eq!(raw, 1);
    }

    #[test]
    fn empty_edges_count_zero() {
        let (deduped, raw) = count_consolidation_edges(&[]);
        assert_eq!(deduped, 0);
        assert_eq!(raw, 0);
    }

    // ---------------------------------------------------------------
    // Search recall@k
    // ---------------------------------------------------------------

    #[test]
    fn perfect_recall() {
        let known = vec!["Alice".into(), "Bob".into(), "Carol".into()];
        let found = vec!["Alice".into(), "Bob".into(), "Carol".into(), "Dave".into()];
        let recall = recall_at_k(&known, &found);
        assert!(
            (recall - 1.0).abs() < 0.01,
            "all known found => recall 1.0, got {recall}"
        );
    }

    #[test]
    fn partial_recall() {
        let known = vec!["Alice".into(), "Bob".into(), "Carol".into(), "Dave".into()];
        let found = vec!["Alice".into(), "Carol".into()];
        let recall = recall_at_k(&known, &found);
        assert!(
            (recall - 0.5).abs() < 0.01,
            "2/4 known found => recall 0.5, got {recall}"
        );
    }

    #[test]
    fn zero_recall() {
        let known = vec!["Alice".into(), "Bob".into()];
        let found: Vec<String> = vec!["Unknown".into()];
        let recall = recall_at_k(&known, &found);
        assert!(
            recall < 0.01,
            "none found => recall 0.0, got {recall}"
        );
    }

    #[test]
    fn empty_known_trivially_complete() {
        let recall = recall_at_k(&[], &["Alice".into()]);
        assert!(
            (recall - 1.0).abs() < 0.01,
            "empty known set => trivially 1.0, got {recall}"
        );
    }

    // ---------------------------------------------------------------
    // Spread activation reach
    // ---------------------------------------------------------------

    #[test]
    fn spread_activation_counts_unique_nodes() {
        let response = make_spread_response(vec![
            json!({"entity_id": "A", "score": 1.0}),
            json!({"entity_id": "B", "score": 0.8}),
            json!({"entity_id": "C", "score": 0.5}),
            json!({"entity_id": "A", "score": 0.3}), // duplicate
        ]);
        let reach = parse_spread_activation_reach(&response);
        assert_eq!(reach, 3, "should count 3 unique nodes, got {reach}");
    }

    #[test]
    fn spread_activation_empty_response() {
        let response = json!({});
        let reach = parse_spread_activation_reach(&response);
        assert_eq!(reach, 0);
    }

    // ---------------------------------------------------------------
    // Parse helpers
    // ---------------------------------------------------------------

    #[test]
    fn parse_edges_from_mcp_response() {
        let response = make_edge_response(vec![
            json!({"source_id": "A", "target_id": "B", "edge_type": "co_occurs"}),
            json!({"source_id": "B", "target_id": "C", "edge_type": "related_to"}),
        ]);
        let edges = parse_edges(&response);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].edge_type, "co_occurs");
    }

    #[test]
    fn parse_edges_with_alternate_field_names() {
        let response = json!({
            "edges": [
                {"from_entity_id": "X", "to_entity_id": "Y", "relation_type": "consolidation"}
            ]
        });
        let edges = parse_edges(&response);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source_id, "X");
        assert_eq!(edges[0].target_id, "Y");
    }

    #[test]
    fn parse_search_results_extracts_names() {
        let response = make_search_response(vec![
            json!({"entity_name": "Alice", "score": 0.95}),
            json!({"entity_name": "Bob", "score": 0.80}),
        ]);
        let results = parse_search_results(&response);
        assert_eq!(results, vec!["Alice", "Bob"]);
    }

    // ---------------------------------------------------------------
    // Full analyzer integration
    // ---------------------------------------------------------------

    #[test]
    fn analyze_with_edges_and_search_scores_high() {
        let edge_response = make_edge_response(vec![
            json!({"source_id": "A", "target_id": "B", "edge_type": "co_occurs"}),
            json!({"source_id": "A", "target_id": "C", "edge_type": "co_occurs"}),
        ]);
        let search_response = make_search_response(vec![
            json!({"entity_name": "Alice"}),
            json!({"entity_name": "Bob"}),
        ]);
        let spread_response = make_spread_response(vec![
            json!({"entity_id": "A"}),
            json!({"entity_id": "B"}),
            json!({"entity_id": "C"}),
        ]);

        let traces = vec![
            make_trace("explore_connections", edge_response),
            make_trace("hybrid_search", search_response),
            make_trace("spread_activation", spread_response),
        ];

        let known = vec!["Alice".into(), "Bob".into()];
        let result = analyze(&traces, &known, 3);

        assert_eq!(result.label, "info_to_knowledge");
        assert!(
            result.score > 0.8,
            "good edges + recall + reach should score > 0.8, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_empty_traces_scores_low() {
        let result = analyze(&[], &["Alice".into()], 5);
        assert!(
            result.score < 0.1,
            "empty traces should score near 0, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_double_counted_edges_still_scores_correctly() {
        // EF11: A->B and B->A should count as 1 edge, not 2
        let edge_response = make_edge_response(vec![
            json!({"source_id": "A", "target_id": "B", "edge_type": "co_occurs"}),
            json!({"source_id": "B", "target_id": "A", "edge_type": "co_occurs"}),
        ]);

        let traces = vec![make_trace("explore_connections", edge_response)];
        let result = analyze(&traces, &[], 0);

        // Edge score: deduped=1, raw=2, ratio=0.5
        // No search recall (known empty => 1.0), no spread (0 expected => 0.0)
        // 0.5 * 0.4 + 1.0 * 0.35 + 0.0 * 0.25 = 0.55
        assert!(
            result.score > 0.4 && result.score < 0.7,
            "double-counted edges should produce moderate score, got {}",
            result.score
        );
    }
}
