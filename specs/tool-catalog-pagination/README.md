---
title: Bounded Tool Catalog Pagination Blueprint
executive_summary:
  purpose: >-
    Indexes the focused blueprint for bounded, searchable MCP tool discovery in
    Ferrosa Memory.
  critical_items:
    - The current 95-tool catalog produces an approximately 132 KiB inner result.
    - Every implemented catalog surface caps its stable caller-visible result at 16 KiB.
    - The static catalog is traversed family-lazily; no complete production catalog is retained.
status: implemented-awaiting-independent-review
last_updated: 2026-08-12
---

# Bounded Tool Catalog Pagination Blueprint

This bundle defines the architecture, risks, tests, and executable work packets
for replacing unbounded tool-catalog responses with source-level pagination.

## Implementation status

Implemented on `feature/bounded-tool-catalog-pagination`:

- `all_tools` compact/search/category/schema/named modes with versioned cursors
- legacy and modern MCP `tools/list` pagination, including paginated
  `include_all` compatibility
- direct operator pagination, incremental workbench loading, and selected-name
  schema fetch
- bounded setup/eval traversal with cursor-cycle and 256-page guards
- exact surface-envelope admission, single-copy canonical text output, typed restart data,
  contract tests, and MCP Inspector evidence

The present registry is static, not CQL-backed. It emits one bounded family at
a time and never retains the complete catalog. If definitions later move to a
database, ADR-008 requires keyset position, filters, projection, and limit to be
pushed into that query; server-side fetch-all remains forbidden.

## Documents

| Document | Purpose |
|---|---|
| [Overview](overview.md) | Current state, target state, scope, and acceptance gates |
| [Components](components.md) | Interfaces, ownership, and adapter boundaries |
| [Data flow](data-flow.md) | Search, pagination, named detail, and stale-cursor flows |
| [DSM analysis](dsm-analysis.md) | Dependency impact and target module boundary |
| [Threat model](threat-model.md) | STRIDE analysis and required controls |
| [FMEA](fmea.md) | Failure modes, RPN scores, and adversarial tests |
| [Project plan](project-plan.md) | Risk-prioritized delivery plan |
| [Test specification](test-specification.md) | Seven-layer test plan and traceability |
| [Compiled project plan](compiled-project-plan.md) | Agent-executable task packets and dependency DAG |

## Related decisions

- [Blueprint decisions](../decisions.md)
- [Decision tree](../decision-tree.md)
- [ADR-008](../decisions/adr-008-bounded-tool-catalog-pagination.md)
