-- Ferrosa Memory MCP Server — cwd/workspace-spanning task resource index.
-- Run after 049_consolidation_lease_queue.cql.
--
-- This table is a side index for active task discovery by workspace/cwd across
-- all agent sessions. The canonical row remains session_tasks_by_id; reads
-- re-fetch by (session_id, task_id) and re-check workspace/status so stale
-- index rows cannot become authoritative.

CREATE TABLE IF NOT EXISTS agent_memory.session_tasks_by_workspace (
    tenant_id uuid,
    workspace text,
    status text,
    priority int,
    updated_at timestamp,
    session_id uuid,
    task_id uuid,
    title text,
    parent_task_id uuid,
    focus_rank int,
    PRIMARY KEY ((tenant_id, workspace, status), priority, updated_at, session_id, task_id)
) WITH CLUSTERING ORDER BY (priority ASC, updated_at DESC, session_id ASC, task_id ASC);
