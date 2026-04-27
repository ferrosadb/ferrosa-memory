-- Entity warmth field: persistent spreading activation with Ebbinghaus decay
-- Sprint 5 — RMH warmth module

CREATE TABLE IF NOT EXISTS agent_memory.entity_warmth (
    tenant_id uuid,
    entity_id uuid,
    session_id uuid,
    warmth double,
    pagerank double,
    reputation double,
    last_accessed_at timestamp,
    access_count int,
    decay_zone text,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, entity_id))
);

CREATE INDEX IF NOT EXISTS idx_warmth_session
    ON agent_memory.entity_warmth (session_id);
