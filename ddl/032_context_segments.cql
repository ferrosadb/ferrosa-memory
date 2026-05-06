-- Temporal semantic context segments for pre-compaction raw context recall.
-- Additive-only migration: new storage plane plus temporal traversal edges.

CREATE TABLE IF NOT EXISTS agent_memory.context_segments (
    tenant_id          uuid,
    session_id         uuid,
    segment_id         uuid,
    source_session     uuid,
    source_fold_id     uuid,
    conversation_id    text,
    segment_index      int,
    start_turn         int,
    end_turn           int,
    start_time         timestamp,
    end_time           timestamp,
    segment_text       text,
    segment_summary    text,
    bm25_text          text,
    segment_embedding  vector<float, 768>,
    token_count        int,
    content_hash       text,
    prev_segment_id    uuid,
    next_segment_id    uuid,
    created_at         timestamp,
    PRIMARY KEY ((tenant_id, session_id), segment_id)
);

CREATE INDEX IF NOT EXISTS idx_context_segments_embedding
    ON agent_memory.context_segments (segment_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};

CREATE INDEX IF NOT EXISTS idx_context_segments_hash
    ON agent_memory.context_segments (content_hash);

CREATE INDEX IF NOT EXISTS idx_context_segments_conversation
    ON agent_memory.context_segments (conversation_id);

-- Deterministic lexical fallback until FerrosaDB exposes native BM25 for text columns.
CREATE TABLE IF NOT EXISTS agent_memory.context_segment_terms (
    tenant_id      uuid,
    session_id     uuid,
    term           text,
    segment_id     uuid,
    tf             int,
    doc_len        int,
    PRIMARY KEY ((tenant_id, session_id, term), segment_id)
);

CREATE TABLE IF NOT EXISTS agent_memory.temporal_edges (
    tenant_id       uuid,
    session_id      uuid,
    src_id          uuid,
    edge_type       text,
    dst_id          uuid,
    relation_time   timestamp,
    ordinal         int,
    metadata        text,
    created_at      timestamp,
    PRIMARY KEY ((tenant_id, session_id), src_id, edge_type, dst_id)
);

CREATE INDEX IF NOT EXISTS idx_temporal_edges_dst
    ON agent_memory.temporal_edges (dst_id);
