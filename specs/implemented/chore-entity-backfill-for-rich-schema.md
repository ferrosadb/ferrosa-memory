---
type: chore
priority: P2
reported-by: user
implemented-by: ""
verified-by: ""
created: 2026-04-16
updated: 2026-04-16
source: skills-layer-design session
source-location: "specs/skills-layer-design.md"
---

# Backfill existing entities for the rich entity schema

## Problem

When the rich entity schema lands (Sprint 1 of `specs/skills-layer-design.md`), existing entities default to:

- `description = None`
- `description_embedding = None`
- `tags = []`
- `properties = {}`
- `scope = Session`
- `content_hash = None`

The schema migration is non-breaking — name-embedding retrieval keeps working — but existing entities won't benefit from description-aware ranking, tag filtering, or hierarchical walks until backfilled.

Additionally, the current `enrich_entities` tool writes LLM-generated descriptions into `context_snippet` with an `ENRICHED_PREFIX` sentinel (see `crates/ferrosa-memory-core/src/enrich.rs:151`). After the migration, those descriptions should live in the dedicated `description` field with an accompanying `description_embedding`.

## Scope

One admin tool: `backfill_rich_entities` (MCP tool, not a public one — gated behind admin flag or run via a CLI subcommand in `ferrosa-memory-batch`).

### Phase 0 — Re-embed name vectors with nomic-embed-text-v2-moe

The default embedding model is moving from `nomic-embed-text` (v1, 137M params, 768-dim) to `nomic-embed-text-v2-moe` (v2, 305M params, 768-dim). Dimensions are unchanged so the CQL schema is compatible, but **the vectors themselves are incomparable** — v1 and v2 produce different embeddings for the same text. ANN search mixes vectors from both models would return garbage.

Every existing `entity_embedding` must be regenerated:

1. For each entity in the tenant (across all sessions and scope), call `EmbeddingClient::embed(entity_name)` using v2.
2. Write the new `entity_embedding` and bump `updated_at`.
3. Also re-embed other tables that store embeddings against the same model: `fold_embedding`, `memo_embedding`. Query the codebase for every call to `client.embed(...)` before running this phase to enumerate the write sites.

**Cutover safety:** run the backfill with the v2 model active (server config already updated). If any legacy code path still uses v1, its writes will corrupt the index — audit and fail the backfill if any v1-tagged vectors remain.

Batch with concurrency: ~30-50ms per embed call × thousands of entities ⇒ a few minutes with `N=8` concurrent workers. Failures on individual entities are logged and skipped; backfill exits 1 at the end with a count.

### Phase 1 — Parse existing enrichment into dedicated fields

For every entity where `context_snippet` starts with `ENRICHED_PREFIX`:
1. Parse out the description portion and the original context.
2. Write `description = parsed_description`, `context_snippet = original_context`.
3. Generate `description_embedding` via the embedding client (nomic-embed-text, 768-dim).
4. Update `updated_at = now()`.

### Phase 2 — Generate missing description_embeddings

For every entity where `description.is_some() && description_embedding.is_none()`:
1. Call `EmbeddingClient::embed(description)`.
2. Write `description_embedding`.

### Phase 3 — Global scope promotion (opt-in, manual)

Global scope promotion involves physically moving a row from its session partition to the tenant's global partition (delete + reinsert). Not done as part of the generic backfill — it's destructive-ish and users may want to review.

Provide a separate subcommand `promote_entities_to_global --entity-types skill,tag,concept,decision,pattern,code_symbol [--dry-run]` that:
- Lists all entities of the given types in any session
- Moves each to the global partition (preserving entity_id, edges, audit fields)
- Records `ingested_by_session` from the original session
- Reports counts per type

Re-runnable, idempotent, safe.

### Phase 4 — Content hash backfill (optional)

For entities with `description.is_some()`, compute `content_hash = sha256(name || description || properties_json)` and write it. This enables idempotent re-ingest for future tools.

## Tool interface

### CLI via `ferrosa-memory-batch`

```
ferrosa-memory-batch backfill-rich-entities [OPTIONS]

Options:
  --phase {0,1,2,3,4,all}  Which phase to run (default: 0,1,2,4 — skip promotion)
  --session <UUID>         Scope to one session; default: all sessions for the tenant
  --entity-types <LIST>    Filter: only backfill these types
  --batch-size <N>         Entities per batch (default: 50)
  --dry-run                Report what would change, don't write
  --force                  Re-generate description_embedding even if present
```

Exit 0 on success; 1 on any entity failure (continues through the batch, reports count at end).

### Observability

Summary:
```
Phase 1: 124 entities migrated (ENRICHED_PREFIX → description), 8 failed.
Phase 2: 132 description_embeddings generated.
Phase 4: 132 content_hashes written.
Elapsed: 45.2s. Embedding calls: 132. LLM calls: 0.
```

Per-entity progress at `debug` level; per-phase summary at `info`.

## Acceptance Criteria

- [ ] Dry-run reports counts accurately without writing.
- [ ] Phase 0: after running, every entity's `entity_embedding`, fold's `fold_embedding`, and memo's `memo_embedding` has been re-generated against v2. A test vector (known entity name) produces the same embedding as a fresh v2 call.
- [ ] Phase 0 fails loud if the embedding endpoint becomes unreachable mid-run — no partial-write corruption.
- [ ] Phase 1: after running, every entity whose `context_snippet` previously started with `ENRICHED_PREFIX` now has a populated `description` field and clean `context_snippet`.
- [ ] Phase 2: every entity with `description.is_some()` has `description_embedding.is_some()` after running.
- [ ] Phase 2 handles embedding provider outage gracefully — logs the failure, skips the entity, exits 1.
- [ ] Phase 3 (promotion): a skill entity in session X ends up in the global partition with `ingested_by_session = X` preserved.
- [ ] Phase 3 is idempotent: second run of promotion reports 0 changes.
- [ ] Existing tests pass; new integration test for each phase.

## Dependencies

- Rich entity schema migration (Sprint 1 of `specs/skills-layer-design.md`) must land first — the new columns need to exist.
- Ollama / nomic-embed-text must be reachable for Phases 1-2.

## Out of Scope

- LLM-driven description generation for entities that never had one. `enrich_entities` is the tool for that; this backfill only moves existing descriptions into the new schema and generates embeddings for them.
- Re-embedding name vectors (the existing `entity_embedding` already uses nomic-embed-text — no model switch, no re-embed needed).
- Cross-tenant backfill (each tenant's admin runs for their tenant).

## Implementation Notes

_To be filled in by implementer._
