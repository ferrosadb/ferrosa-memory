---
name: consolidate-wrapup
description: Force a Ferrosa Memory consolidation pass at the end of a productive session so the dream cycle (edges, scenes, profiles) runs now instead of on the idle timer. Use at wrap-up, when the user says "that's it for now", or before a long break.
---

# Consolidate Wrap-up — run the dream cycle on demand

Ferrosa Memory runs **dream consolidation** automatically: a background worker fires on
a periodic tick (default every ~20 s when there is new data) and, for each session that
received writes, it:

- discovers connections and creates `CO_OCCURS` graph edges,
- folds clusters of 3+ related entities into durable, retrievable **scenes** (with a
  member-centroid embedding for semantic matching),
- builds/refreshes a per-session **profile** (the workspace gist), and
- decays stale edge weights.

You usually don't need to do anything — it's free and automatic. Use this skill to
**force** a pass now (e.g. at the end of a session) so the next session starts with
fresh scenes and a current profile.

## Steps

1. **Wrap up writes first.** Make sure the session's new knowledge is ingested
   (`smart_ingest`) and any known relationships are linked (`create_edge`).

2. **Request consolidation.** Call `run_consolidation` (optionally with `session_id`).
   The request path only *queues* the work; the background worker performs it shortly
   after — so the call returns immediately.

3. **Confirm (optional).** On the next `hybrid_search`, scene results
   (`result_type: "scene"`) and an injected session profile (`source: "profile"`)
   indicate consolidation has run.

## Why

Scenes and profiles make later recall dramatically better: a single coherent cluster is
retrieved as a unit (and expands to its members), and the profile gives every search the
session's frame. Forcing a pass at wrap-up means the *next* session restores richer
context immediately.

## Notes

- Consolidation is idempotent (stable scene ids) — running it repeatedly upserts rather
  than duplicating.
- It needs at least 3 related entities in a session to form a scene; smaller sessions
  still build edges and refresh nothing-to-do quietly.
- Pairs with [`memory-session-start`](../memory-session-start/SKILL.md), which consumes
  the scenes and profile this produces.
