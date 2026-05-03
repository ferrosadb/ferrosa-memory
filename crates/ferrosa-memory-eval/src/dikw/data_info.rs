//! DIKW Data-to-Information Analyzer (T-018).
//!
//! Evaluates how well raw data is transformed into structured information:
//! - Entity type assignment: entities have non-default types (not just "concept")
//! - Temporal scoping: temporal facts have valid event_time and optional valid_until
//! - Session isolation: all entities belong to expected session_id
//!
//! Risk mitigation:
//! - EF10 (RPN 120): Settle delay before reading state to allow eventual consistency

use serde_json::Value;
use uuid::Uuid;

use crate::report::TransitionScore;
use crate::scenario::ToolCallTrace;

/// Default fallback entity type that should be penalized (EF05).
const FALLBACK_TYPE: &str = "concept";

/// Minimum settle delay in milliseconds before reading state (EF10).
pub const SETTLE_DELAY_MS: u64 = 100;

// ---------------------------------------------------------------------------
// Entity analysis helpers
// ---------------------------------------------------------------------------

/// A parsed entity entry from a retrieve_entities or get_stats response.
#[derive(Debug, Clone)]
pub struct EntityEntry {
    pub entity_id: String,
    pub entity_type: String,
    pub session_id: Option<String>,
}

/// Parse entity entries from an MCP retrieve_entities response.
///
/// Expected shape (MCP content wrapper):
/// ```json
/// {"content": [{"type": "text", "text": "{\"entities\": [...]}"}]}
/// ```
/// Or direct: `{"entities": [{"entity_id": "...", "entity_type": "...", ...}]}`
pub fn parse_entities(response: &Value) -> Vec<EntityEntry> {
    let inner = extract_inner_json(response);
    let entities_arr = inner
        .get("entities")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    entities_arr
        .iter()
        .filter_map(|e| {
            let entity_id = e.get("entity_id").and_then(|v| v.as_str())?.to_string();
            let entity_type = e
                .get("entity_type")
                .and_then(|v| v.as_str())
                .unwrap_or(FALLBACK_TYPE)
                .to_string();
            let session_id = e
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(EntityEntry {
                entity_id,
                entity_type,
                session_id,
            })
        })
        .collect()
}

/// A parsed temporal fact from a get_temporal_chain response.
#[derive(Debug, Clone)]
pub struct TemporalFact {
    pub entity_id: String,
    pub event_time: Option<String>,
    pub valid_until: Option<String>,
}

/// Parse temporal facts from an MCP get_temporal_chain response.
pub fn parse_temporal_facts(response: &Value) -> Vec<TemporalFact> {
    let inner = extract_inner_json(response);
    let facts_arr = inner
        .get("facts")
        .or_else(|| inner.get("temporal_chain"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    facts_arr
        .iter()
        .filter_map(|f| {
            let entity_id = f.get("entity_id").and_then(|v| v.as_str())?.to_string();
            let event_time = f
                .get("event_time")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let valid_until = f
                .get("valid_until")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(TemporalFact {
                entity_id,
                event_time,
                valid_until,
            })
        })
        .collect()
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

/// Score entity type assignment: proportion with non-default types.
///
/// Entities typed as anything other than "concept" score 1.0 each.
/// Entities typed as "concept" (NER fallback) score 0.0.
fn score_entity_types(entities: &[EntityEntry]) -> (f64, String) {
    if entities.is_empty() {
        return (0.0, "no entities to evaluate".to_string());
    }

    let typed_count = entities
        .iter()
        .filter(|e| e.entity_type != FALLBACK_TYPE)
        .count();
    let total = entities.len();
    let ratio = typed_count as f64 / total as f64;

    let detail =
        format!("{typed_count}/{total} entities have non-default types (score: {ratio:.2})");
    (ratio, detail)
}

/// Score temporal scoping: proportion of temporal facts with valid event_time.
fn score_temporal_scoping(facts: &[TemporalFact]) -> (f64, String) {
    if facts.is_empty() {
        return (
            1.0,
            "no temporal facts to evaluate (trivially valid)".to_string(),
        );
    }

    let valid_count = facts.iter().filter(|f| f.event_time.is_some()).count();
    let total = facts.len();
    let ratio = valid_count as f64 / total as f64;

    let detail =
        format!("{valid_count}/{total} temporal facts have valid event_time (score: {ratio:.2})");
    (ratio, detail)
}

/// Score session isolation: proportion of entities belonging to expected session.
fn score_session_isolation(entities: &[EntityEntry], expected_session: &str) -> (f64, String) {
    if entities.is_empty() {
        return (1.0, "no entities to check session isolation".to_string());
    }

    let matching = entities
        .iter()
        .filter(|e| {
            e.session_id
                .as_deref()
                .map(|s| s == expected_session)
                .unwrap_or(false)
        })
        .count();
    let total = entities.len();
    let ratio = matching as f64 / total as f64;

    let detail = format!(
        "{matching}/{total} entities in expected session {expected_session} (score: {ratio:.2})"
    );
    (ratio, detail)
}

/// Analyze scenario traces to produce a Data-to-Information TransitionScore.
///
/// Weights: entity types 50%, temporal scoping 25%, session isolation 25%.
pub fn analyze(traces: &[ToolCallTrace], expected_session_id: &Uuid) -> TransitionScore {
    let mut all_entities: Vec<EntityEntry> = Vec::new();
    let mut all_temporal: Vec<TemporalFact> = Vec::new();

    for trace in traces {
        match trace.tool.as_str() {
            "retrieve_entities" | "get_stats" => {
                all_entities.extend(parse_entities(&trace.response));
            }
            "get_temporal_chain" | "write_temporal_fact" => {
                all_temporal.extend(parse_temporal_facts(&trace.response));
            }
            _ => {}
        }
    }

    let (type_score, type_detail) = score_entity_types(&all_entities);
    let (temporal_score, temporal_detail) = score_temporal_scoping(&all_temporal);
    let (session_score, session_detail) =
        score_session_isolation(&all_entities, &expected_session_id.to_string());

    // Weighted composite: entity types 50%, temporal 25%, session 25%
    let composite = type_score * 0.50 + temporal_score * 0.25 + session_score * 0.25;

    let detail =
        format!("types: {type_detail}; temporal: {temporal_detail}; session: {session_detail}");

    TransitionScore {
        label: "data_to_info".to_string(),
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

    fn make_entity_response(entities: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "entities": entities })).unwrap()
            }]
        })
    }

    fn make_temporal_response(facts: Vec<Value>) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&json!({ "facts": facts })).unwrap()
            }]
        })
    }

    // ---------------------------------------------------------------
    // Entity type scoring
    // ---------------------------------------------------------------

    #[test]
    fn typed_entities_score_high() {
        let entities = vec![
            EntityEntry {
                entity_id: "e1".into(),
                entity_type: "person".into(),
                session_id: None,
            },
            EntityEntry {
                entity_id: "e2".into(),
                entity_type: "project".into(),
                session_id: None,
            },
            EntityEntry {
                entity_id: "e3".into(),
                entity_type: "organization".into(),
                session_id: None,
            },
        ];
        let (score, _detail) = score_entity_types(&entities);
        assert!(
            score > 0.99,
            "all typed entities should score ~1.0, got {score}"
        );
    }

    #[test]
    fn fallback_entities_score_low() {
        let entities = vec![
            EntityEntry {
                entity_id: "e1".into(),
                entity_type: FALLBACK_TYPE.into(),
                session_id: None,
            },
            EntityEntry {
                entity_id: "e2".into(),
                entity_type: FALLBACK_TYPE.into(),
                session_id: None,
            },
        ];
        let (score, _detail) = score_entity_types(&entities);
        assert!(
            score < 0.01,
            "all fallback entities should score ~0.0, got {score}"
        );
    }

    #[test]
    fn mixed_entities_score_partial() {
        let entities = vec![
            EntityEntry {
                entity_id: "e1".into(),
                entity_type: "person".into(),
                session_id: None,
            },
            EntityEntry {
                entity_id: "e2".into(),
                entity_type: FALLBACK_TYPE.into(),
                session_id: None,
            },
        ];
        let (score, _detail) = score_entity_types(&entities);
        assert!(
            (score - 0.5).abs() < 0.01,
            "half typed should score 0.5, got {score}"
        );
    }

    #[test]
    fn empty_entities_score_zero() {
        let (score, _) = score_entity_types(&[]);
        assert!(score < 0.01, "no entities should score 0.0, got {score}");
    }

    // ---------------------------------------------------------------
    // Temporal scoping
    // ---------------------------------------------------------------

    #[test]
    fn temporal_facts_with_event_time_score_high() {
        let facts = vec![
            TemporalFact {
                entity_id: "e1".into(),
                event_time: Some("2026-01-01T00:00:00Z".into()),
                valid_until: None,
            },
            TemporalFact {
                entity_id: "e2".into(),
                event_time: Some("2026-01-02T00:00:00Z".into()),
                valid_until: Some("2026-06-01T00:00:00Z".into()),
            },
        ];
        let (score, _) = score_temporal_scoping(&facts);
        assert!(
            score > 0.99,
            "all timed facts should score ~1.0, got {score}"
        );
    }

    #[test]
    fn temporal_facts_without_event_time_score_low() {
        let facts = vec![TemporalFact {
            entity_id: "e1".into(),
            event_time: None,
            valid_until: None,
        }];
        let (score, _) = score_temporal_scoping(&facts);
        assert!(
            score < 0.01,
            "fact without event_time should score 0.0, got {score}"
        );
    }

    #[test]
    fn no_temporal_facts_trivially_valid() {
        let (score, _) = score_temporal_scoping(&[]);
        assert!(
            score > 0.99,
            "no temporal facts should be trivially valid (1.0), got {score}"
        );
    }

    // ---------------------------------------------------------------
    // Session isolation
    // ---------------------------------------------------------------

    #[test]
    fn all_entities_in_expected_session() {
        let session = "aaaa-bbbb";
        let entities = vec![
            EntityEntry {
                entity_id: "e1".into(),
                entity_type: "person".into(),
                session_id: Some(session.into()),
            },
            EntityEntry {
                entity_id: "e2".into(),
                entity_type: "project".into(),
                session_id: Some(session.into()),
            },
        ];
        let (score, _) = score_session_isolation(&entities, session);
        assert!(
            score > 0.99,
            "all entities in session should score 1.0, got {score}"
        );
    }

    #[test]
    fn entities_in_wrong_session_score_low() {
        let entities = vec![EntityEntry {
            entity_id: "e1".into(),
            entity_type: "person".into(),
            session_id: Some("wrong-session".into()),
        }];
        let (score, _) = score_session_isolation(&entities, "expected-session");
        assert!(score < 0.01, "wrong session should score 0.0, got {score}");
    }

    // ---------------------------------------------------------------
    // Full analyzer integration
    // ---------------------------------------------------------------

    #[test]
    fn analyze_typed_entities_scores_above_threshold() {
        let session_id = Uuid::new_v4();
        let session_str = session_id.to_string();

        let entities_response = make_entity_response(vec![
            json!({
                "entity_id": "e1",
                "entity_type": "person",
                "session_id": session_str
            }),
            json!({
                "entity_id": "e2",
                "entity_type": "project",
                "session_id": session_str
            }),
            json!({
                "entity_id": "e3",
                "entity_type": "technology",
                "session_id": session_str
            }),
        ]);

        let temporal_response = make_temporal_response(vec![json!({
            "entity_id": "e1",
            "event_time": "2026-01-01T00:00:00Z"
        })]);

        let traces = vec![
            make_trace("retrieve_entities", entities_response),
            make_trace("get_temporal_chain", temporal_response),
        ];

        let result = analyze(&traces, &session_id);
        assert_eq!(result.label, "data_to_info");
        assert!(
            result.score > 0.8,
            "typed entities with temporal facts should score > 0.8, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_fallback_entities_scores_below_threshold() {
        let session_id = Uuid::new_v4();
        let session_str = session_id.to_string();

        let entities_response = make_entity_response(vec![
            json!({
                "entity_id": "e1",
                "entity_type": "concept",
                "session_id": session_str
            }),
            json!({
                "entity_id": "e2",
                "entity_type": "concept",
                "session_id": session_str
            }),
        ]);

        let traces = vec![make_trace("retrieve_entities", entities_response)];

        let result = analyze(&traces, &session_id);
        assert!(
            result.score <= 0.5,
            "fallback-only entities should score <= 0.5, got {}",
            result.score
        );
    }

    #[test]
    fn analyze_empty_traces_returns_low_score() {
        let session_id = Uuid::new_v4();
        let result = analyze(&[], &session_id);
        assert_eq!(result.label, "data_to_info");
        // No entities => type_score=0.0; no temporal => 1.0; no session => 1.0
        // 0.0 * 0.5 + 1.0 * 0.25 + 1.0 * 0.25 = 0.5
        assert!(
            (result.score - 0.5).abs() < 0.01,
            "empty traces should score 0.5, got {}",
            result.score
        );
    }

    // ---------------------------------------------------------------
    // Parse helpers
    // ---------------------------------------------------------------

    #[test]
    fn parse_entities_from_mcp_response() {
        let response = make_entity_response(vec![
            json!({"entity_id": "e1", "entity_type": "person"}),
            json!({"entity_id": "e2"}),
        ]);
        let entities = parse_entities(&response);
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].entity_type, "person");
        assert_eq!(entities[1].entity_type, FALLBACK_TYPE);
    }

    #[test]
    fn parse_entities_from_direct_json() {
        let response = json!({
            "entities": [
                {"entity_id": "e1", "entity_type": "project", "session_id": "s1"}
            ]
        });
        let entities = parse_entities(&response);
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_type, "project");
        assert_eq!(entities[0].session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn parse_temporal_facts_from_mcp_response() {
        let response = make_temporal_response(vec![json!({
            "entity_id": "e1",
            "event_time": "2026-01-01T00:00:00Z",
            "valid_until": "2026-12-31T23:59:59Z"
        })]);
        let facts = parse_temporal_facts(&response);
        assert_eq!(facts.len(), 1);
        assert!(facts[0].event_time.is_some());
        assert!(facts[0].valid_until.is_some());
    }

    // ---------------------------------------------------------------
    // Settle delay constant
    // ---------------------------------------------------------------

    #[test]
    fn settle_delay_within_spec_range() {
        // EF10: 50-200ms range
        assert!(
            SETTLE_DELAY_MS >= 50 && SETTLE_DELAY_MS <= 200,
            "settle delay must be 50-200ms, got {SETTLE_DELAY_MS}"
        );
    }
}
