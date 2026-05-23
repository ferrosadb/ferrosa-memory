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
  - Local fix: `~/.codex/config.toml` uses `http://127.0.0.1:18765/mcp` with explicit `Authorization` header because this Codex build reports URL-embedded Basic credentials as `Unsupported`; `~/.mcp.json` and Claude both connect over HTTP. This matches `.runtime/ferrosa-memory-http-18765.toml` where `require_tls = false`.

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

- [x] `ReconnectingStorage` generic delegates and operator CQL passthrough must not hold the reconnect read lock across awaited CQL calls.
  - Evidence: the first streaming fix covered viz chunk sends, but the delegate macro, `feedback_list_all`, and operator CQL passthrough still used `inner.read().await` around awaited backend work.
  - Tests: static regression tests assert the delegate macro and operator passthrough clone `current_cql()` before awaited backend calls.
  - Verify: `cargo test -p ferrosa-memory-mcp reconnecting_storage` and `cargo test -p ferrosa-memory-mcp operator_cql_passthrough_does_not_hold_read_lock_while_streaming_rows`.

- [x] Consolidation queue must be bounded or coalesce deterministically.
  - Test: more unique sessions than queue budget cannot grow memory without bound.
  - Verify: `cargo test -p ferrosa-memory-core consolidation_queue_is_bounded`.

## Remaining Follow-Up

- [x] CQL tenant/session entity listing still has unbounded list APIs for non-viz call sites: `entity_list_all` and `entity_list_session`.
  - Fix: both methods now collect from `execute_iter` paging and fail clearly after `CQL_ENTITY_LIST_MAX_ROWS` instead of issuing one unpaged result materialization.
  - Verify: `cargo test -p ferrosa-memory-core cql_entity_list_apis_use_paged_iterators_with_explicit_cap`.
- [x] `fold_list_all` still materializes full fold payloads, including large trajectory/embedding columns, for non-viz callers.
  - Fix: `fold_list_all` now pages through CQL rows and fails clearly after `CQL_FOLD_LIST_MAX_ROWS`; full-fidelity payload columns are preserved for callers that need backup/sync semantics.
  - Verify: `cargo test -p ferrosa-memory-core cql_fold_list_all_uses_paged_iterator_with_explicit_cap`.
- [x] Legacy `edge_list_session` and `edge_list_all` still materialize/filter broad reads and can hide per-table failures.
  - Fix: legacy edge list APIs now use scoped paged iteration, fail closed on table-level query errors, and stop with a clear error after `CQL_LEGACY_EDGE_LIST_MAX_ROWS`.
  - Verify: `cargo test -p ferrosa-memory-core cql_legacy_edge_list_apis_are_scoped_paged_and_fail_closed`.
- [x] Streaming row decode errors should be surfaced to stream consumers instead of only logged for `fold_stream_all`, `typed_edge_stream_all`, and `typed_edge_stream_session`.
  - Fix: stream producers now flush any partial chunk, send `Err(e)` to the receiver, and stop on malformed rows.
  - Verify: `cargo test -p ferrosa-memory-core cql_stream_decode_errors_are_sent_to_consumers`.
- [x] SPARQL passthrough is byte-capped, but still parses all bindings within the cap before applying `limit`.
  - Fix: the SPARQL JSON parser now uses `DeserializeSeed` to retain only `limit` bindings while counting skipped rows with `IgnoredAny`; the response byte cap still applies.
  - Verify: `cargo test -p ferrosa-memory-mcp sparql_result_parser_keeps_only_limit_bindings`.
- [ ] Batch update/delete paths are bounded but still serial; evaluate concurrency/backpressure for large operator batches.

## CI Gate

- [x] `cargo fmt --check`
- [x] `cargo clippy --all-targets -- -D warnings`
- [x] `make test-all`
  - Rust/unit/contract phases passed through `make test-all`.
  - Python phases were run with `uv run --with-requirements tests/requirements.txt ...` because system `python3` lacks `pytest`.
- [x] PR opened and GitHub CI monitored until green.
  - PR: https://github.com/ferrosadb/ferrosa-memory/pull/33
  - Final GitHub result: Format & Lint, Build, Tests & Coverage, Cluster integration tests, Dependency Advisories, Complexity Analysis, Generate Docs, Blueprint Harness Smoke, Verify SHA pins, and CI Pass all succeeded for commit `5db7840`.
