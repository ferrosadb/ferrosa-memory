-- Explicit tenant reviewers for artifact policy decisions. The uploader is
-- always eligible for an ordinary activation; this table grants additional
-- review authority without trusting a client-supplied reviewer name.
CREATE TABLE IF NOT EXISTS agent_memory.artifact_reviewer (
    tenant_id   uuid,
    reviewer_id text,
    granted_at  timestamp,
    granted_by  text,
    PRIMARY KEY ((tenant_id), reviewer_id)
);
