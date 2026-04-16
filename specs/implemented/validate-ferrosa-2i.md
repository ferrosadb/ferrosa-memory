---
type: chore
priority: P2
reported-by: user
implemented-by: ""
verified-by: ""
created: 2026-04-16
updated: 2026-04-16
source: skills-layer-design Sprint 2 prereq
source-location: "specs/skills-layer-design.md#skill-name-lookup--secondary-index-with-ferrosa-2i-validation"
---

# Validate Ferrosa CQL secondary index (2i) correctness before relying on it

## Motivation

Sprint 2 of the skills layer requires O(1) exact lookup by skill name: `invoke_skill("tdd")`. The plan is a CQL secondary index on `(tenant_id, entity_type, entity_name)`. Before building against that, prove Ferrosa's 2i implementation is correct for our access patterns.

Per project policy (CLAUDE.md): "No workarounds for Ferrosa bugs — fix DB bugs upstream." If 2i misbehaves, file a bug in `../ferrosa/specs/` and fix it there rather than changing the schema.

## Validation suite

One integration test crate or binary under `ferrosa-memory/tests/ferrosa_2i/`. Each case below becomes a dedicated test.

### C1 — Index visibility after write

1. Create table with 2i on `entity_name`.
2. Insert row with `entity_name = "tdd"`.
3. Immediately query by index: `SELECT * FROM table WHERE entity_name = 'tdd'`.
4. Assert the row is returned.

Expectation: consistent reads. Any delay reveals index lag — file upstream.

### C2 — Concurrent writers

1. Spawn N=16 concurrent writers, each inserting rows with unique `entity_name`.
2. After all writes complete, query the index for each name.
3. Assert every name resolves to exactly one row.

Expectation: no dropped or duplicated index entries under concurrency.

### C3 — Update via index

1. Insert row `(id=1, entity_name="tdd", version=1)`.
2. Update to `(id=1, entity_name="tdd-v2", version=2)`.
3. Query index for `entity_name="tdd"` → expect empty.
4. Query index for `entity_name="tdd-v2"` → expect the row.

Expectation: index reflects updates, old entries are purged.

### C4 — Restart durability

1. Insert 100 rows with distinct `entity_name`.
2. Restart the Ferrosa cluster.
3. Query each name via the index.
4. Assert all 100 resolve correctly.

Expectation: index survives restart without rebuild or data loss.

### C5 — Compaction safety

1. Insert 100 rows, delete 50, update 25.
2. Force compaction.
3. Re-run queries on every remaining name.
4. Assert results match the post-mutation state.

Expectation: compaction preserves index correctness.

### C6 — Performance at scale

1. Populate table with 100k rows.
2. Measure p50/p99 lookup latency via the index vs a full partition scan.
3. Index should be O(1)-ish: <5ms p99 regardless of row count.

Expectation: acceptable latency (<50ms p99 at 100k rows). If not, it's not a real index.

## Deliverables

- Integration tests under `ferrosa-memory/tests/ferrosa_2i/`.
- Results summary document written to `specs/reports/ferrosa-2i-validation.md`: pass/fail per case, observed latency numbers, any anomalies.
- If any case fails, file an upstream bug at `../ferrosa/specs/bug-2i-<symptom>.md`.

## Blocker for

- Sprint 2 of `specs/skills-layer-design.md` (skill name lookup).
- Any other fmem work that wants 2i: denormalized tag filter column queries, enrich batch lookups, etc.

## Out of scope

- Fixing bugs in Ferrosa 2i (that's upstream work).
- Building an alternative lookup (e.g. a manual index table) — decided only after validation results are in.

## Implementation Notes

_To be filled in by implementer._
