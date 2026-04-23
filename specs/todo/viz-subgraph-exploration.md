---
type: feat
priority: P2
status: draft
created: 2026-04-06
updated: 2026-04-20
---

# Viz: Subgraph Exploration Mode

## Goal

Allow the user to explore a subset of the knowledge graph at full detail — seeing individual entities, their edges, labels, and types — without rendering the entire 20K+ node graph.

Current full-render works at ~2K nodes. Once the data loss bug is fixed and we have 15K+ entities, we need both:
1. **Overview mode** (hierarchical clustering, crate-level) — already implemented
2. **Exploration mode** — select a starting entity or cluster and see its neighborhood at full detail

## Exploration Modes

### 1. Cluster drill-down (already built, needs re-enabling)
- Top level: crate clusters (~30 nodes)
- Double-click: expand to modules
- Double-click: expand to functions
- Breadcrumb navigation back up

### 2. Entity neighborhood explorer (new)
- Search for an entity → show it + N hops of connected entities
- Slider: 1-hop, 2-hop, 3-hop radius
- Only renders the local neighborhood (50-200 nodes), not the full graph
- Good for: "show me everything related to the SPARQL endpoint"

### 3. Type filter view (partially built)
- Filter by entity_type: show only documents + people (paper graph)
- Filter by edge_type: show only `calls` edges (call graph)
- Preset buttons: "Code Structure", "Research", "Decisions", "Call Graph"

### 4. Path explorer
- Select two entities → highlight the shortest path between them
- Uses `find_memory_chain` from the MCP server
- Good for: "how does this bug relate to that decision?"

## Implementation Notes

- The hierarchical clustering code is in `cluster_snapshot()` in http.rs — currently bypassed for full render
- The frontend drill-down JS is in viz.html — `drillInto()`, `sendDrillDown()`, breadcrumbs
- Re-enable clustering behind a toggle: "Overview / Detail" switch in the header
- The neighborhood explorer would need a new WebSocket message type: `{"type": "explore", "entity_id": "...", "hops": 2}`
- The server filters the flat snapshot to the requested neighborhood and sends it

## Priority

Blocked by: ferrosa P0 data loss bug (need stable data to test with)
Depends on: hierarchical clustering (already built), entity search in viz (partially built)
