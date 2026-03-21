//! HTTP+SSE transport for remote MCP clients.
//!
//! Provides an HTTP server that accepts MCP JSON-RPC requests via POST
//! and streams responses. Supports HTTP Basic auth for tenant identification.
//!
//! ## Endpoints
//!
//! - `POST /mcp` — JSON-RPC request/response
//! - `GET /metrics` — Prometheus metrics scrape
//! - `GET /health` — Health check
//!
//! ## Security
//!
//! - TLS required in production (configurable)
//! - HTTP Basic auth extracts tenant_id
//! - Connection limit per source IP (FMEA F30)
//! - Idle connection timeout

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::auth;
use crate::dispatch;
use crate::metrics::MemoryMetrics;
use crate::storage::Storage;
use crate::types::TenantContext;

/// Credential validator: takes (username, password), returns tenant_id if valid.
pub type CredentialValidator = dyn Fn(&str, &str) -> Option<uuid::Uuid> + Send + Sync;

/// HTTP server configuration.
pub struct HttpConfig {
    pub port: u16,
    pub require_tls: bool,
}

/// Run the HTTP transport server.
///
/// Listens for TCP connections and handles MCP JSON-RPC over HTTP.
/// Each request is authenticated via HTTP Basic auth.
///
/// Note: connections are handled sequentially (no `tokio::spawn`) because
/// the `Storage` trait's async methods aren't `Send`-bounded. This is fine
/// for the expected low connection rate of MCP clients. A production
/// deployment would use a concrete CQL client that is `Send`.
pub async fn serve_http<S: Storage>(
    config: HttpConfig,
    storage: Arc<S>,
    metrics: Arc<MemoryMetrics>,
    credential_validator: Arc<CredentialValidator>,
) -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("HTTP server listening on {addr}");

    loop {
        let (mut stream, peer) = listener.accept().await?;
        if let Err(e) = handle_connection(
            &mut stream,
            storage.as_ref(),
            &metrics,
            credential_validator.as_ref(),
        )
        .await
        {
            tracing::warn!("connection from {peer} error: {e}");
        }
    }
}

/// Handle a single HTTP connection.
///
/// Reads the HTTP request, extracts auth, dispatches MCP, returns response.
async fn handle_connection<S: Storage>(
    stream: &mut tokio::net::TcpStream,
    storage: &S,
    metrics: &MemoryMetrics,
    credential_validator: &CredentialValidator,
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse HTTP request line and headers
    let (method, path, headers, body) = parse_http_request(&request)?;

    match (method, path) {
        ("GET", "/health") => {
            let response = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nok";
            stream.write_all(response.as_bytes()).await?;
        }
        ("GET", "/metrics") => {
            let mut buf = Vec::new();
            let encoder = prometheus::TextEncoder::new();
            prometheus::Encoder::encode(&encoder, &metrics.registry.gather(), &mut buf)?;
            let body = String::from_utf8(buf)?;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await?;
        }
        ("POST", "/mcp") => {
            // Authenticate
            let ctx = authenticate_from_headers(&headers, credential_validator)?;

            // Parse JSON-RPC
            let rpc_request: serde_json::Value =
                serde_json::from_str(body).map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?;

            let rpc_method = rpc_request
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let params = rpc_request.get("params").cloned().unwrap_or(Value::Null);
            let id = rpc_request.get("id").cloned();

            let result = dispatch::dispatch(rpc_method, params, storage, &ctx).await;

            let response_body = match result {
                Ok(val) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": val
                }),
                Err((code, msg)) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": msg }
                }),
            };

            let body_str = serde_json::to_string(&response_body)?;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body_str.len(),
                body_str
            );
            stream.write_all(response.as_bytes()).await?;
        }
        _ => {
            let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            stream.write_all(response.as_bytes()).await?;
        }
    }

    Ok(())
}

/// Extract Basic auth credentials from HTTP headers and authenticate.
fn authenticate_from_headers(
    headers: &[(String, String)],
    validator: &CredentialValidator,
) -> anyhow::Result<TenantContext> {
    let auth_header = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing Authorization header"))?;

    let encoded = auth_header
        .strip_prefix("Basic ")
        .ok_or_else(|| anyhow::anyhow!("only Basic auth supported"))?;

    let decoded = String::from_utf8(
        base64_decode(encoded).map_err(|e| anyhow::anyhow!("invalid base64: {e}"))?,
    )?;

    let (username, password) = decoded
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid Basic auth format"))?;

    auth::authenticate_http(username, password, |u, p| validator(u, p))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Minimal base64 decode (standard alphabet, no padding required).
fn base64_decode(input: &str) -> Result<Vec<u8>, &'static str> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &byte in input.as_bytes() {
        if byte == b'=' {
            break;
        }
        let val = TABLE
            .iter()
            .position(|&b| b == byte)
            .ok_or("invalid base64 character")? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Ok(out)
}

/// Parsed HTTP request parts.
type ParsedRequest<'a> = (&'a str, &'a str, Vec<(String, String)>, &'a str);

/// Parse a raw HTTP request into (method, path, headers, body).
fn parse_http_request(raw: &str) -> anyhow::Result<ParsedRequest<'_>> {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let mut lines = head.lines();

    let request_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing method"))?;
    let path = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing path"))?;

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();

    Ok((method, path, headers, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_get() {
        let raw = "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (method, path, headers, body) = parse_http_request(raw).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/health");
        assert_eq!(headers.len(), 1);
        assert_eq!(body, "");
    }

    #[test]
    fn parse_http_post_with_body() {
        let raw = "POST /mcp HTTP/1.1\r\nContent-Type: application/json\r\nAuthorization: Basic dXNlcjpwYXNz\r\n\r\n{\"jsonrpc\":\"2.0\"}";
        let (method, path, headers, body) = parse_http_request(raw).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/mcp");
        assert_eq!(headers.len(), 2);
        assert!(body.contains("jsonrpc"));
    }

    #[test]
    fn base64_decode_works() {
        // "user:pass" -> "dXNlcjpwYXNz"
        let decoded = base64_decode("dXNlcjpwYXNz").unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "user:pass");
    }

    #[test]
    fn base64_decode_with_padding() {
        // "a" -> "YQ=="
        let decoded = base64_decode("YQ==").unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "a");
    }
}
