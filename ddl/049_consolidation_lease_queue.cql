-- Ferrosa Memory MCP Server — Cross-replica consolidation coordination
-- Run against Ferrosa CQL interface after 048_scene_embedding.cql
--
-- Adds a durable work queue and LWT-based lease so that exactly one
-- ferrosa-memory replica consolidates a given (tenant, session) at a time.
--
-- NOTE: Both tables are pure coordination/audit state. They can be rebuilt
-- from entity and edge data, so operators should pin them to local NVMe
-- storage where available and avoid S3-only/cold tiers. Use the Ferrosa
-- operator's table-level storage policy (e.g. `ferrosa.storage = 'local_disk'`
-- extension or equivalent) rather than the default S3-backed profile.

CREATE TABLE IF NOT EXISTS agent_memory.consolidation_requests (
    tenant_id uuid,
    session_id uuid,
    state text,
    requested_at timestamp,
    lease_owner text,
    lease_expires_at timestamp,
    attempt_count int,
    last_error text,
    completed_at timestamp,
    PRIMARY KEY ((tenant_id, session_id))
);

-- Allow replicas to find pending work without scanning all sessions.
CREATE INDEX IF NOT EXISTS agent_memory.consolidation_requests_state_idx
    ON agent_memory.consolidation_requests (state);

-- Append-only audit log of consolidation runs. Clustering by run_id (timeuuid)
-- gives newest-first ordering for operator queries.
CREATE TABLE IF NOT EXISTS agent_memory.consolidation_runs (
    tenant_id uuid,
    session_id uuid,
    run_id timeuuid,
    lease_owner text,
    started_at timestamp,
    finished_at timestamp,
    status text,
    entities_processed int,
    connections_created int,
    error text,
    PRIMARY KEY ((tenant_id, session_id), run_id)
) WITH CLUSTERING ORDER BY (run_id DESC);
