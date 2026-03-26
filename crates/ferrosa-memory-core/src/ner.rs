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
}
