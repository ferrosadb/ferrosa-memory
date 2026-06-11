---
type: todo
priority: P2
status: partial
created: 2026-04-20
updated: 2026-06-11
---

# Extend ferrosa-memory's graph client to route edge writes through Cypher instead of CQL

This todo is the implementation half of [bug-ferrosa-memory-bypasses-graph-api-for-writes.md](./bug-ferrosa-memory-bypasses-graph-api-for-writes.md) and should be treated as part of the larger [feat-endpoint-only-ferrosa-client.md](./feat-endpoint-only-ferrosa-client.md) workstream.

## Why

**2026-06-11 update:** the serving-path implementation slice is now in place.
`GraphClient` exposes graph edge write/delete helpers, `ReconnectingStorage`
routes graph-owned edge writes through those helpers, and direct `CqlStorage`
graph writer methods fail loud. `typed_edges` are insertable through MCP
`edge`/`create_edge` and through the graph API, then queryable through graph
lookups, CQL typed-edge reads, `explore_connections`, and `find_memory_chain`.

Remaining work before this todo can move out of `todo/`:

- remove or explicitly isolate maintenance-only direct `typed_edges` repair in
  `crates/ferrosa-memory-mcp/src/tools/fix_edge_sessions.rs`;
- add least-privilege role enforcement so normal serving credentials cannot
  `MODIFY` graph backing tables;
- keep live graph smoke coverage for MCP, graph API, and CQL readback.

Original 2026-04-20 state: ferrosa-memory wrote edges to `typed_edges` and related graph tables
with raw CQL `INSERT` statements today — see
`bug-ferrosa-memory-bypasses-graph-api-for-writes.md`. The right fix is
to reuse the Cypher HTTP channel it already uses for reads.

## Proposed

1. In `../ferrosa-memory/crates/ferrosa-memory-core/src/graph.rs`, add
   write methods alongside the existing reads:
   - `merge_edge(src, edge_type, dst, properties)` → Cypher:
     `MATCH (s {id: $src}), (d {id: $dst}) MERGE (s)-[r:$edge_type]->(d)
      SET r += $props RETURN r`
   - `delete_edge(src, edge_type, dst)` → Cypher:
     `MATCH (s {id: $src})-[r:$edge_type]->(d {id: $dst}) DELETE r`
   - Symmetric helpers for `folded_into`, `mentioned_in`,
     `co_occurs_with`, `supersedes`, plus the derived-edges variants.
2. In `ferrosa-memory-core/src/cql_storage.rs`, remove every
   `INSERT/UPDATE/DELETE` that names a graph-owned table; delegate to
   the `GraphClient` methods from step 1.
3. The `Storage` trait in `cql_storage.rs` should hide whether a given
   write goes to CQL or Cypher — callers shouldn't care. Keep the
   split behind the trait; wire ferrosa-memory-mcp to the same trait
   object it already uses.
4. Keep read paths on Cypher (already the case). No change there.
5. Performance check: batch edge writes via a single Cypher `UNWIND`
   where possible. If the per-edge round-trip overhead becomes a
   problem, switch to Bolt (which `tokio-tungstenite` can already
   carry) rather than reintroducing raw CQL.

## Acceptance criteria

- [ ] No `INSERT INTO {ks}.(typed_edges|folded_into|mentioned_in|
      co_occurs_with|supersedes|derived_edges_by_*)` in any
      ferrosa-memory source file.
- [ ] `graph.rs` exposes write methods; tests exercise them
      end-to-end against a running ferrosa cluster.
- [ ] Existing ferrosa-memory black-box tests
      (`skill_e2e_live.rs`, `launch_gates_g3_g4.rs`, etc.) pass.
- [ ] Under the CQL-auth change
      (`todo-enable-cql-role-auth-for-graph-table-isolation.md`),
      ferrosa-memory still works when its role lacks MODIFY on graph
      tables.

## Related

- `bug-ferrosa-memory-bypasses-graph-api-for-writes.md`
- `feat-endpoint-only-ferrosa-client.md`
- `todo-enable-cql-role-auth-for-graph-table-isolation.md`
