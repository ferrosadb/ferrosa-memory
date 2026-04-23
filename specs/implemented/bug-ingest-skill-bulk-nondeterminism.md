---
type: bug
priority: P2
status: implemented
created: 2026-04-18
updated: 2026-04-20
reported-by: forge fmem-skill-ingest bulk run against research/skills (2026-04-18)
---

# `ingest_skill` bulk ingest: non-deterministic verification + missing TAGGED_AS edges

## Observed

Running `frg fmem-skill-ingest --root skills` against the research repo (79
skills) produces:

- **Run 1:** 75 created, 4 failed (client-side — paths escaping skill dir;
  unrelated to this bug), 60 verified, 19 verification failures.
- **Run 2 (immediately after):** same 75 created, same 4 failed, **38**
  verified, **41** verification failures.
- **Per-skill filter re-run:** every one of the 38+ Run-2 failures verifies
  cleanly when ingested individually via `--filter <name>`.

Two distinct verification-failure classes appeared:

### Class A — "expected tags but fmem has []"

`verify_skill` returns the skill entity but `tags` is empty. Affected
skills on Run 2 included: `commit-it`, `op-init`, `test-gen`,
`try-backend`, `try-mcp-with-inspector`, `azure`, `cloud-architecture`,
`cypher`, `elixir`, `gpu-compute`, `knowledge-graph`, `property-testing`,
`semver-api`, `typescript`, `complexity-audit`, `database-consistency-audit`,
`estimate`, `graph-query-optimization`.

Per `ingest_skill` spec (see
`specs/skills-layer-design.md` Finding 5), the tool is supposed to auto-
create tag entities + `TAGGED_AS` edges from the `category` field and
the `tags:` list. The skill entity exists, but the edges are missing.

### Class B — "skill not in fmem"

`verify_skill` cannot find the skill at all, despite Run 1 reporting it
as `Created`. Affected skills on Run 2 included: `blueprint`,
`cloud-architect`, `cloud-audit`, `compile-project`, `data-analysis`,
`dsm-analysis`, `graph-create`, `new-project`, `secure-review`, `semver`,
`tdd`, `threat-model`, `try-it`, `aws`, `ci-cd`, `csharp`, `docker-dev`,
`go`, `java`, `product-marketing`, `rust`, `terraform`.

Each of these ingests + verifies cleanly under `--filter` in isolation.

## Why it matters

`ingest_skill` is the write path for the skill catalog. If bulk ingest
produces non-deterministic state (skills present in run 1 invisible in
run 2), then:

1. `retrieve_skills_for_context` can't be trusted to surface the correct
   subset of skills between ingestion runs.
2. The verification-gate exit code (`exit 4`) fires spuriously, making
   CI noisy.
3. forge's `fmem-skill-ingest` spec requires verification to be a hard
   exit gate (see `research/tools/forge/specs/fmem-skill-ingest/overview.md`
   section "Locked design choices"). A flaky server-side verification
   undermines that contract.

## Hypothesis

The two classes suggest the same underlying issue from different angles:

- **Class A (missing TAGGED_AS):** the edge-creation phase inside
  `ingest_skill` races the entity-creation phase. Subsequent
  `verify_skill` reads see the entity but not yet the edges.
- **Class B (skill not in fmem):** the entity-write itself is eventually
  consistent — under bulk load, a later `verify_skill` against the same
  session reads a pre-write snapshot. Possibly related to CQL read-
  consistency settings or to session routing across ferrosa replicas.

Both points at **bulk ingestion not being read-your-writes consistent**
within a single MCP session.

Possibly related: `bug-content-hash-clobbered-by-partial-entity-updates.md`
(concurrent writes racing on the same entity). If two paths write the same
skill back-to-back, one write may observe only the older value.

## Reproduction

```bash
# From research repo, with fmem MCP server binary available:
frg fmem-skill-ingest --root skills \
  --server '/path/to/ferrosa-memory-mcp' 2>&1 | tail -30

# Immediately re-run:
frg fmem-skill-ingest --root skills \
  --server '/path/to/ferrosa-memory-mcp' 2>&1 | tail -30

# Pick any verification-failed skill from run 2 and re-ingest in isolation:
frg fmem-skill-ingest --root skills --filter <failing-skill> \
  --server '/path/to/ferrosa-memory-mcp'
# → expect created=1 failed=0 verified=1
```

## Proposed investigation directions

- Instrument `ingest_skill` to log the entity-write + edge-write + verify
  timings, and join with the CQL write timestamps.
- Check whether `verify_skill` reads use the same session/consistency
  level as `ingest_skill` writes (`LOCAL_QUORUM`? `ONE`?).
- Consider adding a server-side "bulk" mode that flushes pending writes
  before returning `Created`, so caller-side verification is
  read-your-writes consistent.
- Add a transactional boundary around entity + `TAGGED_AS` edges so
  the pair is atomic from `verify_skill`'s perspective.

## Evidence

Raw JSON outputs from the two bulk runs are available on request; they
show identical `skills.created` counts but divergent
`verification_failures` lists.

## Implementation Notes

Root cause: `ingest_skill` and `verify_skill`/`get_skill_by_name` used
`entity_find_phonetic` (full-partition `ALLOW FILTERING` scan) as the
by-name lookup. Under bulk load that scan returned a stale view, so
re-ingests allocated fresh `entity_id`s (Class B) and concurrent
`verify_skill` reads resolved to a sibling row without edges
(Class A).

Fix: new `Storage::entity_find_by_exact_name(session_id, name, entity_type)`
that takes `(entity_name, entity_type)` as the idempotency key, backed
by a `WHERE entity_name = ? AND entity_type = ?` query against the
existing `idx_entity_name_phonetic` 2i. `ingest_skill` (self + prereq
lookups) and `get_skill_by_name` now route through it. Phonetic scan
is retained only for `similar_skill_names` (did_you_mean hints).

Coverage added:

- `storage::mock::tests::entity_find_by_exact_name_{returns_hit,returns_none_on_miss,filters_by_entity_type,ignores_substring_matches}`
- `skill::tests::ingest_skill_is_idempotent_without_phonetic_lookup`
  — forces `entity_find_phonetic` to error; ingest must still return
  `Skipped` on unchanged `content_hash`, proving the idempotency path
  is independent of the fuzzy scan.
- Existing `ingest_skill_propagates_prereq_lookup_error` reworked
  to arm `force_exact_name_error` now that prereq lookup is on the
  exact-name path.

Files changed: `crates/ferrosa-memory-core/src/storage.rs`,
`crates/ferrosa-memory-core/src/cql_storage.rs`,
`crates/ferrosa-memory-core/src/skill.rs`,
`crates/ferrosa-memory-mcp/src/main.rs`. Workspace tests green (602
core + 7 bins); clippy clean modulo one pre-existing unrelated
warning.
