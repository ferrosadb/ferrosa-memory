-- Backfill the typed_edges table on installs that adopted the pre-versioning
-- baseline (schema_version row 19) BEFORE ddl/017_typed_edges.cql was added to
-- the bootstrap set. Those keyspaces never created typed_edges, so create_edge
-- fails until manual DDL/schema_version repair.
--
-- This is a forward migration (version 42) rather than an edit to the
-- bootstrap-only 017 file, so the append-only migration registry stays intact
-- and the runner applies + records it on every install that lacks it. All
-- statements are idempotent (IF NOT EXISTS / ALTER WITH extensions), so on an
-- install that already has typed_edges this migration is a no-op.
--
-- Mirrors ddl/017_typed_edges.cql exactly. Schema migration only — no
-- serving-path graph-table writes (graph writes go through GraphClient).

CREATE TABLE IF NOT EXISTS agent_memory.typed_edges (
    tenant_id uuid,
    session_id uuid,
    src_id uuid,
    edge_type text,
    dst_id uuid,
    weight double,
    metadata text,
    created_at timestamp,
    PRIMARY KEY ((tenant_id, session_id), src_id, edge_type, dst_id)
);

-- Reverse lookup: find edges pointing TO an entity
CREATE INDEX IF NOT EXISTS idx_typed_edges_dst
    ON agent_memory.typed_edges (dst_id);

-- Graph engine annotations: make typed_edges visible to Cypher/viz
ALTER TABLE agent_memory.typed_edges WITH extensions = {
    'graph.type': 'edge',
    'graph.label': 'TYPED_EDGE',
    'graph.source': 'src_id',
    'graph.target': 'dst_id',
    'graph.source_label': 'Entity',
    'graph.target_label': 'Entity'
};
