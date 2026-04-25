---
type: bug
priority: P2
status: implemented
created: 2026-04-19
updated: 2026-04-20
reported-by: frg fmem-skill-ingest bulk run against research/skills (2026-04-19)
---

# `ingest_skill` bulk run: TAGGED_AS edges attach to wrong skill entities

## Observed

Running `frg fmem-skill-ingest --root skills` against the full research
skill catalog (79 skills, 17 taxonomy edges) now completes the pipeline
— 72 created, 7 updated, 0 failed, 16 taxonomy edges created,
**76/79 verified** — but the 3 verification failures show a new
failure shape that was not visible before the earlier non-determinism
fix.

Earlier symptom (pre-fix — original
`bug-ingest-skill-bulk-nondeterminism.md`, now in `implemented/`):
> `expected tags ["X", "Y"] but fmem has ["X"]`
(partial write — one tag dropped)

New symptom (this bug):
> `expected tags ["X", "Y"] but fmem has ["Z", "Y"]`
(wrong-target write — tag Z is a tag that belongs to a *different*
skill in the same batch)

### Evidence — three skills, three symptoms, two of them obviously swapped

```
verify fail: cloud-architect:
  expected ["cloud", "task-level"]
  fmem has ["task-level", "tech", "tooling"]

verify fail: compile-project:
  expected ["architecture", "task-level"]
  fmem has ["analysis", "task-level"]

verify fail: complexity-audit:
  expected ["analysis", "task-level"]
  fmem has ["architecture", "task-level"]
```

`compile-project` and `complexity-audit` **swapped tags**:
`complexity-audit` ended up with `architecture` (the correct tag for
`compile-project`) and `compile-project` ended up with `analysis`
(the correct tag for `complexity-audit`). The two skills are adjacent
in the ingest ordering and both in the `task-level` category —
plausibly ingested on adjacent or overlapping worker goroutines.

`cloud-architect` (correct: `[cloud]`) picked up `tech` and `tooling`,
which are tags that belong to tech-tree skills ingested in the same
batch. It kept `task-level` (its correct category tag) but lost
`cloud` and gained two unrelated tags.

## Why it matters

`retrieve_skills_for_context` surfaces skills to the LLM by matching
requested tags. If tags are attached to the wrong skill:

1. A query for `analysis` will surface `compile-project` (wrong skill)
   and miss `complexity-audit` (correct skill).
2. A query for `architecture` will do the inverse.
3. A query for `cloud-infra` / `tooling` / `tech` may surface
   `cloud-architect`, pulling a task-level skill into a tech-tree
   retrieval.

This is functionally worse than the pre-fix behavior, which would at
least drop a tag (the caller gets fewer results, not wrong results).
The correctness degradation pattern is: **false positives in skill
retrieval** rather than false negatives.

## Hypothesis

Candidates, ordered by likelihood:

1. **TAGGED_AS write uses the wrong `skill_id`** — the handler
   reads a shared mutable "current skill id" cell that races when two
   `ingest_skill` calls execute concurrently. Tags meant for skill A
   are written pointing at skill B.

2. **Tag-resolution cache contamination** — if the server maintains an
   in-memory `(tag_name → tag_entity_id)` cache keyed by a session or
   global state, a concurrent ingest may insert the wrong `tag_entity_id`
   for the in-flight request. Then TAGGED_AS links skill A to tag B's
   entity id while correctly naming the edge as "tag A" on the wire.

3. **Batch interleave inside a single CQL batch** — if `ingest_skill`
   constructs a logged/unlogged CQL batch and two calls share a batch
   builder, partition-key assignment can cross-contaminate.

Ordering signal: `compile-project` and `complexity-audit` are both
under `task-level/` and are neighbors when the catalog is walked in
directory order. `cloud-architect` also lives under `task-level/` and
is ingested near the others. Whatever the mechanism, it affects
adjacent-in-time ingests more than distant ones.

## Reproduction

```bash
# Cold ferrosa cluster + fresh fmem binary:
podman compose -f /Users/bkearns/src/ferrosa-memory/docker-compose.yml up -d
cargo build --release -p ferrosa-memory-mcp

# Run bulk ingest end-to-end:
FERROSA_MEMORY_CONFIG=./examples/ferrosa-memory.toml \
CLAUDE_PROJECT_DIR=/Users/bkearns/src/research \
frg fmem-skill-ingest --root skills \
  --server './target/release/ferrosa-memory-mcp'

# Inspect verification_failures[] in the trailing JSON.
# Expect: 0-5 failures, at least one of which has "fmem has [Z, ...]"
# where Z is a tag that belongs to a different skill in the batch.
```

Re-running changes *which* skills fail (non-deterministic), but the
cross-talk shape recurs.

Per-skill isolation run verifies cleanly — the bug only manifests
under concurrent multi-skill ingest:

```bash
# Each of these returns created=1 failed=0 verified=1 individually:
for s in compile-project complexity-audit cloud-architect; do
  FERROSA_MEMORY_CONFIG=./examples/ferrosa-memory.toml \
  CLAUDE_PROJECT_DIR=/Users/bkearns/src/research \
  frg fmem-skill-ingest --root skills --filter $s \
    --server './target/release/ferrosa-memory-mcp'
done
```

## Investigation starters

```bash
# Log every TAGGED_AS write with (skill_name, skill_id, tag_name, tag_id):
RUST_LOG=ferrosa_memory_mcp::skill::ingest=trace,ferrosa_memory_core::tags=trace \
FERROSA_MEMORY_CONFIG=./examples/ferrosa-memory.toml \
./target/release/ferrosa-memory-mcp 2> /tmp/fmem-trace.log &

# Run the bulk ingest, then grep for entries where
# the (skill_name → skill_id) mapping flips mid-batch,
# or where a tag_id is written for a skill whose name
# doesn't match the tag's expected skill list.
```

Also consider: add a server-side post-ingest invariant check —
"every TAGGED_AS edge must point at a tag whose normalized name
matches the declared `category` or `tags:` list of the source skill."
If that invariant fires in the write path, the offending request
can be logged with full context.

## Implementation Notes

Root cause: `ensure_tag_entity` used a phonetic lookup to resolve a
tag's `entity_id`, falling back to `Uuid::new_v4()` when nothing
matched. Under concurrent bulk ingest the lookup was racy — two
tasks creating the same tag would each mint their own random id,
and a subsequent TAGGED_AS write could end up referencing a tag
entity whose name didn't match what the caller asked for.

Fix: tag `entity_id`s are now deterministic,
`UUIDv5((tenant_id, normalized_tag_name))`, via a new
`scope::tenant_tag_entity_uuid`. `ensure_tag_entity` computes the
id in-process and does a single `entity_put` (upsert) — no lookup,
no race. Every caller for the same tag name agrees on the id, so a
TAGGED_AS write cannot point at the wrong tag.

Coverage:

- `skill::tests::ensure_tag_entity_is_deterministic_across_stores`
  — two fresh `MockStorage` instances must produce the same id for
  the same `(tenant, tag_name)`, and different tenants must not
  collide.
- `skill::tests::concurrent_ingest_of_distinct_skills_does_not_crosslink_tags`
  — `tokio::join!` three `ingest_skill` calls with disjoint tag
  sets (the compile-project / complexity-audit / cloud-architect
  triad from the bug report), then assert every TAGGED_AS edge
  points at a tag entity whose name is in that skill's declared
  category+tags. Encodes the acceptance invariant as a regression
  guard.

Files changed: `crates/ferrosa-memory-core/src/scope.rs` (new
`tenant_tag_entity_uuid` + namespace),
`crates/ferrosa-memory-core/src/skill.rs` (`ensure_tag_entity`
simplified to compute-and-upsert).

Live acceptance (`79/79 verified` across 3 runs of
`frg fmem-skill-ingest --root skills`) is left for the verifier.

## Related

- `specs/implemented/bug-ingest-skill-bulk-nondeterminism.md` —
  prior bug (partial tag writes) fixed in the malloc / init work
  shipped 2026-04-19. This new bug likely escaped detection because
  the post-fix verification failure count dropped from ~25 to ~3 and
  the remaining 3 were assumed to be residual instances of the same
  class. They are a different shape.
- `specs/todo/bug-content-hash-clobbered-by-partial-entity-updates.md`
  — another concurrent-write bug in the entity store path; may share
  a common root cause with this one if the handler reuses a mutable
  context across concurrent requests.

## Acceptance

- Bulk run of `frg fmem-skill-ingest --root skills` reaches
  **79/79 verified** on a cold ferrosa cluster, deterministically
  across 3 consecutive runs.
- No TAGGED_AS edge is ever written where the tag's normalized name
  does not appear in the source skill's declared `category` or `tags`.
- (Optional) Server emits a `WARN`-level log for any write that would
  violate the TAGGED_AS invariant — useful even after the fix, as a
  guard against regressions.
