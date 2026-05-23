---
title: Streaming audit follow-up checklist
status: in-process
created: 2026-05-23
updated: 2026-05-23
---

# Streaming Audit Follow-Up Checklist

Goal: fix the remaining concrete rust-streaming audit findings in impact order with TDD, then rerun local CI and push clean PR updates. Prioritize hot storage paths that can cause memory pressure, orphaned outputs, or data loss before HTTP polish.

## P0 Hot Storage / Data-Loss Paths

- [ ] Compaction promotion must be generation-atomic, not component-by-component live renames.
  - Evidence: `ferrosa-storage/src/engine.rs` renames component files one at a time; a crash after one rename can expose a partial SSTable generation and leave orphaned compaction outputs.
  - TDD: inject failure after first component rename; restart must not discover the partial target generation, and staged outputs must be cleaned or recoverable.

- [ ] SSTable streaming must write to staging, verify, then atomically promote.
  - Evidence: stream chunks are written directly under live `sstables/.../{sstable_id}/{component}` before checksum validation.
  - TDD: bad checksum leaves no live SSTable files visible and cleans staged files.

- [ ] Object-store restore must stream object bodies to staging instead of materializing them and writing live paths directly.
  - Evidence: `ferrosa-storage/src/engine.rs` and `ferrosa-storage/src/restore/manager.rs` use `.bytes().await` and direct final-path writes for SSTable/commit-log restore.
  - TDD: fake large object plus failing writer leaves no live partial component and memory stays bounded.

- [ ] Row streaming receiver must not accumulate all mutations in memory before apply.
  - Evidence: `ferrosa-cluster/src/streaming/receiver.rs` stores `mutations: Vec<StreamedMutation>` and extends it per chunk.
  - TDD: oversized or too-many mutation streams are rejected or backpressured before unbounded growth.

- [ ] Restore/index/compaction schedulers must use bounded queues or explicit backpressure.
  - Evidence: compaction executor, index scheduler, and index builder use unbounded channels or full-object materialization before checking budgets.
  - TDD: blocked workers reject or backpressure submissions beyond configured capacity.

## P1 Ferrosa Memory Hot Paths

- [ ] Batch/sync jobs must not materialize full tenant datasets without caps or streaming.
  - Evidence: `ferrosa-memory-batch` and `ferrosa-memory-sync` call broad list APIs; `memo_list_all()` and `typed_edge_list_all()` still collect all paged rows without named caps.
  - TDD: source/unit tests requiring explicit caps or streaming replacements for memo and typed-edge list-all paths.

## P2 Runtime / Transport Correctness

- [ ] Top-level HTTP request timeout must not cancel composite read/dispatch/write futures mid-operation.
  - Evidence: `serve_one_connection_with_session` wraps `handle_connection_rw` in `tokio::time::timeout`; timeout can drop a partially read request, mutating handler, or response write.
  - TDD: slow body/write test proves timeout happens at safe checkpoints or returns before dispatching mutating work.

- [ ] Forming-mode DDL queue must be bounded.
  - Evidence: `tokio::sync::mpsc::unbounded_channel` accepts unbounded DDL operations while forming.
  - TDD: queue-full behavior returns an explicit retryable error after the configured capacity.

- [ ] Streaming session maps must cap or expire abandoned sessions.
  - Evidence: `DashMap` session state grows on starts and is only removed on end/error.
  - TDD: many starts without matching end cannot grow session state past the configured cap or timeout.

- [ ] RPC lane timeouts must not cancel non-cancel-safe sends with leaked pending stream IDs.
  - Evidence: `RpcClient::send` documents non-cancel-safety while lane actor wraps it in `tokio::time::timeout`.
  - TDD: force timeout while frame send is blocked; pending response slots are removed and stream IDs do not leak.

## Completed Ferrosa Memory Fixes

- [x] Workbench list endpoints must not convert storage/backpressure errors into empty successful responses.
  - Evidence: `/workbench/api/approvals` and `/workbench/api/aliases` used `entity_list_all(...).unwrap_or_default()`.
  - Fix: propagate list-scan failures as explicit operator errors.
  - Verify: `cargo test -p ferrosa-memory-core workbench_list_endpoints_propagate_entity_scan_errors`.

- [x] Viz facts endpoint must apply `limit` at the storage boundary instead of loading a full derived-cache partition.
  - Evidence: `/viz/facts` calls `derived_cache_get` and truncates only after materialization.
  - Fix: `/viz/api/derived_facts` now calls `derived_cache_get_limited`; CQL pushes `LIMIT` into the derived-cache query and the viz route preserves query params after route matching.
  - Verify: `cargo test -p ferrosa-memory-core viz_derived_facts_applies_limit_at_storage_boundary` and `cargo test -p ferrosa-memory-core derived_cache_limited_query_pushes_limit_to_cql`.

- [x] List-all storage helpers must avoid unpaged full-result materialization or enforce explicit caps.
  - Evidence: `exec_prepared_rows` backs several `*_list_all` paths with `execute_unpaged`.
  - Fix: `temporal_list_all`, `feedback_list_all`, and `intention_list_all` now use driver paged iteration with explicit row caps and actionable errors.
  - Verify: `cargo test -p ferrosa-memory-core cql_secondary_list_all_apis_use_paged_iterators_with_explicit_caps`.

- [x] `list_entities` must apply caller `limit` during CQL paging, not after materializing a tenant-wide candidate set.
  - Evidence: default `entity_list_matching` called `entity_list_all()` for `scope=all`, then sorted/filtered/took the limit in memory.
  - Fix: CQL storage overrides `entity_list_matching`, pages through scoped rows, keeps only a bounded top result set, and fails closed after the named broad-scan cap.
  - Verify: `cargo test -p ferrosa-memory-core cql_entity_list_matching_streams_and_applies_limit_during_scan`.

- [x] Viz derived facts limit must be bounded before reaching storage.
  - Evidence: `/viz/api/derived_facts?limit=999999999` passed the raw caller limit into `derived_cache_get_limited`.
  - Fix: clamp to `VIZ_DERIVED_FACTS_MAX_LIMIT`.
  - Verify: `cargo test -p ferrosa-memory-core viz_derived_facts_clamps_large_limit_before_storage_call`.

- [x] Workbench summary should avoid broad expensive scans for counts and should surface degraded count paths clearly.
  - Evidence: live `/workbench/api/summary` returned `status:"not_ready"` with Ferrosa `Bulk lane send timeout` while MCP health was ready.
  - Fix: summary now stream-counts entities, approval mirrors, and derived-cache rows exactly; it returns `not_ready` with the storage error instead of silently returning capped or degraded counts.
  - Verify: `cargo test -p ferrosa-memory-core workbench_summary_avoids_broad_entity_and_derived_cache_scans`, `cargo test -p ferrosa-memory-core workbench_summary_reports_ready_when_storage_queries_succeed`, and `cargo test -p ferrosa-memory-core cql_count_apis_stream_without_materializing_rows`.

## MCP Runtime Diagnosis

- [x] Confirm the client-side MCP config uses credentials matching `.runtime/http-auth.toml`.
  - Evidence: direct HTTP JSON-RPC succeeds with the `codex` header from `~/.codex/config.toml`; a wrong `codex:codex` header returns `unauthorized`.
  - Verify: authenticated `check_intentions` against `http://127.0.0.1:18765/mcp`.

- [x] MCP stdio/HTTP startup must not block `initialize` on backend handshakes.
  - Evidence: startup previously awaited graph health, admin migration CQL, runtime CQL, and embedding health before serving transports.
  - Fix: startup now creates reconnecting storage immediately; the reconnect worker runs migrations before runtime CQL connect, graph client construction avoids a startup health probe, and embedding health runs in a background task.
  - Verify: `cargo test -p ferrosa-memory-mcp startup_main_does_not_await_backend_connects_before_serving` and `cargo test -p ferrosa-memory-mcp reconnect_watcher_runs_migrations_before_runtime_cql_connect`; manual offline stdio initialize returns immediately when stderr is drained.

- [x] MCP `tools/call` must not overflow tokio worker stacks on the first request.
  - Evidence: launchd restarted `ferrosa-memory-mcp` with `thread 'tokio-rt-worker' has overflowed its stack` before any tool handler log; foreground debug reproduced this with `check_intentions`.
  - Fix: `dispatch_tool` now boxes the selected handler future instead of embedding every handler future in one large match; reconnecting storage keeps `Arc<CqlStorage>` handles; best-effort telemetry avoids the generic connection-error formatting path.
  - Verify: `cargo test -p ferrosa-memory-core tool_dispatch_boxes_selected_handler_future`; launchd `check_intentions` returns HTTP 200 in about 10ms after restart.

- [ ] Investigate the repeated 30s request timeouts in `/tmp/ferrosa-memory-mcp.log`.
  - Evidence: server has been listening and health-ready, but logs repeated `request exceeded 30s`.
  - Current lead: broad workbench or query paths can exceed the transport budget when Ferrosa index reads time out.
