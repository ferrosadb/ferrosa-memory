-- Ferrosa Memory MCP Server — durable mobile control queue and replay log.
-- Run after 050_session_task_workspace_index.cql.
--
-- These are application-owned coordination tables. The cloud gateway never
-- receives command payloads, results, events, or resume cursors.

CREATE TABLE IF NOT EXISTS agent_memory.mobile_control_cursor_state (
    tenant_id uuid,
    server_fingerprint text,
    next_cursor bigint,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, server_fingerprint))
);

-- Events are bucketed by cursor so replay never creates an unbounded CQL
-- partition. Cursor allocation may leave gaps after a process crash; cursor
-- values are never reused.
CREATE TABLE IF NOT EXISTS agent_memory.mobile_control_events (
    tenant_id uuid,
    server_fingerprint text,
    cursor_bucket int,
    cursor bigint,
    event_id uuid,
    command_id uuid,
    event_type text,
    payload text,
    created_at timestamp,
    PRIMARY KEY ((tenant_id, server_fingerprint, cursor_bucket), cursor)
) WITH CLUSTERING ORDER BY (cursor ASC);

CREATE TABLE IF NOT EXISTS agent_memory.mobile_control_commands (
    tenant_id uuid,
    server_fingerprint text,
    command_id uuid,
    command_type text,
    request_payload text,
    state text,
    result_payload text,
    result_cursor bigint,
    created_at timestamp,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, server_fingerprint), command_id)
);
