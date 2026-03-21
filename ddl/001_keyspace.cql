-- Ferrosa Memory MCP Server — Keyspace and Sprint 1 Tables
-- Run against Ferrosa CQL interface

CREATE KEYSPACE IF NOT EXISTS agent_memory
WITH replication = {
    'class': 'NetworkTopologyStrategy',
    'datacenter1': 3
};

USE agent_memory;

-- 4.1 Memoization Cache
CREATE TABLE IF NOT EXISTS memo_cache (
    content_hash     text,
    model_version    text,
    tenant_id        uuid,
    result           text,
    result_embedding vector<float, 768>,
    created_at       timestamp,
    last_hit_at      timestamp,
    hit_count        bigint,
    expires_at       timestamp,
    PRIMARY KEY ((content_hash, model_version), tenant_id)
);

CREATE INDEX IF NOT EXISTS idx_memo_embedding ON memo_cache (result_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};

-- 4.2 Plan State
CREATE TABLE IF NOT EXISTS plan_state (
    session_id       uuid,
    depth            int,
    subtask_id       text,
    tenant_id        uuid,
    parent_subtask   text,
    goal_text        text,
    status           text,
    outcome_summary  text,
    created_at       timestamp,
    completed_at     timestamp,
    PRIMARY KEY ((session_id, tenant_id), depth, subtask_id)
) WITH CLUSTERING ORDER BY (depth ASC, subtask_id ASC);

CREATE INDEX IF NOT EXISTS idx_plan_depth ON plan_state (depth)
    USING 'btree';
