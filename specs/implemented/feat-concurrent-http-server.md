---
type: feat
priority: P1
status: implemented
created: 2026-04-19
updated: 2026-04-20
reported-by: live HTTPS wedge debugging session (2026-04-19)
---

## Implementation Notes

- `Storage` trait methods all use `fn foo(...) -> impl Future<Output = ...> + Send`.
  Tried `trait-variant::make` first; reverted because the paired `LocalStorage`
  it generates collides with `Storage` at call sites (`multiple applicable items
  in scope`). Manual `+ Send` on all 170 signatures is the spec's conservative
  option.
- `serve_http` spawns per connection. Accept loop only rate-limits and hands off;
  TLS handshake + request handling run under `REQUEST_BUDGET = 30s` inside the
  spawned task. Timeout emits HTTP 504.
- New `read_http_request(stream, max_bytes)` loops reads until headers (`\r\n\r\n`)
  and `Content-Length` body are both present. Fixes Python `http.client`'s
  two-`sendall` split-write request that was closing without a response.
- `serve_viz` accept loop collapsed to accept + spawn. `POST /consolidate`,
  `GET /viz/api/enrich/models`, `?session=` parsing, and snapshot build all moved
  inside `handle_viz_connection`. Pre-spawn peek and snapshot carve-out deleted.
- Tests: 3 `read_http_request_*` unit tests + 6 integration tests in
  `tests/http_concurrency.rs` (stalled-client, split-write, no-shutdown
  Python-style, real `python3 http.client` end-to-end, rate-limit no-reset,
  40 parallel) + 3 cancellation-safety tests in
  `tests/ingest_skill_cancellation.rs`. Full workspace green (612 lib +
  27 mcp + 6 concurrency + 3 cancellation).
- Rate-limit path reshaped from `drop(stream)` to a spawned task that writes
  HTTP 429 + `Retry-After: 60`, half-closes, drains any pending request bytes
  from the recv buffer, then drops. Prior `drop(stream)` with request bytes
  still buffered made macOS emit RST on close, which Python's `http.client`
  surfaces as `ConnectionResetError`. The earlier `split_write` integration
  test masked this by calling `stream.shutdown()` on the client side; the new
  `python_style_post_without_client_shutdown_gets_response` test drops that
  shortcut so the rate-limit close path is actually exercised.
- Cancellation safety relies on the `TAGGED_AS` decoupling from
  `bug-ingest-skill-cluster-tag-dropped`: tag edge writes use a deterministic
  UUIDv5 (tenant + normalized name), so a retry after mid-flight cancellation
  emits the same edge identity. Partial state + retry collapses onto the same
  rows instead of creating parallel ones; the double-run idempotency test
  makes that the tested invariant.
- Deferred: `RateLimiter` contention audit under spawn, `concurrency_scan`
  sweep. Neither blocks acceptance.

# Concurrent HTTP connection handling via `tokio::spawn`

## Observed

`serve_http`'s accept loop in `crates/ferrosa-memory-core/src/http.rs`
handles connections **sequentially**:

```rust
loop {
    let (stream, peer) = listener.accept().await?;
    // ... rate limit check ...
    if let Some(ref acceptor) = tls_acceptor {
        match accept_tls_with_budget(acceptor, stream, TLS_ACCEPT_BUDGET).await {
            Ok(mut tls_stream) => {
                if let Err(e) = handle_connection_rw(&mut tls_stream, ...).await {
                    tracing::warn!(...);
                }
            }
            Err(e) => { tracing::warn!(...); }
        }
    }
}
```

The code comment explains why:

> connections are handled sequentially (no `tokio::spawn`) because the
> `Storage` trait's async methods aren't `Send`-bounded. This is fine for
> the expected low connection rate of MCP clients.

Two live incidents disprove that "fine":

1. **2026-04-19 afternoon wedge.** A client opened TCP on `:8765` but
   never sent ClientHello. `acceptor.accept(stream).await` blocked
   forever; every subsequent client — curl probes, Claude Code, viz —
   timed out at the TCP-accept backlog. Fixed narrowly with
   `TLS_ACCEPT_BUDGET = 10s` (`run_with_budget` helper added in
   `http.rs`) — bounded the TLS handshake but not the handler.

2. **2026-04-19 evening re-wedge.** After the TLS fix shipped, PID
   76468 still went unresponsive. TLS handshakes succeeded, but
   `handle_connection_rw` (which does storage reads under
   CQL-reconnect conditions) blocked the accept loop on a single slow
   request. Same failure mode, deeper in the stack, not addressed by
   the handshake-only timeout.

The sequential model is also the reason the viz server needs a
`build_snapshot`-only-when-needed carve-out (see
`specs/implemented/bug-ingest-skill-cluster-tag-dropped.md` and
related http.rs hoists) — the viz accept loop has the same
constraint.

## Why the constraint is self-imposed

`tokio::spawn` requires its future to be `Send`. The `Storage` trait
declares most methods with `async fn`, which in trait position does
**not** add a `Send` bound on the associated future — so callers
can't spawn storage-touching work.

But every concrete `Storage` impl in this repo produces `Send`
futures in practice:

- `CqlStorage` wraps `cdrs_tokio::Session` (`Send + Sync`).
- `ReconnectingStorage` wraps `RwLock<Option<CqlStorage>>`.
- `MockStorage` uses `tokio::sync::Mutex` throughout.

A few trait methods already use the explicit form
`fn foo(...) -> impl Future<Output = ...> + Send` (e.g.,
`entity_get_by_id`, `entity_get_batch`); most don't. Converting the
rest would unlock spawning everywhere.

## Why this matters

- **Any slow storage call stalls every client.** CQL reconnect
  storms, slow `typed_edge_list_all` scans, Raft leader election
  pauses — each of these today is a DoS for every concurrent MCP
  client, browser hitting viz, or health probe.
- **The TLS handshake timeout only narrows one window.** Request
  handling is still unbounded; a single `retrieve_entities` against
  a large partition can hold the accept loop for seconds.
- **Special-case carve-outs are proliferating.** `POST /consolidate`
  is inline because its future isn't `Send`. Viz snapshot is built
  on the accept task because storage futures aren't `Send`. Every
  future feature that touches storage inherits the constraint.
- **Actor/isolation story is weak.** Today one bad connection can
  corrupt the accept-loop task's state (shared rate limiter, etc.)
  because everything runs on one task. A per-connection task gives
  us the Tokio equivalent of Erlang-style process isolation at
  essentially no cost.

## Proposed change

1. **Add `Send` bound to every `async fn` on the `Storage` trait.**
   Convert declarations from
   ```rust
   async fn entity_put(&self, ctx: &TenantContext, entry: &EntityEntry) -> anyhow::Result<()>;
   ```
   to
   ```rust
   fn entity_put<'a>(&'a self, ctx: &'a TenantContext, entry: &'a EntityEntry)
       -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a;
   ```
   or use the newer `async fn` + `#[trait_variant::make(... + Send)]`
   macro once it's stable. Every existing impl already satisfies `Send`
   in practice, so no impl bodies change.

2. **Rewrite the accept loop to spawn per connection.**
   ```rust
   loop {
       let (stream, peer) = listener.accept().await?;
       if !rate_limiter.check(peer.ip()) { continue; }
       let storage = Arc::clone(&storage);
       let metrics = Arc::clone(&metrics);
       let validator = Arc::clone(&credential_validator);
       let readiness = config.readiness_checker.clone();
       let acceptor = tls_acceptor.clone();
       tokio::spawn(async move {
           if let Err(e) = serve_one(stream, peer, acceptor, storage,
                                     metrics, validator, readiness).await {
               tracing::warn!(peer = %peer, error = %e, "connection error");
           }
       });
   }
   ```

3. **Wrap the per-request handler in a deadline.**
   `tokio::time::timeout(REQUEST_BUDGET, handle_connection_rw(...))` —
   defense in depth alongside the TLS handshake timeout. On timeout,
   return HTTP 504 Gateway Timeout so the client can distinguish
   "we took too long" from "TLS broke".

4. **Remove the inline `POST /consolidate` branch.** Once storage
   is `Send`, consolidation can run in the spawned connection task
   like everything else.

5. **Apply the same treatment to `serve_viz`.** Its accept loop has
   the identical shape and the same constraint. Delete the
   "`build_snapshot` only when the path needs it" carve-out — with
   per-connection tasks, every path can build its own snapshot
   lazily without blocking the accept loop.

6. **Audit hot-path `Mutex<HashMap<...>>` call sites.** Not part of
   the core fix, but once per-connection tasks can contend on shared
   state, read-heavy maps should move to `DashMap` or arc-swapped
   snapshots. Track which ones matter with `concurrency_scan` or
   by instrumenting contention counters first — no speculative
   rewrites.

## Acceptance

- With a stalled TLS client holding an accept slot, a concurrent
  curl against `POST /mcp` completes within 100ms (currently: times
  out).
- With a 30s-blocking storage call in flight, a concurrent curl
  against `POST /mcp` completes within `REQUEST_BUDGET + 100ms`
  (currently: times out).
- `cargo test --workspace` still green (Storage impls are unchanged;
  trait signatures add `Send` but impls already satisfy it).
- No remaining `inline` / `not Send` carve-outs in `serve_http` or
  `serve_viz`. `POST /consolidate` + viz snapshot both run in
  spawned tasks.
- Concurrency smoke test: 100 parallel curl clients against `/mcp`
  complete in `< 2s total` on a healthy cluster (currently:
  sequential, ~100 × per-request latency).

## Risks / follow-ups

- **Cancellation.** Dropping a spawned task mid-request drops its
  storage calls at their `.await` points. Individual CQL writes are
  atomic, but the `ingest_skill` tag-edge loop (now resilient to
  partial failures per `bug-ingest-skill-cluster-tag-dropped`) needs
  a re-read to confirm it handles being cut off between the skill
  entity_put and the TAGGED_AS edges. Add a test.
- **Rate limiter.** `RateLimiter` today is per-IP with a shared
  map. Under concurrent spawned tasks it needs either a `Mutex`
  wrapper or a lock-free bucket. `governor` crate is the standard
  pick.
- **Trait-variant macro vs. manual `impl Future + Send`.** Manual
  form is ugly but stable. The `trait_variant` macro is cleaner but
  requires a dep. Either works; manual is the conservative pick.
- **Unknown downstream.** Any caller that `.await`s a Storage method
  while holding a `!Send` value (e.g., `std::cell::RefCell`, a
  non-Send guard) will break at compile time. Unlikely in this
  codebase but worth a grep before committing.

## Related

- `specs/implemented/bug-ingest-skill-cluster-tag-dropped.md` — the
  TAGGED_AS decoupling that made bulk ingest resilient; its
  "the tag edge survives partial failures" property is what makes
  connection-level cancellation safe here.
- `http.rs` ~140-170 — the existing doc comment acknowledging the
  constraint. Remove that comment as part of this work.
- `scripts/memory-watchdog.sh` — separate resilience layer that
  kills and restarts wedged instances. Keep it as belt-and-
  suspenders but this work should make its restarts rare.
