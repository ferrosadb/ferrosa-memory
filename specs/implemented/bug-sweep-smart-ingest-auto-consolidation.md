# Bug Sweep: Smart Ingest Auto-Consolidation

Status: Implemented
Date: 2026-06-03

## Scope

Stop asking the LLM to count newly created memories before consolidation. The memory server owns the threshold and queues background consolidation after enough `smart_ingest` creates in a session.

The same sweep covers a reported `record_outcome` serialization failure by returning UUID-bearing entity updates as string arrays and adding an end-to-end regression for `record_outcome` responses that include UUID `entity_ids`.

## Focused Blueprint

Architecture: `dispatch::SessionState` already owns the idle consolidation queue and dirty flag. Add per-session smart-ingest create counters there so thresholding stays near queue ownership.

DSM: Keep changes in `dispatch.rs`; do not change `dream.rs` consolidation logic or storage schemas.

Threat model: Thresholding must not block memory writes. If queueing fails after a successful create, log and return the created entity response.

FMEA:
- Failure mode: LLM miscounts created entities and never consolidates. Mitigation: server-side counter.
- Failure mode: repeated manual `run_consolidation` calls duplicate queue entries. Existing queue coalescing remains in place.
- Failure mode: record outcome response fails JSON serialization with UUID-bearing input. Mitigation: expose updated entity IDs as strings and regression-check full MCP wrapped response serialization.

## Plan

1. Add a `SMART_INGEST_AUTO_CONSOLIDATE_THRESHOLD` and per-session create counter.
2. Queue the session when `smart_ingest` returns `Created` for the threshold count.
3. Reset the counter when the session is queued or already pending.
4. Remove model-facing guidance that asks the LLM to count entities before consolidation.
5. Add regression tests for auto queueing and `record_outcome` response serialization.

## Success Criteria

- The first 9 `Created` smart-ingest calls for a session do not queue consolidation.
- The 10th `Created` smart-ingest call queues that session for idle consolidation.
- Smart-ingest responses say consolidation is automatic.
- `record_outcome` with UUID `entity_ids` serializes through the full MCP `CallToolResult` and reports updated IDs as strings.
