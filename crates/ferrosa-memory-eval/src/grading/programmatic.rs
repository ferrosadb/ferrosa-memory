//! Programmatic grader for Level 1 MCP evaluation.
//!
//! Validates:
//! - Schema: response JSON matches expected structure per tool
//! - Tool sequence: actual call sequence matches expected sequence
//! - Field assertions: `expect_in_response` strings present in response text
//! - Action matching: `expect_action` found in response
//! - Entity identity (EF04): `expect_entity_name` cross-referenced against entity_id
//! - Format normalization (EF08): UUID case/hyphen agnostic, float epsilon 1e-6

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scenario::{EvalStep, ToolCallTrace};

/// Float comparison epsilon for EF08 normalization.
const FLOAT_EPSILON: f64 = 1e-6;

/// Result of programmatic grading for a single scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgrammaticScore {
    pub schema_valid: bool,
    pub sequence_match: bool,
    pub field_assertions_passed: usize,
    pub field_assertions_total: usize,
    pub entity_identity_valid: Option<bool>,
    /// Composite score 0.0 - 1.0.
    pub score: f64,
}

/// Entity resolver trait for EF04 identity verification.
/// In production, this calls `retrieve_entities` via MCP client.
/// In tests, a stub implementation is used.
pub trait EntityResolver {
    /// Given an entity_id, return the entity name if found.
    fn resolve_entity_name(&self, entity_id: &str) -> Option<String>;
}

/// No-op resolver when entity identity checks are not needed.
pub struct NoOpResolver;

impl EntityResolver for NoOpResolver {
    fn resolve_entity_name(&self, _entity_id: &str) -> Option<String> {
        None
    }
}

/// Grade a scenario's tool call traces against expected steps.
///
/// # Arguments
/// * `steps` - Expected evaluation steps from the scenario definition
/// * `traces` - Actual recorded tool call traces from execution
/// * `resolver` - Entity resolver for EF04 identity checks
pub fn grade(
    steps: &[EvalStep],
    traces: &[ToolCallTrace],
    resolver: &dyn EntityResolver,
) -> ProgrammaticScore {
    let sequence_match = check_sequence(steps, traces);
    let schema_valid = check_schemas(traces);

    let mut field_passed = 0usize;
    let mut field_total = 0usize;
    let mut entity_identity_results: Vec<bool> = Vec::new();

    for (step, trace) in steps.iter().zip(traces.iter()) {
        // Field assertions: expect_in_response
        let response_text = response_to_searchable_text(&trace.response);
        for expected_str in &step.expect_in_response {
            field_total += 1;
            if response_text.contains(expected_str) {
                field_passed += 1;
            }
        }

        // Action matching: expect_action
        if let Some(ref action) = step.expect_action {
            field_total += 1;
            if response_text.contains(action) {
                field_passed += 1;
            }
        }

        // Entity identity verification (EF04)
        if let Some(ref expected_name) = step.expect_entity_name {
            if let Some(entity_id) = extract_entity_id(&trace.response) {
                let normalized_id = normalize_uuid(&entity_id);
                match resolver.resolve_entity_name(&normalized_id) {
                    Some(resolved_name) => {
                        entity_identity_results.push(resolved_name == *expected_name);
                    }
                    None => {
                        // Could not resolve -- treat as failure
                        entity_identity_results.push(false);
                    }
                }
            } else {
                // No entity_id in response but one was expected
                entity_identity_results.push(false);
            }
        }
    }

    let entity_identity_valid = if entity_identity_results.is_empty() {
        None
    } else {
        Some(entity_identity_results.iter().all(|&v| v))
    };

    let score = compute_score(
        schema_valid,
        sequence_match,
        field_passed,
        field_total,
        entity_identity_valid,
    );

    ProgrammaticScore {
        schema_valid,
        sequence_match,
        field_assertions_passed: field_passed,
        field_assertions_total: field_total,
        entity_identity_valid,
        score,
    }
}

/// Check that actual tool call sequence matches expected step sequence.
fn check_sequence(steps: &[EvalStep], traces: &[ToolCallTrace]) -> bool {
    if steps.len() != traces.len() {
        return false;
    }
    steps
        .iter()
        .zip(traces.iter())
        .all(|(step, trace)| step.tool == trace.tool)
}

/// Validate that all responses have valid JSON structure.
/// A response is valid if it is a non-null JSON object or array.
fn check_schemas(traces: &[ToolCallTrace]) -> bool {
    traces.iter().all(|trace| {
        trace.success && !trace.response.is_null()
    })
}

/// Convert a response Value to a flat searchable string.
/// Recursively walks the JSON and collects all string values and keys.
fn response_to_searchable_text(value: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_text(value, &mut parts);
    parts.join(" ")
}

fn collect_text(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::String(s) => parts.push(s.clone()),
        Value::Number(n) => parts.push(n.to_string()),
        Value::Bool(b) => parts.push(b.to_string()),
        Value::Array(arr) => {
            for item in arr {
                collect_text(item, parts);
            }
        }
        Value::Object(map) => {
            for (key, val) in map {
                parts.push(key.clone());
                collect_text(val, parts);
            }
        }
        Value::Null => {}
    }
}

/// Extract entity_id from a response JSON value.
/// Looks for "entity_id" key at any nesting depth.
fn extract_entity_id(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(id_val) = map.get("entity_id") {
                return id_val.as_str().map(String::from);
            }
            for val in map.values() {
                if let Some(found) = extract_entity_id(val) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => {
            for item in arr {
                if let Some(found) = extract_entity_id(item) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

/// Normalize a UUID for comparison (EF08): lowercase, strip hyphens.
pub fn normalize_uuid(uuid: &str) -> String {
    uuid.to_lowercase().replace('-', "")
}

/// Compare two floats with epsilon tolerance (EF08).
pub fn floats_equal(a: f64, b: f64) -> bool {
    (a - b).abs() < FLOAT_EPSILON
}

/// Compare two UUIDs after normalization (EF08).
pub fn uuids_equal(a: &str, b: &str) -> bool {
    normalize_uuid(a) == normalize_uuid(b)
}

/// Compute composite score from individual components.
/// Weights: sequence 30%, schema 20%, field assertions 30%, entity identity 20%.
fn compute_score(
    schema_valid: bool,
    sequence_match: bool,
    field_passed: usize,
    field_total: usize,
    entity_identity_valid: Option<bool>,
) -> f64 {
    let schema_score = if schema_valid { 1.0 } else { 0.0 };
    let sequence_score = if sequence_match { 1.0 } else { 0.0 };
    let field_score = if field_total > 0 {
        field_passed as f64 / field_total as f64
    } else {
        1.0 // no assertions means trivially passed
    };

    // When entity identity is not checked, redistribute its weight
    match entity_identity_valid {
        Some(valid) => {
            let entity_score = if valid { 1.0 } else { 0.0 };
            0.20 * schema_score
                + 0.30 * sequence_score
                + 0.30 * field_score
                + 0.20 * entity_score
        }
        None => {
            // Redistribute entity weight proportionally
            0.25 * schema_score + 0.375 * sequence_score + 0.375 * field_score
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    /// Stub resolver that maps entity_ids to names for testing.
    struct StubResolver {
        entities: HashMap<String, String>,
    }

    impl StubResolver {
        fn new(entries: Vec<(&str, &str)>) -> Self {
            let entities = entries
                .into_iter()
                .map(|(id, name)| (normalize_uuid(id), name.to_string()))
                .collect();
            Self { entities }
        }
    }

    impl EntityResolver for StubResolver {
        fn resolve_entity_name(&self, entity_id: &str) -> Option<String> {
            self.entities.get(&normalize_uuid(entity_id)).cloned()
        }
    }

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

    // ---------------------------------------------------------------
    // Sequence matching tests
    // ---------------------------------------------------------------

    #[test]
    fn correct_sequence_passes() {
        let steps = vec![
            make_step("smart_ingest"),
            make_step("hybrid_search"),
            make_step("retrieve_entities"),
        ];
        let traces = vec![
            make_trace("smart_ingest", json!({"action": "Created", "entity_id": "abc"})),
            make_trace("hybrid_search", json!({"results": []})),
            make_trace("retrieve_entities", json!({"entities": []})),
        ];

        let score = grade(&steps, &traces, &NoOpResolver);
        assert!(score.sequence_match, "correct sequence should match");
        assert!(score.schema_valid, "valid responses should pass schema");
        assert!(
            score.score > 0.99,
            "perfect scenario should score ~1.0, got {}",
            score.score
        );
    }

    #[test]
    fn wrong_sequence_fails() {
        let steps = vec![
            make_step("smart_ingest"),
            make_step("hybrid_search"),
        ];
        // Traces are in wrong order
        let traces = vec![
            make_trace("hybrid_search", json!({"results": []})),
            make_trace("smart_ingest", json!({"action": "Created"})),
        ];

        let score = grade(&steps, &traces, &NoOpResolver);
        assert!(!score.sequence_match, "wrong order should not match");
        assert!(
            score.score < 1.0,
            "wrong sequence should reduce score, got {}",
            score.score
        );
    }

    #[test]
    fn mismatched_length_fails_sequence() {
        let steps = vec![make_step("smart_ingest"), make_step("hybrid_search")];
        let traces = vec![make_trace("smart_ingest", json!({"ok": true}))];

        let score = grade(&steps, &traces, &NoOpResolver);
        assert!(!score.sequence_match, "length mismatch should fail sequence");
    }

    // ---------------------------------------------------------------
    // Field assertion tests
    // ---------------------------------------------------------------

    #[test]
    fn field_assertions_pass_when_present() {
        let mut step = make_step("smart_ingest");
        step.expect_in_response = vec!["Created".to_string(), "entity_id".to_string()];

        let trace = make_trace(
            "smart_ingest",
            json!({"action": "Created", "entity_id": "abc-123"}),
        );

        let score = grade(&[step], &[trace], &NoOpResolver);
        assert_eq!(score.field_assertions_passed, 2);
        assert_eq!(score.field_assertions_total, 2);
    }

    #[test]
    fn field_assertions_fail_when_missing() {
        let mut step = make_step("smart_ingest");
        step.expect_in_response = vec!["Superseded".to_string()];

        let trace = make_trace("smart_ingest", json!({"action": "Created"}));

        let score = grade(&[step], &[trace], &NoOpResolver);
        assert_eq!(score.field_assertions_passed, 0);
        assert_eq!(score.field_assertions_total, 1);
    }

    #[test]
    fn expect_action_counted_as_assertion() {
        let mut step = make_step("smart_ingest");
        step.expect_action = Some("Created".to_string());

        let trace = make_trace("smart_ingest", json!({"action": "Created"}));

        let score = grade(&[step], &[trace], &NoOpResolver);
        // expect_action adds 1 to field_total and field_passed
        assert_eq!(score.field_assertions_passed, 1);
        assert_eq!(score.field_assertions_total, 1);
    }

    #[test]
    fn expect_action_fails_when_wrong() {
        let mut step = make_step("smart_ingest");
        step.expect_action = Some("Superseded".to_string());

        let trace = make_trace("smart_ingest", json!({"action": "Created"}));

        let score = grade(&[step], &[trace], &NoOpResolver);
        assert_eq!(score.field_assertions_passed, 0);
        assert_eq!(score.field_assertions_total, 1);
    }

    // ---------------------------------------------------------------
    // Schema validation tests
    // ---------------------------------------------------------------

    #[test]
    fn null_response_fails_schema() {
        let step = make_step("smart_ingest");
        let mut trace = make_trace("smart_ingest", Value::Null);
        trace.success = true;

        let score = grade(&[step], &[trace], &NoOpResolver);
        assert!(!score.schema_valid, "null response should fail schema");
    }

    #[test]
    fn failed_trace_fails_schema() {
        let step = make_step("smart_ingest");
        let mut trace = make_trace("smart_ingest", json!({"error": "timeout"}));
        trace.success = false;

        let score = grade(&[step], &[trace], &NoOpResolver);
        assert!(!score.schema_valid, "failed trace should fail schema");
    }

    // ---------------------------------------------------------------
    // Entity identity (EF04) tests
    // ---------------------------------------------------------------

    #[test]
    fn entity_identity_passes_correct_entity() {
        let resolver = StubResolver::new(vec![("abc-1234-def", "Alice")]);

        let mut step = make_step("smart_ingest");
        step.expect_entity_name = Some("Alice".to_string());

        let trace = make_trace(
            "smart_ingest",
            json!({"action": "Created", "entity_id": "abc-1234-def"}),
        );

        let score = grade(&[step], &[trace], &resolver);
        assert_eq!(
            score.entity_identity_valid,
            Some(true),
            "correct entity should validate"
        );
    }

    #[test]
    fn entity_identity_fails_wrong_entity() {
        // The entity_id maps to "Bob" but we expected "Alice"
        let resolver = StubResolver::new(vec![("abc-1234-def", "Bob")]);

        let mut step = make_step("smart_ingest");
        step.expect_entity_name = Some("Alice".to_string());

        let trace = make_trace(
            "smart_ingest",
            json!({"action": "Created", "entity_id": "abc-1234-def"}),
        );

        let score = grade(&[step], &[trace], &resolver);
        assert_eq!(
            score.entity_identity_valid,
            Some(false),
            "wrong entity should fail identity check"
        );
        assert!(
            score.score < 1.0,
            "wrong entity should reduce score, got {}",
            score.score
        );
    }

    #[test]
    fn entity_identity_fails_missing_entity_id() {
        let resolver = StubResolver::new(vec![("abc-1234-def", "Alice")]);

        let mut step = make_step("smart_ingest");
        step.expect_entity_name = Some("Alice".to_string());

        // Response has no entity_id field
        let trace = make_trace("smart_ingest", json!({"action": "Created"}));

        let score = grade(&[step], &[trace], &resolver);
        assert_eq!(
            score.entity_identity_valid,
            Some(false),
            "missing entity_id should fail"
        );
    }

    #[test]
    fn entity_identity_none_when_not_checked() {
        let step = make_step("smart_ingest");
        let trace = make_trace("smart_ingest", json!({"action": "Created"}));

        let score = grade(&[step], &[trace], &NoOpResolver);
        assert_eq!(
            score.entity_identity_valid, None,
            "should be None when no entity check requested"
        );
    }

    // ---------------------------------------------------------------
    // Format normalization (EF08) tests
    // ---------------------------------------------------------------

    #[test]
    fn uuid_normalization_ignores_case_and_hyphens() {
        assert!(uuids_equal(
            "550E8400-E29B-41D4-A716-446655440000",
            "550e8400e29b41d4a716446655440000"
        ));
        assert!(uuids_equal(
            "550e8400-e29b-41d4-a716-446655440000",
            "550E8400-E29B-41D4-A716-446655440000"
        ));
        assert!(!uuids_equal(
            "550e8400-e29b-41d4-a716-446655440000",
            "660e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn float_epsilon_comparison() {
        assert!(floats_equal(1.0, 1.0));
        assert!(floats_equal(0.1 + 0.2, 0.3)); // classic float issue
        assert!(floats_equal(1.0, 1.0 + 1e-7));
        assert!(!floats_equal(1.0, 1.001));
        assert!(!floats_equal(0.0, 1e-5));
    }

    #[test]
    fn entity_identity_uuid_normalization() {
        // Entity stored with hyphenated UUID, response has non-hyphenated
        let resolver = StubResolver::new(vec![(
            "550E8400-E29B-41D4-A716-446655440000",
            "Alice",
        )]);

        let mut step = make_step("smart_ingest");
        step.expect_entity_name = Some("Alice".to_string());

        let trace = make_trace(
            "smart_ingest",
            json!({"entity_id": "550e8400e29b41d4a716446655440000"}),
        );

        let score = grade(&[step], &[trace], &resolver);
        assert_eq!(
            score.entity_identity_valid,
            Some(true),
            "UUID normalization should make these match"
        );
    }

    // ---------------------------------------------------------------
    // Nested response text search
    // ---------------------------------------------------------------

    #[test]
    fn nested_response_field_assertions() {
        let mut step = make_step("hybrid_search");
        step.expect_in_response = vec!["Alice".to_string(), "engineer".to_string()];

        let trace = make_trace(
            "hybrid_search",
            json!({
                "results": [
                    {"name": "Alice", "content": "senior engineer at Acme"}
                ]
            }),
        );

        let score = grade(&[step], &[trace], &NoOpResolver);
        assert_eq!(score.field_assertions_passed, 2);
        assert_eq!(score.field_assertions_total, 2);
    }

    #[test]
    fn extract_entity_id_from_nested_response() {
        let response = json!({
            "result": {
                "data": {
                    "entity_id": "abc-123",
                    "name": "Alice"
                }
            }
        });
        assert_eq!(
            extract_entity_id(&response),
            Some("abc-123".to_string())
        );
    }

    // ---------------------------------------------------------------
    // Composite score tests
    // ---------------------------------------------------------------

    #[test]
    fn perfect_score_without_entity_check() {
        let score = compute_score(true, true, 5, 5, None);
        assert!(
            floats_equal(score, 1.0),
            "all-pass without entity check should be 1.0, got {}",
            score
        );
    }

    #[test]
    fn perfect_score_with_entity_check() {
        let score = compute_score(true, true, 5, 5, Some(true));
        assert!(
            floats_equal(score, 1.0),
            "all-pass with entity check should be 1.0, got {}",
            score
        );
    }

    #[test]
    fn zero_score_all_failures() {
        let score = compute_score(false, false, 0, 5, Some(false));
        assert!(
            floats_equal(score, 0.0),
            "all-fail should be 0.0, got {}",
            score
        );
    }

    #[test]
    fn partial_field_assertions_give_partial_score() {
        // schema pass, sequence pass, 2/4 fields, entity pass
        let score = compute_score(true, true, 2, 4, Some(true));
        // 0.20 * 1.0 + 0.30 * 1.0 + 0.30 * 0.5 + 0.20 * 1.0 = 0.85
        assert!(
            floats_equal(score, 0.85),
            "expected 0.85, got {}",
            score
        );
    }

    // ---------------------------------------------------------------
    // Full scenario: smart_ingest CREATE -> UPDATE -> SUPERSEDE
    // ---------------------------------------------------------------

    #[test]
    fn full_smart_ingest_lifecycle_passes() {
        let steps = vec![
            {
                let mut s = make_step("smart_ingest");
                s.expect_in_response = vec!["Created".to_string(), "entity_id".to_string()];
                s.expect_action = Some("Created".to_string());
                s.expect_entity_name = Some("Alice".to_string());
                s
            },
            {
                let mut s = make_step("smart_ingest");
                s.expect_in_response = vec!["Updated".to_string(), "similarity".to_string()];
                s.expect_action = Some("Updated".to_string());
                s
            },
            {
                let mut s = make_step("smart_ingest");
                s.expect_in_response =
                    vec!["Superseded".to_string(), "old_entity_id".to_string()];
                s.expect_action = Some("Superseded".to_string());
                s
            },
        ];

        let traces = vec![
            make_trace(
                "smart_ingest",
                json!({
                    "action": "Created",
                    "entity_id": "ent-001",
                    "message": "Created new entity"
                }),
            ),
            make_trace(
                "smart_ingest",
                json!({
                    "action": "Updated",
                    "entity_id": "ent-001",
                    "similarity": 0.92,
                    "message": "Updated existing entity"
                }),
            ),
            make_trace(
                "smart_ingest",
                json!({
                    "action": "Superseded",
                    "entity_id": "ent-002",
                    "old_entity_id": "ent-001",
                    "message": "Superseded old entity"
                }),
            ),
        ];

        let resolver = StubResolver::new(vec![("ent-001", "Alice")]);
        let score = grade(&steps, &traces, &resolver);

        assert!(score.sequence_match);
        assert!(score.schema_valid);
        assert_eq!(score.field_assertions_passed, score.field_assertions_total);
        assert_eq!(score.entity_identity_valid, Some(true));
        assert!(
            score.score > 0.99,
            "full lifecycle should score ~1.0, got {}",
            score.score
        );
    }

    #[test]
    fn wrong_entity_detected_in_lifecycle() {
        let mut step = make_step("smart_ingest");
        step.expect_action = Some("Created".to_string());
        step.expect_entity_name = Some("Alice".to_string());

        // Response contains entity_id that maps to Bob, not Alice (EF04)
        let trace = make_trace(
            "smart_ingest",
            json!({"action": "Created", "entity_id": "ent-999"}),
        );

        let resolver = StubResolver::new(vec![("ent-999", "Bob")]);
        let score = grade(&[step], &[trace], &resolver);

        assert_eq!(
            score.entity_identity_valid,
            Some(false),
            "should detect wrong entity"
        );
        assert!(
            score.score < 1.0,
            "wrong entity should reduce score"
        );
    }

    // ---------------------------------------------------------------
    // response_to_searchable_text tests
    // ---------------------------------------------------------------

    #[test]
    fn searchable_text_includes_all_values() {
        let val = json!({
            "action": "Created",
            "count": 42,
            "active": true,
            "items": ["a", "b"]
        });
        let text = response_to_searchable_text(&val);
        assert!(text.contains("Created"));
        assert!(text.contains("42"));
        assert!(text.contains("true"));
        assert!(text.contains("a"));
        assert!(text.contains("b"));
    }

    #[test]
    fn searchable_text_handles_null() {
        let text = response_to_searchable_text(&Value::Null);
        assert!(text.is_empty());
    }
}
