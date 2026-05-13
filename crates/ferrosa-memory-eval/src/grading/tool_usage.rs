//! Tool usage grader (T-033).
//!
//! Computes:
//! - Latency percentiles (p50, p95, p99) from `ToolCallTrace` latencies
//! - Unnecessary call detection: tool response `entity_id`s that never appear
//!   in any subsequent tool's arguments
//! - Token cost estimation: `json_bytes / 4`
//! - Efficiency score: `1.0 - (unnecessary / total)`
//! - Returns `report::ToolUsageScore`

use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;

use crate::report::ToolUsageScore;
use crate::scenario::ToolCallTrace;

/// Compute latency percentile from a **sorted** slice of latencies.
///
/// Uses nearest-rank: `ceil(p/100 * n) - 1`, clamped.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    assert!(!sorted.is_empty(), "percentile requires non-empty slice");
    assert!(
        (0.0..=100.0).contains(&p),
        "percentile must be 0-100, got {p}"
    );

    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

/// Extract all `entity_id` string values from a JSON value at any depth.
fn extract_entity_ids(value: &Value) -> HashSet<String> {
    let mut ids = HashSet::new();
    collect_entity_ids(value, &mut ids);
    ids
}

fn collect_entity_ids(value: &Value, ids: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(id_val) = map.get("entity_id")
                && let Some(s) = id_val.as_str()
            {
                ids.insert(s.to_string());
            }
            // Also collect from entity_ids arrays
            if let Some(Value::Array(arr)) = map.get("entity_ids") {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        ids.insert(s.to_string());
                    }
                }
            }
            for val in map.values() {
                collect_entity_ids(val, ids);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_entity_ids(item, ids);
            }
        }
        _ => {}
    }
}

/// Flatten all string values from a JSON value into a single string for substring matching.
fn flatten_args_text(value: &Value) -> String {
    let mut parts = Vec::new();
    collect_strings(value, &mut parts);
    parts.join(" ")
}

fn collect_strings(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::String(s) => parts.push(s.clone()),
        Value::Array(arr) => {
            for item in arr {
                collect_strings(item, parts);
            }
        }
        Value::Object(map) => {
            for val in map.values() {
                collect_strings(val, parts);
            }
        }
        _ => {}
    }
}

/// Estimate token count from JSON byte size (1 token ~= 4 bytes).
fn estimate_tokens(trace: &ToolCallTrace) -> u64 {
    let args_bytes = serde_json::to_string(&trace.arguments)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    let response_bytes = serde_json::to_string(&trace.response)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    (args_bytes + response_bytes) / 4
}

/// Detect which tool calls are "unnecessary": their response entity_ids
/// never appear in any subsequent tool's arguments.
fn detect_unnecessary(traces: &[ToolCallTrace]) -> Vec<bool> {
    let n = traces.len();
    let mut unnecessary = vec![false; n];

    for i in 0..n {
        let response_ids = extract_entity_ids(&traces[i].response);
        if response_ids.is_empty() {
            // No entity_ids in response -- cannot judge, not flagged
            continue;
        }

        // Check if any entity_id appears in any subsequent tool's arguments
        let mut referenced = false;
        for later in &traces[i + 1..] {
            let args_text = flatten_args_text(&Value::Object(
                later
                    .arguments
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ));
            for id in &response_ids {
                if args_text.contains(id.as_str()) {
                    referenced = true;
                    break;
                }
            }
            if referenced {
                break;
            }
        }

        if !referenced {
            unnecessary[i] = true;
        }
    }

    unnecessary
}

/// Grade tool usage from a sequence of tool call traces.
///
/// Returns a `ToolUsageScore` with latency stats, token estimate,
/// unnecessary call count, and efficiency score.
pub fn grade(traces: &[ToolCallTrace]) -> ToolUsageScore {
    if traces.is_empty() {
        return ToolUsageScore {
            total_calls: 0,
            unnecessary_calls: 0,
            total_tokens: 0,
            total_latency: Duration::ZERO,
            efficiency: 1.0,
        };
    }

    let total_calls = traces.len();

    // Latency
    let total_latency_ms: u64 = traces.iter().map(|t| t.latency_ms).sum();

    // Token estimation
    let total_tokens: u64 = traces.iter().map(estimate_tokens).sum();

    // Unnecessary call detection
    let unnecessary_flags = detect_unnecessary(traces);
    let unnecessary_calls = unnecessary_flags.iter().filter(|&&u| u).count();

    // Efficiency: 1.0 - (unnecessary / total)
    let efficiency = 1.0 - (unnecessary_calls as f64 / total_calls as f64);

    ToolUsageScore {
        total_calls,
        unnecessary_calls,
        total_tokens,
        total_latency: Duration::from_millis(total_latency_ms),
        efficiency,
    }
}

/// Compute latency percentiles (p50, p95, p99) from traces.
///
/// Returns `(p50, p95, p99)` in milliseconds.
/// Returns `(0, 0, 0)` for empty traces.
pub fn latency_percentiles(traces: &[ToolCallTrace]) -> (u64, u64, u64) {
    if traces.is_empty() {
        return (0, 0, 0);
    }

    let mut latencies: Vec<u64> = traces.iter().map(|t| t.latency_ms).collect();
    latencies.sort_unstable();

    let p50 = percentile(&latencies, 50.0);
    let p95 = percentile(&latencies, 95.0);
    let p99 = percentile(&latencies, 99.0);

    (p50, p95, p99)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    // ── Helpers ───────────────────────────────────────────────────

    fn make_trace(tool: &str, args: Value, response: Value, latency_ms: u64) -> ToolCallTrace {
        let arguments = match args {
            Value::Object(map) => map.into_iter().collect::<HashMap<String, Value>>(),
            _ => HashMap::new(),
        };
        ToolCallTrace {
            tool: tool.to_string(),
            arguments,
            response,
            latency_ms,
            success: true,
        }
    }

    // ── Percentile tests ─────────────────────────────────────────

    #[test]
    fn tool_usage_percentile_single_value() {
        assert_eq!(percentile(&[42], 50.0), 42);
        assert_eq!(percentile(&[42], 95.0), 42);
        assert_eq!(percentile(&[42], 99.0), 42);
    }

    #[test]
    fn tool_usage_percentile_sorted_values() {
        // 10 values: indices 0..9
        let sorted = vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let p50 = percentile(&sorted, 50.0);
        assert_eq!(p50, 50, "p50 of 10 values should be 5th element");

        let p95 = percentile(&sorted, 95.0);
        assert_eq!(p95, 100, "p95 of 10 values should be near max");

        let p99 = percentile(&sorted, 99.0);
        assert_eq!(p99, 100, "p99 of 10 values should be max");
    }

    #[test]
    fn tool_usage_latency_percentiles_from_traces() {
        let traces: Vec<ToolCallTrace> = (1..=20)
            .map(|i| make_trace("tool", json!({}), json!({}), i * 10))
            .collect();

        let (p50, p95, p99) = latency_percentiles(&traces);
        assert_eq!(p50, 100, "p50 of 10..200 should be ~100ms");
        assert_eq!(p95, 190, "p95 of 10..200 should be ~190ms");
        assert_eq!(p99, 200, "p99 of 10..200 should be ~200ms");
    }

    #[test]
    fn tool_usage_latency_percentiles_empty() {
        let (p50, p95, p99) = latency_percentiles(&[]);
        assert_eq!(p50, 0);
        assert_eq!(p95, 0);
        assert_eq!(p99, 0);
    }

    // ── Entity ID extraction ─────────────────────────────────────

    #[test]
    fn tool_usage_extract_entity_ids_from_response() {
        let response = json!({
            "action": "Created",
            "entity_id": "ent-001",
            "nested": {
                "entity_id": "ent-002"
            }
        });
        let ids = extract_entity_ids(&response);
        assert!(ids.contains("ent-001"));
        assert!(ids.contains("ent-002"));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn tool_usage_extract_entity_ids_from_array() {
        let response = json!({
            "results": [
                {"entity_id": "ent-a"},
                {"entity_id": "ent-b"}
            ]
        });
        let ids = extract_entity_ids(&response);
        assert!(ids.contains("ent-a"));
        assert!(ids.contains("ent-b"));
    }

    #[test]
    fn tool_usage_extract_entity_ids_empty() {
        let response = json!({"action": "OK"});
        let ids = extract_entity_ids(&response);
        assert!(ids.is_empty());
    }

    // ── Unnecessary call detection ───────────────────────────────

    #[test]
    fn tool_usage_unnecessary_call_detected() {
        // Call 1: smart_ingest returns entity_id "ent-001"
        // Call 2: hybrid_search does NOT reference "ent-001" in args
        // => Call 1 is unnecessary (its output was never used)
        let traces = vec![
            make_trace(
                "smart_ingest",
                json!({"content": "Alice is an engineer"}),
                json!({"action": "Created", "entity_id": "ent-001"}),
                50,
            ),
            make_trace(
                "hybrid_search",
                json!({"query": "something unrelated"}),
                json!({"results": []}),
                30,
            ),
        ];

        let flags = detect_unnecessary(&traces);
        assert!(
            flags[0],
            "Call 0 should be unnecessary (ent-001 never referenced later)"
        );
        // Call 1 is the last call -- its output can't be referenced by anything
        // so it has entity_ids but no subsequent calls. Still flagged.
    }

    #[test]
    fn tool_usage_necessary_call_not_flagged() {
        // Call 1: smart_ingest returns entity_id "ent-001"
        // Call 2: retrieve_entities references "ent-001" in its args
        // => Call 1 is necessary
        let traces = vec![
            make_trace(
                "smart_ingest",
                json!({"content": "Alice is an engineer"}),
                json!({"action": "Created", "entity_id": "ent-001"}),
                50,
            ),
            make_trace(
                "retrieve_entities",
                json!({"entity_ids": "ent-001"}),
                json!({"entities": [{"name": "Alice"}]}),
                30,
            ),
        ];

        let flags = detect_unnecessary(&traces);
        assert!(
            !flags[0],
            "Call 0 should be necessary (ent-001 is referenced in call 1)"
        );
    }

    // ── Token estimation ─────────────────────────────────────────

    #[test]
    fn tool_usage_token_estimation() {
        let trace = make_trace(
            "smart_ingest",
            json!({"content": "Hello world"}),
            json!({"action": "Created", "entity_id": "ent-001"}),
            50,
        );
        let tokens = estimate_tokens(&trace);
        // args ~= 24 bytes, response ~= 48 bytes => (24+48)/4 = 18 tokens (approx)
        assert!(tokens > 0, "Token estimate should be positive");
        assert!(tokens < 1000, "Token estimate should be reasonable");
    }

    // ── Efficiency score ─────────────────────────────────────────

    #[test]
    fn tool_usage_efficiency_perfect_sequence() {
        // All calls are necessary: each returns entity_id used by the next.
        // The last call returns no entity_ids so it isn't penalized.
        let traces = vec![
            make_trace(
                "smart_ingest",
                json!({"content": "Alice is an engineer"}),
                json!({"action": "Created", "entity_id": "ent-001"}),
                50,
            ),
            make_trace(
                "retrieve_entities",
                json!({"entity_ids": "ent-001"}),
                json!({"entities": [{"name": "Alice"}]}),
                30,
            ),
        ];

        let score = grade(&traces);
        assert_eq!(score.unnecessary_calls, 0);
        assert!(
            (score.efficiency - 1.0).abs() < f64::EPSILON,
            "Perfect sequence should have efficiency 1.0, got {}",
            score.efficiency
        );
    }

    #[test]
    fn tool_usage_efficiency_with_unnecessary_call() {
        // Call 1: smart_ingest -> ent-001 (unnecessary: ent-001 not used later)
        // Call 2: hybrid_search -> no entity_ids (not flaggable)
        // Call 3: smart_ingest -> ent-002 (unnecessary: last call, nothing after)
        let traces = vec![
            make_trace(
                "smart_ingest",
                json!({"content": "Alice"}),
                json!({"action": "Created", "entity_id": "ent-001"}),
                50,
            ),
            make_trace(
                "hybrid_search",
                json!({"query": "search unrelated"}),
                json!({"results": []}),
                30,
            ),
            make_trace(
                "smart_ingest",
                json!({"content": "Bob"}),
                json!({"action": "Created", "entity_id": "ent-002"}),
                40,
            ),
        ];

        let score = grade(&traces);
        assert_eq!(score.total_calls, 3);
        // ent-001 is not referenced by call 2 or 3 -> unnecessary
        // ent-002 is the last call (no subsequent calls reference it) -> unnecessary
        assert!(
            score.unnecessary_calls >= 1,
            "At least one unnecessary call should be detected, got {}",
            score.unnecessary_calls
        );
        assert!(
            score.efficiency < 1.0,
            "Efficiency should be less than 1.0 with unnecessary calls, got {}",
            score.efficiency
        );
    }

    #[test]
    fn tool_usage_sequence_with_unnecessary_scores_lower_than_optimal() {
        // Optimal: ingest -> retrieve (entity_id flows through, last call has no entity_ids)
        let optimal = vec![
            make_trace(
                "smart_ingest",
                json!({"content": "Alice"}),
                json!({"action": "Created", "entity_id": "ent-001"}),
                50,
            ),
            make_trace(
                "retrieve_entities",
                json!({"entity_ids": "ent-001"}),
                json!({"entities": [{"name": "Alice"}]}),
                30,
            ),
        ];

        // Suboptimal: ingest -> unrelated_search -> retrieve
        // The unrelated_search's response has entity_ids that go unused.
        let suboptimal = vec![
            make_trace(
                "smart_ingest",
                json!({"content": "Alice"}),
                json!({"action": "Created", "entity_id": "ent-001"}),
                50,
            ),
            make_trace(
                "hybrid_search",
                json!({"query": "unrelated"}),
                json!({"results": [{"entity_id": "ent-999"}]}),
                30,
            ),
            make_trace(
                "retrieve_entities",
                json!({"entity_ids": "ent-001"}),
                json!({"entities": [{"name": "Alice"}]}),
                30,
            ),
        ];

        let optimal_score = grade(&optimal);
        let suboptimal_score = grade(&suboptimal);

        assert!(
            suboptimal_score.efficiency < optimal_score.efficiency,
            "Suboptimal sequence ({:.2}) should have lower efficiency than optimal ({:.2})",
            suboptimal_score.efficiency,
            optimal_score.efficiency
        );
        assert!(
            suboptimal_score.unnecessary_calls > optimal_score.unnecessary_calls,
            "Suboptimal should have more unnecessary calls ({}) than optimal ({})",
            suboptimal_score.unnecessary_calls,
            optimal_score.unnecessary_calls
        );
    }

    // ── Empty traces ─────────────────────────────────────────────

    #[test]
    fn tool_usage_empty_traces() {
        let score = grade(&[]);
        assert_eq!(score.total_calls, 0);
        assert_eq!(score.unnecessary_calls, 0);
        assert_eq!(score.total_tokens, 0);
        assert_eq!(score.total_latency, Duration::ZERO);
        assert!((score.efficiency - 1.0).abs() < f64::EPSILON);
    }

    // ── Total latency ────────────────────────────────────────────

    #[test]
    fn tool_usage_total_latency_sums_correctly() {
        let traces = vec![
            make_trace("a", json!({}), json!({}), 100),
            make_trace("b", json!({}), json!({}), 200),
            make_trace("c", json!({}), json!({}), 150),
        ];

        let score = grade(&traces);
        assert_eq!(score.total_latency, Duration::from_millis(450));
    }

    // ── No entity_ids means not flagged ──────────────────────────

    #[test]
    fn tool_usage_calls_without_entity_ids_not_flagged() {
        // Both calls return no entity_ids -- neither can be flagged
        let traces = vec![
            make_trace("get_stats", json!({}), json!({"entity_count": 10}), 20),
            make_trace("get_stats", json!({}), json!({"entity_count": 10}), 20),
        ];

        let score = grade(&traces);
        assert_eq!(score.unnecessary_calls, 0);
        assert!((score.efficiency - 1.0).abs() < f64::EPSILON);
    }
}
