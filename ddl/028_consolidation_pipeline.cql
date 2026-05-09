CREATE TABLE IF NOT EXISTS agent_memory.consolidation_queue (
    tenant_id uuid,
    stage text,
    artifact_id uuid,
    artifact_kind text,
    source_session_id uuid,
    promotion_score double,
    promoted_at timestamp,
    PRIMARY KEY ((tenant_id, stage), promotion_score, artifact_id)
) WITH CLUSTERING ORDER BY (promotion_score DESC);

CREATE TABLE IF NOT EXISTS agent_memory.consolidation_history (
    tenant_id uuid,
    artifact_id uuid,
    from_stage text,
    to_stage text,
    promoted_at timestamp,
    promotion_reason text,
    PRIMARY KEY ((tenant_id, artifact_id), promoted_at)
) WITH CLUSTERING ORDER BY (promoted_at DESC);
