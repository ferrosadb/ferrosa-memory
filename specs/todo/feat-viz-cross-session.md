---
type: feat
priority: P2
reported-by: user
implemented-by: ""
verified-by: ""
created: 2026-04-16
updated: 2026-04-16
source: ferrosa-memory skills-layer design session
source-location: "specs/skills-layer-design.md#viz-cross-session-support"
---

# Viz should visualize across sessions, not just one

## Problem

The viz snapshot builder at `crates/ferrosa-memory-core/src/http.rs:976` loads nodes via `entity_list_session(ctx, session_id)`, so the graph only shows entities for a single configured session. With the entity scope work (see `specs/skills-layer-design.md`), global entities (skills, code symbols, concepts) live outside any one session and become invisible to viz under this approach. Even without the scope work, users can't see relationships that cross session boundaries.

## Required Changes

### 1. Backend — viz snapshot endpoint supports scope

- Endpoint (currently returns a single-session snapshot) accepts a `scope` query param:
  - `scope=all` (default) — union of all entities for the tenant via `entity_list_all`
  - `scope=global` — global-scope entities only
  - `scope=session` — current behavior, scoped to the configured viz session
- When `scope=all`, implementation uses the existing `Storage::entity_list_all(ctx)` method. No new storage method needed.
- Continue to load folds and edges; edges already carry their own `session_id` and should not be filtered on traversal.

### 2. Backend — viz nodes include session/scope metadata

Each `VizNode` gets two new fields:

- `session_id: Uuid` — the session the entity belongs to (or the global sentinel for global entities)
- `is_global: bool` — convenience flag for frontend rendering

`viz::entity_to_viz_node` populates both from the `EntityEntry`.

### 3. Backend — `ExploreNeighborhood` crosses sessions

`viz::VizClientMessage::ExploreNeighborhood { entity_id, hops }` currently expects neighbors to share the session. Change the traversal to follow edges regardless of session. Test case: skill entity (global) referenced by a fold in session A and another fold in session B — exploring from the skill should return both folds.

### 4. Frontend — session filter UI

`crates/ferrosa-memory-core/assets/viz.html` additions:

- Multi-select dropdown populated from the distinct `session_id` values in the current snapshot, plus a "Global" option.
- Default: all sessions selected + global.
- Unselecting a session hides its nodes (client-side filter, not a new fetch).
- Per-node visual: small badge or colored border showing whether the node is global or which session it belongs to. Global gets a distinct color from the session palette.

### 5. Frontend — respect scope on load

Default load remains `?scope=all`. Expose a scope toggle in the UI sidebar for power users: All / Global only / Session only.

## Acceptance Criteria

- [ ] `GET /viz/api/snapshot?scope=all` returns entities from every session in the tenant.
- [ ] `GET /viz/api/snapshot?scope=global` returns only global-scope entities.
- [ ] `GET /viz/api/snapshot?scope=session` returns only the configured viz-session entities (current behavior preserved).
- [ ] Each viz node includes `session_id` and `is_global` fields.
- [ ] `ExploreNeighborhood` on a global entity returns neighbors from multiple sessions.
- [ ] UI renders a session filter and global toggle; selections update the displayed graph without a re-fetch.
- [ ] Global nodes are visually distinguishable from session nodes.

## Dependencies

- Entity scope work (Sprint 1 of `specs/skills-layer-design.md`) — this work item assumes `EntityEntry.scope` exists. If that ships later, `is_global` can default to `false` and the UI shows everything as session-scoped until scope lands.

## Out of Scope

- Cross-tenant viz (every query still filtered to the caller's tenant).
- Session deletion UI from within viz.
- Time-range filtering (show only entities created in the last N days) — separate work item.

## Implementation Notes

_To be filled in by implementer._
