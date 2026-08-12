---
title: Feature Work Item - Bounded Tool Catalog Pagination
executive_summary:
  purpose: >-
    Tracks implementation of the accepted bounded and searchable all_tools
    architecture.
  critical_items:
    - Preserve all_tools while changing its response to compact pagination.
    - Enforce the 16 KiB final-result limit and page-plus-one source bound.
    - Implement through the focused compiled plan and Forge checklist.
status: implemented-awaiting-independent-review
last_updated: 2026-08-12
priority: P0
source_location: crates/ferrosa-memory-core/src/dispatch
---

# Feature: Bounded Tool Catalog Pagination

The current 95-definition `all_tools` result is approximately 132 KiB and is
materialized in full. Implement the accepted architecture in
[the focused blueprint](../tool-catalog-pagination/README.md).

## Definition of done

- `all_tools` remains public and provides compact/search/schema/named modes.
- Every catalog result is at most 16,384 UTF-8 bytes at its defined final result
  boundary and splits only between complete entries.
- Cursors are versioned, surface/query bound, stale-recoverable, and untrusted.
- Static discovery never constructs the complete catalog and retains only the
  bounded page plus one bounded static family; any future database source must
  push cursor/filter/projection/limit into its query.
- MCP list, operator/workbench, eval, and setup clients use the shared paging
  behavior.
- The test layers and rollout gates in the compiled plan pass.

## Execution source

- [Compiled human plan](../tool-catalog-pagination/compiled-project-plan.md)
- Forge checklist: `.forge/checklists/bounded-tool-catalog-pagination.json`
- [ADR-008](../decisions/adr-008-bounded-tool-catalog-pagination.md)

## Implementation notes

Implemented test-first across the catalog core, MCP surfaces, operator route,
workbench, setup probe, and eval client. The final implementation gates include:

- 1,194 passing core unit tests, 387 passing eval unit tests, and 45 passing
  sync unit tests (10 live/opt-in tests ignored)
- five focused catalog contract tests plus affected workbench and client tests
- workspace check and strict Clippy with zero warnings
- MCP Inspector stdio verification of compact search, bounded first-page output,
  versioned continuation, and named schema discovery
- zero focused materialization, secret, high-confidence threat/fail-loud, or
  high-severity dependency findings

An independent reviewer should still verify the branch before this work item is
moved to `implemented/`. The larger synthetic 10,000-entry and duration/RSS
campaign in AT-108 is a rollout gate, not a prerequisite for the bounded static
catalog implementation.
