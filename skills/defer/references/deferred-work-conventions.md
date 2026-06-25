# Deferred-work conventions (shared by /defer, /whats-next, /roadmap, defer-capture hook)

One shared contract so captured work is findable later. Backed by the **forge task
board** (CQL `agent_memory.tasks`) — the same DB `/whats-next` and `/roadmap` read.

## Where it goes
- **DB:** forge task board. Host resolves via `--cql-host` arg → `FORGE_CQL_HOST` env →
  `.forge/config.toml` `cql_host` → `127.0.0.1:9042`.
- **Deferred inbox = `status: triage`.** New, unreviewed work lands here. Promotion:
  `triage → ready → in_progress → complete` (or `archived` if dropped).

## Tagging contract
| Field | Value |
|---|---|
| `title` | Imperative, specific, ≤ ~80 chars. Doubles as the dedup key. |
| `body` | Why it was deferred + enough context to act. **Hook/CLI path** (no `workspace_path` flag) MUST start the body with a machine line: `repo=<abs repo root>` then `source=… session=…`. |
| `workspace_path` | **(agent/MCP path)** absolute repo root (`git rev-parse --show-toplevel`). The per-repo key. |
| `workspace_kind` | `repo` (agent/MCP path). |
| `created_by` | `deferred:manual` (/defer) · `deferred:hook` (Stop hook) · `deferred:roadmap`. |
| `priority` | 1–100, default 50. Blockers/prereqs higher. |
| `skills` | optional domain tags. |

> CLI gap: `frg task create` exposes `--workspace` (kind) but not `--workspace-path`.
> Until that flag exists, the hook encodes the repo in the body `repo=` line; queries
> match **either** `workspace_path == repo` **or** body `repo=<repo>`. (Follow-up: add
> `--workspace-path` + `--metadata` to `frg task create` to unify.)

## "Belongs to repo R" (used by queries)
A task belongs to repo `R` if `workspace_path == R` OR its body's first line is `repo=R`.

## Dedup rule
Before creating, list open tasks (status not in {complete, archived}) for the repo and
**skip** if a normalized title (lowercased, collapse whitespace, strip trailing
punctuation) already matches. Prefer updating/commenting over duplicating.

## What counts as "deferred work"
Work the assistant *surfaced but did not do*: out-of-scope follow-ups, prerequisites,
"leave for later", "next step", "should also", "didn't get to", TODO-in-prose, known
gaps. NOT: completed work, speculative ideas with no decision, or trivia.
