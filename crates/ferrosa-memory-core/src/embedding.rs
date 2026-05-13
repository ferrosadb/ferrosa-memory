//! HTTP client for embedding endpoints (Ollama, OpenAI-compatible).
//!
//! The embedding client calls out to an external endpoint to generate vector
//! embeddings for text. It does not run a model itself — it's a thin HTTP
//! client with timeout and health check support.
//!
//! ## Graceful degradation
//!
//! If the embedding endpoint is down, tools that require embeddings
//! (memo store, fold retrieval, entity retrieval) fail fast with a clear error.
//! Tools that don't need embeddings (plan_tools, feedback_tools) are unaffected.

use serde::{Deserialize, Serialize};

use crate::config::EmbeddingConfig;

/// Embedding client errors.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding endpoint unavailable: {0}")]
    Unavailable(String),
    #[error("embedding request failed: {0}")]
    RequestFailed(String),
    #[error("unexpected response format: {0}")]
    BadResponse(String),
    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: u32, actual: usize },
}

/// Ollama embedding request body.
#[derive(Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
}

/// Ollama embedding response body.
#[derive(Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f64>>,
}

/// HTTP client for generating text embeddings.
pub struct EmbeddingClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    dimensions: u32,
    max_input_chars: usize,
}

impl EmbeddingClient {
    /// Create a new embedding client from config.
    ///
    /// The HTTP client is configured with a 10-second timeout to fail fast
    /// if the embedding endpoint is unavailable (FMEA F25).
    pub fn new(config: &EmbeddingConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");

        Self {
            http,
            base_url: config.ollama_base_url.clone(),
            model: config.model.clone(),
            dimensions: config.dimensions,
            max_input_chars: config.max_input_chars,
        }
    }

    /// Generate an embedding for a single text input.
    ///
    /// # Errors
    ///
    /// - [`EmbeddingError::Unavailable`] if the endpoint can't be reached
    /// - [`EmbeddingError::DimensionMismatch`] if the response has wrong dimensions
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let chunks = chunk_text_for_embedding(text, self.max_input_chars);
        let mut accum: Option<Vec<f64>> = None;
        let mut count = 0usize;

        for chunk in chunks {
            let embedding = self.embed_one(&chunk).await?;
            if let Some(ref mut sum) = accum {
                for (slot, value) in sum.iter_mut().zip(embedding) {
                    *slot += value;
                }
            } else {
                accum = Some(embedding);
            }
            count += 1;
        }

        let Some(mut averaged) = accum else {
            return Err(EmbeddingError::BadResponse(
                "no embedding chunks produced".into(),
            ));
        };
        for value in &mut averaged {
            *value /= count as f64;
        }
        Ok(averaged.into_iter().map(|v| v as f32).collect())
    }

    async fn embed_one(&self, text: &str) -> Result<Vec<f64>, EmbeddingError> {
        let url = format!("{}/api/embed", self.base_url);
        let body = OllamaEmbedRequest {
            model: &self.model,
            input: text,
        };

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbeddingError::Unavailable(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(EmbeddingError::RequestFailed(format!(
                "status {}",
                resp.status()
            )));
        }

        let parsed: OllamaEmbedResponse = resp
            .json()
            .await
            .map_err(|e| EmbeddingError::BadResponse(e.to_string()))?;

        let embedding = parsed
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::BadResponse("empty embeddings array".into()))?;

        if embedding.len() != self.dimensions as usize {
            return Err(EmbeddingError::DimensionMismatch {
                expected: self.dimensions,
                actual: embedding.len(),
            });
        }

        Ok(embedding)
    }

    /// Health check: verify the embedding endpoint is reachable AND the
    /// configured model is loaded.
    ///
    /// Returns `Err(Unavailable)` if Ollama can't be reached, or
    /// `Err(RequestFailed)` with a clear message if the configured model is
    /// not in the loaded model list.
    pub async fn health_check(&self) -> Result<(), EmbeddingError> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| EmbeddingError::Unavailable(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(EmbeddingError::Unavailable(format!(
                "status {}",
                resp.status()
            )));
        }

        let body: OllamaTagsResponse = resp
            .json()
            .await
            .map_err(|e| EmbeddingError::BadResponse(e.to_string()))?;

        if !is_model_loaded(&body.models, &self.model) {
            return Err(EmbeddingError::RequestFailed(format!(
                "model '{}' not loaded; loaded models: {}",
                self.model,
                body.models
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        Ok(())
    }
}

fn chunk_text_for_embedding(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;

    for ch in text.chars() {
        if current_chars == max_chars {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current.push(ch);
        current_chars += 1;
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Shape of Ollama's `/api/tags` response (subset we care about).
#[derive(Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaLoadedModel>,
}

#[derive(Deserialize)]
struct OllamaLoadedModel {
    name: String,
}

/// Check if a model matching `target` is in the loaded list.
///
/// Ollama appends `:latest` to unqualified names (`nomic-embed-text` becomes
/// `nomic-embed-text:latest` in the tags output). Match on both the full name
/// and the prefix-before-colon so config can specify either form.
fn is_model_loaded(loaded: &[OllamaLoadedModel], target: &str) -> bool {
    loaded.iter().any(|m| {
        m.name == target
            || m.name.split(':').next() == Some(target)
            || target.split(':').next() == m.name.split(':').next()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn client_creation_with_defaults() {
        let config = EmbeddingConfig::default();
        let client = EmbeddingClient::new(&config);
        assert_eq!(client.dimensions, 768);
        assert_eq!(client.model, "nomic-embed-text-v2-moe");
        assert_eq!(client.max_input_chars, 6_000);
    }

    #[test]
    fn embedding_chunks_keep_oversized_input_below_configured_limit() {
        let chunks = chunk_text_for_embedding("abcdefghijklmnop", 5);
        assert_eq!(chunks, vec!["abcde", "fghij", "klmno", "p"]);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 5));
    }

    #[tokio::test]
    async fn embed_sends_long_inputs_as_bounded_chunks_and_averages_vectors() {
        let (base_url, requests, handle) = spawn_embedding_server(vec![
            "{\"embeddings\":[[1.0,3.0]]}",
            "{\"embeddings\":[[3.0,5.0]]}",
            "{\"embeddings\":[[5.0,7.0]]}",
        ]);
        let config = EmbeddingConfig {
            ollama_base_url: base_url,
            dimensions: 2,
            max_input_chars: 4,
            ..EmbeddingConfig::default()
        };

        let embedding = EmbeddingClient::new(&config)
            .embed("aaaabbbbcccc")
            .await
            .unwrap();

        assert_eq!(embedding, vec![3.0, 5.0]);
        let bodies = requests.lock().unwrap().clone();
        assert_eq!(bodies.len(), 3);
        assert!(bodies.iter().all(|body| body.contains("\"input\":")));
        assert!(bodies.iter().any(|body| body.contains("aaaa")));
        assert!(bodies.iter().any(|body| body.contains("bbbb")));
        assert!(bodies.iter().any(|body| body.contains("cccc")));
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn embed_rejects_ollama_error_payload_even_when_http_status_is_success() {
        let (base_url, _requests, handle) =
            spawn_embedding_server(vec!["{\"error\":\"input exceeds context length\"}"]);
        let config = EmbeddingConfig {
            ollama_base_url: base_url,
            dimensions: 2,
            max_input_chars: 100,
            ..EmbeddingConfig::default()
        };

        let err = EmbeddingClient::new(&config)
            .embed("too long")
            .await
            .unwrap_err();
        assert!(
            matches!(err, EmbeddingError::BadResponse(_)),
            "HTTP 200 with an Ollama error payload must fail hard, got {err:?}"
        );
        handle.join().unwrap();
    }

    fn spawn_embedding_server(
        responses: Vec<&'static str>,
    ) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for response_body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{addr}"), requests, handle)
    }

    fn model(name: &str) -> OllamaLoadedModel {
        OllamaLoadedModel { name: name.into() }
    }

    #[test]
    fn is_model_loaded_exact_match() {
        let loaded = vec![model("nomic-embed-text:latest")];
        assert!(is_model_loaded(&loaded, "nomic-embed-text:latest"));
    }

    #[test]
    fn is_model_loaded_target_unqualified_loaded_latest() {
        // Config says "nomic-embed-text"; Ollama reports "nomic-embed-text:latest".
        let loaded = vec![model("nomic-embed-text:latest")];
        assert!(is_model_loaded(&loaded, "nomic-embed-text"));
    }

    #[test]
    fn is_model_loaded_target_latest_loaded_unqualified() {
        let loaded = vec![model("nomic-embed-text")];
        assert!(is_model_loaded(&loaded, "nomic-embed-text:latest"));
    }

    #[test]
    fn is_model_loaded_rejects_missing_model() {
        let loaded = vec![model("qwen3.5:latest"), model("gemma4:26b")];
        assert!(!is_model_loaded(&loaded, "nomic-embed-text"));
    }

    #[test]
    fn is_model_loaded_rejects_empty_list() {
        assert!(!is_model_loaded(&[], "nomic-embed-text"));
    }
}
