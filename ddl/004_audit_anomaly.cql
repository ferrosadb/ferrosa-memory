-- Audit log and anomaly detection tables for ferrosa-memory-mcp

USE agent_memory;

-- Append-only audit log (STRIDE R1)
-- Records every write operation. Cannot be deleted via MCP tools.
CREATE TABLE IF NOT EXISTS audit_log (
    tenant_id        uuid,
    created_at       timestamp,
    audit_id         uuid,
    operation        text,       -- 'memo_put', 'entity_put', 'fold_put', 'temporal_put', 'feedback_put', 'edge_*'
    target_table     text,       -- 'memo_cache', 'entity_store', etc.
    target_id        text,       -- primary key of the written row (serialized)
    session_id       uuid,
    PRIMARY KEY ((tenant_id), created_at, audit_id)
) WITH CLUSTERING ORDER BY (created_at DESC, audit_id DESC);

-- Entity retrieval frequency tracking (FMEA F19 anomaly detection)
-- Tracks how many times each entity is retrieved per session.
-- Entities retrieved at >3σ from session baseline are flagged.
CREATE TABLE IF NOT EXISTS entity_retrieval_counts (
    tenant_id        uuid,
    session_id       uuid,
    entity_id        uuid,
    retrieval_count  int,
    last_retrieved   timestamp,
    PRIMARY KEY ((tenant_id, session_id), entity_id)
);
