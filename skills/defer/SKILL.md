---
name: defer
description: Capture deferred/follow-up work to the forge task board so it isn't lost. Use when you surface work you won't do now — out-of-scope items, prerequisites, follow-ups, "leave for later", known gaps. Pairs with /whats-next and /roadmap.
---

# Defer — capture surfaced-but-not-done work

When you (or the user) surface work that won't be done in this turn, write it to the
forge task board immediately so it can be recovered later via `/whats-next` and
`/roadmap`. This closes the "agents surface work then forget it" leak.

Follow `references/deferred-work-conventions.md` for the exact tagging contract.

Invocation shape: `/defer <work description> [--priority N] [--skill name] [--repo path]`.
Supporting reference: `references/deferred-work-conventions.md`.

## When to use
- You said any of: "out of scope", "follow-up", "prerequisite", "leave for later",
  "next step", "should also", "didn't get to", "known gap", or wrote a TODO in prose.
- A user hands you a bug batch, observations list, pasted incident notes, or uploaded report and asks you to "do the same thing" / continue a PR train / fix the first slice. **Record every non-trivial item from the batch immediately** before or while implementing the first slice; do not rely on mental triage.
- Treat capturing deferred work as part of a task's definition of done.

## Steps

0. **Explode batch reports before moving on.** For each discrete issue in an uploaded/pasted report:
   - create or update one Forge task per shippable fix,
   - preserve the source filename/message and reproduction clues in the body,
   - mark any item already fixed in this session complete with commit/verification evidence,
   - call out duplicates by commenting/updating the existing task rather than silently skipping them.

1. **Resolve the repo.** `--repo` if given, else `git -C <cwd> rev-parse --show-toplevel`.
   If not in a git repo, set repo to the cwd and note it.

2. **Dedup.** Call `mcp__forge__task_list` (or `frg task list`) and check open tasks
   (status not complete/archived) for this repo. If a near-identical title exists,
   **comment on it** (`mcp__forge__task_comment`) instead of creating a duplicate; stop.

3. **Create the task** via `mcp__forge__task_create`:
   - `title`: imperative, specific (this is the dedup key).
   - `body`: why deferred + context to act on it later.
   - `workspace_path`: the repo root. `workspace_kind`: `repo`.
   - `created_by`: `deferred:manual`. `priority`: `--priority` or 50. `skills`: `--skill`.
   Prefer MCP so `workspace_path` is set natively. If only the CLI is available, follow
   the conventions' body `repo=` line.

4. **Confirm** the task id and title back to the user in one line.

## Examples
```
/defer add a --workspace-path flag to `frg task create` so the hook can tag repo natively --priority 60 --skill rust
/defer repair the quarantined system_schema.columns node on the ferrosa-memory cluster --priority 70
```

## Notes
- This is the *explicit* capture path. The `defer-capture` Stop hook is the automatic
  safety net that catches deferrals you forget to log.
- Keep one task per discrete unit of work; link prerequisites with `parents`.
