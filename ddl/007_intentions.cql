-- Ferrosa Memory MCP Server — Intentions table for prospective memory
-- Run against Ferrosa CQL interface after 006_entity_state.cql

USE agent_memory;

CREATE TABLE IF NOT EXISTS intentions (
    tenant_id uuid,
    intention_id uuid,
    description text,
    trigger_json text,
    priority text,
    status text,
    created_at timestamp,
    triggered_at timestamp,
    completed_at timestamp,
    PRIMARY KEY ((tenant_id), intention_id)
);
