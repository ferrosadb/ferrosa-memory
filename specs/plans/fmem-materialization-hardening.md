---
title: "ferrosa-memory Materialization Hardening Pass"
executive_summary: >
  Feature freeze checklist for closing the materialization / non-streaming bug
  class in ferrosa-memory. Grounded in a frg materialization-scan of
  ferrosa-memory-core (76 files, 174 findings: 86 high, 88 medium) and in one
  confirmed production hang (PR #186) where an entity count transferred 77k
  rows to produce an integer. Work is organised into six batches executed with
  /tdd + /rust + /refactor, each with an explicit done-gate. Scope is fmem
  only; ferrosa engine changes are out of scope and must be filed upstream.
status: active
phase: hardening
updated: 2026-07-30
scope: ferrosa-memory only (no ferrosa engine changes)
source: frg materialization-scan crates/ferrosa-memory-core/src, PR #186, t_fbe2c2f8
mode: TDD (Red → Green → Refactor per finding group)
---

## 1. Why this pass exists

`memory_metrics` hung indefinitely on a 77k-entity tenant. Root cause was not
the database: the identical `count(*)` answers in 2.1s, while fmem's
client-side row-stream never completed and had its connection shut down
mid-scan. fmem was **sizing a set by transferring the set**.

That instance is fixed (PR #186). The scan says it is not unique:

| kind | high | medium | total |
|---|---:|---:|---:|
| `growing_vec_in_io_path` | 35 | 47 | 82 |
| `collect_in_io_path` | 13 | 33 | 46 |
| `query_rows_materialization` | 31 | 0 | 31 |
| `whole_file_read` | 0 | 8 | 8 |
| `expanding_map_of_vec_in_io_path` | 7 | 0 | 7 |
| **total** | **86** | **88** | **174** |

By file: `cql_storage.rs` 84, `http.rs` 22, `migration.rs` 17, `dispatch.rs`
13, `storage.rs` 7, `migration_backfill.rs` 6, `skill.rs` 4, `config.rs` 4.

**No new features until batches 1–3 are green.**

## 2. Ground rules

1. **A finding is not a bug.** Classify every one: `bounded-intentional`,
   `test-only`, `false-positive`, `production-refactor`, or `design-task`.
   Record the verdict; an unclassified finding is not done.
2. **fmem only.** If the fix belongs in the engine, file it against `ferrosa`
   and stop. Never add a local workaround — that is what created this class
   (`e886f62` was an explicit "workaround for a Ferrosa quirk" whose
   justification had already been fixed elsewhere in the same file).
3. **Never bound a RESULT to fix a scan.** ferrosa streams and spills; a cap
   on returned rows means the path is not streaming. Bound *work* (timeouts,
   paging), not results.
4. **Guards encode invariants, not mechanisms.** PR #33 added a guard
   *requiring* `execute_iter`, which locked in the unbounded shape. Assert
   "does not transfer N to learn N", not "calls function X".
5. **Fail loud beats silent truncation.** Where a bounded path is genuinely
   correct, it must log/error at the bound, never silently return a prefix.

## 3. Execution batches

Each batch: `/tdd` for the RED test, `/rust` for streaming patterns, then
`/refactor`. Each finding group gets a test that fails on the old shape.

### Batch 1 — count/size paths (highest value, smallest risk)
- [ ] Inventory every `*_count*` / `*_len*` / `exists` style API in
      `cql_storage.rs` and `storage.rs`.
- [ ] For each: does it transfer rows? A count that streams rows is **always**
      wrong — convert to a server-side aggregate via
      `cql_get_i64_from_single_aggregate`.
- [ ] RED test per converted path asserting the aggregate shape.
- [ ] **Done-gate:** no count API issues a row-returning SELECT; a source
      guard asserts it for the whole module, not per-function.

### Batch 2 — serving-path reads (`dispatch.rs`, `http.rs`)
- [ ] Triage all 13 `dispatch.rs` + 22 `http.rs` findings.
- [ ] Any user-facing read that `collect()`s a full scan → stream to the
      transport (chunked HTTP / MCP incremental) per `rust-streaming-patterns.md`.
- [ ] **Done-gate:** no serving-path handler holds a whole result set; live
      smoke against the 3-node native cluster passes incl. `memory_metrics`.

### Batch 3 — `cql_storage.rs` remaining high findings (~50 after batch 1)
- [ ] Group the 31 `query_rows_materialization` by call shape; fix by group,
      not one-by-one.
- [ ] `growing_vec_in_io_path` (35 high): distinguish bounded accumulation
      (fine) from unbounded scan collection (not fine).
- [ ] **Done-gate:** zero high findings in `cql_storage.rs` that are not
      explicitly classified with a recorded reason.

### Batch 4 — migration / backfill (`migration.rs` 17, `migration_backfill.rs` 6)
- [ ] These run at startup and can block boot — same failure mode as the
      gateway bootstrap deadlock found on Fly.
- [ ] **Done-gate:** migrations/backfills page or stream; startup cannot be
      wedged by table size.

### Batch 5 — medium findings + `whole_file_read` (8) + `expanding_map_of_vec` (7)
- [ ] Map-of-Vec grouping in I/O paths is the classic OOM shape — check each
      against the compose-node OOM history.
- [ ] **Done-gate:** all mediums classified; production ones fixed or filed.

### Batch 6 — regression tripwire
- [ ] Add `frg materialization-scan` to fmem CI with a **baseline allowlist**
      (current classified findings), failing on anything new.
- [ ] Allowlist entries require a reason string — an entry without one is a
      CI failure (cf. the ferrosa audit where allow-entries without `symbol`
      silently hid new violations).
- [ ] **Done-gate:** a deliberately-added `collect()` over a full scan fails CI.

## 4. Cross-cutting cleanups discovered en route

- [ ] Grep for other "workaround … Ferrosa" comments; each is both a possible
      perf bug and a CLAUDE.md policy violation. Fix or file upstream.
- [ ] `handle_memory_metrics` mis-attributed a handler-wide hang to the next
      sub-count's label, sending triage the wrong way. Each sub-count must
      report its own identity on failure.
- [ ] Unexplained: node3 RSS 1288MB vs node1/node2 41MB/27MB on the local
      native cluster. Investigate before assuming it is unrelated.

## 5. Exit criteria for the freeze

1. Batches 1–3 done-gates met.
2. `frg materialization-scan` baseline committed, CI tripwire live (batch 6).
3. `smoke-18765.sh` green end-to-end against the 3-node native cluster.
4. Every remaining finding carries a recorded classification.
5. No engine workarounds added; anything engine-side filed against `ferrosa`.
