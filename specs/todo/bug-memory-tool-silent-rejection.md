---
type: bug
priority: P1
status: draft
created: 2026-05-04
reported-by: Hermes Agent (post-debugging session)
---

# `memory` tool silent rejection at ~2,200 chars blocks operational notes

## Problem

The Hermes `memory` tool has a hard limit of ~2,200 characters. When adding a new entry would exceed the limit, it silently fails with:
> `Memory at 2,056/2,200 chars. Adding this entry (504 chars) would exceed the limit. Replace or remove existing entries first.`

During a live debugging session, this blocked me from saving three critical operational notes mid-stream. I had to pivot to a **skill** (`ferrosa-memory-ops`) instead, which works but isn't automatic — I have to `skill_view` it to recall.

This creates a perverse incentive: agents will avoid `memory` and dump operational notes into ephemeral chat context instead.

## Why it matters

The `memory` tool is the primary per-turn recall mechanism. If it's too small for operational runbooks, agents will lose context between sessions or rely on slower retrieval paths (fmem `hybrid_search`, skill files).

## Desired Behavior

One of:
1. **Auto-spill to fmem:** When `memory` is full, automatically call `smart_ingest` with the entry content and return a short memo reference (e.g., `stored in fmem as entity e96b127c`).
2. **Increase the limit:** Raise `memory` capacity to ~10KB or make it configurable per-agent.
3. **Explicit eviction policy:** When full, auto-remove the oldest/lowest-priority entries and notify the user.

## Proposed Fix Directions

### Option A: Auto-spill to fmem (preferred)
- In the `memory` tool implementation, catch the "would exceed limit" error.
- Call `smart_ingest` with the entry content, entity_type="note", entity_name=extracted from first sentence.
- Return a reference: `Stored in ferrosa-memory as entity <id>. Use hybrid_search to recall.`
- This makes `memory` a thin cache layer over fmem, which is the intended architecture.

### Option B: Increase limit
- Raise the `memory` char limit from 2,200 to 8,192 (one typical model context window page).
- Or make it configurable in `config.yaml`: `memory.max_chars: 8192`.
- Pros: Simple. Cons: Still a hard limit, still silently rejects if exceeded.

### Option C: Eviction + notification
- When `memory` is full, remove the oldest entry (or lowest-priority based on access count).
- Notify: `Replaced oldest memory entry (X) with new entry (Y).`
- Pros: Keeps memory fresh. Cons: Loses old context without user consent.

**Recommendation: Option A** — aligns with the fmem-first architecture and doesn't require UI changes.

## Acceptance Criteria

- [ ] `memory` tool with 2,200/2,200 chars can still accept a new entry by spilling to fmem.
- [ ] The spill creates a retrievable entity with `entity_type="note"`.
- [ ] A `hybrid_search` for the spilled content returns it.
- [ ] The user sees a clear message: `Memory full — stored in ferrosa-memory as <id>.`
- [ ] The old `memory` entries remain intact (no eviction without consent).

## Related

- `feat-auto-session-entity-extraction.md` — automatic entity extraction
- `memory-sync.md` — memory lifecycle and eviction
- `memory-lifecycle.md` — design for memory tiers
