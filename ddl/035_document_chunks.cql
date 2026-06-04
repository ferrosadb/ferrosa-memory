-- First-class document chunk retrieval plane.
--
-- Documents remain represented as entity_store rows for identity, metadata,
-- graph edges, and governance. This table stores semantically ordered chunks
-- optimized for retrieval, with prev/next links so callers can expand around
-- a hit without hoping adjacent context also ranked highly.

CREATE TABLE IF NOT EXISTS agent_memory.document_chunks (
    tenant_id              uuid,
    session_id             uuid,
    document_id            uuid,
    chunk_id               uuid,
    ordinal                int,
    source_doc_id          text,
    title                  text,
    section_path           text,
    semantic_kind          text,
    content                text,
    bm25_text              text,
    chunk_embedding        vector<float, 768>,
    token_count            int,
    content_hash           text,
    prev_chunk_id          uuid,
    next_chunk_id          uuid,
    overlap_from_prev      boolean,
    overlap_to_next        boolean,
    metadata               text,
    created_at             timestamp,
    updated_at             timestamp,
    PRIMARY KEY ((tenant_id, session_id), chunk_id)
);

CREATE INDEX IF NOT EXISTS idx_document_chunks_embedding
    ON agent_memory.document_chunks (chunk_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};

CREATE INDEX IF NOT EXISTS idx_document_chunks_document
    ON agent_memory.document_chunks (document_id);

CREATE INDEX IF NOT EXISTS idx_document_chunks_hash
    ON agent_memory.document_chunks (content_hash);

CREATE TABLE IF NOT EXISTS agent_memory.document_terms (
    tenant_id      uuid,
    session_id     uuid,
    term           text,
    chunk_id       uuid,
    document_id    uuid,
    tf             int,
    doc_len        int,
    PRIMARY KEY ((tenant_id, session_id, term), chunk_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.document_phonetic_terms (
    tenant_id      uuid,
    session_id     uuid,
    phonetic_code  text,
    chunk_id       uuid,
    document_id    uuid,
    term           text,
    PRIMARY KEY ((tenant_id, session_id, phonetic_code), chunk_id)
);
