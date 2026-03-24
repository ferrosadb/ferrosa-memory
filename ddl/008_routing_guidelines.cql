-- Routing guideline versions for strategy selection (ADR-002)
-- Written by ferrosa-memory-batch, read by MCP server tool_router.

USE agent_memory;

CREATE TABLE IF NOT EXISTS routing_guidelines (
    version          text,
    rules            text,      -- JSON-encoded strategy rules
    created_at       timestamp,
    PRIMARY KEY (version)
);
