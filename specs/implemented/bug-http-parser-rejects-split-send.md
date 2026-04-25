---
type: bug
priority: P2
status: implemented
created: 2026-04-19
updated: 2026-04-20
reported-by: Claude Code memory hooks failing against fmem HTTP MCP endpoint (2026-04-19)
---

## Implementation Notes

Fixed alongside `feat-concurrent-http-server` — the root cause the spec
predicted in hypothesis (1) is exactly what landed:

- `read_http_request(stream, max_bytes)` in `http.rs` now loops reads
  until both `\r\n\r\n` is seen AND `Content-Length` bytes of body are
  in the buffer, tolerating any number of intervening `recv` calls.
  Generic over `AsyncReadExt`, so it runs identically on `TcpStream`
  and `tokio_rustls::server::TlsStream`.
- `serve_one_connection` now calls `stream.shutdown()` at the end so
  TLS sends `close_notify` and plain TCP sends FIN. Without this,
  strict clients (rustls, Go's `net/http`) surface `UnexpectedEof`
  instead of a clean close.
- Adjacent fix: rate-limit path no longer `drop(stream)` — it writes
  HTTP 429 + `Retry-After: 60` and half-closes. The old `drop` was a
  second `ConnectionResetError` source for Python clients that had
  already written a POST body.

Regression tests in `tests/http_concurrency.rs`:

- `split_write_request_gets_response` — plain HTTP, headers/body with
  a 30ms gap.
- `python_style_post_without_client_shutdown_gets_response` — plain
  HTTP, no client `shutdown()` (Python `http.client` doesn't).
- `python_http_client_post_succeeds_end_to_end` — spawns real
  `python3 -c http.client` and asserts no `ConnectionResetError`.
- `https_split_send_post_gets_response` — HTTPS via `rcgen`-
  generated self-signed cert, split send.
- `rate_limited_connection_does_not_reset_client` — 60 requests
  against a 50/min limiter, asserts 0 RSTs and that 429s are
  observed.

All 7 concurrency tests green; full workspace untouched.

# HTTP parser rejects POST requests whose body arrives in a second TCP packet

## Observed

Python's standard library HTTP client (`http.client.HTTPSConnection`,
which is what `urllib.request` uses under the hood) consistently fails
when POSTing a JSON-RPC call to fmem's HTTPS MCP endpoint. The error
surfaces as either `http.client.RemoteDisconnected: Remote end closed
connection without response` or `ConnectionResetError: [Errno 54]
Connection reset by peer`. The failure reproduces with TLS 1.2 *and*
TLS 1.3, with or without ALPN, and with headers matched byte-for-byte
to curl's working request.

`curl` succeeds against the same endpoint with identical headers and
body. A raw Python TLS socket that calls `sendall(headers + body)` in
a single write also succeeds.

The differentiator is visible in `http.client`'s debug output
(`set_debuglevel(1)`):

```
send: b'POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n...\r\n\r\n'
send: b'{"jsonrpc": ...}'
reply: ''
```

**Two separate `send` calls** — one for the request line + headers,
one for the body — produce two TCP segments (especially over loopback
where Nagle is off by default). fmem's HTTP parser treats the second
segment as an anomaly and closes the connection without returning a
response.

Minimizing the send into one `sendall(headers + body)` fixes the
failure — see `~/.claude/hooks/memory_lib.py` in the research repo for
the workaround.

## Why it matters

Splitting a POST into "send headers, then send body" is the default
behavior of:

- Python `http.client` / `urllib.request` / `requests` / `httpx`
- Go's `net/http` under some conditions (especially with
  `Transfer-Encoding: chunked` or `Expect: 100-continue`)
- Java's `HttpURLConnection` / JDK `HttpClient`
- Node.js `http`/`https` modules
- Many other stdlib clients

Any of these talking to the fmem HTTPS endpoint will hit the same
failure. Forcing every client to coalesce its send is not a
reasonable expectation for a well-behaved HTTP/1.1 server.

Observed impact in this project:

- Claude Code's auto-memory hooks (written in Python) couldn't reach
  the endpoint until I dropped `urllib` and implemented a raw-socket
  POST.
- Any third-party integration using a stdlib HTTP client will hit
  this.
- The failure mode is silent (connection reset, no body) — hard to
  diagnose without packet capture or debug-level HTTP logging.

## Reproduction

Requires fmem running with the `examples/ferrosa-memory-http.toml`
config (HTTPS + basic auth) and a valid credential in `~/.mcp.json`.

```bash
python3 <<'EOF'
import json, ssl, http.client
ctx = ssl.create_default_context()
ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
auth = json.load(open('/Users/bkearns/.mcp.json'))['mcpServers']['ferrosa-memory']['headers']['Authorization']
body = json.dumps({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}).encode()
conn = http.client.HTTPSConnection('localhost', 8765, context=ctx, timeout=5)
conn.request('POST', '/mcp', body=body, headers={
    "Content-Type":"application/json",
    "Accept":"application/json, text/event-stream",
    "Authorization": auth,
})
try:
    r = conn.getresponse()
    print('status:', r.status)
except Exception as e:
    print('EXC:', type(e).__name__, e)
EOF
# Expected: status: 200
# Actual:   EXC: ConnectionResetError [Errno 54] Connection reset by peer
#        or EXC: RemoteDisconnected Remote end closed connection without response
```

curl succeeds against the same endpoint with the same headers:

```bash
AUTH=$(python3 -c "import json; d=json.load(open('/Users/bkearns/.mcp.json')); print(d['mcpServers']['ferrosa-memory']['headers']['Authorization'])")
curl -sk -X POST https://localhost:8765/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -H "Authorization: $AUTH" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  -o /dev/null -w "%{http_code}\n"
# → 200
```

A raw Python TLS socket with a single `sendall` also succeeds:

```python
ssock.sendall(http_request_bytes + body_bytes)  # single call
# → 200 OK with full JSON-RPC result
```

## Hypothesis

fmem's HTTP listener (hyper-based, presumably, given Rust + tokio)
may be using a read strategy that gives up when the first TCP read
doesn't contain the complete HTTP message. Likely candidates:

1. The parser reads `content-length` bytes **from the same recv** as
   the headers, and if the body hasn't arrived yet, treats it as
   malformed.
2. A middleware layer (auth? logging? TLS termination?) consumes the
   body eagerly and raises when `fill_buf` returns fewer bytes than
   `Content-Length` on the first read.
3. Idle-connection or read-timeout config is set too aggressively
   (sub-millisecond), causing the second segment — which arrives
   microseconds after the first due to TCP_NODELAY + loopback
   scheduling — to miss the window.

## Proposed fix directions

- **Preferred:** ensure the HTTP body reader loops until
  `Content-Length` bytes have been read (or the connection closes),
  tolerating any number of intervening `recv` calls. This is what
  production HTTP servers do by default.
- If using `hyper`, upgrade to the latest stable — this class of bug
  has been fixed multiple times upstream. Worth checking
  `Cargo.lock`'s `hyper` version.
- Add a regression test: spawn a small Python/Go client that splits
  `send` and assert the server still returns 200. Loopback + TLS
  makes this race deterministic.

## Related

- `specs/implemented/bug-initialize-blocks-on-backend-connect.md` —
  earlier HTTPS endpoint issues (slow initialize). Fixed.
- `specs/implemented/bug-ingest-skill-tag-crosstalk.md` and
  `specs/implemented/bug-ingest-skill-bulk-nondeterminism.md` —
  concurrency bugs in the ingest path. Fixed.
- `specs/todo/bug-ingest-skill-cluster-tag-dropped.md` — remaining
  dropped-tag regression, unrelated to this.

## Acceptance

- `http.client.HTTPSConnection` in Python 3.11+ can POST JSON-RPC to
  `https://localhost:8765/mcp` and receive a response.
- `requests.post(...)` / `httpx.post(...)` / Go `http.Post(...)` /
  `curl --data-binary @file` all succeed.
- Regression test: client that writes headers, sleeps 10ms, then
  writes body. Server returns 200.
- `frg fmem-skill-ingest` — already uses a custom stdio path, so this
  doesn't affect it. But any future HTTP-transport wiring will.

---

## Verification 2026-04-19

Fix shipped and confirmed end-to-end after fmem HTTPS restart (new
pid etime 02:39).

**Test 1 — deliberate split-send (raw socket + 10ms sleep):**

```
sendall(headers); time.sleep(0.01); sendall(body)
→ 3/3 runs: HTTP/1.1 200 OK
```

Previously: 3/3 `ConnectionResetError`.

**Test 2 — Python stdlib `http.client` default behavior:**

```python
conn = http.client.HTTPSConnection('localhost', 8765, context=ctx)
conn.request('POST', '/mcp', body=body, headers={...})
r = conn.getresponse()
→ 3/3 runs: status=200, 22164 bytes (full tools/list response)
```

Previously: 3/3 `RemoteDisconnected` / `ConnectionResetError`.

**Note:** Claude Code auto-memory hooks
(`~/.claude/hooks/memory_lib.py`) still use the raw-socket workaround
coded during diagnosis. It is strictly safer and more portable across
Python stdlib versions, so it stays. Future HTTP integrations can use
stdlib clients directly.
