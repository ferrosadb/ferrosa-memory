-- Migration 31: add first_seen timestamp to co_occurs_with edge table
-- This column is written by the graph Cypher MERGE path in
-- crates/ferrosa-memory-core/src/graph.rs (build_co_occurs_merge_query).
-- It was referenced in code before the corresponding schema change was
-- registered as a migration, causing every dream/consolidation run to
-- log "SET references unknown property 'first_seen'".
--
-- Safe: ALTER TABLE ADD is additive and non-destructive. Re-running is
-- harmless (Scylla/Cassandra will error on duplicate column, but the
-- migration runner tracks versions and never re-applies a recorded migration).

USE agent_memory;

ALTER TABLE co_occurs_with ADD first_seen timestamp;
