-- Ferrosa Memory MCP Server — workspace/profile summaries
-- Run against Ferrosa CQL interface after 044_mem_scenes.cql
--
-- A compact, durable per-session profile/workspace-state summary derived from
-- consolidation scenes (mem_scenes). Injected into retrieval so an agent always
-- gets the session's gist (active entities, repo/branch/task context) without
-- re-deriving it from raw logs. One row per (tenant, session); upserted each
-- consolidation cycle.

USE agent_memory;

CREATE TABLE IF NOT EXISTS mem_profiles (
    tenant_id    uuid,
    session_id   uuid,
    summary      text,
    scene_count  int,
    updated_at   timestamp,
    PRIMARY KEY ((tenant_id, session_id))
);
