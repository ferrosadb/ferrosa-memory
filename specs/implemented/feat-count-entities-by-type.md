---
type: feat
priority: P2
status: implemented
created: 2026-04-22
updated: 2026-04-22
---

# feat: add `count_entities_by_type` MCP tool

**Status:** implemented
**Consumer:** forge (status/diagnostics surface after retiring the Python CQL loader)
**Created:** 2026-04-22
**Driving need:** forge's `loader.rs::count_entities` returns a per-type/per-state histogram used by `frg ingest` status output and by agents asking "how many bugs are open in this session?". Today it does the bucketing via a Python `cassandra-driver` subprocess, duplicating ferrosa-memory's storage boundary. To delete the Python path, forge needs one MCP call that returns the same histogram. `get_stats` only reports a single `entity_count` total — not enough.

## Goal

Add `count_entities_by_type` — a read-only MCP tool that returns the entity count for one `(tenant_id, session_id)` scope broken down by `entity_type` and by `state`, plus a joint breakdown for both.

The server owns:

- the CQL read path (single SELECT, bucketed server-side or via Rust-side fold)
- schema coupling (forge doesn't need to know the column layout)
- tenant isolation (same enforcement as `get_stats`)

The client (forge) computes any product-specific buckets it cares about (e.g. "code entities" = `total - document - section - bug`).

## Request

```json
{
  "session_id": "UUID"
}
```

- `session_id`: optional; defaults to the nil UUID (matches `get_stats` / `batch_update_entities` / `batch_delete_entities` convention).
- No other inputs. Tenant is derived from the authenticated caller context.

## Response

```json
{
  "total": 1234,
  "by_entity_type": {
    "document": 12,
    "section": 340,
    "bug": 45,
    "function": 600,
    "method": 180,
    "struct": 40,
    "trait": 8,
    "file": 17
  },
  "by_state": {
    "active": 1180,
    "resolved": 40,
    "dormant": 14
  },
  "by_type_and_state": {
    "bug": { "active": 30, "resolved": 15 },
    "function": { "active": 600 },
    "section": { "active": 338, "dormant": 2 }
  },
  "duration_ms": 6
}
```

- `total` equals the sum of `by_entity_type` values, equals the sum of `by_state` values. Server asserts this; clients assert as a sanity check.
- Keys in `by_entity_type` are the raw `entity_type` strings as stored — the tool does not normalize or filter. Unknown types (including future ones forge hasn't heard of) are passed through.
- Keys in `by_state` come from the `MemoryState` enum the server already tracks (`active` | `dormant` | `silent` | `unavailable` | `resolved`). Empty buckets are omitted.
- `by_type_and_state` is the joint histogram — only `(type, state)` combinations with count > 0 are included. Empty outer entries are omitted (no empty `{}` objects).
- `duration_ms` matches the `get_stats` / `ingest_entities` convention for self-reported latency.

## Invariants

1. Totals across `by_entity_type`, `by_state`, and the sum-of-sums of `by_type_and_state` all equal `total`. Server asserts before responding — mismatch is a server bug, not a per-row failure.
2. `total == 0` produces empty objects for the three breakdowns, never `null`.
3. Tenant isolation is enforced server-side. `session_id` cannot widen access across tenants.
4. The tool is strictly read-only — no session `dirty` flag flip, no `last_activity` notification, no side-effects.
5. Response shape is stable: keys `total`, `by_entity_type`, `by_state`, `by_type_and_state`, `duration_ms` are always present. New top-level keys MAY be added in a non-breaking way in future versions.

## Why This Shape

- Three breakdowns cover every realistic caller need: status tools want `by_entity_type`, bug-triage UIs want `by_state`, forge's existing `count_entities` wants `by_type_and_state["bug"]` for the `active`/`resolved` split.
- Returning all three in one call avoids the N-round-trip pattern forge would otherwise adopt (`retrieve_entities` per type doesn't work; it caps at 100 and is a similarity query).
- No pagination required — counts don't scale with result set size. Single CQL round-trip.
- Matches the server-owned-schema boundary: forge never touches `entity_store` columns directly.

## Acceptance Criteria

- [x] `count_entities_by_type` appears in `tools/list` with the documented input/output schema.
- [x] Handler registered in `dispatch.rs` alongside `get_stats`.
- [x] Reads via the `Storage` trait — no direct CQL in `dispatch.rs`. Add `Storage::entity_counts_by_type_and_state(ctx, session_id)` (or similar) that returns a flat histogram the handler aggregates.
- [x] Sum-of-breakdowns invariant asserted server-side (debug_assert! is acceptable; log + return 500 in release is also acceptable — never quietly return a mismatched response).
- [x] `session_id` omitted/nil returns counts for the nil session (same convention as `get_stats`).
- [x] Empty session (zero entities) returns `total: 0` + three empty objects + a `duration_ms`.
- [x] Unit test: known fixture with 3 entity types × 2 states → counts land in the right buckets and sums agree.
- [x] Integration test: `frg` consumer test (or mock) confirms the response round-trips through forge's MCP transport layer.
- [x] Tenant-isolation test: caller for tenant A cannot read tenant B's counts even when supplying B's session_id.

## Dependencies

- Existing `Storage` trait. If `entity_list_by_session` or equivalent already exists, the new method is likely a small fold over its result set. Otherwise one new CQL query is required.
- No new schema — reads existing columns.
- No new crates.

## Out of Scope

- Entity listing / pagination (use `retrieve_entities` for semantic queries).
- Cross-session aggregation.
- Counts by `entity_name` substring, tag, or other attrs.
- Write or mutate operations of any kind.
- Edge count breakdown — possible follow-on (`count_edges_by_type`) if forge needs it, but not part of this tool.

## Estimated Effort

- Tool registration + input/output schema: 0.5 day
- Storage method (single CQL + optional `GROUP BY` handling): 0.5 day
- Handler aggregation + invariant check + tests: 0.5 day
- Integration with `forge` to delete `loader.rs::count_entities`: 0.5 day (forge side)
- **Total:** ~1 sprint day on fmem, ~0.5 day on forge follow-up.

## Related

- `feat-ingest-entities.md` — the write-side tool that shares the same `(tenant_id, session_id)` scope and response-envelope discipline.
- forge's `loader.rs` removal — blocked by this tool for the `count_entities` call site; all other `loader.rs` call sites already have MCP equivalents (`ingest_entities`).

## Notes

- Implementation suggestion: if CQL `GROUP BY` is awkward on the existing `entity_store` partition scheme, do a single `SELECT entity_type, state FROM entity_store WHERE tenant_id = ? AND session_id = ?` and bucket in Rust. Same cost profile as `entity_count` in `get_stats`.
- Consider whether this tool should be promoted to include an optional `by_type_and_state` flag (defaulting to true) — keeps response compact for callers that only need type breakdown. Leave default on for v1 to keep the shape stable.

## Implementation Notes

- Added a new read-only storage seam, `Storage::entity_counts_by_type_and_state(ctx, session_id)`, and implemented it in both `MockStorage` and `CqlStorage`.
- `dispatch.rs` now exposes `count_entities_by_type` as a tier-1 tool and aggregates the flat histogram into `by_entity_type`, `by_state`, and `by_type_and_state` while asserting that all sums agree with `total`.
- The handler defaults omitted `session_id` to the nil UUID and does not dirty the session state.
- Verification covered:
  - unit tests for empty-session, nil-session default, known 3-type/2-state fixture, and tenant isolation
  - `tools/list` discoverability tests
  - a live MCP `tools/call` round-trip against the managed `18765` server returning a real histogram payload
