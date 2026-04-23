CREATE TABLE IF NOT EXISTS agent_memory.approvals_by_target (
    tenant_id uuid,
    artifact_kind text,
    artifact_ref text,
    created_at timestamp,
    approval_id uuid,
    decision text,
    review_note text,
    reviewer text,
    scope text,
    workspace_scope text,
    session_scope uuid,
    mirror_entity_id uuid,
    PRIMARY KEY ((tenant_id, artifact_kind, artifact_ref), created_at, approval_id)
) WITH CLUSTERING ORDER BY (created_at DESC, approval_id DESC);
