-- Native full-text indexes for lexical recall.
--
-- Older memory releases maintained deterministic term tables and scored BM25
-- client-side. Ferrosa now exposes full-text sidecars through
-- `WHERE column = fts_match('query')`, so these indexes let runtime search use
-- the native engine first while retaining the term tables as compatibility
-- fallback.

USE agent_memory;

CREATE INDEX IF NOT EXISTS idx_entity_name_fts
    ON entity_store (entity_name)
    USING 'fulltext';

CREATE INDEX IF NOT EXISTS idx_context_segments_bm25_text_fts
    ON context_segments (bm25_text)
    USING 'fulltext';

CREATE INDEX IF NOT EXISTS idx_document_chunks_bm25_text_fts
    ON document_chunks (bm25_text)
    USING 'fulltext';
