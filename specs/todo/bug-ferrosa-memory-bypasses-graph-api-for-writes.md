---
type: bug
priority: P2
status: partial
created: 2026-04-20
updated: 2026-06-11
reported-by: 2026-04-20 architecture review during post-incident monitoring
---

# ferrosa-memory bypasses the graph API and writes directly into ferrosa-graph's backing CQL tables

This bug is one concrete slice of the broader graph-boundary and query-passthrough refactor tracked in [feat-endpoint-only-ferrosa-client.md](./feat-endpoint-only-ferrosa-client.md).

## Observed

**2026-06-11 update:** the normal MCP serving path no longer writes graph-owned
edge rows through direct `CqlStorage`. `ReconnectingStorage` routes
`typed_edges`, `folded_into`, `mentioned_in`, `co_occurs_with`, and
`supersedes` mutations through `GraphClient`, while direct `CqlStorage` graph
writer methods return explicit graph-write errors. Live smoke now verifies MCP
edge write, CQL readback, direct graph readback/traversal, MCP chain traversal,
direct graph API mutation, and CQL readback of the graph-created edge.

The remaining scope is cleanup/enforcement, not the main serving path:

- maintenance tool `crates/ferrosa-memory-mcp/src/tools/fix_edge_sessions.rs`
  still intentionally repairs `typed_edges` rows directly;
- role-level CQL enforcement still needs to prevent accidental graph-table
  `MODIFY` from normal serving credentials;
- Ferrosa graph still rejects variable-length `TYPED_EDGE*` traversal, so
  `find_memory_chain` handles multi-hop verification over typed-edge reads.

Original 2026-04-20 observation:

`ferrosa-memory` (the MCP-facing memory/knowledge-graph service that sits
alongside the ferrosa cluster) writes graph edges to the cluster via CQL
`INSERT` statements that name ferrosa-graph's internal storage tables.
Confirmed in `../ferrosa-memory/crates/ferrosa-memory-core/src/cql_storage.rs`:

- `INSERT INTO {ks}.typed_edges …`   (line ~480 region)
- `INSERT INTO {ks}.folded_into …`
- `INSERT INTO {ks}.mentioned_in …`
- `INSERT INTO {ks}.co_occurs_with …`
- `INSERT INTO {ks}.supersedes …`
- `INSERT INTO {ks}.derived_edges_by_pred …` / `by_src`

Reads use a different path — `ferrosa-memory-core/src/graph.rs` hits the
public Cypher HTTP endpoint on port 7474 via `reqwest`. **Writes skip
Cypher entirely.**

`ferrosa-memory` does **not** link against any `ferrosa-*` crate, so this
isn't a symbol-level coupling — it's a **schema-level coupling**. The
wire protocol is public (CQL v4 via `cdrs-tokio`), but the tables named
in those INSERTs are owned by the graph engine and are supposed to be
manipulated through Cypher/Bolt, not directly.

## Why this is a bug

1. **Invariant bypass.** `ferrosa_graph::adjacency::reconcile` and the
   rest of the graph engine maintain invariants on edge rows (property
   typing, uniqueness, reverse-index consistency). Raw `INSERT` skips
   every one of them. Today it happens to work because the CQL schema
   is permissive; any future tightening — unique constraints, materialized
   reverse indexes, generation columns — silently breaks ferrosa-memory.
2. **Schema coupling.** ferrosa-graph owns the row encoding of
   `typed_edges` (column layout, partition key shape, clustering order).
   If the graph engine reshapes that (e.g., for a compaction-strategy
   change or a new query pattern), every ferrosa-memory INSERT becomes
   either wrong or a silent data-loss path.
3. **Hidden consumers.** Rules like "edges are always traversed via
   Cypher" stop being true. A code audit of ferrosa-graph that assumes
   Cypher is the only writer will miss ferrosa-memory's back-door writes.
4. **Telemetry and audit.** Cypher-level hooks (logging, rate limits,
   access control) don't see these writes.

This isn't hypothetical — the 2026-04-19 `tool_usage_log` corruption
incident exposed how easily direct CQL writes can produce rows that look
valid at the protocol level but violate schema assumptions further in.

## Desired

ferrosa-memory stops naming graph-owned tables in any CQL statement.
Every edge mutation travels through the Cypher endpoint
(`POST /graph/cypher` with a `MERGE`/`CREATE` statement, or the Bolt
channel if that's preferred for performance). The graph engine remains
the single writer of its own storage.

## Related

- `todo-enable-cql-role-auth-for-graph-table-isolation.md` — enforcement
  via CQL roles so even buggy clients can't bypass once the code fix
  lands.
- `todo-extend-ferrosa-memory-graph-client-with-cypher-writes.md` —
  the implementation slice for this boundary fix inside ferrosa-memory.
- 2026-04-19 `tool_usage_log` corruption incident (in
  `bug-read-path-memory-growth-bloats-coordinator.md`) — same class of
  problem: public protocol, private schema.

## Acceptance criteria

- [ ] No `INSERT`/`UPDATE`/`DELETE`/`TRUNCATE` in
      `ferrosa-memory-core` or any sibling crate names a table that
      ferrosa-graph considers internal (at minimum: `typed_edges`,
      `folded_into`, `mentioned_in`, `co_occurs_with`, `supersedes`,
      `derived_edges_by_pred`, `derived_edges_by_src`).
- [ ] All graph mutations arrive at the cluster via Cypher or Bolt;
      reads may continue using Cypher.
- [ ] Existing ferrosa-memory tests that exercise edge writes pass
      end-to-end after the switch.
