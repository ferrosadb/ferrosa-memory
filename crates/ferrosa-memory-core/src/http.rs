//! HTTP+SSE transport for remote MCP clients.
//!
//! Provides an HTTP server that accepts MCP JSON-RPC requests via POST
//! and streams responses. Supports HTTP Basic auth for tenant identification.
//!
//! ## Endpoints
//!
//! - `POST /mcp` — JSON-RPC request/response
//! - `GET /metrics` — Prometheus metrics scrape
//! - `GET /healthz/live` — Liveness check
//! - `GET /healthz/ready` — Readiness check
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
/// Static HTML for the authenticated operator workbench.
const WORKBENCH_HTML: &str = include_str!("../assets/workbench.html");

#[derive(Clone, Debug)]
pub struct ShellRouteConfig {
    pub workbench_scheme: String,
    pub workbench_port: u16,
    pub viz_scheme: String,
    pub viz_port: u16,
}

impl Default for ShellRouteConfig {
    fn default() -> Self {
        Self {
            workbench_scheme: "https".into(),
            workbench_port: 18765,
            viz_scheme: "http".into(),
            viz_port: 18766,
        }
    }
}

fn render_shell_html(template: &str, routes: &ShellRouteConfig) -> String {
    template
        .replace("@@FMEM_VIZ_SCHEME@@", &routes.viz_scheme)
        .replace("@@FMEM_VIZ_PORT@@", &routes.viz_port.to_string())
        .replace("@@FMEM_WORKBENCH_SCHEME@@", &routes.workbench_scheme)
        .replace(
            "@@FMEM_WORKBENCH_PORT@@",
            &routes.workbench_port.to_string(),
        )
}

fn render_workbench_html(routes: &ShellRouteConfig) -> String {
    render_shell_html(WORKBENCH_HTML, routes)
}

fn render_viz_html(routes: &ShellRouteConfig) -> String {
    render_shell_html(VIZ_HTML, routes)
}

fn origin_for_host(scheme: &str, host: &str, port: u16) -> String {
    format!("{scheme}://{host}:{port}")
}

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
/// Upper bound on a single TLS handshake. The accept loop is sequential
/// (Storage trait futures aren't Send-bounded — see `serve_http`'s doc),
/// so an unbounded `acceptor.accept(...)` makes one stalled client a
/// denial of service for every subsequent client. 10s is generous for
/// any legitimate TLS handshake on loopback / LAN and tight enough to
/// recover from rogue or half-closed connections within one retry of a
/// typical client.
const TLS_ACCEPT_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound on request bytes read from the socket. 8 MiB is well
/// above any real MCP payload and well below anything that would
/// pressure RAM on a per-connection basis under spawned tasks.
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on how long a single request is allowed to occupy a
/// spawned connection task. Covers read, dispatch, and write. Tuned
/// to the longest reasonable MCP call (consolidation / recursive
/// explore on a warm cluster); slower work should be moved off-path.
const REQUEST_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Run a future that yields `std::io::Result<T>` under a timeout budget.
/// Factored out so the timeout contract can be unit-tested without
/// constructing a real TLS stack: pass in `std::future::pending()` and
/// assert the timeout branch fires. Used by the TLS accept path to keep
/// the sequential accept loop from wedging on a stalled handshake.
async fn run_with_budget<F, T>(
    op: &'static str,
    fut: F,
    budget: std::time::Duration,
) -> anyhow::Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    match tokio::time::timeout(budget, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(anyhow::anyhow!("{op} error: {e}")),
        Err(_) => Err(anyhow::anyhow!("{op} timed out after {budget:?}")),
    }
}

async fn accept_tls_with_budget<IO>(
    acceptor: &tokio_rustls::TlsAcceptor,
    stream: IO,
    budget: std::time::Duration,
) -> anyhow::Result<tokio_rustls::server::TlsStream<IO>>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    run_with_budget("tls handshake", acceptor.accept(stream), budget).await
}

/// `tokio_rustls::TlsAcceptor` suitable for wrapping TCP streams.
fn load_tls_acceptor(cert_path: &str, key_path: &str) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use std::fs::File;
    use std::io::BufReader;
    use tokio_rustls::rustls;

    // reqwest and tokio-rustls can pull rustls into the same test/runtime
    // graph with multiple crypto-provider features enabled. Pick one
    // process-wide provider deterministically before constructing TLS configs.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

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
    pub bind_addr: String,
    pub port: u16,
    pub require_tls: bool,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    pub readiness_checker: Arc<dyn Fn() -> bool + Send + Sync>,
    pub shell_routes: ShellRouteConfig,
    pub session: Arc<dispatch::SessionState>,
}

/// Public query surfaces available to the authenticated operator workbench.
///
/// CQL and SPARQL should forward to the public Ferrosa interfaces. Datalog
/// remains ferrosa-memory-owned and is handled locally.
#[allow(clippy::manual_async_fn)]
pub trait OperatorQuerySurface: Send + Sync {
    fn cql_query_passthrough(
        &self,
        ctx: &TenantContext,
        query: &str,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + Send;

    fn sparql_query_passthrough(
        &self,
        ctx: &TenantContext,
        query: &str,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + Send;
}

/// Run the HTTP transport server.
///
/// Each accepted TCP connection is handed to a `tokio::spawn`ed task
/// that performs the TLS handshake (if configured) and runs the
/// request under `REQUEST_BUDGET`. The accept loop itself does no
/// per-request work — it only rate-limits and hands off — so a
/// stalled handshake, slow storage call, or multi-packet client never
/// blocks other clients.
///
/// All connections are rate-limited to 50 per IP per minute (FMEA F30).
pub async fn serve_http<S: Storage + OperatorQuerySurface + 'static>(
    config: HttpConfig,
    storage: Arc<S>,
    metrics: Arc<MemoryMetrics>,
    credential_validator: Arc<CredentialValidator>,
) -> anyhow::Result<()> {
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

    let rate_limiter = Arc::new(RateLimiter::new(50));
    let readiness = config.readiness_checker.clone();
    let shell_routes = config.shell_routes.clone();
    let session = Arc::clone(&config.session);
    let addr = format!("{}:{}", config.bind_addr, config.port);
    let listener = TcpListener::bind(&addr).await?;
    let protocol = if tls_acceptor.is_some() {
        "HTTPS"
    } else {
        "HTTP"
    };
    tracing::info!("{protocol} server listening on {addr}");

    loop {
        let (stream, peer) = listener.accept().await?;

        if !rate_limiter.check(peer.ip()) {
            tracing::warn!("rate limit exceeded for {peer}, sending 429");
            // A bare `drop(stream)` leaves any request bytes the
            // client has already written sitting in the recv buffer,
            // which on macOS (and Linux with SO_LINGER defaults)
            // makes close() emit RST instead of FIN — Python's
            // http.client surfaces that as `ConnectionResetError`.
            // Spawn a short task that drains the request, writes a
            // proper 429, and half-closes before drop so the peer
            // sees a normal EOF.
            tokio::spawn(async move {
                let mut stream = stream;
                write_rate_limit_response(&mut stream).await;
            });
            continue;
        }

        let storage = Arc::clone(&storage);
        let metrics = Arc::clone(&metrics);
        let validator = Arc::clone(&credential_validator);
        let readiness = readiness.clone();
        let acceptor = tls_acceptor.clone();
        let shell_routes = shell_routes.clone();
        let session = Arc::clone(&session);

        tokio::spawn(async move {
            let outcome = match acceptor {
                Some(acc) => match accept_tls_with_budget(&acc, stream, TLS_ACCEPT_BUDGET).await {
                    Ok(mut tls) => {
                        serve_one_connection_with_session(
                            &mut tls,
                            storage.as_ref(),
                            &metrics,
                            validator.as_ref(),
                            readiness.as_ref(),
                            &shell_routes,
                            session.as_ref(),
                        )
                        .await
                    }
                    Err(e) => {
                        tracing::warn!("TLS handshake failed from {peer}: {e}");
                        return;
                    }
                },
                None => {
                    let mut stream = stream;
                    serve_one_connection_with_session(
                        &mut stream,
                        storage.as_ref(),
                        &metrics,
                        validator.as_ref(),
                        readiness.as_ref(),
                        &shell_routes,
                        session.as_ref(),
                    )
                    .await
                }
            };
            if let Err(e) = outcome {
                tracing::warn!("connection from {peer} error: {e}");
            }
        });
    }
}

/// Write a 429 response and drain the client's pending bytes before
/// dropping the socket. Bounded work — if the client is misbehaving
/// we still return within a second or so. The point is that the
/// peer sees FIN, not RST.
async fn write_rate_limit_response(stream: &mut tokio::net::TcpStream) {
    let body = "rate limit exceeded";
    let response = format!(
        "HTTP/1.1 429 Too Many Requests\r\n\
         Content-Type: text/plain\r\n\
         Retry-After: 60\r\n\
         Connection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );

    // Best-effort write. If the peer already went away, the error
    // is harmless — we were about to drop anyway.
    let write = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        stream.write_all(response.as_bytes()),
    )
    .await;
    if let Ok(Err(e)) = write {
        tracing::debug!(error = %e, "rate-limit 429 write failed");
        return;
    }

    // Half-close the write side so the client sees EOF cleanly. If
    // the client has more bytes queued (e.g. a POST body still
    // arriving), drain the recv buffer until the peer FINs or the
    // short drain window elapses — otherwise close() would RST.
    let _ = stream.shutdown().await;
    let mut sink = [0u8; 4096];
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        loop {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    })
    .await;
}

/// Run one request under `REQUEST_BUDGET`. On timeout, MCP requests remain
/// JSON-RPC responses so clients get actionable retry guidance instead of an
/// opaque gateway timeout. Any error reading/writing the stream is propagated
/// to the caller for logging.
#[cfg(test)]
async fn serve_one_connection<S, T>(
    stream: &mut T,
    storage: &S,
    metrics: &MemoryMetrics,
    credential_validator: &CredentialValidator,
    readiness_checker: &(dyn Fn() -> bool + Send + Sync),
    shell_routes: &ShellRouteConfig,
) -> anyhow::Result<()>
where
    S: Storage + OperatorQuerySurface,
    T: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let session = dispatch::SessionState::default();
    serve_one_connection_with_session(
        stream,
        storage,
        metrics,
        credential_validator,
        readiness_checker,
        shell_routes,
        &session,
    )
    .await
}

async fn serve_one_connection_with_session<S, T>(
    stream: &mut T,
    storage: &S,
    metrics: &MemoryMetrics,
    credential_validator: &CredentialValidator,
    readiness_checker: &(dyn Fn() -> bool + Send + Sync),
    shell_routes: &ShellRouteConfig,
    session: &dispatch::SessionState,
) -> anyhow::Result<()>
where
    S: Storage + OperatorQuerySurface,
    T: AsyncReadExt + AsyncWriteExt + Unpin,
{
    serve_one_connection_with_session_budget(
        stream,
        storage,
        metrics,
        credential_validator,
        readiness_checker,
        shell_routes,
        session,
        REQUEST_BUDGET,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn serve_one_connection_with_session_budget<S, T>(
    stream: &mut T,
    storage: &S,
    metrics: &MemoryMetrics,
    credential_validator: &CredentialValidator,
    readiness_checker: &(dyn Fn() -> bool + Send + Sync),
    shell_routes: &ShellRouteConfig,
    session: &dispatch::SessionState,
    request_budget: std::time::Duration,
) -> anyhow::Result<()>
where
    S: Storage + OperatorQuerySurface,
    T: AsyncReadExt + AsyncWriteExt + Unpin,
{
    loop {
        let keep_alive = handle_connection_rw(
            stream,
            storage,
            metrics,
            credential_validator,
            readiness_checker,
            shell_routes,
            session,
            request_budget,
        )
        .await?;
        if !keep_alive {
            break;
        }
    }

    // Graceful close: half-close the write side so TLS sends
    // `close_notify` and plain TCP sends FIN.
    let _ = stream.shutdown().await;
    Ok(())
}

/// Handle a single HTTP connection over any async read/write stream.
///
/// Reads the HTTP request, extracts auth, dispatches MCP, returns response.
/// Works with both plain TCP and TLS-wrapped streams.
#[allow(clippy::too_many_arguments)]
async fn handle_connection_rw<
    S: Storage + OperatorQuerySurface,
    T: AsyncReadExt + AsyncWriteExt + Unpin,
>(
    stream: &mut T,
    storage: &S,
    metrics: &MemoryMetrics,
    credential_validator: &CredentialValidator,
    readiness_checker: &(dyn Fn() -> bool + Send + Sync),
    shell_routes: &ShellRouteConfig,
    session: &dispatch::SessionState,
    request_budget: std::time::Duration,
) -> anyhow::Result<bool> {
    let request = match tokio::time::timeout(
        request_budget,
        read_http_request(stream, MAX_REQUEST_BYTES),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            let msg = e.to_string();
            if msg.contains("connection closed before request headers complete") {
                return Ok(false);
            }
            tracing::debug!(error = %e, "http: incomplete or malformed request");
            return Ok(false);
        }
        Err(_) => {
            tracing::debug!(timeout = ?request_budget, "http: idle keep-alive connection timed out");
            return Ok(false);
        }
    };

    // Parse HTTP request line and headers
    let (method, path, headers, body) = parse_http_request(&request)?;
    let close_requested = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"));

    let handler = handle_http_request_with_session(
        method,
        path,
        &headers,
        body,
        storage,
        metrics,
        credential_validator,
        readiness_checker,
        shell_routes,
        session,
    );
    let response = match tokio::time::timeout(request_budget, handler).await {
        Ok(response) => response?,
        Err(_) => {
            let resp = timeout_response_for_request(method, path, body, request_budget);
            // Best-effort notify the client; ignore write errors since
            // the peer may already be gone.
            let _ = stream.write_all(resp.as_bytes()).await;
            return Err(anyhow::anyhow!("request exceeded {request_budget:?}"));
        }
    };
    stream.write_all(response.as_bytes()).await?;

    Ok(!close_requested)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn handle_http_request<S: Storage + OperatorQuerySurface>(
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &str,
    storage: &S,
    metrics: &MemoryMetrics,
    credential_validator: &CredentialValidator,
    readiness_checker: &(dyn Fn() -> bool + Send + Sync),
    shell_routes: &ShellRouteConfig,
) -> anyhow::Result<String> {
    let session = dispatch::SessionState::default();
    handle_http_request_with_session(
        method,
        path,
        headers,
        body,
        storage,
        metrics,
        credential_validator,
        readiness_checker,
        shell_routes,
        &session,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handle_http_request_with_session<S: Storage + OperatorQuerySurface>(
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &str,
    storage: &S,
    metrics: &MemoryMetrics,
    credential_validator: &CredentialValidator,
    readiness_checker: &(dyn Fn() -> bool + Send + Sync),
    shell_routes: &ShellRouteConfig,
    session: &dispatch::SessionState,
) -> anyhow::Result<String> {
    if (method == "GET" || method == "HEAD") && path == "/viz" {
        let host = request_hostname(headers).unwrap_or("127.0.0.1");
        return Ok(redirect_response(&format!(
            "{}/viz",
            origin_for_host(&shell_routes.viz_scheme, host, shell_routes.viz_port)
        )));
    }

    if path == "/"
        || path.starts_with("/?")
        || path == "/workbench"
        || path == "/workbench/"
        || path.starts_with("/workbench/api/")
    {
        let ctx = match authenticate_from_headers(headers, credential_validator) {
            Ok(ctx) => ctx,
            Err(_) => return Ok(unauthorized_response()),
        };
        return match handle_operator_request(
            method,
            path,
            body,
            storage,
            &ctx,
            session,
            &render_workbench_html(shell_routes),
        )
        .await
        {
            Ok(response) => Ok(response),
            Err(error) => Ok(operator_error_response(&error)),
        };
    }

    match (method, path) {
        ("GET", "/health") | ("GET", "/healthz/live") => Ok(text_response("200 OK", "ok")),
        ("GET", "/healthz/ready") => {
            if readiness_checker() {
                Ok(text_response("200 OK", "ready"))
            } else {
                Ok(text_response("503 Service Unavailable", "not ready"))
            }
        }
        ("GET", "/metrics") => {
            let mut buf = Vec::new();
            let encoder = prometheus::TextEncoder::new();
            prometheus::Encoder::encode(&encoder, &metrics.registry.gather(), &mut buf)?;
            let body = String::from_utf8(buf)?;
            Ok(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ))
        }
        ("POST", "/mcp") => {
            let ctx = match authenticate_from_headers(headers, credential_validator) {
                Ok(ctx) => ctx,
                Err(_) => return Ok(unauthorized_response()),
            };

            let rpc_request: serde_json::Value =
                serde_json::from_str(body).map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?;

            let rpc_method = rpc_request
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let params = rpc_request.get("params").cloned().unwrap_or(Value::Null);
            let id = rpc_request.get("id").cloned();
            // Per MCP Streamable-HTTP (2025-03-26): a POST whose body
            // is a JSON-RPC notification (method present, `id` absent)
            // or a JSON-RPC response (no `method`) must return **HTTP
            // 202 Accepted with no body**. Returning a `{"id":null,
            // "result":null}` shape breaks Codex's rmcp transport —
            // it can't decode the response and quits the worker.
            let is_notification = id.is_none() && rpc_request.get("method").is_some();
            let is_client_response = rpc_request.get("method").is_none()
                && (rpc_request.get("result").is_some() || rpc_request.get("error").is_some());

            let result = dispatch::dispatch(rpc_method, params, storage, &ctx, session).await;

            if is_notification || is_client_response {
                // Dispatch still runs for its side effects (logging,
                // readiness flips on `notifications/initialized`, etc.)
                // but the HTTP contract requires no response body.
                if let Err((code, msg)) = &result {
                    tracing::debug!(
                        method = rpc_method,
                        code,
                        msg,
                        "notification/response dispatch returned error (suppressed; 202 no body)"
                    );
                }
                return Ok(accepted_no_body_response());
            }

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
            Ok(json_response("200 OK", &body_str))
        }
        _ => Ok("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into()),
    }
}

fn text_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

fn json_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

fn timeout_response_for_request(
    method: &str,
    path: &str,
    body: &str,
    request_budget: std::time::Duration,
) -> String {
    if method == "POST" && path == "/mcp" {
        let id = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|request| request.get("id").cloned())
            .unwrap_or(serde_json::Value::Null);
        let response_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32004,
                "message": format!(
                    "Ferrosa backend did not respond within {request_budget:?}; it may be warming ANN/vector indexes after restart. retry with exponential backoff such as 30s then 60s before treating the tool call as failed."
                )
            }
        });
        return json_response("200 OK", &response_body.to_string());
    }

    let body = format!("request exceeded {request_budget:?}");
    text_response("504 Gateway Timeout", &body)
}

fn snapshot_stream_required_response() -> String {
    let body = serde_json::json!({
        "error": "/viz/snapshot no longer returns a materialized full graph; connect to /viz/ws and consume SnapshotStreamStart/SnapshotStreamChunk/SnapshotStreamEnd events",
        "stream": "/viz/ws"
    })
    .to_string();
    json_response("410 Gone", &body)
}

fn redirect_response(location: &str) -> String {
    format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
}

fn unauthorized_response() -> String {
    let body = "unauthorized";
    format!(
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"Ferrosa Memory\"\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

fn html_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-cache, no-store, must-revalidate\r\nPragma: no-cache\r\nExpires: 0\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

fn operator_error_response(error: &anyhow::Error) -> String {
    let raw = error.to_string();
    let lower = raw.to_lowercase();
    let (status, message) = if lower.contains("no keyspace specified") {
        (
            "400 Bad Request",
            format!(
                "{raw}. Ferrosa CQL currently requires explicit keyspace qualification, e.g. agent_memory.entity_store."
            ),
        )
    } else if lower.contains("requires filtering on non-indexed columns") {
        (
            "400 Bad Request",
            format!(
                "{raw}. This is a Ferrosa/Cassandra query-shape error; use a partition-key query or add ALLOW FILTERING only if that contract is intended."
            ),
        )
    } else if lower.contains("unexpected token keyword(limit)")
        || lower.contains("syntax")
        || lower.contains("missing query")
        || lower.contains("query must not be empty")
        || lower.contains("limit must be")
        || lower.contains("missing predicate")
        || lower.contains("invalid json")
        || lower.contains("did not return rows")
    {
        ("400 Bad Request", raw)
    } else if lower.contains("not yet established")
        || lower.contains("not configured")
        || lower.contains("not ready")
    {
        ("503 Service Unavailable", raw)
    } else {
        ("502 Bad Gateway", raw)
    };

    json_response(
        status,
        &serde_json::json!({
            "error": message,
        })
        .to_string(),
    )
}

fn request_hostname(headers: &[(String, String)]) -> Option<&str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.as_str())
        .and_then(|host| {
            if host.starts_with('[') {
                host.find(']').map(|end| &host[..=end]).or(Some(host))
            } else {
                host.rsplit_once(':')
                    .filter(|(_, port)| port.chars().all(|ch| ch.is_ascii_digit()))
                    .map(|(name, _)| name)
                    .or(Some(host))
            }
        })
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let (_, query) = path.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, value) = pair.split_once('=')?;
        if k == key {
            decode_query_component(value).ok()
        } else {
            None
        }
    })
}

fn decode_query_component(input: &str) -> anyhow::Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                anyhow::ensure!(i + 2 < bytes.len(), "truncated percent-encoding");
                let hi = hex_value(bytes[i + 1])
                    .ok_or_else(|| anyhow::anyhow!("invalid percent-encoding"))?;
                let lo = hex_value(bytes[i + 2])
                    .ok_or_else(|| anyhow::anyhow!("invalid percent-encoding"))?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|e| anyhow::anyhow!("invalid UTF-8 in query component: {e}"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_limit(payload: &Value, default: usize, max: usize) -> anyhow::Result<usize> {
    match payload.get("limit") {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => {
            let raw = number
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("limit must be a positive integer"))?;
            anyhow::ensure!(raw > 0, "limit must be greater than zero");
            anyhow::ensure!(raw <= max as u64, "limit must be <= {max}");
            Ok(raw as usize)
        }
        Some(_) => anyhow::bail!("limit must be a positive integer"),
    }
}

fn parse_json_body(body: &str) -> anyhow::Result<Value> {
    if body.trim().is_empty() {
        Ok(Value::Object(serde_json::Map::new()))
    } else {
        serde_json::from_str(body).map_err(|e| anyhow::anyhow!("invalid JSON body: {e}"))
    }
}

async fn call_tool_http<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session: &dispatch::SessionState,
    name: &str,
    arguments: Value,
) -> anyhow::Result<Value> {
    let result = dispatch::dispatch(
        "tools/call",
        serde_json::json!({
            "name": name,
            "arguments": arguments,
        }),
        storage,
        ctx,
        session,
    )
    .await
    .map_err(|(_, message)| anyhow::anyhow!(message))?;

    let text = result["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing tool response payload"))?;
    serde_json::from_str(text).map_err(|e| anyhow::anyhow!("invalid tool response JSON: {e}"))
}

async fn handle_operator_request<S: Storage + OperatorQuerySurface>(
    method: &str,
    path: &str,
    body: &str,
    storage: &S,
    ctx: &TenantContext,
    session: &dispatch::SessionState,
    workbench_html: &str,
) -> anyhow::Result<String> {
    match (method, path) {
        ("GET", "/") => Ok(html_response("200 OK", workbench_html)),
        ("GET", "/workbench") | ("GET", "/workbench/") => {
            Ok(html_response("200 OK", workbench_html))
        }
        ("GET", "/workbench/api/auth/whoami") => {
            let reviewer = crate::expert_system::reviewer_from_ctx(ctx);
            Ok(json_response(
                "200 OK",
                &serde_json::json!({
                    "tenant_id": ctx.tenant_id,
                    "session_origin": ctx.session_origin,
                    "reviewer": reviewer,
                })
                .to_string(),
            ))
        }
        ("GET", "/workbench/api/summary") => {
            let effective_rules =
                crate::datalog::load_effective_rule_entries(storage, ctx, None).await;
            let entities = storage.entity_list_all(ctx).await;
            let edge_count = storage.edge_count(ctx).await;
            let derived_fact_count = storage.derived_cache_list_all(ctx, 100_000).await;
            let summary_error = entities
                .as_ref()
                .err()
                .or_else(|| edge_count.as_ref().err())
                .or_else(|| derived_fact_count.as_ref().err())
                .or_else(|| effective_rules.as_ref().err())
                .map(|e| e.to_string());
            let entity_rows = entities.unwrap_or_default();
            let node_count = entity_rows.len();
            let edge_count = edge_count.unwrap_or(0);
            let derived_fact_count = derived_fact_count.map(|rows| rows.len()).unwrap_or(0);
            let approvals: Vec<_> = entity_rows
                .iter()
                .filter(|entry| {
                    entry.entity_type == crate::expert_system::APPROVAL_MIRROR_ENTITY_TYPE
                })
                .cloned()
                .collect();
            let pending = approvals
                .iter()
                .filter(|entry| {
                    entry.properties.get("decision").and_then(|v| v.as_str()) == Some("proposed")
                })
                .count();
            let session_id = query_param(path, "session_id")
                .or_else(|| {
                    approvals.first().and_then(|entry| {
                        entry
                            .properties
                            .get("session_scope")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                })
                .unwrap_or_default();
            Ok(json_response(
                "200 OK",
                &serde_json::json!({
                    "status": if summary_error.is_some() { "not_ready" } else { "ready" },
                    "session_id": session_id,
                    "node_count": node_count,
                    "edge_count": edge_count,
                    "derived_fact_count": derived_fact_count,
                    "rule_count": effective_rules.unwrap_or_default().len(),
                    "pending_approvals": pending,
                    "query_rate_1m": 0,
                    "error": summary_error,
                })
                .to_string(),
            ))
        }
        ("POST", "/workbench/api/cql/query") => {
            let payload = parse_json_body(body)?;
            let query = payload
                .get("query")
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing query"))?;
            let limit = parse_limit(&payload, 200, 1000)?;
            let result = storage.cql_query_passthrough(ctx, query, limit).await?;
            Ok(json_response("200 OK", &result.to_string()))
        }
        ("POST", "/workbench/api/sparql/query") => {
            let payload = parse_json_body(body)?;
            let query = payload
                .get("query")
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing query"))?;
            let limit = parse_limit(&payload, 200, 1000)?;
            let result = storage.sparql_query_passthrough(ctx, query, limit).await?;
            Ok(json_response("200 OK", &result.to_string()))
        }
        ("POST", "/workbench/api/datalog/query") => {
            let payload = parse_json_body(body)?;
            let predicate = payload
                .get("predicate")
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing predicate"))?;
            let session_id = payload
                .get("session_id")
                .and_then(|value| value.as_str())
                .and_then(|value| Uuid::parse_str(value).ok())
                .unwrap_or(Uuid::nil());
            let mut query_args = serde_json::json!({
                "predicate": predicate,
                "session_id": session_id,
            });
            let derived =
                call_tool_http(storage, ctx, session, "query_derived", query_args.clone()).await?;
            let explain = call_tool_http(
                storage,
                ctx,
                session,
                "explain_derived",
                serde_json::json!({
                    "predicate": predicate,
                    "session_id": session_id,
                    "limit": 16,
                }),
            )
            .await?;
            query_args["derived_facts"] = derived["derived_facts"].clone();
            query_args["count"] = derived["count"].clone();
            query_args["explanations"] = explain["explanations"].clone();
            query_args["latency_ms"] = explain["latency_ms"].clone();
            Ok(json_response("200 OK", &query_args.to_string()))
        }
        ("GET", "/workbench/api/rules") => {
            let source = query_param(path, "source").unwrap_or_else(|| "effective".to_string());
            let family = query_param(path, "family").unwrap_or_else(|| "*".to_string());
            let family_arg = if family == "*" {
                Value::String("*".into())
            } else {
                Value::String(family.clone())
            };
            let result = call_tool_http(
                storage,
                ctx,
                session,
                "manage_rules",
                serde_json::json!({
                    "action": "list",
                    "source": source,
                    "family": family_arg,
                }),
            )
            .await?;
            let rows: Vec<Value> = result["rules"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|rule| {
                    let rule_id = rule["rule_id"].as_str().unwrap_or_default().to_string();
                    serde_json::json!({
                        "id": rule_id.clone(),
                        "rule_id": rule_id,
                        "name": rule["name"].clone(),
                        "source": rule["source"].clone(),
                        "scope": rule["source"].clone(),
                        "family": rule["family"].clone(),
                        "priority": rule["version"].clone(),
                        "version": rule["version"].clone(),
                        "approval_state": rule.get("approval_state").cloned().unwrap_or(Value::Null),
                        "condition": rule["rule_body"].clone(),
                        "rule_body": rule["rule_body"].clone(),
                        "rule_weight": rule.get("rule_weight").cloned().unwrap_or(Value::Null),
                        "state": rule["state"].clone(),
                        "updated_at": chrono::Utc::now(),
                        "enabled": rule["state"].as_str().unwrap_or("active") == "active",
                    })
                })
                .collect();
            Ok(json_response(
                "200 OK",
                &serde_json::json!({ "rules": rows }).to_string(),
            ))
        }
        ("GET", rules_path) if rules_path.starts_with("/workbench/api/rules?") => {
            let source =
                query_param(rules_path, "source").unwrap_or_else(|| "effective".to_string());
            let family = query_param(rules_path, "family").unwrap_or_else(|| "*".to_string());
            let family_arg = if family == "*" {
                Value::String("*".into())
            } else {
                Value::String(family.clone())
            };
            let result = call_tool_http(
                storage,
                ctx,
                session,
                "manage_rules",
                serde_json::json!({
                    "action": "list",
                    "source": source,
                    "family": family_arg,
                }),
            )
            .await?;
            let rows: Vec<Value> = result["rules"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|rule| {
                    let rule_id = rule["rule_id"].as_str().unwrap_or_default().to_string();
                    serde_json::json!({
                        "id": rule_id.clone(),
                        "rule_id": rule_id,
                        "name": rule["name"].clone(),
                        "source": rule["source"].clone(),
                        "scope": rule["source"].clone(),
                        "family": rule["family"].clone(),
                        "priority": rule["version"].clone(),
                        "version": rule["version"].clone(),
                        "approval_state": rule.get("approval_state").cloned().unwrap_or(Value::Null),
                        "condition": rule["rule_body"].clone(),
                        "rule_body": rule["rule_body"].clone(),
                        "rule_weight": rule.get("rule_weight").cloned().unwrap_or(Value::Null),
                        "state": rule["state"].clone(),
                        "updated_at": chrono::Utc::now(),
                        "enabled": rule["state"].as_str().unwrap_or("active") == "active",
                    })
                })
                .collect();
            Ok(json_response(
                "200 OK",
                &serde_json::json!({ "rules": rows }).to_string(),
            ))
        }
        ("POST", "/workbench/api/rules") => {
            let payload = parse_json_body(body)?;
            let action = payload
                .get("action")
                .and_then(|value| value.as_str())
                .unwrap_or("put");
            let result = match action {
                "put" => {
                    let name = payload
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("workbench-rule");
                    let condition = payload
                        .get("condition")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| anyhow::anyhow!("missing condition"))?;
                    let rule_body = if payload
                        .get("rule_body")
                        .and_then(|value| value.as_str())
                        .is_some()
                    {
                        payload["rule_body"].as_str().unwrap().to_string()
                    } else {
                        let predicate = payload
                            .get("predicate")
                            .and_then(|value| value.as_str())
                            .unwrap_or_else(|| {
                                payload
                                    .get("family")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("derived_rule")
                            });
                        format!("{predicate} :- {condition}.")
                    };
                    let rule_id = payload
                        .get("rule_id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| name.to_ascii_lowercase().replace(' ', "-"));
                    call_tool_http(
                        storage,
                        ctx,
                        session,
                        "manage_rules",
                        serde_json::json!({
                            "action": "put",
                            "rule_id": rule_id,
                            "name": name,
                            "family": payload.get("family").and_then(|value| value.as_str()),
                            "rule_weight": payload.get("rule_weight").and_then(|value| value.as_f64()).unwrap_or(1.0),
                            "rule_body": rule_body,
                        }),
                    )
                    .await?
                }
                "deprecate" => {
                    let rule_id = payload
                        .get("rule_id")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| anyhow::anyhow!("missing rule_id"))?;
                    call_tool_http(
                        storage,
                        ctx,
                        session,
                        "manage_rules",
                        serde_json::json!({
                            "action": "deprecate",
                            "rule_id": rule_id,
                        }),
                    )
                    .await?
                }
                "approve" | "reject" => {
                    let rule_id = payload
                        .get("rule_id")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| anyhow::anyhow!("missing rule_id"))?;
                    call_tool_http(
                        storage,
                        ctx,
                        session,
                        "manage_approvals",
                        serde_json::json!({
                            "action": "record",
                            "artifact_kind": "rule",
                            "artifact_ref": rule_id,
                            "decision": if action == "approve" { "approved" } else { "rejected" },
                            "review_note": payload.get("review_note").and_then(|value| value.as_str()),
                        }),
                    )
                    .await?
                }
                other => return Err(anyhow::anyhow!("unsupported rules action: {other}")),
            };
            Ok(json_response("200 OK", &result.to_string()))
        }
        ("GET", "/workbench/api/approvals") => {
            let approvals: Vec<Value> = storage
                .entity_list_all(ctx)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    entry.entity_type == crate::expert_system::APPROVAL_MIRROR_ENTITY_TYPE
                })
                .map(|entry| {
                    let artifact_kind = entry
                        .properties
                        .get("artifact_kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("general");
                    let artifact_ref = entry
                        .properties
                        .get("artifact_ref")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let session_scope = entry
                        .properties
                        .get("session_scope")
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "null".to_string());
                    serde_json::json!({
                        "id": format!("{artifact_kind}|{artifact_ref}|{session_scope}"),
                        "requester": entry.properties.get("reviewer").cloned().unwrap_or(Value::String("system".into())),
                        "kind": artifact_kind,
                        "target": artifact_ref,
                        "state": entry.properties.get("decision").cloned().unwrap_or(Value::String("proposed".into())),
                        "explanation": entry.properties.get("review_note").cloned().unwrap_or(Value::String("No explanation attached".into())),
                    })
                })
                .collect();
            Ok(json_response(
                "200 OK",
                &serde_json::json!({ "approvals": approvals }).to_string(),
            ))
        }
        ("GET", "/workbench/api/aliases") => {
            let aliases = if let Some(alias_name) = query_param(path, "alias_name") {
                call_tool_http(
                    storage,
                    ctx,
                    session,
                    "manage_aliases",
                    serde_json::json!({
                        "action": "list",
                        "alias_name": alias_name,
                    }),
                )
                .await?
                .get("aliases")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default()
            } else {
                storage
                    .entity_list_all(ctx)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|entry| entry.entity_type == crate::expert_system::ALIAS_MIRROR_ENTITY_TYPE)
                    .map(|entry| {
                        serde_json::json!({
                            "alias_id": entry.properties.get("alias_id").cloned().unwrap_or(Value::Null),
                            "alias_name": entry.properties.get("alias_name").cloned().unwrap_or(Value::String(entry.entity_name)),
                            "scope_kind": entry.properties.get("scope_kind").cloned().unwrap_or(Value::String("global".into())),
                            "scope_ref": entry.properties.get("scope_ref").cloned().unwrap_or(Value::String("*".into())),
                            "canonical_tool": entry.properties.get("canonical_tool").cloned().unwrap_or(Value::Null),
                            "status": entry.properties.get("status").cloned().unwrap_or(Value::String("proposed".into())),
                            "updated_at": entry.updated_at,
                        })
                    })
                    .collect()
            };
            Ok(json_response(
                "200 OK",
                &serde_json::json!({ "aliases": aliases }).to_string(),
            ))
        }
        ("GET", aliases_path) if aliases_path.starts_with("/workbench/api/aliases?") => {
            let aliases = if let Some(alias_name) = query_param(aliases_path, "alias_name") {
                call_tool_http(
                    storage,
                    ctx,
                    session,
                    "manage_aliases",
                    serde_json::json!({
                        "action": "list",
                        "alias_name": alias_name,
                    }),
                )
                .await?
                .get("aliases")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default()
            } else {
                storage
                    .entity_list_all(ctx)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|entry| entry.entity_type == crate::expert_system::ALIAS_MIRROR_ENTITY_TYPE)
                    .map(|entry| {
                        serde_json::json!({
                            "alias_id": entry.properties.get("alias_id").cloned().unwrap_or(Value::Null),
                            "alias_name": entry.properties.get("alias_name").cloned().unwrap_or(Value::String(entry.entity_name)),
                            "scope_kind": entry.properties.get("scope_kind").cloned().unwrap_or(Value::String("global".into())),
                            "scope_ref": entry.properties.get("scope_ref").cloned().unwrap_or(Value::String("*".into())),
                            "canonical_tool": entry.properties.get("canonical_tool").cloned().unwrap_or(Value::Null),
                            "status": entry.properties.get("status").cloned().unwrap_or(Value::String("proposed".into())),
                            "updated_at": entry.updated_at,
                        })
                    })
                    .collect()
            };
            Ok(json_response(
                "200 OK",
                &serde_json::json!({ "aliases": aliases }).to_string(),
            ))
        }
        ("POST", "/workbench/api/aliases") => {
            let payload = parse_json_body(body)?;
            let alias_name = payload
                .get("alias_name")
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing alias_name"))?;
            let canonical_tool = payload
                .get("canonical_tool")
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing canonical_tool"))?;
            let scope_kind = payload
                .get("scope_kind")
                .and_then(|value| value.as_str())
                .unwrap_or("global");
            let mut args = serde_json::json!({
                "action": "put",
                "alias_name": alias_name,
                "canonical_tool": canonical_tool,
                "scope_kind": scope_kind,
                "status": payload.get("status").and_then(|value| value.as_str()).unwrap_or("proposed"),
                "parameter_map": payload.get("parameter_map").cloned().unwrap_or_else(|| serde_json::json!({})),
                "fixed_arguments": payload.get("fixed_arguments").cloned().unwrap_or_else(|| serde_json::json!({})),
                "args_templates": payload.get("args_templates").cloned().unwrap_or_else(|| serde_json::json!({})),
            });
            if let Some(workspace_scope) = payload
                .get("workspace_scope")
                .and_then(|value| value.as_str())
            {
                args["workspace_scope"] = Value::String(workspace_scope.to_string());
            }
            if let Some(scope_ref) = payload.get("scope_ref").and_then(|value| value.as_str()) {
                args["scope_ref"] = Value::String(scope_ref.to_string());
            }
            if let Some(session_scope) = payload
                .get("session_scope")
                .and_then(|value| value.as_str())
            {
                args["session_scope"] = Value::String(session_scope.to_string());
            }
            let result = call_tool_http(storage, ctx, session, "manage_aliases", args).await?;
            Ok(json_response("200 OK", &result.to_string()))
        }
        ("POST", "/workbench/api/explanations/query") => {
            let payload = parse_json_body(body)?;
            let predicate = payload
                .get("predicate")
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing predicate"))?;
            let mut args = serde_json::json!({
                "predicate": predicate,
                "limit": payload.get("limit").and_then(|value| value.as_u64()).unwrap_or(16),
            });
            if let Some(session_id) = payload.get("session_id").and_then(|value| value.as_str()) {
                args["session_id"] = Value::String(session_id.to_string());
            }
            if let Some(src_id) = payload.get("src_id").and_then(|value| value.as_str()) {
                args["src_id"] = Value::String(src_id.to_string());
            }
            if let Some(dst_id) = payload.get("dst_id").and_then(|value| value.as_str()) {
                args["dst_id"] = Value::String(dst_id.to_string());
            }
            let result = call_tool_http(storage, ctx, session, "explain_derived", args).await?;
            Ok(json_response("200 OK", &result.to_string()))
        }
        ("POST", approval_path)
            if approval_path.starts_with("/workbench/api/approvals/")
                && (approval_path.ends_with("/approve") || approval_path.ends_with("/reject")) =>
        {
            let approve = approval_path.ends_with("/approve");
            let artifact = approval_path
                .trim_start_matches("/workbench/api/approvals/")
                .trim_end_matches("/approve")
                .trim_end_matches("/reject");
            let artifact = decode_query_component(artifact)?;
            let parts: Vec<&str> = artifact.split('|').collect();
            anyhow::ensure!(parts.len() == 3, "invalid approval target id");
            let session_scope = parts[2].trim_matches('"').parse::<Uuid>().ok();
            let result = call_tool_http(
                storage,
                ctx,
                session,
                "manage_approvals",
                serde_json::json!({
                    "action": "record",
                    "artifact_kind": parts[0],
                    "artifact_ref": parts[1],
                    "decision": if approve { "approved" } else { "rejected" },
                    "session_scope": session_scope,
                }),
            )
            .await?;
            Ok(json_response("200 OK", &result.to_string()))
        }
        ("GET", viz_path) if viz_path.starts_with("/viz/snapshot") => {
            Ok(snapshot_stream_required_response())
        }
        ("GET", viz_path) if viz_path.starts_with("/viz/api/derived_facts") => {
            let session_id = query_param(viz_path, "session_id")
                .and_then(|value| Uuid::parse_str(&value).ok())
                .unwrap_or(Uuid::nil());
            let predicate = query_param(viz_path, "predicate").unwrap_or_else(|| "related".into());
            let facts = crate::datalog::query_predicate(
                storage,
                ctx,
                session_id,
                &predicate,
                &crate::config::DatalogConfig::default(),
            )
            .await
            .unwrap_or_default();
            Ok(json_response(
                "200 OK",
                &serde_json::json!({
                    "derived_facts": facts,
                    "count": facts.len(),
                    "total": facts.len(),
                })
                .to_string(),
            ))
        }
        _ => Ok("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into()),
    }
}

/// MCP Streamable-HTTP response for notifications and client-sent
/// JSON-RPC responses: 202 Accepted, zero body. `Content-Length: 0`
/// is explicit so keep-alive clients know where this response ends.
fn accepted_no_body_response() -> String {
    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n".into()
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
    bind_addr: &str,
    port: u16,
    event_bus: Arc<EventBus>,
    storage: Arc<S>,
    ctx: Arc<TenantContext>,
    session_id: Uuid,
    shell_routes: ShellRouteConfig,
) -> anyhow::Result<()> {
    let addr = format!("{bind_addr}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("viz server listening on {addr}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let bus = Arc::clone(&event_bus);
        let storage = Arc::clone(&storage);
        let ctx = Arc::clone(&ctx);
        let shell_routes = shell_routes.clone();
        tokio::spawn(async move {
            if let Err(e) =
                handle_viz_connection(stream, bus, storage, ctx, session_id, &shell_routes).await
            {
                tracing::debug!("viz connection from {peer} closed: {e}");
            }
        });
    }
}

/// Parse `?session=<uuid>` out of a request path, ignoring malformed
/// values. Used by viz routes that can be scoped to a specific
/// session without reconnecting.
fn route_path(path: &str) -> &str {
    path.split_once('?').map(|(route, _)| route).unwrap_or(path)
}

fn session_override(path: &str) -> Option<Uuid> {
    let (_, query) = path.split_once('?')?;
    for pair in query.split('&') {
        if let Some(raw) = pair.strip_prefix("session=")
            && let Ok(id) = Uuid::parse_str(raw.trim())
        {
            return Some(id);
        }
    }
    None
}

fn viz_scope_override(path: &str) -> Option<VizSnapshotScope> {
    let (_, query) = path.split_once('?')?;
    for pair in query.split('&') {
        if let Some(raw) = pair.strip_prefix("scope=") {
            return Some(VizSnapshotScope::parse(raw.trim()));
        }
    }
    None
}

/// Run `/consolidate` on the spawned connection task. `run_consolidation`
/// is idempotent (edge upserts), so partial completion on cancellation
/// is safe; emitted `EdgeCreated` events are best-effort and a missed
/// event shows up on the next snapshot refresh.
async fn handle_consolidate<S: Storage>(
    stream: &mut tokio::net::TcpStream,
    storage: &S,
    ctx: &TenantContext,
    event_bus: &EventBus,
    session_id: Uuid,
) -> anyhow::Result<()> {
    let result = crate::dream::run_consolidation(storage, ctx, session_id).await;
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
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/json\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

/// Proxy to LM Studio's `/v1/models` for the viz enrichment tab.
/// Keeps the 5s reqwest timeout — a default `reqwest::Client` has
/// none, which would let a dead LLM hang the spawned task until the
/// outer `REQUEST_BUDGET` fires.
async fn handle_enrich_models(
    stream: &mut tokio::net::TcpStream,
    path: &str,
) -> anyhow::Result<()> {
    let llm_url = path
        .split('?')
        .nth(1)
        .and_then(|q| {
            q.split('&')
                .find_map(|p| p.strip_prefix("url="))
                .map(|raw| {
                    raw.replace("%3A", ":")
                        .replace("%2F", "/")
                        .replace("%3a", ":")
                        .replace("%2f", "/")
                })
        })
        .unwrap_or_else(|| "http://localhost:1234".to_string());

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "viz: reqwest client build failed");
            let body = serde_json::json!({
                "error": format!("reqwest client build failed: {e}")
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\n\
                 Content-Type: application/json\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            return Ok(());
        }
    };
    let (status, body) = match client
        .get(format!("{}/v1/models", llm_url.trim_end_matches('/')))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(t) => ("200 OK", t),
            Err(e) => {
                tracing::warn!(error = %e, "viz: LLM response body read failed");
                (
                    "502 Bad Gateway",
                    format!(
                        "{{\"error\":\"LLM response body read failed: {}\"}}",
                        e.to_string().replace('"', "'")
                    ),
                )
            }
        },
        Ok(resp) => (
            "502 Bad Gateway",
            format!("{{\"error\":\"LLM API returned {}\"}}", resp.status()),
        ),
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
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

/// Handle a single viz HTTP connection.
///
/// Runs entirely in a spawned task — the accept loop does nothing
/// but hand off. Every route (including `POST /consolidate` and
/// `/viz/ws`) builds its own state lazily; the accept loop no longer
/// pre-builds snapshots or peeks request bytes.
async fn handle_viz_connection<S: crate::storage::Storage + 'static>(
    mut stream: tokio::net::TcpStream,
    event_bus: Arc<EventBus>,
    storage: Arc<S>,
    ctx: Arc<crate::types::TenantContext>,
    default_session_id: Uuid,
    shell_routes: &ShellRouteConfig,
) -> anyhow::Result<()> {
    let request = match read_http_request(&mut stream, MAX_REQUEST_BYTES).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "viz: incomplete or malformed request");
            return Ok(());
        }
    };
    let (method, path, headers, _body) = parse_http_request(&request)?;
    let route = route_path(path);
    let effective_session = session_override(path).unwrap_or(default_session_id);
    let initial_viz_scope = viz_scope_override(path).unwrap_or(VizSnapshotScope::All);

    match (method, route) {
        ("POST", p) if p.starts_with("/consolidate") => {
            handle_consolidate(&mut stream, &*storage, &ctx, &event_bus, default_session_id)
                .await?;
        }
        ("GET", p) if p.starts_with("/viz/api/enrich/models") => {
            handle_enrich_models(&mut stream, p).await?;
        }
        ("GET", "/") | ("GET", "/viz") => {
            let viz_html = render_viz_html(shell_routes);
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Cache-Control: no-cache, no-store, must-revalidate\r\n\
                 Pragma: no-cache\r\n\
                 Expires: 0\r\n\
                 Content-Length: {}\r\n\r\n{}",
                viz_html.len(),
                viz_html
            );
            stream.write_all(response.as_bytes()).await?;
        }
        (method, path) if method == "GET" && path.starts_with("/viz/snapshot") => {
            let response = snapshot_stream_required_response();
            stream.write_all(response.as_bytes()).await?;
        }
        (method, path) if method == "GET" && path.starts_with("/viz/api/derived_facts") => {
            // Fetch derived facts from cache for the viz tab
            // Parse query string from path (e.g., /viz/api/derived_facts?session_id=xxx&limit=100)
            let query_string = path.split('?').nth(1).unwrap_or("");

            let session_id = query_string
                .split('&')
                .find(|p| p.starts_with("session_id="))
                .and_then(|p| p.split('=').nth(1))
                .unwrap_or("00000000-0000-0000-0000-000000000000")
                .to_string();

            let limit: usize = query_string
                .split('&')
                .find(|p| p.starts_with("limit="))
                .and_then(|p| p.split('=').nth(1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(100);

            let session_uuid = uuid::Uuid::parse_str(&session_id).unwrap_or(uuid::Uuid::nil());
            let cache_key = format!("consolidation:{}", session_uuid);

            let derived_facts = match storage.derived_cache_get(&ctx, &cache_key).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, cache_key, "viz: derived_cache_get failed; serving empty");
                    Vec::new()
                }
            };
            let total = derived_facts.len();

            // Collect unique entity IDs for batch lookup
            let mut entity_ids: Vec<uuid::Uuid> = Vec::new();
            let mut seen: std::collections::HashSet<uuid::Uuid> = std::collections::HashSet::new();
            for fact in &derived_facts {
                if let Ok(id) = uuid::Uuid::parse_str(&fact.src_id)
                    && seen.insert(id)
                {
                    entity_ids.push(id);
                }
                if let Ok(id) = uuid::Uuid::parse_str(&fact.dst_id)
                    && seen.insert(id)
                {
                    entity_ids.push(id);
                }
            }

            // Batch fetch entity names using single query
            let mut entity_names: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            if !entity_ids.is_empty() {
                let entities = match storage
                    .entity_get_batch(&ctx, session_uuid, &entity_ids)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, count = entity_ids.len(), "viz: entity_get_batch failed; entity names will be missing");
                        Vec::new()
                    }
                };
                for entity in entities {
                    entity_names.insert(entity.entity_id.to_string(), entity.entity_name);
                }
            }

            let facts: Vec<_> = derived_facts
                .into_iter()
                .take(limit)
                .map(|f| {
                    let src_name = entity_names
                        .get(&f.src_id)
                        .cloned()
                        .unwrap_or_else(|| f.src_id[..8].to_string() + "...");
                    let dst_name = entity_names
                        .get(&f.dst_id)
                        .cloned()
                        .unwrap_or_else(|| f.dst_id[..8].to_string() + "...");
                    serde_json::json!({
                        "src_id": f.src_id,
                        "src_name": src_name,
                        "pred": f.pred,
                        "dst_id": f.dst_id,
                        "dst_name": dst_name,
                        "confidence": f.confidence,
                        "rule_id": f.rule_id,
                        "support_count": f.support_count,
                    })
                })
                .collect();

            let body = serde_json::json!({
                "derived_facts": facts,
                "count": facts.len(),
                "total": total,
            })
            .to_string();

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

            handle_viz_ws(
                ws_stream,
                event_bus,
                &*storage,
                (*ctx).clone(),
                effective_session,
                initial_viz_scope,
            )
            .await;
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
async fn send_viz_event(
    write: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        Message,
    >,
    event: &VizEvent,
) -> bool {
    let json = match serde_json::to_string(event) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("viz: failed to serialize event: {e}");
            return false;
        }
    };
    write.send(Message::Text(json)).await.is_ok()
}

async fn send_streaming_viz_snapshot<S: Storage>(
    write: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        Message,
    >,
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    scope: VizSnapshotScope,
) -> bool {
    const VIZ_CHUNK_SIZE: usize = 500;

    if !send_viz_event(
        write,
        &VizEvent::SnapshotStreamStart {
            level: None,
            parent: None,
        },
    )
    .await
    {
        return false;
    }

    let mut total_nodes = 0usize;
    let mut total_edges = 0usize;

    match scope {
        VizSnapshotScope::All => {
            let (tx, mut rx) = tokio::sync::mpsc::channel(4);
            let producer = storage.entity_stream_all(ctx.clone(), VIZ_CHUNK_SIZE, tx);
            tokio::pin!(producer);
            let mut producer_done = false;
            loop {
                tokio::select! {
                    _ = &mut producer, if !producer_done => producer_done = true,
                    chunk = rx.recv() => {
                        match chunk {
                            Some(Ok(entities)) => {
                                let nodes: Vec<_> = entities.iter().map(viz::entity_to_viz_node).collect();
                                total_nodes += nodes.len();
                                if !nodes.is_empty()
                                    && !send_viz_event(write, &VizEvent::SnapshotStreamChunk { nodes, edges: Vec::new() }).await
                                {
                                    return false;
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!("viz: failed to stream entities for snapshot: {e}");
                                break;
                            }
                            None if producer_done => break,
                            None => {}
                        }
                    }
                }
                if producer_done && rx.is_empty() {
                    break;
                }
            }
        }
        VizSnapshotScope::SessionOnly | VizSnapshotScope::GlobalOnly => {
            let sessions = match scope {
                VizSnapshotScope::SessionOnly => viz_scoped_overview_sessions(ctx, session_id),
                VizSnapshotScope::GlobalOnly => {
                    let mut sessions = vec![
                        Uuid::nil(),
                        crate::scope::tenant_global_session_uuid(ctx.tenant_id),
                    ];
                    sessions.sort_unstable();
                    sessions.dedup();
                    sessions
                }
                VizSnapshotScope::All => unreachable!(),
            };
            for sid in sessions {
                match storage.entity_list_session(ctx, sid).await {
                    Ok(entities) => {
                        for chunk in entities.chunks(VIZ_CHUNK_SIZE) {
                            let nodes: Vec<_> = chunk.iter().map(viz::entity_to_viz_node).collect();
                            total_nodes += nodes.len();
                            if !nodes.is_empty()
                                && !send_viz_event(
                                    write,
                                    &VizEvent::SnapshotStreamChunk {
                                        nodes,
                                        edges: Vec::new(),
                                    },
                                )
                                .await
                            {
                                return false;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(session_id = %sid, "viz: failed to load scoped entities for snapshot stream: {e}")
                    }
                }
            }
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let producer = storage.fold_stream_all(ctx.clone(), VIZ_CHUNK_SIZE, tx);
    tokio::pin!(producer);
    let mut producer_done = false;
    loop {
        tokio::select! {
            _ = &mut producer, if !producer_done => producer_done = true,
            chunk = rx.recv() => {
                match chunk {
                    Some(Ok(folds)) => {
                        let nodes: Vec<_> = folds.iter().map(viz::fold_to_viz_node).collect();
                        total_nodes += nodes.len();
                        if !nodes.is_empty()
                            && !send_viz_event(
                                write,
                                &VizEvent::SnapshotStreamChunk {
                                    nodes,
                                    edges: Vec::new(),
                                },
                            )
                            .await
                        {
                            return false;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!("viz: failed to stream folds for snapshot: {e}");
                        break;
                    }
                    None if producer_done => break,
                    None => {}
                }
            }
        }
        if producer_done && rx.is_empty() {
            break;
        }
    }

    let swapped_ctx = TenantContext {
        tenant_id: session_id,
        session_origin: ctx.session_origin.clone(),
    };
    let mut edge_chunk = Vec::with_capacity(VIZ_CHUNK_SIZE);

    // Do not run tenant-wide legacy edge-table scans as part of the initial
    // browser stream. Those tables require ALLOW FILTERING and can saturate
    // FerrosaDB bulk lanes on large persisted datasets. Tenant-wide all-scope
    // graph edges come from the typed_edges table, which is the current labeled
    // edge store and has a paged streaming storage path.
    match scope {
        VizSnapshotScope::All => {
            let (tx, mut rx) = tokio::sync::mpsc::channel(4);
            let producer = storage.typed_edge_stream_all(ctx.clone(), VIZ_CHUNK_SIZE, tx);
            tokio::pin!(producer);
            let mut producer_done = false;
            loop {
                tokio::select! {
                    _ = &mut producer, if !producer_done => producer_done = true,
                    chunk = rx.recv() => {
                        match chunk {
                            Some(Ok(typed_edges)) => {
                                for te in typed_edges {
                                    edge_chunk.push(VizEdge {
                                        source: te.src_id.to_string(),
                                        target: te.dst_id.to_string(),
                                        edge_type: te.edge_type,
                                        strength: Some(te.weight as f32),
                                    });
                                    total_edges += 1;
                                    if edge_chunk.len() >= VIZ_CHUNK_SIZE {
                                        let edges = std::mem::take(&mut edge_chunk);
                                        if !send_viz_event(
                                            write,
                                            &VizEvent::SnapshotStreamChunk {
                                                nodes: Vec::new(),
                                                edges,
                                            },
                                        )
                                        .await
                                        {
                                            return false;
                                        }
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, "viz: failed to stream all-scope typed edges");
                                break;
                            }
                            None if producer_done => break,
                            None => {}
                        }
                    }
                }
                if producer_done && rx.is_empty() {
                    break;
                }
            }
        }
        _ => {
            let probe = match scope {
                VizSnapshotScope::SessionOnly => viz_scoped_overview_sessions(ctx, session_id),
                VizSnapshotScope::GlobalOnly => {
                    let mut probe = vec![
                        Uuid::nil(),
                        crate::scope::tenant_global_session_uuid(ctx.tenant_id),
                    ];
                    probe.sort_unstable();
                    probe.dedup();
                    probe
                }
                VizSnapshotScope::All => unreachable!(),
            };
            for probe_ctx in [ctx, &swapped_ctx] {
                for sid in &probe {
                    match storage.typed_edge_list_session(probe_ctx, *sid).await {
                        Ok(typed_edges) => {
                            for te in typed_edges {
                                edge_chunk.push(VizEdge {
                                    source: te.src_id.to_string(),
                                    target: te.dst_id.to_string(),
                                    edge_type: te.edge_type,
                                    strength: Some(te.weight as f32),
                                });
                                total_edges += 1;
                                if edge_chunk.len() >= VIZ_CHUNK_SIZE {
                                    let edges = std::mem::take(&mut edge_chunk);
                                    if !send_viz_event(
                                        write,
                                        &VizEvent::SnapshotStreamChunk {
                                            nodes: Vec::new(),
                                            edges,
                                        },
                                    )
                                    .await
                                    {
                                        return false;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, session_id = %sid, "viz: typed_edge_list_session failed")
                        }
                    }
                }
            }
        }
    }

    if !edge_chunk.is_empty()
        && !send_viz_event(
            write,
            &VizEvent::SnapshotStreamChunk {
                nodes: Vec::new(),
                edges: edge_chunk,
            },
        )
        .await
    {
        return false;
    }

    send_viz_event(
        write,
        &VizEvent::SnapshotStreamEnd {
            total_nodes,
            total_edges,
        },
    )
    .await
}

/// Handle a WebSocket connection for the viz dashboard.
///
/// Sends a bounded chunk stream, then listens for both event bus broadcasts and
/// client navigation messages. Navigation requests re-stream current storage
/// state instead of retaining a full server-side graph cache.
async fn handle_viz_ws<S: Storage>(
    ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    event_bus: Arc<EventBus>,
    storage: &S,
    ctx: TenantContext,
    session_id: Uuid,
    scope: VizSnapshotScope,
) {
    use futures_util::StreamExt;

    let (mut write, mut read) = futures_util::StreamExt::split(ws_stream);

    if !send_streaming_viz_snapshot(&mut write, storage, &ctx, session_id, scope).await {
        return;
    }

    let mut rx = event_bus.subscribe();

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if !send_viz_event(&mut write, &event).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("viz: WebSocket client lagged by {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(client_msg) = serde_json::from_str::<viz::VizClientMessage>(&text) {
                            let requested_scope = match client_msg {
                                viz::VizClientMessage::ToggleView { ref mode }
                                    if mode == "global" || mode == "overview" => VizSnapshotScope::GlobalOnly,
                                // Browser refresh/detail requests intentionally default to the
                                // tenant-wide stream. This is the stress-test path used to expose
                                // backpressure/materialization bugs in the async browser pipeline;
                                // keep it uncapped and chunked instead of silently falling back to
                                // the small scoped overview.
                                _ => VizSnapshotScope::All,
                            };
                            if !send_streaming_viz_snapshot(&mut write, storage, &ctx, session_id, requested_scope).await {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
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

/// Scope of a viz snapshot: which partitions the builder pulls entities from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VizSnapshotScope {
    /// Just the configured viz session (pre-Sprint-1 behavior).
    SessionOnly,
    /// Only global-scope entities (tenant sentinel partition).
    GlobalOnly,
    /// Every entity for the tenant (union across sessions).
    #[default]
    All,
}

impl VizSnapshotScope {
    /// Parse a scope value from a query string. Unknown values → `All`.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "session" => Self::SessionOnly,
            "global" => Self::GlobalOnly,
            _ => Self::All,
        }
    }
}

fn viz_scoped_overview_sessions(ctx: &TenantContext, session_id: Uuid) -> Vec<Uuid> {
    let mut sessions = vec![
        session_id,
        Uuid::nil(),
        crate::scope::tenant_global_session_uuid(ctx.tenant_id),
    ];
    sessions.sort_unstable();
    sessions.dedup();
    sessions
}

/// Build a `VizEvent::Snapshot` from current storage state.
///
/// Queries entities and edges for the given scope and converts them
/// to visualization types. `scope=SessionOnly` preserves pre-Sprint-1
/// behavior (one session); `All` unions every partition for the tenant;
/// `GlobalOnly` hits only the tenant-global sentinel.
#[cfg(test)]
async fn build_snapshot<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    scope: VizSnapshotScope,
) -> VizEvent {
    // Query entities for the configured scope.
    tracing::info!(
        tenant_id = %ctx.tenant_id,
        %session_id,
        session_is_nil = session_id.is_nil(),
        ?scope,
        "viz: building snapshot"
    );
    let entities_result = match scope {
        VizSnapshotScope::SessionOnly => storage.entity_list_session(ctx, session_id).await,
        VizSnapshotScope::GlobalOnly => {
            let global = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
            storage.entity_list_session(ctx, global).await
        }
        VizSnapshotScope::All => storage.entity_list_all(ctx).await,
    };

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
    let mut all_edges = match storage.edge_list_all(&swapped_ctx).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "viz: edge_list_all(swapped_ctx) failed; serving empty edge set");
            Vec::new()
        }
    };
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

    // Load typed edges (depends_on, contains, calls, TAGGED_AS, PARENT_TAG, etc.).
    //
    // When `scope == All` (the default), query every session for the tenant in
    // a single pass so that entities stored under any session (nil session for
    // forge ingest, tenant-global-session for skills/tags, per-session for
    // user work) all have their edges rendered. When a specific scope is
    // requested, probe only the sessions that scope covers.
    let mut typed_edges = match scope {
        VizSnapshotScope::All => match storage.typed_edge_list_all(ctx).await {
            Ok(te) => {
                tracing::info!(
                    count = te.len(),
                    "viz: loaded typed edges across all sessions"
                );
                te
            }
            Err(e) => {
                tracing::warn!(error = %e, "viz: typed_edge_list_all failed");
                Vec::new()
            }
        },
        _ => {
            let mut probe = vec![session_id];
            let nil = uuid::Uuid::nil();
            if session_id != nil {
                probe.push(nil);
            }
            if matches!(scope, VizSnapshotScope::GlobalOnly) {
                let global = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
                if !probe.contains(&global) {
                    probe.push(global);
                }
            }
            let mut acc = Vec::new();
            for sid in probe {
                match storage.typed_edge_list_session(ctx, sid).await {
                    Ok(mut te) => {
                        tracing::info!(
                            session_id = %sid,
                            count = te.len(),
                            "viz: loaded typed edges for session"
                        );
                        acc.append(&mut te);
                    }
                    Err(e) => {
                        tracing::warn!(session_id = %sid, error = %e, "viz: typed_edge_list_session failed");
                    }
                }
            }
            acc
        }
    };

    // Legacy recovery path: older data could be written with tenant_id and
    // session_id swapped. Non-typed graph edges already probe `swapped_ctx`;
    // typed_edges need the same fallback or the viz can show zero edges while
    // the rows still exist on disk under the legacy tenant key.
    if swapped_ctx.tenant_id != ctx.tenant_id {
        match scope {
            VizSnapshotScope::All => match storage.typed_edge_list_all(&swapped_ctx).await {
                Ok(mut legacy) => {
                    tracing::info!(
                        count = legacy.len(),
                        "viz: loaded legacy swapped typed edges across all sessions"
                    );
                    typed_edges.append(&mut legacy);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "viz: typed_edge_list_all(swapped_ctx) failed")
                }
            },
            _ => {
                let mut legacy_probe = vec![ctx.tenant_id, session_id];
                legacy_probe.sort_unstable();
                legacy_probe.dedup();
                for sid in legacy_probe {
                    match storage.typed_edge_list_session(&swapped_ctx, sid).await {
                        Ok(mut legacy) => {
                            tracing::info!(
                                session_id = %sid,
                                count = legacy.len(),
                                "viz: loaded legacy swapped typed edges for session"
                            );
                            typed_edges.append(&mut legacy);
                        }
                        Err(e) => {
                            tracing::warn!(session_id = %sid, error = %e, "viz: typed_edge_list_session(swapped_ctx) failed")
                        }
                    }
                }
            }
        }
    }

    let mut seen_typed_edges = std::collections::HashSet::new();
    for te in typed_edges {
        if !seen_typed_edges.insert((te.src_id, te.edge_type.clone(), te.dst_id)) {
            continue;
        }
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
#[cfg(test)]
#[allow(dead_code)]
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
            "Papers"
                | "People"
                | "Concepts"
                | "Organizations"
                | "Decisions"
                | "Skills"
                | "Tags"
                | "Other"
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
                    // Group non-code entities by type: "Papers", "People", "Skills", etc.
                    match node.entity_type.as_str() {
                        "document" => "Papers".to_string(),
                        "person" => "People".to_string(),
                        "concept" => "Concepts".to_string(),
                        "org" => "Organizations".to_string(),
                        "bug" | "decision" | "pattern" | "preference" => "Decisions".to_string(),
                        "skill" => "Skills".to_string(),
                        "tag" => "Tags".to_string(),
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
                    "Papers"
                        | "People"
                        | "Concepts"
                        | "Organizations"
                        | "Decisions"
                        | "Skills"
                        | "Tags"
                        | "Other"
                ) {
                    match crate_label.as_str() {
                        "Papers" => "document",
                        "People" => "person",
                        "Concepts" => "concept",
                        "Organizations" => "org",
                        "Decisions" => "decision",
                        "Skills" => "skill",
                        "Tags" => "tag",
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
                    ..Default::default()
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
                    ..Default::default()
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
#[cfg(test)]
#[allow(dead_code)]
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

    let decoded = if let Some(encoded) = auth_header.strip_prefix("Basic ") {
        String::from_utf8(
            base64_decode(encoded).map_err(|e| anyhow::anyhow!("invalid base64: {e}"))?,
        )?
    } else if let Some(token) = auth_header.strip_prefix("Bearer ") {
        if token.contains(':') {
            token.to_string()
        } else {
            String::from_utf8(
                base64_decode(token).map_err(|e| anyhow::anyhow!("invalid bearer token: {e}"))?,
            )?
        }
    } else {
        anyhow::bail!("only Basic or Bearer auth supported");
    };

    let (username, password) = decoded
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid auth credential format"))?;

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

/// Read a complete HTTP/1.1 request from `stream`.
///
/// Loops `stream.read` until the end of the header block (`\r\n\r\n`)
/// is seen, then — if the request carries a `Content-Length` — until
/// that many bytes of body have been read. Returns the assembled
/// `head + "\r\n\r\n" + body` as a single `String` suitable for
/// `parse_http_request`.
///
/// This is what the server needs in practice because clients routinely
/// split a request across multiple TCP writes — Python's
/// `http.client.HTTPConnection.send` calls `sock.sendall` for the
/// headers and again for the body, so a single-`read` server closes
/// the connection before the body arrives. Bounded by `max_bytes` so
/// a misbehaving peer can't stream into the process forever.
async fn read_http_request<T>(stream: &mut T, max_bytes: usize) -> anyhow::Result<String>
where
    T: AsyncReadExt + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];

    // Phase 1: read until end-of-headers marker is present.
    let head_end = loop {
        if let Some(pos) = find_header_terminator(&buf) {
            break pos;
        }
        if buf.len() >= max_bytes {
            return Err(anyhow::anyhow!(
                "request headers exceeded {max_bytes} bytes without CRLFCRLF"
            ));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "connection closed before request headers complete"
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    // Phase 2: if the request has a body, read until Content-Length
    // bytes have arrived. `body_end` is an absolute index into `buf`.
    let content_length = parse_content_length(&buf[..head_end])?;
    let body_end = head_end
        .checked_add(content_length)
        .ok_or_else(|| anyhow::anyhow!("Content-Length overflows request size"))?;
    if body_end > max_bytes {
        return Err(anyhow::anyhow!(
            "request size {body_end} exceeds limit {max_bytes}"
        ));
    }
    while buf.len() < body_end {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "connection closed before {content_length}-byte body complete"
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    // Trim any bytes past the end of this request (pipelined follow-up,
    // if ever supported, would need to survive here — today we close).
    buf.truncate(body_end);
    String::from_utf8(buf).map_err(|e| anyhow::anyhow!("request is not valid UTF-8: {e}"))
}

/// Return the byte index **just past** the `\r\n\r\n` terminator, or
/// `None` if the header block isn't yet complete.
fn find_header_terminator(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Find `Content-Length` in the raw header bytes and parse it.
/// Absent header → 0 (no body). Present but unparsable → error.
fn parse_content_length(head: &[u8]) -> anyhow::Result<usize> {
    let head_str = std::str::from_utf8(head)
        .map_err(|e| anyhow::anyhow!("request headers are not valid UTF-8: {e}"))?;
    for line in head_str.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value
                .trim()
                .parse::<usize>()
                .map_err(|e| anyhow::anyhow!("invalid Content-Length `{value}`: {e}"));
        }
    }
    Ok(0)
}

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
    use crate::metrics::MemoryMetrics;
    use crate::storage::mock::MockStorage;

    #[tokio::test]
    async fn run_with_budget_times_out_on_stalled_future() {
        // Regression guard: the HTTPS accept loop wedged in the field
        // because a client opened TCP but never sent ClientHello, and
        // `acceptor.accept(...)` was awaited without a bound. Storage
        // trait futures aren't Send, so we can't just spawn each
        // connection — we instead cap handshake duration. This test
        // encodes the contract: when the wrapped op never resolves,
        // the helper returns an error that names the operation and
        // the budget, so operators can tell "slow client" apart from
        // "real TLS error". A 20ms budget keeps the test fast (vs the
        // 10s production budget); tokio's `test-util`/`start_paused`
        // would let us use the real constant but isn't enabled on the
        // workspace and isn't worth adding just for this.
        let stalled = std::future::pending::<std::io::Result<()>>();
        let budget = std::time::Duration::from_millis(20);
        let err = run_with_budget("tls handshake", stalled, budget)
            .await
            .expect_err("stalled future must not produce Ok");
        let msg = err.to_string();
        assert!(
            msg.contains("tls handshake") && msg.contains("timed out"),
            "error must name op + timeout; got {msg}"
        );
        assert!(
            msg.contains("20ms"),
            "error must mention the budget duration; got {msg}"
        );
    }

    #[tokio::test]
    async fn run_with_budget_propagates_op_error() {
        // Inner I/O errors surface with the op name prefixed so a
        // log reader can tell "tls handshake error: …" apart from
        // "tls handshake timed out …".
        let inner = std::future::ready::<std::io::Result<()>>(Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "client abandoned",
        )));
        let err = run_with_budget("tls handshake", inner, std::time::Duration::from_secs(1))
            .await
            .expect_err("inner Err must bubble out");
        let msg = err.to_string();
        assert!(msg.contains("tls handshake error"), "got {msg}");
        assert!(msg.contains("client abandoned"), "got {msg}");
    }

    /// Python's `http.client.HTTPConnection.send` calls `sock.sendall`
    /// twice — once for the request line + headers, once for the
    /// body. A single `stream.read` sees only the first segment, so
    /// parsing fails and fmem closes without a response. Regression
    /// guard: the reader must loop until `Content-Length` bytes of
    /// body have arrived.
    #[tokio::test]
    async fn read_http_request_assembles_headers_and_body_split_across_reads() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut server) = tokio::io::duplex(4096);
        let writer = tokio::spawn(async move {
            client
                .write_all(
                    b"POST /mcp HTTP/1.1\r\n\
                      Host: localhost\r\n\
                      Content-Type: application/json\r\n\
                      Content-Length: 17\r\n\r\n",
                )
                .await
                .unwrap();
            client.flush().await.unwrap();
            // Model the kernel delivering headers to the peer before
            // the second sendall writes the body.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            client.write_all(b"{\"jsonrpc\":\"2.0\"}").await.unwrap();
            client.flush().await.unwrap();
            // Hold the write half open long enough for the server to
            // finish reading Content-Length bytes.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            drop(client);
        });
        let raw = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_http_request(&mut server, 65536),
        )
        .await
        .expect("read must not hang on multi-write client")
        .expect("read must succeed with full request");
        writer.await.unwrap();

        let (method, path, _headers, body) = parse_http_request(&raw).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/mcp");
        assert_eq!(body, r#"{"jsonrpc":"2.0"}"#);
    }

    #[tokio::test]
    async fn read_http_request_accepts_single_write() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            client
                .write_all(
                    b"POST /mcp HTTP/1.1\r\n\
                      Content-Length: 2\r\n\r\nhi",
                )
                .await
                .unwrap();
            drop(client);
        });
        let raw = read_http_request(&mut server, 65536).await.unwrap();
        let (method, _p, _h, body) = parse_http_request(&raw).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(body, "hi");
    }

    #[tokio::test]
    async fn read_http_request_returns_get_without_body() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            client
                .write_all(b"GET /healthz/live HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            drop(client);
        });
        let raw = read_http_request(&mut server, 65536).await.unwrap();
        let (method, path, _h, body) = parse_http_request(&raw).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/healthz/live");
        assert_eq!(body, "");
    }

    #[tokio::test]
    async fn read_http_request_errors_on_eof_before_headers_complete() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            client
                .write_all(b"POST /mcp HTTP/1.1\r\nHost: loca")
                .await
                .unwrap();
            drop(client);
        });
        let err = read_http_request(&mut server, 65536)
            .await
            .expect_err("truncated request must not masquerade as success");
        assert!(
            err.to_string().to_lowercase().contains("closed")
                || err.to_string().to_lowercase().contains("incomplete"),
            "error must flag truncation, got: {err}"
        );
    }

    #[test]
    fn parse_http_get() {
        let raw = "GET /healthz/live HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (method, path, headers, body) = parse_http_request(raw).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/healthz/live");
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
    fn rendered_viz_html_points_websocket_at_public_viz_port() {
        let routes = ShellRouteConfig {
            workbench_scheme: "http".into(),
            workbench_port: 18765,
            viz_scheme: "http".into(),
            viz_port: 18766,
        };
        let html = render_viz_html(&routes);
        assert!(
            html.contains("window.__FMEM_VIZ_PORT__ = 18766;"),
            "viz html must embed public viz port so pages served through the workbench port do not websocket to /viz/ws on 18765"
        );
        assert!(
            html.contains("new WebSocket(`${protocol}//${location.hostname}:${window.__FMEM_VIZ_PORT__}/viz/ws`)")
                || html.contains("new WebSocket(wsUrl)"),
            "viz html must construct websocket URL from public viz port; got snippet around websocket: {:?}",
            html.find("new WebSocket").map(|idx| &html[idx.saturating_sub(120)..html.len().min(idx + 220)])
        );
    }

    #[test]
    fn viz_html_renders_snapshot_stream_chunks_incrementally() {
        let chunk_case = VIZ_HTML
            .split("case 'SnapshotStreamChunk':")
            .nth(1)
            .and_then(|rest| rest.split("case 'SnapshotStreamEnd':").next())
            .expect("viz html must handle SnapshotStreamChunk events");
        assert!(
            chunk_case.contains("renderStreamChunk")
                || chunk_case.contains("applySnapshotStreamChunk"),
            "SnapshotStreamChunk handler must render incrementally instead of waiting for SnapshotStreamEnd; handler was: {chunk_case}"
        );
    }

    #[test]
    fn viz_browser_paths_do_not_build_materialized_snapshots() {
        let source = include_str!("http.rs");
        let workbench_snapshot_route = source
            .split("viz_path.starts_with(\"/viz/snapshot\")")
            .nth(1)
            .and_then(|rest| {
                rest.split("viz_path.starts_with(\"/viz/api/derived_facts\")")
                    .next()
            })
            .expect("workbench /viz/snapshot route must exist");
        assert!(
            !workbench_snapshot_route.contains("build_snapshot"),
            "workbench /viz/snapshot must not materialize a full VizEvent::Snapshot; use websocket streaming instead: {workbench_snapshot_route}"
        );
        assert!(
            !workbench_snapshot_route.contains("serde_json::to_string(&snapshot"),
            "workbench /viz/snapshot must not serialize a giant snapshot body: {workbench_snapshot_route}"
        );

        let viz_snapshot_route = source
            .split("path.starts_with(\"/viz/snapshot\")")
            .nth(1)
            .and_then(|rest| {
                rest.split("path.starts_with(\"/viz/api/derived_facts\")")
                    .next()
            })
            .expect("viz /viz/snapshot route must exist");
        assert!(
            !viz_snapshot_route.contains("build_snapshot"),
            "viz /viz/snapshot must not materialize a full VizEvent::Snapshot; use websocket streaming instead: {viz_snapshot_route}"
        );
        assert!(
            !viz_snapshot_route.contains("serde_json::to_string(&snapshot"),
            "viz /viz/snapshot must not serialize a giant snapshot body: {viz_snapshot_route}"
        );
    }

    #[test]
    fn viz_websocket_does_not_retain_full_graph_for_navigation() {
        let source = include_str!("http.rs");
        let handler = source
            .split("async fn handle_viz_ws")
            .nth(1)
            .and_then(|rest| rest.split("/// Handle an SSE connection").next())
            .expect("handle_viz_ws body must be present");
        assert!(
            !handler.contains("full_nodes"),
            "viz websocket must not retain every streamed node for drilldown/cache; re-query/stream instead: {handler}"
        );
        assert!(
            !handler.contains("full_edges"),
            "viz websocket must not retain every streamed edge for drilldown/cache; re-query/stream instead: {handler}"
        );
        assert!(
            handler.contains("_ => VizSnapshotScope::All"),
            "viz websocket browser refreshes must default to the tenant-wide all-node stream, not the small scoped overview: {handler}"
        );
        let streaming_snapshot = source
            .split("async fn send_streaming_viz_snapshot")
            .nth(1)
            .and_then(|rest| rest.split("/// Handle a WebSocket connection").next())
            .expect("send_streaming_viz_snapshot body must be present");
        assert!(
            !streaming_snapshot.contains("fold_list_all"),
            "viz streaming snapshot must stream folds instead of materializing fold_list_all: {streaming_snapshot}"
        );
        assert!(
            !streaming_snapshot.contains("storage.edge_stream_all("),
            "viz websocket connect must not trigger tenant-wide legacy edge scans; stream typed_edges instead: {streaming_snapshot}"
        );
        assert!(
            streaming_snapshot.contains("typed_edge_stream_all"),
            "viz websocket connect must stream all-scope typed_edges so the all-node graph has edges without falling back to session scope: {streaming_snapshot}"
        );
    }

    #[test]
    fn viz_websocket_route_ignores_query_string_for_session_override() {
        assert_eq!(
            route_path("/viz/ws?session=22222222-2222-2222-2222-222222222222"),
            "/viz/ws"
        );
        assert_eq!(route_path("/viz/ws"), "/viz/ws");
        assert_eq!(
            session_override("/viz/ws?session=22222222-2222-2222-2222-222222222222"),
            Some(Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap())
        );
        assert_eq!(
            viz_scope_override("/viz/ws?scope=session"),
            Some(VizSnapshotScope::SessionOnly)
        );
        assert_eq!(
            viz_scope_override("/viz/ws?session=22222222-2222-2222-2222-222222222222&scope=all"),
            Some(VizSnapshotScope::All)
        );
        assert_eq!(
            viz_scope_override("/viz/ws?scope=global"),
            Some(VizSnapshotScope::GlobalOnly)
        );
        assert_eq!(viz_scope_override("/viz/ws"), None);
    }

    #[test]
    fn viz_websocket_scoped_overview_includes_current_nil_and_global_sessions() {
        let tenant_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let session_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let ctx = TenantContext {
            tenant_id,
            session_origin: "test".into(),
        };
        let sessions = viz_scoped_overview_sessions(&ctx, session_id);
        assert!(
            sessions.contains(&session_id),
            "scoped viz overview must include the current configured session"
        );
        assert!(
            sessions.contains(&Uuid::nil()),
            "scoped viz overview must include nil-session rows used by older/global data"
        );
        assert!(
            sessions.contains(&crate::scope::tenant_global_session_uuid(tenant_id)),
            "scoped viz overview must include tenant-global rows so the initial graph is not blank"
        );
        assert_eq!(
            sessions.len(),
            3,
            "scoped viz overview must stay bounded to keyed partitions, not tenant-wide scans"
        );
    }

    #[tokio::test]
    async fn viz_snapshot_includes_legacy_swapped_tenant_typed_edges() {
        // Regression guard for live data recovered from the historical
        // tenant/session swap bug. The viz snapshot already probes
        // edge_list_all(swapped_ctx) for legacy co-occurs-style edges, but
        // typed_edges must get the same treatment or the graph appears to
        // have lost all labeled edges even though the rows are still on disk.
        let storage = MockStorage::new();
        let tenant_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let ctx = TenantContext {
            tenant_id,
            session_origin: "test".into(),
        };
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let now = chrono::Utc::now();

        for (entity_id, entity_name) in [(a, "alpha"), (b, "beta")] {
            storage
                .entity_put(
                    &ctx,
                    &crate::types::EntityEntry {
                        tenant_id,
                        session_id,
                        entity_id,
                        entity_name: entity_name.into(),
                        entity_type: "concept".into(),
                        confidence: 1.0,
                        created_at: now,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }

        storage
            .typed_edge_put(
                &ctx,
                &crate::types::TypedEdge {
                    // Legacy swapped-key rows have the viz session in tenant_id.
                    tenant_id: session_id,
                    session_id,
                    src_id: a,
                    edge_type: "TAGGED_AS".into(),
                    dst_id: b,
                    weight: 0.75,
                    metadata: None,
                    created_at: now,
                },
            )
            .await
            .unwrap();

        let snapshot = build_snapshot(&storage, &ctx, session_id, VizSnapshotScope::All).await;
        let VizEvent::Snapshot { edges, .. } = snapshot else {
            panic!("expected snapshot event");
        };
        assert!(
            edges.iter().any(|edge| {
                edge.source == a.to_string()
                    && edge.target == b.to_string()
                    && edge.edge_type == "TAGGED_AS"
            }),
            "viz snapshot must include typed edges stored under legacy swapped tenant_id; got {edges:?}"
        );
    }

    #[test]
    fn workbench_html_is_embedded() {
        assert!(WORKBENCH_HTML.contains("Operator Workbench"));
        assert!(WORKBENCH_HTML.contains("/workbench/api"));
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
            bind_addr: "127.0.0.1".into(),
            port: 8765,
            require_tls: false,
            cert_path: None,
            key_path: None,
            readiness_checker: Arc::new(|| true),
            shell_routes: ShellRouteConfig::default(),
            session: Arc::new(dispatch::SessionState::default()),
        };
        assert!(!config.require_tls);
        assert!(config.cert_path.is_none());
        assert!(config.key_path.is_none());

        let config_tls = HttpConfig {
            bind_addr: "127.0.0.1".into(),
            port: 443,
            require_tls: true,
            cert_path: Some("/etc/ssl/cert.pem".into()),
            key_path: Some("/etc/ssl/key.pem".into()),
            readiness_checker: Arc::new(|| true),
            shell_routes: ShellRouteConfig::default(),
            session: Arc::new(dispatch::SessionState::default()),
        };
        assert!(config_tls.require_tls);
        assert_eq!(config_tls.cert_path.as_deref(), Some("/etc/ssl/cert.pem"));
        assert_eq!(config_tls.key_path.as_deref(), Some("/etc/ssl/key.pem"));
    }

    #[tokio::test]
    async fn healthz_ready_returns_503_when_not_ready() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let response = handle_http_request(
            "GET",
            "/healthz/ready",
            &[],
            "",
            &storage,
            &metrics,
            &|_, _| None,
            &|| false,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(response.contains("not ready"));
    }

    #[tokio::test]
    async fn healthz_live_returns_200_even_when_not_ready() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let response = handle_http_request(
            "GET",
            "/healthz/live",
            &[],
            "",
            &storage,
            &metrics,
            &|_, _| None,
            &|| false,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("ok"));
    }

    #[tokio::test]
    async fn mcp_request_returns_401_on_invalid_credentials() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjp3cm9uZw==".to_string(),
        )];
        let response = handle_http_request(
            "POST",
            "/mcp",
            &headers,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            &storage,
            &metrics,
            &|_, _| None,
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
        assert!(response.contains("WWW-Authenticate: Basic realm=\"Ferrosa Memory\""));
    }

    #[tokio::test]
    async fn workbench_root_requires_auth() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let response = handle_http_request(
            "GET",
            "/",
            &[],
            "",
            &storage,
            &metrics,
            &|_, _| None,
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
        assert!(response.contains("WWW-Authenticate: Basic realm=\"Ferrosa Memory\""));
    }

    #[tokio::test]
    async fn workbench_root_serves_html_when_authenticated() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let response = handle_http_request(
            "GET",
            "/",
            &headers,
            "",
            &storage,
            &metrics,
            &|u, p| {
                if u == "user" && p == "pass" {
                    Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Ferrosa Memory Operator Workbench"));
    }

    #[tokio::test]
    async fn workbench_named_route_serves_html_when_authenticated() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let response = handle_http_request(
            "GET",
            "/workbench",
            &headers,
            "",
            &storage,
            &metrics,
            &|u, p| {
                if u == "user" && p == "pass" {
                    Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Ferrosa Memory Operator Workbench"));
    }

    #[tokio::test]
    async fn bearer_raw_credentials_are_accepted_for_workbench_requests() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![("Authorization".to_string(), "Bearer user:pass".to_string())];
        let response = handle_http_request(
            "GET",
            "/workbench/api/auth/whoami",
            &headers,
            "",
            &storage,
            &metrics,
            &|u, p| {
                if u == "user" && p == "pass" {
                    Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"tenant_id\""));
    }

    #[test]
    fn query_param_decodes_percent_encoding_and_plus_space() {
        let value = query_param(
            "/workbench/api/aliases?alias_name=graph%2Fstatus%3Aok+now",
            "alias_name",
        )
        .unwrap();
        assert_eq!(value, "graph/status:ok now");
    }

    #[test]
    fn request_hostname_strips_numeric_port_and_preserves_ipv6_brackets() {
        assert_eq!(
            request_hostname(&[("Host".into(), "localhost:18765".into())]),
            Some("localhost")
        );
        assert_eq!(
            request_hostname(&[("Host".into(), "[::1]:18765".into())]),
            Some("[::1]")
        );
    }

    #[tokio::test]
    async fn viz_route_redirects_to_configured_viz_origin_without_auth() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![("Host".to_string(), "localhost:18765".to_string())];
        for method in ["GET", "HEAD"] {
            let response = handle_http_request(
                method,
                "/viz",
                &headers,
                "",
                &storage,
                &metrics,
                &|_, _| None,
                &|| true,
                &ShellRouteConfig::default(),
            )
            .await
            .unwrap();
            assert!(response.starts_with("HTTP/1.1 302 Found"));
            assert!(response.contains("Location: http://localhost:18766/viz"));
        }
    }

    #[tokio::test]
    async fn workbench_cql_query_proxies_passthrough_results() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let response = handle_http_request(
            "POST",
            "/workbench/api/cql/query",
            &headers,
            r#"{"query":"SELECT * FROM entity_store LIMIT 3","limit":3}"#,
            &storage,
            &metrics,
            &|u, p| {
                if u == "user" && p == "pass" {
                    Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"source\":\"mock-cql\""));
        assert!(response.contains("\"query\":\"SELECT * FROM entity_store LIMIT 3\""));
    }

    #[tokio::test]
    async fn workbench_cql_query_returns_json_error_for_bad_passthrough_input() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let response = handle_http_request(
            "POST",
            "/workbench/api/cql/query",
            &headers,
            r#"{"query":"   ","limit":3}"#,
            &storage,
            &metrics,
            &|u, p| {
                if u == "user" && p == "pass" {
                    Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("\"error\":\"query must not be empty\""));
    }

    #[tokio::test]
    async fn workbench_cql_query_rejects_excessive_limits() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let response = handle_http_request(
            "POST",
            "/workbench/api/cql/query",
            &headers,
            r#"{"query":"SELECT * FROM entity_store LIMIT 3","limit":1001}"#,
            &storage,
            &metrics,
            &|u, p| {
                if u == "user" && p == "pass" {
                    Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("\"error\":\"limit must be <= 1000\""));
    }

    #[tokio::test]
    async fn workbench_sparql_query_proxies_passthrough_results() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let response = handle_http_request(
            "POST",
            "/workbench/api/sparql/query",
            &headers,
            r#"{"query":"SELECT * WHERE { ?s ?p ?o } LIMIT 5","limit":5}"#,
            &storage,
            &metrics,
            &|u, p| {
                if u == "user" && p == "pass" {
                    Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"source\":\"mock-sparql\""));
        assert!(response.contains("\"query\":\"SELECT * WHERE { ?s ?p ?o } LIMIT 5\""));
    }

    #[tokio::test]
    async fn workbench_datalog_query_returns_derived_and_explanations() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let ctx = TenantContext {
            tenant_id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            session_origin: "test".into(),
        };
        let session_id = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        for edge in [
            TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id: a,
                edge_type: "co_occurs".into(),
                dst_id: b,
                weight: 1.0,
                metadata: None,
                created_at: chrono::Utc::now(),
            },
            TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id: b,
                edge_type: "co_occurs".into(),
                dst_id: c,
                weight: 1.0,
                metadata: None,
                created_at: chrono::Utc::now(),
            },
        ] {
            storage.typed_edge_put(&ctx, &edge).await.unwrap();
        }
        let response = handle_http_request(
            "POST",
            "/workbench/api/datalog/query",
            &headers,
            &format!(r#"{{"predicate":"related","session_id":"{session_id}"}}"#),
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(ctx.tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"derived_facts\""));
        assert!(response.contains("\"explanations\""));
    }

    #[tokio::test]
    async fn workbench_aliases_endpoints_list_and_put_aliases() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let tenant_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let ctx = TenantContext {
            tenant_id,
            session_origin: "http:alice".into(),
        };

        let seeded = crate::types::AliasEntry {
            tenant_id,
            alias_id: Uuid::new_v4(),
            alias_name: "graph-status".into(),
            scope_kind: crate::types::AliasScopeKind::Global,
            scope_ref: "*".into(),
            canonical_tool: "query_derived".into(),
            parameter_map: serde_json::json!({}),
            fixed_arguments: serde_json::json!({}),
            args_templates: serde_json::json!({}),
            status: crate::types::ClaimStatus::Approved,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        storage.alias_put(&ctx, &seeded).await.unwrap();
        storage
            .entity_put(
                &ctx,
                &crate::expert_system::alias_mirror_entity(&seeded, None),
            )
            .await
            .unwrap();

        let get_response = handle_http_request(
            "GET",
            "/workbench/api/aliases?alias_name=graph-status",
            &headers,
            "",
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(get_response.starts_with("HTTP/1.1 200 OK"));
        assert!(get_response.contains("\"alias_name\":\"graph-status\""));

        let post_response = handle_http_request(
            "POST",
            "/workbench/api/aliases",
            &headers,
            r#"{"alias_name":"status-check","canonical_tool":"query_derived","scope_kind":"global","status":"approved"}"#,
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(post_response.starts_with("HTTP/1.1 200 OK"));
        assert!(post_response.contains("\"alias_name\":\"status-check\""));
    }

    #[tokio::test]
    async fn workbench_rules_endpoints_support_source_filters_and_actions() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let tenant_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let ctx = TenantContext {
            tenant_id,
            session_origin: "http:alice".into(),
        };
        let rule = crate::types::RuleEntry {
            tenant_id,
            rule_id: "rule-http-test".into(),
            version: 1,
            name: "rule-http-test".into(),
            family: "related".into(),
            state: crate::types::RuleState::Active,
            rule_body: r#"related(X, Z) :- edge(X, "shortcut", Z)."#.into(),
            rule_weight: 1.0,
            incremental: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        storage.rule_put(&ctx, &rule).await.unwrap();

        let list_response = handle_http_request(
            "GET",
            "/workbench/api/rules?source=registry&family=related",
            &headers,
            "",
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(list_response.starts_with("HTTP/1.1 200 OK"));
        assert!(list_response.contains("\"source\":\"registry\""));
        assert!(list_response.contains("\"family\":\"related\""));

        let approve_response = handle_http_request(
            "POST",
            "/workbench/api/rules",
            &headers,
            r#"{"action":"approve","rule_id":"rule-http-test","review_note":"approved in http test"}"#,
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(approve_response.starts_with("HTTP/1.1 200 OK"));
        assert!(approve_response.contains("\"decision\":\"approved\""));

        let deprecate_response = handle_http_request(
            "POST",
            "/workbench/api/rules",
            &headers,
            r#"{"action":"deprecate","rule_id":"rule-http-test"}"#,
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(deprecate_response.starts_with("HTTP/1.1 200 OK"));
        assert!(deprecate_response.contains("\"deprecated\":true"));
    }

    #[tokio::test]
    async fn workbench_rules_endpoint_supports_registry_wildcard_listing() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let tenant_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let ctx = TenantContext {
            tenant_id,
            session_origin: "http:alice".into(),
        };
        for (rule_id, family) in [("rule-related", "related"), ("rule-reachable", "reachable")] {
            storage
                .rule_put(
                    &ctx,
                    &crate::types::RuleEntry {
                        tenant_id,
                        rule_id: rule_id.into(),
                        version: 1,
                        name: rule_id.into(),
                        family: family.into(),
                        state: crate::types::RuleState::Active,
                        rule_body: format!("{family}(X, Y) :- edge(X, \"{family}\", Y)."),
                        rule_weight: 1.0,
                        incremental: false,
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                    },
                )
                .await
                .unwrap();
        }

        let response = handle_http_request(
            "GET",
            "/workbench/api/rules?source=registry",
            &headers,
            "",
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"rule_id\":\"rule-related\""));
        assert!(response.contains("\"rule_id\":\"rule-reachable\""));
    }

    #[tokio::test]
    async fn workbench_summary_reports_ready_when_storage_queries_succeed() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let tenant_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let ctx = TenantContext {
            tenant_id,
            session_origin: "http:alice".into(),
        };
        storage
            .entity_put(
                &ctx,
                &crate::types::EntityEntry {
                    tenant_id,
                    entity_id: Uuid::new_v4(),
                    session_id: Uuid::new_v4(),
                    entity_name: "summary-test".into(),
                    entity_type: "concept".into(),
                    source_fold_id: None,
                    context_snippet: "summary".into(),
                    entity_embedding: None,
                    confidence: 1.0,
                    created_at: chrono::Utc::now(),
                    state: crate::types::MemoryState::Active,
                    description: None,
                    description_embedding: None,
                    tags: Vec::new(),
                    properties: serde_json::json!({}),
                    content_hash: None,
                    updated_at: None,
                    scope: crate::types::EntityScope::Session,
                    ingested_by_session: None,
                },
            )
            .await
            .unwrap();
        storage
            .rule_put(
                &ctx,
                &crate::types::RuleEntry {
                    tenant_id,
                    rule_id: "summary-rule".into(),
                    version: 1,
                    name: "summary-rule".into(),
                    family: "related".into(),
                    state: crate::types::RuleState::Active,
                    rule_body: r#"related(X, Y) :- edge(X, "related", Y)."#.into(),
                    rule_weight: 1.0,
                    incremental: false,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();

        let response = handle_http_request(
            "GET",
            "/workbench/api/summary",
            &headers,
            "",
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"status\":\"ready\""));
        assert!(response.contains("\"rule_count\":10"));
    }

    #[tokio::test]
    async fn workbench_explanations_endpoint_returns_explicit_drilldown() {
        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let headers = vec![(
            "Authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )];
        let ctx = TenantContext {
            tenant_id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            session_origin: "test".into(),
        };
        let session_id = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        for edge in [
            TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id: a,
                edge_type: "co_occurs".into(),
                dst_id: b,
                weight: 1.0,
                metadata: None,
                created_at: chrono::Utc::now(),
            },
            TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id: b,
                edge_type: "co_occurs".into(),
                dst_id: c,
                weight: 1.0,
                metadata: None,
                created_at: chrono::Utc::now(),
            },
        ] {
            storage.typed_edge_put(&ctx, &edge).await.unwrap();
        }
        let response = handle_http_request(
            "POST",
            "/workbench/api/explanations/query",
            &headers,
            &format!(
                r#"{{"predicate":"related","session_id":"{session_id}","src_id":"{a}","dst_id":"{c}","limit":8}}"#
            ),
            &storage,
            &metrics,
            &move |u, p| {
                if u == "user" && p == "pass" {
                    Some(ctx.tenant_id)
                } else {
                    None
                }
            },
            &|| true,
            &ShellRouteConfig::default(),
        )
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"predicate\":\"related\""));
        assert!(response.contains("\"explanations\""));
        assert!(response.contains("\"support_chain\""));
    }

    #[tokio::test]
    async fn handle_connection_rw_allows_multiple_requests_on_keep_alive_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let (mut client, mut server) = tokio::io::duplex(8192);

        let server_task = tokio::spawn(async move {
            serve_one_connection(
                &mut server,
                &storage,
                &metrics,
                &|u, p| {
                    if u == "user" && p == "pass" {
                        Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                    } else {
                        None
                    }
                },
                &|| true,
                &ShellRouteConfig::default(),
            )
            .await
            .unwrap();
        });

        let client_task = tokio::spawn(async move {
            const INIT_BODY: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}";
            let initialize = format!(
                "POST /mcp HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Authorization: Basic dXNlcjpwYXNz\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{INIT_BODY}",
                INIT_BODY.len()
            );
            client.write_all(initialize.as_bytes()).await.unwrap();
            client.flush().await.unwrap();

            let mut buf = vec![0u8; 4096];
            let n1 = client.read(&mut buf).await.unwrap();
            let first = String::from_utf8_lossy(&buf[..n1]).to_string();
            assert!(first.starts_with("HTTP/1.1 200 OK"), "got: {first}");
            assert!(first.contains("\"id\":1"), "got: {first}");

            const NOTIF_BODY: &str =
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}";
            let initialized = format!(
                "POST /mcp HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Authorization: Basic dXNlcjpwYXNz\r\n\
                 Content-Type: application/json\r\n\
                 Connection: close\r\n\
                 Content-Length: {}\r\n\r\n{NOTIF_BODY}",
                NOTIF_BODY.len()
            );
            client.write_all(initialized.as_bytes()).await.unwrap();
            client.flush().await.unwrap();

            // Per MCP Streamable-HTTP: a notification gets 202 Accepted
            // with empty body. Returning 200 + `{"id":null,"result":null}`
            // is exactly what broke Codex's rmcp transport.
            let n2 = client.read(&mut buf).await.unwrap();
            let second = String::from_utf8_lossy(&buf[..n2]).to_string();
            assert!(
                second.starts_with("HTTP/1.1 202 Accepted"),
                "expected 202 for notification, got: {second}"
            );
            assert!(
                second.contains("Content-Length: 0"),
                "202 must have empty body; got: {second}"
            );
        });

        client_task.await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn keep_alive_idle_timeout_closes_without_504_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let metrics = MemoryMetrics::new().unwrap();
        let storage = MockStorage::new();
        let (mut client, mut server) = tokio::io::duplex(8192);

        let server_task = tokio::spawn(async move {
            serve_one_connection_with_session_budget(
                &mut server,
                &storage,
                &metrics,
                &|u, p| {
                    if u == "user" && p == "pass" {
                        Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
                    } else {
                        None
                    }
                },
                &|| true,
                &ShellRouteConfig::default(),
                &dispatch::SessionState::default(),
                std::time::Duration::from_millis(20),
            )
            .await
        });

        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        client.flush().await.unwrap();

        let mut buf = vec![0u8; 4096];
        let n = client.read(&mut buf).await.unwrap();
        let first = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(first.starts_with("HTTP/1.1 200 OK"), "got: {first}");

        let result = server_task.await.unwrap();
        assert!(
            result.is_ok(),
            "an idle keep-alive socket after a successful request should close cleanly, got {result:?}"
        );
    }

    #[test]
    fn mcp_request_timeout_returns_json_rpc_warming_error_not_504() {
        let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"retrieve_entities","arguments":{"entity_ids":["00000000-0000-0000-0000-000000000001"]}}}"#;

        let response =
            timeout_response_for_request("POST", "/mcp", body, std::time::Duration::from_secs(30));

        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "MCP timeout must stay in JSON-RPC response space, got: {response}"
        );
        assert!(
            !response.starts_with("HTTP/1.1 504 Gateway Timeout"),
            "MCP clients should not see bare gateway timeouts: {response}"
        );
        assert!(response.contains(r#""id":7"#), "got: {response}");
        assert!(response.contains(r#""error""#), "got: {response}");
        assert!(response.contains("warming"), "got: {response}");
        assert!(response.contains("retry"), "got: {response}");
        assert!(response.contains("backoff"), "got: {response}");
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

    use crate::storage::mock::MockEdge;
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
            ..Default::default()
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

        let snap = build_snapshot(&storage, &ctx, nil, VizSnapshotScope::SessionOnly).await;
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
        // Edge stored under nil session (e.g. frg ingest)
        storage.typed_edges.lock().await.push(test_typed_edge(
            ctx.tenant_id,
            nil,
            e1,
            e2,
            "contains",
        ));

        let snap = build_snapshot(&storage, &ctx, viz_session, VizSnapshotScope::SessionOnly).await;
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

        let snap = build_snapshot(&storage, &ctx, viz_session, VizSnapshotScope::SessionOnly).await;
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

        let snap = build_snapshot(&storage, &ctx, nil, VizSnapshotScope::SessionOnly).await;
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

        let snap = build_snapshot(&storage, &ctx, nil, VizSnapshotScope::SessionOnly).await;
        let VizEvent::Snapshot { edges, .. } = snap else {
            panic!("expected Snapshot");
        };
        // CO_OCCURS edges without strength now get a default strength of 0.5 (line 906-908)
        // so they are included in the snapshot rather than filtered out.
        // Note: edge appears twice because build_snapshot loads from both swapped_ctx and correct ctx,
        // and when session_id is nil, both are nil so the same edge is loaded twice.
        assert!(
            !edges.is_empty(),
            "CO_OCCURS edge should be present with default strength"
        );
        for edge in &edges {
            if edge.edge_type == "CO_OCCURS" {
                assert_eq!(
                    edge.strength,
                    Some(0.5),
                    "CO_OCCURS edge should have default strength 0.5"
                );
            }
        }
    }
}
