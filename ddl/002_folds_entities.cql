-- Ferrosa Memory MCP Server — Sprint 2 & 3 Tables
-- Run against Ferrosa CQL interface after 001_keyspace.cql

USE agent_memory;

-- 4.3 Trajectory Folds
CREATE TABLE IF NOT EXISTS trajectory_folds (
    session_id       uuid,
    fold_id          uuid,
    tenant_id        uuid,
    depth            int,
    parent_fold_id   uuid,
    raw_trajectory   text,
    fold_summary     text,
    fold_embedding   vector<float, 768>,
    token_count      int,
    compression_ratio float,
    status           text,
    created_at       timestamp,
    folded_at        timestamp,
    PRIMARY KEY ((session_id, tenant_id), fold_id)
) WITH CLUSTERING ORDER BY (fold_id DESC);

CREATE INDEX IF NOT EXISTS idx_fold_embedding ON trajectory_folds (fold_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};

-- Graph annotation for fold hierarchy
ALTER TABLE trajectory_folds
    WITH extensions = {'graph.type': 'vertex', 'graph.label': 'Fold'};

-- 4.4 Entity Store
CREATE TABLE IF NOT EXISTS entity_store (
    tenant_id        uuid,
    entity_id        uuid,
    session_id       uuid,
    entity_name      text,
    entity_type      text,
    source_fold_id   timeuuid,
    context_snippet  text,
    entity_embedding vector<float, 768>,
    confidence       float,
    created_at       timestamp,
    PRIMARY KEY ((tenant_id, session_id), entity_id)
);

CREATE INDEX IF NOT EXISTS idx_entity_name_phonetic ON entity_store (entity_name)
    USING 'phonetic'
    WITH OPTIONS = {'algorithm': 'double_metaphone'};

CREATE INDEX IF NOT EXISTS idx_entity_embedding ON entity_store (entity_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};

-- 4.5 Temporal Events
CREATE TABLE IF NOT EXISTS temporal_events (
    tenant_id        uuid,
    entity_id        uuid,
    event_time       timestamp,
    event_id         timeuuid,
    fact_text        text,
    supersedes_id    timeuuid,
    valid_until      timestamp,
    source_session   uuid,
    confidence       float,
    PRIMARY KEY ((tenant_id, entity_id), event_time, event_id)
) WITH CLUSTERING ORDER BY (event_time DESC, event_id DESC);

-- 4.6 Feedback Outcomes
CREATE TABLE IF NOT EXISTS feedback_outcomes (
    tenant_id        uuid,
    session_id       uuid,
    query_id         uuid,
    program_type     text,
    query_embedding  vector<float, 768>,
    task_complexity  text,
    succeeded        boolean,
    latency_ms       int,
    token_cost       int,
    guideline_version text,
    created_at       timestamp,
    PRIMARY KEY ((tenant_id), created_at, query_id)
) WITH CLUSTERING ORDER BY (created_at DESC, query_id DESC);
