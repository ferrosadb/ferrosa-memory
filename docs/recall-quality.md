# Recall Quality

Ferrosa Memory doesn't just store entities — it actively makes retrieval better. This
guide covers the recall-quality features, how they work, and how to use them. Most of it
is **automatic**: a background *dream cycle* (see [Automatic
consolidation](#automatic-consolidation)) builds these structures for free.

- [Time-bounded foresight](#time-bounded-foresight)
- [Consolidation scenes](#consolidation-scenes)
- [Workspace profiles](#workspace-profiles)
- [Retrieval traces](#retrieval-traces)
- [Automatic consolidation](#automatic-consolidation)

---

## Time-bounded foresight

A **foresight fact** is a prospective, time-bounded fact: it only holds during a validity
window. Retrieval surfaces it **only while it is valid at the current time**, so expired
deadlines and not-yet-active plans never leak into context.

### Declaring one

```jsonc
set_foresight {
  "content": "Code freeze on the release branch until the 0.17 cut",
  "valid_until": "2026-07-01T00:00:00Z",   // RFC3339; omit for no expiry
  // "valid_from": "2026-06-30T00:00:00Z",  // RFC3339; omit for "active now"
  // "session_id": "<uuid>"                  // defaults to the current session
}
// -> { "fact_id": "3b4d3210-..." }
```

Use it whenever a fact has a clear start and/or end:

| Statement | Field(s) |
|-----------|----------|
| "Code freeze until 2026-07-01" | `valid_until` |
| "Migration goes live on 2026-06-30" | `valid_from` |
| "API v1 is deprecated as of today" | `valid_until` at the cutover |
| "Use the staging cluster this week" | `valid_from` + `valid_until` |

### Retrieving it

Nothing special — a normal `hybrid_search` includes active foresight facts, ranked
alongside everything else:

```jsonc
hybrid_search { "query": "release branch freeze" }
// -> results include:
// { "content": "Code freeze on the release branch until the 0.17 cut",
//   "result_type": "foresight", "source": "foresight",
//   "hint": "valid until 2026-07-01T00:00:00+00:00" }
```

A fact whose window has **passed** (or hasn't **started**) is filtered out before fusion —
it simply doesn't appear. The validity check happens at *now*; parameterized "as-of T"
time-travel is a future extension.

> Distinct from supersession: ordinary facts are always-on and a newer fact *replaces* an
> older one. Foresight facts are inherently temporary and self-expiring.

---

## Consolidation scenes

When a session accumulates clusters of related entities, consolidation folds each cluster
of **3+ entities** into a durable, retrievable **scene** — a coherent semantic unit. This
turns "loose fragments scattered across results" into "recall the whole cluster at once".

A scene carries:

- a **summary** listing its members (bounded — large clusters show the first members + `+N more`),
- its **member ids**, and
- a **centroid embedding** (the mean of its members' embeddings), computed during
  consolidation — no extra embedding call.

### Two ways a scene matches

`hybrid_search` scores a scene the *stronger* of:

1. **Lexical** — token overlap on the summary, and
2. **Semantic** — cosine between the query embedding and the scene's centroid.

So a scene surfaces even when its wording differs from the query, as long as it's
semantically close (the cosine must clear a threshold so weak vector noise can't surface it).

### Inline member expansion

When a scene matches, retrieval **pulls in its member entities too** — including members
that didn't match the query on their own:

```jsonc
hybrid_search { "query": "billing migration", "embedding": [/* ... */] }
// -> a matching scene (result_type: "scene") PLUS its members
//    (source: "scene_member", hint: "surfaced via its scene")
```

This is bounded (top scenes, capped members, deduped) and the fusion step merges any
overlap with direct entity hits.

---

## Workspace profiles

Each consolidated session gets a compact **profile**: the session's gist — active
entities, and repo / branch / task context. `hybrid_search` injects it as **always-on
context**, so every query starts with the session's frame even if the query itself is
narrow.

```jsonc
hybrid_search { "query": "where did we leave the auth work" }
// -> results include a high-level profile entry:
// { "result_type": "profile", "source": "profile",
//   "content": "Session covers 4 scene(s): ...",
//   "hint": "session profile from 4 scene(s)" }
```

Profiles are rebuilt each consolidation cycle from the session's scenes (one profile per
session, upserted), so they stay current as work progresses.

---

## Retrieval traces

Every `hybrid_search` durably records a **trace**: the query, which candidate sources
produced results (and how many), and the returned result ids. This is the substrate for
offline learning — tuning fusion weights, detecting recall regressions, and (future)
training a learned retrieval policy from feedback.

Traces are best-effort and never gate the search that produced them. They are written
automatically; no action is required.

---

## Automatic consolidation

The **dream cycle** runs as a background worker on the long-running server. It fires on a
**periodic tick** (default every `idle_consolidation_seconds`, ~20 s) and, for each
session that received writes since the last run, it:

1. **decays** stale edge weights,
2. **discovers connections** and creates `CO_OCCURS` graph edges,
3. **folds clusters** of 3+ entities into [scenes](#consolidation-scenes) (with centroid embeddings),
4. **builds/refreshes** the session [profile](#workspace-profiles), and
5. optionally **prunes** stale edges.

This means scenes, profiles, and edges appear **for free** shortly after you ingest related
entities — no manual step. To force a pass immediately (e.g. at the end of a productive
session), call `run_consolidation` (the request only *queues* the work; the worker does it):

```jsonc
run_consolidation { }   // optionally { "session_id": "<uuid>" }
```

Or use the [`/consolidate-wrapup`](../skills/consolidate-wrapup/SKILL.md) skill.

### Configuration

| Key (`[server]`) | Default | Meaning |
|------------------|---------|---------|
| `idle_consolidation_enabled` | `true` | Run the background dream cycle |
| `idle_consolidation_seconds` | `20` | Tick cadence (consolidates only when there is new data) |
| `edge_decay_factor` | `0.95` | Per-cycle multiplier applied to unreinforced edge weights (`1.0` disables) |
| `stale_edge_max_days` | `0` | Prune edges older than N days (`0` disables) |

> Consolidation runs under the tenant + session that wrote the data, and is idempotent
> (stable scene ids), so repeated cycles upsert rather than duplicate.

---

## See also

- [`MEMORY_GUIDE.md`](../MEMORY_GUIDE.md) — day-to-day memory workflow
- [`skills/`](../skills/) — slash-command playbooks (`/set-foresight`, `/consolidate-wrapup`, `/memory-session-start`)
- [`README.md`](../README.md#recall-quality) — the recall-quality summary + measured impact
