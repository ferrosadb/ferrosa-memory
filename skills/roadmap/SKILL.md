---
name: roadmap
description: Build a roadmap for a repo by combining deferred work from the forge task board with a live TODO/FIXME scan of the repo, synthesized into themed, prioritized Now/Next/Later. Use when asked for a repo's roadmap, backlog, or debt plan.
argument-hint: "[repo-path] [--persist] [--include-done]"
supplementary-files:
  - ../defer/references/deferred-work-conventions.md
---

# Roadmap — DB deferred work + live repo TODOs + unshipped WIP → a plan

Synthesize a roadmap for a repo from three sources: (1) captured deferred work in the
forge task board, (2) TODO/FIXME/HACK/BUG comments still in the code, and (3)
unshipped work in branches/worktrees. Tagging contract:
`../defer/references/deferred-work-conventions.md`.

Invocation shape: `/roadmap [repo-path] [--persist] [--include-done]`.
Supporting reference: `../defer/references/deferred-work-conventions.md`.

## Steps

1. **Resolve the repo.** Positional arg if given, else `git rev-parse --show-toplevel`.

2. **Source A — DB deferred work.** `mcp__forge__task_board` / `task_list`; keep open
   tasks (exclude `complete`/`archived` unless `--include-done`) belonging to the repo
   (`workspace_path == repo` OR body `repo=<repo>` line).

3. **Source B — live code TODOs.** `mcp__forge__todo_extract` with `path = repo`
   (blame on). Use its kind (TODO/FIXME/HACK/BUG/XXX), file:line, author, and staleness
   buckets. Discount false positives: keywords inside test fixtures, detector code, or
   docs that *mention* the keywords are not real debt — say so rather than padding.

4. **Source C — unshipped WIP (branches & worktrees).** Unshipped work is the highest-
   value, most-perishable backlog: it's already built and rots fastest. Enumerate it.
   - `git worktree list` — every linked worktree is in-flight work; note the branch + path.
   - `git fetch origin` first (local `main` is often stale), then classify each branch by
     **what it would actually add to `origin/main`** — never by commit SHAs or counts.
     Squash-merged branches keep divergent SHAs, so `git log origin/main..<b>` and the
     two-dot `git diff origin/main..<b>` both still list their commits and falsely flag
     already-merged work as unshipped. Use a **content/merge preview**:
     ```
     git switch -c _wip_check origin/main
     git merge --no-commit --no-ff origin/<b>      # (or the local branch)
     git diff --cached --shortstat origin/main     # the net change merging would land
     git merge --abort; git switch -; git branch -D _wip_check
     ```
     (No-checkout equivalent on git ≥ 2.38: `git merge-tree origin/main origin/<b>`, then
     diff the resulting tree against `origin/main`.)
     - **empty net diff, no conflicts** → the branch is **already merged** (commonly via an
       earlier squash) → it's a **leftover: close the PR / delete the branch**, not roadmap
       work. (Rebase-and-merge failing with "can't be rebased" is a classic symptom — it
       replays commits that re-add files already on main.)
     - **non-empty net diff** → *that diff is the genuine unshipped content* → **ship it**.
   - Use `git log origin/main..<b> --no-merges --oneline` only to *describe* the unshipped
     commits for the roadmap — never to decide merged-vs-unshipped (it lies for squashes).
   - Cross-check `gh pr list --state merged` — a branch whose PR already shows **MERGED** is
     a leftover even though its SHAs still diverge from main.
   - Bucket each WIP branch by **shippability**: *ship-now* (recent, small, focused, builds/
     CI green) vs *stale* (weeks old, huge diff vs origin/main, behind main → rebase-or-
     abandon decision, not a clean ship). Also surface uncommitted working-tree changes.

5. **Merge & dedup.** Fold all sources into one list. A DB task and a code TODO describing
   the same thing are one item (prefer the DB task; cite the `file:line`). Normalize titles.

6. **Theme & prioritize.** Group by theme (subsystem/area). Rank with a blend of: DB
   `priority`, TODO severity (FIXME/BUG > HACK > TODO/NOTE), and staleness (older =
   higher). **Prioritize shipping all WIP first**: ship-now branches and merged-leftover
   cleanup go in **Now** ahead of new work — finished-but-unmerged work is sunk cost left
   stranded, and the longer it sits the more it conflicts with main. Stale WIP needing a
   rebase-or-abandon call goes in **Next**. Then bucket the rest: **Now** (blockers/high),
   **Next** (medium), **Later** (low/nice).

7. **Output** a markdown roadmap: per theme, the bucketed items with source tags
   (`task:<id>`, `<file>:<line>`, or `branch:<name>`), priority, and age. Start with a
   2–3 line summary (counts, top risks). Call out unshipped WIP and merged-leftover
   branches explicitly. End with the biggest gaps and what's unowned.

8. **`--persist` (optional).** Write the roadmap back so it's actionable:
   - Promote stand-alone code-TODOs that aren't yet tracked into `triage` tasks
     (`created_by: deferred:roadmap`, repo-tagged) — dedup first.
   - Capture each ship-now WIP branch and each rebase-or-abandon stale branch as a task
     so the shipping work isn't lost (it usually isn't in the DB yet).
   - Create one parent "Roadmap: <repo>" task and link the buckets as children
     (`parents`), or a `checklist_state create_dag`. Report what was created.

## Examples

```
/roadmap
/roadmap ~/src/ferrosa-suite/ferrosa
/roadmap . --persist
```

## Notes

- Three intents, three sources: code TODOs are left *in the tree*; DB tasks are work the
  agent *surfaced in conversation*; WIP branches/worktrees are work *already built but not
  shipped*. The roadmap is their union — that's the point.
- Unshipped WIP is the most perishable backlog: it's done, but it rots (drifts from main,
  conflicts grow, context is lost). Shipping it clears more value per minute than starting
  anything new, so it leads the roadmap.
- Keep it honest: if a source is empty or unreachable, say so; don't pad the roadmap.
  Don't count merged-but-undeleted branches or keyword-in-fixture TODOs as real work.
