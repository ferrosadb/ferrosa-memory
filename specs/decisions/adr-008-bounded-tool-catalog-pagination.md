---
title: ADR-008 Bounded Searchable Tool Catalog Pagination
executive_summary:
  purpose: >-
    Records the decision to preserve all_tools while replacing full-catalog
    materialization with source-level, versioned, 16 KiB pagination.
  critical_items:
    - all_tools remains the single searchable discovery tool.
    - All catalog surfaces share semantics but retain protocol-native envelopes.
    - No database migration is required for the current static registry.
status: accepted
date: 2026-08-12
---

# ADR-008: Bounded Searchable Tool Catalog Pagination

The Ferrosa Memory catalog has grown beyond the safe output budget of common
MCP clients. This ADR records the target discovery boundary.

## Context

The current `all_tools` call returns every complete definition. The 95-tool
snapshot produces an approximately 132 KiB inner result, and tool-call wrapping
duplicates object data into text and structured content. `tools/list` and the
operator endpoint also construct the complete catalog before filtering or
returning it.

## Decision

Preserve `all_tools` and turn it into the single compact/search/detail discovery
surface. Introduce one lazy catalog source and semantic paginator shared by
`all_tools`, MCP `tools/list`, and operator HTTP adapters. Cap each exact final
surface response at 16,384 UTF-8 bytes and split only between complete entries.

Use deterministic lexical search, exact category filtering, and named schema
lookup. Use opaque, catalog-versioned cursors bound to surface visibility and
the normalized request fingerprint. Every page includes actionable navigation
hints. Stale cursors fail loudly with exact restart arguments.

The current static source must construct only page entries plus one look-ahead.
A future database source must push stable ordering, cursor, filters, projection,
and limit into the database operation.

## Consequences

- Existing callers that assume one complete response must follow pagination or
  use named lookup.
- The full-catalog snapshot remains test-only compatibility evidence.
- Dispatch loses catalog-construction responsibility and consumes registry
  metadata for execution-name resolution.
- Surface adapters must size their final envelopes independently because
  `all_tools`, modern MCP, legacy MCP, and operator JSON add different overhead.
- Dynamic entity types participate in catalog versioning and replica readiness.
- No current database schema or migration changes.

## Rejected alternatives

- **Add `tool_search`:** fragments discovery and grows the compact default
  surface. Search belongs in `all_tools`.
- **Remove repeated output schemas only:** saves about 48 KiB but leaves the
  result well above client limits.
- **Paginate after building a vector:** reduces output tokens but not server
  memory or work.
- **Numeric offset cursors:** unstable under catalog changes and expensive for
  future database-backed sources.
- **Silent cursor restart:** risks duplicates and omissions.
- **Vector or LLM search:** unnecessary cost and unstable ranking for the
  current catalog size.

## Verification

The [test specification](../tool-catalog-pagination/test-specification.md)
defines exact envelope-size, traversal, source-pull, stale-recovery,
cross-surface, client, property, and duration gates.
