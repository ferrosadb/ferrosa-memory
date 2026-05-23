---
title: Streaming audit bugfix checklist
status: in-process
created: 2026-05-23
updated: 2026-05-23
---

# Streaming Audit Bugfix Checklist

Goal: fix the concrete rust-streaming audit findings with tests first, run local CI, then push a PR and monitor GitHub CI until green.

## Runtime Configuration

- [x] Local MCP protocol configuration must match the running server.
  - Evidence today: launchd server listens on plain HTTP `127.0.0.1:18765`, but client config used HTTPS.
  - Verify: `curl http://127.0.0.1:18765/healthz/ready` and authenticated `POST /mcp` both succeed.
  - Local fix: `~/.codex/config.toml` and `~/.mcp.json` both use `http://localhost:18765/mcp`, matching `.runtime/ferrosa-memory-http-18765.toml` where `require_tls = false`.

- [x] Podman MCP healthcheck must probe the actual in-container endpoint.
  - Evidence today: compose probes `http://127.0.0.1:18765/healthz/live`, while the container serves HTTPS on `8765`.
  - Verify: `podman inspect ferrosa-memory-ferrosa-memory-mcp-1` reports `healthy` after recreate.

## P1

- [x] Stdio MCP must reject overlarge JSON-RPC lines without unbounded buffering.
  - Test: an oversized newline-delimited request returns an invalid-request error without invoking the handler.
  - Verify: `cargo test -p ferrosa-memory-core stdio_rejects_overlarge_line`.

- [x] SPARQL passthrough must enforce byte or row caps before full body materialization.
  - Test: fake SPARQL server streams more than `limit` bindings; passthrough stops or rejects at the cap.
  - Verify: `cargo test -p ferrosa-memory-mcp sparql_passthrough_bounds_large_result`.

- [x] CQL paged helpers must not collect unbounded row sets before applying caps.
  - Test: fake paged iterator for a high-cardinality BM25 term enforces candidate cap or streams scoring.
  - Verify: `cargo test -p ferrosa-memory-core cql_paged_helpers_enforce_candidate_cap`.

## P2

- [x] Scoped viz snapshots must use streaming APIs instead of list-session materialization.
  - Test: scoped WebSocket snapshot source rejects `*_list_session` materialization and requires bounded session stream APIs.
  - Verify: `cargo test -p ferrosa-memory-core scoped_viz_snapshot_streams_incrementally`.

- [x] `ReconnectingStorage` must not hold a read lock across stalled streaming sends.
  - Test: stalled stream plus `mark_disconnected` updates state within a short timeout.
  - Verify: `cargo test -p ferrosa-memory-mcp reconnecting_storage_stream_methods_drop_read_lock_before_awaiting_sends`.

- [x] Consolidation queue must be bounded or coalesce deterministically.
  - Test: more unique sessions than queue budget cannot grow memory without bound.
  - Verify: `cargo test -p ferrosa-memory-core consolidation_queue_is_bounded`.

## CI Gate

- [x] `cargo fmt --check`
- [x] `cargo clippy --all-targets -- -D warnings`
- [x] `make test-all`
  - Rust/unit/contract phases passed through `make test-all`.
  - Python phases were run with `uv run --with-requirements tests/requirements.txt ...` because system `python3` lacks `pytest`.
- [ ] PR opened and GitHub CI monitored every 15 minutes until green.
