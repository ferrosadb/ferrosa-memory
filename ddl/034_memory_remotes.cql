-- Remote teacher/learner memory transfer tables.
-- Additive/idempotent migration for signed packets, learner imports, provenance, and feedback.

CREATE TABLE IF NOT EXISTS agent_memory.memory_remotes (
    tenant_id uuid,
    remote_id uuid,
    instance_id uuid,
    name text,
    endpoint text,
    trust_class text,
    public_key_fingerprint text,
    enabled boolean,
    created_at timestamp,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id), remote_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.remote_policy_facts (
    tenant_id uuid,
    remote_id uuid,
    fact_id uuid,
    kind_json text,
    created_at timestamp,
    expires_at timestamp,
    PRIMARY KEY ((tenant_id, remote_id), fact_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.teaching_packets (
    tenant_id uuid,
    remote_id uuid,
    packet_id uuid,
    teacher_instance_id uuid,
    request_id uuid,
    source_namespace text,
    query text,
    packet_json text,
    expires_at timestamp,
    created_at timestamp,
    PRIMARY KEY ((tenant_id, remote_id), packet_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.teaching_items (
    tenant_id uuid,
    packet_id uuid,
    item_id uuid,
    remote_id uuid,
    kind text,
    title text,
    summary text,
    item_json text,
    content_hash text,
    created_at timestamp,
    PRIMARY KEY ((tenant_id, packet_id), item_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.remote_stubs (
    tenant_id uuid,
    remote_id uuid,
    state text,
    stub_id uuid,
    packet_id uuid,
    item_id uuid,
    local_entity_id uuid,
    title text,
    summary text,
    stub_json text,
    created_at timestamp,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, remote_id, state), stub_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.memory_provenance (
    tenant_id uuid,
    local_entity_id uuid,
    provenance_id uuid,
    remote_id uuid,
    packet_id uuid,
    item_id uuid,
    content_hash text,
    signature_hash text,
    imported_at timestamp,
    PRIMARY KEY ((tenant_id, local_entity_id), provenance_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.memory_conflicts (
    tenant_id uuid,
    local_entity_id uuid,
    conflict_id uuid,
    remote_id uuid,
    item_id uuid,
    reason text,
    resolved boolean,
    created_at timestamp,
    resolved_at timestamp,
    PRIMARY KEY ((tenant_id, local_entity_id), conflict_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.memory_feedback (
    tenant_id uuid,
    remote_id uuid,
    target_id uuid,
    feedback_id uuid,
    feedback_type text,
    note text,
    created_at timestamp,
    PRIMARY KEY ((tenant_id, remote_id, target_id), feedback_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.import_batches (
    tenant_id uuid,
    remote_id uuid,
    batch_id uuid,
    packet_id uuid,
    state text,
    imported_count int,
    rejected_count int,
    conflict_count int,
    explanation text,
    created_at timestamp,
    completed_at timestamp,
    PRIMARY KEY ((tenant_id, remote_id), batch_id)
);
