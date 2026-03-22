-- Ferrosa Memory MCP Server — Entity memory state column
-- Run against Ferrosa CQL interface after 005_vector_columns.cql

USE agent_memory;

ALTER TABLE entity_store ADD state text;
