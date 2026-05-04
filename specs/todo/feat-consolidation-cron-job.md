---
type: feature
priority: P2
status: draft
created: 2026-05-04
reported-by: Hermes Agent (post-debugging session)
---

# fmem consolidation as a cron job with retry/backoff

## Problem

`run_consolidation` is currently a synchronous MCP tool call. In the recent session it timed out, leaving the graph flat (no `CO_OCCURS` edges between the 13 newly created entities). There is no retry, no backoff, and no way to run consolidation asynchronously.

## Why it matters

Consolidation is computationally expensive — it clusters entities, computes embeddings, and writes graph edges. Under load (schema drift, node failures, or simply a large entity count), it can exceed the MCP HTTP timeout (30s). Synchronous consolidation forces the agent to choose between:
- Waiting for a timeout
- Skipping consolidation and leaving the graph flat

## Desired Behavior

Consolidation should run as a background cron job:
1. A cron job runs `run_consolidation` every 30 minutes (or after every N new entities).
2. On failure, it retries with exponential backoff (1s, 2s, 4s, max 30s).
3. After 3 failures, it logs an ERROR and stops retrying until the next cron tick.
4. Results (edges created, clusters discovered) are stored in a log table or returned on the next successful run.
5. The agent can still call `run_consolidation` manually for immediate feedback, but it doesn't block on it.

## Proposed Implementation

### Short-term: Cron job in `ferrosa-memory-batch`
- Add a `consolidate` subcommand to `ferrosa-memory-batch`.
- Run it via `cronjob(action='create', schedule='every 30m')` from the Hermes agent.
- The batch binary has a longer timeout and can handle partial failures.

### Medium-term: In-process background worker
- In `ferrosa-memory-mcp`, spawn a Tokio task that runs consolidation on a timer.
- Store results in a CQL table: `consolidation_runs (run_id, started_at, ended_at, edges_created, status, error)`.
- The MCP tool `run_consolidation` just triggers an immediate run and returns the last result.

### Long-term: Event-driven consolidation
- Every `smart_ingest` emits an event to a channel.
- A worker batches events (e.g., 5 new entities or 30s elapsed) and runs consolidation on the batch.
- This is incremental, not full-graph, so it's faster and cheaper.

## Acceptance Criteria

- [ ] A cron job runs consolidation every 30m without manual intervention.
- [ ] After 3 consecutive failures, the job logs ERROR and pauses until the next tick.
- [ ] A successful run writes `edges_created` and `clusters_discovered` to a log table.
- [ ] The agent can query `get_stats` to see when the last consolidation succeeded.
- [ ] Manual `run_consolidation` still works and triggers an immediate run.

## Related

- `bug-run-consolidation-timeout-under-prepare-failures.md` — timeout resilience
- `feat-ingest-entities.md` — bulk ingest pipeline
- `memory-sync.md` — memory lifecycle and eviction
