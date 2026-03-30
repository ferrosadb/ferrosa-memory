-- Typed edges: arbitrary labeled relationships between entities
-- Enables Datalog inference over semantic edge types (depends_on, contains, etc.)

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
