---
title: Bounded MCP Tool Catalog Decision Tree
executive_summary:
  purpose: >-
    Tracks the dependency-ordered decisions required to blueprint a bounded,
    paginated all_tools contract.
  critical_items:
    - The public all_tools name and paginated default behavior are locked.
    - Pages are capped at 16 KiB and split only between complete tool entries.
    - Named schema lookup and pagination hints remain inside all_tools.
    - Cursors embed a catalog version and stale cursors return restart hints.
    - Every catalog surface paginates at the source without full materialization.
    - Deterministic search is integrated into all_tools.
    - No stakeholder decision blocks technical analysis.
status: complete
last_updated: 2026-08-12
---

# Bounded MCP Tool Catalog Decision Tree

The tree separates confirmed constraints from decisions that still affect the
API contract, failure behavior, and verification plan.

```mermaid
flowchart TD
    A[Keep all_tools public name] --> B[Compact paginated default]
    B --> C{Serialized response budget}
    C[16 KiB semantic page budget] --> D{Schema detail contract}
    D[Compact discovery plus named schema detail and hints] --> S[Deterministic source-level search]
    S --> E{Cursor behavior across catalog revisions}
    E[Versioned cursor plus stale restart hint] --> F{Catalog surface scope}
    F[Shared source-level pagination boundary] --> G[Test and rollout gates]

    A:::locked
    B:::locked
    C:::locked
    D:::locked
    S:::locked
    E:::locked
    F:::locked
    G:::derived

    classDef locked fill:#d7f5df,stroke:#18753c,color:#102a18
    classDef open fill:#fff4ce,stroke:#8a6116,color:#332400
    classDef derived fill:#dbeafe,stroke:#1d4ed8,color:#172554
```

## Confirmed branch

1. Preserve the `all_tools` name.
2. Return a compact first page instead of the complete catalog.
3. Return an explicit continuation cursor for subsequent pages.
4. Cap each serialized page at 16 KiB.
5. Split pages only between complete tool entries and fill toward the byte cap.
6. Keep compact discovery and full schema detail inside `all_tools`.
7. Allow direct schema lookup with `names: [...]`.
8. Return actionable next-call hints with every paginated response.
9. Embed a server-owned catalog version in every cursor.
10. Reject stale cursors with a typed result and exact restart hint.
11. Reuse one paginator across `all_tools`, MCP `tools/list`, and operator
    catalog responses.
12. Page at the catalog source, retaining only the current page and bounded
    look-ahead rather than materializing the complete result.
13. Require database cursor/limit pushdown for any database-backed catalog
    source.
14. Add deterministic lexical `query` and exact category filters to `all_tools`
    rather than adding a separate `tool_search` tool.
15. Bind normalized search and detail arguments into the cursor fingerprint.

## Derived implementation gates

The confirmed decisions imply deterministic contract, boundary, memory, and
client-integration tests. Technical analysis will define exact tests and rollout
ordering without another product decision.

## Remaining questions

No stakeholder question remains. Live client compatibility and performance
measurements remain verification work rather than architecture ambiguity.
