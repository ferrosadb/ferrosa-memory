-- Secondary indexes used by local Datalog-backed queries.
--
-- Without these, Ferrosa rejects queries that filter entity facts by type/state
-- or typed edges by semantic edge_type unless callers fall back to filtering.

CREATE INDEX IF NOT EXISTS idx_entity_type
    ON agent_memory.entity_store (entity_type);

CREATE INDEX IF NOT EXISTS idx_entity_state
    ON agent_memory.entity_store (state);

CREATE INDEX IF NOT EXISTS idx_typed_edges_edge_type
    ON agent_memory.typed_edges (edge_type);
