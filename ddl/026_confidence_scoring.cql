CREATE TABLE IF NOT EXISTS agent_memory.confidence_scores (
    entity_id uuid,
    fact_hash text,
    confidence double,
    source_count int,
    last_confirmed_at timestamp,
    contradiction_count int,
    PRIMARY KEY ((entity_id), fact_hash)
);
CREATE INDEX IF NOT EXISTS idx_confidence_high
    ON agent_memory.confidence_scores (confidence);
