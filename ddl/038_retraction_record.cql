-- Retraction records: audit + restore metadata for logically-deleted
-- (retracted) memory objects. The object itself stays in its store but is
-- marked retracted/unavailable (entities reuse entity_store.state =
-- 'unavailable' from ddl/006_entity_state.cql); this table holds the
-- "why / who / restore window / prior state" needed to restore or purge it.
--
-- Generic over object type so anything forgettable (entity, fold, memo,
-- intention, temporal fact, edge) can be retracted and restored uniformly.

CREATE TABLE IF NOT EXISTS agent_memory.retraction (
    tenant_id        uuid,
    object_id        uuid,
    object_type      text,        -- entity | fold | memo | intention | temporal | edge
    session_id       uuid,        -- needed to re-locate the object on restore/purge
    retracted_at     timestamp,
    reason           text,
    actor            text,
    prior_state      text,        -- MemoryState before retraction: active|dormant|silent
    restorable_until timestamp,   -- after this, eligible for hard purge
    forget_id        uuid,        -- links back to forget_journal
    PRIMARY KEY ((tenant_id, object_id), retracted_at)
) WITH CLUSTERING ORDER BY (retracted_at DESC);

-- Find all retractions from one forget op (restore/audit).
CREATE INDEX IF NOT EXISTS idx_retraction_forget
    ON agent_memory.retraction (forget_id);
