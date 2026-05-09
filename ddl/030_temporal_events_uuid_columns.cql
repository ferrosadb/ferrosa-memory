-- 030: Fix temporal_events column types (timeuuid → uuid)
--
-- The scylla Rust driver rejects binding a Rust Uuid to a CQL timeuuid
-- column. Since the table is empty (temporal_fact_count=0 everywhere),
-- dropping and recreating is safe and avoids ALTER-restrictions on
-- clustering columns.
--
-- See: temporal.rs uses Uuid::now_v7() for event_id, and
-- cql_storage.rs/temporal_put binds event_id + supersedes_id as Uuid.

USE agent_memory;

DROP TABLE IF EXISTS temporal_events;

CREATE TABLE temporal_events (
    tenant_id        uuid,
    entity_id        uuid,
    event_time       timestamp,
    event_id         uuid,
    fact_text        text,
    supersedes_id    uuid,
    valid_until      timestamp,
    source_session   uuid,
    confidence       float,
    PRIMARY KEY ((tenant_id, entity_id), event_time, event_id)
) WITH CLUSTERING ORDER BY (event_time DESC, event_id DESC);

-- Re-apply graph annotation (lost on DROP)
ALTER TABLE temporal_events
    WITH extensions = {'graph.type': 'vertex', 'graph.label': 'Fact'};
