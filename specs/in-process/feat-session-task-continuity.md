---
type: feat
priority: P1
status: in-process
created: 2026-06-15
updated: 2026-06-15
reported-by: blueprint phase 0
executive_summary: >
  Add durable session_task state to ferrosa-memory so agents can preserve
  active work, focus, aliases, and resume behavior across compaction,
  restarts, and long conversations without relying on noisy semantic search.
---

# Session Task Continuity

## Goal

Make `ferrosa-memory` the authoritative active-task store for agent sessions.
Hermes and other clients should cache and render task state, but fmem should
generate canonical IDs, maintain focus, expose lifecycle tools, return recovery
hints, and inject compact task context after compaction or session start.

## Current Repo State

- `ferrosa-memory` already owns session configuration, hooks, MCP tools,
  semantic recall, temporal turn links, plan state, and schema migrations.
- Existing `plan_write`, `plan`, and `plan_update` tools provide hierarchical
  planning context, but they do not fully model client-visible work items.
- The tool catalog already includes temporal turn-chain access, which can be
  referenced from compact task recall instead of dumping raw history into
  context.
- Direct CQL is acceptable for fmem-owned app tables. Graph-owned backing tables
  remain outside this feature.

## Phase 0 Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| 1 | Support multiple active tasks per session. | Long sessions can have several in-flight tasks and suspended work. |
| 2 | Return a deterministic foreground task plus active working set. | Compaction needs one clear foreground objective without losing the rest. |
| 3 | Use `focus_rank`, `in_progress`, `priority`, then `updated_at` as deterministic ranking fallback. | Explicit focus wins, but behavior remains stable without client hints. |
| 4 | Recommend sub-agents when active task lists or child task lists grow beyond policy thresholds. | Large task sets should be decomposed rather than carried as one agent context. |
| 5 | Provide both full-state upsert and lifecycle helper tools. | Upsert supports caches and retries; helpers reduce accidental state corruption. |
| 6 | Do not hard-delete tasks through normal lifecycle tools. | Cancelled/superseded history explains why work disappeared. |
| 7 | Scope tasks by `(tenant_id, session_id)` and copy normalized client identity fields. | Session remains canonical while workspace/thread lookups remain deterministic. |
| 8 | fmem generates and tracks canonical task IDs. | Agents should not hallucinate stable identifiers. |
| 9 | Support scoped aliases as non-canonical references. | Clients can use natural handles while fmem resolves to canonical task IDs. |
| 10 | Close events are structured by default; semantic memory ingestion is opt-in or policy-gated. | This reduces recall noise and token usage. |
| 11 | fmem returns structured recovery hints. | Clients can recover deterministically when lost or ambiguous. |
| 12 | `session_task` is client-visible during work; planning is fallback or explicit. | This reduces drift between visible tasks and planning state. |
| 13 | fmem may auto-create, switch, and move focus through a focus stack. | Long conversations naturally shift tasks and need resumable suspended work. |
| 14 | Add `session_task_observe` as the generic observation entry point. | Hooks and clients need one policy-controlled way to report user/tool events. |
| 15 | Auto behavior is per-session policy controlled. | Existing clients should not be surprised by automatic focus changes. |
| 16 | Persist task rows and focus stack separately. | Stack semantics should not be inferred from timestamps. |
| 17 | v1 task-shift detection is deterministic; v2 may add an optional LLM judge. | Task continuity must work without requiring a large model. |
| 18 | Inject compact structured task context plus temporal-link recovery hints. | Agents need low-noise state and a path to fetch history on demand. |

## Future State

The session task subsystem should expose:

- canonical server-generated task IDs
- scoped aliases and idempotency keys for client retries
- active task working set and deterministic foreground task
- persisted focus stack with ask-to-resume and auto-resume modes
- lifecycle event log for audit and recovery
- compact recall snapshot for hooks and compaction
- structured recovery hints for lost-agent conditions
- sub-agent recommendation hints when task lists grow too large

`plan_*` tools should not become a competing source of truth. They can remain
available for explicit planning and compatibility, but visible in-flight work
should flow through `session_task`.

## Proposed MCP Surface

### Core Tools

- `session_task_put`: idempotent full-state upsert; returns canonical `task_id`.
- `session_task_get`: fetch by canonical task ID or scoped alias.
- `session_task_current`: returns foreground task, active working set, focus
  stack summary, policy hints, and recovery hints.
- `session_task_list`: list tasks by status, parent, tag, alias scope, or
  client identity.
- `session_task_observe`: accept deterministic user/tool/session events and
  return the applied or suggested task action.

### Lifecycle Helpers

- `session_task_start`
- `session_task_update`
- `session_task_complete`
- `session_task_cancel`
- `session_task_supersede`
- `session_task_focus`
- `session_task_resume`

### Policy Tools

- `session_task_policy_get`
- `session_task_policy_put`

Initial policy fields:

| Field | Values | Default |
|-------|--------|---------|
| `auto_task_detection` | `off`, `suggest`, `apply` | `suggest` |
| `auto_resume` | `off`, `ask`, `inject` | `ask` |
| `max_active_before_subagents` | positive integer | `5` |
| `max_children_before_subagents` | positive integer | `4` |
| `confidence_threshold` | decimal between `0.0` and `1.0` | implementation-defined |

## Deterministic Observe Actions

`session_task_observe` should return one of:

- `continue_current`
- `update_current`
- `create_child`
- `push_and_focus_new`
- `pop_resume_candidate`
- `ask_user_to_resume`
- `inject_auto_resume`
- `recommend_subagents`
- `refresh_current_before_writing`

V1 inputs should be deterministic event types such as `todo_created`,
`todo_updated`, `task_completed`, `user_requested_new_task`,
`user_requested_pause`, `user_requested_resume`, `user_requested_switch`,
`compaction_boundary`, `session_start`, `alias_not_found`, and
`alias_ambiguous`.

## Storage Model

The original one-table proposal is not sufficient for deterministic Cassandra
queries because `status != completed` over a timestamp-clustered partition is a
filter, not a reliable query shape. Use fmem-owned app tables designed for the
read paths.

Required tables:

- `session_tasks_by_id`: canonical task details by `(tenant_id, session_id,
  task_id)`.
- `session_tasks_by_status`: query active, blocked, pending, completed, and
  cancelled tasks without filtering.
- `session_task_focus_stack`: ordered focus stack per `(tenant_id, session_id)`.
- `session_task_aliases`: scoped alias to canonical task ID mapping.
- `session_task_events`: append-only lifecycle and observe event trail.
- `session_task_policies`: per-session or scoped policy configuration.

Every implementation must include a versioned migration registered in order.
If an existing table is changed incompatibly, use staging/copy/swap with
row-count verification and fail-loud startup behavior.

## Compact Recall Injection

Session start and post-compaction hooks should inject a bounded structured
snapshot:

- foreground task title, status, priority, concise description, and next action
- active task count
- top suspended tasks from the focus stack, usually one to three titles
- sub-agent recommendation hints when thresholds are crossed
- recovery hints when focus or aliases look stale
- instructions for fetching prior context through temporal links, such as
  `turn_chain`, context windows, or session task event IDs

Completed and cancelled history should be omitted unless explicitly requested.
The default token budget should be small enough to avoid replacing useful model
context with task history.

## Recovery Hints

Task API responses should include structured hints when fmem detects likely
lostness:

| Condition | Recovery hint |
|-----------|---------------|
| Alias not found | call `session_task_list` or `session_task_current` |
| Alias ambiguous | list matching aliases and ask client to choose |
| Update targets completed/cancelled task | refresh active tasks before writing |
| No recent task touch but active tasks exist | refresh current task set |
| Active count exceeds threshold | consider sub-agents |
| Child count exceeds threshold | split work into sub-agents |
| Foreground closes with suspended task available | ask to resume or inject auto-resume |

Hints should be machine-readable, for example:

```json
{
  "recovery_hint": {
    "action": "session_task_current",
    "reason": "ambiguous_alias"
  }
}
```

## Acceptance Gates

- fmem, not clients, generates canonical task IDs.
- Upsert and lifecycle helpers are idempotent under retry.
- Multiple active tasks can be listed without server-side filtering hacks.
- `session_task_current` deterministically returns foreground plus active set.
- Focus stack push/pop works across process restart.
- Auto-resume policy can ask or inject a compact resume instruction.
- Scoped aliases resolve exactly and fail loud on ambiguity.
- Completed/cancelled tasks are retained structurally but omitted from compact
  recall by default.
- `session_task_observe` v1 works without an LLM.
- Optional v2 judge is policy-controlled and never a hard dependency.
- Sub-agent recommendation hints are emitted when thresholds are crossed.
- Recall injection includes temporal-link recovery hints instead of raw history.
- Schema changes are delivered through ordered migrations.

## Deferred V2

- Optional LLM judge for task-shift classification.
- Confidence tuning and telemetry for `session_task_observe`.
- Richer UI surfaces for focus stack inspection and manual resume.
- Cross-session task summaries if users explicitly want durable semantic memory
  promotion.

## References

- [ADR-006 Session Task Continuity](../decisions/adr-006-session-task-continuity.md)
- [README](../README.md)
- [project-plan.md](../project-plan.md)
- [memory-lifecycle.md](../memory-lifecycle.md)
