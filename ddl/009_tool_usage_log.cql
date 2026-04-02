-- Ferrosa Memory MCP Server — Tool usage logging for token analysis
-- Run against Ferrosa CQL interface after 008_intentions_repo_scope.cql

USE agent_memory;

CREATE TABLE IF NOT EXISTS tool_usage_log (
    tenant_id uuid,
    day text,
    call_id timeuuid,
    tool_name text,
    repo text,
    input_bytes int,
    output_bytes int,
    estimated_tokens int,
    latency_ms int,
    error boolean,
    PRIMARY KEY ((tenant_id, day), call_id)
) WITH CLUSTERING ORDER BY (call_id DESC);
