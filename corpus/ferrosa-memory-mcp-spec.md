# Ferrosa Memory MCP Server
## Product Specification v0.1

**Status:** Pre-implementation draft
**Authors:** Ben
**Last updated:** 2026-03-21
**Repository:** TBD (ferrosa-memory-mcp)

---

## 1. Overview

### 1.1 Problem Statement

Large language model agents — and specifically Recursive Language Model (RLM) trajectories running inside Claude Code — currently manage all working context as ephemeral REPL variables. This creates three compounding problems:

- **Redundant computation.** Sub-calls for the same logical chunk are re-invoked across sessions and even within a single long trajectory, because there is no shared memoization layer. The "Think, But Don't Overthink" reproduction study observed execution time blowup from 3.6s to 344.5s when depth-2 recursion re-derived results the parent already held.
- **Context loss at boundaries.** Useful intermediate state — entity classifications, fold summaries, parent plan hierarchies — is discarded when the REPL is torn down. The SRLM paper demonstrates that program selection quality (not raw recursion depth) is the primary driver of RLM performance; without durable state, no learning is possible across invocations.
- **No multi-agent coordination.** Multiple sub-call models operate in isolation with no shared memory surface. The MIRIX paper shows six distinct memory types are needed to serve real-world agent workloads; current RLM scaffolds provide none of them explicitly.

### 1.2 Solution

The **Ferrosa Memory MCP Server** (`ferrosa-memory-mcp`) is a lightweight MCP server that exposes Ferrosa's full index and graph infrastructure as typed tools consumable by Claude Code, Claude.ai, and any MCP-compatible client. It provides:

- A structured, durable memory backend for RLM agent trajectories
- Memoization of sub-call results keyed on content hash
- Hierarchical plan state storage with efficient range-scan retrieval
- Trajectory fold/summarization with graph-addressable history
- Semantic retrieval via HNSW vector index
- Phonetic/fuzzy entity search via Ferrosa's Double Metaphone index
- A feedback loop store for offline guideline refinement (per ACON/SRLM)
- Per-tenant memory isolation with audit trails

The server is a **thin adapter layer** — 300–500 lines of Rust — that translates MCP tool calls into CQL and Cypher queries against a Ferrosa keyspace. All intelligence stays in the LLM and in Ferrosa's index infrastructure.

### 1.3 Positioning

| | Claude Code (RLM runtime) | ferrosa-memory-mcp (this project) | Ferrosa DB (storage) |
|---|---|---|---|
| **Role** | Orchestrates agent loop, spawns sub-agents | Typed memory interface, MCP protocol | Durable store, indexes, graph |
| **Owns** | Prompt construction, recursion control | Tool schemas, query translation, auth | Data, indexes, replication, S3 tiering |
| **Replaces?** | No | Existing ad-hoc REPL variables | Nothing — adds a layer above Ferrosa |

This is **not** a replacement for Claude Code or Claude CLI. It is an MCP server that makes both dramatically more capable for long-horizon tasks.

---

## 2. Research Foundation

The design is grounded in the following papers, grouped by the architectural concern they inform.

### 2.1 Core RLM Paradigm
- **Recursive Language Models** (Zhang, Kraska, Khattab 2026) — REPL-environment paradigm, sub-call memoization need
- **SRLM** (Alizadeh et al. 2026) — Program selection > recursion depth; uncertainty signals for retrieval strategy routing
- **Think, But Don't Overthink** (Wang 2026) — Depth cap at 1; memoization as the fix for re-derivation blowup

### 2.2 Memory Architecture
- **Continuum Memory Architecture** (Logan 2026) — Five primitives: persistent storage, selective retention, associative routing, temporal chaining, consolidation
- **MIRIX** (Wang & Chen 2025) — Six memory types: Core, Episodic, Semantic, Procedural, Resource, Knowledge Vault; multi-agent coordination layer
- **Zep** (Rasmussen et al. 2025) — Temporally-aware knowledge graph outperforms RAG by 18.5% on LongMemEval; temporal chaining is first-class
- **MemR³** (Du et al. 2025) — Closed-loop retrieval control: route, reflect, answer; evidence-gap tracker; plug-and-play over any memory store

### 2.3 Recursive Agent Patterns
- **Context-Folding** (Sun et al. 2025) — Branch into sub-trajectory, fold on completion; 10× context reduction; RL-trained fold decisions
- **ReCAP** (Zhang et al. 2025, NeurIPS) — Plan-ahead decomposition + structured re-injection of parent plans; linear cost scaling with depth
- **MARINE** (Zhang et al. 2025) — Persistent reference trajectory; iterative refinement; 80B model matches 1000B standalone with this pattern
- **THREAD** (Schroeder et al. 2024) — Thread spawning model; WASM UDF architectural fit

### 2.4 Compression & Cost Control
- **LLMLingua** (Jiang et al. 2023) — 20× prompt compression; warm/cold tier compression boundary
- **NL-Compress** (Chuang et al. 2024) — Model-agnostic NL capsule compression; store alongside raw content
- **ACON** (Kang et al. 2025) — Failure-pair dataset for guideline refinement; distillable compressor
- **Fractured CoT** (Liao et al. 2025) — Three-axis compute allocation (trajectories × solutions × depth); tune per workload
- **Prompt Cache** (Gim et al. 2023) — KV-cache reuse for repeated prefixes; 8–60× latency reduction on sub-call prefix hits

### 2.5 Security & Trust
- **MCPShield** (Zhou et al. 2026) — MCP trust misalignment; metadata-guided probing before tool invocation; runtime event cognition
- **MemoryGraft** (Srivastava & He 2025) — Memory poisoning via RAG store; semantic imitation heuristic exploited by attacker-supplied benign artifacts
- **Unveiling Privacy Risks in LLM Agent Memory** (Wang et al. 2025) — Membership inference, extraction attacks on agent memory stores
- **Terrarium** (Nakamura et al. 2025) — Blackboard model for multi-agent safety; shared memory as attack surface

---

## 3. Architecture

### 3.1 System Diagram

```
┌────────────────────────────────────────────────────────────┐
│                    MCP Clients                              │
│   Claude Code  │  Claude.ai  │  Third-party MCP clients    │
└──────────────────────┬─────────────────────────────────────┘
                       │  MCP protocol (stdio / HTTP+SSE)
┌──────────────────────▼─────────────────────────────────────┐
│              ferrosa-memory-mcp  (this project)             │
│                                                             │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────────┐  │
│  │ Tool Router  │  │ Auth / Tenant│  │  Compression UDF  │  │
│  │ (SRLM-style) │  │  Isolation   │  │  (LLMLingua/NL)   │  │
│  └──────┬──────┘  └──────┬───────┘  └────────┬──────────┘  │
│         └────────────────┼───────────────────┘             │
│                          │ CQL + Cypher                     │
└──────────────────────────┼─────────────────────────────────┘
                           │
┌──────────────────────────▼─────────────────────────────────┐
│                     Ferrosa DB                              │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  agent_memory keyspace                               │   │
│  │                                                      │   │
│  │  memo_cache  │  plan_state  │  trajectory_folds      │   │
│  │  entity_store│  feedback    │  temporal_events        │   │
│  └──────────────────────────────────────────────────────┘   │
│                                                             │
│  Indexes: HNSW · IVFFlat · B-tree · Phonetic · Hash        │
│  Graph:   Cypher · adjacency index · FOLDED_INTO edges      │
│  Storage: NVMe (hot) → S3 Standard → S3 Glacier (cold)     │
└─────────────────────────────────────────────────────────────┘
```

### 3.2 MCP Transport

The server supports two transport modes:

- **stdio** — default for Claude Code local usage (`~/.claude/settings.json` entry)
- **HTTP + SSE** — for remote / multi-user deployment, compatible with Claude.ai connectors and the Ferrosa DBaaS control plane

Authentication uses HTTP Basic (same credentials as CQL) in HTTP mode; stdio mode inherits the process owner's credentials.

### 3.3 Ferrosa Keyspace Layout

All tables live in the `agent_memory` keyspace with RF=3 (configurable per deployment).

---

## 4. Data Model

### 4.1 Memoization Cache (`memo_cache`)

**Purpose:** Avoid redundant LLM sub-calls for identical logical chunks. Implements the fix identified by the "Think, But Don't Overthink" paper.

```sql
CREATE TABLE agent_memory.memo_cache (
    content_hash     text,          -- SHA-256 of (normalized_prompt + context_slice)
    model_version    text,          -- e.g. 'gpt-5-mini-2026-01'
    tenant_id        uuid,
    result           text,          -- compressed sub-call output (NL capsule format)
    result_embedding vector<float, 768>,
    created_at       timestamp,
    last_hit_at      timestamp,
    hit_count        counter,       -- for eviction policy
    expires_at       timestamp,     -- application-managed TTL (pending native row TTL)
    PRIMARY KEY ((content_hash, model_version), tenant_id)
);

CREATE INDEX idx_memo_embedding ON agent_memory.memo_cache (result_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};
```

**Write path:** On each sub-call completion, the MCP server checks `memo_cache` by `content_hash`. Cache miss → execute LLM → write result. Cache hit → return immediately, increment `hit_count`.

**Eviction:** Sweep job deletes rows where `expires_at < now()`. Default TTL: 7 days for sub-call results, 30 days for fold summaries.

**Thundering herd mitigation:** Application-level idempotency key on first write; last-write-wins on concurrent misses (acceptable — results are deterministic for same hash). Flag for native `INSERT IF NOT EXISTS` (LWT) support tracked as a Ferrosa feature request.

### 4.2 Plan State (`plan_state`)

**Purpose:** Durable, hierarchically addressable storage for RLM plan trees. Implements ReCAP's structured re-injection of parent plans without loading full history into active context.

```sql
CREATE TABLE agent_memory.plan_state (
    session_id       uuid,
    depth            int,
    subtask_id       text,
    tenant_id        uuid,
    parent_subtask   text,
    goal_text        text,          -- NL capsule of goal at this level
    status           text,          -- 'pending' | 'active' | 'complete' | 'failed'
    outcome_summary  text,
    created_at       timestamp,
    completed_at     timestamp,
    PRIMARY KEY ((session_id, tenant_id), depth, subtask_id)
) WITH CLUSTERING ORDER BY (depth ASC, subtask_id ASC);

CREATE INDEX idx_plan_depth ON agent_memory.plan_state (depth)
    USING 'btree';
```

**Query pattern:** `WHERE session_id = ? AND tenant_id = ? AND depth <= ?` — O(depth) range scan, guaranteeing linear cost scaling with task depth (per ReCAP's theoretical requirement).

### 4.3 Trajectory Folds (`trajectory_folds`)

**Purpose:** Stores full sub-trajectory content with fold summaries. Implements Context-Folding's branch-and-collapse pattern. The graph layer provides a queryable fold hierarchy without loading full history.

```sql
CREATE TABLE agent_memory.trajectory_folds (
    session_id       uuid,
    fold_id          timeuuid,      -- ordering by creation time
    tenant_id        uuid,
    depth            int,
    parent_fold_id   timeuuid,
    raw_trajectory   text,          -- full REPL history for this sub-trajectory
    fold_summary     text,          -- NL capsule produced on fold completion
    fold_embedding   vector<float, 768>,
    token_count      int,
    compression_ratio float,
    status           text,          -- 'active' | 'folded' | 'archived'
    created_at       timestamp,
    folded_at        timestamp,
    PRIMARY KEY ((session_id, tenant_id), fold_id)
) WITH CLUSTERING ORDER BY (fold_id DESC);

CREATE INDEX idx_fold_embedding ON agent_memory.trajectory_folds (fold_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};
```

**Graph annotation:** Each fold row is exposed as a Cypher vertex; `FOLDED_INTO` edges connect child folds to their parent, enabling multi-hop traversal of the fold hierarchy.

```sql
ALTER TABLE agent_memory.trajectory_folds
    WITH extensions = {'graph.type': 'vertex', 'graph.label': 'Fold'};

-- Adjacency table auto-created by Ferrosa for FOLDED_INTO edges
```

**Tiering policy:** `status = 'folded'` rows have `raw_trajectory` compressed via LLMLingua WASM UDF on next SSTable flush. `status = 'archived'` rows transition to S3 Glacier via lifecycle rule after 30 days.

### 4.4 Entity Store (`entity_store`)

**Purpose:** Tracks named entities discovered during trajectory traversal. Supports phonetic matching for variant/noisy entity names (per Ferrosa's Double Metaphone index). Enables the BrowseComp+ multi-hop retrieval improvement described in the RLM analysis.

```sql
CREATE TABLE agent_memory.entity_store (
    tenant_id        uuid,
    entity_id        uuid,
    session_id       uuid,
    entity_name      text,
    entity_type      text,          -- 'person' | 'place' | 'event' | 'concept' | 'org'
    source_fold_id   timeuuid,
    context_snippet  text,
    entity_embedding vector<float, 768>,
    confidence       float,
    created_at       timestamp,
    PRIMARY KEY ((tenant_id, session_id), entity_id)
);

CREATE INDEX idx_entity_name_phonetic ON agent_memory.entity_store (entity_name)
    USING 'phonetic'
    WITH OPTIONS = {'algorithm': 'double_metaphone'};

CREATE INDEX idx_entity_embedding ON agent_memory.entity_store (entity_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};
```

**Graph annotation:** Entity vertices + `CO_OCCURS_WITH` and `MENTIONED_IN` edge types enabling Cypher multi-hop queries over the discovered knowledge graph, as validated by the Zep paper's temporal knowledge graph approach.

### 4.5 Temporal Events (`temporal_events`)

**Purpose:** First-class temporal chaining — the core primitive that Zep demonstrated achieves 18.5% accuracy improvement over static RAG on LongMemEval. Stores timestamped facts with supersession tracking.

```sql
CREATE TABLE agent_memory.temporal_events (
    tenant_id        uuid,
    entity_id        uuid,
    event_time       timestamp,
    event_id         timeuuid,
    fact_text        text,
    supersedes_id    timeuuid,      -- points to prior fact this replaces
    valid_until      timestamp,     -- null = currently valid
    source_session   uuid,
    confidence       float,
    PRIMARY KEY ((tenant_id, entity_id), event_time, event_id)
) WITH CLUSTERING ORDER BY (event_time DESC, event_id DESC);

CREATE INDEX idx_event_embedding ON agent_memory.temporal_events (fact_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};
```

**Temporal resolution:** When retrieving facts for an entity, the MCP server returns the most recent `valid_until IS NULL` row, with the full temporal chain available for explicit time-range queries. Supersession chains are graph-traversable via `SUPERSEDES` edges.

### 4.6 Feedback Store (`feedback_outcomes`)

**Purpose:** Stores (query, program_type, strategy, outcome) tuples for offline guideline refinement per SRLM and ACON. The accumulation of success/failure pairs enables learning better retrieval strategies per tenant and workload type.

```sql
CREATE TABLE agent_memory.feedback_outcomes (
    tenant_id        uuid,
    session_id       uuid,
    query_id         timeuuid,
    program_type     text,     -- 'hnsw_ann' | 'phonetic' | 'cypher_hop' | 'btree_range' | 'memo_hit'
    query_embedding  vector<float, 768>,
    task_complexity  text,     -- 'simple' | 'linear' | 'quadratic' (per RLM paper taxonomy)
    succeeded        boolean,
    latency_ms       int,
    token_cost       int,
    guideline_version text,
    created_at       timestamp,
    PRIMARY KEY ((tenant_id), created_at, query_id)
) WITH CLUSTERING ORDER BY (created_at DESC, query_id DESC);
```

**Usage:** Batch export via `SELECT * FROM feedback_outcomes WHERE tenant_id = ? AND succeeded = false` feeds failure pairs into an ACON-style guideline optimizer. The `program_type` distribution over time surfaces which retrieval strategies are being routed to and whether routing accuracy is improving.

---

## 5. MCP Tool Definitions

The server exposes 12 tools, organized into four functional groups.

### 5.1 Memoization Tools

**`check_memo_cache`**
```
Input:  { prompt: string, context_slice: string, model_version: string }
Output: { hit: bool, result?: string, hit_count?: int }
```
Computes `SHA-256(normalize(prompt) + context_slice)`, queries `memo_cache`. Returns cached result on hit; caller proceeds with LLM invocation on miss.

**`store_memo_result`**
```
Input:  { prompt: string, context_slice: string, model_version: string,
          result: string, embedding: float[], ttl_days?: int }
Output: { stored: bool, content_hash: string }
```
Writes sub-call result to `memo_cache` with embedding for fuzzy-hit detection. Applies NL compression before storage if `token_count > threshold`.

### 5.2 Plan State Tools

**`write_plan_node`**
```
Input:  { session_id: string, depth: int, subtask_id: string,
          parent_subtask?: string, goal_text: string }
Output: { written: bool }
```
Writes a plan node. Called by the root LLM at the start of each sub-task decomposition step.

**`get_plan_context`**
```
Input:  { session_id: string, max_depth?: int }
Output: { nodes: PlanNode[], active_path: string[] }
```
Retrieves the full plan tree up to `max_depth` (default: all). Returns a JSON tree the root LLM can inject into its prompt preamble — implementing ReCAP's structured re-injection without consuming the full context window.

**`update_plan_node`**
```
Input:  { session_id: string, depth: int, subtask_id: string,
          status: string, outcome_summary?: string }
Output: { updated: bool }
```
Marks a plan node complete/failed and writes an outcome summary for parent re-injection on recursive return.

### 5.3 Fold / Trajectory Tools

**`start_fold`**
```
Input:  { session_id: string, depth: int, parent_fold_id?: string,
          initial_context: string }
Output: { fold_id: string }
```
Creates a new active trajectory fold. Returns `fold_id` used to append subsequent REPL turns.

**`append_to_fold`**
```
Input:  { fold_id: string, repl_turn: string }
Output: { appended: bool, token_count: int }
```
Appends a REPL turn to an active fold. Surfaces `token_count` so the caller can decide whether to initiate a nested fold.

**`complete_fold`**
```
Input:  { fold_id: string, summary: string, embedding: float[] }
Output: { folded: bool, compression_ratio: float }
```
Seals the fold, writes the summary, creates the `FOLDED_INTO` graph edge to parent, and queues raw trajectory for background compression.

**`retrieve_fold_context`**
```
Input:  { session_id: string, query_embedding: float[], k?: int,
          include_raw?: bool }
Output: { folds: FoldSummary[] }
```
ANN search over `fold_embedding` to surface the most semantically relevant fold summaries for the current query. Returns summaries by default; `include_raw = true` loads full trajectory from S3 for deep inspection.

### 5.4 Entity & Retrieval Tools

**`upsert_entity`**
```
Input:  { session_id: string, entity_name: string, entity_type: string,
          context_snippet: string, embedding: float[],
          source_fold_id?: string, confidence?: float }
Output: { entity_id: string, is_new: bool }
```
Writes a discovered entity to `entity_store`. Returns existing `entity_id` if a phonetic match is found (prevents duplicate entity nodes from variant spellings).

**`retrieve_entities`**
```
Input:  { session_id: string, query: string, embedding?: float[],
          strategy?: 'ann' | 'phonetic' | 'both', k?: int }
Output: { entities: Entity[] }
```
Retrieves entities using the specified strategy. `phonetic` uses Double Metaphone for fuzzy name matching; `ann` uses HNSW cosine similarity; `both` union-merges and deduplicates results.

**`record_outcome`**
```
Input:  { session_id: string, query_id: string, program_type: string,
          task_complexity: string, succeeded: bool,
          latency_ms: int, token_cost: int }
Output: { recorded: bool }
```
Writes a feedback record to `feedback_outcomes`. Called after each retrieval or memo operation completes.

---

## 6. Tool Routing Layer (SRLM-Inspired)

A key design component is the **tool router** — a lightweight classifier that selects the optimal retrieval strategy before invoking storage. This implements the SRLM paper's finding that program selection is the primary performance driver.

### 6.1 Routing Logic

```
Input: { query_text, query_embedding, task_complexity, session_context }

Decision tree:
  1. Is content_hash in memo_cache?         → return cached result (memo_hit)
  2. Is query a named entity search?        → phonetic + ANN on entity_store
  3. Is query a plan-hierarchy traversal?   → btree range on plan_state
  4. Is query a fold-level semantic search? → HNSW ANN on trajectory_folds
  5. Is query a graph multi-hop?            → Cypher traversal on entity graph
  6. Default                                → HNSW ANN on trajectory_folds

Fallback chain: primary strategy → secondary → record failure pair
```

### 6.2 Routing Signals

The router uses four signals derived from the incoming tool call context:

- **Query embedding cosine similarity** to prior successful queries of each strategy type (from `feedback_outcomes`)
- **Task complexity classification** (`simple` / `linear` / `quadratic`) per RLM paper's complexity taxonomy
- **Entity name presence** — triggers phonetic path
- **Fractured CoT axes** — `k` and `include_raw` defaults scaled to observed task complexity

### 6.3 Learning Loop

The `feedback_outcomes` table accumulates (strategy, complexity, outcome) triples. A nightly batch job (implemented as a WASM UDF or external script) exports failure pairs and generates updated routing guidelines in NL format, versioned and stored in a `routing_guidelines` config table. This implements ACON's compression guideline optimization pattern applied to retrieval routing.

---

## 7. Security Model

Security is load-bearing for a multi-tenant memory server. The MemoryGraft paper demonstrated that a small number of poisoned records can dominate retrieval results by exploiting the semantic imitation heuristic. The MCPShield paper demonstrated that MCP tool trust is not enforced by default.

### 7.1 Tenant Isolation

- All tables partition on `(tenant_id, ...)` as the first partition key component
- The MCP server extracts `tenant_id` from the authenticated session — it is never client-supplied
- Ferrosa's Raft-based cluster mode and `LOCAL_QUORUM` consistency ensure tenant data does not cross shard boundaries

### 7.2 Memory Poisoning Defenses (MemoryGraft mitigations)

- **Write-time confidence gating:** `upsert_entity` and `store_memo_result` reject writes with `confidence < threshold` (configurable per tenant, default 0.7)
- **Union retrieval deduplication:** `retrieve_entities` deduplicates by entity identity before returning, preventing a single poisoned record from surfacing multiple times via lexical + embedding similarity
- **Anomaly detection table:** A materialized view tracks retrieval frequency per entity per session; entities retrieved at anomalously high rates (>3σ from session baseline) are flagged in `system_observability`
- **Append-only audit log:** All writes to `entity_store`, `trajectory_folds`, and `memo_cache` emit an append-only audit row. Writes cannot delete audit rows.

### 7.3 MCP Trust Layer (MCPShield pattern)

- Tool schemas are declared with explicit input constraints (enum values for `strategy`, max lengths for text fields, bounded float ranges for embeddings)
- The server rejects tool calls from unrecognized session origins before touching storage
- `record_outcome` is write-only; the feedback store cannot be queried through the MCP interface (only via direct CQL by the batch job, with separate credentials)

### 7.4 Privacy Considerations

Per "Unveiling Privacy Risks in LLM Agent Memory":

- All stored text is associated with `tenant_id`; cross-tenant queries are impossible at the CQL layer
- The `raw_trajectory` column in `trajectory_folds` is the highest-risk field (verbatim user content). It is compressed and tiered to Glacier within 24 hours of folding, minimizing its surface area in hot storage
- A `DELETE WHERE session_id = ? AND tenant_id = ?` cascade deletes all memory objects for a session on explicit request, in compliance with right-to-deletion obligations
- Embeddings are stored but not reversible to source text without the corresponding `fold_summary` or `context_snippet` columns — those are deleted with the parent row

---

## 8. Implementation Plan

### 8.1 Technology Stack

- **Language:** Rust (matches Ferrosa's stack; strong async/await via Tokio; WASM compilation target for UDFs)
- **MCP library:** `mcp-rs` or hand-rolled over `tokio` + `serde_json` (MCP protocol is simple JSON-RPC)
- **Ferrosa driver:** `cdrs-tokio` (Cassandra-compatible async CQL driver) or `scylla-rust-driver` depending on Ferrosa's wire compatibility
- **Embedding generation:** Call out to the Ollama endpoint (qwen3:32b or a dedicated embedding model on the RTX 3090 server) via HTTP; cache embeddings in `memo_cache` alongside results
- **Compression:** LLMLingua as a Python subprocess invoked via WASM boundary, or a Rust port of the core algorithm for the UDF

### 8.2 Milestones

**Phase 1 — Core Memoization (Week 1–2)**
- Ferrosa keyspace DDL (`memo_cache`, `plan_state`)
- MCP server skeleton: stdio transport, auth, tool dispatch
- `check_memo_cache` + `store_memo_result` tools
- `write_plan_node` + `get_plan_context` + `update_plan_node` tools
- Claude Code integration via `~/.claude/settings.json`
- Basic integration test: RLM trajectory on OOLONG-style task with and without memo cache

**Phase 2 — Fold Hierarchy (Week 3–4)**
- `trajectory_folds` DDL + HNSW index
- Graph annotation + `FOLDED_INTO` edge type in Ferrosa
- `start_fold` + `append_to_fold` + `complete_fold` + `retrieve_fold_context` tools
- Background compression job (WASM UDF or external)
- S3 Glacier lifecycle rule via `ferrosa-ctl`

**Phase 3 — Entity Graph (Week 5–6)**
- `entity_store` DDL + phonetic + HNSW indexes
- Graph annotation (`CO_OCCURS_WITH`, `MENTIONED_IN` edge types)
- `temporal_events` DDL + temporal chaining logic
- `upsert_entity` + `retrieve_entities` tools
- `record_outcome` tool + `feedback_outcomes` DDL

**Phase 4 — Routing & Learning (Week 7–8)**
- Tool routing layer with all five strategy paths
- Fractured CoT axis defaults per task complexity
- Nightly guideline refinement batch job
- Security hardening: write-time confidence gating, anomaly detection view, audit log
- HTTP+SSE transport for remote / Claude.ai connector deployment
- `DELETE session` cascade for right-to-deletion

### 8.3 Open Questions Before Phase 1

1. **Native row TTL in Ferrosa** — Does CQL `INSERT ... USING TTL` work in the current beta? If not, the expiry sweep job is the fallback.
2. **LWT / `INSERT IF NOT EXISTS`** — Is Paxos-style lightweight transactions implemented? Needed for thundering-herd-safe cache writes.
3. **Embedding model selection** — Use the dedicated `text-embedding-3-small` equivalent via API, or a local model on the RTX 3090 to avoid latency? Tradeoff: local is faster, API is consistent across machines.
4. **WASM UDF I/O limits** — Ferrosa docs cap WASM UDFs at no network/filesystem access. LLMLingua compression requires a model inference call. Confirm whether the compression step runs as an external sidecar rather than a WASM UDF.
5. **Ferrosa graph mutations from CQL** — Can `INSERT INTO` a CQL table annotated as a vertex automatically update the adjacency index, or does the graph layer require explicit Cypher `CREATE` statements?

---

## 9. Observability

All memory operations emit metrics to Ferrosa's native `/metrics` Prometheus endpoint (no extra exporter needed).

### 9.1 Key Metrics

| Metric | Type | Description |
|---|---|---|
| `ferrosa_memory_memo_hits_total` | Counter | Cache hits by `model_version`, `tenant_id` |
| `ferrosa_memory_memo_misses_total` | Counter | Cache misses — triggers LLM sub-call |
| `ferrosa_memory_fold_token_count` | Histogram | Token count per fold at completion time |
| `ferrosa_memory_fold_compression_ratio` | Histogram | Compression ratio achieved by NL capsule |
| `ferrosa_memory_retrieval_latency_ms` | Histogram | Latency by strategy (`ann`, `phonetic`, `cypher`, `btree`) |
| `ferrosa_memory_routing_strategy_total` | Counter | Strategy selected by router per task complexity |
| `ferrosa_memory_poisoning_flags_total` | Counter | Anomaly detection triggers (MemoryGraft defense) |
| `ferrosa_memory_entity_upserts_total` | Counter | New vs. matched entity upserts (phonetic match rate) |

### 9.2 Virtual Table Views

The MCP server populates a `system_observability.memory_summary` virtual table per tenant, queryable via standard CQL:

```sql
SELECT * FROM system_observability.memory_summary
WHERE tenant_id = ?;
```

Returns: memo hit rate, active fold count, entity graph size, feedback success rate by strategy, estimated cost savings from cache hits (in tokens).

### 9.3 SUBSCRIBE Integration

```sql
-- Real-time stream of anomaly flags
SUBSCRIBE SELECT * FROM system_observability.memory_summary
WHERE tenant_id = ? DELTA;
```

Enables live alerting on memory poisoning detection events and cache efficiency regressions without polling.

---

## 10. Configuration

Configuration is via a `ferrosa-memory.toml` file alongside the binary.

```toml
[server]
transport = "stdio"          # or "http"
http_port = 8765
log_level = "info"

[ferrosa]
contact_points = ["localhost:9042"]
keyspace = "agent_memory"
replication_factor = 3
consistency = "LOCAL_QUORUM"

[memory]
default_ttl_days = 7
fold_ttl_days = 30
archive_after_days = 30
compression_threshold_tokens = 512   # compress folds above this size
confidence_gate = 0.7                # minimum confidence for entity writes
max_memo_results = 50                # max ANN results for fuzzy cache hits

[embeddings]
provider = "ollama"                  # or "openai" | "local"
ollama_base_url = "http://gpu-server:11434"
model = "nomic-embed-text"
dimensions = 768

[security]
audit_log_enabled = true
anomaly_detection_enabled = true
anomaly_sigma_threshold = 3.0

[routing]
guideline_version = "v1"
feedback_export_cron = "0 2 * * *"  # 2am nightly
```

---

## 11. Non-Goals (v1.0)

- **Training / fine-tuning pipeline** — The feedback store accumulates data but the guideline optimizer is an offline batch job, not an online RL loop. Full ACON-style RL training is post-v1.
- **Cross-tenant memory sharing** — Intentionally excluded. No mechanism for sharing memory across tenant boundaries, even with explicit consent.
- **Embedding model hosting** — The server calls out to an existing embedding endpoint; it does not run a model itself.
- **Web UI** — Observability surfaces via Ferrosa's existing web console and `/metrics` endpoint. No dedicated memory dashboard in v1.
- **OpenTelemetry traces** — Prometheus metrics only in v1. OTel traces for Claude Code / Cowork compatibility are a v2 item.

---

## 12. References

| Paper | arXiv | Informs |
|---|---|---|
| Recursive Language Models | 2512.24601 | Core paradigm |
| SRLM | 2603.15653 | Routing layer, program selection |
| Think, But Don't Overthink | 2603.02615 | Memoization requirement |
| Continuum Memory Architecture | 2601.09913 | Memory primitive taxonomy |
| MIRIX | 2507.07957 | Six memory type framework |
| Zep | 2501.13956 | Temporal chaining design |
| MemR³ | 2512.20237 | Closed-loop retrieval control |
| Context-Folding | 2510.11967 | Fold/summarize pattern |
| ReCAP | 2510.23822 | Plan state schema, linear depth scaling |
| MARINE | 2512.07898 | Persistent reference trajectory |
| THREAD | 2405.17402 | WASM UDF thread model |
| LLMLingua | 2310.05736 | Compression for tiered storage |
| NL-Compress | 2402.18700 | Model-agnostic capsule compression |
| ACON | 2510.00615 | Feedback/guideline learning loop |
| Fractured CoT | 2505.12992 | Compute axis tuning |
| Prompt Cache | 2311.04934 | KV-cache reuse for sub-call prefixes |
| MCPShield | 2602.14281 | MCP trust, tool validation |
| MemoryGraft | 2512.16962 | Memory poisoning attack model |
| Unveiling Privacy Risks | 2502.13172 | Privacy threat model |
| Terrarium | 2510.14312 | Multi-agent shared memory safety |
