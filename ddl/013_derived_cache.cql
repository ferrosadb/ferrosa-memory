-- Ephemeral cache for derived Datalog facts
-- Sprint 5 — TTL-bounded, append-oriented

CREATE TABLE IF NOT EXISTS agent_memory.derived_cache_by_query (
    tenant_id uuid,
    cache_key text,
    seq int,
    src_id uuid,
    pred text,
    dst_id uuid,
    confidence double,
    rule_id text,
    computed_at timestamp,
    PRIMARY KEY ((tenant_id, cache_key), seq)
) WITH default_time_to_live = 3600;

CREATE TABLE IF NOT EXISTS agent_memory.derived_cache_by_pred (
    tenant_id uuid,
    pred text,
    bucket text,
    src_id uuid,
    dst_id uuid,
    confidence double,
    rule_id text,
    computed_at timestamp,
    PRIMARY KEY ((tenant_id, pred, bucket), src_id, dst_id)
) WITH default_time_to_live = 3600;
