---
type: bug
priority: P1
status: implemented
created: 2026-04-20
updated: 2026-04-20
reported-by: Codex HTTP MCP verification against container endpoint (2026-04-19/20)
---

## Implementation Notes

Root cause was hypothesis (3), not (1) or (2): pre-existing keep-alive
support (added earlier in this branch) handled the second request
fine. What broke Codex's rmcp transport was the **response body shape**
for notifications. The server wrapped `{"jsonrpc":"2.0","method":
"notifications/initialized"}` as `{"jsonrpc":"2.0","id":null,
"result":null}` at HTTP 200, which rmcp can't decode — notifications
by JSON-RPC spec must not get a response with a request id.

Fix, per MCP Streamable-HTTP (2025-03-26) rules:

- When the POST body is a JSON-RPC **notification** (method present,
  `id` absent) or a **client response** (`result` or `error` present,
  `method` absent), the server returns **HTTP 202 Accepted with
  `Content-Length: 0` and no body**.
- Dispatch still runs for side effects (`notifications/initialized`
  flips readiness, etc.); any dispatch error is logged and suppressed
  since the contract forbids a response body.
- New helper `accepted_no_body_response()` in `http.rs`.

Regression tests:

- `https_initialize_then_initialized_notification_same_connection` —
  the spec's acceptance test: initialize + notifications/initialized
  over one HTTPS keep-alive connection, mirroring Codex's rmcp flow.
- `notification_returns_202_with_no_body` — plain HTTP POST with no
  `id`; asserts 202 + empty body and (negatively) that the
  `"result":null` shape never appears.
- Lib test `handle_connection_rw_allows_multiple_requests_on_keep_alive_connection`
  fixed: the placeholder had off-by-8/off-by-11 `Content-Length`
  values and asserted the now-wrong 200 + `"result":null` shape —
  rewrote to compute lengths from body literals and assert 202 with
  zero body.

Existing concurrency tests picked up `Connection: close` headers
because the earlier keep-alive rewrite made single-request tests
hang on the server waiting for a second request.

# HTTPS MCP endpoint fails Codex on post-`initialize` notification

## Observed

Codex is configured to connect to the container-backed HTTPS MCP endpoint:

```toml
[mcp_servers.ferrosa-memory]
url = "https://codex:***@localhost:18765/mcp"
```

The endpoint itself is alive and passes a normal verified HTTPS MCP
`initialize` request:

```bash
curl -u 'codex:***' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  https://localhost:18765/mcp

# Result: HTTP/1.1 200 OK
# TLS verifies cleanly for localhost
# Response body contains the expected initialize result
```

But a fresh Codex session against the same URL fails immediately after
handshake:

```text
ERROR rmcp::transport::worker: worker quit with fatal:
Client error: error decoding response body,
when send initialized notification
```

Observed via:

```bash
codex exec \
  -c 'mcp_servers.ferrosa-memory.url="https://codex:***@localhost:18765/mcp"' \
  --skip-git-repo-check \
  "Reply with the word ok."
```

Codex still answers the user prompt, but the ferrosa-memory MCP worker
dies during startup, so the server is not actually usable as a Codex
HTTP MCP backend.

## Why it matters

This blocks the intended shared HTTPS deployment model for Codex.

The trust chain is **not** the problem:

- `curl` verifies the presented `mkcert` certificate for `localhost`
- Basic auth succeeds
- the first MCP request (`initialize`) succeeds

So the remaining failure is in the MCP-over-HTTP behavior after
handshake, not TLS, auth, or endpoint reachability.

## Strong hypothesis

Codex sends a follow-up MCP notification after `initialize`:

```json
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

The ferrosa-memory HTTP transport is likely not behaving the way Codex's
HTTP MCP client expects for that second request. The most likely classes
of bug are:

1. **Connection lifecycle bug**
   The server accepts `initialize` then closes the connection, but Codex
   reuses the same keep-alive HTTPS connection for
   `notifications/initialized`.

2. **Malformed second HTTP response**
   The second response body or framing is invalid for the client decoder
   even though the first response is fine.

3. **Incorrect no-id notification handling**
   `notifications/initialized` returns a body shape that Codex's MCP
   HTTP transport rejects.

The current signal strongly suggests (1) or (2): the first request is
good, the failure happens specifically on the post-initialize
notification, and Codex reports a transport/body decode error rather
than an MCP application error.

## Reproduction

### Repro A — endpoint itself looks healthy

```bash
curl -v -u 'codex:***' \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  https://localhost:18765/mcp
```

Expected and observed:

- TLS verifies successfully
- auth succeeds
- `HTTP/1.1 200 OK`
- JSON body contains `serverInfo`, `protocolVersion`, and instructions

### Repro B — Codex-compatible flow fails

```bash
codex exec \
  -c 'mcp_servers.ferrosa-memory.url="https://codex:***@localhost:18765/mcp"' \
  --skip-git-repo-check \
  "Reply with the word ok."
```

Observed:

```text
ERROR rmcp::transport::worker: worker quit with fatal:
Client error: error decoding response body,
when send initialized notification
```

## Minimal test to add

Add an HTTP transport regression test that exercises **two MCP requests
on one authenticated HTTPS connection**:

1. `initialize`
2. `notifications/initialized`

The test should assert that:

- the first response is valid JSON-RPC with `id`
- the second request does not kill the connection unexpectedly
- the second response is either:
  - a valid JSON-RPC success body for the notification path, or
  - no body only if the HTTP contract explicitly allows that and the
    client-side expectation is aligned

Pseudo-shape:

```rust
#[tokio::test]
async fn https_connection_supports_initialize_then_initialized_notification() {
    // Start HTTP transport with TLS + auth enabled
    // Open one client connection
    // POST initialize
    // Read and validate response
    // Reuse same connection
    // POST notifications/initialized
    // Assert the response/body/close behavior matches MCP HTTP expectations
}
```

If the transport is intentionally one-request-per-connection, that must
be made explicit and validated against Codex's actual HTTP MCP client
behavior. Right now it is not interoperable.

## Investigation targets

- `crates/ferrosa-memory-core/src/http.rs`
  - connection loop / keep-alive behavior
  - how many requests are read per TCP/TLS session
  - response framing and shutdown behavior
- `crates/ferrosa-memory-core/src/dispatch.rs`
  - `notifications/initialized` handling currently returns `Value::Null`
  - confirm the resulting HTTP body is encoded the way Codex expects

## Acceptance

- Codex can connect to `https://localhost:18765/mcp` without MCP worker
  startup failure.
- `codex exec -c 'mcp_servers.ferrosa-memory.url="https://...:18765/mcp"' ...`
  starts with ferrosa-memory attached and no
  `send initialized notification` transport error.
- Regression test covers `initialize` followed by
  `notifications/initialized` on the same HTTPS connection.
