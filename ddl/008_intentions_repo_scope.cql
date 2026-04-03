-- Ferrosa Memory MCP Server — Add repo scoping to intentions
-- Run against Ferrosa CQL interface after 007_intentions.cql
--
-- Intentions are scoped per-repo so shared memory servers return
-- only the intentions relevant to the current project.
--
-- Migration: drop old table, recreate with (tenant_id, repo) partition key.
-- Back up data before running (scripts/backup-memory.sh).

USE agent_memory;

DROP TABLE IF EXISTS intentions;

CREATE TABLE IF NOT EXISTS intentions (
    tenant_id uuid,
    repo text,
    intention_id uuid,
    description text,
    trigger_json text,
    priority text,
    status text,
    created_at timestamp,
    triggered_at timestamp,
    completed_at timestamp,
    PRIMARY KEY ((tenant_id, repo), intention_id)
);
