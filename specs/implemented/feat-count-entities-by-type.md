---
type: feat
priority: P2
status: implemented
created: 2026-04-22
updated: 2026-07-24
---

# feat: add `count_entities_by_type` MCP tool

**Status:** implemented
**Consumer:** forge (status/diagnostics surface after retiring the Python CQL loader)
**Created:** 2026-04-22
**Driving need:** forge's `loader.rs::count_entities` returns a per-type/per-state histogram used by `frg ingest` status output and by agents asking "how many bugs are open in this session?". Today it does the bucketing via a Python `cassandra-driver` subprocess, duplicating ferrosa-memory's storage boundary. To delete the Python path, forge needs one MCP call that returns the same histogram. `get_stats` only reports a single `entity_count` total — not enough.

## Goal

Add `count_entities_by_type` — a read-only MCP tool that returns a tenant-wide
entity histogram by default, or a single-session histogram when `session_id` is
supplied. The histogram is broken down by `entity_type`, by `state`, and by the
joint `(type, state)` buckets.

The server owns:

- the CQL read path (single SELECT, bucketed server-side or via Rust-side fold)
- schema coupling (forge doesn't need to know the column layout)
- tenant isolation (same enforcement as `get_stats`)

The client (forge) computes any product-specific buckets it cares about (e.g. "code entities" = `total - document - section - bug`).

## Request

```json
{}
```

- `session_id`: optional. Omit it for tenant-wide counts. Supply a UUID to
  scope the histogram to that session.
- No other inputs. Tenant is derived from the authenticated caller context.

The shared entity-scope contract is:

- `list_entities`, `get_stats` (including the `stats` alias), and
  `count_entities_by_type` are tenant-wide when `session_id` is omitted.
- Supplying `session_id` scopes each tool to that session. `list_entities` may
  intentionally override this default with an explicit `scope` or
  `include_cross_session` argument.

## Response

```json
{
  "scope": "tenant",
  "session_id": null,
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
- `scope` is `"tenant"` and `session_id` is `null` when the request omits
  `session_id`; with an explicit session, they are `"session"` and that UUID.

## Invariants

1. Totals across `by_entity_type`, `by_state`, and the sum-of-sums of `by_type_and_state` all equal `total`. Server asserts before responding — mismatch is a server bug, not a per-row failure.
2. `total == 0` produces empty objects for the three breakdowns, never `null`.
3. Tenant isolation is enforced server-side. The tenant-wide default and an
   explicit `session_id` cannot widen access across tenants.
4. The tool is strictly read-only — no session `dirty` flag flip, no `last_activity` notification, no side-effects.
5. The default histogram, default `list_entities`, and default `get_stats`
   describe the same tenant-wide entity scope. Passing `session_id` scopes all
   three to the same session.
6. Response shape is stable: keys `scope`, `session_id`, `total`,
   `by_entity_type`, `by_state`, `by_type_and_state`, `duration_ms` are always
   present. New top-level keys MAY be added in a non-breaking way in future
   versions.

## Why This Shape

- Three breakdowns cover every realistic caller need: status tools want `by_entity_type`, bug-triage UIs want `by_state`, forge's existing `count_entities` wants `by_type_and_state["bug"]` for the `active`/`resolved` split.
- Returning all three in one call avoids the N-round-trip pattern forge would otherwise adopt (`retrieve_entities` per type doesn't work; it caps at 100 and is a similarity query).
- The response is bounded by the number of `(type, state)` buckets. CQL scans
  only the `entity_type` and `state` projection through paged iterators and
  aggregates in Rust, so tenant-wide counts do not materialize entity rows.
- Matches the server-owned-schema boundary: forge never touches `entity_store` columns directly.

## Acceptance Criteria

- [x] `count_entities_by_type` appears in `tools/list` with the documented input/output schema.
- [x] Handler registered in `dispatch.rs` alongside `get_stats`.
- [x] Reads via the `Storage` trait — no direct CQL in `dispatch.rs`. `Storage::entity_counts_by_type_and_state(ctx, query)` returns a flat histogram in the requested tenant or session scope for the handler to aggregate.
- [x] Sum-of-breakdowns invariant asserted server-side (debug_assert! is acceptable; log + return 500 in release is also acceptable — never quietly return a mismatched response).
- [x] Omitting `session_id` returns tenant-wide counts; passing it returns only
  that session's counts, consistent with `list_entities` and `get_stats`.
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
- Cross-tenant aggregation.
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

- Implementation uses `SELECT entity_type, state` projections for either the
  requested session or the tenant-wide scan, then buckets them in Rust as rows
  arrive from paged CQL iterators. It does not materialize entity rows or rely
  on CQL `GROUP BY`.
- Consider whether this tool should be promoted to include an optional `by_type_and_state` flag (defaulting to true) — keeps response compact for callers that only need type breakdown. Leave default on for v1 to keep the shape stable.

## Implementation Notes

- Added a new read-only storage seam,
  `Storage::entity_counts_by_type_and_state(ctx, query)`, and implemented it
  in both `MockStorage` and `CqlStorage`.
- `dispatch.rs` now exposes `count_entities_by_type` as a tier-1 tool and aggregates the flat histogram into `by_entity_type`, `by_state`, and `by_type_and_state` while asserting that all sums agree with `total`.
- The handler defaults omitted `session_id` to tenant-wide scope, applies an
  explicit session consistently with `list_entities` and `get_stats`, and does
  not dirty the session state.
- Verification covered:
  - unit tests for empty-session, tenant-wide default, explicit-session scope,
    known 3-type/2-state fixture, and tenant isolation
  - `tools/list` discoverability tests
  - a live MCP `tools/call` round-trip against the managed `18765` server returning a real histogram payload
