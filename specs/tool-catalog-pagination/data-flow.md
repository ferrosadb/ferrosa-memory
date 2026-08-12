---
title: Bounded Tool Catalog Data Flow
executive_summary:
  purpose: >-
    Specifies request normalization, source-level search, semantic page packing,
    continuation, named schema lookup, and stale recovery flows.
  critical_items:
    - Cursor and query validation occur before source reads.
    - The source produces only bounded candidates and the encoder admits complete entries.
    - Database pushdown is mandatory only if a database-backed catalog source is introduced.
status: draft
last_updated: 2026-08-12
---

# Bounded Tool Catalog Data Flow

All catalog surfaces share semantic selection and paging, but each surface owns
its final protocol envelope.

## Compact search and continuation

```mermaid
sequenceDiagram
    participant C as Caller
    participant A as Surface adapter
    participant N as Query normalizer
    participant K as Cursor codec
    participant S as Catalog source
    participant P as Paginator and encoder

    C->>A: all_tools query=remote detail=compact
    A->>N: arguments plus server visibility
    N-->>A: normalized query and fingerprint
    A->>K: validate optional cursor before source access
    K-->>A: catalog version and after-key
    A->>S: seek after-key and apply query/categories
    loop Until next entry would exceed final 16 KiB envelope
        S-->>P: one complete entry
        P->>P: encode candidate final response
    end
    S-->>P: one look-ahead entry
    P-->>A: page, next cursor, and exact next-call hint
    A-->>C: surface-native bounded response
```

Matching precedence is explicit and stable: exact public name, public-name
prefix, exact family/category or tag, then summary token match. Public name is
the final tie-breaker. The source skips nonmatches incrementally; it never
constructs their schema projection.

## Named schema jump

```mermaid
sequenceDiagram
    participant C as Caller
    participant A as all_tools adapter
    participant S as Catalog source
    participant P as Paginator

    C->>A: detail=schema names=[search, ingest]
    A->>S: exact bounded descriptor lookup
    S-->>P: selected definitions only
    P-->>A: one or more semantic pages
    A-->>C: schemas plus completion or next hint
```

Unknown names fail structurally. Duplicate names are rejected or normalized
according to the contract tests; they never trigger a full scan.

## Stale cursor recovery

```mermaid
sequenceDiagram
    participant C as Caller
    participant A as Adapter
    participant K as Cursor codec
    participant S as Catalog source

    C->>A: cursor from catalog version N
    A->>K: decode and compare with current version N+1
    K-->>A: STALE_CURSOR before source access
    A-->>C: current version plus exact restart arguments and hint
    C->>A: restart without stale cursor
    A->>S: first page under version N+1
```

## Surface mappings

| Semantic field | `all_tools` | MCP `tools/list` | Operator HTTP |
|---|---|---|---|
| Continuation request | `cursor` | `cursor` | `cursor` |
| Continuation response | `next_cursor` | `nextCursor` | `next_cursor` |
| Version | `catalog_version` | `_meta.catalogVersion` | `catalog_version` |
| Hint | `hint` | `_meta.paginationHint` | `hint` |
| Visibility | Server full-discovery policy | Server tier policy | Server operator policy |
| Projection | Compact or schema | Schema | Compact, then named schema |

## Future database-backed source

The present catalog is static and requires no database work. If catalog entries
later become data-backed, the repository query must accept the normalized
filters, stable keyset cursor, projection, and bounded fetch limit. It must not
return all rows to the memory server. The service may request another bounded
micro-page only when filtering leaves room in the current 16 KiB page.

## Telemetry

Record surface, detail mode, final bytes, entries emitted, source entries
pulled, page latency, `has_more`, stale/invalid cursor totals, unknown-name
totals, and non-progress failures. Do not log raw cursors, queries, or name
lists. Alert on responses above the cap, empty pages with `has_more`, or source
pulls beyond the declared bounded algorithm.
