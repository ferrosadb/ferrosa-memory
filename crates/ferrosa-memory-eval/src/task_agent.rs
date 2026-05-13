//! Task agent — mock LLM agent loop using fmem via MCP stdio.
//!
//! Simulates an LLM agent that runs multi-session tasks by making MCP tool calls.
//! Uses scripted deterministic responses instead of real LLM API calls for
//! reproducible evaluation.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::mcp_client::{McpClient, McpClientError};
use crate::runner::McpTransport;

// ---------------------------------------------------------------------------
// Mock LLM — deterministic scripted responses
// ---------------------------------------------------------------------------

/// Pre-canned deterministic LLM that returns scripted tool call JSON in sequence.
/// No API cost, no flakiness.
#[derive(Debug, Clone)]
pub struct MockLlm {
    scripted_responses: Vec<String>,
    current_index: usize,
}

impl MockLlm {
    /// Create a new MockLlm with a sequence of pre-canned responses.
    pub fn new(scripted_responses: Vec<String>) -> Self {
        Self {
            scripted_responses,
            current_index: 0,
        }
    }

    /// Return the next scripted response, or a default "DONE" message.
    pub fn next_response(&mut self) -> String {
        if self.current_index < self.scripted_responses.len() {
            let resp = self.scripted_responses[self.current_index].clone();
            self.current_index += 1;
            resp
        } else {
            "{\"tool_calls\": []}".to_string()
        }
    }

    /// Reset to the beginning of the script.
    pub fn reset(&mut self) {
        self.current_index = 0;
    }

    /// Number of remaining scripted responses.
    pub fn remaining(&self) -> usize {
        self.scripted_responses
            .len()
            .saturating_sub(self.current_index)
    }
}

// ---------------------------------------------------------------------------
// Tool call parsing
// ---------------------------------------------------------------------------

/// A single tool call parsed from an LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedToolCall {
    pub tool_name: String,
    pub arguments: Value,
}

/// Parse JSON tool calls from an LLM response string.
///
/// Expected format:
/// ```json
/// {"tool_calls": [{"tool_name": "smart_ingest", "arguments": {"content": "..."}}]}
/// ```
pub fn parse_tool_calls(response: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    // Try to parse as JSON
    if let Ok(value) = serde_json::from_str::<Value>(response)
        && let Some(arr) = value.get("tool_calls").and_then(|v| v.as_array())
    {
        for item in arr {
            if let (Some(name), Some(args)) = (
                item.get("tool_name").and_then(|v| v.as_str()),
                item.get("arguments"),
            ) {
                calls.push(ParsedToolCall {
                    tool_name: name.to_string(),
                    arguments: args.clone(),
                });
            }
        }
    }

    calls
}

// ---------------------------------------------------------------------------
// Task agent
// ---------------------------------------------------------------------------

/// Output from a single agent session run.
#[derive(Debug, Clone)]
pub struct AgentOutput {
    pub session_id: Uuid,
    pub findings: Vec<String>,
    pub tool_calls: Vec<ToolCallTrace>,
    pub steps_taken: usize,
    pub completed: bool,
}

/// Record of a single tool call made by the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallTrace {
    pub tool_name: String,
    pub arguments: Value,
    pub response: Value,
    pub latency_ms: u64,
    pub success: bool,
}

/// Agent configuration for a single session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub prompt: String,
    pub tools_allowed: Vec<String>,
    pub max_steps: usize,
    pub expected_findings: Vec<String>,
}

/// TaskAgent simulates an LLM agent using fmem via MCP stdio.
///
/// ```ignore
/// let mut agent = TaskAgent::spawn("./ferrosa-memory-mcp", Uuid::new_v4(), mock_llm).await?;
/// let output = agent.run_session(&config).await?;
/// ```
pub struct TaskAgent {
    mcp_client: McpClient,
    session_id: Uuid,
    llm: MockLlm,
}

impl TaskAgent {
    /// Spawn the MCP binary and create a new TaskAgent.
    pub async fn spawn(
        binary_path: &str,
        session_id: Uuid,
        llm: MockLlm,
    ) -> Result<Self, McpClientError> {
        let mut mcp_client = McpClient::spawn(binary_path).await?;
        let _ = mcp_client.initialize().await?;
        mcp_client.send_initialized_notification().await?;

        Ok(Self {
            mcp_client,
            session_id,
            llm,
        })
    }

    /// Run a single session: query fmem context, parse tool calls, execute, repeat.
    pub async fn run_session(
        &mut self,
        config: &SessionConfig,
    ) -> Result<AgentOutput, McpClientError> {
        let mut tool_calls = Vec::new();
        let mut findings = Vec::new();
        let mut completed = false;

        // Step 0: Pre-load context via hybrid_search on the prompt
        let preloaded = self
            .mcp_client
            .call_tool(
                "hybrid_search",
                serde_json::json!({
                    "query": config.prompt,
                    "session_id": self.session_id.to_string(),
                    "limit": 5
                }),
            )
            .await?;

        tool_calls.push(ToolCallTrace {
            tool_name: "hybrid_search".to_string(),
            arguments: serde_json::json!({"query": config.prompt, "limit": 5}),
            response: preloaded.response.clone(),
            latency_ms: preloaded.latency.as_millis() as u64,
            success: true,
        });

        // Run the scripted LLM loop
        for _step in 0..config.max_steps {
            let llm_response = self.llm.next_response();
            let parsed = parse_tool_calls(&llm_response);

            if parsed.is_empty() {
                completed = true;
                break;
            }

            for call in parsed {
                // Filter to allowed tools
                if !config.tools_allowed.is_empty()
                    && !config.tools_allowed.contains(&call.tool_name)
                {
                    continue;
                }

                // Inject session_id into arguments
                let mut args = call.arguments.clone();
                if let Some(obj) = args.as_object_mut()
                    && !obj.contains_key("session_id")
                {
                    obj.insert(
                        "session_id".to_string(),
                        Value::String(self.session_id.to_string()),
                    );
                }

                let result = self
                    .mcp_client
                    .call_tool(&call.tool_name, args.clone())
                    .await;

                match result {
                    Ok(tc) => {
                        tool_calls.push(ToolCallTrace {
                            tool_name: call.tool_name.clone(),
                            arguments: args.clone(),
                            response: tc.response.clone(),
                            latency_ms: tc.latency.as_millis() as u64,
                            success: true,
                        });

                        // Extract findings from smart_ingest responses
                        if call.tool_name == "smart_ingest"
                            && let Some(action) = tc.response.get("action").and_then(|v| v.as_str())
                            && let Some(name) = args.get("entity_name").and_then(|v| v.as_str())
                        {
                            findings.push(format!("{}: {}", action, name));
                        }
                    }
                    Err(e) => {
                        tool_calls.push(ToolCallTrace {
                            tool_name: call.tool_name.clone(),
                            arguments: args,
                            response: serde_json::json!({"error": e.to_string()}),
                            latency_ms: 0,
                            success: false,
                        });
                    }
                }
            }
        }

        // Derive completion from whether we exhausted script or not
        if self.llm.remaining() == 0 {
            completed = true;
        }

        let steps_taken = tool_calls.len();
        Ok(AgentOutput {
            session_id: self.session_id,
            findings,
            tool_calls,
            steps_taken,
            completed,
        })
    }

    /// Shut down the MCP server gracefully.
    pub async fn shutdown(self) -> Result<(), McpClientError> {
        self.mcp_client.shutdown().await
    }
}

// ---------------------------------------------------------------------------
// Transport adapter for the eval runner
// ---------------------------------------------------------------------------

/// Adapter that wraps TaskAgent's MCP client for the runner's McpTransport trait.
///
/// This is a mock transport for testing — it does NOT spawn a real server.
/// Instead, it replays canned responses.
pub struct MockMcpTransport {
    canned_responses: HashMap<String, Value>,
    call_log: Vec<(String, Value)>,
}

impl MockMcpTransport {
    pub fn new(canned_responses: HashMap<String, Value>) -> Self {
        Self {
            canned_responses,
            call_log: Vec::new(),
        }
    }

    pub fn call_log(&self) -> &[(String, Value)] {
        &self.call_log
    }
}

impl McpTransport for MockMcpTransport {
    async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<(Value, Duration), crate::runner::RunnerError> {
        self.call_log
            .push((tool_name.to_string(), arguments.clone()));

        let response = self
            .canned_responses
            .get(tool_name)
            .cloned()
            .unwrap_or(serde_json::json!({"status": "ok"}));

        Ok((response, Duration::from_millis(10)))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mock LLM ─────────────────────────────────────────────────

    #[test]
    fn mock_llm_returns_scripted_sequence() {
        let script = vec![
            r#"{"tool_calls": [{"tool_name": "smart_ingest", "arguments": {"content": "test"}}]}"#
                .to_string(),
            r#"{"tool_calls": []}"#.to_string(),
        ];
        let mut llm = MockLlm::new(script);

        let r1 = llm.next_response();
        assert!(r1.contains("smart_ingest"));

        let r2 = llm.next_response();
        assert!(r2.contains("tool_calls"));

        let r3 = llm.next_response();
        assert_eq!(r3, r#"{"tool_calls": []}"#);
    }

    #[test]
    fn mock_llm_reset_restarts_sequence() {
        let script = vec!["A".to_string(), "B".to_string()];
        let mut llm = MockLlm::new(script);

        assert_eq!(llm.next_response(), "A");
        llm.reset();
        assert_eq!(llm.next_response(), "A");
    }

    // ── Tool call parsing ────────────────────────────────────────

    #[test]
    fn parse_tool_calls_empty_array() {
        let json = r#"{"tool_calls": []}"#;
        let calls = parse_tool_calls(json);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_single_call() {
        let json =
            r#"{"tool_calls": [{"tool_name": "smart_ingest", "arguments": {"content": "hello"}}]}"#;
        let calls = parse_tool_calls(json);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_name, "smart_ingest");
        assert_eq!(
            calls[0].arguments.get("content").unwrap().as_str(),
            Some("hello")
        );
    }

    #[test]
    fn parse_tool_calls_multiple_calls() {
        let json = r#"{"tool_calls": [
            {"tool_name": "smart_ingest", "arguments": {"content": "A"}},
            {"tool_name": "create_edge", "arguments": {"src": "x", "dst": "y"}}
        ]}"#;
        let calls = parse_tool_calls(json);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool_name, "smart_ingest");
        assert_eq!(calls[1].tool_name, "create_edge");
    }

    #[test]
    fn parse_tool_calls_invalid_json_returns_empty() {
        let calls = parse_tool_calls("not json");
        assert!(calls.is_empty());
    }

    // ── Mock transport ───────────────────────────────────────────

    #[test]
    fn mock_transport_records_calls() {
        let mut canned = HashMap::new();
        canned.insert(
            "get_stats".to_string(),
            serde_json::json!({"entity_count": 0}),
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut transport = MockMcpTransport::new(canned);
            let (resp, _latency) = transport
                .call_tool("get_stats", serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!(resp.get("entity_count").unwrap().as_i64(), Some(0));
            assert_eq!(transport.call_log().len(), 1);
        });
    }

    // ── Single session (integration-like with mock transport) ──────

    #[tokio::test]
    async fn single_session_with_mock_transport() {
        let mut canned = HashMap::new();
        canned.insert(
            "hybrid_search".to_string(),
            serde_json::json!({"results": []}),
        );
        canned.insert(
            "smart_ingest".to_string(),
            serde_json::json!({"action": "Created", "entity_id": "00000000-0000-0000-0000-000000000001"}),
        );

        let mut transport = MockMcpTransport::new(canned);

        let _config = SessionConfig {
            prompt: "Test prompt".to_string(),
            tools_allowed: vec!["smart_ingest".to_string()],
            max_steps: 3,
            expected_findings: vec!["Created: TestEntity".to_string()],
        };

        // Simulate the pre-load + tool execution
        let (resp, _) = transport
            .call_tool("hybrid_search", serde_json::json!({}))
            .await
            .unwrap();
        assert!(resp.get("results").is_some());

        let (resp2, _) = transport
            .call_tool(
                "smart_ingest",
                serde_json::json!({
                    "content": "test entity",
                    "entity_type": "test",
                    "entity_name": "TestEntity"
                }),
            )
            .await
            .unwrap();
        assert_eq!(resp2.get("action").unwrap().as_str(), Some("Created"));

        assert_eq!(transport.call_log().len(), 2);
    }

    #[tokio::test]
    async fn single_session_exhausts_max_steps() {
        let mut canned = HashMap::new();
        canned.insert(
            "hybrid_search".to_string(),
            serde_json::json!({"results": []}),
        );
        canned.insert(
            "smart_ingest".to_string(),
            serde_json::json!({"action": "Created"}),
        );

        let mut transport = MockMcpTransport::new(canned);
        let script = vec![
            r#"{"tool_calls": [{"tool_name": "smart_ingest", "arguments": {"content": "x"}}]}"#
                .to_string(),
            r#"{"tool_calls": [{"tool_name": "smart_ingest", "arguments": {"content": "y"}}]}"#
                .to_string(),
            r#"{"tool_calls": [{"tool_name": "smart_ingest", "arguments": {"content": "z"}}]}"#
                .to_string(),
        ];
        let mut llm = MockLlm::new(script);

        // Simulate max_steps=2 — only first 2 responses should execute
        let max_steps = 2;
        for _ in 0..max_steps {
            let resp = llm.next_response();
            let calls = parse_tool_calls(&resp);
            for call in calls {
                let _ = transport.call_tool(&call.tool_name, call.arguments).await;
            }
        }

        // After 2 steps, the third script response should remain
        assert_eq!(llm.remaining(), 1);
        assert_eq!(transport.call_log().len(), 2);
    }
}
