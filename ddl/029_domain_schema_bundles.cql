CREATE TABLE IF NOT EXISTS agent_memory.domain_schemas (
    schema_id uuid,
    schema_name text,
    version int,
    description text,
    skill_names list<text>,
    routing_guidelines text,
    created_at timestamp,
    updated_at timestamp,
    PRIMARY KEY ((schema_id), version)
) WITH CLUSTERING ORDER BY (version DESC);

CREATE INDEX IF NOT EXISTS idx_domain_schema_name
    ON agent_memory.domain_schemas (schema_name);
