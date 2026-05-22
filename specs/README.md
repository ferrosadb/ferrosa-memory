# ferrosa-memory-mcp — Architecture Specs

## Overview

ferrosa-memory-mcp is a Rust MCP server and operator console that should act as a role-scoped client to Ferrosa, not as an embedded storage engine. It provides durable, structured memory workflows for Recursive Language Model (RLM) workloads — memoization, hierarchical plan state, trajectory fold/summarization, semantic retrieval, phonetic entity search, spreading activation, dream consolidation, intention tracking, Datalog-oriented reasoning, and an expert-system knowledge plane for rules, claims, approvals, aliases, and explanations.

```mermaid
%%{init: {'theme':'dark','themeVariables':{'primaryColor':'#16161f','primaryTextColor':'#e8e8ed','primaryBorderColor':'#e2725b','lineColor':'#9494a3','secondaryColor':'#1c1c28','tertiaryColor':'#111118','clusterBkg':'#111118','clusterBorder':'#1e1e2a','edgeLabelBackground':'#111118','nodeTextColor':'#e8e8ed'}}}%%
graph LR
    subgraph Clients
        CC[Claude Code]
        CA[Claude.ai]
        TP[Third-party MCP clients]
    end

    subgraph MCP["ferrosa-memory-mcp"]
        TR[Client Router]
        AT[Auth / Tenant Isolation]
        UI[Workbench / MCP Surface]
    end

    subgraph DB["Ferrosa DB"]
        CQL[CQL API]
        SPQ[SPARQL API]
        CYP[Cypher / Graph API]
        DAT[Datalog API]
    end

    CC -->|stdio| TR
    CA -->|HTTP+SSE| TR
    TP -->|HTTP+SSE| TR
    TR --> AT
    AT --> UI
    UI -->|app-table reads/writes via CQL (app_reader)| CQL
    UI -->|public query calls| SPQ
    UI -->|public query calls| CYP
    UI -->|public query calls| DAT
```

**Boundary rule:** `ferrosa-memory` may use Ferrosa's public wire protocols, including direct CQL via the Rust driver, but it must not treat graph-owned backing tables as a public API. Workbench query surfaces should be passthrough clients, and bugs in those public interfaces should be fixed in Ferrosa rather than papered over locally.

**Current implementation note:** the runtime still contains direct `CqlStorage` coupling in places. See [overview.md](overview.md) and [dsm-analysis.md](dsm-analysis.md) for the current-vs-target gap and refactor direction.

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
| [expert-system-knowledge-plane.md](expert-system-knowledge-plane.md) | ferrosa-memory-side expert-system architecture — rule registry, claims, approvals, aliases, derived facts, explanations |
| [remote-memory.md](remote-memory.md) | Decision record for teacher/learner remote memory transfer, trust boundaries, signed provenance, stubs, and negative feedback |
| [memory-remotes-teacher-learner.md](memory-remotes-teacher-learner.md) | Full blueprint for Ferrosa Memory Remotes: policy-governed knowledge teaching between fmem instances, including Packet K MCP/CLI remote-management schemas |
| [plans/memory-remotes-project-plan.md](plans/memory-remotes-project-plan.md) | Multi-agent TDD project plan for implementing memory remotes |

## Source

All specs derived from `ferrosa-memory-mcp-spec.md` (v0.1, 2026-03-21).

## Update History

- **2026-03-21 (init):** Full 5-phase blueprint created
- **2026-03-21 (update):** Drift detected after 8 commits. Updated: graph_client HTTP refactor, DSM M11 decoupling, vector column gap (F31), graph edge write gap (F32), sprint completion tracking, risk register updates
- **2026-04-01 (update):** Dynamic type registry (DDL 019), multiselect filter UI in viz, extended entity/edge color mapping, CO_OCCURS noise filtering, ghost row resilience, stale prepared statement recovery, NER module, frg ingest data flow, markdown docs ingestion support
- **2026-04-10 (update):** Shared HTTP deployment blueprint: real auth boundary, TLS/secret handling, multi-tenant policy, liveness/readiness probes, and viz exposure decision
- **2026-04-19 (update):** Expert-system knowledge plane review: effective-rule-set gap, core ownership of claims/approvals/aliases, operator console above viz, explanation API risks, and Sprint 8 planning
