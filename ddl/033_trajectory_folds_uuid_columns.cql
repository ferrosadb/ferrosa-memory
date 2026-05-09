-- 033: Fix trajectory_folds fold identifier column types (timeuuid → uuid)
--
-- Rust storage binds uuid::Uuid for FoldEntry.fold_id and parent_fold_id.
-- The original bootstrap DDL declared these columns as CQL timeuuid, causing
-- live fold writes to fail during retrieval-time context promotion:
--   failed to serialize column fold_id: Rust uuid::Uuid against CQL Timeuuid
--
-- CQL cannot alter primary-key column types in place. The live dev cluster had
-- zero trajectory_folds rows when this migration was introduced, so a
-- drop/recreate migration is safe for this local schema repair.

USE agent_memory;

DROP TABLE IF EXISTS trajectory_folds;

CREATE TABLE trajectory_folds (
    session_id       uuid,
    fold_id          uuid,
    tenant_id        uuid,
    depth            int,
    parent_fold_id   uuid,
    raw_trajectory   text,
    fold_summary     text,
    fold_embedding   vector<float, 768>,
    token_count      int,
    compression_ratio float,
    status           text,
    created_at       timestamp,
    folded_at        timestamp,
    PRIMARY KEY ((session_id, tenant_id), fold_id)
) WITH CLUSTERING ORDER BY (fold_id DESC);

CREATE INDEX IF NOT EXISTS idx_fold_embedding ON trajectory_folds (fold_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};

ALTER TABLE trajectory_folds
    WITH extensions = {'graph.type': 'vertex', 'graph.label': 'Fold'};
