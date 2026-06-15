-- Ferrosa Memory MCP Server — durable active session tasks.
-- Run after 038_retraction_record.cql.
--
-- These are Ferrosa Memory app-owned tables, not graph backing tables. They
-- provide deterministic task continuity across compaction and client restarts.

USE agent_memory;

CREATE TABLE IF NOT EXISTS agent_memory.session_tasks_by_id (
    tenant_id uuid,
    session_id uuid,
    task_id uuid,
    title text,
    description text,
    status text,
    priority int,
    tags set<text>,
    parent_task_id uuid,
    focus_rank int,
    client text,
    outcome_summary text,
    created_at timestamp,
    updated_at timestamp,
    completed_at timestamp,
    PRIMARY KEY ((tenant_id, session_id), task_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.session_tasks_by_status (
    tenant_id uuid,
    session_id uuid,
    status text,
    priority int,
    updated_at timestamp,
    task_id uuid,
    title text,
    parent_task_id uuid,
    focus_rank int,
    PRIMARY KEY ((tenant_id, session_id, status), priority, updated_at, task_id)
) WITH CLUSTERING ORDER BY (priority ASC, updated_at DESC, task_id ASC);

CREATE TABLE IF NOT EXISTS agent_memory.session_task_focus_stack (
    tenant_id uuid,
    session_id uuid,
    stack_index int,
    task_id uuid,
    reason text,
    created_at timestamp,
    PRIMARY KEY ((tenant_id, session_id), stack_index)
) WITH CLUSTERING ORDER BY (stack_index ASC);

CREATE TABLE IF NOT EXISTS agent_memory.session_task_aliases (
    tenant_id uuid,
    session_id uuid,
    alias_scope text,
    alias text,
    task_id uuid,
    created_at timestamp,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, session_id, alias_scope), alias)
);

CREATE TABLE IF NOT EXISTS agent_memory.session_task_events (
    tenant_id uuid,
    session_id uuid,
    created_at timestamp,
    event_id uuid,
    task_id uuid,
    event_type text,
    payload text,
    PRIMARY KEY ((tenant_id, session_id), created_at, event_id)
) WITH CLUSTERING ORDER BY (created_at DESC, event_id ASC);

CREATE TABLE IF NOT EXISTS agent_memory.session_task_policies (
    tenant_id uuid,
    session_id uuid,
    auto_task_detection text,
    auto_resume text,
    max_active_before_subagents int,
    max_children_before_subagents int,
    confidence_threshold double,
    updated_at timestamp,
    PRIMARY KEY (tenant_id, session_id)
);
