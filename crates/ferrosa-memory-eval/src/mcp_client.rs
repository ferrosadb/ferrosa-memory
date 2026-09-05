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

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
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

    /// The client could not construct a valid protocol request.
    #[error("invalid MCP request: {0}")]
    InvalidRequest(String),

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
    Ok(hex::encode(hash))
}

// ---------------------------------------------------------------------------
// T-037: HTTP Transport
// ---------------------------------------------------------------------------

const MODERN_MCP_PROTOCOL_VERSION: &str = "2026-07-28";
const MAX_TOOL_LIST_PAGES: usize = 256;

/// Protocol mode used for JSON-RPC over HTTP.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HttpMcpProtocolMode {
    /// Session-oriented MCP used by existing clients.
    #[default]
    Legacy,
    /// Stateless MCP draft with per-request metadata and mirrored HTTP headers.
    Modern,
}

#[derive(Debug)]
struct PreparedHttpRequest {
    body: Value,
    headers: Vec<(String, String)>,
}

impl PreparedHttpRequest {
    #[cfg(test)]
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// HTTP-based MCP client using JSON-RPC over HTTP POST.
pub struct HttpMcpClient {
    client: reqwest::Client,
    url: String,
    next_id: u64,
    basic_auth: Option<(String, String)>,
    protocol_mode: HttpMcpProtocolMode,
}

impl std::fmt::Debug for HttpMcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpMcpClient")
            .field("url", &self.url)
            .field("next_id", &self.next_id)
            .field(
                "basic_auth",
                &self.basic_auth.as_ref().map(|(user, _)| user),
            )
            .field("protocol_mode", &self.protocol_mode)
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
            basic_auth: None,
            protocol_mode: HttpMcpProtocolMode::Legacy,
        }
    }

    /// Use the stateless 2026-07-28 draft protocol for each request.
    pub fn with_modern_protocol(mut self) -> Self {
        self.protocol_mode = HttpMcpProtocolMode::Modern;
        self
    }

    /// Configure HTTP Basic auth credentials for protected MCP endpoints.
    pub fn with_basic_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.basic_auth = Some((username.into(), password.into()));
        self
    }

    /// Returns the configured URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Returns the next request ID.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Negotiate the configured HTTP protocol mode.
    ///
    /// Legacy mode sends `initialize`; modern mode uses the stateless
    /// `server/discover` request instead.
    pub async fn initialize(&mut self) -> Result<ToolCallResult, McpClientError> {
        match self.protocol_mode {
            HttpMcpProtocolMode::Legacy => {
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
            HttpMcpProtocolMode::Modern => {
                self.send_request("server/discover", serde_json::json!({}))
                    .await
            }
        }
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
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut tools = Vec::new();
        for _ in 0..MAX_TOOL_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map(|cursor| serde_json::json!({"cursor": cursor}))
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            let mut page = self.send_request("tools/list", params).await?;
            tools.extend(page.response["tools"].as_array().cloned().ok_or_else(|| {
                McpClientError::InvalidRequest("tools/list result omitted tools array".into())
            })?);
            cursor = page.response["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                page.response["tools"] = Value::Array(tools);
                return Ok(page);
            }
            if !seen_cursors.insert(cursor.clone().unwrap_or_default()) {
                return Err(McpClientError::InvalidRequest(
                    "tools/list repeated a continuation cursor".into(),
                ));
            }
        }
        Err(McpClientError::InvalidRequest(format!(
            "tools/list exceeded {MAX_TOOL_LIST_PAGES} pages"
        )))
    }

    fn prepare_request(
        &self,
        id: u64,
        method: &str,
        mut params: Value,
    ) -> Result<PreparedHttpRequest, McpClientError> {
        let mut headers = Vec::new();
        if self.protocol_mode == HttpMcpProtocolMode::Modern {
            let params = params.as_object_mut().ok_or_else(|| {
                McpClientError::InvalidRequest(format!(
                    "HTTP MCP params for {method} must be a JSON object"
                ))
            })?;
            let meta = params
                .entry("_meta")
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .ok_or_else(|| {
                    McpClientError::InvalidRequest(format!(
                        "HTTP MCP params._meta for {method} must be an object"
                    ))
                })?;
            meta.insert(
                "io.modelcontextprotocol/protocolVersion".to_string(),
                Value::String(MODERN_MCP_PROTOCOL_VERSION.to_string()),
            );
            meta.entry("io.modelcontextprotocol/clientCapabilities")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            meta.entry("io.modelcontextprotocol/clientInfo")
                .or_insert_with(|| {
                    serde_json::json!({
                        "name": "ferrosa-memory-eval",
                        "version": env!("CARGO_PKG_VERSION")
                    })
                });

            headers.push((
                "MCP-Protocol-Version".to_string(),
                MODERN_MCP_PROTOCOL_VERSION.to_string(),
            ));
            headers.push(("Mcp-Method".to_string(), method.to_string()));
            if let Some(name) = mcp_request_name(method, &Value::Object(params.clone())) {
                headers.push(("Mcp-Name".to_string(), encode_mcp_header_value(name)));
            }
        }

        Ok(PreparedHttpRequest {
            body: serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            }),
            headers,
        })
    }

    /// Send a raw JSON-RPC request over HTTP POST.
    pub async fn send_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<ToolCallResult, McpClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let prepared = self.prepare_request(id, method, params)?;

        let start = Instant::now();

        let mut http_request = self.client.post(&self.url).json(&prepared.body);
        for (name, value) in prepared.headers {
            http_request = http_request.header(name, value);
        }
        if let Some((username, password)) = &self.basic_auth {
            http_request = http_request.basic_auth(username, Some(password));
        }

        let resp = http_request
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

fn mcp_request_name<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    match method {
        "tools/call" | "prompts/get" => params.get("name").and_then(Value::as_str),
        "resources/read" => params.get("uri").and_then(Value::as_str),
        _ => None,
    }
}

fn encode_mcp_header_value(value: &str) -> String {
    let plain = !value.is_empty()
        && value.trim_matches([' ', '\t']) == value
        && value
            .as_bytes()
            .iter()
            .all(|byte| matches!(*byte, 0x20..=0x7e));
    if plain {
        value.to_string()
    } else {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(value)
        )
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
        Self::spawn_with_env(binary_path, std::iter::empty::<(&str, &str)>()).await
    }

    /// Spawn the MCP server binary with additional environment variables.
    ///
    /// This is primarily used by live tests to supply an explicit test config
    /// instead of inheriting developer-machine defaults.
    pub async fn spawn_with_env<I, K, V>(binary_path: &str, envs: I) -> Result<Self, McpClientError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new(binary_path);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn()?;

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

    /// Returns the operating-system process id of the spawned server, if it is
    /// still available from Tokio's child handle.
    pub fn process_id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Kill the spawned server process.
    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().await
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
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut tools = Vec::new();
        for _ in 0..MAX_TOOL_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map(|cursor| serde_json::json!({"cursor": cursor}))
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            let mut page = self.send_request("tools/list", params).await?;
            tools.extend(page.response["tools"].as_array().cloned().ok_or_else(|| {
                McpClientError::InvalidRequest("tools/list result omitted tools array".into())
            })?);
            cursor = page.response["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                page.response["tools"] = Value::Array(tools);
                return Ok(page);
            }
            if !seen_cursors.insert(cursor.clone().unwrap_or_default()) {
                return Err(McpClientError::InvalidRequest(
                    "tools/list repeated a continuation cursor".into(),
                ));
            }
        }
        Err(McpClientError::InvalidRequest(format!(
            "tools/list exceeded {MAX_TOOL_LIST_PAGES} pages"
        )))
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
    use serial_test::serial;

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

    async fn spawn_paged_tool_server(repeat_cursor: bool) -> McpClient {
        let repeat = if repeat_cursor { "True" } else { "False" };
        let script = format!(
            r#"
import sys, json
repeat = {repeat}
for line in sys.stdin:
    req = json.loads(line)
    rid = req.get("id")
    if rid is None:
        continue
    cursor = req.get("params", {{}}).get("cursor")
    if cursor is None:
        result = {{"tools":[{{"name":"first"}}],"nextCursor":"page-2"}}
    elif repeat:
        result = {{"tools":[{{"name":"again"}}],"nextCursor":"page-2"}}
    else:
        result = {{"tools":[{{"name":"second"}}]}}
    print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":result}}), flush=True)
"#
        );
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("python3 must be available for tests");
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = BufReader::new(child.stdout.take().unwrap());
        McpClient {
            child,
            stdin,
            stdout,
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
    async fn list_tools_follows_pages_and_rejects_cursor_cycles() {
        let mut client = spawn_paged_tool_server(false).await;
        let result = client.list_tools().await.unwrap();
        let names: Vec<_> = result.response["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(names, ["first", "second"]);
        client.shutdown().await.unwrap();

        let mut cycling = spawn_paged_tool_server(true).await;
        let error = cycling.list_tools().await.unwrap_err();
        assert!(
            matches!(error, McpClientError::InvalidRequest(message) if message.contains("repeated"))
        );
        cycling.shutdown().await.unwrap();
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

        // Wait for the child to actually exit rather than guessing with a
        // fixed sleep. A 100ms sleep here was enough when this test ran alone
        // but not when it ran alongside the rest of the suite: spawn+exit had
        // not completed, so initialize() reported a different error variant
        // and the strict ServerCrashed assertion below failed. try_wait()
        // caches the status, so the client's own crash detection still sees it.
        loop {
            if child.try_wait().expect("try_wait on test child").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let mut client = McpClient {
            child,
            stdin: BufWriter::new(child_stdin),
            stdout: BufReader::new(child_stdout),
            next_id: 1,
            binary_path: "python3".to_string(),
        };

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
    fn modern_http_client_adds_per_request_metadata_and_headers() {
        let client = HttpMcpClient::new("http://localhost:8080").with_modern_protocol();
        let params = serde_json::json!({
            "name": "get_stats",
            "arguments": {}
        });

        let prepared = client.prepare_request(7, "tools/call", params).unwrap();

        assert_eq!(
            prepared.body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert!(
            prepared.body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]
                .is_object()
        );
        assert_eq!(prepared.header("MCP-Protocol-Version"), Some("2026-07-28"));
        assert_eq!(prepared.header("Mcp-Method"), Some("tools/call"));
        assert_eq!(prepared.header("Mcp-Name"), Some("get_stats"));
    }

    #[test]
    fn legacy_http_client_preserves_initialize_request_without_draft_metadata() {
        let client = HttpMcpClient::new("http://localhost:8080");
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {}
        });

        let prepared = client
            .prepare_request(3, "initialize", params.clone())
            .unwrap();

        assert_eq!(prepared.body["params"], params);
        assert!(prepared.headers.is_empty());
    }

    #[test]
    fn modern_http_client_encodes_non_ascii_mcp_names() {
        let client = HttpMcpClient::new("http://localhost:8080").with_modern_protocol();
        let prepared = client
            .prepare_request(
                9,
                "resources/read",
                serde_json::json!({"uri": "ferrosa-memory://tasks/😀/current"}),
            )
            .unwrap();

        assert_eq!(
            prepared.header("Mcp-Name"),
            Some("=?base64?ZmVycm9zYS1tZW1vcnk6Ly90YXNrcy/wn5iAL2N1cnJlbnQ=?=")
        );
    }

    #[test]
    fn modern_http_client_rejects_non_object_params_without_panicking() {
        let client = HttpMcpClient::new("http://localhost:8080").with_modern_protocol();

        let error = client
            .prepare_request(10, "tools/list", Value::Null)
            .unwrap_err();

        assert!(matches!(error, McpClientError::InvalidRequest(_)));
        assert!(error.to_string().contains("params for tools/list"));
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
    #[ignore = "requires an HTTP-bound MCP server; set FERROSA_MCP_HTTP_URL=http://host:port to opt in"]
    async fn http_client_live_initialize_and_get_stats() {
        // Skip cleanly when the harness URL isn't set. The default workspace
        // --ignored sweep (local + CI) doesn't spin up an HTTP-bound MCP
        // server — only contributors testing that surface specifically do.
        let Ok(url) = std::env::var("FERROSA_MCP_HTTP_URL") else {
            eprintln!(
                "skip: FERROSA_MCP_HTTP_URL unset — set to an HTTP-bound MCP \
                 server URL (e.g. http://localhost:8080) to exercise the \
                 HttpMcpClient initialize/get_stats round-trip"
            );
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
    #[serial(mcp_live)]
    async fn live_initialize_and_get_stats() {
        // Build path: target/debug/ferrosa-memory-mcp
        let binary = find_mcp_binary();
        let config = live_mcp_test_config();

        let mut client = spawn_live_mcp(&binary, &config)
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
    #[serial(mcp_live)]
    async fn live_tools_list_contains_expected_tools() {
        let binary = find_mcp_binary();
        let config = live_mcp_test_config();

        let mut client = spawn_live_mcp(&binary, &config)
            .await
            .expect("failed to spawn MCP server");

        client.initialize().await.expect("initialize failed");
        client.send_initialized_notification().await.unwrap();

        let result = client.list_tools().await.expect("tools/list failed");
        let tools = result.response["tools"]
            .as_array()
            .expect("expected tools array");

        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

        for expected in ["all_tools", "stats", "search", "ingest", "feedback"] {
            assert!(
                names.contains(&expected),
                "tools should include {expected}; advertised tools: {names:?}"
            );
        }

        for hidden_by_default in ["get_stats", "hybrid_search", "smart_ingest"] {
            assert!(
                !names.contains(&hidden_by_default),
                "verbose tier-2 tool {hidden_by_default} should not be advertised by default; \
                 advertised tools: {names:?}"
            );
        }

        client.shutdown().await.expect("shutdown failed");
    }

    #[tokio::test]
    #[ignore]
    #[serial(mcp_live)]
    async fn live_server_crash_detection() {
        let binary = find_mcp_binary();
        let config = live_mcp_test_config();

        let mut client = spawn_live_mcp(&binary, &config)
            .await
            .expect("failed to spawn MCP server");

        // Initialize normally
        client.initialize().await.expect("initialize failed");

        // Kill the child process to simulate crash
        client.kill().await.expect("kill failed");

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

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires live ferrosa test cluster; run via make test-live"]
    #[serial(mcp_live)]
    async fn live_multi_replica_consolidation_takeover_after_holder_kill() {
        use ferrosa_memory_core::config::FerrosaCqlConfig;
        use ferrosa_memory_core::cql_storage::CqlStorage;
        use ferrosa_memory_core::storage::Storage;
        use ferrosa_memory_core::types::TenantContext;
        use uuid::Uuid;

        let (Ok(cql_host), Ok(cql_port)) = (
            std::env::var("FERROSA_TEST_CQL_HOST"),
            std::env::var("FERROSA_TEST_CQL_PORT"),
        ) else {
            eprintln!(
                "skip: FERROSA_TEST_CQL_HOST/FERROSA_TEST_CQL_PORT unset — run via make test-live"
            );
            return;
        };

        let binary = find_mcp_binary();
        let config = live_mcp_consolidation_test_config(&cql_host, &cql_port);
        let tenant_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let session_id = Uuid::new_v4();
        let ctx = TenantContext {
            tenant_id,
            session_origin: "mcp-live-multi-replica-consolidation-test".to_string(),
        };

        // Spawn both replicas first. Their startup runs schema migrations, which
        // creates the coordination tables in the test keyspace before the test
        // opens its direct CQL storage connection below.
        let mut replica_a = spawn_live_mcp(&binary, &config)
            .await
            .expect("failed to spawn MCP replica A");
        let mut replica_b = spawn_live_mcp(&binary, &config)
            .await
            .expect("failed to spawn MCP replica B");
        let replica_a_pid = replica_a.process_id().expect("replica A pid available");
        let replica_b_pid = replica_b.process_id().expect("replica B pid available");

        replica_a.initialize().await.expect("initialize replica A");
        replica_a
            .send_initialized_notification()
            .await
            .expect("initialized notification replica A");
        replica_b.initialize().await.expect("initialize replica B");
        replica_b
            .send_initialized_notification()
            .await
            .expect("initialized notification replica B");

        // Wait for a replica to finish CQL connection setup by retrying the
        // actual write tool. `get_stats` succeeds before CQL is ready, so we use
        // `smart_ingest` (which requires CQL) as the readiness probe.
        let ingest_args = serde_json::json!({
            "session_id": session_id.to_string(),
            "content": "multi-replica consolidation test payload",
            "entity_type": "concept",
            "entity_name": "multi-replica consolidation takeover test"
        });
        wait_for_smart_ingest_ok(
            &mut replica_a,
            "smart_ingest",
            &ingest_args,
            Duration::from_secs(15),
        )
        .await
        .expect("replica A CQL readiness via smart_ingest");

        // Issue a second write so the consolidation request stays fresh after the
        // readiness probe. Reuse replica A for deterministic initial ownership.
        replica_a
            .call_tool("smart_ingest", ingest_args.clone())
            .await
            .expect("second smart_ingest call should succeed");

        let storage_config = FerrosaCqlConfig {
            tls_ca_path: None,
            tls_skip_hostname_verify: false,
            contact_points: vec![format!("{cql_host}:{cql_port}")],
            keyspace: "agent_memory_test".to_string(),
            replication_factor: 1,
            consistency: "LOCAL_QUORUM".to_string(),
            username: "cassandra".to_string(),
            password: "cassandra".to_string(),
            admin_username: None,
            admin_password: None,
        };
        let storage = CqlStorage::connect(&storage_config)
            .await
            .expect("connect direct CQL storage");

        let first_owner =
            wait_for_leased_owner(&storage, &ctx, session_id, Duration::from_secs(10))
                .await
                .expect("request should become leased");
        let latest_run = storage
            .consolidation_run_get_latest(&ctx, session_id)
            .await
            .expect("read latest consolidation run");
        if let Some(run) = latest_run {
            assert_eq!(
                run.lease_owner.as_deref(),
                Some(first_owner.as_str()),
                "latest running consolidation run should match request lease owner"
            );
        }

        let holder_pid = lease_owner_pid(&first_owner).expect("lease owner should include pid");
        let survivor_pid = if holder_pid == replica_a_pid {
            replica_a.kill().await.expect("kill replica A holder");
            replica_b_pid
        } else if holder_pid == replica_b_pid {
            replica_b.kill().await.expect("kill replica B holder");
            replica_a_pid
        } else {
            panic!(
                "lease owner pid {holder_pid} did not match replica pids A={replica_a_pid} B={replica_b_pid}; owner={first_owner}"
            );
        };

        let takeover_owner = wait_for_changed_leased_owner(
            &storage,
            &ctx,
            session_id,
            &first_owner,
            // The lease expires after three seconds, but the surviving worker
            // can be between polling ticks while the killed replica's CQL
            // connection is being observed. Leave a full polling/claim window
            // after expiry so this test checks takeover rather than runner
            // scheduling jitter.
            Duration::from_secs(15),
        )
        .await
        .expect("surviving replica should take over expired consolidation lease");
        assert_eq!(
            lease_owner_pid(&takeover_owner),
            Some(survivor_pid),
            "new lease_owner should belong to surviving replica"
        );

        if replica_a.process_id() == Some(survivor_pid) {
            let _ = replica_a.kill().await;
        }
        if replica_b.process_id() == Some(survivor_pid) {
            let _ = replica_b.kill().await;
        }
    }

    async fn spawn_live_mcp(
        binary: &str,
        config: &tempfile::NamedTempFile,
    ) -> Result<McpClient, McpClientError> {
        let config_path = config.path().to_string_lossy().to_string();
        McpClient::spawn_with_env(binary, [("FERROSA_MEMORY_CONFIG", config_path.as_str())]).await
    }

    fn live_mcp_test_config() -> tempfile::NamedTempFile {
        use std::io::Write as _;

        let mut file = tempfile::NamedTempFile::new().expect("create live MCP test config");
        write!(
            file,
            r#"
[server]
transport = "stdio"
tenant_id = "00000000-0000-0000-0000-000000000001"

[ferrosa]
contact_points = ["127.0.0.1:19542"]
keyspace = "agent_memory_test"
username = "cassandra"
password = "cassandra"

[viz]
enabled = false

[embeddings]
provider = "synthetic"
ollama_base_url = ""
model = "synthetic-ci"
dimensions = 768
"#
        )
        .expect("write live MCP test config");
        file.flush().expect("flush live MCP test config");
        file
    }

    fn live_mcp_consolidation_test_config(host: &str, port: &str) -> tempfile::NamedTempFile {
        use std::io::Write as _;

        let mut file =
            tempfile::NamedTempFile::new().expect("create live MCP consolidation config");
        write!(
            file,
            r#"
[server]
transport = "stdio"
tenant_id = "00000000-0000-0000-0000-000000000001"

[ferrosa]
contact_points = ["{host}:{port}"]
keyspace = "agent_memory_test"
username = "cassandra"
password = "cassandra"

[viz]
enabled = false

[embeddings]
provider = "synthetic"
ollama_base_url = ""
model = "synthetic-ci"
dimensions = 768

[consolidation]
enabled = true
poll_seconds = 1
lease_seconds = 3
min_interval_seconds = 0
stale_edge_max_days = 0
edge_decay_factor = 1.0
"#
        )
        .expect("write live MCP consolidation config");
        file.flush().expect("flush live MCP consolidation config");
        file
    }

    async fn wait_for_leased_owner<S: ferrosa_memory_core::storage::Storage>(
        storage: &S,
        ctx: &ferrosa_memory_core::types::TenantContext,
        session_id: uuid::Uuid,
        timeout: Duration,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let request = storage
                .consolidation_request_get(ctx, session_id)
                .await
                .expect("read consolidation request");
            if let Some(request) = request
                && request.state == ferrosa_memory_core::types::ConsolidationRequestState::Leased
                && let Some(owner) = request.lease_owner
            {
                return Some(owner);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    async fn wait_for_changed_leased_owner<S: ferrosa_memory_core::storage::Storage>(
        storage: &S,
        ctx: &ferrosa_memory_core::types::TenantContext,
        session_id: uuid::Uuid,
        old_owner: &str,
        timeout: Duration,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let request = storage
                .consolidation_request_get(ctx, session_id)
                .await
                .expect("read consolidation request");
            if let Some(request) = request
                && request.state == ferrosa_memory_core::types::ConsolidationRequestState::Leased
                && let Some(owner) = request.lease_owner
                && owner != old_owner
            {
                return Some(owner);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    fn lease_owner_pid(owner: &str) -> Option<u32> {
        owner.rsplit_once('@')?.1.parse().ok()
    }

    async fn wait_for_smart_ingest_ok(
        client: &mut McpClient,
        tool: &str,
        args: &serde_json::Value,
        timeout: Duration,
    ) -> Result<(), McpClientError> {
        let deadline = Instant::now() + timeout;
        loop {
            match client.call_tool(tool, args.clone()).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
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

        if std::path::Path::new(&debug_path).exists() {
            debug_path
        } else if std::path::Path::new(&release_path).exists() {
            release_path
        } else {
            panic!(
                "ferrosa-memory-mcp binary not found. Build it first with: \
                 cargo build -p ferrosa-memory-mcp"
            );
        }
    }
}
