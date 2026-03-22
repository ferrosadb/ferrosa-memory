# ferrosa-memory-mcp — Architecture Specs

## Index

| Document | Description |
|----------|-------------|
| [overview.md](overview.md) | System overview, positioning, and high-level Mermaid diagrams |
| [components.md](components.md) | Component architecture — modules, responsibilities, interfaces |
| [data-flow.md](data-flow.md) | Data flow diagrams — tool call paths, storage paths, retrieval paths |
| [threat-model.md](threat-model.md) | STRIDE threat analysis with trust boundaries |
| [project-plan.md](project-plan.md) | Timeboxed sprint plan prioritized by risk |
| [decisions/](decisions/) | Architecture Decision Records |

| [dsm-analysis.md](dsm-analysis.md) | Design Structure Matrix — module boundaries and coupling |
| [fmea.md](fmea.md) | Failure Mode and Effects Analysis with RPN scoring |

## Source

All specs derived from `ferrosa-memory-mcp-spec.md` (v0.1, 2026-03-21).

## Update History

- **2026-03-21 (init):** Full 5-phase blueprint created
- **2026-03-21 (update):** Drift detected after 8 commits. Updated: graph_client HTTP refactor, DSM M11 decoupling, vector column gap (F31), graph edge write gap (F32), sprint completion tracking, risk register updates
