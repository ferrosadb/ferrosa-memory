# ferrosa-memory-mcp — Architecture Specs

## Overview

ferrosa-memory-mcp is a Rust MCP server (~14,400 lines) that exposes Ferrosa DB's index and graph infrastructure as typed memory tools for LLM agent trajectories. It provides durable, structured memory for Recursive Language Model (RLM) workloads — memoization, hierarchical plan state, trajectory fold/summarization, semantic retrieval, phonetic entity search, spreading activation, dream consolidation, intention tracking, and a feedback loop for retrieval strategy refinement.

```mermaid
%%{init: {'theme':'dark','themeVariables':{'primaryColor':'#16161f','primaryTextColor':'#e8e8ed','primaryBorderColor':'#e2725b','lineColor':'#9494a3','secondaryColor':'#1c1c28','tertiaryColor':'#111118','clusterBkg':'#111118','clusterBorder':'#1e1e2a','edgeLabelBackground':'#111118','nodeTextColor':'#e8e8ed'}}}%%
graph LR
    subgraph Clients
        CC[Claude Code]
        CA[Claude.ai]
        TP[Third-party MCP clients]
    end

    subgraph MCP["ferrosa-memory-mcp"]
        TR[Tool Router]
        AT[Auth / Tenant Isolation]
        CU[Compression Engine]
    end

    subgraph DB["Ferrosa DB"]
        KS[agent_memory keyspace]
        IX["HNSW · Phonetic · B-tree"]
        GR["Graph: Cypher + adjacency index"]
        ST["NVMe → S3 → Glacier"]
    end

    CC -->|stdio| TR
    CA -->|HTTP+SSE| TR
    TP -->|HTTP+SSE| TR
    TR --> AT
    AT -->|CQL + Cypher| KS
    CU -->|compress before write| KS
    KS --- IX
    KS --- GR
    KS --- ST
```

**Stack:** Rust (Tokio), cdrs-tokio for CQL, reqwest for HTTP Cypher, Ollama for embeddings (nomic-embed-text, 768d). No Python anywhere — all algorithms (LLMLingua compression, spreading activation, dream consolidation) ported to native Rust.

**32+ MCP tools** across 8 functional groups: memoization, plan state, trajectory folds, entity graph, temporal chains, feedback/routing, cognitive memory (spreading activation, dream consolidation, importance scoring, intention tracking), and hybrid search. Entity type schemas are dynamic — loaded from the `entity_types` registry table at startup.

## Index

| Document | Description |
|----------|-------------|
| [overview.md](overview.md) | System overview, positioning, and high-level Mermaid diagrams |
| [components.md](components.md) | Component architecture — modules, responsibilities, interfaces |
| [data-flow.md](data-flow.md) | Data flow diagrams — tool call paths, storage paths, retrieval paths |
| [threat-model.md](threat-model.md) | STRIDE threat analysis with trust boundaries |
| [project-plan.md](project-plan.md) | Timeboxed sprint plan prioritized by risk |
| [shared-http-deployment.md](shared-http-deployment.md) | Production HTTP deployment blueprint: auth, TLS, probes, tenant policy, viz boundary |
| [decisions/](decisions/) | Architecture Decision Records |

| [memory-lifecycle.md](memory-lifecycle.md) | Memory consolidation, forgetting, importance decay, and state machine |
| [visualization.md](visualization.md) | Real-time graph dashboard — WebSocket protocol, D3.js frontend, event types |
| [dsm-analysis.md](dsm-analysis.md) | Design Structure Matrix — module boundaries and coupling |
| [fmea.md](fmea.md) | Failure Mode and Effects Analysis with RPN scoring |
| [lsp-code-indexing.md](lsp-code-indexing.md) | LSP-based code indexing spec for structural codebase ingestion |

## Source

All specs derived from `ferrosa-memory-mcp-spec.md` (v0.1, 2026-03-21).

## Update History

- **2026-03-21 (init):** Full 5-phase blueprint created
- **2026-03-21 (update):** Drift detected after 8 commits. Updated: graph_client HTTP refactor, DSM M11 decoupling, vector column gap (F31), graph edge write gap (F32), sprint completion tracking, risk register updates
- **2026-04-01 (update):** Dynamic type registry (DDL 019), multiselect filter UI in viz, extended entity/edge color mapping, CO_OCCURS noise filtering, ghost row resilience, stale prepared statement recovery, NER module, frg ingest data flow, markdown docs ingestion support
- **2026-04-10 (update):** Shared HTTP deployment blueprint: real auth boundary, TLS/secret handling, multi-tenant policy, liveness/readiness probes, and viz exposure decision
