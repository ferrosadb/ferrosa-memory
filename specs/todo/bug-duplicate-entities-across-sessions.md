---
type: bug
priority: P2
status: draft
created: 2026-05-04
reported-by: Hermes Agent (post-debugging session)
---

# Duplicate entities created across sessions — no pre-ingest dedup

## Problem

When manually migrating facts from file-based memory to fmem, I accidentally created duplicate entities for the same concept:
- Two versions of "ferrosa dev cluster" (entities `95c8b877` and `b9775a6a`)
- Two versions of "hippo health brokerage localhost" (`ac68cfa3` and `a998b604`)

There is no pre-ingest dedup guard. `smart_ingest` creates a new entity every time unless the caller explicitly runs `hybrid_search` first.

## Why it matters

Duplicate entities pollute the graph, make `hybrid_search` noisy, and waste embedding storage. Over time, the same bug or config fact will be ingested 3-4 times, each with a different UUID.

## Desired Behavior

`smart_ingest` (or a wrapper) should:
1. Run a `hybrid_search` or `retrieve_entities` (phonetic + ANN) for the proposed entity name/content.
2. If a similar entity exists above a threshold (e.g., 0.85 cosine similarity), skip creation and return the existing entity_id.
3. If the caller wants to force a new entity (e.g., two genuinely different bugs with similar names), provide an override flag.

## Proposed Fix Directions

### Option A: Deduplication inside `smart_ingest`
- Before creating, `smart_ingest` runs a quick phonetic search on `entity_name`.
- If a match exists, compare content similarity (embedding or simple text overlap).
- If above threshold, return `Skipped (duplicate of <id>)`.
- Pros: Transparent to callers. Cons: Adds ~15ms latency per ingest.

### Option B: Deduplication as a separate tool
- Add `mcp_ferrosa_memory_check_duplicate(query)` that returns candidate duplicates.
- Callers decide whether to skip or proceed.
- Pros: Explicit, no hidden logic. Cons: Requires callers to remember to use it.

### Option C: Post-ingest merge job
- A cron job runs consolidation, detects clusters of near-duplicate entities, and proposes merges.
- Human (or agent) reviews and approves.
- Pros: Handles complex cases (partial overlap, evolving facts). Cons: Delayed, requires UI.

**Recommendation: Option A for `smart_ingest`, Option C as a weekly background job.**

## Acceptance Criteria

- [ ] Calling `smart_ingest` twice with the same entity name and content returns the same entity_id.
- [ ] Calling `smart_ingest` with a name that is a 90% text match to an existing entity returns the existing entity_id.
- [ ] A genuinely different entity with a similar name (e.g., "FRSA-BUG-024" vs "FRSA-BUG-025") still creates a new entity.
- [ ] Unit test: mock search returns a high-similarity entity; `smart_ingest` skips creation.

## Related

- `feat-auto-session-entity-extraction.md` — automatic extraction would exacerbate duplicates without this
- `feat-ingest-entities.md` — bulk ingest pipeline
- `memory-sync.md` — memory lifecycle and eviction
