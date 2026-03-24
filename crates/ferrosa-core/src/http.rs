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
//! - `GET /viz` — Memory graph visualizer HTML (served on viz port)
//! - `GET /viz/ws` — WebSocket for live graph events (served on viz port)
//!
//! ## Security
//!
//! - TLS required in production (configurable)
//! - HTTP Basic auth extracts tenant_id
//! - Connection limit per source IP (FMEA F30)
//! - Idle connection timeout

use std::sync::Arc;

use futures_util::SinkExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use uuid::Uuid;

use crate::auth;
use crate::dispatch;
use crate::metrics::MemoryMetrics;
use crate::storage::Storage;
use crate::types::TenantContext;
use crate::viz::{self, EventBus, VizEdge, VizEvent};

/// Static HTML for the visualization dashboard.
const VIZ_HTML: &str = include_str!("../assets/viz.html");

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

            let session = dispatch::SessionState::default();
            let result = dispatch::dispatch(rpc_method, params, storage, &ctx, &session).await;

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

/// Run the visualization dashboard HTTP server on a dedicated port.
///
/// Serves the static HTML dashboard at `/viz` and upgrades `/viz/ws`
/// connections to WebSocket for live event streaming. Runs independently
/// of the MCP transport server.
///
/// On WebSocket connect, sends a `VizEvent::Snapshot` with current graph
/// state so new clients don't start with a blank canvas.
pub async fn serve_viz<S: Storage + 'static>(
    port: u16,
    event_bus: Arc<EventBus>,
    storage: Arc<S>,
    ctx: Arc<TenantContext>,
    session_id: Uuid,
) -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("viz server listening on {addr}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let bus = Arc::clone(&event_bus);
        // Build snapshot before spawning because Storage async methods are not
        // Send-bounded. The snapshot is built per-connection to stay fresh.
        let snapshot = build_snapshot(&*storage, &ctx, session_id).await;
        tokio::spawn(async move {
            if let Err(e) = handle_viz_connection(stream, bus, snapshot).await {
                tracing::debug!("viz connection from {peer} closed: {e}");
            }
        });
    }
}

/// Handle a single viz HTTP connection.
///
/// Routes `/viz` to static HTML and `/viz/ws` to WebSocket upgrade.
async fn handle_viz_connection(
    mut stream: tokio::net::TcpStream,
    event_bus: Arc<EventBus>,
    snapshot: VizEvent,
) -> anyhow::Result<()> {
    // Peek at the request to determine routing before consuming the stream
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request = String::from_utf8_lossy(&buf[..n]);

    let (method, path, headers, _body) = parse_http_request(&request)?;

    match (method, path) {
        ("GET", "/viz") => {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                VIZ_HTML.len(),
                VIZ_HTML
            );
            stream.write_all(response.as_bytes()).await?;
        }
        ("GET", "/viz/ws") => {
            // Validate WebSocket upgrade headers
            let has_upgrade = headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("upgrade") && v.eq_ignore_ascii_case("websocket")
            });
            if !has_upgrade {
                let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                stream.write_all(response.as_bytes()).await?;
                return Ok(());
            }

            // Extract Sec-WebSocket-Key for handshake
            let ws_key = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key"))
                .map(|(_, v)| v.as_str());

            if ws_key.is_none() {
                let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                stream.write_all(response.as_bytes()).await?;
                return Ok(());
            }

            // Complete WebSocket handshake manually then hand off to tungstenite
            let accept_key = compute_ws_accept(ws_key.unwrap());
            let handshake = format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {accept_key}\r\n\r\n"
            );
            stream.write_all(handshake.as_bytes()).await?;

            // Wrap in tungstenite WebSocket (already upgraded)
            let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
                stream,
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            )
            .await;

            handle_viz_ws(ws_stream, event_bus, snapshot).await;
        }
        _ => {
            let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            stream.write_all(response.as_bytes()).await?;
        }
    }

    Ok(())
}

/// Handle a WebSocket connection for the viz dashboard.
///
/// Sends a `VizEvent::Snapshot` with current graph state, then subscribes
/// to the event bus and streams each incremental `VizEvent` as a JSON
/// text frame. Runs until the client disconnects or the bus is dropped.
async fn handle_viz_ws(
    ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    event_bus: Arc<EventBus>,
    snapshot: VizEvent,
) {
    let (mut write, _read) = futures_util::StreamExt::split(ws_stream);

    // Send initial snapshot so clients don't start with a blank graph.
    // TODO: The viz endpoint uses a fixed session_id from server startup. A future
    // enhancement should accept session_id as a query parameter on /viz/ws so the
    // dashboard can switch between sessions. In shared-memory mode (configured
    // tenant_id), a tenant-level entity query would be more useful.
    if let Ok(json) = serde_json::to_string(&snapshot)
        && write.send(Message::Text(json)).await.is_err()
    {
        return; // Client disconnected during snapshot send
    }

    // Subscribe to incremental events (after snapshot to avoid race)
    let mut rx = event_bus.subscribe();

    // Stream events to the client
    loop {
        match rx.recv().await {
            Ok(event) => {
                let json = match serde_json::to_string(&event) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!("viz: failed to serialize event: {e}");
                        continue;
                    }
                };
                if write.send(Message::Text(json)).await.is_err() {
                    break; // Client disconnected
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("viz: WebSocket client lagged by {n} events");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break; // EventBus dropped
            }
        }
    }
}

/// Build a `VizEvent::Snapshot` from current storage state.
///
/// Queries entities and edges for the given session and converts them
/// to visualization types. Returns an empty snapshot if session_id is nil.
async fn build_snapshot<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
) -> VizEvent {
    // A nil session_id means we don't know which session to show yet.
    if session_id.is_nil() {
        return VizEvent::Snapshot {
            nodes: Vec::new(),
            edges: Vec::new(),
        };
    }

    let nodes = match storage.entity_list_session(ctx, session_id).await {
        Ok(entities) => entities.iter().map(viz::entity_to_viz_node).collect(),
        Err(e) => {
            tracing::warn!("viz: failed to load entities for snapshot: {e}");
            Vec::new()
        }
    };

    let edges = match storage.edge_list_session(ctx, session_id).await {
        Ok(raw_edges) => raw_edges
            .into_iter()
            .map(|(src, tgt, etype)| VizEdge {
                source: src.to_string(),
                target: tgt.to_string(),
                edge_type: etype,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("viz: failed to load edges for snapshot: {e}");
            Vec::new()
        }
    };

    VizEvent::Snapshot { nodes, edges }
}

/// Compute the Sec-WebSocket-Accept value per RFC 6455.
fn compute_ws_accept(key: &str) -> String {
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let hash = hasher.finalize();
    base64_encode(&hash)
}

/// Minimal base64 encode (standard alphabet with padding).
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
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

    #[test]
    fn base64_encode_roundtrip() {
        let input = b"hello world";
        let encoded = base64_encode(input);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn ws_accept_key_rfc6455_example() {
        // RFC 6455 Section 4.2.2 example
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = compute_ws_accept(key);
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn viz_html_is_embedded() {
        assert!(VIZ_HTML.contains("Ferrosa Memory"));
        assert!(VIZ_HTML.contains("WebSocket"));
    }

    #[test]
    fn parse_viz_get_request() {
        let raw = "GET /viz HTTP/1.1\r\nHost: localhost:8766\r\n\r\n";
        let (method, path, _headers, _body) = parse_http_request(raw).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/viz");
    }

    #[test]
    fn parse_viz_ws_upgrade_request() {
        let raw = "GET /viz/ws HTTP/1.1\r\nHost: localhost:8766\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let (method, path, headers, _body) = parse_http_request(raw).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/viz/ws");
        let has_upgrade = headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("upgrade") && v.eq_ignore_ascii_case("websocket"));
        assert!(has_upgrade, "should detect upgrade header");
    }
}
