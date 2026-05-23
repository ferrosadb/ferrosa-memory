---
title: Streaming audit follow-up checklist
status: in-process
created: 2026-05-23
updated: 2026-05-23
---

# Streaming Audit Follow-Up Checklist

Goal: fix the remaining concrete rust-streaming audit findings in ferrosa-memory with TDD, then rerun local CI and push the clean PR update.

## Ferrosa Memory

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

- [ ] Workbench summary should avoid broad expensive scans for counts and should surface degraded count paths clearly.
  - Evidence: live `/workbench/api/summary` returned `status:"not_ready"` with Ferrosa `Bulk lane send timeout` while MCP health was ready.
  - TDD: summary count path should use bounded/aggregate queries and distinguish partial/degraded counters from total endpoint failure.

## Ferrosa Core Streaming Findings

- [ ] Row streaming receiver must not accumulate all mutations in memory before apply.
  - Evidence: `ferrosa-cluster/src/streaming/receiver.rs` stores `mutations: Vec<StreamedMutation>` and extends it per chunk.
  - TDD: oversized or too-many mutation streams are rejected or backpressured before unbounded growth.

- [ ] SSTable streaming must write to staging, verify, then atomically promote.
  - Evidence: stream chunks are written directly under live `sstables/.../{sstable_id}/{component}` before checksum validation.
  - TDD: bad checksum leaves no live SSTable files visible and cleans staged files.

- [ ] Forming-mode DDL queue must be bounded.
  - Evidence: `tokio::sync::mpsc::unbounded_channel` accepts unbounded DDL operations while forming.
  - TDD: queue-full behavior returns an explicit retryable error after the configured capacity.

- [ ] Streaming session maps must cap or expire abandoned sessions.
  - Evidence: `DashMap` session state grows on starts and is only removed on end/error.
  - TDD: many starts without matching end cannot grow session state past the configured cap or timeout.

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
