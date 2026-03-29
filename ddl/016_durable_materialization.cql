-- Durable materialization tables for promoted predicates
-- B10 — Workload-driven promotion pipeline

CREATE TABLE IF NOT EXISTS agent_memory.derived_edges_by_src (
    tenant_id uuid,
    src_id text,
    shard smallint,
    pred text,
    dst_id text,
    rule_id text,
    support_count int,
    confidence double,
    batch_id text,
    materialized_at timestamp,
    PRIMARY KEY ((tenant_id, src_id, shard), pred, dst_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.derived_edges_by_pred (
    tenant_id uuid,
    pred text,
    shard smallint,
    src_id text,
    dst_id text,
    rule_id text,
    support_count int,
    confidence double,
    batch_id text,
    materialized_at timestamp,
    PRIMARY KEY ((tenant_id, pred, shard), src_id, dst_id)
);

-- Promotion registry: tracks which predicates are durably materialized
CREATE TABLE IF NOT EXISTS agent_memory.promoted_predicates (
    tenant_id uuid,
    pred text,
    promotion_score double,
    estimated_rows int,
    materialized_at timestamp,
    batch_id text,
    status text,
    PRIMARY KEY ((tenant_id), pred)
);
