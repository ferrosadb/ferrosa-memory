---
type: bug
priority: P2
status: implemented
created: 2026-04-19
updated: 2026-04-20
reported-by: frg fmem-skill-ingest bulk run against research/skills, post-crosstalk-fix (2026-04-19)
related: [specs/implemented/bug-ingest-skill-bulk-nondeterminism.md]
---

# `ingest_skill` bulk run: frontmatter cluster tags silently dropped

## Observed

With both prior ingest bugs marked implemented today
(`bug-ingest-skill-bulk-nondeterminism.md`, `bug-ingest-skill-tag-crosstalk.md`),
re-running the full bulk ingest now produces a **different**
failure mode: the *partial-write* shape from the first bug has
returned, even though the crosstalk shape from the second is gone.

Post-crosstalk-fix run metrics:

- 79 skills (0 created, 14 updated, 65 skipped-unchanged, 0 failed)
- 17 taxonomy edges created
- **73/79 verified** (was 76/79 in the crosstalk-fix run)
- 6 verification failures, **all of the same shape**:
  "expected `[<cluster>, <category>]` but fmem has `[<category>]`"

```
cloud-audit:                expected ["analysis", "cloud", "task-level"]  got ["analysis", "task-level"]    (dropped "cloud")
code-audit:                 expected ["analysis", "task-level"]           got ["task-level"]                (dropped "analysis")
database-consistency-audit: expected ["analysis", "task-level"]           got ["task-level"]                (dropped "analysis")
dsm-analysis:               expected ["analysis", "task-level"]           got ["task-level"]                (dropped "analysis")
fmea:                       expected ["analysis", "task-level"]           got ["task-level"]                (dropped "analysis")
azure:                      expected ["cloud-infra", "tech"]              got ["tech"]                      (dropped "cloud-infra")
```

The auto-derived category tag (from the skill's parent dir) is always
preserved. The frontmatter-declared cluster tag (from `tags: [...]`)
is what drops.

## Why this matters

`retrieve_skills_for_context` relies on cluster tags for grouping.
Dropping `analysis` on 5 of the 6 analysis skills means a request
for `tag=analysis` will find only the 1 skill that survived the
race, not all 6. Retrieval quality degrades silently — no error
surfaces at write time.

## Hypothesis (ordered by likelihood)

### (1) Cluster-tag cache/create race keyed on tag name

Five of six dropped-tag failures share the `analysis` tag. These five
skills are ingested adjacent-in-time under the same `task-level/` walk
and all declare `tags: [analysis]`. The pattern strongly suggests a
race on the tag-entity create: the first ingest wins the create of
the `analysis` tag entity; the next four see a cache or existence
check that says "tag exists" and skip both the create *and* the
TAGGED_AS edge.

Check:
- Is the "create tag if missing" path idempotent with respect to
  the TAGGED_AS edge write, or does a cache hit cause the edge
  write to be skipped?
- Does the server read an "already have this tag" flag from a
  stale local cache populated by an in-flight peer request?

### (2) Crosstalk fix over-corrected

The crosstalk fix may have added a guard like "skip the TAGGED_AS
write when the in-flight skill id doesn't match the expected one."
Correct for wrong-target writes, but may now over-skip for
right-target writes when the skill-id cell races benignly on the
frontmatter-tag write path (but not on the category-tag write
path — which explains why category tags are never dropped).

### (3) Frontmatter-tag writes are deferred and lost

If frontmatter tags are batched separately from the category tag
and written in a second phase, that second phase may drop on error,
timeout, or reorder. Category tags — written inline with the skill
create — survive. Frontmatter tags — written in a post-create loop —
don't.

## Investigation starters

```bash
# Trace every tag resolve + TAGGED_AS write:
RUST_LOG=ferrosa_memory_mcp::skill::ingest=trace,\
ferrosa_memory_core::tags=trace,\
ferrosa_memory_core::tag_cache=trace \
FERROSA_MEMORY_CONFIG=./examples/ferrosa-memory.toml \
./target/release/ferrosa-memory-mcp 2> /tmp/fmem-trace.log &

# Run bulk ingest, then grep for analysis-tag writes:
frg fmem-skill-ingest --root skills \
  --server './target/release/ferrosa-memory-mcp'

grep -E "tag=analysis|TAGGED_AS.*analysis" /tmp/fmem-trace.log | head -40
```

Expected pattern: `code-audit` creates the `analysis` tag, then the
next 4 ingests either skip the create (fine) and also skip the edge
(bug) — or fire the edge against a stale `analysis` tag id and have
it dropped on write.

## Reproduction

```bash
# Reset fmem state (drop the keyspace or truncate the entity_store)
# and run cold:
FERROSA_MEMORY_CONFIG=./examples/ferrosa-memory.toml \
CLAUDE_PROJECT_DIR=/Users/bkearns/src/research \
frg fmem-skill-ingest --root skills \
  --server './target/release/ferrosa-memory-mcp'

# Expect: 73-77 of 79 verified. The failures cluster on skills that
# share a cluster tag with another adjacent skill (e.g., the 5-skill
# "analysis" cluster). Per-skill isolation verifies cleanly.
```

Per-skill isolation recovers:

```bash
for s in code-audit dsm-analysis fmea database-consistency-audit cloud-audit azure; do
  FERROSA_MEMORY_CONFIG=./examples/ferrosa-memory.toml \
  CLAUDE_PROJECT_DIR=/Users/bkearns/src/research \
  frg fmem-skill-ingest --root skills --filter $s --force \
    --server './target/release/ferrosa-memory-mcp'
done
# → each reports created=1 failed=0 verified=1
```

So the race is strictly concurrent-ingest-vs-same-cluster-tag.

## Related

- `specs/implemented/bug-ingest-skill-bulk-nondeterminism.md` —
  the original dropped-tag bug. This bug is a **regression of the
  same failure shape**. Start by diffing the handler between the fix
  commit and HEAD — either the fix was reverted or a later change
  opened a new window.
- `specs/implemented/bug-ingest-skill-tag-crosstalk.md` — the fix
  for this one may be the change that regressed the dropped-tag
  guarantee. Check whether the new "skip TAGGED_AS on skill-id
  mismatch" guard fires over-eagerly.

## Implementation Notes

Root cause matched hypothesis (1) and (3): `ingest_skill` called
`ensure_tag_entity` and then used the `Ok(tag_id)` / `Err(_)`
return to decide whether to write the TAGGED_AS edge. Under
concurrent bulk ingest against a live CQL cluster, the tag-entity
upsert can transiently fail (lane reconnect, write timeout) even
when a sibling ingest targeting the same row succeeds. The old
code treated the transient error as "give up on this tag" and
skipped the edge — and because the cluster tag was shared across
many skills while the category tag was not, the shared cluster
tag was disproportionately dropped.

Fix: decouple the edge write from the tag upsert's success. Tag
entity_ids are already deterministic
(`scope::tenant_tag_entity_uuid(tenant, tag_name)`, from the
previous bug fix), so `ingest_skill` can compute the edge's
`dst_id` independently and write the edge unconditionally. The
upsert remains best-effort — if one writer fails, another
concurrent (or later) ingest writes the row and resolves the
edge. Pre-fix we'd lose the edge; post-fix we at worst have a
brief window where the edge points at a not-yet-materialized tag,
which `verify_skill` handles by silently skipping dangling edges.

Coverage:

- `skill::tests::concurrent_ingest_sharing_a_cluster_tag_every_skill_gets_the_edge`
  — 5 concurrent `ingest_skill` calls all declaring `"analysis"`
  in their `tags`. Each skill must end with exactly one TAGGED_AS
  edge pointing at the deterministic analysis-tag UUID. Regression
  guard for acceptance #3.
- `skill::tests::tagged_as_edge_persists_when_tag_entity_upsert_fails`
  — forces every `entity_type == "tag"` upsert to fail via a new
  `force_entity_put_error` knob on `MockStorage`, then asserts the
  TAGGED_AS edge still lands against the deterministic tag id.
  Drove the decoupling fix (test was RED; passes after fix).

Files changed: `crates/ferrosa-memory-core/src/skill.rs` (tag
edge loop), `crates/ferrosa-memory-core/src/storage.rs`
(`MockStorage::force_entity_put_error` test hook).

Live acceptance — `79/79 verified` across 3 runs of
`frg fmem-skill-ingest --root skills` — is left for the verifier.

## Acceptance

- Bulk run of `frg fmem-skill-ingest --root skills` reaches
  **79/79 verified** on a cold ferrosa cluster, deterministically
  across 3 consecutive runs.
- No TAGGED_AS edge is silently dropped. Every tag declared in a
  SKILL.md's `category` or frontmatter `tags:` results in a
  persisted TAGGED_AS edge by the time `ingest_skill` returns.
- Regression test: ingest 5 skills that all declare the same
  cluster tag; verify all 5 have that tag after concurrent ingest.
