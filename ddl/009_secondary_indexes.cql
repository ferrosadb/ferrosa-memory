-- Secondary indexes to eliminate ALLOW FILTERING full-table scans.
-- Run against Ferrosa CQL interface after 007_intentions.cql

USE agent_memory;

-- entity_store: PK is (tenant_id, session_id), entity_id.
-- Queries that filter by tenant_id alone (without session_id) require
-- ALLOW FILTERING because tenant_id is only part of the composite
-- partition key. A secondary index on tenant_id lets Cassandra resolve
-- the partition without a full scan.

-- Supports: entity_list_all — "SELECT ... FROM entity_store WHERE tenant_id = ?"
-- Supports: edge_list_all entity scan fallback
CREATE INDEX IF NOT EXISTS idx_entity_by_tenant
    ON entity_store (tenant_id);

-- Supports: update_entity_state — "SELECT session_id FROM entity_store WHERE tenant_id = ? AND entity_id = ?"
-- Supports: find_entity_by_name — "SELECT ... FROM entity_store WHERE tenant_id = ? AND session_id = ?"
--   (entity_id is a clustering column that can be looked up independently
--    with this index, without requiring the full composite partition key)
CREATE INDEX IF NOT EXISTS idx_entity_by_id
    ON entity_store (entity_id);

-- Edge tables: all four edge tables store tenant_id as a regular column
-- but have non-tenant partition keys. The edge_list_all query and
-- per-entity neighbor queries filter on tenant_id, requiring ALLOW FILTERING.

-- mentioned_in: PK is (entity_id), fold_id.
-- Supports: edge_list_all — "SELECT ... FROM mentioned_in WHERE tenant_id = ?"
-- Supports: edge_mentioned_in_by_entity — "SELECT fold_id FROM mentioned_in WHERE entity_id = ? AND tenant_id = ?"
CREATE INDEX IF NOT EXISTS idx_mentioned_in_by_tenant
    ON mentioned_in (tenant_id);

-- co_occurs_with: PK is (entity_a), entity_b.
-- Supports: edge_list_all — "SELECT ... FROM co_occurs_with WHERE tenant_id = ?"
-- Supports: edge_co_occurs_by_a — "SELECT entity_b FROM co_occurs_with WHERE entity_a = ? AND tenant_id = ?"
CREATE INDEX IF NOT EXISTS idx_co_occurs_by_tenant
    ON co_occurs_with (tenant_id);

-- Supports: edge_co_occurs_by_b — "SELECT entity_a FROM co_occurs_with WHERE entity_b = ? AND tenant_id = ?"
--   entity_b is a clustering column; queries without the partition key (entity_a)
--   need a secondary index to avoid a full scan.
CREATE INDEX IF NOT EXISTS idx_co_occurs_by_entity_b
    ON co_occurs_with (entity_b);

-- supersedes: PK is (new_event_id), old_event_id.
-- Supports: edge_list_all — "SELECT ... FROM supersedes WHERE tenant_id = ?"
-- Supports: edge_supersedes_by_new — "SELECT old_event_id FROM supersedes WHERE new_event_id = ? AND tenant_id = ?"
CREATE INDEX IF NOT EXISTS idx_supersedes_by_tenant
    ON supersedes (tenant_id);

-- Supports: edge_supersedes_by_old — "SELECT new_event_id FROM supersedes WHERE old_event_id = ? AND tenant_id = ?"
--   old_event_id is a clustering column; queries without the partition key
--   (new_event_id) need a secondary index to avoid a full scan.
CREATE INDEX IF NOT EXISTS idx_supersedes_by_old_event
    ON supersedes (old_event_id);

-- folded_into: PK is (source_fold_id), target_fold_id.
-- Supports: edge_list_all — "SELECT ... FROM folded_into WHERE tenant_id = ?"
CREATE INDEX IF NOT EXISTS idx_folded_into_by_tenant
    ON folded_into (tenant_id);
