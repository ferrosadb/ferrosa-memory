CREATE TABLE IF NOT EXISTS agent_memory.contradictions (
    tenant_id uuid,
    entity_id uuid,
    old_fact_hash text,
    new_fact_hash text,
    old_fact_text text,
    new_fact_text text,
    detected_at timestamp,
    resolved_at timestamp,
    resolution text,
    resolver text,
    PRIMARY KEY ((tenant_id, entity_id), detected_at, old_fact_hash, new_fact_hash)
) WITH CLUSTERING ORDER BY (detected_at DESC);
