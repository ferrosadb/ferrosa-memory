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
//! - `GET /subscribe/anomalies` — SSE stream of anomaly alerts (served on viz port)
//!
//! ## Security
//!
//! - TLS required in production (configurable)
//! - HTTP Basic auth extracts tenant_id
//! - Connection limit per source IP (FMEA F30)
//! - Idle connection timeout

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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

/// Per-IP connection rate limiter.
///
/// Tracks connection counts per source IP within a rolling one-minute window.
/// When an IP exceeds `max_per_minute` connections, subsequent connections
/// are rejected until the window resets.
pub struct RateLimiter {
    limits: Mutex<HashMap<IpAddr, (usize, Instant)>>,
    max_per_minute: usize,
}

impl RateLimiter {
    /// Create a rate limiter allowing `max_per_minute` connections per IP.
    pub fn new(max_per_minute: usize) -> Self {
        assert!(max_per_minute > 0, "max_per_minute must be positive");
        Self {
            limits: Mutex::new(HashMap::new()),
            max_per_minute,
        }
    }

    /// Check whether a connection from `ip` should be allowed.
    ///
    /// Returns `true` if the connection is within the rate limit, `false` if
    /// the IP has exceeded the limit. Automatically resets the counter when
    /// the one-minute window expires.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut limits = self.limits.lock().expect("rate limiter lock poisoned");
        let now = Instant::now();
        let entry = limits.entry(ip).or_insert((0, now));

        // Reset window if more than 60 seconds have elapsed
        if now.duration_since(entry.1).as_secs() >= 60 {
            entry.0 = 0;
            entry.1 = now;
        }

        entry.0 += 1;
        entry.0 <= self.max_per_minute
    }
}

/// Load a TLS acceptor from PEM certificate and key files.
///
/// Reads the certificate chain and private key, then constructs a
/// `tokio_rustls::TlsAcceptor` suitable for wrapping TCP streams.
fn load_tls_acceptor(cert_path: &str, key_path: &str) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use std::fs::File;
    use std::io::BufReader;
    use tokio_rustls::rustls;

    let cert_file = File::open(cert_path)
        .map_err(|e| anyhow::anyhow!("failed to open cert file {cert_path}: {e}"))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("failed to parse certificates: {e}"))?;

    if certs.is_empty() {
        return Err(anyhow::anyhow!("no certificates found in {cert_path}"));
    }

    let key_file = File::open(key_path)
        .map_err(|e| anyhow::anyhow!("failed to open key file {key_path}: {e}"))?;
    let mut key_reader = BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| anyhow::anyhow!("failed to parse private key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path}"))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// HTTP server configuration.
pub struct HttpConfig {
    pub port: u16,
    pub require_tls: bool,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

/// Run the HTTP transport server.
///
/// Listens for TCP connections and handles MCP JSON-RPC over HTTP.
/// Each request is authenticated via HTTP Basic auth.
///
/// When `require_tls` is true and certificate/key paths are configured,
/// connections are wrapped in TLS via `tokio-rustls`. If `require_tls` is
/// true but cert/key paths are missing, the server logs an error and exits.
///
/// All connections are rate-limited to 50 per IP per minute (FMEA F30).
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
    // Set up TLS if required
    let tls_acceptor = if config.require_tls {
        let cert = config.cert_path.as_deref().ok_or_else(|| {
            anyhow::anyhow!("require_tls is true but cert_path is not configured")
        })?;
        let key = config
            .key_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("require_tls is true but key_path is not configured"))?;
        let acceptor = load_tls_acceptor(cert, key)?;
        tracing::info!("TLS enabled with cert={cert} key={key}");
        Some(acceptor)
    } else {
        None
    };

    let rate_limiter = RateLimiter::new(50);
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr).await?;
    let protocol = if tls_acceptor.is_some() {
        "HTTPS"
    } else {
        "HTTP"
    };
    tracing::info!("{protocol} server listening on {addr}");

    loop {
        let (stream, peer) = listener.accept().await?;

        // Rate limit by source IP
        if !rate_limiter.check(peer.ip()) {
            tracing::warn!("rate limit exceeded for {peer}, dropping connection");
            drop(stream);
            continue;
        }

        if let Some(ref acceptor) = tls_acceptor {
            // TLS-wrapped connection
            match acceptor.accept(stream).await {
                Ok(mut tls_stream) => {
                    if let Err(e) = handle_connection_rw(
                        &mut tls_stream,
                        storage.as_ref(),
                        &metrics,
                        credential_validator.as_ref(),
                    )
                    .await
                    {
                        tracing::warn!("TLS connection from {peer} error: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!("TLS handshake failed from {peer}: {e}");
                }
            }
        } else {
            // Plain TCP connection
            let mut stream = stream;
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
}

/// Handle a single HTTP connection over a plain TCP stream.
///
/// Delegates to `handle_connection_rw` which is generic over any
/// `AsyncRead + AsyncWrite` stream.
async fn handle_connection<S: Storage>(
    stream: &mut tokio::net::TcpStream,
    storage: &S,
    metrics: &MemoryMetrics,
    credential_validator: &CredentialValidator,
) -> anyhow::Result<()> {
    handle_connection_rw(stream, storage, metrics, credential_validator).await
}

/// Handle a single HTTP connection over any async read/write stream.
///
/// Reads the HTTP request, extracts auth, dispatches MCP, returns response.
/// Works with both plain TCP and TLS-wrapped streams.
async fn handle_connection_rw<S: Storage, T: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut T,
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
        let (mut stream, peer) = listener.accept().await?;
        let bus = Arc::clone(&event_bus);

        // Peek at the request path before deciding how to handle it.
        // /consolidate needs storage access (not Send-safe), so handle it
        // on this task. Everything else is spawned.
        let mut peek_buf = [0u8; 256];
        let peeked = stream.peek(&mut peek_buf).await.unwrap_or(0);
        let peek_str = String::from_utf8_lossy(&peek_buf[..peeked]);

        if peek_str.contains("POST /consolidate") {
            // Cancel safety: runs on the accept task (not spawned), so only
            // cancelled on shutdown. run_consolidation is idempotent (edge
            // upserts), so partial completion is safe. Edge events are
            // best-effort — missed events show up on next snapshot refresh.
            let result = crate::dream::run_consolidation(storage.as_ref(), &ctx, session_id).await;
            let (status, body) = match result {
                Ok(r) => {
                    for (src, tgt) in &r.edges {
                        event_bus.emit(crate::viz::VizEvent::EdgeCreated {
                            edge: crate::viz::VizEdge {
                                source: src.to_string(),
                                target: tgt.to_string(),
                                edge_type: "CO_OCCURS".into(),
                                strength: None,
                            },
                        });
                    }
                    let json = serde_json::json!({
                        "entities_processed": r.entities_processed,
                        "connections_created": r.connections_created,
                        "insights": r.insights,
                    });
                    ("200 OK", json.to_string())
                }
                Err(e) => {
                    let json = serde_json::json!({"error": e.to_string()});
                    ("500 Internal Server Error", json.to_string())
                }
            };
            // Consume the request then send response (async to avoid CPU spin)
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status}\r\n\
                 Content-Type: application/json\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            continue;
        }

        // Proxy /viz/api/enrich/models to LM Studio to list available models.
        if peek_str.contains("GET /viz/api/enrich/models") {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;

            // Extract llm_url from query string (?url=http://...)
            let llm_url = if let Some(pos) = peek_str.find("url=") {
                let start = pos + 4;
                let end = peek_str[start..]
                    .find(|c: char| ['&', ' ', '\r', '\n'].contains(&c))
                    .map(|i| start + i)
                    .unwrap_or(peek_str.len().min(start + 100));
                peek_str[start..end]
                    .replace("%3A", ":")
                    .replace("%2F", "/")
                    .replace("%3a", ":")
                    .replace("%2f", "/")
            } else {
                "http://localhost:1234".to_string()
            };

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default();
            let (status, body) = match client
                .get(format!("{}/v1/models", llm_url.trim_end_matches('/')))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    ("200 OK", resp.text().await.unwrap_or_default())
                }
                Ok(resp) => {
                    let s = resp.status();
                    (
                        "502 Bad Gateway",
                        format!("{{\"error\":\"LLM API returned {s}\"}}"),
                    )
                }
                Err(e) => (
                    "502 Bad Gateway",
                    format!("{{\"error\":\"{}\"}}", e.to_string().replace('"', "'")),
                ),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\n\
                 Content-Type: application/json\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            continue;
        }

        // Check for session_id override in query string (e.g., /viz/ws?session=UUID)
        let effective_session = if let Some(pos) = peek_str.find("session=") {
            let start = pos + 8;
            let end = peek_str[start..]
                .find(|c: char| ['&', ' ', '\r', '\n'].contains(&c))
                .map(|i| start + i)
                .unwrap_or(peek_str.len().min(start + 36));
            Uuid::parse_str(&peek_str[start..end]).unwrap_or(session_id)
        } else {
            session_id
        };

        // Build snapshot before spawning because Storage async methods are not
        // Send-bounded. The snapshot is built per-connection to stay fresh.
        let snapshot = build_snapshot(&*storage, &ctx, effective_session).await;
        tokio::spawn(async move {
            if let Err(e) = handle_viz_connection(stream, bus, snapshot).await {
                tracing::debug!("viz connection from {peer} closed: {e}");
            }
        });
    }
}

/// Handle a single viz HTTP connection.
///
/// Routes `/viz` to static HTML, `/viz/ws` to WebSocket upgrade, and
/// `/subscribe/anomalies` to an SSE stream of anomaly alerts (Sprint 4.9).
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
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Cache-Control: no-cache, no-store, must-revalidate\r\n\
                 Pragma: no-cache\r\n\
                 Expires: 0\r\n\
                 Content-Length: {}\r\n\r\n{}",
                VIZ_HTML.len(),
                VIZ_HTML
            );
            stream.write_all(response.as_bytes()).await?;
        }
        ("GET", "/viz/snapshot") => {
            let body = serde_json::to_string(&snapshot).unwrap_or_default();
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Cache-Control: no-cache\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Content-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await?;
        }
        ("GET", "/subscribe/anomalies") => {
            handle_anomaly_sse(stream, event_bus).await?;
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
/// Sends a clustered `VizEvent::Snapshot` (crate level by default), then
/// listens for both event bus broadcasts and client drill-down messages.
/// The full flat node/edge data is kept in memory so drill-down requests
/// can be served without re-querying storage.
async fn handle_viz_ws(
    ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    event_bus: Arc<EventBus>,
    snapshot: VizEvent,
) {
    use futures_util::StreamExt;

    let (mut write, mut read) = futures_util::StreamExt::split(ws_stream);

    // Extract the full flat data from the snapshot for clustering.
    let (full_nodes, full_edges) = match &snapshot {
        VizEvent::Snapshot { nodes, edges, .. } => (nodes.clone(), edges.clone()),
        _ => (vec![], vec![]),
    };

    // Track navigation state for drill_up support.
    let mut nav_stack: Vec<(viz::VizLevel, Option<String>)> = Vec::new();
    let mut current_level = viz::VizLevel::Crate;
    let mut current_parent: Option<String> = None;

    // Track view mode: "detail" (flat) or "overview" (clustered).
    // Default to clustered overview when graph is large (>2000 nodes).
    let large_graph = full_nodes.len() > 2000;
    let mut view_mode = if large_graph {
        String::from("overview")
    } else {
        String::from("detail")
    };

    // Send initial snapshot — clustered for large graphs, flat for small.
    let initial = if large_graph {
        current_level = viz::VizLevel::Crate;
        cluster_snapshot(&full_nodes, &full_edges, &viz::VizLevel::Crate, None)
    } else {
        snapshot
    };
    if let Ok(json) = serde_json::to_string(&initial) {
        if write.send(Message::Text(json)).await.is_err() {
            return;
        }
    }

    // Subscribe to incremental events (after snapshot to avoid race)
    let mut rx = event_bus.subscribe();

    // Multiplex: listen for both broadcast events and client messages.
    loop {
        tokio::select! {
            // Broadcast event from the event bus
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        // Only forward incremental events when at the flat
                        // (function) level — clustered levels get full
                        // snapshots on drill-down so incrementals would be
                        // confusing.
                        if current_level == viz::VizLevel::Function {
                            let json = match serde_json::to_string(&event) {
                                Ok(j) => j,
                                Err(e) => {
                                    tracing::warn!("viz: failed to serialize event: {e}");
                                    continue;
                                }
                            };
                            if write.send(Message::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("viz: WebSocket client lagged by {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            // Client message (drill_down / drill_up)
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(client_msg) = serde_json::from_str::<viz::VizClientMessage>(&text) {
                            let new_snapshot = match client_msg {
                                viz::VizClientMessage::DrillDown { level, parent } => {
                                    // Push current state onto nav stack
                                    nav_stack.push((current_level.clone(), current_parent.clone()));
                                    current_level = level.clone();
                                    current_parent = parent.clone();
                                    cluster_snapshot(
                                        &full_nodes,
                                        &full_edges,
                                        &level,
                                        parent.as_deref(),
                                    )
                                }
                                viz::VizClientMessage::DrillUp => {
                                    if let Some((prev_level, prev_parent)) = nav_stack.pop() {
                                        current_level = prev_level.clone();
                                        current_parent = prev_parent.clone();
                                        cluster_snapshot(
                                            &full_nodes,
                                            &full_edges,
                                            &prev_level,
                                            prev_parent.as_deref(),
                                        )
                                    } else {
                                        // Already at top — send crate level
                                        current_level = viz::VizLevel::Crate;
                                        current_parent = None;
                                        cluster_snapshot(
                                            &full_nodes,
                                            &full_edges,
                                            &viz::VizLevel::Crate,
                                            None,
                                        )
                                    }
                                }
                                viz::VizClientMessage::ToggleView { mode } => {
                                    view_mode = mode.clone();
                                    if mode == "overview" {
                                        // Reset to crate-level clustered view
                                        nav_stack.clear();
                                        current_level = viz::VizLevel::Crate;
                                        current_parent = None;
                                        cluster_snapshot(
                                            &full_nodes,
                                            &full_edges,
                                            &viz::VizLevel::Crate,
                                            None,
                                        )
                                    } else {
                                        // "detail" — send full flat snapshot
                                        nav_stack.clear();
                                        current_level = viz::VizLevel::Function;
                                        current_parent = None;
                                        let total_n = full_nodes.len();
                                        let total_e = full_edges.len();
                                        VizEvent::Snapshot {
                                            nodes: full_nodes.clone(),
                                            edges: full_edges.clone(),
                                            level: None,
                                            parent: None,
                                            total_nodes: Some(total_n),
                                            total_edges: Some(total_e),
                                        }
                                    }
                                }
                                viz::VizClientMessage::ExploreNeighborhood { entity_id, hops } => {
                                    let hops = hops.min(3); // cap at 3
                                    neighborhood_snapshot(
                                        &full_nodes,
                                        &full_edges,
                                        &entity_id,
                                        hops,
                                    )
                                }
                            };
                            if let Ok(json) = serde_json::to_string(&new_snapshot) {
                                if write.send(Message::Text(json)).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // Ignore ping/pong/binary
                }
            }
        }
    }
}

/// Handle an SSE connection for anomaly alert subscriptions (Sprint 4.9).
///
/// Sends HTTP headers for Server-Sent Events, then subscribes to the event
/// bus and streams only `AnomalyDetected` events as SSE `data:` lines.
/// Runs until the client disconnects or the event bus is dropped.
async fn handle_anomaly_sse(
    mut stream: tokio::net::TcpStream,
    event_bus: Arc<EventBus>,
) -> anyhow::Result<()> {
    // Send SSE response headers
    let headers = "HTTP/1.1 200 OK\r\n\
                   Content-Type: text/event-stream\r\n\
                   Cache-Control: no-cache\r\n\
                   Connection: keep-alive\r\n\
                   \r\n";
    stream.write_all(headers.as_bytes()).await?;

    let mut rx = event_bus.subscribe();
    tracing::info!("anomaly SSE client connected");

    loop {
        match rx.recv().await {
            Ok(event) => {
                // Only forward AnomalyDetected events
                if !matches!(event, VizEvent::AnomalyDetected { .. }) {
                    continue;
                }
                let json = match serde_json::to_string(&event) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!("anomaly SSE: failed to serialize event: {e}");
                        continue;
                    }
                };
                let sse_frame = format!("event: anomaly\ndata: {json}\n\n");
                if stream.write_all(sse_frame.as_bytes()).await.is_err() {
                    break; // Client disconnected
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("anomaly SSE: client lagged by {n} events");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break; // EventBus dropped
            }
        }
    }

    Ok(())
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
    // Query entities for the configured session (nil UUID is a valid session).
    tracing::info!(
        tenant_id = %ctx.tenant_id,
        %session_id,
        session_is_nil = session_id.is_nil(),
        "viz: building snapshot"
    );
    let entities_result = storage.entity_list_session(ctx, session_id).await;

    let mut nodes: Vec<viz::VizNode> = match &entities_result {
        Ok(entities) => {
            tracing::info!(count = entities.len(), "viz: loaded entities for snapshot");
            entities.iter().map(viz::entity_to_viz_node).collect()
        }
        Err(e) => {
            tracing::warn!("viz: failed to load entities for snapshot: {e}");
            Vec::new()
        }
    };

    // Load folds so MENTIONED_IN and FOLDED_INTO edges have visible targets.
    let folds_result = storage.fold_list_all(ctx).await;
    match folds_result {
        Ok(folds) => nodes.extend(folds.iter().map(viz::fold_to_viz_node)),
        Err(e) => tracing::warn!("viz: failed to load folds for snapshot: {e}"),
    }

    let node_ids: std::collections::HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();

    // Load edges using the swapped tenant context — legacy edges have
    // tenant_id and session_id swapped due to a data bug. The node_ids
    // filter below ensures only edges between visible entities are shown.
    let swapped_ctx = TenantContext {
        tenant_id: session_id,
        session_origin: ctx.session_origin.clone(),
    };
    let mut all_edges = storage
        .edge_list_all(&swapped_ctx)
        .await
        .unwrap_or_default();
    tracing::info!(
        swapped_count = all_edges.len(),
        "viz: loaded edges with swapped ctx"
    );
    // Also load correctly-keyed edges (from new consolidation runs).
    if let Ok(mut correct) = storage.edge_list_all(ctx).await {
        tracing::info!(
            correct_count = correct.len(),
            "viz: loaded edges with correct ctx"
        );
        all_edges.append(&mut correct);
    }
    tracing::info!(
        total_edges = all_edges.len(),
        node_count = node_ids.len(),
        "viz: total edges before node filter"
    );
    let edges_result: anyhow::Result<Vec<_>> = Ok(all_edges);

    // Send CO_OCCURS edges that have a real strength value.
    // Zero/null strength edges are noise (e.g., bulk-ingested entities that
    // co-occur only because they share the same session).
    let mut edges: Vec<VizEdge> = match edges_result {
        Ok(raw_edges) => raw_edges
            .into_iter()
            .filter_map(|(src, tgt, etype)| {
                let src_s = src.to_string();
                let tgt_s = tgt.to_string();
                if node_ids.contains(src_s.as_str()) && node_ids.contains(tgt_s.as_str()) {
                    Some(VizEdge {
                        source: src_s,
                        target: tgt_s,
                        edge_type: etype,
                        strength: None,
                    })
                } else {
                    None
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!("viz: failed to load edges for snapshot: {e}");
            Vec::new()
        }
    };
    // Assign default strength to CO_OCCURS edges that lack it (from co_occurs_with table).
    for e in &mut edges {
        if e.edge_type == "CO_OCCURS" && e.strength.is_none() {
            e.strength = Some(0.5);
        }
    }

    // Load typed edges (depends_on, contains, calls, etc.).
    // Probe both the configured session and nil session (used by skilltools ingest).
    let mut session_ids_to_probe = vec![session_id];
    let nil = uuid::Uuid::nil();
    if session_id != nil {
        session_ids_to_probe.push(nil);
    }
    let mut typed_edges = Vec::new();
    for sid in session_ids_to_probe {
        match storage.typed_edge_list_session(ctx, sid).await {
            Ok(mut te) => {
                tracing::info!(session_id = %sid, count = te.len(), "viz: loaded typed edges");
                typed_edges.append(&mut te);
            }
            Err(e) => {
                tracing::warn!(session_id = %sid, error = %e, "viz: typed_edge_list_session failed");
            }
        }
    }
    for te in typed_edges {
        let src_s = te.src_id.to_string();
        let dst_s = te.dst_id.to_string();
        if node_ids.contains(src_s.as_str()) && node_ids.contains(dst_s.as_str()) {
            edges.push(VizEdge {
                source: src_s,
                target: dst_s,
                edge_type: te.edge_type,
                strength: Some(te.weight as f32),
            });
        }
    }

    let total_n = nodes.len();
    let total_e = edges.len();
    VizEvent::Snapshot {
        nodes,
        edges,
        level: None,
        parent: None,
        total_nodes: Some(total_n),
        total_edges: Some(total_e),
    }
}

/// Build a clustered `VizEvent::Snapshot` at the requested hierarchy level.
///
/// Operates on already-fetched full node/edge data (avoids requiring `Storage`
/// which is not `Send`-bounded, so this can run inside `tokio::spawn`).
///
/// - `level=crate` (default): groups entities by crate name (first `::` segment),
///   aggregates edges between crates. Non-code entities go into a "Research" cluster.
/// - `level=module&parent=X`: shows modules within crate X.
/// - `level=function&parent=X::Y`: shows leaf entities within module X::Y.
fn cluster_snapshot(
    all_nodes: &[viz::VizNode],
    all_edges: &[viz::VizEdge],
    level: &viz::VizLevel,
    parent: Option<&str>,
) -> VizEvent {
    let graph_total_nodes = all_nodes.len();
    let graph_total_edges = all_edges.len();
    // Helper: extract the crate name from an entity label (first `::` segment).
    fn crate_name(label: &str) -> &str {
        label.split("::").next().unwrap_or(label)
    }

    // Helper: extract crate::module from an entity label (first two `::` segments).
    fn module_name(label: &str) -> String {
        let parts: Vec<&str> = label.split("::").collect();
        if parts.len() >= 2 {
            format!("{}::{}", parts[0], parts[1])
        } else {
            label.to_string()
        }
    }

    // Classify entity types into "code" vs "research" for top-level grouping.
    fn is_code_entity(entity_type: &str) -> bool {
        matches!(
            entity_type,
            "crate"
                | "module"
                | "section"
                | "function"
                | "struct"
                | "enum"
                | "trait"
                | "impl"
                | "method"
                | "const"
                | "type"
                | "macro"
                | "mod"
                | "app"
        )
    }

    fn is_code_entity_group(group_name: &str) -> bool {
        !matches!(
            group_name,
            "Papers" | "People" | "Concepts" | "Organizations" | "Decisions" | "Other"
        )
    }

    match level {
        viz::VizLevel::Crate => {
            // Group all entities by crate. Non-code entities go to "Research".
            let mut crate_groups: std::collections::HashMap<String, Vec<usize>> =
                std::collections::HashMap::new();

            for (i, node) in all_nodes.iter().enumerate() {
                let group = if is_code_entity(&node.entity_type) {
                    let label = &node.label;
                    if label.contains("::") || node.entity_type == "crate" {
                        crate_name(label).to_string()
                    } else {
                        // Bare names (functions without crate prefix) → group as "Ungrouped Code"
                        "Ungrouped Code".to_string()
                    }
                } else {
                    // Group non-code entities by type: "Papers", "People", "Concepts", etc.
                    match node.entity_type.as_str() {
                        "document" => "Papers".to_string(),
                        "person" => "People".to_string(),
                        "concept" => "Concepts".to_string(),
                        "org" => "Organizations".to_string(),
                        "bug" | "decision" | "pattern" | "preference" => "Decisions".to_string(),
                        _ => "Other".to_string(),
                    }
                };
                crate_groups.entry(group).or_default().push(i);
            }

            // Build one aggregate node per crate.
            let mut crate_nodes: Vec<viz::VizNode> = Vec::new();
            // Map original node id -> crate group name
            let mut node_to_crate: std::collections::HashMap<&str, String> =
                std::collections::HashMap::new();

            for (crate_label, member_indices) in &crate_groups {
                let child_count = member_indices.len();

                // Determine entity_type for the cluster node
                let entity_type = if matches!(
                    crate_label.as_str(),
                    "Papers" | "People" | "Concepts" | "Organizations" | "Decisions" | "Other"
                ) {
                    match crate_label.as_str() {
                        "Papers" => "document",
                        "People" => "person",
                        "Concepts" => "concept",
                        "Organizations" => "org",
                        "Decisions" => "decision",
                        _ => "concept",
                    }
                    .to_string()
                } else {
                    "crate".to_string()
                };

                // Use max confidence from members
                let max_confidence = member_indices
                    .iter()
                    .map(|&i| all_nodes[i].confidence)
                    .fold(0.0_f64, f64::max);

                for &i in member_indices {
                    node_to_crate.insert(&all_nodes[i].id, crate_label.clone());
                }

                crate_nodes.push(viz::VizNode {
                    id: format!("cluster:{crate_label}"),
                    label: crate_label.clone(),
                    node_type: "cluster".into(),
                    entity_type,
                    state: "active".into(),
                    confidence: max_confidence,
                    created_at: String::new(),
                    context: format!("{child_count} entities"),
                    child_count: Some(child_count),
                });
            }

            // Aggregate edges between crate clusters.
            let mut edge_weights: std::collections::HashMap<(String, String), (f64, String)> =
                std::collections::HashMap::new();

            for edge in all_edges {
                let src_crate = node_to_crate.get(edge.source.as_str());
                let tgt_crate = node_to_crate.get(edge.target.as_str());
                if let (Some(sc), Some(tc)) = (src_crate, tgt_crate) {
                    if sc == tc {
                        continue; // skip intra-crate edges
                    }
                    let key = if sc < tc {
                        (format!("cluster:{sc}"), format!("cluster:{tc}"))
                    } else {
                        (format!("cluster:{tc}"), format!("cluster:{sc}"))
                    };
                    let weight = edge.strength.unwrap_or(0.5) as f64;
                    let entry = edge_weights
                        .entry(key)
                        .or_insert((0.0, edge.edge_type.clone()));
                    entry.0 += weight;
                }
            }

            let crate_edges: Vec<viz::VizEdge> = edge_weights
                .into_iter()
                .map(|((src, tgt), (weight, etype))| viz::VizEdge {
                    source: src,
                    target: tgt,
                    edge_type: etype,
                    strength: Some(weight.min(1.0) as f32),
                })
                .collect();

            VizEvent::Snapshot {
                nodes: crate_nodes,
                edges: crate_edges,
                level: Some("crate".into()),
                parent: None,
                total_nodes: Some(graph_total_nodes),
                total_edges: Some(graph_total_edges),
            }
        }

        viz::VizLevel::Module => {
            let parent_crate = parent.unwrap_or("");

            // Filter to entities belonging to the specified crate/group.
            let in_crate: Vec<&viz::VizNode> = all_nodes
                .iter()
                .filter(|n| {
                    if !is_code_entity_group(parent_crate) {
                        // For non-code groups, match by entity type
                        match parent_crate {
                            "Papers" => n.entity_type == "document",
                            "People" => n.entity_type == "person",
                            "Concepts" => n.entity_type == "concept",
                            "Organizations" => n.entity_type == "org",
                            "Decisions" => matches!(
                                n.entity_type.as_str(),
                                "decision" | "bug" | "pattern" | "preference"
                            ),
                            _ => !is_code_entity(&n.entity_type),
                        }
                    } else {
                        is_code_entity(&n.entity_type) && crate_name(&n.label) == parent_crate
                    }
                })
                .collect();

            // Group by module (second :: segment).
            let mut mod_groups: std::collections::HashMap<String, Vec<usize>> =
                std::collections::HashMap::new();

            // For non-code groups, show individual entities directly (flat, no sub-grouping)
            if !is_code_entity_group(parent_crate) {
                let node_ids: std::collections::HashSet<&str> =
                    in_crate.iter().map(|n| n.id.as_str()).collect();
                let flat_nodes: Vec<viz::VizNode> = in_crate.iter().map(|n| (*n).clone()).collect();
                let flat_edges: Vec<viz::VizEdge> = all_edges
                    .iter()
                    .filter(|e| {
                        node_ids.contains(e.source.as_str()) && node_ids.contains(e.target.as_str())
                    })
                    .cloned()
                    .collect();
                return VizEvent::Snapshot {
                    nodes: flat_nodes,
                    edges: flat_edges,
                    level: Some("function".into()), // leaf level — no further drill-down
                    parent: Some(parent_crate.to_string()),
                    total_nodes: Some(graph_total_nodes),
                    total_edges: Some(graph_total_edges),
                };
            }

            for (i, node) in in_crate.iter().enumerate() {
                let group = if false {
                    // (dead branch — non-code handled above)
                    node.entity_type.clone()
                } else {
                    let parts: Vec<&str> = node.label.split("::").collect();
                    if parts.len() >= 2 {
                        parts[1].to_string()
                    } else {
                        "(root)".to_string()
                    }
                };
                mod_groups.entry(group).or_default().push(i);
            }

            let mut mod_nodes: Vec<viz::VizNode> = Vec::new();
            let mut node_to_mod: std::collections::HashMap<&str, String> =
                std::collections::HashMap::new();

            for (mod_label, member_indices) in &mod_groups {
                let child_count = member_indices.len();
                let full_label = if !is_code_entity_group(parent_crate) {
                    mod_label.clone()
                } else {
                    format!("{parent_crate}::{mod_label}")
                };

                let max_confidence = member_indices
                    .iter()
                    .map(|&i| in_crate[i].confidence)
                    .fold(0.0_f64, f64::max);

                for &i in member_indices {
                    node_to_mod.insert(&in_crate[i].id, full_label.clone());
                }

                mod_nodes.push(viz::VizNode {
                    id: format!("cluster:{full_label}"),
                    label: mod_label.clone(),
                    node_type: "cluster".into(),
                    entity_type: "module".into(),
                    state: "active".into(),
                    confidence: max_confidence,
                    created_at: String::new(),
                    context: format!("{child_count} entities"),
                    child_count: Some(child_count),
                });
            }

            // Aggregate edges between modules.
            let member_ids: std::collections::HashSet<&str> =
                in_crate.iter().map(|n| n.id.as_str()).collect();

            let mut edge_weights: std::collections::HashMap<(String, String), (f64, String)> =
                std::collections::HashMap::new();

            for edge in all_edges {
                let src_in = member_ids.contains(edge.source.as_str());
                let tgt_in = member_ids.contains(edge.target.as_str());
                if !src_in || !tgt_in {
                    continue;
                }
                let src_mod = node_to_mod.get(edge.source.as_str());
                let tgt_mod = node_to_mod.get(edge.target.as_str());
                if let (Some(sm), Some(tm)) = (src_mod, tgt_mod) {
                    if sm == tm {
                        continue;
                    }
                    let key = if sm < tm {
                        (format!("cluster:{sm}"), format!("cluster:{tm}"))
                    } else {
                        (format!("cluster:{tm}"), format!("cluster:{sm}"))
                    };
                    let weight = edge.strength.unwrap_or(0.5) as f64;
                    let entry = edge_weights
                        .entry(key)
                        .or_insert((0.0, edge.edge_type.clone()));
                    entry.0 += weight;
                }
            }

            let mod_edges: Vec<viz::VizEdge> = edge_weights
                .into_iter()
                .map(|((src, tgt), (weight, etype))| viz::VizEdge {
                    source: src,
                    target: tgt,
                    edge_type: etype,
                    strength: Some(weight.min(1.0) as f32),
                })
                .collect();

            VizEvent::Snapshot {
                nodes: mod_nodes,
                edges: mod_edges,
                level: Some("module".into()),
                parent: Some(parent_crate.to_string()),
                total_nodes: Some(graph_total_nodes),
                total_edges: Some(graph_total_edges),
            }
        }

        viz::VizLevel::Function => {
            let parent_module = parent.unwrap_or("");

            // Filter to entities within the specified module.
            let in_module: Vec<&viz::VizNode> = all_nodes
                .iter()
                .filter(|n| {
                    if !is_code_entity(&n.entity_type) {
                        // For research drill-down, parent_module is the entity_type
                        n.entity_type == parent_module
                    } else {
                        let mn = module_name(&n.label);
                        mn == parent_module
                    }
                })
                .collect();

            let member_ids: std::collections::HashSet<&str> =
                in_module.iter().map(|n| n.id.as_str()).collect();

            // Return the raw entity nodes (no clustering).
            let leaf_nodes: Vec<viz::VizNode> = in_module.iter().map(|n| (*n).clone()).collect();

            let leaf_edges: Vec<viz::VizEdge> = all_edges
                .iter()
                .filter(|e| {
                    member_ids.contains(e.source.as_str()) && member_ids.contains(e.target.as_str())
                })
                .cloned()
                .collect();

            VizEvent::Snapshot {
                nodes: leaf_nodes,
                edges: leaf_edges,
                level: Some("function".into()),
                parent: Some(parent_module.to_string()),
                total_nodes: Some(graph_total_nodes),
                total_edges: Some(graph_total_edges),
            }
        }
    }
}

/// Build a `VizEvent::Snapshot` containing only the neighborhood of a given
/// entity, found by BFS through the edge list for up to `hops` levels.
///
/// Returns all reached nodes plus edges that connect any two reached nodes.
fn neighborhood_snapshot(
    all_nodes: &[viz::VizNode],
    all_edges: &[viz::VizEdge],
    entity_id: &str,
    hops: usize,
) -> VizEvent {
    use std::collections::{HashMap, HashSet, VecDeque};

    let graph_total_nodes = all_nodes.len();
    let graph_total_edges = all_edges.len();

    // Build adjacency list from edges.
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in all_edges {
        adjacency
            .entry(edge.source.as_str())
            .or_default()
            .push(edge.target.as_str());
        adjacency
            .entry(edge.target.as_str())
            .or_default()
            .push(edge.source.as_str());
    }

    // BFS from entity_id for `hops` levels.
    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<(&str, usize)> = VecDeque::new();

    visited.insert(entity_id);
    queue.push_back((entity_id, 0));

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= hops {
            continue;
        }
        if let Some(neighbors) = adjacency.get(current) {
            for &neighbor in neighbors {
                if visited.insert(neighbor) {
                    queue.push_back((neighbor, depth + 1));
                }
            }
        }
    }

    // Collect nodes that were reached.
    let neighborhood_nodes: Vec<viz::VizNode> = all_nodes
        .iter()
        .filter(|n| visited.contains(n.id.as_str()))
        .cloned()
        .collect();

    // Collect edges where both endpoints are in the neighborhood.
    let neighborhood_edges: Vec<viz::VizEdge> = all_edges
        .iter()
        .filter(|e| visited.contains(e.source.as_str()) && visited.contains(e.target.as_str()))
        .cloned()
        .collect();

    VizEvent::Snapshot {
        nodes: neighborhood_nodes,
        edges: neighborhood_edges,
        level: Some("neighborhood".into()),
        parent: Some(entity_id.to_string()),
        total_nodes: Some(graph_total_nodes),
        total_edges: Some(graph_total_edges),
    }
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

    #[test]
    fn rate_limiter_allows_within_limit() {
        let limiter = RateLimiter::new(5);
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        for i in 1..=5 {
            assert!(limiter.check(ip), "connection {i} should be allowed");
        }
    }

    #[test]
    fn rate_limiter_rejects_over_limit() {
        let limiter = RateLimiter::new(3);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        assert!(limiter.check(ip), "connection 1 should be allowed");
        assert!(limiter.check(ip), "connection 2 should be allowed");
        assert!(limiter.check(ip), "connection 3 should be allowed");
        assert!(!limiter.check(ip), "connection 4 should be rejected");
        assert!(!limiter.check(ip), "connection 5 should be rejected");
    }

    #[test]
    fn rate_limiter_independent_per_ip() {
        let limiter = RateLimiter::new(2);
        let ip_a: IpAddr = "10.0.0.1".parse().unwrap();
        let ip_b: IpAddr = "10.0.0.2".parse().unwrap();

        assert!(limiter.check(ip_a));
        assert!(limiter.check(ip_a));
        assert!(!limiter.check(ip_a), "ip_a should be rejected at limit");

        // ip_b is independent and should still be allowed
        assert!(limiter.check(ip_b), "ip_b should be allowed");
        assert!(
            limiter.check(ip_b),
            "ip_b second connection should be allowed"
        );
        assert!(!limiter.check(ip_b), "ip_b should be rejected at limit");
    }

    #[test]
    fn http_config_tls_fields() {
        let config = HttpConfig {
            port: 8765,
            require_tls: false,
            cert_path: None,
            key_path: None,
        };
        assert!(!config.require_tls);
        assert!(config.cert_path.is_none());
        assert!(config.key_path.is_none());

        let config_tls = HttpConfig {
            port: 443,
            require_tls: true,
            cert_path: Some("/etc/ssl/cert.pem".into()),
            key_path: Some("/etc/ssl/key.pem".into()),
        };
        assert!(config_tls.require_tls);
        assert_eq!(config_tls.cert_path.as_deref(), Some("/etc/ssl/cert.pem"));
        assert_eq!(config_tls.key_path.as_deref(), Some("/etc/ssl/key.pem"));
    }

    #[test]
    fn parse_anomaly_subscribe_request() {
        let raw = "GET /subscribe/anomalies HTTP/1.1\r\nHost: localhost:8766\r\nAccept: text/event-stream\r\n\r\n";
        let (method, path, headers, _body) = parse_http_request(raw).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/subscribe/anomalies");
        let has_accept = headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("accept") && v.contains("text/event-stream"));
        assert!(has_accept, "should have SSE accept header");
    }

    // --- build_snapshot tests ---

    use crate::storage::mock::{MockEdge, MockStorage};
    use crate::types::{EntityEntry, MemoryState, TenantContext, TypedEdge};

    fn test_tenant() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            session_origin: String::new(),
        }
    }

    fn test_entity(tenant_id: Uuid, session_id: Uuid, id: Uuid, name: &str) -> EntityEntry {
        EntityEntry {
            tenant_id,
            entity_id: id,
            session_id,
            entity_name: name.to_string(),
            entity_type: "concept".to_string(),
            source_fold_id: None,
            context_snippet: String::new(),
            entity_embedding: None,
            confidence: 0.9,
            state: MemoryState::Active,
            created_at: chrono::Utc::now(),
        }
    }

    fn test_typed_edge(
        tenant_id: Uuid,
        session_id: Uuid,
        src: Uuid,
        dst: Uuid,
        edge_type: &str,
    ) -> TypedEdge {
        TypedEdge {
            tenant_id,
            session_id,
            src_id: src,
            edge_type: edge_type.to_string(),
            dst_id: dst,
            weight: 1.0,
            metadata: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn snapshot_typed_edges_from_nil_session_appear_with_nil_viz_session() {
        let ctx = test_tenant();
        let nil = Uuid::nil();
        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();

        let storage = MockStorage::new();
        storage.entities.lock().await.extend(vec![
            test_entity(ctx.tenant_id, nil, e1, "foo"),
            test_entity(ctx.tenant_id, nil, e2, "bar"),
        ]);
        storage.typed_edges.lock().await.push(test_typed_edge(
            ctx.tenant_id,
            nil,
            e1,
            e2,
            "depends_on",
        ));

        let snap = build_snapshot(&storage, &ctx, nil).await;
        let VizEvent::Snapshot { edges, .. } = snap else {
            panic!("expected Snapshot");
        };
        assert_eq!(edges.len(), 1, "typed edge under nil session should appear");
        assert_eq!(edges[0].edge_type, "depends_on");
    }

    #[tokio::test]
    async fn snapshot_typed_edges_from_nil_session_appear_with_nonnil_viz_session() {
        let ctx = test_tenant();
        let nil = Uuid::nil();
        let viz_session = Uuid::new_v4();
        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();

        let storage = MockStorage::new();
        // Entities under the viz session (entity_list_session filters by session)
        storage.entities.lock().await.extend(vec![
            test_entity(ctx.tenant_id, viz_session, e1, "foo"),
            test_entity(ctx.tenant_id, viz_session, e2, "bar"),
        ]);
        // Edge stored under nil session (e.g. skilltools ingest)
        storage.typed_edges.lock().await.push(test_typed_edge(
            ctx.tenant_id,
            nil,
            e1,
            e2,
            "contains",
        ));

        let snap = build_snapshot(&storage, &ctx, viz_session).await;
        let VizEvent::Snapshot { edges, .. } = snap else {
            panic!("expected Snapshot");
        };
        assert_eq!(
            edges.len(),
            1,
            "nil-session typed edge should appear even when viz uses a different session"
        );
        assert_eq!(edges[0].edge_type, "contains");
    }

    #[tokio::test]
    async fn snapshot_combines_typed_edges_from_both_sessions() {
        let ctx = test_tenant();
        let nil = Uuid::nil();
        let viz_session = Uuid::new_v4();
        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e3 = Uuid::new_v4();

        let storage = MockStorage::new();
        // All entities under the viz session so they appear in the node set
        storage.entities.lock().await.extend(vec![
            test_entity(ctx.tenant_id, viz_session, e1, "a"),
            test_entity(ctx.tenant_id, viz_session, e2, "b"),
            test_entity(ctx.tenant_id, viz_session, e3, "c"),
        ]);
        // Edges split across nil and viz sessions
        storage.typed_edges.lock().await.extend(vec![
            test_typed_edge(ctx.tenant_id, nil, e1, e2, "calls"),
            test_typed_edge(ctx.tenant_id, viz_session, e2, e3, "depends_on"),
        ]);

        let snap = build_snapshot(&storage, &ctx, viz_session).await;
        let VizEvent::Snapshot { edges, .. } = snap else {
            panic!("expected Snapshot");
        };
        assert_eq!(
            edges.len(),
            2,
            "should combine edges from nil and viz session"
        );
        let types: std::collections::HashSet<&str> =
            edges.iter().map(|e| e.edge_type.as_str()).collect();
        assert!(types.contains("calls"));
        assert!(types.contains("depends_on"));
    }

    #[tokio::test]
    async fn snapshot_excludes_typed_edges_with_missing_endpoints() {
        let ctx = test_tenant();
        let nil = Uuid::nil();
        let e1 = Uuid::new_v4();
        let orphan = Uuid::new_v4(); // not in entities

        let storage = MockStorage::new();
        storage
            .entities
            .lock()
            .await
            .push(test_entity(ctx.tenant_id, nil, e1, "foo"));
        // Edge points to entity not in the node set
        storage.typed_edges.lock().await.push(test_typed_edge(
            ctx.tenant_id,
            nil,
            e1,
            orphan,
            "references",
        ));

        let snap = build_snapshot(&storage, &ctx, nil).await;
        let VizEvent::Snapshot { edges, .. } = snap else {
            panic!("expected Snapshot");
        };
        assert_eq!(
            edges.len(),
            0,
            "edge with missing endpoint should be excluded"
        );
    }

    #[tokio::test]
    async fn snapshot_filters_co_occurs_without_strength() {
        let ctx = test_tenant();
        let nil = Uuid::nil();
        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();

        let storage = MockStorage::new();
        storage.entities.lock().await.extend(vec![
            test_entity(ctx.tenant_id, nil, e1, "foo"),
            test_entity(ctx.tenant_id, nil, e2, "bar"),
        ]);
        // MockEdge produces CO_OCCURS with no strength via edge_list_all
        storage.edges.lock().await.push(MockEdge {
            source: e1,
            target: e2,
            edge_type: "CO_OCCURS".to_string(),
            session_id: nil,
        });

        let snap = build_snapshot(&storage, &ctx, nil).await;
        let VizEvent::Snapshot { edges, .. } = snap else {
            panic!("expected Snapshot");
        };
        assert_eq!(
            edges.len(),
            0,
            "CO_OCCURS without strength should be filtered out"
        );
    }
}
