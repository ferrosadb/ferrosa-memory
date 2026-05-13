//! MCP client for spawning and communicating with ferrosa-memory-mcp via stdio.
//!
//! Spawns the MCP server as a child process with stdio pipes, sends JSON-RPC 2.0
//! requests (newline-delimited), and reads responses. Tracks per-call latency and
//! detects server crashes (child process exit) returning structured errors.
//!
//! ## Protocol
//!
//! JSON-RPC 2.0 over stdio, newline-delimited. See `ferrosa-memory-core::transport`
//! for request/response types.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Errors from the MCP client.
#[derive(Debug, Error)]
pub enum McpClientError {
    /// The server process exited unexpectedly.
    #[error("server crashed: exit status {status}")]
    ServerCrashed { status: String },

    /// The server process was killed or its stdio pipes closed.
    #[error("server pipe closed (process may have crashed)")]
    PipeClosed,

    /// JSON serialization/deserialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// IO error communicating with the server.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The server returned a JSON-RPC error response.
    #[error("server error {code}: {message}")]
    ServerError {
        code: i32,
        message: String,
        data: Option<Value>,
    },

    /// Timed out waiting for a response.
    #[error("timeout after {0:?}")]
    Timeout(Duration),

    /// HTTP transport error.
    #[error("http error: {0}")]
    Http(String),
}

/// Result of a successful tool call, including latency metadata.
#[derive(Debug)]
pub struct ToolCallResult {
    /// The JSON-RPC result value from the server.
    pub response: Value,
    /// Wall-clock latency for this call.
    pub latency: Duration,
    /// The JSON-RPC request ID used.
    pub request_id: u64,
}

// ---------------------------------------------------------------------------
// T-040: Server identity verification
// ---------------------------------------------------------------------------

/// Recorded identity of the MCP server binary and initialize response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerIdentity {
    /// SHA-256 hex digest of the MCP server binary.
    pub binary_hash: String,
    /// Server name from the initialize response.
    pub server_name: String,
    /// Server version from the initialize response.
    pub server_version: String,
}

/// Compute the SHA-256 hex digest of a file at the given path.
pub fn compute_binary_hash(path: &Path) -> Result<String, McpClientError> {
    let bytes = std::fs::read(path).map_err(McpClientError::Io)?;
    let hash = Sha256::digest(&bytes);
    Ok(format!("{hash:x}"))
}

// ---------------------------------------------------------------------------
// T-037: HTTP Transport
// ---------------------------------------------------------------------------

/// HTTP-based MCP client using JSON-RPC over HTTP POST.
pub struct HttpMcpClient {
    client: reqwest::Client,
    url: String,
    next_id: u64,
}

impl std::fmt::Debug for HttpMcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpMcpClient")
            .field("url", &self.url)
            .field("next_id", &self.next_id)
            .finish()
    }
}

impl HttpMcpClient {
    /// Create an HTTP MCP client connected to the given URL.
    pub fn new(url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.to_string(),
            next_id: 1,
        }
    }

    /// Returns the configured URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Returns the next request ID.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Send the MCP `initialize` request over HTTP.
    pub async fn initialize(&mut self) -> Result<ToolCallResult, McpClientError> {
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "ferrosa-memory-eval",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        self.send_request("initialize", params).await
    }

    /// Send a `tools/call` request over HTTP.
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<ToolCallResult, McpClientError> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });
        self.send_request("tools/call", params).await
    }

    /// Send a `tools/list` request over HTTP.
    pub async fn list_tools(&mut self) -> Result<ToolCallResult, McpClientError> {
        self.send_request("tools/list", Value::Object(serde_json::Map::new()))
            .await
    }

    /// Send a raw JSON-RPC request over HTTP POST.
    pub async fn send_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<ToolCallResult, McpClientError> {
        let id = self.next_id;
        self.next_id += 1;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let start = Instant::now();

        let resp = self
            .client
            .post(&self.url)
            .json(&request)
            .send()
            .await
            .map_err(|e| McpClientError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(McpClientError::Http(format!(
                "HTTP {} from {}",
                resp.status(),
                self.url
            )));
        }

        let response: Value = resp
            .json()
            .await
            .map_err(|e| McpClientError::Http(e.to_string()))?;

        let latency = start.elapsed();

        // Check for JSON-RPC error
        if let Some(error) = response.get("error") {
            return Err(McpClientError::ServerError {
                code: error.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32,
                message: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
                data: error.get("data").cloned(),
            });
        }

        let result = response.get("result").cloned().unwrap_or(Value::Null);

        Ok(ToolCallResult {
            response: result,
            latency,
            request_id: id,
        })
    }
}

// ---------------------------------------------------------------------------
// Stdio MCP client
// ---------------------------------------------------------------------------

/// MCP client that communicates with a ferrosa-memory-mcp child process via stdio.
pub struct McpClient {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    binary_path: String,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("next_id", &self.next_id)
            .field("binary_path", &self.binary_path)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Spawn the MCP server binary and establish stdio communication.
    ///
    /// `binary_path` is the path to the `ferrosa-memory-mcp` executable.
    /// The server inherits the current process environment (including
    /// `FERROSA_CQL_SEEDS`).
    pub async fn spawn(binary_path: &str) -> Result<Self, McpClientError> {
        let mut child = Command::new(binary_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;

        let child_stdin = child.stdin.take().ok_or(McpClientError::PipeClosed)?;
        let child_stdout = child.stdout.take().ok_or(McpClientError::PipeClosed)?;

        Ok(Self {
            child,
            stdin: BufWriter::new(child_stdin),
            stdout: BufReader::new(child_stdout),
            next_id: 1,
            binary_path: binary_path.to_string(),
        })
    }

    /// Returns the binary path used to spawn this client (for identity verification, ET-S2).
    pub fn binary_path(&self) -> &str {
        &self.binary_path
    }

    /// Returns the next request ID that will be used.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Send the MCP `initialize` request and return the server info response.
    pub async fn initialize(&mut self) -> Result<ToolCallResult, McpClientError> {
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "ferrosa-memory-eval",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        self.send_request("initialize", params).await
    }

    /// Send the `notifications/initialized` notification (no response expected).
    pub async fn send_initialized_notification(&mut self) -> Result<(), McpClientError> {
        self.check_alive()?;

        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });

        let mut data = serde_json::to_vec(&notification)?;
        data.push(b'\n');
        self.stdin.write_all(&data).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Send a `tools/call` request for the given tool name and arguments.
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<ToolCallResult, McpClientError> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });
        self.send_request("tools/call", params).await
    }

    /// Send a `tools/list` request and return the tool definitions.
    pub async fn list_tools(&mut self) -> Result<ToolCallResult, McpClientError> {
        self.send_request("tools/list", Value::Object(serde_json::Map::new()))
            .await
    }

    /// Send a raw JSON-RPC request and wait for the response.
    ///
    /// Tracks latency and increments the request ID counter. Detects server
    /// crashes by checking child process exit status before reading.
    pub async fn send_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<ToolCallResult, McpClientError> {
        self.check_alive()?;

        let id = self.next_id;
        self.next_id += 1;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let start = Instant::now();

        // Write request
        let mut data = serde_json::to_vec(&request)?;
        data.push(b'\n');
        self.stdin.write_all(&data).await?;
        self.stdin.flush().await?;

        // Read response line
        let mut line = String::new();
        let bytes_read = self.stdout.read_line(&mut line).await?;
        let latency = start.elapsed();

        if bytes_read == 0 {
            // EOF — server closed stdout, likely crashed
            return Err(self.diagnose_crash().await);
        }

        let response: Value = serde_json::from_str(line.trim())?;

        // Check for JSON-RPC error
        if let Some(error) = response.get("error") {
            return Err(McpClientError::ServerError {
                code: error.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32,
                message: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
                data: error.get("data").cloned(),
            });
        }

        let result = response.get("result").cloned().unwrap_or(Value::Null);

        Ok(ToolCallResult {
            response: result,
            latency,
            request_id: id,
        })
    }

    /// Check if the child process is still running.
    fn check_alive(&mut self) -> Result<(), McpClientError> {
        match self.child.try_wait() {
            Ok(Some(status)) => Err(McpClientError::ServerCrashed {
                status: status.to_string(),
            }),
            Ok(None) => Ok(()), // still running
            Err(e) => Err(McpClientError::Io(e)),
        }
    }

    /// Diagnose a crash by checking the child process exit status.
    async fn diagnose_crash(&mut self) -> McpClientError {
        match self.child.try_wait() {
            Ok(Some(status)) => McpClientError::ServerCrashed {
                status: status.to_string(),
            },
            Ok(None) => McpClientError::PipeClosed,
            Err(e) => McpClientError::Io(e),
        }
    }

    /// Gracefully shut down the server by closing stdin and waiting for exit.
    pub async fn shutdown(mut self) -> Result<(), McpClientError> {
        // Drop stdin to signal EOF to the server
        drop(self.stdin);
        // Wait for the child to exit
        self.child.wait().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------
    // Unit tests (no live server required)
    // -------------------------------------------------------------------

    #[test]
    fn tool_call_result_stores_latency() {
        let result = ToolCallResult {
            response: serde_json::json!({"ok": true}),
            latency: Duration::from_millis(42),
            request_id: 7,
        };
        assert_eq!(result.latency, Duration::from_millis(42));
        assert_eq!(result.request_id, 7);
        assert_eq!(result.response["ok"], true);
    }

    #[test]
    fn error_display_server_crashed() {
        let err = McpClientError::ServerCrashed {
            status: "exit status: 1".to_string(),
        };
        assert!(err.to_string().contains("server crashed"));
        assert!(err.to_string().contains("exit status: 1"));
    }

    #[test]
    fn error_display_server_error() {
        let err = McpClientError::ServerError {
            code: -32601,
            message: "not found".to_string(),
            data: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("-32601"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn error_display_pipe_closed() {
        let err = McpClientError::PipeClosed;
        assert!(err.to_string().contains("pipe closed"));
    }

    #[test]
    fn error_display_timeout() {
        let err = McpClientError::Timeout(Duration::from_secs(5));
        assert!(err.to_string().contains("timeout"));
    }

    // -------------------------------------------------------------------
    // Tests with a mock child process (echo server)
    // -------------------------------------------------------------------

    /// Helper: spawn a simple echo server that reads JSON-RPC requests from
    /// stdin and writes success responses to stdout. Uses a shell one-liner
    /// that works on macOS/Linux without external dependencies.
    async fn spawn_echo_server() -> McpClient {
        // This Python one-liner acts as a minimal JSON-RPC echo server:
        // - reads lines from stdin
        // - for each line, parses JSON, extracts id and method
        // - responds with {"jsonrpc":"2.0","id":<id>,"result":{"method":<method>,"echo":true}}
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except:
        continue
    rid = req.get("id")
    if rid is None:
        continue
    method = req.get("method", "")
    params = req.get("params", {})
    if method == "initialize":
        result = {"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mock-mcp","version":"0.0.1"}}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":json.dumps({"tool":params.get("name","?"),"ok":True})}]}
    elif method == "tools/list":
        result = {"tools":[{"name":"get_stats","description":"Get stats","inputSchema":{"type":"object","properties":{}}}]}
    else:
        result = {"method": method, "echo": True}
    resp = {"jsonrpc":"2.0","id":rid,"result":result}
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"#;

        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("python3 must be available for tests");

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();

        McpClient {
            child,
            stdin: BufWriter::new(child_stdin),
            stdout: BufReader::new(child_stdout),
            next_id: 1,
            binary_path: "python3".to_string(),
        }
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let mut client = spawn_echo_server().await;

        let result = client.initialize().await.unwrap();
        assert_eq!(result.response["serverInfo"]["name"], "mock-mcp");
        assert!(result.response["capabilities"]["tools"].is_object());
        assert!(result.latency > Duration::ZERO);
        assert_eq!(result.request_id, 1);

        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn call_tool_returns_response() {
        let mut client = spawn_echo_server().await;

        let result = client
            .call_tool("get_stats", serde_json::json!({}))
            .await
            .unwrap();

        // The mock returns content array with tool result
        let content = result.response["content"].as_array().unwrap();
        assert!(!content.is_empty());
        assert_eq!(content[0]["type"], "text");

        // Parse the nested JSON text
        let text: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["tool"], "get_stats");
        assert_eq!(text["ok"], true);

        assert!(result.latency > Duration::ZERO);
        assert_eq!(result.request_id, 1);

        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn list_tools_returns_tool_definitions() {
        let mut client = spawn_echo_server().await;

        let result = client.list_tools().await.unwrap();
        let tools = result.response["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        assert_eq!(tools[0]["name"], "get_stats");

        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn request_ids_increment() {
        let mut client = spawn_echo_server().await;

        let r1 = client.initialize().await.unwrap();
        assert_eq!(r1.request_id, 1);

        let r2 = client.list_tools().await.unwrap();
        assert_eq!(r2.request_id, 2);

        let r3 = client
            .call_tool("get_stats", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(r3.request_id, 3);

        assert_eq!(client.next_id(), 4);

        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn binary_path_is_recorded() {
        let client = spawn_echo_server().await;
        assert_eq!(client.binary_path(), "python3");
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn latency_is_tracked_per_call() {
        let mut client = spawn_echo_server().await;

        let r1 = client.initialize().await.unwrap();
        let r2 = client
            .call_tool("get_stats", serde_json::json!({}))
            .await
            .unwrap();

        // Both should have non-zero latency
        assert!(r1.latency > Duration::ZERO);
        assert!(r2.latency > Duration::ZERO);

        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn detects_server_crash_on_send() {
        // Spawn a process that exits immediately
        let script = r#"import sys; sys.exit(1)"#;

        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("python3 must be available for tests");

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();

        let mut client = McpClient {
            child,
            stdin: BufWriter::new(child_stdin),
            stdout: BufReader::new(child_stdout),
            next_id: 1,
            binary_path: "python3".to_string(),
        };

        // Give the process a moment to exit
        tokio::time::sleep(Duration::from_millis(100)).await;

        let err = client.initialize().await.unwrap_err();
        match err {
            McpClientError::ServerCrashed { status } => {
                assert!(status.contains("1"), "expected exit code 1, got: {status}");
            }
            other => panic!("expected ServerCrashed, got: {other}"),
        }
    }

    #[tokio::test]
    async fn detects_server_crash_on_read() {
        // Spawn a process that reads one line then crashes
        let script = r#"
import sys
line = sys.stdin.readline()
sys.exit(42)
"#;

        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("python3 must be available for tests");

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();

        let mut client = McpClient {
            child,
            stdin: BufWriter::new(child_stdin),
            stdout: BufReader::new(child_stdout),
            next_id: 1,
            binary_path: "python3".to_string(),
        };

        let err = client.initialize().await.unwrap_err();
        match err {
            McpClientError::ServerCrashed { .. } | McpClientError::PipeClosed => {
                // Both are acceptable — depends on timing
            }
            other => panic!("expected ServerCrashed or PipeClosed, got: {other}"),
        }
    }

    #[tokio::test]
    async fn handles_json_rpc_error_response() {
        // Mock server that returns a JSON-RPC error
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    rid = req.get("id")
    if rid is None:
        continue
    resp = {"jsonrpc":"2.0","id":rid,"error":{"code":-32601,"message":"method not found"}}
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"#;

        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("python3 must be available for tests");

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();

        let mut client = McpClient {
            child,
            stdin: BufWriter::new(child_stdin),
            stdout: BufReader::new(child_stdout),
            next_id: 1,
            binary_path: "python3".to_string(),
        };

        let err = client
            .call_tool("nonexistent", serde_json::json!({}))
            .await
            .unwrap_err();

        match err {
            McpClientError::ServerError { code, message, .. } => {
                assert_eq!(code, -32601);
                assert!(message.contains("not found"));
            }
            other => panic!("expected ServerError, got: {other}"),
        }

        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn send_initialized_notification_does_not_block() {
        let mut client = spawn_echo_server().await;

        // Initialize first
        client.initialize().await.unwrap();

        // Send notification — should not block or fail
        client.send_initialized_notification().await.unwrap();

        // Can still make calls after notification
        let result = client.list_tools().await.unwrap();
        assert!(result.response["tools"].is_array());

        client.shutdown().await.unwrap();
    }

    // -------------------------------------------------------------------
    // T-040: Server identity verification tests
    // -------------------------------------------------------------------

    #[test]
    fn compute_binary_hash_returns_hex_sha256() {
        // Create a temp file with known content
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test-binary");
        std::fs::write(&file_path, b"hello world").unwrap();

        let hash = compute_binary_hash(&file_path).unwrap();

        // SHA-256 of "hello world" is well-known
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(hash.len(), 64, "SHA-256 hex digest should be 64 chars");
    }

    #[test]
    fn compute_binary_hash_detects_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test-binary");

        std::fs::write(&file_path, b"version-1").unwrap();
        let hash1 = compute_binary_hash(&file_path).unwrap();

        std::fs::write(&file_path, b"version-2").unwrap();
        let hash2 = compute_binary_hash(&file_path).unwrap();

        assert_ne!(
            hash1, hash2,
            "different content should produce different hashes"
        );
    }

    #[test]
    fn compute_binary_hash_errors_on_missing_file() {
        let result = compute_binary_hash(std::path::Path::new("/nonexistent/binary"));
        assert!(result.is_err());
    }

    #[test]
    fn server_identity_serializes_to_json() {
        let identity = ServerIdentity {
            binary_hash: "abc123".to_string(),
            server_name: "mock-mcp".to_string(),
            server_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(json["binary_hash"], "abc123");
        assert_eq!(json["server_name"], "mock-mcp");
        assert_eq!(json["server_version"], "0.1.0");
    }

    #[test]
    fn server_identity_hash_appears_in_report_structure() {
        // Verify identity can be embedded in a report-like structure
        let identity = ServerIdentity {
            binary_hash: "deadbeef".repeat(8),
            server_name: "ferrosa-memory-mcp".to_string(),
            server_version: "0.5.0".to_string(),
        };

        let report = serde_json::json!({
            "server_identity": identity,
        });

        assert!(
            report["server_identity"]["binary_hash"]
                .as_str()
                .unwrap()
                .len()
                == 64,
            "hash should be 64 hex chars in report"
        );
    }

    // -------------------------------------------------------------------
    // T-037: HTTP transport tests
    // -------------------------------------------------------------------

    #[test]
    fn http_client_stores_url_and_increments_ids() {
        let client = HttpMcpClient::new("http://localhost:8080");
        assert_eq!(client.url(), "http://localhost:8080");
        assert_eq!(client.next_id(), 1);
    }

    #[test]
    fn http_client_error_variant_displays() {
        let err = McpClientError::Http("connection refused".to_string());
        assert!(err.to_string().contains("http error"));
        assert!(err.to_string().contains("connection refused"));
    }

    #[tokio::test]
    async fn http_client_returns_error_for_unreachable_server() {
        let mut client = HttpMcpClient::new("http://127.0.0.1:1");
        let result = client.initialize().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            McpClientError::Http(msg) => {
                assert!(!msg.is_empty(), "should have error details");
            }
            other => panic!("expected Http error, got: {other}"),
        }
    }

    #[tokio::test]
    async fn http_client_call_tool_returns_error_for_unreachable() {
        let mut client = HttpMcpClient::new("http://127.0.0.1:1");
        let result = client.call_tool("get_stats", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn http_client_live_initialize_and_get_stats() {
        // Requires a live MCP HTTP server. The cluster CI job provides
        // Ferrosa CQL/graph services, but not an authenticated MCP HTTP
        // listener, so this test is opt-in even when `--ignored` is used.
        let Ok(url) = std::env::var("FERROSA_EVAL_HTTP_URL") else {
            eprintln!("FERROSA_EVAL_HTTP_URL unset; skipping live MCP HTTP client smoke");
            return;
        };
        let mut client = HttpMcpClient::new(&url);

        let init = client.initialize().await.expect("initialize failed");
        assert!(init.response["serverInfo"]["name"].is_string());
        assert!(init.latency > Duration::ZERO);
        assert_eq!(init.request_id, 1);

        let stats = client
            .call_tool("get_stats", serde_json::json!({}))
            .await
            .expect("get_stats failed");
        assert!(stats.latency > Duration::ZERO);
        assert_eq!(stats.request_id, 2);
    }

    #[tokio::test]
    async fn spawn_nonexistent_binary_returns_error() {
        let result = McpClient::spawn("/nonexistent/binary/path").await;
        match result {
            Err(McpClientError::Io(_)) => {} // expected
            Err(other) => panic!("expected Io error, got: {other}"),
            Ok(_) => panic!("expected error for nonexistent binary"),
        }
    }

    // -------------------------------------------------------------------
    // Integration tests (require live ferrosa cluster + built binary)
    // -------------------------------------------------------------------

    #[tokio::test]
    #[ignore]
    async fn live_initialize_and_get_stats() {
        // Build path: target/debug/ferrosa-memory-mcp
        let binary = find_mcp_binary();

        let mut client = McpClient::spawn(&binary)
            .await
            .expect("failed to spawn MCP server");

        // AC2: initialize returns server info
        let init = client.initialize().await.expect("initialize failed");
        assert_eq!(
            init.response["serverInfo"]["name"], "ferrosa-memory-mcp",
            "server name mismatch"
        );
        assert!(
            init.response["capabilities"]["tools"].is_object(),
            "missing tools capability"
        );
        assert!(
            init.response["protocolVersion"].is_string(),
            "missing protocolVersion"
        );
        assert!(init.latency > Duration::ZERO, "latency should be tracked");

        // Send initialized notification
        client
            .send_initialized_notification()
            .await
            .expect("initialized notification failed");

        // AC3: call get_stats tool
        let stats = client
            .call_tool("get_stats", serde_json::json!({}))
            .await
            .expect("get_stats call failed");

        // Verify response structure — get_stats returns content array
        let content = stats.response["content"]
            .as_array()
            .expect("expected content array");
        assert!(!content.is_empty(), "content should not be empty");
        assert_eq!(content[0]["type"], "text", "expected text content type");

        // The text field should contain valid JSON with stats
        let text = content[0]["text"].as_str().expect("text should be string");
        let stats_json: Value =
            serde_json::from_str(text).expect("stats text should be valid JSON");
        assert!(stats_json.is_object(), "stats should be a JSON object");

        // AC4: latency is tracked
        assert!(stats.latency > Duration::ZERO, "latency should be tracked");

        // AC6: binary path recorded
        assert_eq!(client.binary_path(), binary);

        client.shutdown().await.expect("shutdown failed");
    }

    #[tokio::test]
    #[ignore]
    async fn live_tools_list_contains_expected_tools() {
        let binary = find_mcp_binary();

        let mut client = McpClient::spawn(&binary)
            .await
            .expect("failed to spawn MCP server");

        client.initialize().await.expect("initialize failed");
        client.send_initialized_notification().await.unwrap();

        let result = client.list_tools().await.expect("tools/list failed");
        let tools = result.response["tools"]
            .as_array()
            .expect("expected tools array");

        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

        assert!(
            names.contains(&"get_stats"),
            "tools should include get_stats"
        );
        assert!(
            names.contains(&"hybrid_search"),
            "tools should include hybrid_search"
        );
        assert!(
            names.contains(&"smart_ingest"),
            "tools should include smart_ingest"
        );

        client.shutdown().await.expect("shutdown failed");
    }

    #[tokio::test]
    #[ignore]
    async fn live_server_crash_detection() {
        let binary = find_mcp_binary();

        let mut client = McpClient::spawn(&binary)
            .await
            .expect("failed to spawn MCP server");

        // Initialize normally
        client.initialize().await.expect("initialize failed");

        // Kill the child process to simulate crash
        client.child.kill().await.expect("kill failed");

        // Give it a moment to die
        tokio::time::sleep(Duration::from_millis(100)).await;

        // AC5: next call should return structured error, not panic
        let err = client
            .call_tool("get_stats", serde_json::json!({}))
            .await
            .unwrap_err();

        match err {
            McpClientError::ServerCrashed { .. } => {} // expected
            McpClientError::PipeClosed => {}           // also acceptable
            McpClientError::Io(_) => {}                // broken pipe is acceptable too
            other => panic!("expected crash-related error, got: {other}"),
        }
    }

    /// Find the MCP server binary, building it if necessary.
    fn find_mcp_binary() -> String {
        // Check common locations
        let workspace_root = env!("CARGO_MANIFEST_DIR")
            .strip_suffix("/crates/ferrosa-memory-eval")
            .unwrap_or(env!("CARGO_MANIFEST_DIR"));

        let debug_path = format!("{workspace_root}/target/debug/ferrosa-memory-mcp");
        let release_path = format!("{workspace_root}/target/release/ferrosa-memory-mcp");

        if std::path::Path::new(&release_path).exists() {
            release_path
        } else if std::path::Path::new(&debug_path).exists() {
            debug_path
        } else {
            panic!(
                "ferrosa-memory-mcp binary not found. Build it first with: \
                 cargo build -p ferrosa-memory-mcp"
            );
        }
    }
}
