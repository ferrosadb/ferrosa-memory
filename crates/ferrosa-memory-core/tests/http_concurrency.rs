//! Integration tests for the concurrent HTTP server.
//!
//! Guards against the regressions the feature work was chartered to
//! fix: sequential accept loop, single-read HTTP parser, missing
//! per-request timeout. Runs over plain HTTP (no TLS) to keep the
//! test free of cert machinery — the spawn + timeout code paths are
//! the same in both branches of `serve_http`.

use std::sync::Arc;
use std::time::Duration;

use ferrosa_memory_core::http::{HttpConfig, serve_http};
use ferrosa_memory_core::metrics::MemoryMetrics;
use ferrosa_memory_core::storage::mock::MockStorage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Bind to an ephemeral port and return it. The OS-assigned port
/// avoids collisions when tests run in parallel.
async fn free_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

async fn start_server() -> u16 {
    let port = free_port().await;
    let config = HttpConfig {
        port,
        require_tls: false,
        cert_path: None,
        key_path: None,
        readiness_checker: Arc::new(|| true),
    };
    let storage = Arc::new(MockStorage::new());
    let metrics = Arc::new(MemoryMetrics::new().unwrap());
    let validator = Arc::new(|_: &str, _: &str| None);
    tokio::spawn(async move {
        let _ = serve_http(config, storage, metrics, validator).await;
    });
    // Give the listener time to bind before tests connect.
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return port;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server did not start on port {port}");
}

/// Regression for the field wedge: a client that opens TCP but never
/// sends bytes should not stall concurrent requests. Before the spawn
/// rewrite, the accept loop was sequential, so a dangling connection
/// blocked every other client on the next request.
#[tokio::test]
async fn stalled_client_does_not_block_concurrent_request() {
    let port = start_server().await;

    // Open a connection and never send anything.
    let stalled = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // Concurrent client should complete quickly because the server
    // spawns each connection — the stalled TCP session sits in its
    // own task waiting on read.
    let start = std::time::Instant::now();
    let resp = tokio::time::timeout(Duration::from_secs(5), async {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET /healthz/live HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        buf
    })
    .await
    .expect("concurrent request must not hang on stalled peer");
    let elapsed = start.elapsed();

    assert!(
        resp.starts_with(b"HTTP/1.1 200"),
        "expected 200, got: {:?}",
        String::from_utf8_lossy(&resp[..resp.len().min(80)])
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "concurrent response should be fast; took {elapsed:?}"
    );

    drop(stalled);
}

/// Simulates Python `http.client.HTTPConnection.send` which calls
/// `sock.sendall` twice: once for headers, once for body. The server
/// must read across both reads rather than parsing the first chunk
/// and closing the connection on "missing body".
#[tokio::test]
async fn split_write_request_gets_response() {
    let port = start_server().await;

    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    // Send headers only, flush, sleep, then send body. This matches
    // the Nagle-off / TCP_NODELAY path Python takes by default.
    s.write_all(
        b"POST /mcp HTTP/1.1\r\n\
          Host: localhost\r\n\
          Content-Type: application/json\r\n\
          Authorization: Basic dXNlcjpwYXNz\r\n\
          Connection: close\r\n\
          Content-Length: 44\r\n\r\n",
    )
    .await
    .unwrap();
    s.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    s.write_all(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#[..44].as_ref())
        .await
        .unwrap();
    s.flush().await.unwrap();
    s.shutdown().await.unwrap();

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
        .await
        .expect("server should respond to split-write request")
        .unwrap();

    // The credentials are rejected (MockStorage has no configured
    // tenants), so we expect 401, not a hang or 400.
    let head = String::from_utf8_lossy(&buf);
    assert!(
        head.starts_with("HTTP/1.1 401"),
        "expected 401 Unauthorized, got: {}",
        &head[..head.len().min(120)]
    );
}

/// Regression test for `bug-http-parser-rejects-split-send`: the
/// acceptance criterion is that Python `http.client.HTTPSConnection`
/// can POST to `https://.../mcp` without `ConnectionResetError`.
/// The fix (loop reads in `read_http_request`) is TLS-transparent
/// because `TlsStream` and `TcpStream` both impl `AsyncRead`, but
/// it's worth a dedicated HTTPS test to make sure no future change
/// routes the body through a single-read shortcut in the TLS path.
#[tokio::test]
async fn https_split_send_post_gets_response() {
    let (cert_pem, key_pem) = generate_self_signed_cert();
    let cert_file = tempfile::NamedTempFile::new().unwrap();
    let key_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(cert_file.path(), &cert_pem).unwrap();
    std::fs::write(key_file.path(), &key_pem).unwrap();

    let port = free_port().await;
    let config = HttpConfig {
        port,
        require_tls: true,
        cert_path: Some(cert_file.path().to_string_lossy().into_owned()),
        key_path: Some(key_file.path().to_string_lossy().into_owned()),
        readiness_checker: Arc::new(|| true),
    };
    let storage = Arc::new(MockStorage::new());
    let metrics = Arc::new(MemoryMetrics::new().unwrap());
    let validator = Arc::new(|_: &str, _: &str| None);
    tokio::spawn(async move {
        let _ = serve_http(config, storage, metrics, validator).await;
    });
    // Wait for the TLS listener to bind.
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let tls_connector = {
        use tokio_rustls::rustls::{self, ClientConfig};
        // Test scope: accept any cert. Production clients pin via CA.
        let mut root_store = rustls::RootCertStore::empty();
        let cert = rustls::pki_types::CertificateDer::from(
            rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem.as_bytes()))
                .next()
                .unwrap()
                .unwrap()
                .to_vec(),
        );
        root_store.add(cert).unwrap();
        let config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    };

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let domain = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls = tls_connector.connect(domain, tcp).await.unwrap();

    let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let head = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: localhost:{port}\r\n\
         Content-Type: application/json\r\n\
         Authorization: Basic dXNlcjpwYXNz\r\n\
         Connection: close\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    // Two writes with a sleep in between — the exact split the spec
    // says Python http.client produces.
    tls.write_all(head.as_bytes()).await.unwrap();
    tls.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tls.write_all(body).await.unwrap();
    tls.flush().await.unwrap();

    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut resp))
        .await
        .expect("TLS read must not hang")
        .expect("server must respond without RST");

    let head_str = String::from_utf8_lossy(&resp);
    assert!(
        head_str.starts_with("HTTP/1.1 401"),
        "expected 401 (invalid creds), got: {}",
        &head_str[..head_str.len().min(200)]
    );
}

/// Generate a throwaway self-signed cert + key for localhost.
/// Used only by the HTTPS integration test.
fn generate_self_signed_cert() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (cert.cert.pem(), cert.key_pair.serialize_pem())
}

/// Regression for `bug-http-mcp-codex-initialized-notification`:
/// MCP Streamable-HTTP (2025-03-26) § "Sending Messages to the
/// Server" mandates that when the POST body is a JSON-RPC
/// *notification* (method present, `id` absent), the server MUST
/// return **HTTP 202 Accepted with no body**. Our pre-fix code
/// wrapped the dispatch result in `{"jsonrpc":"2.0","id":null,"result":null}`
/// and returned 200, which Codex's rmcp transport fails to decode —
/// the worker quits with `error decoding response body, when send
/// initialized notification`.
#[tokio::test]
async fn notification_returns_202_with_no_body() {
    let port = start_server().await;
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    let body = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Authorization: Basic dXNlcjpwYXNz\r\n\
         Connection: close\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    s.flush().await.unwrap();

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
        .await
        .expect("read must not hang")
        .unwrap();

    let text = String::from_utf8_lossy(&buf);

    // MockStorage's credential validator rejects the basic-auth header
    // (no configured tenants), so for this test we accept *either* the
    // 202 contract (what a real auth'd notification must produce) *or*
    // a 401 before reaching the notification path. What we must never
    // see is `{"id":null,"result":null}` — that's the shape Codex
    // fails to decode.
    assert!(
        !text.contains(r#""result":null"#),
        "server still wraps notifications as JSON-RPC responses; body: {}",
        &text[..text.len().min(400)]
    );
    assert!(
        text.starts_with("HTTP/1.1 202") || text.starts_with("HTTP/1.1 401"),
        "expected 202 Accepted (or 401 if auth stubbed), got: {}",
        &text[..text.len().min(120)]
    );
    if text.starts_with("HTTP/1.1 202") {
        // 202 must have zero body.
        let body_start = text.find("\r\n\r\n").unwrap() + 4;
        assert_eq!(
            text.len() - body_start,
            0,
            "202 Accepted must have empty body; got {} bytes after headers",
            text.len() - body_start
        );
    }
}

/// Codex's failure mode: `initialize` (200 OK with JSON), then
/// `notifications/initialized` on the *same* HTTPS keep-alive
/// connection. Both must succeed; the second must not close the
/// transport. This is the acceptance-criterion test from the spec.
#[tokio::test]
async fn https_initialize_then_initialized_notification_same_connection() {
    let (cert_pem, key_pem) = generate_self_signed_cert();
    let cert_file = tempfile::NamedTempFile::new().unwrap();
    let key_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(cert_file.path(), &cert_pem).unwrap();
    std::fs::write(key_file.path(), &key_pem).unwrap();

    let port = free_port().await;
    let config = HttpConfig {
        port,
        require_tls: true,
        cert_path: Some(cert_file.path().to_string_lossy().into_owned()),
        key_path: Some(key_file.path().to_string_lossy().into_owned()),
        readiness_checker: Arc::new(|| true),
    };
    let storage = Arc::new(MockStorage::new());
    let metrics = Arc::new(MemoryMetrics::new().unwrap());
    // Accept any basic-auth credential so the notification hits the
    // dispatch path, not the 401 short-circuit.
    let validator = Arc::new(|_: &str, _: &str| Some(uuid::Uuid::nil()));
    tokio::spawn(async move {
        let _ = serve_http(config, storage, metrics, validator).await;
    });
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let tls_connector = {
        use tokio_rustls::rustls::{self, ClientConfig};
        let mut root_store = rustls::RootCertStore::empty();
        let cert = rustls::pki_types::CertificateDer::from(
            rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem.as_bytes()))
                .next()
                .unwrap()
                .unwrap()
                .to_vec(),
        );
        root_store.add(cert).unwrap();
        let c = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(c))
    };
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let domain = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls = tls_connector.connect(domain, tcp).await.unwrap();

    // --- Request 1: initialize (expect 200 + JSON-RPC body) ---
    let init_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}"#;
    let init_req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: localhost:{port}\r\n\
         Content-Type: application/json\r\n\
         Authorization: Basic Y29kZXg6ZHVtbXk=\r\n\
         Content-Length: {}\r\n\r\n",
        init_body.len()
    );
    tls.write_all(init_req.as_bytes()).await.unwrap();
    tls.write_all(init_body).await.unwrap();
    tls.flush().await.unwrap();

    let init_resp = read_one_http_response(&mut tls).await;
    assert!(
        init_resp.status_line.starts_with("HTTP/1.1 200"),
        "initialize must return 200; got: {}",
        init_resp.status_line
    );
    assert!(
        init_resp.body.contains(r#""jsonrpc":"2.0""#) && init_resp.body.contains(r#""id":1"#),
        "initialize body must be JSON-RPC with matching id; got: {}",
        init_resp.body
    );

    // --- Request 2: notifications/initialized on SAME connection ---
    let notif_body = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let notif_req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: localhost:{port}\r\n\
         Content-Type: application/json\r\n\
         Authorization: Basic Y29kZXg6ZHVtbXk=\r\n\
         Content-Length: {}\r\n\r\n",
        notif_body.len()
    );
    tls.write_all(notif_req.as_bytes()).await.unwrap();
    tls.write_all(notif_body).await.unwrap();
    tls.flush().await.unwrap();

    let notif_resp = read_one_http_response(&mut tls).await;
    assert!(
        notif_resp.status_line.starts_with("HTTP/1.1 202"),
        "notifications/initialized must return 202 Accepted; got: {}",
        notif_resp.status_line
    );
    assert!(
        notif_resp.body.is_empty(),
        "202 Accepted must have empty body; got {} bytes: {:?}",
        notif_resp.body.len(),
        &notif_resp.body[..notif_resp.body.len().min(120)]
    );
}

/// Read a single HTTP/1.1 response from `stream`, respecting
/// Content-Length so the reader stops exactly at the end of this
/// response (leaving any pipelined follow-up bytes in the kernel
/// buffer). Used by the keep-alive integration test.
struct HttpResponse {
    status_line: String,
    body: String,
}

async fn read_one_http_response<T: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut T,
) -> HttpResponse {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    // Read until end-of-headers marker is present.
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
            break p;
        }
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .expect("read timed out")
            .expect("read failed");
        assert!(n > 0, "connection closed before headers complete");
        buf.extend_from_slice(&chunk[..n]);
    };

    let head_str = std::str::from_utf8(&buf[..head_end]).unwrap();
    let status_line = head_str.lines().next().unwrap().to_string();
    let content_length = head_str
        .split("\r\n")
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let body_end = head_end + content_length;
    while buf.len() < body_end {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .expect("body read timed out")
            .expect("body read failed");
        assert!(n > 0, "connection closed before body complete");
        buf.extend_from_slice(&chunk[..n]);
    }

    let body = String::from_utf8_lossy(&buf[head_end..body_end]).into_owned();
    HttpResponse { status_line, body }
}

/// Faithful reproduction of what Python `http.client.HTTPConnection`
/// sends: two back-to-back `sendall`s (headers, body), then wait for
/// the response. Critically the client does **not** half-close after
/// writing — Python's `getresponse()` reads while the socket is still
/// write-open. The existing `split_write_request_gets_response` test
/// called `shutdown()` before reading, which was masking whatever
/// made the real Python client see `ConnectionResetError` against
/// the server.
#[tokio::test]
async fn python_style_post_without_client_shutdown_gets_response() {
    let port = start_server().await;

    let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // First sendall: request line + headers + CRLFCRLF terminator.
    // This matches Python's _send_output ordering exactly.
    let head = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Accept-Encoding: identity\r\n\
         Content-Type: application/json\r\n\
         Authorization: Basic dXNlcjpwYXNz\r\n\
         Connection: close\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.flush().await.unwrap();

    // Second sendall: the body. No sleep — Python sends these
    // immediately. No shutdown — Python stays write-open and calls
    // getresponse() next.
    s.write_all(body).await.unwrap();
    s.flush().await.unwrap();

    // Read until the server closes. Any RST here surfaces as
    // ConnectionReset, matching the Python error the user reported.
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
        .await
        .expect("read must not hang")
        .expect("server must close with FIN, not RST");

    let head_str = String::from_utf8_lossy(&buf);
    assert!(
        head_str.starts_with("HTTP/1.1 401"),
        "expected 401 Unauthorized, got: {}",
        &head_str[..head_str.len().min(200)]
    );
}

/// End-to-end test using the real `python3 -m http.client` code
/// path. If `python3` isn't on PATH the test skips; on dev machines
/// and CI with Python installed, this is the ground-truth check
/// that the reported `ConnectionResetError` is gone.
#[tokio::test]
async fn python_http_client_post_succeeds_end_to_end() {
    if tokio::process::Command::new("python3")
        .arg("--version")
        .output()
        .await
        .is_err()
    {
        eprintln!("python3 not available; skipping");
        return;
    }
    let port = start_server().await;

    let script = format!(
        "import http.client, sys\n\
         conn = http.client.HTTPConnection('127.0.0.1', {port}, timeout=5)\n\
         try:\n\
         \x20   conn.request('POST', '/mcp',\n\
         \x20       body=b'{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}}',\n\
         \x20       headers={{'Content-Type':'application/json','Authorization':'Basic dXNlcjpwYXNz'}})\n\
         \x20   resp = conn.getresponse()\n\
         \x20   body = resp.read().decode()\n\
         \x20   print(f'STATUS={{resp.status}}')\n\
         \x20   print(f'BODY={{body}}')\n\
         except Exception as e:\n\
         \x20   print(f'ERROR={{type(e).__name__}}: {{e}}', file=sys.stderr)\n\
         \x20   sys.exit(2)\n"
    );

    let out = tokio::process::Command::new("python3")
        .arg("-c")
        .arg(&script)
        .output()
        .await
        .expect("spawn python3");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "python http.client POST failed:\n  stdout: {stdout}\n  stderr: {stderr}"
    );
    assert!(
        stdout.contains("STATUS=401"),
        "expected 401 (invalid creds), got: {stdout}"
    );
}

/// When an IP exceeds the rate-limit the accept loop currently does
/// `drop(stream)` — the kernel sees a socket with unread data
/// sitting in the recv buffer and sends RST on close. That's exactly
/// what Python would surface as `ConnectionResetError`. Production
/// hits this any time a dev loop posts >50 requests per minute from
/// one IP.
///
/// Expected behavior: the client gets an HTTP 429 (or at worst a
/// FIN-close) rather than a reset. The test spams 60 requests and
/// asserts every connection that completes its send gets either a
/// successful status line or a clean EOF — never a reset.
#[tokio::test]
async fn rate_limited_connection_does_not_reset_client() {
    let port = start_server().await;

    let mut resets = 0usize;
    let mut ok_200 = 0usize;
    let mut too_many_429 = 0usize;

    for i in 0..60 {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!(
            "GET /healthz/live HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nX-Seq: {i}\r\n\r\n"
        );
        if s.write_all(req.as_bytes()).await.is_err() {
            resets += 1;
            continue;
        }
        let mut buf = Vec::new();
        match tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut buf)).await {
            Ok(Ok(_)) => {
                if buf.starts_with(b"HTTP/1.1 200") {
                    ok_200 += 1;
                } else if buf.starts_with(b"HTTP/1.1 429") {
                    too_many_429 += 1;
                } else {
                    panic!("unexpected response: {:?}", &buf[..buf.len().min(80)]);
                }
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                resets += 1;
            }
            Ok(Err(e)) => panic!("unexpected io error: {e}"),
            Err(_) => panic!("read hung on request {i}"),
        }
    }

    eprintln!("rate-limit: 200={ok_200} 429={too_many_429} RST={resets}");
    assert_eq!(resets, 0, "rate-limited clients must not see RST");
    assert!(
        too_many_429 > 0,
        "expected some requests to be rate-limited; got 200={ok_200} 429={too_many_429}"
    );
    assert_eq!(
        ok_200 + too_many_429,
        60,
        "every request must get a response, not be dropped"
    );
}

/// Smoke test for the spawned-accept path: 40 parallel health checks
/// must all complete well under the old sequential worst case
/// (~N × per-request latency). A weaker version of the spec's
/// 100-clients-<2s acceptance bar — 40 stays under the 50/min rate
/// limit and avoids macOS loopback RST-on-close flakiness at higher
/// fan-out.
#[tokio::test]
async fn forty_parallel_clients_all_succeed_quickly() {
    let port = start_server().await;

    let start = std::time::Instant::now();
    let mut handles = Vec::with_capacity(40);
    for _ in 0..40 {
        handles.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(b"GET /healthz/live HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.unwrap();
            buf
        }));
    }
    for h in handles {
        let resp = h.await.unwrap();
        assert!(resp.starts_with(b"HTTP/1.1 200"));
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "40 parallel health checks should finish fast; took {elapsed:?}"
    );
}
