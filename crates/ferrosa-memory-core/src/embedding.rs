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
use sha2::{Digest, Sha256};

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

/// OpenAI-compatible embedding request body.
///
/// llama.cpp (`llama-server`), vLLM, LM Studio and OpenAI itself all accept
/// this; only Ollama differs.
#[derive(Serialize)]
struct OpenAiEmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
}

/// OpenAI-compatible embedding response body.
#[derive(Deserialize)]
struct OpenAiEmbedResponse {
    data: Vec<OpenAiEmbedDatum>,
}

#[derive(Deserialize)]
struct OpenAiEmbedDatum {
    embedding: Vec<f64>,
}

/// Which wire protocol a provider speaks.
///
/// Runtimes are grouped by PROTOCOL rather than listed individually, so adding
/// another OpenAI-compatible server is a one-line alias and not a new code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingApi {
    Ollama,
    OpenAiCompatible,
    Synthetic,
}

impl EmbeddingApi {
    fn from_provider(provider: &str) -> Option<Self> {
        match provider.trim().to_ascii_lowercase().as_str() {
            "ollama" | "ollama.com" => Some(Self::Ollama),
            "openai" | "openai_compatible" | "openai-compatible" | "lmstudio" | "lm-studio"
            | "llamacpp" | "llama.cpp" | "llama-cpp" | "vllm" | "vllm-metal" => {
                Some(Self::OpenAiCompatible)
            }
            "synthetic" => Some(Self::Synthetic),
            _ => None,
        }
    }
}

/// HTTP client for generating text embeddings.
pub struct EmbeddingClient {
    http: reqwest::Client,
    provider: String,
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
            provider: config.provider.clone(),
            // `base_url` is the general setting; `ollama_base_url` is kept as a
            // fallback so configs written before multi-runtime support keep
            // working without edits.
            base_url: if config.base_url.trim().is_empty() {
                config.ollama_base_url.clone()
            } else {
                config.base_url.clone()
            },
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
        // Resolve the protocol BEFORE doing any work: an unknown provider must
        // fail by name rather than quietly behaving like Ollama and producing
        // baffling 404s from someone else's server.
        let api = EmbeddingApi::from_provider(&self.provider).ok_or_else(|| {
            EmbeddingError::RequestFailed(format!(
                "unsupported embedding provider '{}'; expected one of: ollama, openai, \
openai_compatible, lmstudio, llamacpp, vllm, synthetic",
                self.provider
            ))
        })?;
        if api == EmbeddingApi::Synthetic {
            return Ok(synthetic_embedding(text, self.dimensions));
        }

        let chunks = chunk_text_for_embedding(text, self.max_input_chars);
        let mut accum: Option<Vec<f64>> = None;
        let mut count = 0usize;

        for chunk in chunks {
            let embedding = self.embed_one(api, &chunk).await?;
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

    async fn embed_one(&self, api: EmbeddingApi, text: &str) -> Result<Vec<f64>, EmbeddingError> {
        let base_url = self.base_url.trim_end_matches('/');
        let (url, body) = match api {
            EmbeddingApi::Ollama => (
                format!("{base_url}/api/embed"),
                serde_json::to_value(OllamaEmbedRequest {
                    model: &self.model,
                    input: text,
                })
                .map_err(|e| EmbeddingError::BadResponse(e.to_string()))?,
            ),
            EmbeddingApi::OpenAiCompatible => (
                format!("{base_url}/v1/embeddings"),
                serde_json::to_value(OpenAiEmbedRequest {
                    model: &self.model,
                    input: text,
                })
                .map_err(|e| EmbeddingError::BadResponse(e.to_string()))?,
            ),
            EmbeddingApi::Synthetic => {
                return Ok(synthetic_embedding(text, self.dimensions)
                    .into_iter()
                    .map(f64::from)
                    .collect());
            }
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

        let embedding =
            match api {
                EmbeddingApi::Ollama => {
                    let parsed: OllamaEmbedResponse = resp
                        .json()
                        .await
                        .map_err(|e| EmbeddingError::BadResponse(e.to_string()))?;
                    parsed.embeddings.into_iter().next().ok_or_else(|| {
                        EmbeddingError::BadResponse("empty embeddings array".into())
                    })?
                }
                EmbeddingApi::OpenAiCompatible => {
                    let parsed: OpenAiEmbedResponse = resp
                        .json()
                        .await
                        .map_err(|e| EmbeddingError::BadResponse(e.to_string()))?;
                    parsed
                        .data
                        .into_iter()
                        .next()
                        .map(|datum| datum.embedding)
                        .ok_or_else(|| EmbeddingError::BadResponse("empty data array".into()))?
                }
                EmbeddingApi::Synthetic => unreachable!("handled above"),
            };

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
        let api = EmbeddingApi::from_provider(&self.provider).ok_or_else(|| {
            EmbeddingError::RequestFailed(format!(
                "unsupported embedding provider '{}'; expected one of: ollama, openai, \
openai_compatible, lmstudio, llamacpp, vllm, synthetic",
                self.provider
            ))
        })?;
        if api == EmbeddingApi::Synthetic {
            return Ok(());
        }

        let base_url = self.base_url.trim_end_matches('/');
        let url = match api {
            EmbeddingApi::Ollama => format!("{base_url}/api/tags"),
            // OpenAI-compatible servers have no /api/tags; /v1/models is the
            // equivalent. Without this a healthy llama.cpp reported itself
            // unavailable.
            EmbeddingApi::OpenAiCompatible => format!("{base_url}/v1/models"),
            EmbeddingApi::Synthetic => unreachable!("handled above"),
        };

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

        match api {
            EmbeddingApi::Ollama => {
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
            }
            EmbeddingApi::OpenAiCompatible => {
                // A successful status is the whole check, deliberately. The body
                // shape is NOT portable: OpenAI returns {"data":[...]} while
                // llama.cpp returns {"models":[...]}, so parsing it would make
                // this fail against a healthy server for no benefit.
                //
                // Nor is the model name checked: single-model servers report the
                // gguf filename or --alias, not the configured model name. A
                // genuinely wrong model still fails loudly on the first embed
                // with DimensionMismatch.
            }
            EmbeddingApi::Synthetic => unreachable!("handled above"),
        }

        Ok(())
    }
}

fn synthetic_embedding(text: &str, dimensions: u32) -> Vec<f32> {
    let dimensions = dimensions.max(1) as usize;
    let mut out = Vec::with_capacity(dimensions);
    let mut counter = 0u64;

    while out.len() < dimensions {
        let mut hasher = Sha256::new();
        hasher.update(counter.to_le_bytes());
        hasher.update(text.as_bytes());
        for byte in hasher.finalize() {
            if out.len() == dimensions {
                break;
            }
            out.push((byte as f32 / 127.5) - 1.0);
        }
        counter += 1;
    }

    out
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
    async fn synthetic_provider_returns_deterministic_local_vectors() {
        let config = EmbeddingConfig {
            provider: "synthetic".into(),
            ollama_base_url: String::new(),
            model: "synthetic-ci".into(),
            dimensions: 8,
            ..EmbeddingConfig::default()
        };
        let client = EmbeddingClient::new(&config);

        client.health_check().await.unwrap();
        let first = client.embed("same input").await.unwrap();
        let second = client.embed("same input").await.unwrap();
        let different = client.embed("different input").await.unwrap();

        assert_eq!(first.len(), 8);
        assert_eq!(first, second);
        assert_ne!(first, different);
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

    /// llama.cpp, vLLM, LM Studio and OpenAI all speak the same embeddings
    /// shape; Ollama does not. The client was hardcoded to Ollama's
    /// `/api/embed` + `{"embeddings":[[..]]}`, so none of the others could be
    /// used as a runtime at all.
    #[tokio::test]
    async fn openai_compatible_provider_uses_the_v1_embeddings_contract() {
        let (base_url, requests, handle) =
            spawn_embedding_server(vec!["{\"data\":[{\"embedding\":[1.0,2.0],\"index\":0}]}"]);
        let config = EmbeddingConfig {
            provider: "openai_compatible".into(),
            base_url: base_url.clone(),
            model: "nomic-embed-text-v2-moe".into(),
            dimensions: 2,
            ..EmbeddingConfig::default()
        };

        let embedding = EmbeddingClient::new(&config).embed("hello").await.unwrap();

        assert_eq!(embedding, vec![1.0, 2.0]);
        let body = requests.lock().unwrap()[0].clone();
        assert!(
            body.starts_with("POST /v1/embeddings "),
            "must POST the OpenAI route, got: {}",
            body.lines().next().unwrap_or_default()
        );
        assert!(
            body.contains("\"model\":\"nomic-embed-text-v2-moe\""),
            "{body}"
        );
        assert!(body.contains("\"input\":"), "{body}");
        handle.join().unwrap();
    }

    /// Existing Ollama installs must keep working byte for byte: same route,
    /// same request shape, same response shape.
    #[tokio::test]
    async fn ollama_provider_is_unchanged() {
        let (base_url, requests, handle) =
            spawn_embedding_server(vec!["{\"embeddings\":[[4.0,5.0]]}"]);
        let config = EmbeddingConfig {
            provider: "ollama".into(),
            ollama_base_url: base_url,
            dimensions: 2,
            ..EmbeddingConfig::default()
        };

        let embedding = EmbeddingClient::new(&config).embed("hello").await.unwrap();

        assert_eq!(embedding, vec![4.0, 5.0]);
        let body = requests.lock().unwrap()[0].clone();
        assert!(
            body.starts_with("POST /api/embed "),
            "Ollama must keep its own route, got: {}",
            body.lines().next().unwrap_or_default()
        );
        handle.join().unwrap();
    }

    /// LM Studio was already named in the config docs as an OpenAI-compatible
    /// server; it must route there rather than silently falling back to Ollama.
    #[tokio::test]
    async fn lmstudio_and_llamacpp_aliases_route_to_the_openai_contract() {
        for provider in ["lmstudio", "llamacpp", "llama.cpp", "openai", "vllm"] {
            let (base_url, requests, handle) =
                spawn_embedding_server(vec!["{\"data\":[{\"embedding\":[7.0]}]}"]);
            let config = EmbeddingConfig {
                provider: provider.into(),
                base_url: base_url.clone(),
                dimensions: 1,
                ..EmbeddingConfig::default()
            };

            EmbeddingClient::new(&config).embed("x").await.unwrap();

            let body = requests.lock().unwrap()[0].clone();
            assert!(
                body.starts_with("POST /v1/embeddings "),
                "{provider} must use the OpenAI route"
            );
            handle.join().unwrap();
        }
    }

    /// Config written before this change has no `base_url`, only
    /// `ollama_base_url`. Those files must keep working untouched.
    #[tokio::test]
    async fn base_url_falls_back_to_ollama_base_url_for_existing_configs() {
        let (base_url, requests, handle) =
            spawn_embedding_server(vec!["{\"data\":[{\"embedding\":[9.0]}]}"]);
        let config = EmbeddingConfig {
            provider: "openai_compatible".into(),
            ollama_base_url: base_url,
            dimensions: 1,
            ..EmbeddingConfig::default()
        };

        EmbeddingClient::new(&config).embed("x").await.unwrap();
        assert!(requests.lock().unwrap()[0].starts_with("POST /v1/embeddings "));
        handle.join().unwrap();
    }

    /// An unknown provider must fail loudly rather than quietly behaving like
    /// Ollama and producing confusing 404s from someone else's server.
    #[tokio::test]
    async fn unknown_provider_fails_with_a_named_error() {
        let config = EmbeddingConfig {
            provider: "definitely-not-a-runtime".into(),
            base_url: "http://127.0.0.1:9".into(),
            dimensions: 2,
            ..EmbeddingConfig::default()
        };

        let err = EmbeddingClient::new(&config).embed("x").await.unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("definitely-not-a-runtime"),
            "the error must name the unsupported provider: {message}"
        );
    }

    /// The health check was Ollama-only (`/api/tags` + loaded-model list). An
    /// OpenAI-compatible server has no such route, so a perfectly healthy
    /// llama.cpp would report itself unavailable.
    #[tokio::test]
    async fn health_check_uses_the_right_route_per_protocol() {
        let (base_url, requests, handle) =
            spawn_embedding_server(vec!["{\"object\":\"list\",\"data\":[{\"id\":\"nomic\"}]}"]);
        let config = EmbeddingConfig {
            provider: "llamacpp".into(),
            base_url: base_url.clone(),
            model: "nomic".into(),
            dimensions: 2,
            ..EmbeddingConfig::default()
        };

        EmbeddingClient::new(&config).health_check().await.unwrap();

        let request = requests.lock().unwrap()[0].clone();
        assert!(
            request.starts_with("GET /v1/models "),
            "OpenAI-compatible health must query /v1/models, got: {}",
            request.lines().next().unwrap_or_default()
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
