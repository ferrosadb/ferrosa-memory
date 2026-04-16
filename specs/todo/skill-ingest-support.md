# feat: support tools for `frg fmem-skill-ingest`

**Status:** todo
**Consumer:** forge (`research/tools/forge/specs/fmem-skill-ingest/`)
**Created:** 2026-04-16
**Driving need:** forge's bulk skill-catalog seeder needs (a) idempotent tag-hierarchy edge seeding and (b) post-ingest verification that all expected edges are present.

## Goal

Add two MCP tools to ferrosa-memory that close the gaps surfaced when blueprinting forge's skill ingest command:

1. `ensure_parent_tag(child, parent)` — idempotent PARENT_TAG edge creation by tag name.
2. `verify_skill(name)` — return the full graph neighborhood of a named skill so a caller can confirm ingest landed all expected edges.

Without these, forge has to glue together `retrieve_entities` (phonetic, then client-side exact-match filter) + `create_edge` for every PARENT_TAG, and has no way to verify TAGGED_AS / REQUIRES edges short of a custom CQL query.

## Tool 1: `ensure_parent_tag`

### Purpose

forge ingests a `tag-hierarchy.yaml` (e.g. `tdd PARENT_TAG testing`, `testing PARENT_TAG quality`) before walking SKILL.md files. For each declared edge, forge needs to: resolve both tag entities by name, create the PARENT_TAG edge if missing, no-op if present. Doing this with the existing tool surface requires three round-trips per edge plus client-side exact-match filtering of phonetic results.

### Input

```json
{
  "session_id": "...",
  "child_tag": "tdd",
  "parent_tag": "testing"
}
```

### Behavior

1. Normalize both names via the existing `normalize_tag` (lowercase, dash-separated).
2. Resolve `child` and `parent` entities by exact `entity_name` match with `entity_type="tag"`. If either is missing, create it via the same `ensure_tag_entity` path that `ingest_skill` uses (entity_type="tag", scope=Global).
3. Check for an existing PARENT_TAG edge between them. If present → no-op. If absent → create.
4. The DAG cycle prevention added in Sprint 2d (commit 00de792) already rejects cycles at edge creation; surface that error verbatim.

### Output

```json
{ "action": "Created" | "Skipped", "child_id": "...", "parent_id": "..." }
```

### Why not just `create_edge`?

`create_edge` requires entity UUIDs. The caller (forge) has only names. The current `retrieve_entities(strategy=phonetic)` returns ranked phonetic matches without an exact-name filter, forcing the caller to scan results and verify. `ensure_parent_tag` is the right abstraction layer for tag-hierarchy seeding because tags are name-keyed in practice.

## Tool 2: `verify_skill`

### Purpose

After `frg fmem-skill-ingest` finishes its first pass, it needs to confirm every skill landed correctly: TAGGED_AS edges to its category and additional tags, REQUIRES edges to its prerequisites, and (for prerequisite chains) the inverse edges from skills that require this one. The current `invoke_skill` returns only `{description, steps, first_step_prompt, completion_criteria, output_artifacts}` — none of which surface the graph neighborhood.

### Input

```json
{
  "session_id": "...",
  "skill_name": "tdd"
}
```

### Output

```json
{
  "exists": true,
  "entity_id": "...",
  "version": "2026041601",
  "content_hash": "sha256:...",
  "tags": ["task-level", "testing"],            // names, normalized
  "prerequisites": ["unit-testing"],             // skill names this REQUIRES
  "required_by": ["bdd"],                        // skills that REQUIRE this one
  "missing_prerequisites": []                    // names declared in ingest_skill that don't yet exist
}
```

### Behavior

1. Look up the skill by `entity_name + entity_type="skill"`.
2. Enumerate outgoing TAGGED_AS edges → tag names.
3. Enumerate outgoing REQUIRES edges → target skill names.
4. Enumerate incoming REQUIRES edges → source skill names → `required_by`.
5. Cross-check the skill's stored `prerequisites` property (or its ingest record) against the resolved REQUIRES edges. Any prereq name in the property but missing from the edge set → `missing_prerequisites`.

If `exists=false`, return `{exists: false}` and `INVALID_PARAMS` is *not* an error — verification expects to see negative results too.

### Why not extend `invoke_skill`?

`invoke_skill` is documented as the *runtime* entry point for an LLM to start executing a skill. Adding graph neighborhood data to its response would bloat that surface for every caller. `verify_skill` is a separate concern — administrative read used by ingest pipelines and audits — and benefits from a separate tool definition so the LLM-facing description stays focused.

## Acceptance criteria

- [ ] `ensure_parent_tag` creates the edge on first call, returns `Skipped` on second call with same args.
- [ ] `ensure_parent_tag` rejects a cycle attempt with the existing DAG cycle error (no special-casing needed).
- [ ] `ensure_parent_tag` handles the case where one or both tags don't exist by creating them via the same path `ingest_skill` uses.
- [ ] `verify_skill` returns the full edge neighborhood for a skill ingested with `tags=["a","b"]` and `prerequisites=["x"]`.
- [ ] `verify_skill` reports `missing_prerequisites: ["x"]` when skill A declares `prerequisites: ["x"]` but skill x has not been ingested yet — matching the silent-skip behavior `ingest_skill` already has for missing prereqs.
- [ ] `verify_skill` returns `{exists: false}` cleanly for unknown names (no `INVALID_PARAMS`).
- [ ] Both tools use the existing `normalize_tag` for name comparison.

## Dependencies

- Sprint 2 (skills layer) — already shipped.
- Sprint 2d (DAG cycle prevention) — already shipped (commit 00de792).
- No schema migrations.

## Out of scope

- Generic name-keyed entity lookup (`get_entity_by_name`). `verify_skill` and `ensure_parent_tag` are scoped to specific entity types; a generic lookup is a separate work item if a future caller needs it.
- Tag-deletion / hierarchy-rebuild — this spec only covers seeding and verification, not teardown.
- Bulk variants (`ensure_parent_tags(edges)`, `verify_skills([names])`). forge will issue these in a small loop; if profiling later shows N×RTT is the bottleneck, add bulk variants then.

## Estimated effort

- `ensure_parent_tag`: ~3h (handler + DAG-cycle-error pass-through + 3 unit tests).
- `verify_skill`: ~5h (handler + 4 edge-enumeration paths + missing-prereq cross-check + 4 unit tests).
- MCP wiring + tool definitions: ~1h.
- **Total: ~1 day.**

## References

- forge consumer: `../../tools/forge/specs/fmem-skill-ingest/` (compiled-plan.md task packets P31–P35 reference these tools).
- Existing `ensure_tag_entity` in `crates/ferrosa-memory-core/src/skill.rs:687`.
- Existing `create_edge` and DAG cycle handling in `crates/ferrosa-memory-core/src/dispatch.rs`.
