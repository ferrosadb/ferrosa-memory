# Viz Snapshot Cursor-Progress Guard

**Status:** Implemented (`fix/viz-cursor-progress-guard`)
**Component:** `ferrosa-memory-core` — `src/viz.rs` (`CursorProgressGuard`), `src/http.rs` (`drain_snapshot_stream`, `send_streaming_viz_snapshot`), `assets/viz.html`
**Motivating incident:** fmem task t_c081cbb7 / ferrosa bug t_a0f922a3

## Problem

Ferrosa has a paging bug (tracked separately as t_a0f922a3): `paging_state`
for large-partition scans can cycle — `has_more_pages` never goes false and
the same ~5,000-row window is re-served forever. The viz snapshot builder
(the paged streams feeding `ws /viz/ws`) innocently followed the cursor: a
240 s live probe received 37,347 chunks / 18.6 M edge items from a ~50 k-edge
table and never received `SnapshotStreamEnd`; the UI showed only the ~5 k
unique edges it deduplicates client-side.

The DB fix is separate. This guard makes ferrosa-memory defend itself: a
pathological server cursor must produce a **loud, terminating** snapshot —
never an infinite duplicate stream.

## Design

Every paged stream in `send_streaming_viz_snapshot` (entities, folds, typed
edges, temporal edges; all-scope and per-session variants — they all share
the `drain_snapshot_stream` code path) runs a fresh `CursorProgressGuard`,
one per server-side cursor.

Per page (one producer batch) the guard tracks:

- a **page fingerprint** — SipHash over the ordered row keys plus row count;
- a **seen-key set** of per-row identity hashes (also used for dedup);
- **rows_delivered / unique_rows / pages** counters.

Row identity keys deliberately include enough columns that legitimate
repeats never look like a stall (typed edges: `session_id, src, type, dst`;
temporal edges additionally `relation_time, ordinal`; entities:
`session_id, entity_id`; folds: `session_id, fold_id`).

### Trip conditions (`CursorGuardConfig` defaults)

| Trigger | Condition | Default |
|---|---|---|
| `RepeatedPage` | byte-identical page content served N times | N = 3 |
| `DuplicationRatio` | `rows_delivered > factor x unique_rows`, only after a floor | factor = 3, floor = 5,000 rows |
| `PageBound` | absolute backstop on pages per cursor (no cheap server-side COUNT/estimate is available to the builder, so this is a fixed generous bound — deliberately *not* a new per-snapshot COUNT) | 100,000 pages |

`RepeatedPage` catches phase-aligned cycles fast (~2.5 k rows on the observed
bug); `DuplicationRatio` catches cycles whose page boundaries drift so
fingerprints never repeat; `PageBound` is the final backstop.

### On trip (designed, observable fallback — never silent)

1. Stop paging: the drain returns, dropping the producer future, which
   cancels the underlying paged query.
2. The deduplicated rows collected so far have already been emitted as
   `SnapshotStreamChunk`s; pending buffers are flushed.
3. One `tracing::error!` fires with `stream`, `reason`, `pages`,
   `rows_delivered`, `unique_rows`.
4. Remaining streams still run (each with its own guard) so one bad table
   does not blank the rest of the graph.
5. The stream ends explicitly with an error-bearing end event:

```json
{"type":"SnapshotStreamEnd","total_nodes":123,"total_edges":5000,
 "error":"snapshot truncated: typed-edges(current): server cursor not progressing (identical page served repeatedly; pages=21, rows_delivered=10500, unique_rows=5000)"}
```

The `error` field is omitted (`skip_serializing_if`) on healthy snapshots,
so existing clients are unaffected. `assets/viz.html` renders it as a
`TRUNCATED — …` connection status plus a `console.error`, instead of the
usual `LIVE`.

## Memory bounds

- Seen-key set: u64 hashes, capped at `seen_key_cap` (1 M keys ≈ 8 MB + set
  overhead; ~50 k at current graph scales). Beyond the cap, dedup and the
  duplication-ratio trigger **degrade loudly** (one `tracing::warn!`);
  repeated-page and page-bound detection remain active, so termination is
  still guaranteed.
- Fingerprint map: at most `max_pages` entries.
- Healthy streams never buffer the whole graph: chunks are flushed at 500
  rows exactly as before.

## Dedup semantics

Dedup is per stream (per cursor). Duplicate rows *across page boundaries* on
a healthy, progressing cursor are deduplicated on emission and never trip
the guard (covered by tests). Rows that repeat across *different* streams
(e.g. current vs legacy-swapped tenant probes) are emitted as before; the UI
already deduplicates by edge key.

## Tests

- `viz::tests::guard_*` — trip/no-trip unit tests for all three triggers,
  cap degradation, and key determinism.
- `http::tests::viz_snapshot_drain_without_guard_streams_cycling_cursor_forever`
  — permanent RED baseline: with detection disabled, a cycling cursor
  streams unboundedly.
- `http::tests::viz_snapshot_drain_guard_terminates_cycling_cursor_emitting_unique_edges_once`
  — the fix: prompt termination, each unique edge once, truncation recorded.
- `http::tests::viz_snapshot_drain_completes_healthy_slow_stream_without_guard_trip`
  — healthy-path guard: boundary duplicates dedup yes, trigger no.
- `http::tests::viz_streaming_snapshot_healthy_path_ends_without_error`
  — full builder protocol unchanged on healthy data (`error: None`).
- `http::tests::viz_html_renders_truncated_snapshot_end_loudly` — UI renders
  the error frame.

## Non-goals

- Fixing the ferrosa paging bug itself (t_a0f922a3).
- The temporal-edge `relation_time` decode bug (separate task).
- Guarding the bounded list APIs (`entity_list_all` etc.) — they already
  fail loudly at `CQL_*_LIST_MAX_ROWS`.
