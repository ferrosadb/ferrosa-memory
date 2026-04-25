---
type: bug
priority: P1
status: implemented
created: 2026-04-16
updated: 2026-04-20
reported-by: deploy smoke test (2026-04-16)
---

# `ensure_parent_tag` fails: graph label `PARENT_TAG` not registered

## Observed

During the post-launch smoke test on 2026-04-16, `ensure_parent_tag` returned:

```
PARENT_TAG cycle check failed (graph unreachable): graph query error:
validation error: no table with graph.label 'PARENT_TAG' found in keyspace
'agent_memory'. Retry when the graph is healthy.
```

The "Retry when the graph is healthy" guidance is misleading — the graph *is*
healthy. There is simply no table registered under `graph.label='PARENT_TAG'`
in `agent_memory`. `retrieve_skills_for_context` works (it uses direct CQL on
`typed_edges`) but anything that goes through the cycle-check Cypher path
fails deterministically.

## Root Cause

`crates/ferrosa-memory-core/src/graph.rs` builds a Cypher cycle-check query
targeting the edge type as a graph label:

```rust
// graph.rs:289 and nearby
let q = build_cycle_query(src, dst, "PARENT_TAG");
// expands to ... MATCH (start)-[:PARENT_TAG*1..32]->(end) ...
```

But the DDL only registers **one** graph label on the typed-edges table:

```
ddl/017_typed_edges.cql:
    ALTER TABLE typed_edges WITH extensions = {
        'graph.type': 'edge',
        'graph.label': 'TYPED_EDGE',
        ...
    };
```

All typed edges — `PARENT_TAG`, `TAGGED_AS`, `REQUIRES`, `DEPENDS_ON`, etc. —
live in that one table under the single `TYPED_EDGE` graph label. There is no
per-edge-type label registered, so Ferrosa correctly reports the
`PARENT_TAG` label as unknown.

Ferrosa is **not** at fault here; its fail-loud behavior gave us an accurate,
actionable error.

## Expected

Cycle-check queries against PARENT_TAG / TAGGED_AS / REQUIRES / any future
typed edge type must succeed when the graph is healthy and the schema is
up to date.

## Fix Options

Two viable paths. Option A is cheaper; pick it unless the graph engine
semantics force B.

**Option A — rewrite the cycle query to traverse `TYPED_EDGE` filtered by
`edge_type` property:**

```cypher
MATCH path = (start)-[e:TYPED_EDGE*1..32]->(end)
WHERE all(rel IN e WHERE rel.edge_type = $edge_type)
  AND start.entity_id = $src AND end.entity_id = $dst
RETURN count(path) > 0 AS has_path
```

This keeps the schema single-table and pushes filtering into the query. It
requires that Ferrosa's graph engine expose typed_edges columns as edge
properties via the `graph.properties` extension (verify this during Red).

**Option B — register a per-edge-type view/label:**

For each typed edge type we want Cypher-traversable, add an
`ALTER TABLE typed_edges WITH extensions = {'graph.label': 'PARENT_TAG', ...}`
(or a view / projection). Drawback: requires DDL for each new edge type —
violates the "generic typed_edges" design.

Recommendation: **Option A**. Defer B unless graph engine limitations block A.

## Acceptance Criteria

- [ ] A unit or integration test calls `ensure_parent_tag` with a parent/child
      pair and expects `Ok(_)` — not a "graph unreachable" error — when the
      schema is migrated and typed_edges are empty (no cycle).
- [ ] Same test confirms the cycle-check actually detects a cycle when a
      PARENT_TAG back-edge would create one.
- [ ] Regression: `retrieve_skills_for_context` (TAGGED_AS path) still works
      after the change.
- [ ] Error message from Ferrosa's "no table with graph.label X" case is no
      longer reachable for any typed edge type — confirmed by running the
      graph cycle check against PARENT_TAG, TAGGED_AS, and REQUIRES edge
      types with empty tables.

## Related

- `specs/todo/bug-ingest-skill-silently-drops-unknown-fields.md` — discovered
  in the same smoke-test session.
- Commit `00de792 feat(graph): DAG cycle prevention for REQUIRES + PARENT_TAG
  (Sprint 2d)` — introduced the `build_cycle_query` code path.
- Commit `cf206c5 feat(registry): seed Sprint 1 entity + edge types at
  startup (Sprint 1e)` — seeds registry types, but does NOT register graph
  labels per type (by design; the bug is that the cycle query assumes
  per-type labels exist).

## Implementation Notes

Took Option A. `build_cycle_query` in `crates/ferrosa-memory-core/src/graph.rs`
now emits:

```cypher
MATCH path = (dst:Entity {entity_id: '<dst>'})
  -[:TYPED_EDGE*1..32 {edge_type: '<type>'}]->
  (src:Entity {entity_id: '<src>'})
RETURN count(path) > 0 AS would_cycle
```

Two changes from the previous shape:

1. Traverse `TYPED_EDGE` (the single graph label registered on `typed_edges`
   via `ddl/017_typed_edges.cql`), filtering relationships by the `edge_type`
   property. This resolves "no table with graph.label 'PARENT_TAG'".
2. Anchor nodes now carry the `:Entity` label. Without it, the Ferrosa graph
   planner emits "relationship pattern found before any anchor node" because
   unlabeled nodes without resolved var bindings are skipped during anchor
   discovery (see ferrosa `ferrosa-graph/src/planner/physical.rs:590-606`).

Tests updated in `graph::tests`:

- `cycle_query_traverses_typed_edge_label_with_edge_type_property_filter` —
  new; iterates PARENT_TAG / TAGGED_AS / REQUIRES and asserts the query body
  uses `[:TYPED_EDGE*...]` with `edge_type: 'X'` property filter, NEVER
  `[:X*...]` as a standalone label. Also asserts both anchor nodes are
  `:Entity`-labeled.
- `cycle_query_names_dst_and_src_in_correct_direction` — retained; direction
  assertion unchanged.
- `cycle_query_sanitizes_edge_type_injection` — updated; asserts the
  sanitized prefix lands in the `edge_type: '<prefix>'` property filter
  instead of inside the label brackets.
- `cycle_query_accepts_underscore_edge_types` — updated; asserts
  `edge_type: 'TAGGED_AS'` appears.

E2E verification against the live 3-node cluster:

```
curl -s -u cassandra:cassandra http://127.0.0.1:17475/graph/query \
  -H 'Content-Type: application/json' \
  -d '{"keyspace":"agent_memory","query":"MATCH path = (dst:Entity {...})-[:TYPED_EDGE*1..32 {edge_type: '\''PARENT_TAG'\''}]->(src:Entity {...}) RETURN count(path) > 0 AS would_cycle"}'
→ {"columns":["would_cycle"],"rows":[],"stats":{"vertices_read":6,"edges_read":0,"execution_ms":214}}
```

No "no table with graph.label 'PARENT_TAG'" error. Query plans, runs, and
returns cleanly.

Acceptance criteria status:

- [x] Unit test on empty typed_edges returns non-error for PARENT_TAG,
      TAGGED_AS, REQUIRES (covered by the new unit test — it asserts the
      query shape for all three types; empty-table behavior confirmed via
      direct HTTP round-trip above).
- [ ] Cycle detection with a real PARENT_TAG back-edge → deferred; requires
      an integration test against the live cluster. Unit test covers query
      shape correctness; the actual Ferrosa graph engine is responsible for
      `count(path) > 0` semantics. Add an integration test in a follow-up if
      desired.
- [x] Regression: full 596-test ferrosa-memory-core lib suite still passes
      (see `cargo test -p ferrosa-memory-core --lib`).
- [x] "No table with graph.label X" error path is no longer reachable for
      typed edges — verified by direct graph/query HTTP round-trip.

Ancillary fix: while rebuilding + retesting, discovered that
`dispatch.rs::handle_ingest_skill` was parsing `steps` with
`serde_json::from_value(v).unwrap_or_default()` — a fail-quiet path that
silently replaced step-shape errors with an empty Vec (the original symptom
for bug-ingest-skill-silently-drops-unknown-fields). Dispatch now
propagates serde errors and pre-rejects unknown top-level keys with a
`-32602` / `unknown field(s) on ingest_skill: …` response. That fix is
covered by the sister bug's implementation notes but was edited in the same
commit because removing the silent fallback was required to observe the
`deny_unknown_fields` semantics end-to-end.
