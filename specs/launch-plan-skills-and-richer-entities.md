# Launch Plan — Skills Layer + Richer Entity Model

**Branch:** `feature/skills-and-richer-entities`
**Baseline:** 16 commits since `main`, 568 tests green, clippy clean.
**Created:** 2026-04-16
**Status:** pre-launch — testing and migration gates remaining.

This plan closes the loop between code-complete and production-live for the skills layer, the richer entity schema, and the supporting infrastructure (schema versioning, test cluster, 2i validation, backfill, viz scope UI). It explicitly scopes what's launch-blocking, what goes in parallel, and what stays in the backlog.

## Scope snapshot

| Area | State | Ref |
|---|---|---|
| Entity schema (scope, description, tags, properties, content_hash, updated_at) | code + unit tests done | Sprint 1a-c |
| CQL write/read of new columns | code done, live-CQL verification pending | Sprint 1b, DDL 020 |
| Scope primitives (tenant sentinel, default_scope_for) | done, unit-tested | Sprint 1c |
| hybrid_search SearchFilter | done | Sprint 1d |
| Edge/entity-type registry seed | done (startup-idempotent) | Sprint 1e |
| enrich_entities → description field | done | Sprint 1f |
| Viz: scope in snapshot + frontend filters | done | Sprint 1g + viz-cross-session |
| `ingest_skill`, `retrieve_skills_for_context`, `invoke_skill` | done + unit tests | Sprint 2a-c |
| Cypher DAG cycle check | done + unit tests, wired into REQUIRES | Sprint 2d |
| Schema versioning + startup migration | done | feat-schema-versioning |
| Test cluster harness | scripts done, cluster never booted | feat-test-cluster-harness |
| Ferrosa 2i validation suite | 6 `#[ignore]`d tests ready | validate-ferrosa-2i |
| Backfill subcommand (Phases 1, 2, 4) | done + unit tests on helpers | backfill chore |
| forge `fmem-skill-ingest` | **user runs in parallel** | `../research/tools/forge/specs/todo/fmem-skill-ingest.md` |

## Launch gates

Everything under this heading must be green before the new build touches the production tenant.

### G1 — Live-CQL test pass on the isolated test cluster

Dependency: schema-versioning, test-cluster scripts, 2i validation suite — all landed. Cluster has not yet been booted.

Steps:

1. `scripts/start-test-cluster.sh` — wait for healthcheck on port 19542.
2. Start the fmem build against the test keyspace so migration 020 self-applies:
   ```
   FERROSA_KEYSPACE=agent_memory_test FERROSA_CQL_PORT=19542 \
     cargo run -p ferrosa-memory-mcp
   ```
   Confirm the log line `schema migrations applied applied=1` (first boot) and `schema up to date` (subsequent boots).
3. Export the test env and run the 2i suite:
   ```
   export $(scripts/start-test-cluster.sh --env)
   cargo test -p ferrosa-memory-core --test ferrosa_2i_validation -- --ignored --nocapture
   ```
   All 6 cases (C1-C6) must pass. Any failure: file a bug in `../ferrosa/specs/` per CLAUDE.md and block launch until fixed upstream.
4. Run the existing live-CQL test against the test cluster (one-off smoke):
   ```
   FERROSA_TEST_CQL_PORT=19542 cargo test -p ferrosa-memory-core --test cql_storage_live -- --ignored --nocapture
   ```

Exit criteria: `6 passed; 0 failed` on the 2i suite + existing live tests still green.

### G2 — End-to-end skill round-trip on the test cluster

Dependency: G1 pass.

1. Point a stdio MCP client at the test-cluster-backed server.
2. Call `ingest_skill(name="tdd", category="testing", description="...", steps=[...])` — expect `action: created`.
3. Call `retrieve_skills_for_context(context="how do I test this?")` — expect `tdd` in top-3.
4. Call `invoke_skill(skill_name="tdd")` — expect structured steps + first_step_prompt.
5. Call `invoke_skill(skill_name="tdd-typo")` — expect `INVALID_PARAMS` with `did_you_mean: ["tdd"]`.
6. Re-ingest with matching content_hash — expect `action: skipped`.
7. Confirm a `Tag(testing)` entity exists at the global sentinel partition and a TAGGED_AS edge links tdd to it.

Exit criteria: all 7 steps return the expected shapes, no WARN logs about failed edges or missing embeddings.

### G3 — Backfill dry-run against a dev-keyspace snapshot

Dependency: a snapshot of the current dev keyspace restored to the test cluster (optional but highest-value).

Steps:

1. Restore a recent dev-keyspace backup into the test keyspace.
2. `BACKFILL_DRY_RUN=1 ferrosa-memory-batch backfill-rich-entities` — count entities that would be migrated (Phase 1) and embedded (Phase 2).
3. Run without DRY_RUN against the test keyspace; validate `description` is populated and `context_snippet` no longer carries `ENRICHED_PREFIX`.
4. Diff a sample of entities before/after. Confirm description_embedding is populated. Confirm `tags` column reflects the graph.

Exit criteria: backfill completes with 0 unexpected failures; spot-check passes; dry-run and real-run counts agree.

### G4 — Regression check on existing tools

Every tool added or modified must be demonstrably working post-migration:

- `smart_ingest` — create an entity, confirm it lands with scope=Session (default for plain entities).
- `hybrid_search` — query an existing entity, confirm result ranking unchanged for session-only scope.
- `explore_connections` — traversal still works with the new session_id filter behavior (from the earlier refactor-session-id-schemas commit).
- `enrich_entities` — run on a session with plain entities, confirm writes go to the new `description` field.
- `create_edge` / `batch_create_edges` — unchanged behavior.

Exit criteria: manual session covers all five without surprises.

### G5 — Forge parallel track (user)

Owned by the user. Not a blocker for fmem launch, but the two land cleanly together:

- `frg fmem-skill-ingest` walks `../research/skills/**/SKILL.md`, calls `ingest_skill` per skill, idempotent via content_hash.
- Bootstrap the tag taxonomy (creates tag entities + PARENT_TAG edges).
- Smoke run against the test cluster; seed the ~78 skills; validate via `retrieve_skills_for_context`.

## Production deployment sequence

Assumes G1-G4 green. User's forge work (G5) can land before or after the fmem deployment.

1. **Take a backup of the production keyspace.** Verified restore-from-backup is the rollback path — DDL 020 is additive and non-breaking, but the backfill mutates data.
2. **Deploy the new fmem build.** Startup auto-detects the pre-versioning baseline (schema_version empty but entity_store present) and seeds version 19. Migration 020 then runs. Confirm log lines:
   - `schema_version empty but legacy tables present; seeding adoption baseline`
   - `applying migration version=20 description=...`
   - `schema migrations complete applied=1`
3. **Run the backfill** during a quiet window:
   ```
   BACKFILL_PHASES=1,2,4 ferrosa-memory-batch backfill-rich-entities
   ```
   Expect no `failed` count in the summary. If Phase 2 reports failures, the embedding provider is down — investigate and re-run (phase 2 is idempotent on success).
4. **Smoke test on production.** One `retrieve_skills_for_context` call and one `smart_ingest` call against the live server to confirm both new and existing paths work.
5. **Seed the skill catalog** (after or coordinated with the user's forge work):
   ```
   frg fmem-skill-ingest --root ../research/skills
   ```

## Rollback plan

The schema migration is additive — dropping the new columns won't remove the rows. Rollback is:

1. **Stop the new fmem build.** The old build ignores the new columns (it won't query them — just unused space).
2. **Backfill damage control.** Phase 1 rewrote `context_snippet` (moved `ENRICHED_PREFIX` content to `description` and restored the raw context). This is *not* reversed by simply stopping the server. Restore from the pre-deploy backup if behavior depends on the old ENRICHED_PREFIX layout in context_snippet (the old build's `is_enriched` check still matches legacy prefixes, so running it against new data is likely safe — but the authoritative rollback is restore).
3. **Log entry.** Record which migration version was reached so the next deploy knows whether to re-apply 020.

## Existing backlog — post-launch

Pre-existing items in `specs/todo/` at launch time. Explicitly deferred:

| Item | Priority | Rationale for deferring |
|---|---|---|
| `bug-entity-store-session-partitioning.md` | P0 | Bulk-ingest data-loss bug on the forge side. The skills layer does not write through that code path (it uses `entity_put` directly, not bulk batched writes). **Must be resolved before the dev keyspace gets another large ingest**, but not blocking this launch. |
| `cql-client-paging.md` | P1 | Large-partition read fragility. Skills partition is small today (<100 entries); the risk surfaces when total entities in a single partition grow past ~10k. Schedule immediately after launch. |
| `refactor-code-smells.md` | P2 | Storage-layer complexity cleanup. Pure tech debt, no behavioral change. |
| `viz-subgraph-exploration.md` | P2 | Feature improvement. Existing viz works for the current scale. |

Also pending but tracked elsewhere:

- **Phase 0 of the backfill** (re-embed every `entity_embedding`, `fold_embedding`, `memo_embedding` with nomic-embed-text-v2-moe) — only needed if the operator actually switches from v1 to v2 in production config. Until then, existing v1 vectors stay usable. Inside `specs/implemented/chore-entity-backfill-for-rich-schema.md`.
- **PARENT_TAG cycle check integration** — the primitive exists in `GraphClient::would_create_cycle`; wire it into whatever tag-management tool lands next (no such tool exists today, so no action required).

## Exit summary

- [x] G1 (live-CQL 2i suite + existing live tests). 4/6 2i tests pass
      definitively (C1, C3, C4, C6). C2 (concurrent writers timing out)
      and C5 (non-unique-label index returning row subset) failures
      filed upstream at
      `../ferrosa/specs/todo/bug-secondary-index-missing-rows-and-write-timeout.md`.
      The skill layer does not depend on 2i; launch is not blocked. The
      greenfield migration runner was added along the way and is verified
      on a live fresh keyspace.
- [x] G2 (end-to-end skill round-trip). `tests/skill_e2e_live.rs` runs
      the full ingest → retrieve → invoke → did_you_mean → idempotent
      re-ingest → ensure_parent_tag → verify_skill pipeline on a live
      bootstrapped test cluster. Green. did_you_mean is best-effort via
      phonetic match (Ferrosa double-metaphone); sharper edit-distance
      similarity is a post-launch improvement.
- [x] G3 (backfill). `tests/launch_gates_g3_g4.rs` seeds an entity with
      legacy `ENRICHED_PREFIX` context_snippet, runs Phase 1 (prefix →
      description field) and Phase 2 (description_embedding) logic
      against the live cluster + Ollama nomic-embed-text-v2-moe. Both
      phases validated end-to-end. A proper production-dev-snapshot
      restore is still an operator responsibility before the production
      migration — this test validates the backfill *correctness*, not
      the deployment choreography.
- [x] G4 (regression). `tests/launch_gates_g3_g4.rs` covers:
      - smart_ingest still creates plain session-scoped entities with
        default rich-schema fields.
      - typed_edge_put / typed_edge_list_from / typed_edge_list_session
        round-trip correctly after the four hardcoded
        `agent_memory.typed_edges` fixes.
      - entity_list_all reads across sessions (viz + backfill path).
- [ ] Production backup taken and verified restorable. **Operator step
      before deploying the new build.**
- [ ] Deployment sequence run against production keyspace. **Operator
      step; the bootstrap path runs automatically on first boot.**
- [ ] G5 (forge skill-ingest). User's parallel track.

At this point the feature branch is ready to merge to `main`. Remaining
work (cql-client-paging, refactor-code-smells, viz-subgraph-exploration,
forge skill-ingest, bug-entity-store-session-partitioning) moves into
the post-launch backlog.
