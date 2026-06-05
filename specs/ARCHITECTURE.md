# Architecture

A new contributor should be able to orient in ten minutes. Read this file; then
open `crates/ferrosa-memory-core/src/lib.rs` with the module list in mind.

## What this project is

ferrosa-memory is an MCP (Model Context Protocol) server that gives LLM agents
— primarily Claude Code — a durable, structured scratchpad. Agents call typed
tools (`check_memo_cache`, `upsert_entity`, `start_fold`, ...); the server
stores and retrieves facts from a Ferrosa cluster; nothing in the hot path is
inference beyond an embedding call.

It is, in intent, a thin adapter over Ferrosa's public interfaces. In current
reality it reaches past those interfaces in several places. That gap — between
"client to Ferrosa" and "second storage engine over Ferrosa's tables" — is
the single most important thing to understand about the codebase, because most
open refactors point at it.

## The four crates

The workspace is declared in [Cargo.toml](../Cargo.toml). There are four
primary crates plus an evaluation harness:

| Crate | Kind | Purpose |
|-------|------|---------|
| [`ferrosa-memory-core`](../crates/ferrosa-memory-core) | library | All the logic: tools, storage trait, CQL impl, Datalog, workbench backends. ~15k LoC across ~40 modules. |
| [`ferrosa-memory-mcp`](../crates/ferrosa-memory-mcp/src/main.rs) | binary | The server. Wires transport + CQL + dispatch together. Owns the reconnect loop. |
| [`ferrosa-memory-batch`](../crates/ferrosa-memory-batch/src/main.rs) | binary | Nightly job: reads `feedback_outcomes`, writes routing guidelines (ACON-style offline learning). |
| [`ferrosa-memory-sync`](../crates/ferrosa-memory-sync/src/main.rs) | binary | Cross-device/cross-cluster state reconciler (CLI-driven). |
| [`ferrosa-memory-eval`](../crates/ferrosa-memory-eval) | binary | Scenario-driven evaluation harness that drives the MCP server and grades responses. Not part of the serving path. |

The three non-core crates are deliberately small (~400–700 LoC each, except
`-eval` which is large because it owns scenarios, scoring, and its own MCP
client). They hold no business rules. If you are tempted to put logic in a
binary crate, stop and move it into `ferrosa-memory-core`.

## The one-page picture

```
MCP client (Claude Code / Claude.ai / codex)
       │
       │  JSON-RPC  (stdio  |  HTTPS+SSE)
       ▼
┌─────────────────────────────────────────────────────┐
│ ferrosa-memory-mcp                                  │
│   main.rs: config, TLS, auth file, reconnect loop   │
│                                                     │
│   transport ──► dispatch ──► tool handlers          │
│     (stdio/http)   (40+ tools)    (memo/plan/fold/  │
│                                    entity/feedback/ │
│                                    datalog/rules…)  │
│                                                     │
│                       │  Storage trait              │
│                       ▼                             │
│                  CqlStorage   (the only impl        │
│                                in production;       │
│                                MockStorage          │
│                                is test-only)        │
└─────────┬────────────────────────────┬──────────────┘
          │                            │
          │ CQL (cdrs-tokio)           │ HTTP Cypher  +  HTTP embeddings
          │  port 9042                 │ port 7474        (Ollama, 11434)
          ▼                            ▼                  ▼
┌──────────────────────┐     ┌──────────────────┐   ┌──────────────┐
│ Ferrosa cluster      │     │ Ferrosa /graph   │   │ Ollama       │
│   keyspace           │     │   (read-only     │   │  nomic-embed │
│   agent_memory       │     │    today)        │   │  768-d f32   │
│                      │     └──────────────────┘   └──────────────┘
│   AND graph-owned    │
│   tables (leaky!)    │
└──────────────────────┘
```

## Big boxes, in order of caller → callee

### Transport and dispatch

`transport` ([core/src/transport.rs](../crates/ferrosa-memory-core/src/transport.rs))
frames JSON-RPC over either stdio or the shared HTTP listener in
[core/src/http.rs](../crates/ferrosa-memory-core/src/http.rs). Requests are
handed to `dispatch` ([core/src/dispatch.rs](../crates/ferrosa-memory-core/src/dispatch.rs),
~5k LoC), which owns:

- the tool manifest — [`tool_definitions`](../crates/ferrosa-memory-core/src/dispatch.rs:186)
  builds JSON schemas dynamically from the entity/edge type registry so the
  enum values are not hardcoded;
- the per-tool handler table — a large `match tool_name { ... }` starting at
  [dispatch.rs:1174](../crates/ferrosa-memory-core/src/dispatch.rs:1174);
- fire-and-forget writes to `tool_usage_log` after every handler (token
  accounting, DDL 009).

Handlers are thin. They call auth → route (optional) → `Storage` trait →
return JSON. The interesting code lives in the per-domain modules: `memo`,
`plan`, `fold`, `entity`, `feedback`, `recursive_explore`, `datalog`,
`promotion`, `warmth`, `pagerank`, `dream`, `spreading`, `chains`,
`hybrid_search`, `smart_ingest`, `ner`, `skill`, `expert_system`.

### The Storage trait — the main abstraction seam

[`Storage`](../crates/ferrosa-memory-core/src/storage.rs:37) is a large async
trait (~100 methods: `memo_get`, `plan_put`, `fold_complete`, `entity_put`,
`typed_edge_put`, `rule_upsert`, ...). Every method takes a `&TenantContext`
as its second argument — see
[storage.rs:29](../crates/ferrosa-memory-core/src/storage.rs:29). That
argument is not optional and it is never client-supplied; it is produced by
`auth::authenticate` and threaded from `dispatch` down. Tenant isolation is
enforced at the trait boundary, not per-call.

There are exactly two implementations:

- [`CqlStorage`](../crates/ferrosa-memory-core/src/cql_storage.rs) — 4k LoC of
  prepared-statement management, reconnection, ghost-row handling, and CQL
  quirks. This is what `ferrosa-memory-mcp` constructs at startup.
- [`MockStorage`](../crates/ferrosa-memory-core/src/storage.rs:756) — in-memory,
  gated on `#[cfg(any(test, feature = "mock-storage"))]`. Used by unit tests
  only. The production binary never falls back to it; see the comment at
  [main.rs:7](../crates/ferrosa-memory-mcp/src/main.rs:7) — "Never falls back
  to mock storage — mock silently loses data."

Everything above the trait (dispatch, every tool module, Datalog, warmth,
recursive explore) is backend-agnostic. Everything below it names tables.

### Connection lifecycle: `ReconnectingStorage`

`ferrosa-memory-mcp/src/main.rs` wraps `CqlStorage` in a `ReconnectingStorage`
([main.rs:43](../crates/ferrosa-memory-mcp/src/main.rs:43)). Invariants:

- The server binds its listener **before** CQL is connected. On startup
  failure it enters `disconnected` state and returns `NOT_CONNECTED_MSG`
  errors until a background task with exponential backoff (1→2→4→…→30s cap,
  see [`next_backoff`](../crates/ferrosa-memory-mcp/src/main.rs)) succeeds.
- A generation counter prevents stale "connection lost" errors from a
  pre-reconnect query from tearing down a freshly rebuilt session. See
  [`mark_disconnected`](../crates/ferrosa-memory-mcp/src/main.rs:101).
- "Stale prepared statement" errors (post-node-restart) are classified as
  connection errors in [`is_connection_error`](../crates/ferrosa-memory-mcp/src/main.rs:123)
  so the whole statement cache gets rebuilt.

Readiness probe (`GET /healthz/ready`) reports whether `inner.is_some()`.
Liveness (`/healthz/live`) reports process liveness only.

## How MCP tools map onto Storage methods

Five functional groups currently exposed to agents (full list and schemas in
[`tool_definitions`](../crates/ferrosa-memory-core/src/dispatch.rs:186)):

| Tool | Handler | Storage methods used |
|------|---------|---------------------|
| `check_memo_cache`, `store_memo_result` | [`memo`](../crates/ferrosa-memory-core/src/memo.rs) | `memo_get`, `memo_touch`, `memo_put` |
| `write_plan_node`, `get_plan_context`, `update_plan_node` | [`plan`](../crates/ferrosa-memory-core/src/plan.rs) | `plan_put`, `plan_get`, `plan_update_status` |
| `start_fold`, `append_to_fold`, `complete_fold`, `retrieve_fold_context` | [`fold`](../crates/ferrosa-memory-core/src/fold.rs) | `fold_put`, `fold_append`, `fold_complete`, `fold_summary_search` + graph edge writes |
| `upsert_entity`, `retrieve_entities` | [`entity`](../crates/ferrosa-memory-core/src/entity.rs), [`smart_ingest`](../crates/ferrosa-memory-core/src/smart_ingest.rs) | `entity_put`, `entity_find_phonetic`, `typed_edge_put`, embedding call, HNSW search |
| `record_outcome` | [`feedback`](../crates/ferrosa-memory-core/src/feedback.rs) | `feedback_put` only — writes are write-only via MCP; the batch job reads via a separate CQL credential |

Plus the expert-system governance surface (`manage_rules`, `manage_claims`,
`manage_approvals`, `manage_aliases`, `query_derived`, `explain_derived`,
`recursive_explore`, `promotion`, `dream`) which is routed through a single
effective-rule loader so every inference path sees the same merged rule set.
See [expert-system-knowledge-plane.md](./expert-system-knowledge-plane.md).

## How ferrosa-memory talks to the cluster

Three outbound channels, all addressing the same cluster but not with the
same honesty:

1. **CQL (cdrs-tokio) — public protocol, leaky usage.** Writes and reads
   against the `agent_memory` keyspace tables owned by this project
   (`memo_cache`, `plan_state`, `trajectory_folds`, `entity_store`,
   `temporal_events`, `feedback_outcomes`, `entity_types`, `edge_types`,
   `rules_by_id`, etc.) are legitimate. But `CqlStorage` also writes graph
   edges directly into **tables owned by the graph engine**
   (`typed_edges`, `folded_into`, `mentioned_in`, `co_occurs_with`,
   `supersedes`, `derived_edges_by_pred`, `derived_edges_by_src`). See
   [cql_storage.rs:3948](../crates/ferrosa-memory-core/src/cql_storage.rs:3948)
   for the `typed_edges` INSERT. The wire protocol is public; the schema is
   not. Graph invariants (uniqueness, reverse-index consistency, generation
   columns) bypass the Cypher layer entirely. Tracked as
   [bug-ferrosa-memory-bypasses-graph-api-for-writes.md](./todo/bug-ferrosa-memory-bypasses-graph-api-for-writes.md)
   and rolled up into
   [feat-endpoint-only-ferrosa-client.md](./todo/feat-endpoint-only-ferrosa-client.md)
   (ADR-005).
2. **HTTP Cypher on :7474 — public and correct, but read-only today.**
   [`graph`](../crates/ferrosa-memory-core/src/graph.rs) POSTs Cypher MATCH
   queries for traversals: `FOLDED_INTO`, `MENTIONED_IN`, `CO_OCCURS_WITH`,
   `SUPERSEDES`. The symmetric write path does not exist yet; when it does,
   the direct-CQL edge inserts should disappear.
3. **S3 / Glacier — public.** Trajectory archival and cold memo storage go
   through Ferrosa's standard lifecycle tiering (NVMe → S3 → Glacier after
   30 days). ferrosa-memory itself does not talk to S3 directly; it relies on
   the cluster's storage tiering.

SPARQL is exposed only as an authenticated public passthrough surface for
operator inspection. `ferrosa-memory` does not implement SPARQL semantics
locally. Datalog is evaluated locally in
[`datalog`](../crates/ferrosa-memory-core/src/datalog.rs) over Ferrosa-backed
facts and is intentionally repo-owned.

## The embedding pipeline

Any tool that writes something semantic (`store_memo_result`,
`complete_fold`, `upsert_entity`) produces an embedding through
[`embedding`](../crates/ferrosa-memory-core/src/embedding.rs):

1. MCP handler receives text.
2. HTTP POST to Ollama's `/api/embeddings` (default
   `http://localhost:11434`, model `nomic-embed-text-v2-moe`, 768-d f32). Optional
   small in-process cache by content hash.
3. Result serialized via [`vector::encode_vector`](../crates/ferrosa-memory-core/src/vector.rs)
   into CQL wire bytes for the VECTOR column — a workaround for cdrs-tokio
   v9 not understanding type ID 0x0023 natively.
4. Stored in `entity_store` / `trajectory_folds` / `memo_cache`.
5. Entity writes trigger derived-edge updates: phonetic dedup against
   existing rows, then `CO_OCCURS_WITH` edges between entities from the same
   source fold (see [`dream`](../crates/ferrosa-memory-core/src/dream.rs) and
   [`smart_ingest`](../crates/ferrosa-memory-core/src/smart_ingest.rs)).
   Today those edges land as direct CQL inserts — see the graph-bypass bug
   above.

NER ([`ner`](../crates/ferrosa-memory-core/src/ner.rs)) uses a second Ollama
endpoint (`/api/generate`) for entity extraction when heuristics are
ambiguous; this is a slower, opt-in tier.

## MCP surface: who calls, how

Two transports:

- **stdio** — the default for Claude Code. Process-owner trust, one tenant
  per process, configured in `~/.claude/settings.json` with
  `FERROSA_MEMORY_CONFIG` pointing at `examples/ferrosa-memory.toml`.
- **shared HTTPS** — for Claude.ai connectors and codex-compatible remote
  MCP clients. `POST /mcp` for JSON-RPC, `GET /metrics`,
  `GET /healthz/{live,ready}`. The viz dashboard is a separate loopback-only
  listener behind `[viz]`.

Shared HTTP is **fail-closed on startup** —
[`validate_shared_http_config`](../crates/ferrosa-memory-core/src/config.rs)
refuses to bind unless all of `require_tls=true`, `cert_path`, `key_path`,
and `auth_file` are set and no `tenant_id` fallback is configured.
Authentication is file-backed
([`FileAuthValidator`](../crates/ferrosa-memory-core/src/auth.rs) — lowercase
SHA-256 password digests) with SIGHUP reload. One principal maps to exactly
one tenant; `TenantContext.tenant_id` is always server-derived.

## Invariants and where they live

| Invariant | Enforced in |
|-----------|------------|
| `tenant_id` is never client-supplied | [`auth::authenticate`](../crates/ferrosa-memory-core/src/auth.rs); every `Storage` method takes `&TenantContext` ([storage.rs:37](../crates/ferrosa-memory-core/src/storage.rs:37)) |
| Production never runs on `MockStorage` | `cfg` gate on [storage.rs:756](../crates/ferrosa-memory-core/src/storage.rs:756); comment on [main.rs:7](../crates/ferrosa-memory-mcp/src/main.rs:7) |
| Shared HTTP requires TLS + auth file + no tenant fallback | [`validate_shared_http_config`](../crates/ferrosa-memory-core/src/config.rs); binder refuses otherwise |
| Feedback store is write-only via MCP | No reader tool registered in [`tool_definitions`](../crates/ferrosa-memory-core/src/dispatch.rs:186); the batch job reads via a separate CQL credential |
| Audit log is append-only | [`audit::log_write`](../crates/ferrosa-memory-core/src/audit.rs) is the only writer; no MCP tool deletes rows |
| One effective rule set for all inference paths | `get_effective_rule_set` in [`dispatch.rs`](../crates/ferrosa-memory-core/src/dispatch.rs) is called from `manage_rules`, `query_derived`, `recursive_explore` ([recursive_explore.rs:148](../crates/ferrosa-memory-core/src/recursive_explore.rs:148)), and `promotion` ([promotion.rs:60](../crates/ferrosa-memory-core/src/promotion.rs:60)) |
| Fail-loud on backend error | `ReconnectingStorage` surfaces explicit `NOT_CONNECTED_MSG` ([main.rs:119](../crates/ferrosa-memory-mcp/src/main.rs:119)) rather than returning empty results |

## Things that are not right yet (and where to read about them)

- **Graph-boundary refactor** — [feat-endpoint-only-ferrosa-client.md](./todo/feat-endpoint-only-ferrosa-client.md),
  [ADR-005](./decisions/adr-005-endpoint-only-ferrosa-client.md). The
  serving path should converge on a role-aware split: direct CQL for
  app-owned tables, graph reads/writes through graph interfaces, and
  passthrough workbench query adapters for CQL/SPARQL/Datalog. Today
  `CqlStorage` still names graph-owned tables directly in some paths.
- **Graph writes via Cypher** — [todo-extend-ferrosa-memory-graph-client-with-cypher-writes.md](./todo/todo-extend-ferrosa-memory-graph-client-with-cypher-writes.md).
- **Workbench query explorers as passthroughs, not local engines** — the
  `/workbench/api/cql/query` and `/workbench/api/datalog/query` routes in
  [`http`](../crates/ferrosa-memory-core/src/http.rs) still run query
  semantics locally.
- **`initialize` should not block on backend connect** —
  [bug-initialize-blocks-on-backend-connect.md](./todo/bug-initialize-blocks-on-backend-connect.md).
- **Content-hash clobbering on partial entity updates** —
  [bug-content-hash-clobbered-by-partial-entity-updates.md](./todo/bug-content-hash-clobbered-by-partial-entity-updates.md).

When you read those, the code pointers above should make the gap obvious:
the trait seam already exists, but `CqlStorage` is still the only provider
and it knows too much about Ferrosa's private schema. The refactor is to
split `CqlStorage` into four public-endpoint clients behind the same
`Storage` trait.
