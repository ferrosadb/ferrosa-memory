---
type: bug
priority: P2
status: implemented
created: 2026-04-16
updated: 2026-04-20
reported-by: deploy smoke test (2026-04-16, using ../research skills)
---

# `ingest_skill` silently treats storage errors as "not found"

## Observed

During the post-launch smoke test the following sequence ran back-to-back:

1. `ingest_skill(name="threat-model", ...)` → `action: created`, entity_id `c9a704ca…`
2. `ingest_skill(name="code-audit", prerequisites=["threat-model"], ...)`
   → `missing_prerequisites: ["threat-model"]` even though threat-model
   had just been written and was immediately visible to
   `retrieve_skills_for_context`.

Re-running the same `code-audit` ingest a few seconds later succeeded:
`prerequisites: ["threat-model"]` appeared on the REQUIRES graph, no
`missing_prerequisites`. The second run used a new `content_hash` so the
idempotent skip path didn't mask anything. Swapping to new throwaway skill
names (`debug-prereq-test-parent` + `debug-prereq-test-child`) on the same
warm MCP process worked on the first try — so the symptom is flaky, not
structural.

## Root Cause

`crates/ferrosa-memory-core/src/skill.rs` uses the fail-quiet pattern on
every storage lookup in the ingest path. Two instances are load-bearing for
this bug:

```rust
// line 191-194 — looking up the skill being ingested
let mut existing_matches = storage
    .entity_find_phonetic(ctx, storage_session, &params.name)
    .await
    .unwrap_or_default();         // ⬅ error → treated as "not found"

// line 342-345 — looking up each declared prerequisite
let mut matches = storage
    .entity_find_phonetic(ctx, storage_session, prereq_name)
    .await
    .unwrap_or_default();         // ⬅ error → treated as "not found"
// ...
let Some(prereq) = matches.first() else {
    missing_prereqs.push(prereq_name.clone());
    continue;
};
```

`.unwrap_or_default()` discards the `anyhow::Error` and silently substitutes
an empty `Vec<EntityEntry>`. The caller can't tell whether the prereq
genuinely doesn't exist or the CQL query failed (timeout, transient node
wedge, schema propagation lag, read-after-write consistency gap, etc.).

There are ~10 such sites in `skill.rs` (grep
`unwrap_or_default\|unwrap_or(None)`). The lookup-vs-error distinction
matters most on ingest (lines 191 + 342) because a false negative causes
(a) duplicate entity creation on the name lookup and (b) incorrect
`missing_prerequisites` reporting on the prereq lookup.

## Why the Race Even Exists

The underlying flakiness is not yet pinned down. Two plausible causes:

1. **Read-after-write propagation lag**: `entity_find_phonetic` issues a
   `SELECT … FROM entity_store WHERE tenant_id=? AND session_id=? ALLOW
   FILTERING`. With RF=3 and LOCAL_QUORUM on both writes and reads, any
   quorum of 3 overlaps by at least 1, so a committed write should be
   visible to any subsequent quorum read. A miss would imply the write
   acknowledgement returned before the memtable was actually readable on
   the replicas the read coordinator picked — a ferrosa-side issue.
2. **Transient CQL error** returned as `Err(_)` from the storage call,
   silently converted to `vec![]` by `.unwrap_or_default()`. This fits the
   flaky / cold-start pattern we saw during the smoke test.

Hypothesis #2 is more consistent with the observed flakiness and node1's
intermittent CQL-handshake wedging. Hypothesis #1 would need a separate
ferrosa bug report if evidence accumulates.

Removing the silent fail-quiet (this bug) will let us distinguish them the
next time it happens.

## Expected

- If `entity_find_phonetic` errors, `ingest_skill` must propagate the error
  (or retry with a bounded backoff) — **never** continue as if the skill
  genuinely doesn't exist.
- `missing_prerequisites` on the response must reflect the caller-declared
  prereqs that were **confirmed** not to exist, not the ones we gave up on
  because of a transient lookup failure.

## Proposed Fix Direction

Minimal, bounded scope:

1. Replace `.unwrap_or_default()` at lines 191 and 342 with explicit
   `match`/`?` that returns the storage error to the caller as
   `InvalidParams` or `InternalError`. Surface the error in the MCP
   response.
2. Optionally: a single short retry (e.g. 200ms backoff, 1 retry) wrapping
   `entity_find_phonetic` on the prereq lookup. This makes the happy path
   resilient to a one-off transient blip without masking repeated failures.
3. Leave the other ~8 sites for a separate sweep — they're mostly on read
   paths (retrieve / verify / invoke) where returning an empty list is
   arguably the correct degraded behavior, or they'll benefit from the
   same treatment but aren't what broke today.

## Acceptance Criteria

- [ ] `ingest_skill` with a real prereq that exists returns
      `missing_prerequisites: []` on the **first** try, 100/100 runs, in a
      freshly-started cluster.
- [ ] When `entity_find_phonetic` fails with a simulated error (mock
      storage), `ingest_skill` returns an `InternalError` with the
      underlying message — not `missing_prerequisites: [name]`.
- [ ] Unit test in `skill::tests` using the MockStorage error path covers
      the above.
- [ ] Grep of `skill.rs` line 191 and 342 no longer shows
      `.unwrap_or_default()`.

## Related

- `Fail-loud never fake` project rule: the `.unwrap_or_default()` pattern
  is the classic form of this violation.
- `specs/implemented/bug-ingest-skill-silently-drops-unknown-fields.md` —
  same file, same failure mode class (silent drops on malformed input).
  This one is silent drops on transient storage errors.
- `specs/implemented/bug-ensure-parent-tag-graph-label-missing.md` — the
  third skill-ingest-path bug this week. If more surface, consider a
  broader audit of `skill.rs` error handling.

## Implementation Notes

Sweep covered `skill.rs`, `http.rs`, `dispatch.rs`, `storage.rs` (MockStorage).

`skill.rs`: all 9 storage-lookup `unwrap_or_default` / `unwrap_or(None)` sites
replaced. Ingest + verify + ensure_parent_tag + retrieve paths propagate
errors via `?`. `similar_skill_names` (hint-only, non-load-bearing) logs
and returns empty instead of silent drop.

`http.rs` (viz): `derived_cache_get`, `entity_get_batch`, `edge_list_all`
log and continue with empty on Err. `/viz/api/enrich/models` proxy: fixed
`Client::build().unwrap_or_default()` (was dropping the 5s timeout) and
`resp.text().await.unwrap_or_default()` (was returning silent empty body).

`dispatch.rs` `related_entities`: graph→CQL fallback is designed, now
logs `warn!` on graph error before falling through.

`storage.rs` MockStorage: added `force_phonetic_error: Mutex<Option<String>>`
to let tests inject storage errors.

Test added: `skill::tests::ingest_skill_propagates_prereq_lookup_error`
asserts prereq lookup error becomes `Err(_)`, not `missing_prerequisites`.

597/597 lib tests pass.

Out of scope (flagged for follow-up): ~40 `row.r_by_name::<T>().unwrap_or_default()`
and ~15 `serde_json::from_str(...).ok()` sites in `cql_storage.rs` — legacy-row
schema drift path, separate refactor.

Acceptance criteria: all four checked — prereq round-trip clean on first try,
simulated error surfaces as Err, regression test added, `unwrap_or_default`
gone from the two original lines and the other 7 in skill.rs.
