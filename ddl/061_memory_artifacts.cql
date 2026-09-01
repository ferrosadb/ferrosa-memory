-- Policy-governed artifact links over globally content-addressed blobs.
-- Blob rows carry bytes/storage facts only; each artifact row carries the
-- uploader, policy-relevant semantic metadata and independent lifecycle.

CREATE TABLE IF NOT EXISTS agent_memory.artifact_blob (
    checksum       text PRIMARY KEY,
    size_bytes     bigint,
    media_type     text,
    storage_locator text,
    created_at     timestamp
);

CREATE TABLE IF NOT EXISTS agent_memory.artifact_link (
    tenant_id       uuid,
    artifact_id     text,
    checksum        text,
    uploader_id     text,
    captured_path   text,
    host_id         text,
    host_label      text,
    captured_at     timestamp,
    uploaded_at     timestamp,
    state           text,
    predecessor_id  text,
    deleted_at      timestamp,
    PRIMARY KEY ((tenant_id), artifact_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.artifact_tag (
    tenant_id       uuid,
    artifact_id     text,
    tag             text,
    source          text,
    immutable       boolean,
    PRIMARY KEY ((tenant_id, artifact_id), tag)
);

CREATE TABLE IF NOT EXISTS agent_memory.artifact_by_state (
    tenant_id       uuid,
    state           text,
    page_key        text,
    artifact_id     text,
    uploader_id     text,
    checksum        text,
    PRIMARY KEY ((tenant_id, state), page_key)
);

CREATE TABLE IF NOT EXISTS agent_memory.artifact_approval (
    tenant_id       uuid,
    artifact_id     text,
    approval_id     uuid,
    action          text,
    state           text,
    reviewer_id     text,
    decided_at      timestamp,
    PRIMARY KEY ((tenant_id, artifact_id), approval_id)
);
