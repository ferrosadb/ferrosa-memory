# System Overview

## Purpose

ferrosa-memory-mcp is a Rust MCP server (~14,400 lines) that exposes Ferrosa DB's index and graph infrastructure as typed tools for LLM agent trajectories. It provides durable, structured memory for Recursive Language Model (RLM) workloads — memoization, hierarchical plan state, trajectory fold/summarization, semantic retrieval, phonetic entity search, spreading activation, dream consolidation, intention tracking, Datalog graph inference with provenance-tracked derived knowledge, recursive multi-pass exploration, and a feedback loop for retrieval strategy refinement.

## Positioning

```mermaid
%%{init: {'theme':'dark','themeVariables':{'primaryColor':'#16161f','primaryTextColor':'#e8e8ed','primaryBorderColor':'#e2725b','lineColor':'#9494a3','secondaryColor':'#1c1c28','tertiaryColor':'#111118','clusterBkg':'#111118','clusterBorder':'#1e1e2a','edgeLabelBackground':'#111118','nodeTextColor':'#e8e8ed'}}}%%
graph LR
    subgraph Clients
        CC[Claude Code]
        CA[Claude.ai]
        TP[Third-party MCP clients]
    end

    subgraph "ferrosa-memory-mcp (this project)"
        TR[Tool Router]
        AT[Auth / Tenant Isolation]
        CU[Compression Engine]
    end

    subgraph "Ferrosa DB"
        KS[agent_memory keyspace]
        IX[Indexes: HNSW, B-tree, Phonetic, Hash]
        GR[Graph: Cypher, adjacency index]
        ST[Storage: NVMe -> S3 -> Glacier]
    end

    CC -->|MCP stdio| TR
    CA -->|MCP HTTP+SSE| TR
    TP -->|MCP HTTP+SSE| TR
    TR --> AT
    AT -->|CQL + Cypher| KS
    CU -->|compress before write| KS
    KS --- IX
    KS --- GR
    KS --- ST
```

| Layer | Role | Owns |
|-------|------|------|
| Claude Code (RLM runtime) | Orchestrates agent loop, spawns sub-agents | Prompt construction, recursion control |
| ferrosa-memory-mcp (this project) | Typed memory interface, MCP protocol | Tool schemas, query translation, auth |
| Ferrosa DB (storage) | Durable store, indexes, graph | Data, indexes, replication, S3 tiering |

## Transport

- **stdio** — default for Claude Code local usage (`~/.claude/settings.json`)
- **HTTP + SSE** — remote / multi-user deployment, Claude.ai connectors

Authentication: HTTP Basic (same credentials as CQL) in HTTP mode; stdio inherits process owner credentials.

## Technology Stack

- **Language:** Rust (Tokio async runtime)
- **MCP protocol:** JSON-RPC over stdio or HTTP+SSE
- **CQL driver:** `cdrs-tokio` v9 (selected over `scylla-rust-driver` — see risk register)
- **Graph client:** HTTP POST to Ferrosa's `/graph/query` endpoint via `reqwest` (neo4rs Bolt v4 incompatible with Ferrosa Bolt v5)
- **Compression:** Rust-native implementation (no Python — LLMLingua algorithm ported to Rust)
- **Embedding:** HTTP call to Ollama endpoint (nomic-embed-text, 768 dimensions)
- **Serialization:** `serde` + `serde_json`

## Workspace Structure

```
ferrosa-memory/
├── crates/
│   ├── ferrosa-memory-core/          # Shared library: storage traits, tool handlers, config
│   ├── ferrosa-memory-mcp/    # MCP server binary (stdio + HTTP)
│   └── ferrosa-memory-batch/  # Nightly batch job binary
│   ├── ferrosa-memory-sync/    # Cross-device sync binary
├── ddl/                       # CQL schema files (001–019)
├── docker-compose.yml         # Dev cluster via podman compose (3-node Ferrosa + RustFS + Ollama)
└── specs/                     # Architecture documentation
```

## Keyspace

All tables in `agent_memory` keyspace, RF=3 (configurable).

Eight tables:
1. `memo_cache` — sub-call memoization
1. `plan_state` — hierarchical plan trees
1. `trajectory_folds` — fold summaries with graph edges
1. `entity_store` — named entities with phonetic + vector indexes
1. `temporal_events` — timestamped facts with supersession chains
1. `feedback_outcomes` — retrieval strategy success/failure pairs
1. `entity_types` — dynamic type registry for entity types (DDL 019)
1. `edge_types` — dynamic type registry for edge types with source/destination constraints (DDL 019)

## Dynamic Type Registry

Entity and edge types are stored in `entity_types` and `edge_types` tables rather than hardcoded in the binary. At MCP startup, `CqlStorage::load_entity_types()` and `load_edge_types()` query these tables and build the MCP tool schemas dynamically. New types can be added by inserting a row — no recompile required.

Seeded types include: person, place, event, concept, org, decision, pattern, preference, bug, document, section. Edge types: depends_on, contains, calls, references, CO_OCCURS, MENTIONED_IN, FOLDED_INTO, SUPERSEDES.

## Datalog Inference and Recursive Exploration

The Datalog inference layer operates as a logical view over existing CQL storage. Existing entity edges are normalized into canonical predicates (`edge(Src, Pred, Dst)`, `node(Id)`) at query time, enabling semi-naive evaluation of transitive closure, taxonomy hierarchies, and co-occurrence clustering without migrating the underlying schema. Derived facts carry provenance chains and are cached with TTL in ephemeral CQL tables.

The recursive exploration engine builds on the Datalog layer to support multi-pass retrieval. Complex queries are decomposed into sub-queries, each pass runs 5-signal RRF fusion (phonetic, ANN, fold, warmth, PageRank), then Datalog evaluation discovers connected entity clusters via `related()`, `cluster()`, and `reachable()` derived predicates. Convergence is detected when the Datalog fixpoint produces no new facts or novelty drops below a configurable threshold. Persistent warmth (Ebbinghaus decay with zone-based multipliers) and Personalized PageRank scores feed into the fusion pipeline to bias retrieval toward frequently accessed and structurally important entities.

## Research Foundation

Design grounded in: RLM (memoization), SRLM (tool routing), ReCAP (plan state), Context-Folding (trajectory folds), Zep (temporal chaining), MIRIX (memory type taxonomy), ACON (feedback learning), MCPShield + MemoryGraft (security model), Recursive Memory Harness (persistent warmth, recursive exploration), and Datalog graph materialization (semi-naive inference, provenance, ephemeral caching). See spec Section 2 for full citations.
