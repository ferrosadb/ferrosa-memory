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
        let (stream, peer) = listener.accept().await?;
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
            // Consume the request then send response
            let mut buf = vec![0u8; 4096];
            let _ = stream.try_read(&mut buf);
            let response = format!(
                "HTTP/1.1 {status}\r\n\
                 Content-Type: application/json\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.try_write(response.as_bytes());
            continue;
        }

        // Check for session_id override in query string (e.g., /viz/ws?session=UUID)
        let effective_session = if let Some(pos) = peek_str.find("session=") {
            let start = pos + 8;
            let end = peek_str[start..]
                .find(|c: char| c == '&' || c == ' ' || c == '\r' || c == '\n')
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
    // Also load correctly-keyed edges (from new consolidation runs).
    if let Ok(mut correct) = storage.edge_list_all(ctx).await {
        all_edges.append(&mut correct);
    }
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
    // Drop CO_OCCURS edges with no strength — they add no information.
    edges.retain(|e| e.strength.is_some() || e.edge_type != "CO_OCCURS");

    // Load typed edges (depends_on, contains, calls, etc.) for the configured session.
    let typed_edges = storage
        .typed_edge_list_session(ctx, session_id)
        .await
        .unwrap_or_default();
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
}
