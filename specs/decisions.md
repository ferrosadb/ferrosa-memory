---
title: Ferrosa Memory Blueprint Decisions
executive_summary:
  purpose: >-
    Records stakeholder decisions that constrain the Ferrosa Memory blueprint
    before implementation planning begins.
  critical_items:
    - >-
      The all_tools public name remains stable, but its response becomes compact
      and paginated by default.
    - >-
      Callers receive an explicit continuation cursor and can request subsequent
      pages without replaying the full catalog.
    - >-
      Every page is capped at 16 KiB and ends at the nearest complete tool-entry
      boundary below that limit.
    - >-
      Compact discovery, named schema lookup, and actionable pagination hints
      remain available through the all_tools contract.
    - >-
      Cursors embed a catalog version and stale cursors return an actionable
      restart hint instead of continuing across changed catalog ordering.
    - >-
      Every catalog surface shares the hard limit, and pagination is enforced
      at its source so the memory server never fetches or builds a full result
      merely to slice it afterward.
    - >-
      Deterministic tool search is part of all_tools rather than a separate
      public tool, and filtering occurs before page construction.
status: complete
last_updated: 2026-08-12
---

# Ferrosa Memory Blueprint Decisions

This document records confirmed product and API decisions for the bounded MCP
tool-catalog response project. Open decisions remain in the decision tree until
they are resolved or explicitly deferred.

## Decision FM-AT-001: Preserve the tool name and paginate the response

**Status:** Confirmed

**Decision:** Keep the public `all_tools` tool name. Change its default result
from one complete catalog response to a compact page. Each successful page
returns a continuation cursor that callers can use to request the next page.

**Rationale:** Existing callers retain a stable discovery entry point while the
server prevents catalog growth from exhausting a client's tool-result context
budget. Pagination also avoids repeatedly paying the token cost of definitions
that the caller does not need.

**Constraints:**

- The default response must not serialize all tool descriptions and schemas.
- Page traversal must be deterministic for a stable catalog revision.
- The response must say whether another page exists and provide the cursor
  required to retrieve it.
- A caller must not need to resend or interpret the entire preceding page to
  advance.

**Evidence class:** Stakeholder-confirmed target behavior.

## Decision FM-AT-002: Enforce semantic 16 KiB pages

**Status:** Confirmed

**Decision:** Cap each serialized `all_tools` page at 16 KiB. Build a page by
adding complete tool entries in deterministic order until the next entry would
cross the byte limit. End the page at that semantic boundary and return a
continuation cursor for the next entry.

**Rationale:** A byte ceiling directly controls tool-result context cost, while
entry-boundary splitting keeps every returned item valid and independently
usable. A fixed item count alone cannot control cost because tool schemas vary
substantially in size.

**Constraints:**

- The serialized success envelope, metadata, and entries count toward the
  16 KiB limit.
- No description, schema, property, or individual tool definition may be split
  across pages.
- Page assembly should approach the limit without exceeding it.
- Ordering and cursor advancement must ensure every matching entry appears
  exactly once during an unchanged catalog traversal.
- A single entry that cannot fit in an otherwise empty page must fail with a
  typed size error rather than violate the limit or loop on the same cursor.

**Evidence class:** Stakeholder-confirmed target behavior.

## Decision FM-AT-003: Provide compact discovery, named detail, and hints

**Status:** Confirmed

**Decision:** Keep discovery and schema retrieval within `all_tools`. Compact
mode returns bounded discovery metadata. Schema mode returns complete tool
definitions and accepts explicit tool names so a caller can jump directly to
the required definitions. Every paginated response includes actionable hints
that explain how to request the next page.

**Compact entry contract:**

- Public tool name.
- Stable category or family.
- Short usage summary.
- Schema digest for cache and change detection.

**Schema request contract:**

- `detail: "schema"` requests complete definitions.
- `names: [...]` selects one or more tools without traversing unrelated pages.
- Requests without explicit names use deterministic catalog order and the same
  semantic pagination contract.

**Pagination hint contract:**

- Return `has_more` and `next_cursor` as machine-readable fields.
- Return a compact `hint` object on every paginated response, including the
  final page.
- When `has_more` is true, the hint includes the exact arguments required for
  the next `all_tools` call while preserving the current filters and detail
  mode.
- When `has_more` is false, the hint states that traversal is complete and
  explains how to request named schema detail without restarting discovery.
- Hints count toward the 16 KiB serialized page budget.

**Rationale:** One stable entry point avoids expanding the default MCP surface.
Named lookup prevents unnecessary page traversal, while self-describing
navigation keeps clients from guessing how opaque cursors and filters compose.

**Evidence class:** Stakeholder-confirmed target behavior.

## Decision FM-AT-004: Version cursors and fail loudly when stale

**Status:** Confirmed

**Decision:** Embed the catalog version in every opaque continuation cursor.
When a cursor version differs from the current catalog version, reject the
request with a typed stale-cursor result and an actionable restart hint.

**Constraints:**

- The server owns the catalog version; callers must not select or override it.
- The cursor remains opaque to callers even though it carries versioned state.
- A stale cursor must not silently restart, skip forward, or continue against
  the new ordering.
- The stale result includes the current catalog version and exact replacement
  arguments for restarting the same detail mode and filters without the old
  cursor.
- The stale response remains within the 16 KiB serialized-response budget.
- Cursor decoding and version validation occur before page traversal.

**Rationale:** Catalog changes can reorder, add, or remove definitions. Explicit
version rejection prevents duplicate or missing entries and gives clients a
deterministic recovery path after deployment.

**Evidence class:** Stakeholder-confirmed target behavior.

## Decision FM-AT-005: Share source-level pagination across catalog surfaces

**Status:** Confirmed

**Decision:** Apply one hard-limited pagination contract to `all_tools`, MCP
`tools/list`, and the operator/workbench catalog endpoint. Pagination must be
implemented at the catalog source boundary. The memory server must not fetch,
construct, or serialize a complete catalog result and then slice it into pages.

**Current-state clarification:** The inspected tool catalog is statically
assembled in-process rather than read from a database. The current
`tool_definitions` path materializes a complete `Vec<ToolDef>`. The target
design replaces that discovery behavior with incremental source traversal. If
a database-backed catalog source is introduced, it must push cursor, stable
ordering, projection, and a bounded row limit into the database operation.

**Constraints:**

- All catalog-returning surfaces use the same cursor codec, catalog version,
  filtering rules, projection modes, semantic page builder, and hint builder.
- The 16 KiB limit includes the complete serialized response envelope.
- The source returns entries incrementally in stable order; it does not return
  an unbounded collection for downstream pagination.
- Page assembly keeps at most the current page plus one look-ahead entry needed
  to determine `has_more`.
- A database implementation must expose a paged repository operation and must
  not use an unbounded query followed by in-process slicing.
- The operator/workbench client follows cursors explicitly and does not ask its
  server route to reassemble every page into one response.
- Tool execution remains independent of catalog pagination.

**Rationale:** A response-size cap protects client context, but it does not
protect server memory if the implementation still materializes the full source.
Enforcing the boundary at the source controls both token cost and server memory
as the catalog grows or becomes data-backed.

**Evidence class:** Stakeholder-confirmed target behavior plus repo-proven
current-state clarification.

## Decision FM-AT-006: Make `all_tools` the searchable discovery surface

**Status:** Confirmed

**Decision:** Add deterministic search and exact filters to `all_tools` instead
of introducing a separate `tool_search` tool. Search narrows the catalog at the
source before semantic page construction.

**Request contract:**

- `query` performs deterministic lexical matching across public name, category,
  tags, and compact summary.
- `categories` applies exact category filtering.
- `names` performs exact direct lookup and remains the preferred schema-detail
  path when the caller already knows the tools it needs.
- `detail` selects compact discovery or complete schema entries.
- `cursor` continues the same normalized query, filters, ordering, and detail
  mode.

**Constraints:**

- Search must not add another always-visible MCP tool.
- The initial implementation must not require embeddings, an LLM, or a database
  scan.
- Matching and ranking are deterministic. Exact name, name prefix, category or
  tag, and summary-token matches use explicit stable precedence with public name
  as the final tie-breaker.
- The cursor binds to a normalized request fingerprint. Reusing it with changed
  query, filters, or detail mode returns a typed cursor-mismatch result and a
  restart hint.
- Search filtering occurs at the catalog source. A future database-backed
  source must push supported predicates, keyset position, and limit into its
  query instead of loading candidates into the memory server.
- Search responses use the same catalog version, 16 KiB envelope limit,
  complete-entry boundary, and navigation-hint contract.

**Rationale:** Callers need to find relevant tools without traversing a growing
catalog. Extending the existing discovery tool avoids fragmenting the compact
default surface and keeps direct lookup, browsing, and schema retrieval under
one predictable contract.

**Evidence class:** Stakeholder-confirmed target behavior.

## Open decisions

No stakeholder decision blocks technical analysis. Test thresholds and rollout
sequencing can be derived from the confirmed hard limits and repository gates.
