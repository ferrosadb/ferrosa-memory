//! MCP protocol transport layer (stdio).
//!
//! Implements the JSON-RPC framing for MCP over stdio. Each message is a
//! single JSON object on one line, delimited by newlines.
//!
//! ## Error handling
//!
//! Malformed JSON-RPC is caught at the transport layer and returns a proper
//! JSON-RPC error response — the server never panics on bad input (FMEA F29).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A JSON-RPC 2.0 request.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// Standard JSON-RPC error codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

impl JsonRpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Handler function type — takes a method and params, returns a result or error.
pub type Handler = Box<
    dyn Fn(
            &str,
            Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Value, (i32, String)>> + Send>,
        > + Send
        + Sync,
>;

/// Run the stdio transport loop.
///
/// Reads JSON-RPC requests from stdin, dispatches to the handler,
/// and writes responses to stdout. Runs until stdin is closed.
pub async fn serve_stdio(handler: Handler) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(req) => {
                let id = req.id.clone();
                let result = match (handler)(&req.method, req.params).await {
                    Ok(result) => JsonRpcResponse::success(id.clone(), result),
                    Err((code, msg)) => JsonRpcResponse::error(id.clone(), code, msg),
                };
                // JSON-RPC notifications omit `id` and must not receive a response.
                id.map(|_| result)
            }
            Err(e) => Some(JsonRpcResponse::error(
                None,
                PARSE_ERROR,
                format!("parse error: {e}"),
            )),
        };

        if let Some(response) = response {
            let mut out = serde_json::to_vec(&response)?;
            out.push(b'\n');
            stdout.write_all(&out).await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.id, Some(Value::Number(1.into())));
    }

    #[test]
    fn parse_request_without_params() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.params, Value::Null);
    }

    #[test]
    fn parse_notification_no_id() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn notification_without_id_would_not_emit_response() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        let id = req.id.clone();
        let response = id.map(|_| JsonRpcResponse::success(req.id, serde_json::json!(null)));
        assert!(response.is_none());
    }

    #[test]
    fn malformed_json_does_not_panic() {
        let json = r#"{"not valid json"#;
        let result = serde_json::from_str::<JsonRpcRequest>(json);
        assert!(result.is_err());
    }

    #[test]
    fn success_response_serializes() {
        let resp = JsonRpcResponse::success(
            Some(Value::Number(1.into())),
            serde_json::json!({"ok": true}),
        );
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"result\""));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn error_response_serializes() {
        let resp =
            JsonRpcResponse::error(Some(Value::Number(1.into())), METHOD_NOT_FOUND, "not found");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"error\""));
        assert!(s.contains("-32601"));
        assert!(!s.contains("\"result\""));
    }
}
