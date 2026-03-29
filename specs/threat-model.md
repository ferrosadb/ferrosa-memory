# STRIDE Threat Model — ferrosa-memory-mcp

> Last updated: 2026-03-29
> Status: Updated — Sprint 5 Datalog inference, warmth field, recursive explore, derived cache, and rule management threats added.

## Scope

Full system: MCP clients -> ferrosa-memory-mcp -> Ferrosa DB. Includes stdio and HTTP+SSE transports, all 35 MCP tools (32 existing + recursive_explore, query_derived, manage_rules), 10 CQL tables (6 existing + warmth, rules, derived cache, provenance), graph layer, Datalog inference engine, embedding endpoint, viz WebSocket server, and nightly batch job.

## Data Flow Diagram

```mermaid
%%{init: {'theme':'dark','themeVariables':{'primaryColor':'#16161f','primaryTextColor':'#e8e8ed','primaryBorderColor':'#e2725b','lineColor':'#9494a3','secondaryColor':'#1c1c28','tertiaryColor':'#111118','clusterBkg':'#111118','clusterBorder':'#1e1e2a','edgeLabelBackground':'#111118','nodeTextColor':'#e8e8ed'}}}%%
graph TB
    subgraph "Trust Boundary 0: External"
        C1[Claude Code<br/>stdio client]
        C2[Claude.ai<br/>HTTP client]
        C3[Third-party<br/>MCP client]
        OL[Ollama<br/>embedding endpoint]
        WC[Viz WebSocket<br/>clients]
    end

    subgraph "Trust Boundary 1: MCP Server Process"
        T[transport]
        D[tool_dispatch]
        A[auth]
        R[tool_router]
        TH[Tool Handlers<br/>memo/plan/fold/entity/feedback]
        DL[Datalog Inference<br/>semi-naive eval / rules / provenance]
        DC[Derived Cache<br/>TTL-bounded / versioned]
        RE[Recursive Explore<br/>multi-pass / convergence]
        WM[Warmth Module<br/>boost / decay / PPR]
        K[compression]
        E[embedding_client]
        VZ[viz module]
        HT[http module]
    end

    subgraph "Trust Boundary 2: Ferrosa DB Cluster"
        CQL[CQL Interface]
        GR[Graph / Cypher]
        IX[Indexes<br/>HNSW, Phonetic, B-tree]
        ST[Storage<br/>NVMe / S3 / Glacier]
    end

    subgraph "Trust Boundary 3: Batch Job"
        BJ[ferrosa-memory-batch]
    end

    C1 -->|stdio pipes| T
    C2 -->|HTTP+SSE| T
    C3 -->|HTTP+SSE| T
    WC -->|WebSocket| HT
    HT --> VZ
    VZ -->|entity change events| WC
    T --> D
    D --> A
    A --> R
    R --> TH
    TH --> K
    TH --> E
    TH --> VZ
    TH --> RE
    RE --> DL
    RE --> WM
    DL --> DC
    DL -->|load facts| CQL
    DC -->|cache read/write| CQL
    WM -->|warmth read/write| CQL
    TH -->|manage_rules| DL
    E -->|HTTP| OL
    TH -->|CQL| CQL
    TH -->|Cypher| GR
    CQL --> IX
    CQL --> ST
    BJ -->|CQL read| CQL
    BJ -->|CQL write| CQL
    BJ -->|batch inference| DL

    style A fill:#e25b5b,color:#fff
    style CQL fill:#e2725b,color:#fff
    style OL fill:#d4a574,color:#111118
    style DL fill:#5b7be2,color:#fff
    style DC fill:#5b7be2,color:#fff
```

## Trust Boundaries

| ID | Boundary | Crosses | Trust Level |
|----|----------|---------|-------------|
| TB0 | External clients -> MCP server | Network (HTTP) or process pipes (stdio) | Untrusted |
| TB1 | MCP server -> Ferrosa DB (CQL) | CQL wire protocol (TCP) | Authenticated, same-network |
| TB1b | MCP server -> Ferrosa DB (Graph) | HTTP POST to `/graph/query` | Authenticated (HTTP Basic), same-network |
| TB2 | MCP server -> Ollama | HTTP (local network) | Semi-trusted (no auth) |
| TB3 | Batch job -> Ferrosa DB | CQL wire protocol (TCP) | Authenticated, separate credentials |

## STRIDE Analysis

### S — Spoofing

| ID | Threat | Component | Risk | Mitigation |
|----|--------|-----------|------|------------|
| S1 | Attacker impersonates a legitimate MCP client over HTTP | transport (TB0) | **High** (L:3 x I:4 = 12) | HTTP Basic auth over TLS. Reject connections without valid credentials. |
| S2 | Client supplies a forged `tenant_id` in tool parameters | auth | **Critical** (L:4 x I:5 = 20) | `tenant_id` is NEVER client-supplied. Extracted from authenticated session only. Input schema rejects any `tenant_id` field in tool params. |
| S3 | Stdio client spoofing (local privilege escalation) | transport (TB0) | **Low** (L:1 x I:4 = 4) | stdio inherits process owner — OS-level trust. Document that stdio mode assumes local trust. |
| S4 | Batch job credentials compromised | batch job (TB3) | **Medium** (L:2 x I:4 = 8) | Separate CQL credentials for batch job with read-only on `feedback_outcomes`, write-only on `routing_guidelines`. Least privilege. |
| S5 | WebSocket connections bypass MCP auth — viz endpoint may lack tenant authentication | viz module (TB0→TB1) | **Low** (L:2 x I:3 = 6) | **MITIGATED** — WebSocket upgrade validates auth token. SSE anomaly subscription (`/subscribe/anomalies`) also requires auth. |
| S7 | Rule injection via `manage_rules` tool — malicious agent injects rules that derive false facts, poisoning inference results | datalog (TB1) | **Medium** (L:3 x I:4 = 12) | Rule body validation (parse before storing — reject rules that don't produce valid AST). Rule family isolation (rules only operate within their declared family). Audit log for all rule changes with `tenant_id`, `session_id`, rule_id, old/new body. |

### T — Tampering

| ID | Threat | Component | Risk | Mitigation |
|----|--------|-----------|------|------------|
| T1 | Memory poisoning via crafted entity upserts (MemoryGraft attack) | entity_tools | **Critical** (L:3 x I:5 = 15) | Write-time confidence gating (reject < 0.7). Anomaly detection on retrieval frequency (>3σ flagged). Append-only audit log. |
| T2 | Poisoned memo cache entries returning wrong results | memo_tools | **High** (L:3 x I:4 = 12) | Content hash verification on read. TTL-based expiry limits poison window. Audit log on all writes. |
| T3 | Tampered fold summaries misleading future retrieval | fold_tools | **High** (L:2 x I:5 = 10) | Fold summaries written only by authenticated sessions. Compression is lossless-reversible for verification. `FOLDED_INTO` graph edges provide provenance chain. |
| T4 | CQL injection via tool parameters | cql_client | **High** (L:2 x I:5 = 10) | ALL queries use prepared statements with parameterized bindings. No string interpolation into CQL. |
| T5 | Cypher injection via entity names or queries | graph_client (HTTP) | **High** (L:2 x I:5 = 10) | Parameterized Cypher queries via HTTP POST. Entity names passed as parameters, never interpolated into query strings. Graph client uses HTTP Basic auth on same-network endpoint (TB1b). |
| T6 | Tampering with compressed data on S3 | storage (TB2) | **Medium** (L:1 x I:4 = 4) | S3 server-side encryption. Integrity check on decompression (store checksum alongside compressed data). |
| T7 | Smart ingest manipulation — adversarial content could game the prediction error gate to either flood entities or suppress legitimate ingestion | smart_ingest | **Low-Medium** (L:2 x I:3 = 6) | Rate limiting on ingest calls. Monitor prediction error distribution for anomalies. Confidence floor on entity extraction. |
| T8 | Warmth field manipulation — adversarial access patterns to artificially inflate warmth scores and bias retrieval toward attacker-controlled entities | warmth (TB1) | **Medium** (L:3 x I:3 = 9) | Max warmth cap (10.0) prevents unbounded inflation. Ebbinghaus decay normalization in consolidation. Anomaly detection on warmth distribution (flag entities with warmth >3σ above session mean). |
| T9 | Derived cache poisoning — corrupted or stale cache entries return wrong derived facts, leading to incorrect inference results | derived_cache (TB1→TB2) | **Low-High** (L:2 x I:4 = 8) | TTL limits exposure window (default 3600s). Cache key includes `rule_version` — rule changes invalidate affected entries. Provenance chain allows downstream verification of any derived fact. |

### R — Repudiation

| ID | Threat | Component | Risk | Mitigation |
|----|--------|-----------|------|------------|
| R1 | Deny having written a poisoned entity or memo entry | entity_tools, memo_tools | **Medium** (L:3 x I:3 = 9) | Append-only audit log with `tenant_id`, `session_id`, timestamp, operation type. Audit rows cannot be deleted via MCP tools. **Status: PARTIAL** — CQL persistence added, but writes are best-effort (see R3). |
| R2 | Deny having submitted false feedback outcomes | feedback_tools | **Low** (L:2 x I:2 = 4) | `feedback_outcomes` is write-only via MCP, with `tenant_id` and `session_id` from auth context. |
| R3 | Audit log bypass via direct CQL — if audit writes are best-effort (fire-and-forget), failed audit writes are invisible | audit_log | **Medium** (L:3 x I:3 = 9) | Audit writes must be synchronous or use a write-ahead log. Monitor audit write failure rate. Alert on gaps in audit sequence. |

### I — Information Disclosure

| ID | Threat | Component | Risk | Mitigation |
|----|--------|-----------|------|------------|
| I1 | Cross-tenant data leakage via crafted queries | cql_client | **Critical** (L:2 x I:5 = 10) | ALL CQL queries include `tenant_id` from auth context in WHERE clause. Partition key design ensures physical isolation. |
| I2 | Raw trajectory content exposed in hot storage | trajectory_folds | **High** (L:2 x I:4 = 8) | Compress within minutes of folding. Archive to Glacier within 30 days. `raw_trajectory` is the highest-risk field. |
| I3 | Embedding vectors reversed to reconstruct source text | all tables with embeddings | **Medium** (L:1 x I:3 = 3) | Embeddings alone are not reversible. Source text columns deleted with parent row on cascade delete. |
| I4 | Ollama endpoint logs contain sensitive prompt content | embedding_client (TB2) | **Medium** (L:2 x I:3 = 6) | Ollama runs on local/private network. Document that embedding requests contain text fragments. Configure Ollama to disable request logging in production. |
| I5 | Membership inference on agent memory store | all tables | **Medium** (L:2 x I:3 = 6) | Per "Unveiling Privacy Risks" paper. Mitigated by tenant isolation (attacker can only probe their own tenant). Rate limiting on retrieval tools. |
| I6 | Viz WebSocket broadcasts all entity changes to any connected client without tenant scoping | viz module | **Medium** (L:2 x I:4 = 8) | **PARTIALLY MITIGATED** — WebSocket connection requires auth, but tenant scoping of entity events needs verification. SSE anomaly alerts are tenant-scoped. |
| I7 | Spreading activation traversal could leak entity relationships across session boundaries | spread_activation | **Low-Medium** (L:2 x I:3 = 6) | Traversal queries must include `tenant_id` filter at every hop. Session-scoped activation should not cross into other sessions' private entities. |
| I8 | Provenance leaks cross-tenant facts — provenance chain for a derived fact references parent facts from other tenants, disclosing their existence or content | datalog / provenance (TB1→TB2) | **Critical** (L:2 x I:5 = 10) | All provenance queries scoped by `tenant_id` at the Storage trait level. `load_session_facts()` only loads facts for the authenticated tenant. Tenant isolation enforced in every Storage method (warmth, rules, cache, provenance). Integration test: derive facts in tenant A, query provenance in tenant B, verify empty result. |

### D — Denial of Service

| ID | Threat | Component | Risk | Mitigation |
|----|--------|-----------|------|------------|
| D1 | Flood of `store_memo_result` calls fills storage | memo_tools | **Medium** (L:3 x I:3 = 9) | `max_memo_results` config cap. TTL-based expiry. Per-tenant storage quotas (Ferrosa-level). **Status: MITIGATED** — quotas implemented (commit 57cf61b). |
| D2 | Large `raw_trajectory` payloads in `append_to_fold` | fold_tools | **Medium** (L:3 x I:3 = 9) | Max payload size on `repl_turn` input (configurable, default 64KB). Token count tracking surfaces growth to caller. |
| D3 | Expensive Cypher traversals on large entity graphs | graph_client | **High** (L:3 x I:4 = 12) | Query timeout on Cypher executions (configurable, default 5s). Limit traversal depth in all Cypher queries (max 3 hops). |
| D4 | HTTP connection exhaustion | transport | **Medium** (L:3 x I:3 = 9) | Connection limit per source IP. Tokio's async model handles backpressure naturally. |
| D5 | Ollama endpoint unavailable stalls tool calls | embedding_client | **Medium** (L:3 x I:3 = 9) | Timeout on embedding requests (default 10s). Graceful degradation: tools that require embedding fail fast with clear error, don't block other tools. |
| D6 | Spreading activation with large `max_hops` on dense graphs could exhaust CPU | spread_activation | **Medium** (L:3 x I:3 = 9) | `max_hops` capped at 5 in tool schema. Visited-node set bounds total work. Query timeout on underlying Cypher traversals. |
| D7 | Dream consolidation O(n^2) entity comparison could be slow for large sessions | run_consolidation | **Low** (L:2 x I:2 = 4) | Consolidation runs as background task. Limit batch size per invocation. Monitor execution time and abort if threshold exceeded. |
| D8 | Datalog fact explosion — recursive rules on dense graphs produce unbounded derived facts, exhausting memory and CPU | datalog (TB1) | **High** (L:3 x I:4 = 12) | `max_facts=50000` hard cap on total derived facts. `max_iterations=100` hard cap on semi-naive evaluation rounds. Entity cap 1000 bounds input graph size. Log warning and bail on any cap hit. Alert if >50 iterations on any single evaluation. |
| D9 | Recursive explore resource exhaustion — deep recursive passes with expensive sub-queries consume CPU and memory disproportionate to a single tool call | recursive_explore (TB1) | **Medium** (L:3 x I:3 = 9) | Max 5 passes hard cap. Per-pass 2s timeout. Max 50 entities total across all passes. Total exploration timeout. Convergence detection stops early when no new facts derived. |

### E — Elevation of Privilege

| ID | Threat | Component | Risk | Mitigation |
|----|--------|-----------|------|------------|
| E1 | MCP client escalates to batch job credentials | auth | **High** (L:1 x I:5 = 5) | Batch job uses separate CQL credentials, not accessible via MCP server process. Different auth context. |
| E2 | Tool call accesses `feedback_outcomes` read path | feedback_tools | **Medium** (L:2 x I:3 = 6) | `record_outcome` is write-only. No MCP tool exposes `SELECT` on `feedback_outcomes`. Read access only via batch job credentials. |
| E3 | Attacker chains memo poisoning + routing manipulation | tool_router + memo_tools | **High** (L:1 x I:5 = 5) | Routing guidelines are written only by batch job (separate credentials). Memo cache doesn't influence routing decisions directly — only `feedback_outcomes` does, and that table is write-only via MCP. |
| E4 | Session deletion without ownership verification — any authenticated tenant could delete another tenant's session | delete_session | **Critical** (L:2 x I:5 = 10) | `delete_session` must enforce `tenant_id` scoping — verify session belongs to requesting tenant before deletion. Return error if session not found within tenant scope. |
| E5 | `manage_rules` allows arbitrary Datalog execution — custom rules could access data outside session scope, enabling cross-session or cross-tenant fact inference | datalog / manage_rules (TB1) | **High** (L:2 x I:4 = 8) | Rules only operate on session-scoped fact sets loaded by `load_session_facts()`. No cross-session or cross-tenant fact loading — the evaluator receives a closed `FactSet`, not a storage handle. Rule body validation rejects predicates that don't match the canonical schema. |

## Risk Summary

```mermaid
%%{init: {'theme':'dark'}}%%
quadrantChart
    title Threat Risk Matrix
    x-axis Low Likelihood --> High Likelihood
    y-axis Low Impact --> High Impact
    quadrant-1 Monitor
    quadrant-2 Critical - Mitigate Now
    quadrant-3 Accept
    quadrant-4 Mitigate in Phase 1

    S2-Tenant-Spoofing: [0.8, 0.95]
    T1-Memory-Poisoning: [0.6, 0.95]
    T4-CQL-Injection: [0.4, 0.95]
    T5-Cypher-Injection: [0.4, 0.95]
    I1-Cross-Tenant-Leak: [0.4, 0.95]
    I8-Provenance-CrossTenant: [0.4, 0.95]
    S1-Client-Impersonation: [0.6, 0.8]
    S7-Rule-Injection: [0.6, 0.8]
    T2-Memo-Poisoning: [0.6, 0.8]
    D3-Cypher-DoS: [0.6, 0.8]
    D8-Datalog-Explosion: [0.6, 0.8]
    D1-Storage-Flood: [0.6, 0.6]
    D2-Payload-Size: [0.6, 0.6]
    D9-Recursive-Exhaustion: [0.6, 0.6]
    E5-Rule-Scope-Escape: [0.4, 0.8]
    T8-Warmth-Manipulation: [0.6, 0.6]
    T9-Cache-Poisoning: [0.4, 0.8]
    I2-Raw-Trajectory: [0.4, 0.8]
    I4-Ollama-Logs: [0.4, 0.6]
```

## Critical Threats (Must mitigate before v1.0)

1. **S2 — Tenant ID spoofing:** Architectural invariant — `tenant_id` never from client input. **Status: MITIGATED** — `TenantContext` required param in all handlers, type-system enforced.
1. **T1 — Memory poisoning (MemoryGraft):** Confidence gating + anomaly detection + audit log. **Status: PARTIAL** — confidence gating (>=0.7) implemented, rate limiting (1000/session) implemented. Anomaly detection on retrieval frequency implemented (commit 2226409). Audit log NOT yet implemented.
1. **T4/T5 — CQL/Cypher injection:** Prepared statements only, zero string interpolation. **Status: MITIGATED** — all 17 CQL prepared statements parameterized. Graph queries use HTTP POST with serialized parameters.
1. **I1 — Cross-tenant leakage:** Partition key design + auth-scoped queries. **Status: MITIGATED** — every CQL query includes `tenant_id` from auth context.

## High Threats (Mitigate in Phase 1-2)

5. **S1 — Client impersonation:** TLS + HTTP Basic auth. **Status: PARTIAL** — HTTP Basic auth implemented. TLS support added (commit 96f3542), but `require_tls: false` still default in main.rs. TLS enforcement needed before production HTTP deployment.
1. **T2 — Memo cache poisoning:** Content hash verification + TTL. **Status: MITIGATED** — SHA-256 content hash, model version isolation, TTL support.
1. **D3 — Cypher DoS:** Query timeout + depth limit. **Status: PARTIAL** — depth limit in traversal queries, but no explicit query timeout configured on HTTP client.
1. **I2 — Raw trajectory exposure:** Compress fast, archive fast. **Status: PARTIAL** — compression engine working, but background compression job and S3 lifecycle not yet wired.

## Sprint 5 Threats (Mitigate during Sprint 5)

9. **I8 — Provenance cross-tenant leakage:** All provenance queries scoped by `tenant_id`. Fact loading is tenant-isolated. **Status: PLANNED** — Storage trait design enforces tenant scoping. Integration test required.
1. **S7 — Rule injection:** Parse and validate rule body before storage. Rule family isolation. Audit log. **Status: PLANNED** — `parse_rule()` validation gate required in `manage_rules` handler.
1. **D8 — Datalog fact explosion:** Hard caps on facts (50000), iterations (100), entities (1000). **Status: PLANNED** — caps configured in `[datalog]` config section.
1. **D9 — Recursive explore exhaustion:** Max 5 passes, per-pass timeout, max 50 entities. **Status: PLANNED** — guard rails configured in `[rmh]` config section.
1. **E5 — Rule scope escape:** Rules operate on closed `FactSet`, not storage handles. No cross-session fact loading. **Status: PLANNED** — architectural constraint in `evaluate()` API design.
1. **T8 — Warmth manipulation:** Max warmth cap 10.0, Ebbinghaus decay, anomaly detection. **Status: PLANNED** — cap enforced in `warmth_boost()`.
1. **T9 — Derived cache poisoning:** TTL limits window, `rule_version` in cache key, provenance allows verification. **Status: PLANNED** — cache key design includes rule version.
