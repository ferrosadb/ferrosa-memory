-- Ferrosa Memory MCP Server — verifiable mobile cursor block ownership.
-- Run after 051_mobile_control_store.cql.
--
-- Some Ferrosa versions apply a conditional UPDATE without returning the
-- Cassandra-compatible [applied] result column. A unique reservation token
-- lets the allocator read back the winning write without ever guessing that
-- it owns a cursor range.

ALTER TABLE agent_memory.mobile_control_cursor_state
    ADD reservation_token uuid;
