---
name: whats-next
description: Answer "what's next?" by surfacing open deferred/follow-up work for the current repo from the forge task board, ranked. Use when asked what to work on next, what's outstanding, or to resume work.
---

# What's Next — surface outstanding work for a repo

Read the deferred-work inbox (forge task board) and present what to do next. Companion
to `/defer` (capture) and `/roadmap` (synthesize). Tagging contract:
`../defer/references/deferred-work-conventions.md`.

Invocation shape: `/whats-next [repo-path] [--all-status] [--global] [--limit N]`.
Supporting reference: `../defer/references/deferred-work-conventions.md`.

## Steps

1. **Resolve the repo.** Positional arg if given, else
   `git -C <cwd> rev-parse --show-toplevel`. `--global` = across all repos (skip filter).

2. **Fetch open work.** `mcp__forge__task_board` (or `task_list`). Open = status in
   `triage`, `ready`, `in_progress`, `blocked` (exclude `complete`/`archived`). With
   `--all-status`, include everything.

3. **Filter to the repo** (unless `--global`): keep tasks where
   `workspace_path == repo` **OR** the body's first line is `repo=<repo>` (the hook/CLI
   path). See the conventions' "belongs to repo" rule.

4. **Rank & group.** Order by status (in_progress → blocked → ready → triage), then
   priority desc, then oldest `created_at`. Show ≤ `--limit` (default 15) per group.

5. **Present** compactly — per item: `task_id` · title · priority · age (days) ·
   source (`created_by`). Lead with a one-line count summary
   (e.g. "3 in progress, 5 ready, 12 triage"). Then offer next actions: promote a
   triage item (`task_update --status ready`), start one, or run `/roadmap` for the
   full themed view.

## Examples
```
/whats-next
/whats-next ~/src/ferrosa-suite/ferrosa --limit 10
/whats-next --global
```

## Notes
- `triage` items are the unreviewed deferred inbox — skim them first; promote real ones
  to `ready`, archive noise.
- If the board can't be reached (no cluster), say so plainly (fail loud) rather than
  reporting "nothing next".
