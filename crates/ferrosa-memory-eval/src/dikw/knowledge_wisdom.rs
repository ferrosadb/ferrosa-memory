//! DIKW Knowledge-to-Wisdom Analyzer (T-020).
//!
//! Evaluates transformation from connected knowledge to actionable wisdom:
//! - Intention trigger verification: check_intentions triggers on correct context
//! - Context correctness (EF12): verify trigger context matches ground truth,
//!   unrelated context must NOT trigger
//! - Smart ingest decision scoring: CREATE/UPDATE/SUPERSEDE match ground truth
//! - predict_needed accuracy: predictions match actual access patterns
//!
//! Risk mitigation:
//! - EF12 (RPN 196): Intention triggers on wrong context — ground truth specifies
//!   expected trigger context; compare actual against expected; negative test for
//!   unrelated context.

use serde_json::Value;

use crate::report::TransitionScore;
use crate::scenario::{EvalStep, ToolCallTrace};

// ---------------------------------------------------------------------------
// Intention trigger analysis
// ---------------------------------------------------------------------------

/// Result of checking whether an intention triggered correctly.
#[derive(Debug, Clone)]
pub struct IntentionTriggerResult {
    /// Whether the intention triggered at all.
    pub triggered: bool,
    /// Whether the trigger context matches the expected context.
    pub context_correct: bool,
    /// The actual context that caused the trigger (if any).
    pub actual_context: Option<String>,
    /// The expected context from ground truth.
    pub expected_context: String,
}

/// Parse check_intentions response to determine if any intention triggered.
///
/// Expected response shape:
/// ```json
/// {"intentions": [{"id": "...", "triggered": true, "context": "..."}]}
/// ```
pub fn parse_intention_triggers(response: &Value) -> Vec<(bool, Option<String>)> {
    let inner = extract_inner_json(response);
    let intentions_arr = inner
        .get("intentions")
        .or_else(|| inner.get("results"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    intentions_arr
        .iter()
        .map(|intent| {
            let triggered = intent
                .get("triggered")
                .and_then(|v| v.as_bool())
                .or_else(|| {
                    // Some responses use "status" field
                    intent
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| s == "triggered" || s == "active")
                })
                .unwrap_or(false);
            let context = intent
                .get("context")
                .or_else(|| intent.get("trigger_context"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (triggered, context)
        })
        .collect()
}

/// Verify intention trigger correctness (EF12).
///
/// Checks that:
/// 1. The intention triggered when expected context is provided
/// 2. The trigger context matches the expected ground truth context
pub fn verify_intention_trigger(
    response: &Value,
    expected_context: &str,
) -> IntentionTriggerResult {
    let triggers = parse_intention_triggers(response);

    // Find any trigger that fired
    let triggered_entry = triggers.iter().find(|(triggered, _)| *triggered);

    match triggered_entry {
        Some((_, actual_ctx)) => {
            let context_correct = actual_ctx
                .as_deref()
                .map(|ctx| ctx.contains(expected_context) || expected_context.contains(ctx))
                .unwrap_or(false);

            IntentionTriggerResult {
                triggered: true,
                context_correct,
                actual_context: actual_ctx.clone(),
                expected_context: expected_context.to_string(),
            }
        }
        None => IntentionTriggerResult {
            triggered: false,
            context_correct: false,
            actual_context: None,
            expected_context: expected_context.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// Smart ingest decision scoring
// ---------------------------------------------------------------------------

/// Parse the action from a smart_ingest response.
///
/// Returns the action string (e.g., "Created", "Updated", "Superseded").
pub fn parse_smart_ingest_action(response: &Value) -> Option<String> {
    let inner = extract_inner_json(response);
    inner
        .get("action")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Score smart_ingest decisions against ground truth.
///
/// Returns (correct_count, total_count).
pub fn score_smart_ingest_decisions(
    steps: &[EvalStep],
    traces: &[ToolCallTrace],
) -> (usize, usize) {
    let mut correct = 0;
    let mut total = 0;

    for (step, trace) in steps.iter().zip(traces.iter()) {
        if trace.tool != "smart_ingest" {
            continue;
        }

        if let Some(ref expected_action) = step.expect_action {
            total += 1;
            if let Some(actual_action) = parse_smart_ingest_action(&trace.response)
                && actual_action.to_lowercase() == expected_action.to_lowercase() {
                    correct += 1;
                }
        }
    }

    (correct, total)
}

// ---------------------------------------------------------------------------
// predict_needed accuracy
// ---------------------------------------------------------------------------

/// Parse predict_needed response to get predicted entity names/ids.
pub fn parse_predictions(response: &Value) -> Vec<String> {
    let inner = extract_inner_json(response);
    let predictions_arr = inner
        .get("predictions")
        .or_else(|| inner.get("predicted"))
        .or_else(|| inner.get("results"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    predictions_arr
        .iter()
        .filter_map(|p| {
            p.get("entity_name")
                .or_else(|| p.get("name"))
                .or_else(|| p.get("entity_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Score predict_needed accuracy against actual access patterns.
///
/// Returns (precision, recall) tuple.
pub fn score_predictions(predicted: &[String], actual_accessed: &[String]) -> (f64, f64) {
    if predicted.is_empty() && actual_accessed.is_empty() {
        return (1.0, 1.0);
    }

    let predicted_set: std::collections::HashSet<&str> =
        predicted.iter().map(|s| s.as_str()).collect();
    let actual_set: std::collections::HashSet<&str> =
        actual_accessed.iter().map(|s| s.as_str()).collect();

    // Precision: of predicted, how many were actually accessed?
    let precision = if predicted.is_empty() {
        0.0
    } else {
        let true_positives = predicted_set.intersection(&actual_set).count();
        true_positives as f64 / predicted.len() as f64
    };

    // Recall: of actually accessed, how many were predicted?
    let recall = if actual_accessed.is_empty() {
        1.0 // nothing to predict
    } else {
        let true_positives = actual_set.intersection(&predicted_set).count();
        true_positives as f64 / actual_accessed.len() as f64
    };

    (precision, recall)
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

/// Analyze scenario traces for Knowledge-to-Wisdom transition.
///
/// Weights: intention triggers 40%, smart ingest 35%, predictions 25%.
///
/// # Arguments
/// * `steps` - Expected scenario steps with ground truth
/// * `traces` - Actual recorded tool call traces
/// * `expected_trigger_context` - Ground truth trigger context for EF12
/// * `expected_predictions` - Ground truth for predict_needed
pub fn analyze(
    steps: &[EvalStep],
    traces: &[ToolCallTrace],
    expected_trigger_context: Option<&str>,
    expected_predictions: &[String],
) -> TransitionScore {
    // 1. Intention trigger verification
    let (intention_score, intention_detail) = score_intentions(traces, expected_trigger_context);

    // 2. Smart ingest decision scoring
    let (ingest_correct, ingest_total) = score_smart_ingest_decisions(steps, traces);
    let ingest_score = if ingest_total == 0 {
        1.0 // no smart_ingest steps with expect_action
    } else {
        ingest_correct as f64 / ingest_total as f64
    };
    let ingest_detail = format!("smart_ingest: {ingest_correct}/{ingest_total} decisions correct");

    // 3. predict_needed accuracy
    let (predict_score, predict_detail) = score_predict_needed(traces, expected_predictions);

    // Weighted composite
    let composite = intention_score * 0.40 + ingest_score * 0.35 + predict_score * 0.25;

    let detail = format!("{intention_detail}; {ingest_detail}; {predict_detail}");

    TransitionScore {
        label: "knowledge_to_wisdom".to_string(),
        score: composite,
        detail,
    }
}

/// Score intention triggers across all check_intentions traces.
fn score_intentions(traces: &[ToolCallTrace], expected_context: Option<&str>) -> (f64, String) {
    let check_traces: Vec<&ToolCallTrace> = traces
        .iter()
        .filter(|t| t.tool == "check_intentions")
        .collect();

    if check_traces.is_empty() {
        return (0.0, "no check_intentions calls found".to_string());
    }

    match expected_context {
        Some(expected) => {
            let mut correct_triggers = 0;
            let mut total_checks = 0;

            for trace in &check_traces {
                total_checks += 1;
                let result = verify_intention_trigger(&trace.response, expected);
                if result.triggered && result.context_correct {
                    correct_triggers += 1;
                }
            }

            let score = correct_triggers as f64 / total_checks as f64;
            let detail = format!(
                "intentions: {correct_triggers}/{total_checks} triggered with correct context"
            );
            (score, detail)
        }
        None => {
            // No expected context => just check that triggers exist
            let triggered_count = check_traces
                .iter()
                .filter(|t| {
                    let triggers = parse_intention_triggers(&t.response);
                    triggers.iter().any(|(triggered, _)| *triggered)
                })
                .count();

            let score = triggered_count as f64 / check_traces.len() as f64;
            let detail = format!(
                "intentions: {triggered_count}/{} triggered (no context check)",
                check_traces.len()
            );
            (score, detail)
        }
    }
}

/// Score predict_needed results across all traces.
fn score_predict_needed(
    traces: &[ToolCallTrace],
    expected_predictions: &[String],
) -> (f64, String) {
    let predict_traces: Vec<&ToolCallTrace> = traces
        .iter()
        .filter(|t| t.tool == "predict_needed")
        .collect();

    if predict_traces.is_empty() {
        if expected_predictions.is_empty() {
            return (1.0, "no predict_needed calls (none expected)".to_string());
        }
        return (0.0, "no predict_needed calls found".to_string());
    }

    // Aggregate all predictions
    let mut all_predicted: Vec<String> = Vec::new();
    for trace in &predict_traces {
        all_predicted.extend(parse_predictions(&trace.response));
    }

    let (precision, recall) = score_predictions(&all_predicted, expected_predictions);
    // F1 score
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };

    let detail =
        format!("predict_needed: precision={precision:.2}, recall={recall:.2}, F1={f1:.2}");
    (f1, detail)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn make_step(tool: &str) -> EvalStep {
        EvalStep {
            tool: tool.to_string(),
            arguments: HashMap::new(),
            expect_in_response: vec![],
            expect_action: None,
            expect_entity_name: None,
        }
    }

    fn make_trace(tool: &str, response: Value) -> ToolCallTrace {
        ToolCallTrace {
            tool: tool.to_string(),
            arguments: HashMap::new(),
            response,
            latency_ms: 50,
            success: true,
        }
    }

    fn make_intention_response(intentions: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "intentions": intentions })).unwrap()
            }]
        })
    }

    fn make_predict_response(predictions: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "predictions": predictions })).unwrap()
            }]
        })
    }

    // ---------------------------------------------------------------
    // Intention trigger verification (EF12)
    // ---------------------------------------------------------------

    #[test]
    fn intention_triggers_on_correct_context() {
        let response = make_intention_response(vec![json!({
            "id": "int-001",
            "triggered": true,
            "context": "branch:feature/xyz"
        })]);

        let result = verify_intention_trigger(&response, "branch:feature/xyz");
        assert!(result.triggered, "intention should have triggered");
        assert!(result.context_correct, "context should match ground truth");
    }

    #[test]
    fn intention_does_not_trigger_on_wrong_context() {
        let response = make_intention_response(vec![json!({
            "id": "int-001",
            "triggered": false,
            "context": null
        })]);

        let result = verify_intention_trigger(&response, "branch:feature/xyz");
        assert!(!result.triggered, "intention should not trigger");
        assert!(
            !result.context_correct,
            "non-triggered intention has incorrect context"
        );
    }

    #[test]
    fn intention_triggers_on_wrong_context_scored_incorrect() {
        let response = make_intention_response(vec![json!({
            "id": "int-001",
            "triggered": true,
            "context": "branch:main"
        })]);

        let result = verify_intention_trigger(&response, "branch:feature/xyz");
        assert!(result.triggered, "intention did trigger");
        assert!(
            !result.context_correct,
            "wrong context should be scored as incorrect (EF12)"
        );
    }

    #[test]
    fn multiple_intentions_first_triggered_wins() {
        let response = make_intention_response(vec![
            json!({"id": "int-001", "triggered": false}),
            json!({"id": "int-002", "triggered": true, "context": "branch:feature/xyz"}),
        ]);

        let result = verify_intention_trigger(&response, "branch:feature/xyz");
        assert!(result.triggered);
        assert!(result.context_correct);
    }

    // ---------------------------------------------------------------
    // Smart ingest decision scoring
    // ---------------------------------------------------------------

    #[test]
    fn smart_ingest_all_correct() {
        let steps = vec![
            {
                let mut s = make_step("smart_ingest");
                s.expect_action = Some("Created".into());
                s
            },
            {
                let mut s = make_step("smart_ingest");
                s.expect_action = Some("Updated".into());
                s
            },
        ];

        let traces = vec![
            make_trace("smart_ingest", json!({"action": "Created"})),
            make_trace("smart_ingest", json!({"action": "Updated"})),
        ];

        let (correct, total) = score_smart_ingest_decisions(&steps, &traces);
        assert_eq!(correct, 2);
        assert_eq!(total, 2);
    }

    #[test]
    fn smart_ingest_wrong_decision() {
        let mut step = make_step("smart_ingest");
        step.expect_action = Some("Superseded".into());

        let trace = make_trace("smart_ingest", json!({"action": "Created"}));

        let (correct, total) = score_smart_ingest_decisions(&[step], &[trace]);
        assert_eq!(correct, 0);
        assert_eq!(total, 1);
    }

    #[test]
    fn smart_ingest_case_insensitive() {
        let mut step = make_step("smart_ingest");
        step.expect_action = Some("created".into());

        let trace = make_trace("smart_ingest", json!({"action": "Created"}));

        let (correct, total) = score_smart_ingest_decisions(&[step], &[trace]);
        assert_eq!(correct, 1, "comparison should be case-insensitive");
        assert_eq!(total, 1);
    }

    #[test]
    fn non_smart_ingest_steps_ignored() {
        let mut step = make_step("hybrid_search");
        step.expect_action = Some("search".into());

        let trace = make_trace("hybrid_search", json!({"action": "nope"}));

        let (correct, total) = score_smart_ingest_decisions(&[step], &[trace]);
        assert_eq!(total, 0, "non-smart_ingest steps should be ignored");
        assert_eq!(correct, 0);
    }

    // ---------------------------------------------------------------
    // predict_needed accuracy
    // ---------------------------------------------------------------

    #[test]
    fn perfect_predictions() {
        let predicted = vec!["Alice".into(), "Bob".into()];
        let actual = vec!["Alice".into(), "Bob".into()];
        let (precision, recall) = score_predictions(&predicted, &actual);
        assert!(
            (precision - 1.0).abs() < 0.01,
            "precision should be 1.0, got {precision}"
        );
        assert!(
            (recall - 1.0).abs() < 0.01,
            "recall should be 1.0, got {recall}"
        );
    }

    #[test]
    fn overprediction_lowers_precision() {
        let predicted = vec!["Alice".into(), "Bob".into(), "Carol".into()];
        let actual = vec!["Alice".into()];
        let (precision, recall) = score_predictions(&predicted, &actual);
        assert!(
            (precision - 1.0 / 3.0).abs() < 0.01,
            "precision should be 1/3, got {precision}"
        );
        assert!(
            (recall - 1.0).abs() < 0.01,
            "recall should be 1.0, got {recall}"
        );
    }

    #[test]
    fn underprediction_lowers_recall() {
        let predicted = vec!["Alice".into()];
        let actual = vec!["Alice".into(), "Bob".into(), "Carol".into()];
        let (precision, recall) = score_predictions(&predicted, &actual);
        assert!(
            (precision - 1.0).abs() < 0.01,
            "precision should be 1.0, got {precision}"
        );
        assert!(
            (recall - 1.0 / 3.0).abs() < 0.01,
            "recall should be 1/3, got {recall}"
        );
    }

    #[test]
    fn empty_predictions_and_actual() {
        let (precision, recall) = score_predictions(&[], &[]);
        assert!((precision - 1.0).abs() < 0.01);
        assert!((recall - 1.0).abs() < 0.01);
    }

    // ---------------------------------------------------------------
    // Parse helpers
    // ---------------------------------------------------------------

    #[test]
    fn parse_smart_ingest_action_from_mcp_wrapper() {
        let response = json!({
            "content": [{
                "type": "text",
                "text": "{\"action\": \"Superseded\", \"entity_id\": \"e1\"}"
            }]
        });
        let action = parse_smart_ingest_action(&response);
        assert_eq!(action.as_deref(), Some("Superseded"));
    }

    #[test]
    fn parse_smart_ingest_action_direct() {
        let response = json!({"action": "Created"});
        let action = parse_smart_ingest_action(&response);
        assert_eq!(action.as_deref(), Some("Created"));
    }

    #[test]
    fn parse_predictions_from_mcp_response() {
        let response = make_predict_response(vec![
            json!({"entity_name": "Alice"}),
            json!({"entity_name": "Bob"}),
        ]);
        let predictions = parse_predictions(&response);
        assert_eq!(predictions, vec!["Alice", "Bob"]);
    }

    // ---------------------------------------------------------------
    // Full analyzer integration
    // ---------------------------------------------------------------

    #[test]
    fn analyze_correct_triggers_and_decisions_scores_high() {
        let steps = vec![
            make_step("set_intention"),
            make_step("check_intentions"),
            {
                let mut s = make_step("smart_ingest");
                s.expect_action = Some("Created".into());
                s
            },
            make_step("predict_needed"),
        ];

        let traces = vec![
            make_trace("set_intention", json!({"status": "ok"})),
            make_trace(
                "check_intentions",
                json!({
                    "intentions": [{
                        "id": "int-001",
                        "triggered": true,
                        "context": "branch:feature/xyz"
                    }]
                }),
            ),
            make_trace("smart_ingest", json!({"action": "Created"})),
            make_trace(
                "predict_needed",
                json!({
                    "predictions": [
                        {"entity_name": "Alice"},
                        {"entity_name": "Bob"}
                    ]
                }),
            ),
        ];

        let expected_predictions = vec!["Alice".into(), "Bob".into()];
        let result = analyze(
            &steps,
            &traces,
            Some("branch:feature/xyz"),
            &expected_predictions,
        );

        assert_eq!(result.label, "knowledge_to_wisdom");
        assert!(
            result.score > 0.8,
            "correct triggers + decisions should score > 0.8, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_wrong_trigger_context_scores_lower() {
        let steps = vec![make_step("check_intentions")];
        let traces = vec![make_trace(
            "check_intentions",
            json!({
                "intentions": [{
                    "id": "int-001",
                    "triggered": true,
                    "context": "branch:main"
                }]
            }),
        )];

        let result = analyze(&steps, &traces, Some("branch:feature/xyz"), &[]);

        // Intention triggered but wrong context => intention_score = 0.0
        // No smart_ingest => 1.0 (trivial), no predictions => 1.0
        // 0.0 * 0.4 + 1.0 * 0.35 + 1.0 * 0.25 = 0.60
        assert!(
            result.score < 0.7,
            "wrong trigger context should cap score, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_empty_traces_scores_low() {
        let result = analyze(&[], &[], Some("branch:feature/xyz"), &["Alice".into()]);
        // No check_intentions => 0.0, no smart_ingest => 1.0 (trivial),
        // no predict_needed but expected => 0.0
        // 0.0 * 0.4 + 1.0 * 0.35 + 0.0 * 0.25 = 0.35
        assert!(
            result.score < 0.4,
            "empty traces with expectations should score low, got {}",
            result.score
        );
    }
}
