-- Secondary indexes on session_id for edge tables.
-- Required for edge_list_session() to work reliably.
-- Without these, ALLOW FILTERING on session_id does full table scans
-- that return incomplete results on Ferrosa.

USE agent_memory;

CREATE INDEX IF NOT EXISTS idx_co_occurs_by_session
    ON co_occurs_with (session_id);

CREATE INDEX IF NOT EXISTS idx_mentioned_in_by_session
    ON mentioned_in (session_id);

CREATE INDEX IF NOT EXISTS idx_folded_into_by_session
    ON folded_into (session_id);
