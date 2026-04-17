# Skills Layer & Richer Entity Model

**Status:** design (pre-implementation)
**Branch:** `feature/skills-and-richer-entities`
**Author:** agent
**Updated:** 2026-04-16

## Purpose

Evolve fmem from a flat entity store into a richer knowledge base that:

1. Stores structured knowledge (skills, code symbols, decisions, patterns) with per-type shape while keeping a uniform core entity record.
2. Retrieves relevant knowledge for the current context using description-aware two-stage retrieval with re-ranking.
3. Actively surfaces skills into the LLM context via a new intention trigger type, balanced against clutter.

Skills are the driving use case; the design generalizes to any structured knowledge (code symbols, decisions, bugs, patterns).

## Design goals

- **Uniform base, typed shape.** One table for entities; per-type structured properties in JSON with ingest-time schema validation.
- **Session-local by default, global where useful.** Entities carry a scope. Most stay session-local (current behavior). Skills and other shared knowledge are global — queryable across every session regardless of caller.
- **Description-aware retrieval.** Every entity can carry a free-text description and description embedding — retrieval ranks on semantic match over descriptions, not just names.
- **Re-ranking over multiple signals.** Combine semantic similarity, PageRank reputation, warmth, recency, keyword overlap, type match, and session affinity.
- **Graph-native relationships.** Prerequisites, related_to, implements, calls stay as typed edges via the existing edge system. Don't duplicate edge-shaped data inside properties JSON.
- **No breaking changes to existing entities.** Current `entity_type`s (person, place, concept, etc.) keep working with empty properties / no description.
- **No cross-MCP coupling.** `invoke_skill` emits structured step data; it does not call forge or any external tool.

## Entity schema changes

### Additions to `EntityEntry` (CQL + Rust)

```rust
pub struct EntityEntry {
    // Existing fields (unchanged):
    tenant_id: Uuid,
    entity_id: Uuid,
    session_id: Uuid,
    entity_name: String,
    entity_type: String,
    source_fold_id: Option<Uuid>,
    context_snippet: String,
    entity_embedding: Vec<f32>,   // name embedding (existing)
    confidence: f32,
    state: MemoryState,
    created_at: DateTime<Utc>,

    // NEW:
    description: Option<String>,               // free-text description (distinct from context_snippet)
    description_embedding: Option<Vec<f32>>,   // embedding of description
    tags: Vec<String>,                          // denormalized tag names (direct + ancestors) for fast filtering — see "Hierarchical tags"
    properties: serde_json::Value,             // type-specific structured data
    content_hash: Option<String>,              // for idempotent re-ingest
    updated_at: DateTime<Utc>,                 // last modification (distinct from created_at)
    scope: EntityScope,                         // Session | Global (see "Scope" section)
    ingested_by_session: Option<Uuid>,          // audit: who ingested this (even for global entities)
}

enum EntityScope {
    /// Session-local: scoped to a specific session_id. Current default.
    Session,
    /// Global: visible across every session for the tenant. Used for skills
    /// and other shared knowledge like code symbols.
    Global,
}
```

### Scope: session-local vs global

The existing entity store is partitioned by `(tenant_id, session_id)`. Every query filters by session. That works for ephemeral per-session facts (folds, plans, scratch notes), but breaks for knowledge that should be shared across every session — skills, code symbols, global conventions.

**Two scopes:**

- **Session scope** (existing default): entity is partitioned by the caller's session. Cross-session queries impossible by design. Appropriate for session-local facts.
- **Global scope** (new): entity is visible to every session for the tenant. The stored `session_id` is a well-known global sentinel (`tenant_global_session_uuid(tenant_id)`); reads fan to that sentinel partition.

**Which scope by default?**

Decided per `entity_type`:

| Entity type | Default scope | Rationale |
|---|---|---|
| `skill` | Global | Skills are methodology, inherently shared |
| `code_symbol` | Global | Codebase knowledge is shared across sessions |
| `concept` | Global | Domain concepts cross sessions |
| `decision` | Global | Decisions apply to the project, not one session |
| `pattern` | Global | Patterns are reusable knowledge |
| `person`, `place`, `org` | Global | Real-world entities are shared |
| `plan`, `fold` | Session | Inherently per-trajectory |
| `bug`, `event`, `preference` | Session (for now) | Often session-specific; can be promoted |

`ingest_skill` always writes scope=Global regardless of the caller's `session_id`. The caller's session is recorded in `ingested_by_session` for audit.

**Query behavior:**

- `retrieve_skills_for_context` reads from the global partition only (session_id is ignored for scope resolution, still used for the session-affinity re-rank signal below).
- `hybrid_search` queries BOTH scopes by default (global ∪ session). A new parameter `scope: Option<"session" | "global" | "both">` overrides.
- `explore_connections`, `find_memory_chain`, etc. follow the same scope rules as hybrid_search.

**Session affinity as a re-rank signal, not a filter.**

The caller's session is still relevant for ranking — if a skill was used in the current session, it's more likely still relevant. See "Session affinity" under the re-ranking section.

**Migration impact:**

Existing entities get `scope = Session` by default (no behavioral change). A one-shot migration (or passive as-you-go promotion) can set `scope = Global` for entity_types in the global list above. Skill ingest starts writing Global immediately; no backfill required for empty catalog.

### Why `description` is distinct from `context_snippet`

- `context_snippet` = the source text the entity was extracted from (audit trail).
- `description` = curated, retrieval-optimized prose about what the entity *is*. Embedded and ranked on.

For existing entity types without curated descriptions, `description = None` — retrieval falls back to name matching (current behavior).

### Properties JSON schema validation

Each `entity_type` declares a JSON schema for its `properties` field. Validation happens in the ingest handler, not at storage. Example:

```rust
// In entity::schemas
fn properties_schema(entity_type: &str) -> Option<&'static str /* JSON schema */> {
    match entity_type {
        "skill" => Some(SKILL_PROPERTIES_SCHEMA),
        "code_symbol" => Some(CODE_SYMBOL_PROPERTIES_SCHEMA),
        _ => None, // unvalidated types accept any properties
    }
}
```

### Skill properties schema (v1)

```json
{
  "$id": "fmem/skill.v1",
  "type": "object",
  "properties": {
    "category": { "type": "string" },
    "trigger_keywords": { "type": "array", "items": { "type": "string" } },
    "steps": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "phase": { "type": "string" },
          "instruction": { "type": "string" }
        },
        "required": ["instruction"]
      }
    },
    "output_artifacts": { "type": "array", "items": { "type": "string" } },
    "completion_criteria": { "type": "string" }
  },
  "required": ["category"]
}
```

Relationships like `prerequisites` and `related_concepts` are **edges**, not properties. Ingest creates `REQUIRES` and `RELATED_TO` edges to other skill entities.

## Hierarchical tags (graph-native)

Tags are first-class entities, not strings. Hierarchy is expressed via edges, allowing traversal (walk all tags under `testing`, find the closest common ancestor of two skills, etc.).

### Graph shape

- **Tag entity:** `entity_type="tag"`, scope=Global. Name is the tag's human label (`testing`, `tdd`, `quality`).
- **Edge `TAGGED_AS`:** entity → tag. "This skill belongs to this tag." An entity can have many.
- **Edge `PARENT_TAG`:** child tag → parent tag. "This tag is a sub-category of that tag." Builds the hierarchy DAG.

Example:

```
Skill(tdd) --TAGGED_AS--> Tag(tdd)
Tag(tdd) --PARENT_TAG--> Tag(testing)
Tag(testing) --PARENT_TAG--> Tag(quality)

Skill(bdd) --TAGGED_AS--> Tag(bdd)
Tag(bdd) --PARENT_TAG--> Tag(testing)
```

A walk from `Tag(testing)` via incoming `PARENT_TAG` edges reaches `tdd` and `bdd`; one more hop finds the skills themselves via incoming `TAGGED_AS`.

### Why not flat string tags?

Flat strings (`["testing/tdd", "testing/bdd"]`) support prefix match but lose:
- Ability to ask "what's the parent of testing?"
- Reparenting (moving `chaos-testing` from under `testing` to under `reliability` requires rewriting every skill).
- Visualization of the taxonomy itself.
- Tag descriptions, embeddings, PageRank, warmth — tags get the full entity treatment.

### Denormalized `tags` column for fast filter

Graph traversal per query is too slow for `hybrid_search`'s ~15ms budget. Each entity's `tags: Vec<String>` column stores **direct tags + all ancestor tag names**, materialized at ingest time.

- Skill `tdd` with `TAGGED_AS → Tag(tdd)` and `Tag(tdd) PARENT_TAG → Tag(testing) PARENT_TAG → Tag(quality)` gets `tags = ["tdd", "testing", "quality"]`.
- `hybrid_search` with `tags=["testing"]` does a column filter — no graph traversal.
- Reparenting triggers re-materialization of affected entities' columns (rare, batchable).

### Tag operations

- **Create tag:** `smart_ingest` with `entity_type="tag"`, scope=Global. Creates the node.
- **Tag an entity:** `create_edge(src=entity, dst=tag, edge_type="tagged_as")`. Ingest also appends tag name and ancestors to entity's `tags` column.
- **Build hierarchy:** `create_edge(src=child_tag, dst=parent_tag, edge_type="parent_tag")`.
- **Walk a taxonomy branch:** `explore_connections(entity_id=tag_id, traversal="related_entities", edge_types=["parent_tag", "tagged_as"])`. Existing tooling, no new primitives needed.

### `ingest_skill` interaction with tags

Skill ingestion:
1. Resolve each tag name in `tags: Vec<String>` — `smart_ingest` with `entity_type="tag"` (idempotent, creates if missing).
2. Resolve or create the skill's `category` as a tag.
3. Create `TAGGED_AS` edges from skill to all resolved tags.
4. Compute ancestor tags for each direct tag (via graph traversal on `PARENT_TAG`) and store the union in the skill's `tags` column.

Building the tag taxonomy itself (e.g. `testing PARENT_TAG quality`) is done via a separate call or a bootstrap migration — not every `ingest_skill` call.

### Bootstrapping the initial taxonomy

Forge's `frg fmem-skill-ingest` (see `../research/tools/forge/specs/todo/fmem-skill-ingest.md`) includes a one-time taxonomy seed step: create top-level tags (`quality`, `testing`, `security`, `architecture`, `tech`, `communication`, `management`, etc. — derived from the directory structure of `../research/skills/`) and link subcategories via `PARENT_TAG`.

## Schema migration

### CQL table changes

`entity_store` table gets new columns:

```cql
ALTER TABLE entity_store ADD description text;
ALTER TABLE entity_store ADD description_embedding list<float>;
ALTER TABLE entity_store ADD tags set<text>;
ALTER TABLE entity_store ADD properties text;  -- JSON
ALTER TABLE entity_store ADD content_hash text;
ALTER TABLE entity_store ADD updated_at timestamp;
```

### Index additions

- HNSW index on `description_embedding` (new ANN path)
- Secondary index on `tags` (for tag-filtered hybrid_search)

### Backfill strategy

Existing entities keep `description = NULL`, `tags = {}`, `properties = {}`. No data loss. Re-ingest can populate later.

## Tools

### Session handling across skill tools

All three skill tools treat `session_id` as **optional** and **non-scoping**:

- If the caller provides a valid UUID, it's used for the session-affinity re-rank signal and `ingested_by_session` audit.
- If the caller provides `"default"`, omits it, or provides junk, `resolve_session_id` injects the configured default (existing dispatcher behavior) — still only for affinity/audit, never for storage partition.
- Skills are always written and read from the global partition.

The tool schemas declare `session_id` as optional with no `format:uuid`, consistent with the recent schema sweep.

### `ingest_skill`

Wrapper over `smart_ingest` that validates skill properties and creates prerequisite/related edges.

**Input:**
```json
{
  "session_id": "...",
  "name": "tdd",
  "category": "testing",
  "description": "Guides red-green-refactor cycles...",
  "trigger_keywords": ["test", "red-green"],
  "prerequisites": ["unit-testing"],
  "related_concepts": ["refactoring", "mocking"],
  "steps": [{"phase": "Red", "instruction": "..."}],
  "output_artifacts": ["checklist"],
  "content_hash": "sha256:..."
}
```

**Behavior:**
1. Validate `properties` against skill schema.
2. Call `smart_ingest` with `entity_type="skill"`, `tags=["skill"]`, `description` embedded, `properties` set.
3. For each `prerequisites` name, resolve to entity_id and create `REQUIRES` edge.
4. For each `related_concepts` name, resolve and create `RELATED_TO` edge.
5. If `content_hash` matches the existing entity's `content_hash`, skip (idempotent).

**Output:** `{entity_id, action: Created|Updated|Skipped, _hint: "..."}`.

**LLM hint in response:** Every `ingest_skill` response includes a `_hint` field teaching the LLM to come back and refine:

> "Skills are global knowledge. If you use this skill and learn something new — a better step, a missing prerequisite, a clearer description — call `ingest_skill` again with refinements. Your changes persist across all sessions."

### `retrieve_skills_for_context`

Filtered two-stage retrieval (see below) restricted to `tags: ["skill"]`.

**Input:** `{session_id?, context, limit=5, min_score=0.6}`
**Output:** `{results: [{skill_name, entity_id, score, description, used_in_session}], _hint: "..."}`

`used_in_session: bool` surfaces whether this skill has been touched in the caller's session — caller can prefer familiar skills.

**LLM hint** (only when results exist):

> "These skills are shared across all sessions. If you successfully apply one, remember it for later. If you discover a better way to run one of these skills, call `ingest_skill` to refine it."

### `invoke_skill`

Fetches a skill by name, returns structured data for the caller to drive.

**Input:** `{session_id, skill_name, current_context}`
**Output:**
```json
{
  "skill_name": "tdd",
  "description": "...",
  "steps": [{"phase": "Red", "instruction": "..."}],
  "first_step_prompt": "Write a failing unit test that defines the expected behavior.",
  "completion_criteria": "All steps completed, tests green, refactor pass done.",
  "prerequisites_satisfied": true
}
```

No external tool calls. Caller (forge, another agent, the LLM itself) decides how to drive the steps.

### `hybrid_search` — new filter parameters

```json
{
  "session_id": "...",           // optional; affinity + audit only
  "query": "...",
  "embedding": [...],
  "entity_types": ["skill"],     // NEW — filter by type
  "tags": ["testing"],            // NEW — filter by tag (intersection)
  "scope": "both",                // NEW — "session" | "global" | "both" (default)
  "limit": 10
}
```

Filters default to `None`. Scope defaults to `"both"` (query global + caller's session, re-rank over the union). LLM is instructed via tool description:

- Pass `entity_types: ["skill"]` when looking for methodologies.
- Omit `session_id` (or pass `"default"`) to search across all your sessions.
- Pass `scope: "session"` only when you explicitly want session-local scratch work.

## Skill intention trigger

New `Skill` variant in the intention trigger enum:

```rust
enum TriggerType {
    Topic { keywords: Vec<String> },
    FilePattern { pattern: String },
    Duration { minutes: u32 },
    Context { condition: String },
    Skill {                          // NEW
        context_template: String,     // e.g. "working on {topic}"
        min_score: f32,               // default 0.8
    },
}
```

### Lifecycle

1. User or agent calls `check_intentions(context=...)`.
2. For each pending `Skill` intention, fill `context_template` with context, call `retrieve_skills_for_context`, take top result if `score >= min_score`.
3. Emit compact suggestion:
   ```
   {
     "trigger": "skill",
     "skill_name": "tdd",
     "score": 0.87,
     "hint": "For implementing features, consider TDD. Call invoke_skill('tdd') to start."
   }
   ```
4. Mark as "suggested in session" to suppress re-firing until `cooldown_turns` elapse.

### Anti-clutter guardrails

- **min_score floor:** 0.8 default, per-trigger override.
- **Rate limit:** ≤1 skill suggestion per N tool calls (default N=5).
- **Session dedup:** each skill suggested at most once per cooldown window.
- **Opt-out:** `SessionState.skill_suggestions_enabled: bool` (default on, configurable).
- **Budget:** the hint is ≤ 200 chars — LLM pulls the full skill body via `invoke_skill` only when interested.

## Two-stage retrieval with re-ranking

This is orthogonal to skills and improves retrieval for *every* entity type.

### Stage 1 — candidate generation (cheap, high recall)

For a query (text + optional embedding):

1. **ANN over name embeddings** — existing path.
2. **ANN over description embeddings** — new path (skip entities without descriptions).
3. **Phonetic match** over `entity_name`.
4. **Tag-exact match** when tags are filter-specified.

Union → deduplicate by `entity_id` → produces ~50-100 candidates.

### Stage 2 — re-ranking (precise)

Score each candidate as a weighted sum:

| Signal | Default weight | Source |
|---|---|---|
| semantic similarity (max of name-sim, desc-sim) | `w_sim = 0.35` | cosine similarity |
| PageRank reputation | `w_rep = 0.15` | already computed (pagerank.rs) |
| warmth (access frequency) | `w_heat = 0.10` | warmth.rs |
| recency decay | `w_age = 0.10` | `updated_at` |
| keyword overlap (query terms ∩ name + description + tags) | `w_kw = 0.10` | Jaccard |
| entity_type match | `w_type = 0.05` | 1.0 if filter matches, else neutral |
| session affinity | `w_session = 0.15` | see below |

Weights default-tuned; overridable per session or per query (`rerank_weights` parameter on hybrid_search). Sums to 1.0.

Scores normalize to [0, 1] per signal before weighting. Log each signal's contribution in tracing at debug level for tuning.

### Session affinity

For global-scope entities (skills, code symbols, concepts), the caller's session is *not* a storage filter but *is* a useful ranking signal. Two sub-components, summed:

- `ingested_this_session`: 1.0 if the entity's `ingested_by_session == caller_session_id`, else 0.
- `used_this_session`: decayed count of retrievals/references in this session from `SessionState.retrieval_tracker`, normalized to [0, 1].

`session_affinity = max(ingested_this_session, used_this_session)` — an entity that was just retrieved in this session boosts the same as one that was ingested in it.

For session-scope entities, `session_affinity = 1.0` (always matches the caller's session by definition — keeps them competitive against global entities in the blended ranking).

This gives us the property the user asked for: "if a skill was used in the current session, it's likely still relevant."

### Why not cross-encoder / LLM rerank?

- Latency budget on hybrid_search is ~15ms. Cross-encoder adds 50-200ms.
- LLM rerank adds a full tool-call round trip.
- Weighted heuristic re-rank runs in-process, <5ms.
- Can add cross-encoder as an opt-in `rerank_mode: "heuristic" | "cross_encoder"` parameter later.

## Viz cross-session support

Current viz (`http.rs:976`) loads nodes via `entity_list_session(session_id)` — snapshot is restricted to one session. This hides global entities and makes it impossible to see relationships that cross session boundaries (e.g., a skill referenced from multiple sessions).

Changes needed:

- **New snapshot mode.** Add a query param to the viz endpoint: `?scope=global|session|all` (default `all`). `all` uses `entity_list_all` (already exists in the Storage trait). `session` preserves current behavior. `global` filters to global-scope entities only.
- **Session chip on every node.** Each viz node includes `session_id` and a boolean `is_global`. UI renders a per-node badge (or colors node border) so the user can see where each entity lives.
- **Session filter control in UI.** A dropdown/multi-select lets the user scope the current visualization to one or more sessions, or to global only, without reloading the whole dataset.
- **Edge traversal across sessions.** `ExploreNeighborhood` messages currently assume the explored entity and its neighbors are in the same session. Update to follow edges regardless of session — edges already store their own `session_id` and the graph client doesn't filter by it on traversal.

Backend changes land with the scope work (Sprint 1). UI changes are a separate frontend task.

See `specs/todo/feat-viz-cross-session.md` for the implementation work item.

## Seed catalog from `../research` (via forge)

Deferred to forge (the generic admin tool). Design lives in:
`/Users/bkearns/src/research/tools/forge/specs/todo/fmem-skill-ingest.md`

Summary: forge walks `../research/skills/**/SKILL.md`, parses frontmatter + body, calls fmem's `ingest_skill` with a `content_hash` for idempotent updates. Re-runnable, idempotent, logs diffs.

## Migration plan

Three sprints, each independently shippable:

### Sprint 1 — Richer entity storage and scope

- Add columns to `entity_store` (CQL migration): `description`, `description_embedding`, `tags`, `properties`, `content_hash`, `updated_at`, `scope`, `ingested_by_session`.
- Extend `EntityEntry` struct and `entity_put`/`entity_get_by_id`.
- Add `tags`, `entity_types`, and `scope` filter to `hybrid_search`.
- Introduce the tenant-global session sentinel and write path for global-scope entities.
- Wire viz snapshot to `entity_list_all` when `scope=all` (see `specs/todo/feat-viz-cross-session.md`).
- No behavioral change for existing callers; new fields optional, scope defaults to Session.
- Tests: round-trip with new fields, existing-entity backward compat, global/session scope queries return correct partitions.

### Sprint 2 — Skills layer

- Register `skill` entity_type in the type registry.
- Implement `ingest_skill`, `retrieve_skills_for_context`, `invoke_skill`.
- JSON schema validation in ingest.
- Unit tests on each tool; e2e on skill round-trip.

### Sprint 3 — Re-ranking + active trigger

- Extract re-ranking into `rerank.rs` module.
- Wire into `hybrid_search` and `retrieve_skills_for_context`.
- Add `Skill` intention trigger variant.
- Integration test: ingest TDD skill → check_intentions with context → verify suggestion fires.

## Decisions

### Embedding model — nomic-embed-text-v2-moe for both name and description

A single embedding model is used for both `entity_embedding` (name) and `description_embedding`. This simplifies operations (one model to warm, one to version) and enables direct cosine comparison between name and description vectors in re-ranking.

**Decision: `nomic-embed-text-v2-moe`.** 305M-param MoE, 768-dim output (matryoshka-capable, default dimension preserved). Pulled via `ollama pull nomic-embed-text-v2-moe`.

**Migration impact:** v2 vectors are incomparable with v1 vectors. All existing `entity_embedding` / `fold_embedding` / `memo_embedding` must be re-generated as **Phase 0** of the backfill work item (`specs/todo/chore-entity-backfill-for-rich-schema.md`). Dimensions unchanged → no CQL schema change.

**Config defaults updated.** `default_embed_model()` in `config.rs` now returns `"nomic-embed-text-v2-moe"`; SessionState's `embed_model` field reads from config (removing four hardcoded occurrences in `dispatch.rs`).

### Tag name normalization

Tags are normalized at ingest: lowercased, dash-separated, alphanumeric + dash only. Enforced in `smart_ingest` when `entity_type="tag"`, applied by `ingest_skill` / `retrieve_skills_for_context` / `ensure_parent_tag` before lookup or write. Invalid input is normalized, not rejected, so callers don't have to pre-normalize.

Exact rule:
- Every character that isn't `[a-z0-9]` (after lowercasing) becomes `-`
- Consecutive `-` collapse to one
- Leading/trailing `-` strip

Examples:
- `"Chaos Engineering"` → `chaos-engineering`
- `"unit_testing"` → `unit-testing` (underscore is NOT preserved — noted by forge's skill-ingest implementation)
- `"foo/bar/baz"` → `foo-bar-baz`
- `"!!!symbols!!!"` → `symbols`
- `"  extra  "` → `extra`

**Underscore is not preserved.** If callers (e.g. forge's skill-ingest pipeline) have tag names using underscore in their source data (`tag-hierarchy.yaml`, SKILL.md frontmatter), they should expect the underscore to collapse to dash server-side and use the dashed form when querying.

### Tag hierarchy — cycle prevention via Cypher

`PARENT_TAG` forms a DAG. Cycle prevention runs on every write via a single Cypher query against the graph client *before* inserting the edge:

```cypher
MATCH path = (src:Tag {entity_id: $src_id})-[:PARENT_TAG*]->(dst:Tag {entity_id: $dst_id})
RETURN count(path) > 0 AS would_cycle
```

If `would_cycle = true`, the edge creation fails with a clear error naming both tags and the existing path.

**This must be tested end-to-end and fail loudly if broken.** A dedicated integration test creates a chain `A → B → C`, attempts `C → A`, and asserts the attempt is rejected. If the graph client is unreachable, cycle check fails closed (reject the edge) — never silently allow writes without the check.

### Skill versioning — YYYYMMDDNN format

Skills carry a `version` field in `properties`, assigned at ingest time: `YYYYMMDDNN` where `NN` is a zero-padded sequence number for that day (starting at `01`). The server auto-generates it on each `ingest_skill` call; callers cannot set it.

- `2026041501` = first skill version ingested on 2026-04-15
- `2026041502` = second on the same day
- History is preserved via the `SUPERSEDES` edge type (already exists in fmem) — newer skill version `SUPERSEDES` older. `invoke_skill(name)` defaults to the latest version; `invoke_skill(name, version)` pins.

### Intention trigger persistence

Skill triggers persist in the intention store like other triggers. **Cooldown state also persists**, not just session-local — survives restarts. Stored as a durable column on the intention row: `last_fired_at: timestamp`. On check, the server compares `last_fired_at + cooldown_window` to `now` before firing.

### Edge type registry — startup migration

Sprint 1 registers four new edge types at server startup via an idempotent CQL write: `TAGGED_AS`, `PARENT_TAG`, `REQUIRES`, `SUPERSEDES`. The startup path already loads the registry (see `main.rs:1086`); the migration inserts missing types inside a conditional write (`IF NOT EXISTS`). No restart surgery needed for existing deployments — the write is idempotent and runs each boot.

### `enrich_entities` writes to `description`

Sprint 1 updates `enrich_entities` to write the LLM-generated description into the dedicated `description` field and generate the matching `description_embedding`. The `ENRICHED_PREFIX` hack in `context_snippet` is removed for new writes. Existing entities keep the hack until Phase 1 of the backfill parses it out (`specs/todo/chore-entity-backfill-for-rich-schema.md`).

### Skill name lookup — secondary index (with Ferrosa 2i validation)

Exact name lookup for `invoke_skill(name)` uses a CQL secondary index on `(tenant_id, entity_type, entity_name)`. Before relying on this in Sprint 2, validate that Ferrosa's 2i implementation is correct:

- Lookup returns current data (no stale reads from index lag)
- Concurrent writes to indexed entities don't corrupt the index
- Index survives restart / compaction
- Performance scales with entity count

See `specs/todo/validate-ferrosa-2i.md` for the validation work item. If 2i has issues, they get fixed upstream in `../ferrosa` (not worked around here — see CLAUDE.md).

### `invoke_skill` — `did_you_mean` on miss

Missing skill returns `INVALID_PARAMS` with a structured payload:

```json
{
  "error": "skill not found: 'tdd-typo'",
  "did_you_mean": ["tdd", "dsm-analysis"],
  "hint": "Call retrieve_skills_for_context to discover available skills."
}
```

`did_you_mean` populated via phonetic match over all skill entity names, top 3 above a similarity threshold.

### Partition sizing — one global partition per tenant to start

Sprint 1 uses a single global partition per tenant: `session_id = tenant_global_session_uuid(tenant_id)`. Monitor partition size and read latency; if hot (>10k entities or p99 read >50ms), split by `entity_type` namespace in a future sprint.

### Re-rank telemetry (defer calibration, instrument now)

Default weights are not calibrated yet. Before deferring calibration, instrument re-ranking so future tuning is data-driven:

- Every `hybrid_search` / `retrieve_skills_for_context` call emits a structured `rerank` log line per returned result: `entity_id`, final score, and each signal's contribution (`sim`, `rep`, `heat`, `age`, `kw`, `type`, `session`).
- Logs go to the existing tracing pipeline at `debug` level (so production runs can flip it on selectively).
- Optional: materialize into a `rerank_audit` table keyed by `(tenant_id, query_id)` for offline analysis. Defer table creation until calibration sprint.

Offline calibration (later sprint): join rerank logs with `record_outcome` feedback to learn weights that predict successful retrievals.

## Non-goals

- Skill execution engine (we emit steps; caller executes).
- Per-tenant skill catalogs (single catalog shared across sessions).
- Skill authoring UI (skills come from markdown files via forge).
- Real-time re-ranking weight tuning (manual for v1).

## Related work

- `specs/memory-lifecycle.md` — memory states (active/dormant/silent/unavailable) — skills participate in this lifecycle.
- `specs/overview.md` — system overview; this document extends the entity store.
- `crates/ferrosa-memory-core/src/hybrid_search.rs` — existing retrieval code, will be extended.
- `crates/ferrosa-memory-core/src/pagerank.rs` — reputation scores consumed by re-rank.
- `crates/ferrosa-memory-core/src/warmth.rs` — warmth scores consumed by re-rank.
