---
type: bug
priority: P1
status: draft
created: 2026-05-04
reported-by: Hermes Agent (post-debugging session)
---

# fmem `run_consolidation` timeouts under PREPARE failures

## Problem

`mcp_ferrosa_memory_run_consolidation` times out when the graph has unmaterialized edges or schema drift. In the recent debugging session, the call returned `TimeoutError` after creating 13 new entities. The knowledge graph stays flat — entities exist but no `CO_OCCURS` edges are generated.

This is a cascading failure:
1. FRSA-BUG-025 causes `trajectory_folds` ANN PREPARE to fail.
2. The MCP falls back to non-ANN LIMIT queries.
3. Graph edge writes (`CO_OCCURS_WITH`) fail because `first_seen` column is missing (migration 31 not applied).
4. Consolidation tries to write edges, hits the same failures, and hangs until timeout.

## Why it matters

Consolidation is what makes the knowledge graph useful. Without it, entities are isolated points — no clusters, no hidden connection discovery, no `CO_OCCURS` edges. The graph degrades to a flat key-value store.

## Desired Behavior

Consolidation should be resilient to partial failures:
1. If edge writes fail, log the failure and continue (don't hang).
2. If the timeout is hard-coded, make it configurable or add a fast-fail path.
3. Consider running consolidation as an async background job rather than a synchronous MCP call.

## Proposed Fix Directions

### Short-term: Timeout + retry
- Add a `timeout_ms` parameter to `run_consolidation`.
- On timeout, return partial results (which edges were written before failure) instead of an opaque error.
- Retry with exponential backoff, up to 3 attempts.

### Medium-term: Async background consolidation
- Queue consolidation jobs in a durable queue (CQL table or in-memory channel with persistence).
- A background worker processes the queue with a longer timeout.
- The MCP tool only enqueues; it doesn't wait for completion.
- This matches the pattern used by `ferrosa-memory-batch` for backfills.

### Long-term: Pre-consolidation health check
- Before running consolidation, verify schema version and cluster health.
- If migrations are pending or nodes are down, return an explicit `unhealthy` status instead of hanging.

## Acceptance Criteria

- [ ] `run_consolidation` completes within 5s even when 50% of edge writes fail.
- [ ] Partial consolidation results are returned (entities processed, edges written, failures).
- [ ] No `TimeoutError` when the cluster has schema drift — instead, a clear diagnostic message.
- [ ] Unit test: mock storage that rejects 50% of writes; consolidation still returns progress.

## Related

- `FRSA-BUG-025` — upstream Ferrosa PREPARE bug
- `ddl/031_co_occurs_first_seen.cql` — migration that would fix the `first_seen` column
- `memory-sync.md` — memory lifecycle and eviction
- `feat-ingest-entities.md` — bulk ingest pipeline
