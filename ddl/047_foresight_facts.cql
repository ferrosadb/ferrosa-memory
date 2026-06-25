-- Ferrosa Memory MCP Server — time-bounded foresight facts
-- Run against Ferrosa CQL interface after 046_retrieval_traces.cql
--
-- Durable planned-future facts and temporary constraints with a validity
-- window (valid_from/valid_until). Retrieval surfaces only facts valid at the
-- query's as-of time (default now), so expired facts and not-yet-active plans
-- don't pollute context. Distinct from supersession (entity valid_until):
-- these are prospective, agent/user-declared facts.

USE agent_memory;

CREATE TABLE IF NOT EXISTS foresight_facts (
    tenant_id   uuid,
    session_id  uuid,
    fact_id     uuid,
    content     text,
    valid_from  timestamp,
    valid_until timestamp,
    created_at  timestamp,
    PRIMARY KEY ((tenant_id, session_id), fact_id)
);
