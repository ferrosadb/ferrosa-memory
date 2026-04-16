-- Ferrosa Memory MCP Server — Richer entity schema (Sprint 1 of skills layer)
-- Run against Ferrosa CQL interface after 019_type_registry.cql
--
-- Adds columns that back the richer entity model:
--   description            — curated, retrieval-optimized prose
--   description_embedding  — vector for description ANN retrieval
--   tags                   — denormalized tag list (direct + ancestor tags)
--   properties             — type-specific structured JSON (stored as text)
--   content_hash           — idempotent re-ingest key
--   updated_at             — last modification timestamp
--   scope                  — "session" or "global"
--   ingested_by_session    — audit + session-affinity signal
--
-- All ALTERs are additive and non-breaking: existing rows read back with
-- NULL/default values for the new columns, which deserialize to
-- `None` / empty / `EntityScope::Session` on the Rust side.
--
-- If any ALTER errors with "column already exists", that column was added
-- in a previous run — safe to ignore that specific error and continue.

USE agent_memory;

ALTER TABLE entity_store ADD description text;
ALTER TABLE entity_store ADD description_embedding vector<float, 768>;
ALTER TABLE entity_store ADD tags text;          -- JSON array, e.g. ["testing","quality"]
ALTER TABLE entity_store ADD properties text;    -- JSON object, type-specific
ALTER TABLE entity_store ADD content_hash text;
ALTER TABLE entity_store ADD updated_at timestamp;
ALTER TABLE entity_store ADD scope text;         -- "session" | "global"
ALTER TABLE entity_store ADD ingested_by_session uuid;

-- HNSW index on description_embedding (mirrors the existing entity_embedding index).
CREATE INDEX IF NOT EXISTS idx_entity_description_embedding
    ON entity_store (description_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};

-- Secondary index on scope for cross-session global queries.
-- NOTE: tags are stored as JSON text (see above). Tag-based filtering runs
-- client-side after the scope/session filter narrows the candidate set. When
-- tag cardinality grows large enough to need a server-side index, split into
-- a dedicated entity_tags table keyed by (tenant_id, tag_name).
CREATE INDEX IF NOT EXISTS idx_entity_scope ON entity_store (scope);
