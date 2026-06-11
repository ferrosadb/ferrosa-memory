# Agent Rules

## Schema Migrations

All schema changes must include a versioned migration. Never change a schema without bumping the migration version and registering the migration in order.

Migrations must be automatic, ordered, and data-preserving:

- A deployment at version `N` must be able to reach version `M` by applying every migration `N+1`, `N+2`, ... `M` in sequence.
- A migration must preserve, transform, or explicitly supersede legacy rows. It must not damage, silently drop, or orphan old data.
- If a primary-key or incompatible type change requires table recreation, use a staging/copy/swap migration with row-count verification and a recoverable failure mode.
- Startup migration logic must fail loud on schema drift or copy mismatch rather than continuing with a partially migrated schema.

## Graph Boundary

Ferrosa Memory is a client of Ferrosa graph APIs, not an owner of graph backing-table encodings.

- Serving-path graph mutations must go through the graph client / public graph API path (`GraphClient` behind `ReconnectingStorage`), not raw CQL writes to graph-owned tables.
- Direct CQL remains acceptable for app-owned Ferrosa Memory tables such as entities, temporal events, context segments, feedback, and configuration registries.
- If a graph API cannot express a needed mutation or traversal, file/fix the Ferrosa graph issue instead of adding a local backing-table workaround.
- Maintenance tools that intentionally repair graph backing rows must be documented as maintenance-only and must not become runtime serving paths.
