//! LLM-backed named entity recognition via Ollama.
//!
//! Two-tier classification:
//! 1. Fast heuristic pass (`infer_entity_type`) — handles
//!    obvious cases (acronyms, org suffixes, known tools, common names).
//! 2. LLM fallback ([`llm_classify_entity`]) — sends ambiguous entities to
//!    a local Ollama model for classification.
//!
//! The LLM is only called when the heuristic returns "concept" (uncertain).

use serde::Deserialize;

/// Entity types the classifier can return.
const VALID_TYPES: &[&str] = &[
    "person",
    "organization",
    "tool",
    "project",
    "place",
    "event",
    "concept",
];

/// Ollama generate response (non-streaming).
#[derive(Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}

/// Classify an entity name using the local Ollama LLM.
///
/// Sends a structured prompt to the model asking it to classify the entity
/// into one of the known types. Falls back to "concept" on any error.
pub async fn llm_classify_entity(
    http: &reqwest::Client,
    ollama_base_url: &str,
    model: &str,
    entity_name: &str,
    context: &str,
) -> String {
    match llm_classify_inner(http, ollama_base_url, model, entity_name, context).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(entity = entity_name, error = %e, "LLM NER failed, using heuristic");
            crate::smart_ingest::infer_entity_type(entity_name).to_string()
        }
    }
}

async fn llm_classify_inner(
    http: &reqwest::Client,
    ollama_base_url: &str,
    model: &str,
    entity_name: &str,
    context: &str,
) -> anyhow::Result<String> {
    let context_line = if context.is_empty() {
        String::new()
    } else {
        format!("\nContext: {context}")
    };

    let prompt = format!(
        "/no_think\nClassify this entity into exactly one type.\n\
         Entity: {entity_name}{context_line}\n\
         Types: person, organization, tool, project, place, event, concept\n\
         Reply with ONLY the type, nothing else."
    );

    let body = serde_json::json!({
        "model": model,
        "prompt": prompt,
        "stream": false,
        "options": {
            "temperature": 0.0,
            "num_predict": 10
        }
    });

    let resp = http
        .post(format!("{ollama_base_url}/api/generate"))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Ollama returned {}", resp.status());
    }

    let parsed: OllamaGenerateResponse = resp.json().await?;
    let raw = parsed.response.trim().to_lowercase();

    // Extract the first valid type from the response.
    let classified = VALID_TYPES
        .iter()
        .find(|&&t| raw.contains(t))
        .copied()
        .unwrap_or("concept");

    Ok(classified.to_string())
}

/// Classify an entity using heuristics first, falling back to LLM for
/// ambiguous ("concept") cases.
pub async fn classify_entity(
    http: &reqwest::Client,
    ollama_base_url: &str,
    model: &str,
    entity_name: &str,
    context: &str,
) -> String {
    let heuristic = crate::smart_ingest::infer_entity_type(entity_name);
    if heuristic != "concept" {
        return heuristic.to_string();
    }

    // Heuristic was uncertain — ask the LLM.
    llm_classify_entity(http, ollama_base_url, model, entity_name, context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_types_are_lowercase() {
        for t in VALID_TYPES {
            assert_eq!(*t, t.to_lowercase(), "type must be lowercase");
        }
    }

    #[test]
    fn valid_types_contains_expected_set() {
        assert!(VALID_TYPES.contains(&"person"));
        assert!(VALID_TYPES.contains(&"organization"));
        assert!(VALID_TYPES.contains(&"tool"));
        assert!(VALID_TYPES.contains(&"project"));
        assert!(VALID_TYPES.contains(&"place"));
        assert!(VALID_TYPES.contains(&"event"));
        assert!(VALID_TYPES.contains(&"concept"));
        assert_eq!(VALID_TYPES.len(), 7);
    }

    #[test]
    fn heuristic_shortcuts_known_types() {
        // classify_entity is async but the heuristic path doesn't need the LLM.
        // We test the heuristic directly.
        assert_ne!(crate::smart_ingest::infer_entity_type("Docker"), "concept");
        assert_ne!(
            crate::smart_ingest::infer_entity_type("Ben Kearns"),
            "concept"
        );
        assert_ne!(crate::smart_ingest::infer_entity_type("AWS"), "concept");
    }

    #[test]
    fn heuristic_shortcut_returns_tool_for_known_tool() {
        assert_eq!(crate::smart_ingest::infer_entity_type("Docker"), "tool");
        assert_eq!(crate::smart_ingest::infer_entity_type("Kubernetes"), "tool");
    }

    #[test]
    fn heuristic_shortcut_returns_person_for_name() {
        assert_eq!(
            crate::smart_ingest::infer_entity_type("Alice Smith"),
            "person"
        );
    }

    #[test]
    fn heuristic_shortcut_returns_org_for_acronym() {
        assert_eq!(
            crate::smart_ingest::infer_entity_type("NASA"),
            "organization"
        );
    }

    #[test]
    fn heuristic_returns_concept_for_ambiguous() {
        // "Memory Lifecycle" is not a known tool, org, or person
        assert_eq!(
            crate::smart_ingest::infer_entity_type("Memory Lifecycle"),
            "concept"
        );
    }

    #[tokio::test]
    async fn classify_entity_uses_heuristic_for_known_tool() {
        let http = reqwest::Client::new();
        // classify_entity should short-circuit on heuristic — no LLM call needed
        let result =
            classify_entity(&http, "http://invalid:99999", "fake-model", "Docker", "").await;
        assert_eq!(result, "tool");
    }

    #[tokio::test]
    async fn classify_entity_uses_heuristic_for_person() {
        let http = reqwest::Client::new();
        let result = classify_entity(
            &http,
            "http://invalid:99999",
            "fake-model",
            "Ben Kearns",
            "",
        )
        .await;
        assert_eq!(result, "person");
    }

    #[tokio::test]
    async fn classify_entity_uses_heuristic_for_org_acronym() {
        let http = reqwest::Client::new();
        let result = classify_entity(&http, "http://invalid:99999", "fake-model", "IBM", "").await;
        assert_eq!(result, "organization");
    }

    #[tokio::test]
    async fn classify_entity_uses_heuristic_for_org_suffix() {
        let http = reqwest::Client::new();
        let result = classify_entity(
            &http,
            "http://invalid:99999",
            "fake-model",
            "Mozilla Foundation",
            "",
        )
        .await;
        assert_eq!(result, "organization");
    }

    #[tokio::test]
    async fn classify_entity_falls_back_for_concept() {
        // "Dream Consolidation" is a concept — heuristic returns "concept",
        // so it tries LLM, which fails (invalid URL), then falls back to heuristic again.
        let http = reqwest::Client::new();
        let result = classify_entity(
            &http,
            "http://invalid:99999",
            "fake-model",
            "Dream Consolidation",
            "",
        )
        .await;
        // LLM fails, fallback to heuristic which returns "concept"
        assert_eq!(result, "concept");
    }

    #[tokio::test]
    async fn llm_classify_entity_falls_back_on_network_error() {
        let http = reqwest::Client::new();
        let result = llm_classify_entity(
            &http,
            "http://invalid:99999",
            "fake-model",
            "SomeEntity",
            "some context",
        )
        .await;
        // Should fall back to heuristic result, not panic
        assert!(!result.is_empty());
    }

    /// Test parsing logic: extract valid type from response containing "person"
    #[test]
    fn extract_valid_type_from_raw_response() {
        let raw = "person";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "person");
    }

    /// Test parsing logic: response with extra whitespace/text
    #[test]
    fn extract_valid_type_from_noisy_response() {
        let raw = "the entity is a tool for developers";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "tool");
    }

    /// Test parsing logic: response with multiple types picks the first match
    #[test]
    fn extract_first_valid_type_from_multi_type_response() {
        // "person" comes before "organization" in VALID_TYPES
        let raw = "person or organization";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "person");
    }

    /// Test parsing logic: garbage response falls back to concept
    #[test]
    fn garbage_response_falls_back_to_concept() {
        let raw = "xyz123 not a real type";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "concept");
    }

    /// Test parsing logic: empty response falls back to concept
    #[test]
    fn empty_response_falls_back_to_concept() {
        let raw = "";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "concept");
    }

    /// Test parsing logic: response with only whitespace falls back to concept
    #[test]
    fn whitespace_response_falls_back_to_concept() {
        let raw = "   \n\t  ";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "concept");
    }

    /// Test that "event" is correctly extracted
    #[test]
    fn extract_event_type() {
        let raw = "event";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "event");
    }

    /// Test that "place" is correctly extracted
    #[test]
    fn extract_place_type() {
        let raw = "this is a place";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "place");
    }

    /// Test that "project" is correctly extracted
    #[test]
    fn extract_project_type() {
        let raw = "project";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "project");
    }

    /// Test that "organization" is correctly extracted
    #[test]
    fn extract_organization_type() {
        let raw = "organization";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "organization");
    }

    /// Response with uppercase type still matches after lowercasing
    #[test]
    fn uppercase_response_handled_by_lowercase_conversion() {
        // Simulating what llm_classify_inner does: raw = parsed.response.trim().to_lowercase()
        let raw = "TOOL".to_lowercase();
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "tool");
    }

    /// Response with mixed case and extra text
    #[test]
    fn mixed_case_response_with_extra_text() {
        let raw = "I think it is a Person".to_lowercase();
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "person");
    }

    /// Deserialization of OllamaGenerateResponse
    #[test]
    fn deserialize_ollama_response() {
        let json = r#"{"response": "  tool\n"}"#;
        let parsed: OllamaGenerateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.response.trim(), "tool");
    }

    /// Deserialization of OllamaGenerateResponse with empty response
    #[test]
    fn deserialize_ollama_response_empty() {
        let json = r#"{"response": ""}"#;
        let parsed: OllamaGenerateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.response, "");
    }

    /// Prompt construction: context line is empty when context is empty
    #[test]
    fn context_line_empty_when_no_context() {
        let context = "";
        let context_line = if context.is_empty() {
            String::new()
        } else {
            format!("\nContext: {context}")
        };
        assert_eq!(context_line, "");
    }

    /// Prompt construction: context line included when context is non-empty
    #[test]
    fn context_line_included_when_context_provided() {
        let context = "used in a Rust project";
        let context_line = if context.is_empty() {
            String::new()
        } else {
            format!("\nContext: {context}")
        };
        assert_eq!(context_line, "\nContext: used in a Rust project");
    }

    /// classify_entity heuristic path for person with common name
    #[tokio::test]
    async fn classify_entity_heuristic_common_name_person() {
        let http = reqwest::Client::new();
        let result = classify_entity(
            &http,
            "http://invalid:99999",
            "fake-model",
            "Alice Johnson",
            "",
        )
        .await;
        assert_eq!(result, "person");
    }

    /// classify_entity heuristic path for known tool
    #[tokio::test]
    async fn classify_entity_heuristic_redis_tool() {
        let http = reqwest::Client::new();
        let result =
            classify_entity(&http, "http://invalid:99999", "fake-model", "Redis", "").await;
        assert_eq!(result, "tool");
    }

    /// classify_entity heuristic path for org suffix
    #[tokio::test]
    async fn classify_entity_heuristic_org_university() {
        let http = reqwest::Client::new();
        let result = classify_entity(
            &http,
            "http://invalid:99999",
            "fake-model",
            "Stanford University",
            "",
        )
        .await;
        assert_eq!(result, "organization");
    }

    /// llm_classify_entity fallback returns non-empty for ambiguous entity
    #[tokio::test]
    async fn llm_classify_fallback_returns_heuristic_for_ambiguous() {
        let http = reqwest::Client::new();
        let result = llm_classify_entity(
            &http,
            "http://invalid:99999",
            "fake-model",
            "Memory Lifecycle",
            "",
        )
        .await;
        // Falls back to heuristic, which returns "concept"
        assert_eq!(result, "concept");
    }

    /// llm_classify_entity fallback for a known tool still returns "tool"
    #[tokio::test]
    async fn llm_classify_fallback_returns_tool_for_known() {
        let http = reqwest::Client::new();
        let result = llm_classify_entity(
            &http,
            "http://invalid:99999",
            "fake-model",
            "Kubernetes",
            "container orchestration",
        )
        .await;
        // LLM fails (invalid URL), falls back to heuristic which returns "tool"
        assert_eq!(result, "tool");
    }

    /// Response containing "concept" embedded in another word still matches
    #[test]
    fn response_with_concept_substring() {
        // "concept" appears as a standalone word
        let raw = "the concept is";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        // "person" comes first in VALID_TYPES but isn't present
        assert_eq!(classified, "concept");
    }

    /// VALID_TYPES does not contain duplicates
    #[test]
    fn valid_types_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for t in VALID_TYPES {
            assert!(seen.insert(*t), "duplicate type found: {t}");
        }
    }

    /// Response with "place" surrounded by text
    #[test]
    fn extract_place_from_sentence() {
        let raw = "this entity refers to a place on the map";
        let classified = VALID_TYPES
            .iter()
            .find(|&&t| raw.contains(t))
            .copied()
            .unwrap_or("concept");
        assert_eq!(classified, "place");
    }
}
