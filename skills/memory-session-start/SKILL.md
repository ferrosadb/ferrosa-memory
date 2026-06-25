---
name: memory-session-start
description: Restore working context from Ferrosa Memory at the start of a session (or after /clear) before doing anything else. Use at session start, after compaction, or when resuming work on a repo.
---

# Memory Session Start — restore context before acting

Ferrosa Memory is a semantic memory the agent should consult **first**, not as a
fallback. At the start of every session (including after `/clear` or compaction),
restore context before reading files or asking the user to re-explain.

Do this automatically — don't wait to be asked.

## Steps

1. **Gather cheap signals.** Current git branch (`git rev-parse --abbrev-ref HEAD`)
   and the last few commit subjects (`git log --oneline -5`).

2. **Check prospective memory.** Call `check_intentions` with a short description of
   what you're about to do (branch + recent commits as context). Act on anything it
   returns — these are "remember to do X when Y" notes left in earlier sessions.

3. **Search for active work.** Call `hybrid_search` with the branch name plus
   keywords from the recent commits. Read what comes back instead of grepping. For
   `document_chunk` hits, call `get_context_window` to pull adjacent chunks.

4. **Summarize out loud.** Tell the user, in 2–3 lines, what you remember about the
   current work so they know you have context (e.g. "Last session you were wiring the
   foresight retrieval filter; PR #121 is open; the `set_foresight` tool is now
   tier-1."). If memory returns nothing useful, say so plainly.

## Why

Agents lose all working context across sessions and compaction. The memory server
persists entities, relationships, scenes, profiles, and intentions across sessions —
restoring from it turns "explain everything again" into a 2-second recall.

## Notes

- Two calls (`check_intentions` + `hybrid_search`) cost ~15 ms total. Always worth it.
- If a session has been consolidated, `hybrid_search` also surfaces a **session
  profile** (the workspace gist) and **scenes** (coherent entity clusters) — these are
  built automatically by background consolidation.
- Pairs with [`consolidate-wrapup`](../consolidate-wrapup/SKILL.md) (run at the end of a
  session) and [`set-foresight`](../set-foresight/SKILL.md) (declare time-bounded facts).
