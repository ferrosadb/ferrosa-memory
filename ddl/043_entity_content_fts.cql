-- Native full-text index on entity CONTENT bodies (context_snippet).
--
-- ddl/040 indexed entity_name, context_segments.bm25_text and
-- document_chunks.bm25_text for lexical recall via `WHERE col = fts_match(...)`,
-- but an entity's content body (context_snippet) was left unindexed. On a fresh
-- install with NO embedding provider (ANN unavailable), a `search` for an
-- entity's content terms therefore returned nothing even though find/list could
-- see it — only its NAME matched lexically. Indexing context_snippet lets the
-- native engine serve entity content recall without embeddings. The deprecated
-- term tables (document_terms / context_segment_terms) are NOT revived.

USE agent_memory;

CREATE INDEX IF NOT EXISTS idx_entity_context_snippet_fts
    ON entity_store (context_snippet)
    USING 'fulltext';
