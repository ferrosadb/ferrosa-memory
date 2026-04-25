-- Derived cache: TTL tracking and indexes (Sprint 6)
--
-- derived_cache_ttl_track: per-fact TTL metadata for maintenance scheduling.
-- Each row mirrors a derived_cache_by_query entry with its TTL rule and next
-- maintenance window. Updated by the TTL maintenance thread.

CREATE TABLE IF NOT EXISTS agent_memory.derived_cache_ttl_track (
    tenant_id uuid,
    cache_key text,
    seq int,
    src_id uuid,
    pred text,
    dst_id uuid,
    ttl_seconds int,
    rule_id text,
    computed_at timestamp,
    next_maintenance timestamp,
    PRIMARY KEY ((tenant_id, cache_key), seq)
) WITH default_time_to_live = 86400;
