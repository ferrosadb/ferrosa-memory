-- Add vector embedding columns to existing tables.
-- Run after 001-004 DDL scripts.
-- Requires Ferrosa with vector<float, N> support.

USE agent_memory;

-- Memo cache: embedding for fuzzy cache hit detection
ALTER TABLE memo_cache ADD result_embedding vector<float, 768>;

-- Trajectory folds: embedding for ANN semantic search
ALTER TABLE trajectory_folds ADD fold_embedding vector<float, 768>;

-- Entity store: embedding for ANN entity search
ALTER TABLE entity_store ADD entity_embedding vector<float, 768>;

-- HNSW indexes for ANN queries
CREATE INDEX IF NOT EXISTS idx_memo_embedding ON memo_cache (result_embedding)
    USING 'vector' WITH OPTIONS = {'similarity_function': 'cosine'};

CREATE INDEX IF NOT EXISTS idx_fold_embedding ON trajectory_folds (fold_embedding)
    USING 'vector' WITH OPTIONS = {'similarity_function': 'cosine'};

CREATE INDEX IF NOT EXISTS idx_entity_embedding ON entity_store (entity_embedding)
    USING 'vector' WITH OPTIONS = {'similarity_function': 'cosine'};
