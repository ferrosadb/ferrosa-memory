//! Provider-neutral LLM client for eval generation and judging.
//!
//! The eval harness runs above the MCP server and database, so model calls live
//! here instead of in Ferrosa storage. The client intentionally supports a
//! deterministic `mock` provider for CI and OpenAI-compatible endpoints for
//! local providers such as LM Studio.

use std::time::Duration;

use ferrosa_memory_core::config::JudgeConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub token: Option<String>,
    pub timeout_seconds: u64,
    pub temperature: f64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        let judge = JudgeConfig::default();
        Self {
            provider: judge.provider,
            base_url: judge.base_url,
            model: judge.model,
            token: judge.token,
            timeout_seconds: judge.timeout_seconds,
            temperature: 0.0,
        }
    }
}

impl From<JudgeConfig> for LlmConfig {
    fn from(value: JudgeConfig) -> Self {
        Self {
            provider: value.provider,
            base_url: value.base_url,
            model: value.model,
            token: value.token,
            timeout_seconds: value.timeout_seconds,
            temperature: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmHealth {
    pub provider: String,
    pub model: String,
    pub available: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeDecision {
    pub score: f64,
    pub rationale: String,
    pub raw_response: String,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("LLM provider is disabled")]
    Disabled,

    #[error("unsupported LLM provider: {0}")]
    UnsupportedProvider(String),

    #[error("LLM request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("LLM provider returned no text")]
    EmptyResponse,
}

#[derive(Clone)]
pub struct LlmClient {
    config: LlmConfig,
    http: reqwest::Client,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> anyhow::Result<Self> {
        let timeout = Duration::from_secs(config.timeout_seconds.clamp(1, 300));
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { config, http })
    }

    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    pub async fn health(&self) -> LlmHealth {
        let provider = self.normalized_provider();
        if provider == "disabled" {
            return self.health_status(false, "disabled".into());
        }
        if provider == "mock" {
            return self.health_status(true, "mock provider is deterministic".into());
        }

        match self.list_models().await {
            Ok(models) if models.iter().any(|model| model == &self.config.model) => {
                self.health_status(true, "configured model found".into())
            }
            Ok(models) if models.is_empty() => {
                self.health_status(false, "provider returned no models".into())
            }
            Ok(models) => self.health_status(
                false,
                format!(
                    "configured model not found; provider returned {}",
                    models.join(", ")
                ),
            ),
            Err(err) => self.health_status(false, err.to_string()),
        }
    }

    pub async fn generate(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, LlmError> {
        let provider = self.normalized_provider();
        match provider.as_str() {
            "disabled" => Err(LlmError::Disabled),
            "mock" => Ok(mock_response(user_prompt)),
            "ollama" | "ollama.com" => self.generate_ollama(system_prompt, user_prompt).await,
            "lmstudio" | "openai_compatible" | "openai-compatible" | "openai" => {
                self.generate_openai_compatible(system_prompt, user_prompt)
                    .await
            }
            _ => Err(LlmError::UnsupportedProvider(self.config.provider.clone())),
        }
    }

    pub async fn judge(
        &self,
        rubric: &str,
        answer: &str,
        evidence: &str,
    ) -> Result<JudgeDecision, LlmError> {
        let prompt = format!(
            "Rubric:\n{rubric}\n\nAnswer:\n{answer}\n\nEvidence:\n{evidence}\n\n\
             Return JSON only: {{\"score\": <0.0 to 1.0>, \"rationale\": \"short reason\"}}"
        );
        let raw = self
            .generate(
                "You are a strict eval judge. Return compact JSON only.",
                &prompt,
            )
            .await?;
        Ok(parse_judge_decision(&raw))
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let provider = self.normalized_provider();
        let base_url = self.config.base_url.trim_end_matches('/');
        let url = if provider == "ollama" || provider == "ollama.com" {
            format!("{base_url}/api/tags")
        } else {
            format!("{base_url}/v1/models")
        };
        let mut request = self.http.get(url);
        if let Some(token) = self.config.token.as_deref()
            && !token.is_empty()
        {
            request = request.bearer_auth(token);
        }
        let value: Value = request.send().await?.error_for_status()?.json().await?;
        let models = if provider == "ollama" || provider == "ollama.com" {
            value
                .get("models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|model| {
                    model
                        .get("name")
                        .or_else(|| model.get("model"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        } else {
            value
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        };
        Ok(models)
    }

    async fn generate_ollama(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, LlmError> {
        let prompt = if system_prompt.trim().is_empty() {
            user_prompt.to_string()
        } else {
            format!("{system_prompt}\n\n{user_prompt}")
        };
        let mut request = self
            .http
            .post(format!(
                "{}/api/generate",
                self.config.base_url.trim_end_matches('/')
            ))
            .json(&serde_json::json!({
                "model": self.config.model,
                "prompt": prompt,
                "stream": false,
                "options": {
                    "temperature": self.config.temperature
                }
            }));
        if let Some(token) = self.config.token.as_deref()
            && !token.is_empty()
        {
            request = request.bearer_auth(token);
        }
        let value: Value = request.send().await?.error_for_status()?.json().await?;
        value
            .get("response")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|text| !text.trim().is_empty())
            .ok_or(LlmError::EmptyResponse)
    }

    async fn generate_openai_compatible(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, LlmError> {
        let mut request = self
            .http
            .post(format!(
                "{}/v1/chat/completions",
                self.config.base_url.trim_end_matches('/')
            ))
            .json(&serde_json::json!({
                "model": self.config.model,
                "temperature": self.config.temperature,
                "messages": [
                    {"role": "system", "content": system_prompt},
                    {"role": "user", "content": user_prompt}
                ]
            }));
        if let Some(token) = self.config.token.as_deref()
            && !token.is_empty()
        {
            request = request.bearer_auth(token);
        }
        let value: Value = request.send().await?.error_for_status()?.json().await?;
        value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|text| !text.trim().is_empty())
            .ok_or(LlmError::EmptyResponse)
    }

    fn normalized_provider(&self) -> String {
        self.config.provider.trim().to_ascii_lowercase()
    }

    fn health_status(&self, available: bool, detail: String) -> LlmHealth {
        LlmHealth {
            provider: self.config.provider.clone(),
            model: self.config.model.clone(),
            available,
            detail,
        }
    }
}

fn mock_response(user_prompt: &str) -> String {
    if user_prompt.contains("\"score\"") || user_prompt.to_ascii_lowercase().contains("rubric") {
        return r#"{"score":0.75,"rationale":"mock judge"}"#.to_string();
    }
    "agent_a: initial answer\nagent_b: user prefers concrete implementation details\nagent_a: correction accepted with exact files"
        .to_string()
}

fn parse_judge_decision(raw: &str) -> JudgeDecision {
    let value = extract_json_object(raw)
        .and_then(|json| serde_json::from_str::<Value>(json).ok())
        .unwrap_or(Value::Null);
    let score = value
        .get("score")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let rationale = value
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("judge returned no rationale")
        .to_string();
    JudgeDecision {
        score,
        rationale,
        raw_response: raw.to_string(),
    }
}

fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end >= start).then_some(&raw[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[tokio::test]
    async fn mock_provider_generates_and_judges_without_network() {
        let client = LlmClient::new(LlmConfig {
            provider: "mock".into(),
            model: "mock-model".into(),
            ..LlmConfig::default()
        })
        .unwrap();

        let health = client.health().await;
        assert!(health.available);
        let generated = client.generate("", "make conversation").await.unwrap();
        assert!(generated.contains("agent_a"));
        let decision = client
            .judge("score correctness", "answer", "evidence")
            .await
            .unwrap();
        assert_eq!(decision.score, 0.75);
    }

    #[tokio::test]
    async fn ollama_provider_posts_generate_endpoint() {
        let (base_url, requests, handle) =
            spawn_json_server(vec![r#"{"response":"generated text"}"#]);
        let client = LlmClient::new(LlmConfig {
            provider: "ollama".into(),
            base_url,
            model: "qwen".into(),
            temperature: 0.9,
            ..LlmConfig::default()
        })
        .unwrap();

        let text = client.generate("system", "user").await.unwrap();
        assert_eq!(text, "generated text");
        let bodies = requests.lock().unwrap();
        assert!(bodies[0].contains("POST /api/generate"));
        assert!(bodies[0].contains("\"temperature\":0.9"));
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn openai_compatible_provider_posts_chat_completions() {
        let (base_url, requests, handle) =
            spawn_json_server(vec![r#"{"choices":[{"message":{"content":"chat text"}}]}"#]);
        let client = LlmClient::new(LlmConfig {
            provider: "lmstudio".into(),
            base_url,
            model: "local".into(),
            token: Some("secret".into()),
            ..LlmConfig::default()
        })
        .unwrap();

        let text = client.generate("system", "user").await.unwrap();
        assert_eq!(text, "chat text");
        let bodies = requests.lock().unwrap();
        assert!(bodies[0].contains("POST /v1/chat/completions"));
        assert!(bodies[0].contains("authorization: Bearer secret"));
        handle.join().unwrap();
    }

    #[test]
    fn judge_parser_clamps_scores_and_keeps_rationale() {
        let decision = parse_judge_decision(r#"prefix {"score": 1.7, "rationale":"good"} suffix"#);
        assert_eq!(decision.score, 1.0);
        assert_eq!(decision.rationale, "good");
    }

    fn spawn_json_server(
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
}
