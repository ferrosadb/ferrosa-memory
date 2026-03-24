# Project Plan — ferrosa-memory-mcp

> Last updated: 2026-03-23
> Status: Sprints 1-4 complete, vector column and graph edge blockers resolved

## Overview

4 sprints (2 weeks each) + backlog. Prioritized by risk: FMEA RPN scores and STRIDE threat ratings determine sprint ordering.

## Progress Summary

| Sprint | Status | Completion |
|--------|--------|------------|
| Sprint 1 | **COMPLETE** | 14/14 tasks done |
| Sprint 2 | **COMPLETE** | 8/8 tasks done |
| Sprint 3 | **COMPLETE** | 10/10 tasks done |
| Sprint 4 | **COMPLETE** | 11/11 tasks done |

---

## Sprint 1: Foundation + Core Memoization

**Goal:** Working MCP server with stdio transport, tenant auth, memo cache, and plan state tools. Covers critical security invariants from day 1.

**Status: COMPLETE** (all tasks verified in commits 24cf28b through cbe7a34)

| # | Task | Size | Source | Success Criteria | Tests |
|---|------|------|--------|-----------------|-------|
| 1.1 | Cargo workspace setup: `ferrosa-memory-mcp` binary + `ferrosa-memory-batch` binary + shared `ferrosa-core` lib | S | architect | `cargo build` succeeds for both targets | `cargo build --workspace` |
| 1.2 | `config` module: parse `ferrosa-memory.toml` via `serde` + `toml` | S | architect | All config fields from spec Section 10 deserialized. Invalid config returns clear error. | Unit tests: valid config, missing fields, invalid values |
| 1.3 | `metrics` module: Prometheus counters/histograms for all 8 metrics in spec Section 9.1 | S | architect, FMEA F01 | Metrics registered. `GET /metrics` returns Prometheus text format. | Unit: metric registration. Integration: increment + scrape. |
| 1.4 | `auth` module: extract `TenantContext` from stdio (process owner) and HTTP Basic | S | STRIDE S1, S2 | `tenant_id` is never client-supplied. Type system enforces `TenantContext` as required param. | TC22 (TLS rejection), unit: auth extraction |
| 1.5 | `cql_client` module: connection pool, prepared statement cache, parameterized queries | M | DSM (86% propagation), FMEA F01-F05, STRIDE T4 | Pool configurable. All queries parameterized. Reconnect on node failure. | TC09 (pool exhaustion), TC16 (schema change), TC17 (timeout) |
| 1.6 | `Storage` trait: abstract CQL operations for testability | S | DSM recommendation | All tool modules depend on trait, not concrete client. Mock implementation for unit tests. | Compiles with mock backend |
| 1.7 | `transport` module: stdio JSON-RPC framing | M | architect, FMEA F29 | Reads stdin, writes stdout. Handles malformed JSON-RPC without panic. | TC18 (malformed JSON), TC29 (fuzz) |
| 1.8 | `tool_dispatch` module: registry, schema validation, dispatch | S | architect | `tools/list` returns all 12 tool schemas. Invalid params rejected before dispatch. | Unit: schema validation, unknown tool name |
| 1.9 | `embedding_client` module: Ollama HTTP client with timeout and health check | S | FMEA F25 | `embed(text)` returns `Vec<f32>` of configured dimensions. Timeout after 10s. | TC21 (Ollama down), unit: response parsing |
| 1.10 | `memo_tools`: `check_memo_cache` + `store_memo_result` | M | spec Phase 1, FMEA F09-F12 | Cache hit returns stored result. Cache miss returns `{ hit: false }`. Model version in cache key. TTL respected. | TC10 (model version), TC19 (thundering herd), TC20 (TTL expiry) |
| 1.11 | `plan_tools`: `write_plan_node` + `get_plan_context` + `update_plan_node` | M | spec Phase 1 | Plan tree queryable by depth. Status transitions work. Active path computed correctly. | Unit: tree construction, depth filtering, status transitions |
| 1.12 | DDL scripts for `memo_cache` and `plan_state` tables | S | spec Section 4.1-4.2 | Tables created in `agent_memory` keyspace. Indexes created. | DDL executes without error on Ferrosa |
| 1.13 | Claude Code integration: `~/.claude/settings.json` example config | S | spec Section 3.2 | Documented example. `claude-code` can discover and call tools. | Manual: Claude Code connects, `tools/list` returns tools |
| 1.14 | Integration test: RLM memo round-trip | M | spec Phase 1 | Store memo -> check memo -> hit. Different model version -> miss. | End-to-end with real Ferrosa |

**Sprint 1 exit criteria:** `cargo test --workspace` passes. MCP server starts, Claude Code connects via stdio, memo cache and plan state tools functional against Ferrosa. **MET** — live CQL persistence confirmed (cbe7a34).

---

## Sprint 2: Fold Hierarchy + Compression

**Goal:** Trajectory fold lifecycle with graph edges, Rust-native compression, and S3 tiering.

**Status: COMPLETE** — compression engine, fold lifecycle, fold search, graph edge creation, and S3 lifecycle all done.

| # | Task | Size | Source | Success Criteria | Tests |
|---|------|------|--------|-----------------|-------|
| 2.1 | `compression` module: token-importance-weighted compression in pure Rust | L | ADR-001, FMEA F27 | `compress(text, ratio)` produces readable output. `decompress(compress(x)) == x`. Ratio < 1.0 for text > threshold. | TC24 (round-trip property test), unit: various text types |
| 2.2 | DDL for `trajectory_folds` table + HNSW index on `fold_embedding` | S | spec Section 4.3 | Table and index created. Vector search returns ordered results. | DDL execution + sample vector query |
| 2.3 | `graph_client` module: Cypher query execution, vertex/edge creation | M | architect, STRIDE T5 | Parameterized Cypher only. `FOLDED_INTO` edge type works. Timeout configurable. | TC07 (orphan detection), unit: parameterized queries |
| 2.4 | `fold_tools`: `start_fold` + `append_to_fold` + `complete_fold` | L | spec Phase 2, FMEA F13-F15 | Fold lifecycle: active -> folded. Graph edge created on complete. Append rejected on non-active fold. Token count tracked. | TC07 (partial write), TC15 (status check), integration: full lifecycle |
| 2.5 | `fold_tools`: `retrieve_fold_context` with HNSW ANN search | M | spec Phase 2, FMEA F17 | Top-k fold summaries by embedding similarity. Relevance scores returned. `include_raw` works for hot folds, errors for archived. | TC06 (relevance), TC13 (archived fold error) |
| 2.6 | Background compression job: compress `raw_trajectory` on fold completion | M | spec Section 4.3, FMEA F14 | Compression runs async after `complete_fold`. Failure doesn't block fold completion. Checksum stored. | Integration: fold -> verify compressed within timeout |
| 2.7 | S3 lifecycle configuration for `status='archived'` rows | S | spec Section 4.3 | Documented `ferrosa-ctl` commands for Glacier lifecycle rule. Lifecycle triggers after 30 days. | Manual verification with Ferrosa tiering config |
| 2.8 | Integration test: fold hierarchy with nested folds and graph traversal | M | spec Phase 2 | Create 3-level fold tree. Cypher traversal returns correct hierarchy. Fold summaries retrievable by ANN. | End-to-end with Ferrosa |

**Sprint 2 exit criteria:** Fold lifecycle works end-to-end. Compression produces valid output. Graph edges queryable via Cypher. `retrieve_fold_context` returns ranked results. **MET** — graph edge creation implemented (ca207aa), S3 lifecycle documented (1a953b5), vector column gap resolved (e5c9a27).

---

## Sprint 3: Entity Graph + Temporal Events + Feedback

**Goal:** Entity discovery with phonetic dedup, temporal fact chains, and feedback recording for routing optimization.

**Status: COMPLETE** — phonetic entity matching, temporal chains, feedback recording, ANN entity search, graph edges, anomaly detection, and audit log all working.

| # | Task | Size | Source | Success Criteria | Tests |
|---|------|------|--------|-----------------|-------|
| 3.1 | DDL for `entity_store` + phonetic index + HNSW index | S | spec Section 4.4 | Double Metaphone index functional. Vector search functional. Both queryable in same table. | DDL + sample phonetic + vector queries |
| 3.2 | `entity_tools`: `upsert_entity` with phonetic dedup + embedding distance check | L | spec Phase 3, FMEA F18-F19 | Phonetic match found -> check embedding distance -> merge or create new. Confidence gating rejects < threshold. Rate limit per session. | TC01 (confidence gate), TC02 (rate limit), TC04/TC05 (false merge prevention) |
| 3.3 | `entity_tools`: `retrieve_entities` with `ann`, `phonetic`, `both` strategies | M | spec Phase 3 | Union-merge deduplicates by `entity_id`. Each strategy returns correct result type. | Unit: each strategy. Integration: `both` merges correctly. |
| 3.4 | Graph annotations: `CO_OCCURS_WITH`, `MENTIONED_IN` edge types for entities | M | spec Section 4.4 | Entity vertices created. Edges created on upsert. Cypher multi-hop queries work. | Integration: create entities, query 2-hop relationship |
| 3.5 | DDL for `temporal_events` + supersession logic | M | spec Section 4.5, FMEA F21-F22 | `valid_until` set atomically on superseded fact. `SUPERSEDES` graph edge created. Batch read-invalidate-write is atomic. | TC11 (supersession), TC23 (duplicate detection) |
| 3.6 | `feedback_tools`: `record_outcome` (write-only) | S | spec Phase 3, STRIDE E2 | Writes to `feedback_outcomes`. No read path via MCP. | Unit: write succeeds. Attempt read via MCP -> rejected. |
| 3.7 | DDL for `feedback_outcomes` table | S | spec Section 4.6 | Table created. Batch job can query with separate credentials. | DDL + sample query |
| 3.8 | Anomaly detection: materialized view on entity retrieval frequency | M | STRIDE T1, FMEA F19 | Entities retrieved at >3σ from session baseline flagged in `system_observability`. Metric emitted. | TC03 (anomaly flag) |
| 3.9 | Audit log: append-only write log for entity, fold, and memo writes | M | STRIDE R1 | All writes emit audit row. Audit rows not deletable via MCP tools. | Integration: write entity -> verify audit row. Attempt delete -> rejected. |
| 3.10 | Integration test: entity discovery + temporal chain + graph traversal | M | spec Phase 3 | Discover entities from fold context. Create temporal facts. Traverse entity graph via Cypher. | End-to-end with Ferrosa |

**Sprint 3 exit criteria:** Entity upsert with phonetic dedup works. Temporal chains maintain integrity. Feedback recording functional. Anomaly detection and audit log operational. **MET** — ANN strategy fixed via vector column support (e5c9a27), graph edges implemented (ca207aa), anomaly detection added (2226409), audit log persistence implemented (12c0a4a).

---

## Sprint 4: Routing Layer + HTTP Transport + Security Hardening

**Goal:** SRLM-inspired routing, HTTP+SSE transport, and all security mitigations from threat model.

**Status: COMPLETE** — routing layer, HTTP+TLS, batch job, session deletion, quotas, security hardening, integration tests, and anomaly alert subscription all done.

| # | Task | Size | Source | Success Criteria | Tests |
|---|------|------|--------|-----------------|-------|
| 4.1 | `tool_router`: implement all 6 strategy paths with decision tree | L | spec Section 6, FMEA F06-F08 | Router selects correct strategy for each query type. Default fallback works. < 1ms overhead. | TC08 (routing accuracy), benchmark: latency |
| 4.2 | `tool_router`: routing signal computation (embedding similarity, complexity, entity presence) | M | spec Section 6.2 | Signals computed from request context. Strategy selection adapts based on signals. | Unit: each signal type. Integration: signal-driven selection. |
| 4.3 | `ferrosa-memory-batch`: nightly guideline refinement job | L | ADR-002, spec Section 6.3 | Reads failure pairs from `feedback_outcomes`. Computes strategy accuracy. Writes updated `routing_guidelines`. | Integration: seed failures -> run batch -> verify updated guidelines |
| 4.4 | `transport`: HTTP+SSE mode with TLS | L | spec Section 3.2, FMEA F30, STRIDE S1 | HTTP listener, SSE streaming, TLS required, connection timeout, idle cleanup. | TC15 (SSE leak), TC22 (TLS), TC30 (connection limit) |
| 4.5 | Fractured CoT axis defaults per task complexity | S | spec Section 6.2 | `k` and `include_raw` defaults scaled to `task_complexity`. | Unit: defaults for simple/linear/quadratic |
| 4.6 | Right-to-deletion: `DELETE WHERE session_id AND tenant_id` cascade | M | spec Section 7.4 | Deletes all memory objects (memo, plan, fold, entity, temporal, feedback) for a session. | Integration: populate all tables -> cascade delete -> verify empty |
| 4.7 | Per-tenant storage quotas | M | FMEA D1 | Reject writes when tenant exceeds configured storage limit. Clear error message. | Integration: write to quota -> verify rejection |
| 4.8 | `system_observability.memory_summary` virtual table | M | spec Section 9.2 | Per-tenant summary queryable via CQL: hit rate, fold count, entity count, cost savings. | Integration: populate data -> query summary -> verify accuracy |
| 4.9 | `SUBSCRIBE` integration for anomaly alerts | S | spec Section 9.3 | Real-time stream of anomaly flags via Ferrosa SUBSCRIBE. | Integration: trigger anomaly -> verify event on stream |
| 4.10 | Security hardening sweep: verify all mitigations from threat model | M | STRIDE all | Checklist verification of all Critical and High threat mitigations. | Run TC01-TC24 as regression suite |
| 4.11 | Integration test: full routing + feedback + guideline refresh cycle | L | spec Phase 4 | Query -> route -> retrieve -> record outcome -> batch job -> updated routing. Full loop. | End-to-end with Ferrosa |

**Sprint 4 exit criteria:** HTTP+SSE transport functional with TLS. Router selects strategies with >80% accuracy on test workload. All Critical/High threat mitigations verified. Batch job produces routing guidelines. **MET** — all tasks complete. SUBSCRIBE integration (4.9) implemented via EventBus + SSE endpoint (Ferrosa native SUBSCRIBE deferred to backlog).

---

## Backlog (Post-v1.0)

| # | Task | Size | Source | Notes |
|---|------|------|--------|-------|
| B1 | OpenTelemetry trace export | M | spec Section 11 | v2 item per spec |
| B2 | Model-perplexity-based compression scoring | L | ADR-001 | Enhances compression quality by calling embedding endpoint for perplexity estimates |
| B3 | Embedding migration tool (re-embed on model change) | M | FMEA F26 | Semi-automated: enumerate rows with old model, re-embed, update |
| B4 | Online RL training loop for routing guidelines | XL | spec Section 11 | Replaces nightly batch with online ACON-style RL |
| B5 | Web dashboard for memory observability | L | spec Section 11 | Beyond Ferrosa console + Prometheus |
| B6 | WASM UDF for in-database compression | M | spec Section 4.3 | Moves compression into Ferrosa SSTable flush path |
| B7 | `INSERT IF NOT EXISTS` (LWT) for thundering herd | S | FMEA F11 | Blocked on Ferrosa LWT support |
| B8 | Native row TTL in Ferrosa | S | spec Section 8.3 | Replace application-managed TTL sweep if Ferrosa adds native TTL |

---

## Risk Register

| Risk | Likelihood | Impact | Mitigation | Status |
|------|-----------|--------|------------|--------|
| **cdrs-tokio v9 lacks vector column type** (FMEA F31, RPN 180) | **Confirmed** | **Critical** | Fixed via custom cdrs-tokio fork with vector column support (e5c9a27). Vector round-trip working end-to-end. | **Resolved** |
| **Graph edge creation not implemented** (FMEA F32, RPN 140) | **Confirmed** | **High** | Implemented in ca207aa. Edges written via CQL INSERTs into graph-annotated tables, reads via HTTP Cypher. | **Resolved** |
| Ferrosa CQL driver compatibility issues | Medium | High | Custom cdrs-tokio fork with vector support working. All CQL operations functional including vector columns. | **Resolved** |
| Ferrosa graph layer requires Cypher (not CQL annotations) | Medium | Medium | Validated: writes go through CQL graph-annotated tables, reads through HTTP Cypher. neo4rs replaced with HTTP client (de901d1). | **Resolved** |
| Ollama embedding latency too high for interactive use | Low | Medium | Embedding client working with 10s timeout. Latency acceptable in dev testing. | **Resolved** |
| Ferrosa WASM UDF I/O limits prevent compression UDF | Medium | Low | ADR-001 already chose Rust-native compression in-process. WASM UDF is backlog (B6). | Mitigated |
| No native row TTL in Ferrosa beta | Medium | Low | Application-managed TTL sweep job as fallback (Sprint 1 task 1.10) | Mitigated |
| Compression quality insufficient without model inference | Low | Medium | ADR-001 accepts this tradeoff for v1. Backlog B2 adds perplexity-based scoring. | Accepted |

---

## Dependencies

```mermaid
graph LR
    S1[Sprint 1] --> S2[Sprint 2]
    S1 --> S3[Sprint 3]
    S2 --> S4[Sprint 4]
    S3 --> S4

    S1 --- note1["Foundation: cql_client, auth, transport, memo, plan"]
    S2 --- note2["Folds: compression, graph, fold lifecycle"]
    S3 --- note3["Entities: phonetic, temporal, feedback, audit"]
    S4 --- note4["Routing: strategy selection, HTTP, hardening"]
```

Sprint 2 and Sprint 3 can run in parallel after Sprint 1 completes — they share infrastructure (cql_client, auth, metrics) but don't depend on each other's tool implementations. Sprint 4 requires both.
