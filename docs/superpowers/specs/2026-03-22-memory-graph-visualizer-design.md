# Memory Graph Visualizer — Design Spec

**Date:** 2026-03-22
**Status:** Draft

## Overview

A real-time force-directed graph visualization of the ferrosa-memory knowledge graph, embedded in the MCP server binary. Point a browser at a local port, see entities, folds, and edges as an interactive D3.js graph. Live updates stream via WebSocket as the memory system processes new content.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  ferrosa-memory-mcp binary                          │
│                                                     │
│  ┌──────────┐   ┌──────────┐   ┌────────────────┐  │
│  │ stdio    │   │ HTTP     │   │ Viz Server     │  │
│  │ transport│   │ transport│   │ GET /viz        │  │
│  │ (MCP)    │   │ (MCP)    │   │ GET /viz/ws    │  │
│  └────┬─────┘   └────┬─────┘   └───────┬────────┘  │
│       │              │                  │           │
│       └──────┬───────┘                  │           │
│              │                          │           │
│       ┌──────▼──────┐   broadcast   ┌───▼────────┐  │
│       │ dispatch()  │──────────────▶│ EventBus   │  │
│       │ handlers    │   (tokio      │ (broadcast) │  │
│       └──────┬──────┘    channel)   └────────────┘  │
│              │                                      │
│       ┌──────▼──────┐                               │
│       │ CQL Storage │                               │
│       │ Graph Client│                               │
│       └─────────────┘                               │
└─────────────────────────────────────────────────────┘
         │                              │
         ▼                              ▼
   ┌──────────┐                  ┌──────────────┐
   │ Ferrosa  │                  │  Browser     │
   │ Cluster  │                  │  D3.js graph │
   └──────────┘                  └──────────────┘
```

### Data Flow

1. **Initial load**: Browser opens `/viz`, loads static HTML+JS+CSS. JS connects to `/viz/ws` WebSocket. On connect, server sends full graph snapshot (all entities + edges for the tenant).

2. **Live updates**: When any MCP tool handler mutates state (smart_ingest, upsert_entity, complete_fold, write_temporal_fact, run_consolidation), it emits a typed event to a `tokio::sync::broadcast` channel. The WebSocket handler forwards these events to all connected browsers.

3. **User interaction**: Click a node in the browser to see its detail panel (entity metadata, edges, temporal chain). The detail data is included in the initial snapshot — no extra round trips.

### Event Types

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum VizEvent {
    /// Full graph snapshot (sent on WebSocket connect)
    Snapshot { nodes: Vec<VizNode>, edges: Vec<VizEdge> },
    /// New entity created or updated
    EntityChanged { node: VizNode, action: String },
    /// New edge created
    EdgeCreated { edge: VizEdge },
    /// Entity state changed (promote/demote)
    StateChanged { entity_id: Uuid, new_state: String },
    /// Temporal fact updated
    FactUpdated { entity_id: Uuid, fact_text: String, superseded: Option<Uuid> },
    /// Fold completed (may link entities)
    FoldCompleted { fold_id: Uuid, summary: String, entity_count: usize },
}
```

### Node and Edge Types

```rust
#[derive(Debug, Clone, Serialize)]
pub struct VizNode {
    pub id: String,          // entity_id or fold_id
    pub label: String,       // entity_name or fold summary prefix
    pub node_type: String,   // "entity" or "fold"
    pub entity_type: String, // "concept", "person", "decision", etc.
    pub state: String,       // "active", "dormant", "silent"
    pub confidence: f64,
    pub created_at: String,
    pub context: String,     // context_snippet for detail panel
}

#[derive(Debug, Clone, Serialize)]
pub struct VizEdge {
    pub source: String,     // source node id
    pub target: String,     // target node id
    pub edge_type: String,  // "CO_OCCURS", "MENTIONED_IN", "FOLDED_INTO", "SUPERSEDES"
}
```

## Frontend

### Technology

- **D3.js v7** force simulation — loaded from CDN (`d3js.org`)
- **Single HTML file** with inline CSS and JS — served as a static string from the Rust binary (no build step, no node_modules)
- **Inter font** from Google Fonts (Ferrosa brand)

### Layout: Graph + Detail Panel

- Full-viewport dark background (`#0a0a0f` Void)
- Top header bar: Fe logo mark, "memory" label, live stats (entity count, edge count, LIVE indicator)
- Main area: SVG force-directed graph (left), collapsible detail panel (right, ~200px)
- Detail panel shows on node click: name, type, state, confidence, created time, edge list, temporal chain

### Color Mapping (Ferrosa Brand)

Entity types → node fill colors:
| Type | Color | Hex |
|------|-------|-----|
| concept | Steel Blue | `#7c9cf5` |
| person | Amethyst | `#c882f0` |
| decision | Terracotta | `#e2725b` |
| pattern | Amber | `#f4a261` |
| org | Copper | `#d4a574` |
| event | Verdigris | `#6bc9a0` |
| place | Steel Blue (light) | `#9cb8f7` |
| preference | Amber (light) | `#f7be7e` |
| fold | Charcoal with border | `#16161f` stroke `#1e1e2a` |

Edge types → line colors:
| Type | Color | Hex |
|------|-------|-----|
| CO_OCCURS | Copper | `#d4a574` |
| MENTIONED_IN | Steel Blue | `#7c9cf5` |
| FOLDED_INTO | Amethyst | `#c882f0` |
| SUPERSEDES | Rust Red | `#e25b5b` |

### Force Simulation Parameters

```javascript
const simulation = d3.forceSimulation(nodes)
    .force("link", d3.forceLink(edges).id(d => d.id).distance(80))
    .force("charge", d3.forceManyBody().strength(-200))
    .force("center", d3.forceCenter(width / 2, height / 2))
    .force("collision", d3.forceCollide().radius(20));
```

### Animations

- **New node**: Fade in from opacity 0 → 1 over 500ms, slight scale pulse
- **New edge**: Stroke-dasharray animation (drawing effect) over 300ms
- **State change**: Brief glow ring around affected node (Terracotta pulse)
- **Supersession**: Old edge fades to 0.2 opacity, new edge draws in

### Interaction

- **Click node**: Highlight node + connected edges, show detail panel
- **Drag node**: Pin/unpin from simulation
- **Zoom/pan**: D3 zoom behavior on the SVG
- **Hover**: Tooltip with entity name + type

## Backend

### Server Component

Embedded in the existing HTTP transport. New routes added to `http.rs`:

- `GET /viz` — returns static HTML page (compiled into binary via `include_str!`)
- `GET /viz/ws` — WebSocket upgrade, subscribes to event broadcast channel

### EventBus

```rust
pub struct EventBus {
    tx: tokio::sync::broadcast::Sender<VizEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(256);
        Self { tx }
    }

    pub fn emit(&self, event: VizEvent) {
        let _ = self.tx.send(event); // ignore if no receivers
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<VizEvent> {
        self.tx.subscribe()
    }
}
```

Shared via `Arc<EventBus>` in `SessionState`. Handlers call `event_bus.emit()` after successful mutations.

### Graph Snapshot

On WebSocket connect, query CQL for all entities and edges for the tenant, then send a `Snapshot` event. This reuses existing Storage trait methods:
- `entity_list_session()` (added in Task 6)
- Edge data via graph client `find_related_entities()` or direct CQL query

### WebSocket Handler

Uses `tokio-tungstenite` for WebSocket support (lightweight, async-native). The handler:
1. Accepts upgrade
2. Sends initial Snapshot
3. Loops on broadcast receiver, forwarding VizEvents as JSON text frames
4. Cleans up on disconnect

### Static Assets

The HTML/CSS/JS dashboard is a single file compiled into the binary:

```rust
const VIZ_HTML: &str = include_str!("../../assets/viz.html");
```

No external build tools. D3.js loaded from CDN. Inter font from Google Fonts.

## File Structure

| File | Responsibility |
|------|---------------|
| `crates/ferrosa-core/src/viz.rs` | VizEvent, VizNode, VizEdge types, EventBus |
| `crates/ferrosa-core/src/http.rs` | Add `/viz` and `/viz/ws` routes |
| `assets/viz.html` | Single-file HTML+CSS+JS dashboard |
| `crates/ferrosa-core/src/dispatch.rs` | Emit VizEvents from handlers |

## Dependencies

New crate dependencies:
- `tokio-tungstenite` — WebSocket support (already using tokio)
- `base64` (already in deps for graph client auth)

No new external dependencies for the frontend (D3 from CDN).

## Configuration

```toml
[viz]
enabled = true        # default: true when HTTP transport active
port = 8766           # default: http_port + 1, or same port with path routing
```

If the server is in stdio mode (Claude Code), the viz server starts on a separate port (default 8766). If in HTTP mode, viz routes are added to the same HTTP server.

## Scope Boundaries

### In Scope (v1)
- Force-directed graph with D3.js
- WebSocket live updates from MCP handlers
- Node click → detail panel (metadata, edges, temporal chain)
- Drag to pin, zoom/pan
- Entity type color coding (Ferrosa brand)
- Edge type color coding
- New node/edge animations
- Stats in header (entity count, edge count)

### Out of Scope (future)
- Sidebar filters (entity type, state) — can add later
- Timeline view
- Search within the graph
- Multiple tenant support in viz
- 3D rendering
- Persistence of graph layout positions
- Authentication on viz endpoint (local dev tool only)

## Testing

- Unit tests for EventBus (emit, subscribe, no-receiver case)
- Unit tests for VizNode/VizEdge serialization
- Integration test: emit event → verify WebSocket client receives it
- Manual: open browser, create entities via MCP, verify they appear
