# Temporal Semantic Context Segments Blueprint

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Status:** Todo  
**Created:** 2026-05-06 14:07 PDT  
**Owner:** Ferrosa Memory + Hermes Agent integration  
**Goal:** Persist pre-compaction conversation context as locally segmented, hybrid-searchable, time-linked semantic pages so Hermes can dynamically expand compacted memories back into the surrounding raw context needed for long-horizon reasoning.

**Architecture:** Add a first-class `context_segments` storage plane in ferrosa-memory with BM25 + Nomic vector retrieval, temporal predecessor/successor edges, and rerank-aware retrieval expansion. Hermes calls this plane before compression and uses retrieval expansion after search hits to page forward/back around relevant chunks.

**Tech Stack:** Rust workspace (`ferrosa-memory-core`, `ferrosa-memory-mcp`), FerrosaDB/CQL vector indexes, local Ollama/Nomic embeddings, deterministic segmentation in Rust or Hermes Python, optional local ONNX/fastembed reranker later.

---

## 1. Problem Statement

Hermes currently compacts long sessions by summarizing old context and discarding raw messages. This makes long-horizon tests brittle: retrieval can find a relevant summary/entity, but the LLM cannot reliably inspect the raw lead-up or aftermath that made the fact meaningful.

The desired behavior is:

1. Before compaction, Hermes sends the raw soon-to-be-discarded messages to fmem.
2. fmem splits the raw context into ordered semantic segments.
3. Each segment is searchable by:
   - BM25 / lexical search over raw text
   - Nomic vector search over segment embeddings
   - existing hybrid/RRF + reranking signals
4. Segments are linked in time order via temporal edges.
5. When search retrieves a segment, Hermes can ask fmem to expand the hit by `prev_n` and `next_n` pages to recover surrounding context.
6. Dynamic compaction can inject only the relevant expanded pages into the LLM context instead of blindly restoring everything.

This directly supports long-horizon memory tests by separating:
- retrieval quality: did fmem find the right evidence page?
- expansion quality: did prev/next recover sufficient raw context?
- packing quality: did Hermes inject the expanded evidence in a useful position?

---

## 2. Recommended Segmentation Strategy

### 2.1 Default: deterministic local segmentation, no separate model

Use deterministic segmentation first. It is cheap, local, reproducible, and avoids running another local model alongside Nomic.

Recommended algorithm, in order:

1. Normalize messages into immutable message records:
   - `role`
   - `content`
   - `turn_index`
   - `created_at` / monotonic logical time if real timestamp unavailable
   - `source_platform`, `chat_id`, `thread_id` when available
2. Split on hard boundaries:
   - explicit topic/thread/session change
   - tool-result boundary after large tool outputs
   - compaction boundary
   - elapsed time gap above threshold, e.g. 10-20 minutes
3. Within each hard boundary, pack messages into chunks with soft limits:
   - target 700-1,200 tokens
   - max 1,800 tokens
   - overlap 1 prior message or ~100-150 tokens only when a message straddles a boundary
4. Use embedding-drift only as a secondary semantic boundary:
   - generate Nomic embedding for a rolling window summary/text
   - split when cosine similarity to current segment centroid drops below threshold, e.g. `< 0.72`
   - enforce min segment size to avoid over-fragmentation

Why deterministic first:
- No additional model operational burden.
- Stable tests: same transcript yields same segment IDs/hashes.
- Nomic already provides the semantic drift signal.
- Easy to evaluate boundary quality with oracle/random retrieval baselines.

### 2.2 Optional local small models if deterministic segmentation is insufficient

Use these only after deterministic segmentation has benchmark evidence showing weak boundaries:

| Option | Approx size | Runtime | Use | Recommendation |
|---|---:|---|---|---|
| `nomic-embed-text-v2-moe` | already present | Ollama | embeddings + drift | **Default** |
| `fastembed` / ONNX reranker such as `bge-reranker-v2-m3` or `jina-reranker-v2-base-multilingual` | ~200-600MB | CPU OK | rerank top candidates | Good phase 3 add-on |
| TextTiling/C99 lexical segmentation | no model | Rust/Python | topic-boundary detection from lexical cohesion | Good deterministic supplement |
| MiniLM/e5-small embedding model | 90-130MB | CPU fast | alternate tiny embeddings | Not needed if Nomic is healthy |
| small local LLM summarizer | 1-4GB | CPU/GPU variable | segment titles/summaries only | Avoid in MVP; optional enrichment |

Do **not** require an LLM to decide segment boundaries in the MVP. If a model is used, use embeddings/rerankers only.

---

## 3. Data Model

### 3.1 New table: `context_segments`

Add a dedicated table instead of overloading `entity_store` or `trajectory_folds`.

Rationale:
- Segments are raw evidence pages, not named entities.
- They need ordered traversal by session + segment ordinal.
- They need their own BM25/vector indexes.
- They must survive compaction without being summarized away.

Proposed CQL:

```sql
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
```

Indexes:

```sql
CREATE INDEX IF NOT EXISTS idx_context_segments_embedding
    ON agent_memory.context_segments (segment_embedding)
    USING 'vector'
    WITH OPTIONS = {'method': 'hnsw', 'metric': 'cosine', 'dimensions': '768'};

-- Exact syntax depends on FerrosaDB's BM25/text index surface.
-- If it exposes a text index similar to vector/phonetic, use that.
CREATE INDEX IF NOT EXISTS idx_context_segments_bm25
    ON agent_memory.context_segments (bm25_text)
    USING 'bm25';

CREATE INDEX IF NOT EXISTS idx_context_segments_hash
    ON agent_memory.context_segments (content_hash);

CREATE INDEX IF NOT EXISTS idx_context_segments_conversation
    ON agent_memory.context_segments (conversation_id);
```

If FerrosaDB cannot yet expose BM25 for arbitrary text columns, add a `segment_terms` side table for deterministic BM25 until the native index exists:

```sql
CREATE TABLE IF NOT EXISTS agent_memory.context_segment_terms (
    tenant_id      uuid,
    session_id     uuid,
    term           text,
    segment_id     uuid,
    tf             int,
    doc_len        int,
    PRIMARY KEY ((tenant_id, session_id, term), segment_id)
);
```

This is slower than native BM25 but makes the API contract testable.

### 3.2 Temporal edge representation

Use first-class temporal edges, not only generic `related_to` metadata.

Preferred schema:

```sql
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
```

Required edge types:
- `next_context_segment`
- `previous_context_segment`
- `segment_contains_turn` optional later
- `segment_before_fold` / `segment_after_fold` optional later
- `segment_temporally_near_entity` optional later

Use `temporal_edges` for traversal and keep `prev_segment_id` / `next_segment_id` denormalized on `context_segments` for fast direct paging.

---

## 4. fmem API Surface

### 4.1 `ingest_context_segments`

Input:

```json
{
  "session_id": "uuid",
  "conversation_id": "discord:<channel>:<thread> or hermes:<session>",
  "messages": [
    {
      "role": "user|assistant|tool|system",
      "content": "raw text",
      "turn_index": 42,
      "created_at": "RFC3339 optional",
      "metadata": {}
    }
  ],
  "segmentation": {
    "strategy": "deterministic_v1",
    "target_tokens": 1000,
    "max_tokens": 1800,
    "semantic_drift_threshold": 0.72
  },
  "embed_missing": true
}
```

Output:

```json
{
  "segments_created": 8,
  "segments_skipped": 2,
  "segments": [
    {
      "segment_id": "uuid",
      "segment_index": 0,
      "start_turn": 10,
      "end_turn": 14,
      "prev_segment_id": null,
      "next_segment_id": "uuid",
      "content_hash": "sha256:..."
    }
  ],
  "edges_created": 14
}
```

Semantics:
- Idempotent by `(tenant_id, session_id, content_hash)`.
- Server owns embeddings when `embed_missing=true` and embedding client is configured.
- If embedding fails, lexical-only ingest can still succeed with a warning bucket.
- Edge creation is fail-loud for missing segment IDs.

### 4.2 `search_context_segments`

Input:

```json
{
  "session_id": "uuid optional",
  "query": "what happened before we fixed gateway threading?",
  "query_embedding": [0.0],
  "limit": 10,
  "scope": "session_only|global_only|both|conversation",
  "conversation_id": "optional",
  "rerank": true,
  "expand": { "prev": 1, "next": 2, "max_tokens": 6000 }
}
```

Output:

```json
{
  "results": [
    {
      "segment_id": "uuid",
      "score": 0.42,
      "sources": ["bm25", "ann", "rerank", "temporal_warmth"],
      "segment_text": "...",
      "expanded_context": [
        {"segment_id": "prev", "direction": "previous", "segment_text": "..."},
        {"segment_id": "hit", "direction": "hit", "segment_text": "..."},
        {"segment_id": "next", "direction": "next", "segment_text": "..."}
      ]
    }
  ]
}
```

Ranking pipeline:
1. BM25 candidate list.
2. Nomic ANN candidate list.
3. Existing RRF merge.
4. Optional cross-encoder/local reranker on top 20-50.
5. Temporal locality boost:
   - if one segment is high-confidence, adjacent segments get a small boost for expansion eligibility, not necessarily top-level ranking.
6. Existing warmth/PageRank/reputation signals where applicable.

### 4.3 `get_context_window`

Input:

```json
{
  "segment_id": "uuid",
  "session_id": "uuid optional",
  "prev": 2,
  "next": 2,
  "max_tokens": 8000
}
```

Output:
- Ordered list from oldest to newest.
- Includes hit marker and cumulative token count.
- Uses temporal edges first, then `prev_segment_id` / `next_segment_id` fallback.

---

## 5. Hermes Integration

### 5.1 Before compression

Modify Hermes provider hook:
- File: `~/.hermes/hermes-agent/plugins/memory/ferrosa/__init__.py`
- Hook: `FerrosaMemoryProvider.on_pre_compress(messages)`

Current stopgap: Hermes chunks locally and calls `smart_ingest` section entities. Replace with fmem-owned `ingest_context_segments` once the tool exists.

Desired behavior:

```python
self._client.call("ingest_context_segments", {
    "session_id": self._effective_session_id(),
    "conversation_id": self._conversation_id_from_runtime(),
    "messages": normalized_messages,
    "segmentation": {"strategy": "deterministic_v1"},
    "embed_missing": True,
})
```

Return text to compression prompt:

```text
Ferrosa Memory persisted N raw context segments before compaction. Later queries can expand retrieved segments with get_context_window(prev=N,next=N).
```

### 5.2 During retrieval/prefetch

Modify:
- `FerrosaMemoryProvider.prefetch(...)`
- dynamic compaction/session context assembly in Hermes once provider prefetch can carry expanded context blocks.

Behavior:
1. Run normal memory/entity search.
2. Run `search_context_segments(query, expand={prev:1,next:1,max_tokens:...})`.
3. Inject a compact evidence block:

```text
## Ferrosa Context Segment Recall
Hit: session ..., turns 40-44, score ...
Previous page: ...
Hit page: ...
Next page: ...
```

Budget rule:
- Start with `prev=1,next=1`.
- Expand further only if the model asks, or if a task explicitly requires audit trail.
- Never inject more than configured `memory.context_segments.max_expanded_tokens`.

### 5.3 Time awareness in Hermes

Hermes should preserve two clocks:

1. Wall-clock time:
   - message `created_at`, Discord/Telegram timestamps where available.
2. Logical conversation time:
   - session id
   - turn index
   - compaction epoch
   - segment index

These fields let fmem answer:
- “what happened just before this?”
- “what did we do after that?”
- “show the raw context around the remembered chunk.”

---

## 6. Implementation Plan

### Phase 0: Contract tests and schema specification

**Files:**
- Create: `ddl/0XX_context_segments.cql`
- Create: `crates/ferrosa-memory-core/tests/context_segments.rs`
- Modify: `crates/ferrosa-memory-core/src/migration.rs`
- Modify: `specs/test-specification.md` later after design stabilizes

**Tests first:**
1. `deterministic_segmenter_splits_on_message_boundaries_and_token_limits`
2. `deterministic_segmenter_splits_on_semantic_drift_when_embeddings_available`
3. `ingest_context_segments_is_idempotent_by_content_hash`
4. `ingest_context_segments_creates_next_previous_temporal_edges`
5. `search_context_segments_rrf_merges_bm25_and_ann_candidates`
6. `get_context_window_returns_ordered_prev_hit_next_pages`

**Verification:**

```bash
cargo test -p ferrosa-memory-core context_segments -- --nocapture
```

Expected RED initially: missing module/types/storage methods.

### Phase 1: Core types and deterministic segmenter

**Files:**
- Create: `crates/ferrosa-memory-core/src/context_segment.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Modify: `crates/ferrosa-memory-core/src/types.rs`

Add:
- `ContextMessage`
- `ContextSegment`
- `SegmentationConfig`
- `SegmentIngestResult`
- deterministic segmenter
- content hash helper
- token estimator helper

Implementation details:
- Use stable SHA-256 hash over normalized segment text + role/turn range.
- Token estimator can be approximate in MVP: `chars / 4` or existing tokenizer if available.
- Keep semantic drift optional: if embeddings are not provided, do hard-boundary + token packing only.

### Phase 2: Storage trait + MockStorage

**Files:**
- Modify: `crates/ferrosa-memory-core/src/storage.rs`

Add trait methods:
- `context_segment_put`
- `context_segment_get`
- `context_segment_get_by_hash`
- `context_segment_search_ann`
- `context_segment_search_bm25`
- `context_segment_neighbors`
- `temporal_edge_put`
- `temporal_edge_list_from`
- `temporal_edge_list_to`

MockStorage must support all of them for unit tests.

### Phase 3: CQL storage + migrations

**Files:**
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs`
- Add DDL migration under `ddl/`
- Modify migration registry

Implement:
- Insert/upsert segment rows.
- ANN query over `segment_embedding`.
- BM25 query if FerrosaDB supports native BM25.
- If native BM25 is not ready, implement term-table fallback for MVP.
- Temporal edge insert/list.

Important: avoid `ALLOW FILTERING` for hot traversal. `get_context_window` must be primary-key/edge-index shaped.

### Phase 4: Core ingest/search/window functions

**Files:**
- `crates/ferrosa-memory-core/src/context_segment.rs`
- `crates/ferrosa-memory-core/src/hybrid_search.rs` if integrating into main search

Implement:
- `ingest_context_segments(storage, embedding_client, ctx, session_id, params)`
- `search_context_segments(...)`
- `get_context_window(...)`

Rerank plan:
- MVP: existing RRF + optional score normalization.
- Phase 4b: add local reranker trait:

```rust
pub trait Reranker {
    async fn rerank(&self, query: &str, candidates: &[ContextSegment]) -> anyhow::Result<Vec<RerankedSegment>>;
}
```

Potential local rerankers:
- ONNX cross-encoder through `ort` if already acceptable dependency-wise.
- Fastembed reranker if Rust support is stable enough.
- Keep reranker optional and disabled by default.

### Phase 5: MCP tools

**Files:**
- Modify: `crates/ferrosa-memory-mcp/src/main.rs`
- Add tool handlers or split into `src/tools/context_segments.rs` if tool module structure supports it.

Expose:
- `ingest_context_segments`
- `search_context_segments`
- `get_context_window`

Tool contract must be fail-loud:
- Bad UUIDs reject.
- Empty messages reject.
- Missing embedding endpoint degrades only if lexical path can still be indexed; response must include warnings.
- Reranker unavailable never blocks retrieval unless `rerank_required=true`.

### Phase 6: Hermes provider integration

**Files:**
- `~/.hermes/hermes-agent/plugins/memory/ferrosa/__init__.py`
- Tests: `~/.hermes/hermes-agent/tests/test_ferrosa_skill_provider.py` or a new provider-specific test module.

Replace stopgap chunking with fmem-owned tool calls:
- `on_pre_compress` calls `ingest_context_segments`.
- `prefetch` calls `search_context_segments` with bounded expansion.
- system prompt block advertises segment recall availability when stats show segments exist.

Add tests:
- provider calls `ingest_context_segments` before compaction.
- provider injects expanded segment recall into prefetch output.
- provider does not pass explicit `session_id` to old `smart_ingest` stopgap paths after replacement.

### Phase 7: Long-horizon evaluation harness updates

**Files:**
- `crates/ferrosa-memory-eval/src/...`
- Existing eval specs under `specs/plans/eval-blueprint-v1.md` and related eval harness files.

Add metrics:
- segment recall@k against ground-truth evidence segment IDs
- expansion coverage: whether prev/next pages include required lead-up/aftermath
- packing ablation: retrieved-only vs retrieved+expanded vs oracle-expanded
- random segment baseline
- oracle segment baseline

This is critical for proving the feature improves long-horizon memory tests rather than only adding storage.

---

## 7. Acceptance Criteria

1. Given a 100+ turn transcript, fmem stores deterministic context segments with stable IDs/hashes.
2. Each segment has:
   - BM25-searchable text
   - Nomic vector embedding when embedding service is available
   - `prev` and `next` temporal edges
   - turn/time metadata
3. `search_context_segments` returns relevant segments via BM25, ANN, or fused ranking.
4. `get_context_window` returns ordered prev/hit/next context within a token budget.
5. Hermes compaction uses `ingest_context_segments` before discarding raw messages.
6. Hermes prefetch can inject expanded segment windows into context under a strict token budget.
7. Eval harness can score evidence-segment recall and expansion coverage separately from answer grading.
8. No cleanup/destructive commands are required to run the feature tests.

---

## 8. Risks and Design Decisions

### Risk: native BM25 may not exist yet

Mitigation: implement `context_segment_terms` fallback and keep API identical. Native BM25 can replace fallback later.

### Risk: segment table duplicates entity_store functionality

Decision: keep separate. Segments are raw evidence pages with temporal order; entities are semantic nodes. Mixing them would pollute entity search and make page traversal awkward.

### Risk: another local model increases ops burden

Decision: MVP uses deterministic segmentation + Nomic only. Reranker is optional and disabled by default.

### Risk: explicit fmem `session_id` bugs

Known issue: prior `smart_ingest` paths mishandled explicit session IDs. New context segment APIs must be tested with explicit session IDs at the storage and MCP layers. Do not route segment ingest through `smart_ingest`.

### Risk: prompt budget blow-up

Mitigation: `search_context_segments(... expand.max_tokens)` and Hermes provider caps. Expansion is bounded and explicit.

---

## 9. Open Questions

1. Does FerrosaDB currently expose a native BM25 index syntax for text columns? If not, term-table fallback is MVP.
2. Should `context_segments` be session-scoped only, or should a global/conversation scope exist for Discord threads that span Hermes sessions?
3. Should segment IDs be deterministic UUIDv5 from content hash, or random UUID plus content-hash idempotency? Recommendation: UUIDv5 for stable eval fixtures, random UUID acceptable if content_hash lookup is reliable.
4. Should temporal edges be a generic graph relation table extension or a dedicated temporal edge table? Recommendation: dedicated table plus optional graph projection.
5. Should segment summaries be model-generated? Recommendation: no for MVP; use deterministic title/excerpt. Add local summarizer later if eval proves value.

---

## 10. Recommended Local Model/Code Choices

MVP:
- Segmentation: deterministic Rust implementation.
- Embeddings: existing Ollama `nomic-embed-text-v2-moe`, 768 dimensions.
- Lexical: native BM25 if available, otherwise deterministic term-table fallback.
- Rerank: existing RRF first.

Phase 2:
- Add TextTiling/C99-style lexical cohesion to improve deterministic boundaries without a model.
- Add embedding-drift split using Nomic rolling centroids.

Phase 3:
- Optional local reranker for top-k only:
  - `bge-reranker-v2-m3` if multilingual/general quality matters.
  - `jina-reranker-v2-base-multilingual` if already easy to run locally.
  - Keep behind config: `context_segments.reranker.enabled=false` by default.

Avoid for MVP:
- Local LLM segment boundary generation.
- Cloud reranking.
- Large summarization models.

---

## 11. Exact First Implementation Tasks

1. Add RED unit tests for deterministic segmenting in `crates/ferrosa-memory-core/tests/context_segments.rs`.
2. Add `ContextMessage`, `ContextSegment`, and `SegmentationConfig` in `types.rs` or new `context_segment.rs`.
3. Implement deterministic segmenter with token/max-message thresholds.
4. Add DDL for `context_segments` and `temporal_edges`.
5. Register migration in `migration.rs`.
6. Add Storage trait methods and MockStorage implementation.
7. Implement core ingest with idempotency and temporal edge creation.
8. Implement context window traversal over temporal edges.
9. Implement ANN + BM25/fallback search and RRF fusion.
10. Expose MCP tools.
11. Replace Hermes provider stopgap with fmem-owned tools.
12. Add eval harness scenario with oracle/random/retrieved/expanded ablations.

Verification commands:

```bash
cd ~/src/ferrosa-suite/ferrosa-memory
cargo test -p ferrosa-memory-core context_segments -- --nocapture
cargo test -p ferrosa-memory-mcp context_segments -- --nocapture
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Hermes-side verification after fmem tools land:

```bash
cd ~/.hermes/hermes-agent
venv/bin/python -m pytest tests/test_ferrosa_skill_provider.py -q -o 'addopts='
venv/bin/python -m py_compile plugins/memory/ferrosa/__init__.py
```
