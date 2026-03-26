# Vestige Parity: Cognitive Memory Tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire built-but-unexposed modules (smart_ingest, intentions, temporal, graph) into MCP tools, add new vestige-inspired features (dream/consolidation, hybrid search, memory promotion/demotion, co-occurrence edges), and make the knowledge graph build itself automatically from conversation content.

**Architecture:** New MCP tools follow the existing dispatch pattern — `ToolDef` in `tool_definitions()`, match arm in `dispatch_tool()`, handler function calling domain module. Dream consolidation runs as a background task triggered by MCP tool. IntentionStore gains CQL persistence via the Storage trait. The `smart_ingest` tool becomes the primary write path, replacing manual `upsert_entity` for most use cases.

**Tech Stack:** Rust, Tokio, serde_json, cdrs-tokio (CQL), HTTP Cypher (graph reads), Ollama (embeddings)

---

## File Structure

| File | Responsibility | Status |
|------|---------------|--------|
| `crates/ferrosa-memory-core/src/dispatch.rs` | MCP tool registry + dispatch + handlers | Modify: add 12 new tools + handlers |
| `crates/ferrosa-memory-core/src/smart_ingest.rs` | Prediction error gating | Modify: add `source_fold_id` param |
| `crates/ferrosa-memory-core/src/intention.rs` | Prospective memory store | Modify: add CQL persistence methods |
| `crates/ferrosa-memory-core/src/temporal.rs` | Temporal fact chains | No change (already complete) |
| `crates/ferrosa-memory-core/src/graph.rs` | Cypher graph traversal client | Modify: add `explore_connections` |
| `crates/ferrosa-memory-core/src/dream.rs` | **New:** Consolidation/dream engine | Create |
| `crates/ferrosa-memory-core/src/hybrid_search.rs` | **New:** Multi-strategy search with RRF | Create |
| `crates/ferrosa-memory-core/src/storage.rs` | Storage trait | Modify: add 6 new trait methods |
| `crates/ferrosa-memory-core/src/storage.rs` (inline `mod mock`) | Mock storage | Modify: implement new trait methods |
| `crates/ferrosa-memory-core/src/cql_storage.rs` | CQL storage impl | Modify: implement new trait methods |
| `crates/ferrosa-memory-core/src/types.rs` | Domain types | Modify: add new types |
| `crates/ferrosa-memory-core/src/lib.rs` | Module declarations | Modify: add `dream`, `hybrid_search` |
| `crates/ferrosa-memory-core/src/entity.rs` | Entity upsert/retrieve | Modify: add co-occurrence edge creation |
| `ddl/006_intentions.cql` | **New:** Intentions table DDL | Create |

---

## Task 1: Wire `smart_ingest` MCP Tool

**Files:**
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`
- Modify: `crates/ferrosa-memory-core/src/smart_ingest.rs`

This is the highest-value tool — it replaces manual entity creation with intelligent CREATE/UPDATE/SUPERSEDE/SKIP decisions.

- [ ] **Step 1: Write the failing test**

In `dispatch.rs`, add to the `#[cfg(test)] mod tests` block:

```rust
#[tokio::test]
async fn smart_ingest_creates_on_new_content() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let sid = Uuid::new_v4();

    let params = serde_json::json!({
        "name": "smart_ingest",
        "arguments": {
            "session_id": sid.to_string(),
            "content": "Ferrosa uses LSM-tree storage with S3 tiering",
            "entity_type": "concept"
        }
    });
    let result = dispatch("tools/call", params, &store, &ctx)
        .await
        .unwrap();
    assert_eq!(result["action"], "Created");
    assert!(result["entity_id"].is_string());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib -p ferrosa-memory-core smart_ingest_creates_on_new_content`
Expected: FAIL — no match arm for "smart_ingest" in dispatch_tool

- [ ] **Step 3: Add ToolDef for smart_ingest**

In `tool_definitions()` in `dispatch.rs`, after the `delete_session` ToolDef, add:

```rust
// --- Cognitive memory tools ---
ToolDef {
    name: "smart_ingest".into(),
    description: "Intelligently ingests content by comparing against existing memories. Uses prediction error gating to decide: CREATE (novel), UPDATE (similar topic), SUPERSEDE (contradicts existing), or SKIP (redundant).\n\nCALL WHEN: You learn something new that should be remembered — facts, decisions, patterns, preferences. This is the primary write path for building the knowledge graph.\nDO NOT CALL: For ephemeral conversation or task-specific state. Use plan tools for task state.\nRETURNS: The action taken and affected entity_id(s).\nCost: ~15ms (includes similarity search).".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string", "format": "uuid" },
            "content": { "type": "string", "maxLength": 8192, "description": "The content to ingest" },
            "entity_type": { "type": "string", "enum": ["person", "place", "event", "concept", "org", "decision", "pattern", "preference"] },
            "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding vector" },
            "source_fold_id": { "type": "string", "format": "uuid", "description": "Fold that produced this content" }
        },
        "required": ["session_id", "content", "entity_type"]
    }),
},
```

- [ ] **Step 4: Update smart_ingest module to accept source_fold_id**

In `smart_ingest.rs`, update the `smart_ingest()` function signature to add `source_fold_id: Option<Uuid>` parameter after `embedding`:

```rust
pub async fn smart_ingest(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    content: &str,
    entity_type: &str,
    embedding: Option<&[f32]>,
    source_fold_id: Option<Uuid>,  // NEW
    config: &IngestConfig,
) -> anyhow::Result<IngestDecision> {
```

Update all 3 `EntityEntry` constructors (lines 94, 152, 188) to use `source_fold_id` instead of `None`.

Also update the existing test `smart_ingest_creates_on_empty_store` (line 257) to pass the new arg:
```rust
let result = smart_ingest(
    &store, &ctx, Uuid::new_v4(),
    "Ferrosa is a Rust-native Cassandra-compatible database",
    "concept", None, None, &IngestConfig::default(),
).await.unwrap();
```

- [ ] **Step 5: Add handler function and match arm**

Add match arm in `dispatch_tool()`:
```rust
"smart_ingest" => handle_smart_ingest(args, storage, ctx).await,
```

Add handler:
```rust
async fn handle_smart_ingest<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let content = require_str(&args, "content")?;
    let entity_type = require_str(&args, "entity_type")?;
    let embedding = optional_f32_array(&args, "embedding")?;
    let source_fold_id = optional_uuid(&args, "source_fold_id")?;

    let config = crate::smart_ingest::IngestConfig::default();
    let decision = crate::smart_ingest::smart_ingest(
        storage,
        ctx,
        session_id,
        content,
        entity_type,
        embedding.as_deref(),
        source_fold_id,
        &config,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(decision).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}
```

- [ ] **Step 6: Update tool count assertion**

In `dispatch.rs` test `tools_list_returns_all_tools`, update:
```rust
assert_eq!(tools.len(), 14); // was 13
```

- [ ] **Step 7: Run tests and verify pass**

Run: `cargo test --lib -p ferrosa-memory-core -- dispatch`
Expected: All dispatch tests pass including the new one.

- [ ] **Step 8: Commit**

```bash
git add crates/ferrosa-memory-core/src/dispatch.rs crates/ferrosa-memory-core/src/smart_ingest.rs
git commit -m "feat: wire smart_ingest MCP tool for prediction error gating"
```

---

## Task 2: Wire Intention Tools (set, check, complete, list, snooze)

**Files:**
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`
- Modify: `crates/ferrosa-memory-core/src/intention.rs`

IntentionStore is currently in-memory. For now we wire it through dispatch using per-session state held in the transport layer. CQL persistence is Task 9.

The dispatch function currently takes `storage` and `ctx`. Intentions need session-scoped mutable state. We'll pass an `Arc<Mutex<IntentionStore>>` through a new `SessionState` struct.

- [ ] **Step 1: Add SessionState to dispatch.rs** (not types.rs — avoids coupling types→intention)

At the top of `dispatch.rs`, add:

```rust
use std::sync::Arc;
use tokio::sync::Mutex;

/// Per-session mutable state (not persisted in CQL).
pub struct SessionState {
    pub intentions: Arc<Mutex<crate::intention::IntentionStore>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            intentions: Arc::new(Mutex::new(
                crate::intention::IntentionStore::new(),
            )),
        }
    }
}
```

- [ ] **Step 2: Update dispatch signature to accept SessionState**

Update `dispatch()` and `dispatch_tool()` signatures:
```rust
pub async fn dispatch<S: crate::storage::Storage>(
    method: &str,
    params: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)>
```

**All callers that must be updated** (pass `&SessionState::default()` or a shared instance):
1. `crates/ferrosa-memory-core/src/http.rs:125` — `dispatch::dispatch(rpc_method, params, storage, &ctx)` → add session param
2. `crates/ferrosa-memory-mcp/src/main.rs:420` — `dispatch::dispatch(&method, params, storage.as_ref(), ctx.as_ref())` → add session param. Create `SessionState` in the session setup and pass `Arc` clone.
3. `crates/ferrosa-memory-core/src/security_tests.rs:48,59,118` — 3 call sites, add `&dispatch::SessionState::default()`
4. `crates/ferrosa-memory-core/src/dispatch.rs` tests — all `dispatch(...)` calls in the `#[cfg(test)]` block (lines 716, 727, 745, 756, 770, 791, 805, 828, 840). Add `&SessionState::default()` to each.

For `main.rs`, create the session state once per connection:
```rust
let session = Arc::new(dispatch::SessionState::default());
```

- [ ] **Step 3: Write failing tests for intention tools**

```rust
#[tokio::test]
async fn set_intention_and_check() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();

    // Set intention
    let params = serde_json::json!({
        "name": "set_intention",
        "arguments": {
            "description": "Review auth error handling",
            "trigger": { "type": "Topic", "keywords": ["auth", "authentication"] },
            "priority": "high"
        }
    });
    let result = dispatch("tools/call", params, &store, &ctx, &session)
        .await
        .unwrap();
    assert!(result["intention_id"].is_string());

    // Check — no match
    let params = serde_json::json!({
        "name": "check_intentions",
        "arguments": { "context": "working on database" }
    });
    let result = dispatch("tools/call", params, &store, &ctx, &session)
        .await
        .unwrap();
    assert_eq!(result["triggered"].as_array().unwrap().len(), 0);

    // Check — match
    let params = serde_json::json!({
        "name": "check_intentions",
        "arguments": { "context": "now looking at auth middleware" }
    });
    let result = dispatch("tools/call", params, &store, &ctx, &session)
        .await
        .unwrap();
    assert_eq!(result["triggered"].as_array().unwrap().len(), 1);
}
```

- [ ] **Step 4: Add 5 ToolDefs for intention tools**

```rust
ToolDef {
    name: "set_intention".into(),
    description: "Sets a prospective memory — 'remember to do X when Y happens.' Triggers automatically when context matches.\n\nCALL WHEN: You identify a deferred action — something to check, review, or do when a specific topic, file, or condition comes up later.\nCost: ~1ms.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "description": { "type": "string", "maxLength": 1024 },
            "trigger": {
                "type": "object",
                "properties": {
                    "type": { "type": "string", "enum": ["Topic", "FilePattern", "Duration", "Context"] },
                    "keywords": { "type": "array", "items": { "type": "string" } },
                    "pattern": { "type": "string" },
                    "minutes": { "type": "integer", "minimum": 1 },
                    "condition": { "type": "string" }
                },
                "required": ["type"]
            },
            "priority": { "type": "string", "enum": ["low", "normal", "high", "critical"] }
        },
        "required": ["description", "trigger"]
    }),
},
ToolDef {
    name: "check_intentions".into(),
    description: "Checks if any pending intentions should trigger based on current context. Returns triggered and pending intentions.\n\nCALL WHEN: At the start of each new task or topic change. Also called automatically by the server on context switches.\nCost: ~1ms.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "context": { "type": "string", "maxLength": 4096, "description": "Current context for matching" }
        },
        "required": ["context"]
    }),
},
ToolDef {
    name: "complete_intention".into(),
    description: "Marks a triggered intention as completed.\n\nCALL WHEN: After acting on a triggered intention.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "intention_id": { "type": "string", "format": "uuid" }
        },
        "required": ["intention_id"]
    }),
},
ToolDef {
    name: "list_intentions".into(),
    description: "Lists all intentions for the current session.\n\nCALL WHEN: Need to review what intentions are pending, triggered, or completed.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {}
    }),
},
ToolDef {
    name: "snooze_intention".into(),
    description: "Snoozes a triggered intention for later.\n\nCALL WHEN: An intention triggered but now is not the right time to act on it.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "intention_id": { "type": "string", "format": "uuid" }
        },
        "required": ["intention_id"]
    }),
},
```

- [ ] **Step 5: Add handler functions and match arms**

Add match arms in `dispatch_tool()`:
```rust
"set_intention" => handle_set_intention(args, session).await,
"check_intentions" => handle_check_intentions(args, session).await,
"complete_intention" => handle_complete_intention(args, session).await,
"list_intentions" => handle_list_intentions(session).await,
"snooze_intention" => handle_snooze_intention(args, session).await,
```

Add handlers:
```rust
async fn handle_set_intention(
    args: Value,
    session: &crate::types::SessionState,
) -> Result<Value, (i32, String)> {
    let description = require_str(&args, "description")?;
    let trigger_json = args.get("trigger")
        .ok_or((INVALID_PARAMS, "missing trigger".into()))?;
    let trigger: crate::intention::IntentionTrigger =
        serde_json::from_value(trigger_json.clone())
            .map_err(|e| (INVALID_PARAMS, format!("invalid trigger: {e}")))?;
    let priority_str = args.get("priority").and_then(|v| v.as_str()).unwrap_or("normal");
    let priority: crate::intention::Priority =
        serde_json::from_value(Value::String(priority_str.into()))
            .map_err(|e| (INVALID_PARAMS, format!("invalid priority: {e}")))?;

    let mut store = session.intentions.lock().await;
    let id = store.set(description, trigger, priority);
    Ok(serde_json::json!({ "intention_id": id.to_string() }))
}

async fn handle_check_intentions(
    args: Value,
    session: &crate::types::SessionState,
) -> Result<Value, (i32, String)> {
    let context = require_str(&args, "context")?;
    let mut store = session.intentions.lock().await;
    let triggered = store.check(context);
    let triggered_json: Vec<Value> = triggered.iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();
    let pending = store.pending();
    let pending_json: Vec<Value> = pending.iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();
    Ok(serde_json::json!({ "triggered": triggered_json, "pending": pending_json }))
}

async fn handle_complete_intention(
    args: Value,
    session: &crate::types::SessionState,
) -> Result<Value, (i32, String)> {
    let id = require_uuid(&args, "intention_id")?;
    let mut store = session.intentions.lock().await;
    let completed = store.complete(id);
    Ok(serde_json::json!({ "completed": completed }))
}

async fn handle_list_intentions(
    session: &crate::types::SessionState,
) -> Result<Value, (i32, String)> {
    let store = session.intentions.lock().await;
    let all: Vec<Value> = store.list().iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();
    Ok(serde_json::json!({ "intentions": all }))
}

async fn handle_snooze_intention(
    args: Value,
    session: &crate::types::SessionState,
) -> Result<Value, (i32, String)> {
    let id = require_uuid(&args, "intention_id")?;
    let mut store = session.intentions.lock().await;
    let snoozed = store.snooze(id);
    Ok(serde_json::json!({ "snoozed": snoozed }))
}
```

- [ ] **Step 6: Add snooze method to IntentionStore**

In `intention.rs`, add:
```rust
/// Snooze a triggered intention — resets to Pending so it can trigger again later.
pub fn snooze(&mut self, id: Uuid) -> bool {
    if let Some(i) = self.intentions.iter_mut().find(|i| i.id == id) {
        if i.status == IntentionStatus::Triggered {
            i.status = IntentionStatus::Pending;
            i.triggered_at = None;
            true
        } else {
            false
        }
    } else {
        false
    }
}
```

- [ ] **Step 7: Update tool count and run all tests**

Update `assert_eq!(tools.len(), 19);` (13 original + 1 smart_ingest + 5 intention = 19).

Run: `cargo test --lib -p ferrosa-memory-core`
Expected: All tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/ferrosa-memory-core/src/dispatch.rs crates/ferrosa-memory-core/src/intention.rs crates/ferrosa-memory-core/src/types.rs
git commit -m "feat: wire intention MCP tools (set, check, complete, list, snooze)"
```

---

## Task 3: Wire Temporal Fact Tools

**Files:**
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`

The temporal module is already complete. We just need MCP tool wrappers.

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn temporal_fact_round_trip() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();
    let sid = Uuid::new_v4();
    let entity_id = Uuid::new_v4();

    let params = serde_json::json!({
        "name": "write_temporal_fact",
        "arguments": {
            "session_id": sid.to_string(),
            "entity_id": entity_id.to_string(),
            "fact_text": "Alice is VP of Engineering",
            "confidence": 0.95
        }
    });
    let result = dispatch("tools/call", params, &store, &ctx, &session)
        .await
        .unwrap();
    assert!(result["event_id"].is_string());
}
```

- [ ] **Step 2: Add 2 ToolDefs**

```rust
ToolDef {
    name: "write_temporal_fact".into(),
    description: "Records a timestamped fact about an entity. Automatically supersedes the previous current fact for that entity.\n\nCALL WHEN: You learn a new fact about an entity that may change over time (role, status, location, preference). Links to the prior fact via SUPERSEDES graph edge.\nCost: ~10ms.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string", "format": "uuid" },
            "entity_id": { "type": "string", "format": "uuid" },
            "fact_text": { "type": "string", "maxLength": 4096 },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "required": ["session_id", "entity_id", "fact_text"]
    }),
},
ToolDef {
    name: "get_temporal_chain".into(),
    description: "Returns the current (most recent valid) fact for an entity, plus the supersession chain if requested.\n\nCALL WHEN: Need to know the current state of a temporal fact, or trace how it evolved.\nCost: ~5ms.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string", "format": "uuid" },
            "entity_id": { "type": "string", "format": "uuid" }
        },
        "required": ["session_id", "entity_id"]
    }),
},
```

- [ ] **Step 3: Add handlers and match arms**

```rust
"write_temporal_fact" => handle_write_temporal_fact(args, storage, ctx).await,
"get_temporal_chain" => handle_get_temporal_chain(args, storage, ctx).await,
```

```rust
async fn handle_write_temporal_fact<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let entity_id = require_uuid(&args, "entity_id")?;
    let fact_text = require_str(&args, "fact_text")?;
    let confidence = args.get("confidence").and_then(|v| v.as_f64()).unwrap_or(1.0);

    let event_id = crate::temporal::write_temporal_fact(
        storage, ctx, entity_id, fact_text, session_id, confidence,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "event_id": event_id.to_string() }))
}

async fn handle_get_temporal_chain<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let _session_id = require_uuid(&args, "session_id")?;
    let entity_id = require_uuid(&args, "entity_id")?;

    let current = crate::temporal::get_current_fact(storage, ctx, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    match current {
        Some(fact) => serde_json::to_value(&fact).map_err(|e| (INTERNAL_ERROR, e.to_string())),
        None => Ok(serde_json::json!({ "fact": null })),
    }
}
```

- [ ] **Step 4: Update tool count, run tests, commit**

Update count to 21 (19 + 2 temporal). Run: `cargo test --lib -p ferrosa-memory-core`

```bash
git add crates/ferrosa-memory-core/src/dispatch.rs
git commit -m "feat: wire temporal fact MCP tools (write, get_chain)"
```

---

## Task 4: Wire Graph Traversal Tool

**Files:**
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`
- Modify: `crates/ferrosa-memory-core/src/graph.rs`

Expose the graph client's traversal functions as a single `explore_connections` MCP tool that supports multiple traversal types.

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn explore_connections_dispatch() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();

    let params = serde_json::json!({
        "name": "explore_connections",
        "arguments": {
            "entity_id": Uuid::new_v4().to_string(),
            "traversal": "related_entities",
            "max_depth": 2,
            "limit": 10
        }
    });
    // This will fail because graph client isn't available in tests,
    // but we verify dispatch routing works.
    let result = dispatch("tools/call", params, &store, &ctx, &session).await;
    // In test mode, graph client is None, so we expect an error
    assert!(result.is_err());
}
```

- [ ] **Step 2: Add ToolDef**

```rust
ToolDef {
    name: "explore_connections".into(),
    description: "Traverses the knowledge graph from an entity or fold. Supports: fold_ancestors (FOLDED_INTO), related_entities (CO_OCCURS_WITH), entities_in_fold (MENTIONED_IN), supersession_chain (SUPERSEDES).\n\nCALL WHEN: Need to understand relationships between entities or folds. Use after retrieving entities to discover connections.\nCost: ~20ms per hop.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "entity_id": { "type": "string", "format": "uuid" },
            "fold_id": { "type": "string", "format": "uuid" },
            "traversal": { "type": "string", "enum": ["fold_ancestors", "related_entities", "entities_in_fold", "supersession_chain"] },
            "max_depth": { "type": "integer", "minimum": 1, "maximum": 5 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
        },
        "required": ["traversal"]
    }),
},
```

- [ ] **Step 3: Add GraphClient to SessionState and handler**

Add `pub graph: Option<Arc<crate::graph::GraphClient>>` to `SessionState` in `dispatch.rs`. Update `SessionState::default()` to set `graph: None`.

In `main.rs`, initialize the graph client from config and set it on SessionState:
```rust
let graph = match config.graph {
    Some(ref g) => Some(Arc::new(GraphClient::connect(g.into()).await)),
    None => None,
};
let session = Arc::new(dispatch::SessionState {
    intentions: Arc::new(Mutex::new(IntentionStore::new())),
    graph,
});
```

Add handler that reads graph client from session:
```rust
async fn handle_explore_connections(
    args: Value,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let graph = session.graph.as_ref()
        .ok_or((INTERNAL_ERROR, "graph client not configured".into()))?;
    let traversal = require_str(&args, "traversal")?;
    let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    match traversal {
        "fold_ancestors" => {
            let fold_id = require_uuid(&args, "fold_id")?;
            let results = graph.get_fold_ancestors(&fold_id.to_string(), max_depth).await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({ "results": results }))
        }
        "related_entities" => {
            let entity_id = require_uuid(&args, "entity_id")?;
            let results = graph.find_related_entities(&entity_id.to_string(), max_depth, limit).await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({ "results": results }))
        }
        "entities_in_fold" => {
            let fold_id = require_uuid(&args, "fold_id")?;
            let results = graph.get_entities_in_fold(&fold_id.to_string()).await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({ "results": results }))
        }
        "supersession_chain" => {
            let entity_id = require_uuid(&args, "entity_id")?;
            let results = graph.get_supersession_chain(&entity_id.to_string()).await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({ "results": results }))
        }
        _ => Err((INVALID_PARAMS, format!("unknown traversal: {traversal}"))),
    }
}
```

If no graph client is configured, the tool returns a clear error rather than silently failing.

- [ ] **Step 4: Run tests, commit**

```bash
git add crates/ferrosa-memory-core/src/dispatch.rs crates/ferrosa-memory-core/src/graph.rs
git commit -m "feat: wire explore_connections MCP tool for graph traversal"
```

---

## Task 5: Add Hybrid Search Tool

**Files:**
- Create: `crates/ferrosa-memory-core/src/hybrid_search.rs`
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`

Inspired by vestige's 7-stage search pipeline. Combines entity phonetic search + fold ANN search + entity ANN search using Reciprocal Rank Fusion (RRF).

- [ ] **Step 1: Create hybrid_search.rs with RRF merge**

```rust
//! Hybrid search — multi-strategy retrieval with Reciprocal Rank Fusion.

use serde::Serialize;
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

/// A unified search result from any source.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub id: Uuid,
    pub source: String,       // "entity_phonetic", "entity_ann", "fold_ann"
    pub content: String,
    pub score: f64,
    pub result_type: String,  // "entity" or "fold"
}

/// Reciprocal Rank Fusion: merge ranked lists.
fn rrf_merge(lists: Vec<Vec<SearchResult>>, k: f64) -> Vec<SearchResult> {
    use std::collections::HashMap;
    let mut scores: HashMap<Uuid, (f64, SearchResult)> = HashMap::new();
    for list in &lists {
        for (rank, item) in list.iter().enumerate() {
            let rrf_score = 1.0 / (k + rank as f64 + 1.0);
            scores
                .entry(item.id)
                .and_modify(|(s, _)| *s += rrf_score)
                .or_insert((rrf_score, item.clone()));
        }
    }
    let mut merged: Vec<SearchResult> = scores
        .into_values()
        .map(|(score, mut r)| { r.score = score; r })
        .collect();
    merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    merged
}

/// Multi-strategy hybrid search.
pub async fn hybrid_search(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query: &str,
    embedding: Option<&[f32]>,
    limit: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    let mut lists = Vec::new();

    // Strategy 1: Phonetic entity search
    if let Ok(Some(entity)) = storage.entity_find_phonetic(ctx, session_id, query).await {
        lists.push(vec![SearchResult {
            id: entity.entity_id,
            source: "entity_phonetic".into(),
            content: entity.context_snippet.clone(),
            score: 1.0,
            result_type: "entity".into(),
        }]);
    }

    // Strategy 2: ANN entity search
    if let Some(emb) = embedding {
        if let Ok(entities) = storage.entity_search_ann(ctx, session_id, emb, limit).await {
            lists.push(entities.into_iter().map(|e| SearchResult {
                id: e.entity_id,
                source: "entity_ann".into(),
                content: e.context_snippet,
                score: 1.0,
                result_type: "entity".into(),
            }).collect());
        }
    }

    // Strategy 3: ANN fold search
    if let Some(emb) = embedding {
        if let Ok(folds) = storage.fold_search(ctx, session_id, emb, limit, false).await {
            lists.push(folds.into_iter().map(|f| SearchResult {
                id: f.fold_id,
                source: "fold_ann".into(),
                content: f.fold_summary,
                score: f.similarity.unwrap_or(0.0),
                result_type: "fold".into(),
            }).collect());
        }
    }

    let merged = rrf_merge(lists, 60.0);
    Ok(merged.into_iter().take(limit).collect())
}
```

- [ ] **Step 2: Add module declaration to lib.rs**

```rust
pub mod hybrid_search;
```

- [ ] **Step 3: Write unit tests for RRF merge**

In `hybrid_search.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_merge_deduplicates_and_ranks() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let list1 = vec![
            SearchResult { id: id1, source: "a".into(), content: "x".into(), score: 1.0, result_type: "entity".into() },
            SearchResult { id: id2, source: "a".into(), content: "y".into(), score: 0.9, result_type: "entity".into() },
        ];
        let list2 = vec![
            SearchResult { id: id2, source: "b".into(), content: "y".into(), score: 1.0, result_type: "entity".into() },
            SearchResult { id: id1, source: "b".into(), content: "x".into(), score: 0.8, result_type: "entity".into() },
        ];
        let merged = rrf_merge(vec![list1, list2], 60.0);
        // Both ids present, id2 should rank higher (rank 0 in list2 + rank 1 in list1)
        assert_eq!(merged.len(), 2);
    }
}
```

- [ ] **Step 4: Add ToolDef and handler for hybrid_search**

```rust
ToolDef {
    name: "hybrid_search".into(),
    description: "Multi-strategy search across entities and folds using Reciprocal Rank Fusion. Combines phonetic, ANN entity, and ANN fold search.\n\nCALL WHEN: Starting a new task and want comprehensive context from prior work. More thorough than retrieve_entities or retrieve_fold_context alone.\nCost: ~30ms.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string", "format": "uuid" },
            "query": { "type": "string", "maxLength": 4096 },
            "embedding": { "type": "array", "items": { "type": "number" } },
            "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
        },
        "required": ["session_id", "query"]
    }),
},
```

- [ ] **Step 5: Run tests, commit**

```bash
git add crates/ferrosa-memory-core/src/hybrid_search.rs crates/ferrosa-memory-core/src/lib.rs crates/ferrosa-memory-core/src/dispatch.rs
git commit -m "feat: hybrid search MCP tool with RRF fusion"
```

---

## Task 6: Add Dream/Consolidation Engine

**Files:**
- Create: `crates/ferrosa-memory-core/src/dream.rs`
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Modify: `crates/ferrosa-memory-core/src/storage.rs` (add `entity_list_session`)
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs` (implement)
- Modify: mock storage (implement)

Inspired by vestige's 5-phase dream consolidation. Simplified to 3 phases for v1: **triage** (identify memories by importance), **connection discovery** (find co-occurring entities in same folds, create CO_OCCURS edges), **insight generation** (identify clusters of related entities).

- [ ] **Step 1: Add `entity_list_session` to Storage trait**

In `storage.rs`:
```rust
/// List all entities for a session (for consolidation).
async fn entity_list_session(
    &self,
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<Vec<EntityEntry>>;
```

Implement in mock storage (return empty vec) and CQL storage.

- [ ] **Step 2: Create dream.rs with 3-phase consolidation**

```rust
//! Dream consolidation — periodic memory processing.
//!
//! Inspired by vestige's 5-phase dream cycle. Simplified for v1:
//! 1. Triage — classify entities by access frequency
//! 2. Connection Discovery — find co-occurring entities, create CO_OCCURS edges
//! 3. Insight Generation — identify clusters and produce summaries

use std::collections::HashMap;
use uuid::Uuid;
use serde::Serialize;

use crate::storage::Storage;
use crate::types::TenantContext;

#[derive(Debug, Serialize)]
pub struct DreamResult {
    pub entities_processed: usize,
    pub connections_created: usize,
    pub insights: Vec<String>,
}

pub async fn run_consolidation(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<DreamResult> {
    // Phase 1: Triage — list all entities for the session
    let entities = storage.entity_list_session(ctx, session_id).await?;
    let entity_count = entities.len();

    // Phase 2: Connection Discovery
    // Group entities by source_fold_id — entities in the same fold co-occur
    let mut fold_groups: HashMap<Uuid, Vec<&crate::types::EntityEntry>> = HashMap::new();
    for entity in &entities {
        if let Some(fold_id) = entity.source_fold_id {
            fold_groups.entry(fold_id).or_default().push(entity);
        }
    }

    let mut connections_created = 0;
    for (_fold_id, group) in &fold_groups {
        // Create CO_OCCURS edges between all pairs in the same fold
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let _ = storage
                    .edge_co_occurs(ctx, group[i].entity_id, group[j].entity_id, session_id)
                    .await;
                connections_created += 1;
            }
        }
    }

    // Phase 3: Insight Generation
    // Identify entity clusters (groups > 2 entities in same fold)
    let mut insights = Vec::new();
    for (fold_id, group) in &fold_groups {
        if group.len() >= 3 {
            let names: Vec<&str> = group.iter().map(|e| e.entity_name.as_str()).collect();
            insights.push(format!(
                "Cluster in fold {}: {} are co-occurring ({} entities)",
                fold_id, names.join(", "), group.len()
            ));
        }
    }

    Ok(DreamResult {
        entities_processed: entity_count,
        connections_created,
        insights,
    })
}
```

- [ ] **Step 3: Add module to lib.rs**

```rust
pub mod dream;
```

- [ ] **Step 4: Write test**

```rust
#[tokio::test]
async fn dream_creates_co_occurs_edges() {
    // Set up mock with entities sharing a fold_id
    // Run consolidation
    // Verify edge_co_occurs was called
}
```

- [ ] **Step 5: Add ToolDef and handler**

```rust
ToolDef {
    name: "run_consolidation".into(),
    description: "Triggers memory consolidation ('dream' cycle). Discovers co-occurring entities, creates graph edges, and generates insights about entity clusters.\n\nCALL WHEN: After a productive session with many entities created. Also useful at session end to solidify the knowledge graph.\nCost: ~100ms+ depending on entity count.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string", "format": "uuid" }
        },
        "required": ["session_id"]
    }),
},
```

- [ ] **Step 6: Run tests, commit**

```bash
git add crates/ferrosa-memory-core/src/dream.rs crates/ferrosa-memory-core/src/lib.rs \
       crates/ferrosa-memory-core/src/dispatch.rs crates/ferrosa-memory-core/src/storage.rs \
       crates/ferrosa-memory-core/src/cql_storage.rs
git commit -m "feat: dream consolidation engine with co-occurrence discovery"
```

---

## Task 7: Add Memory State Management (promote/demote)

**Files:**
- Modify: `crates/ferrosa-memory-core/src/types.rs`
- Modify: `crates/ferrosa-memory-core/src/storage.rs`
- Modify: `crates/ferrosa-memory-core/src/entity.rs`
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`

Vestige tracks memory states: active → dormant → silent → unavailable. We'll add a `state` field to `EntityEntry` and expose promote/demote tools.

- [ ] **Step 1: Add MemoryState enum to types.rs**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryState {
    Active,
    Dormant,
    Silent,
    Unavailable,
}

impl Default for MemoryState {
    fn default() -> Self { Self::Active }
}
```

Add `pub state: MemoryState` to `EntityEntry` (with `#[serde(default)]` so existing deserialization works).

**All `EntityEntry` constructors that must add `state: MemoryState::default()`:**
- `smart_ingest.rs` lines 94, 152, 188 (3 sites)
- `entity.rs` line 91 (1 site)
- `cql_storage.rs` — entity deserialization (add `state` column read with fallback to `Active`)
- `storage.rs` mock — `entity_put` and any test constructors

**DDL migration needed** — create `ddl/007_entity_state.cql`:
```sql
ALTER TABLE agent_memory.entity_store ADD state text;
```

- [ ] **Step 2: Add `entity_update_state` to Storage trait**

```rust
async fn entity_update_state(
    &self,
    ctx: &TenantContext,
    entity_id: Uuid,
    state: crate::types::MemoryState,
) -> anyhow::Result<()>;
```

- [ ] **Step 3: Add promote/demote functions to entity.rs**

```rust
pub async fn promote_memory(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
) -> anyhow::Result<MemoryState> {
    // dormant→active, silent→dormant, unavailable→silent
}

pub async fn demote_memory(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
) -> anyhow::Result<MemoryState> {
    // active→dormant, dormant→silent, silent→unavailable
}
```

- [ ] **Step 4: Add ToolDefs and handlers for promote_memory and demote_memory**

- [ ] **Step 5: Run tests, commit**

```bash
git add crates/ferrosa-memory-core/src/types.rs crates/ferrosa-memory-core/src/storage.rs \
       crates/ferrosa-memory-core/src/entity.rs crates/ferrosa-memory-core/src/dispatch.rs \
       crates/ferrosa-memory-core/src/cql_storage.rs
git commit -m "feat: memory state management (promote/demote)"
```

---

## Task 8: Add Memory Health / Stats Tool

**Files:**
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`
- Modify: `crates/ferrosa-memory-core/src/storage.rs` (add count methods)

Simple stats aggregation — total entities, folds, memos, temporal facts per session.

- [ ] **Step 1: Add count methods to Storage trait**

```rust
async fn fold_count(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<i64>;
async fn memo_count(&self, ctx: &TenantContext) -> anyhow::Result<i64>;
async fn temporal_count(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<i64>;
```

- [ ] **Step 2: Add `get_stats` ToolDef and handler**

Returns JSON with counts per table and memory state distribution.

- [ ] **Step 3: Run tests, commit**

```bash
git commit -m "feat: get_stats MCP tool for memory health monitoring"
```

---

## Task 9: Add Intention CQL Persistence (DDL + Storage)

**Files:**
- Create: `ddl/006_intentions.cql`
- Modify: `crates/ferrosa-memory-core/src/storage.rs`
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs`
- Modify: `crates/ferrosa-memory-core/src/intention.rs`

Persist intentions to CQL so they survive server restarts.

- [ ] **Step 1: Create DDL**

```sql
CREATE TABLE IF NOT EXISTS agent_memory.intentions (
    tenant_id uuid,
    intention_id uuid,
    description text,
    trigger_json text,
    priority text,
    status text,
    created_at timestamp,
    triggered_at timestamp,
    completed_at timestamp,
    PRIMARY KEY ((tenant_id), intention_id)
);
```

- [ ] **Step 2: Add Storage trait methods**

```rust
async fn intention_put(&self, ctx: &TenantContext, intention: &crate::intention::Intention) -> anyhow::Result<()>;
async fn intention_list(&self, ctx: &TenantContext) -> anyhow::Result<Vec<crate::intention::Intention>>;
async fn intention_update_status(&self, ctx: &TenantContext, id: Uuid, status: &str) -> anyhow::Result<()>;
```

- [ ] **Step 3: Implement in CQL storage**

- [ ] **Step 4: Update IntentionStore to load from storage on init**

- [ ] **Step 5: Run tests, commit**

```bash
git add ddl/006_intentions.cql crates/ferrosa-memory-core/src/storage.rs \
       crates/ferrosa-memory-core/src/cql_storage.rs crates/ferrosa-memory-core/src/intention.rs
git commit -m "feat: persist intentions to CQL"
```

---

## Task 10: Auto-Entity Extraction on Fold Complete

**Files:**
- Modify: `crates/ferrosa-memory-core/src/fold.rs`
- Modify: `crates/ferrosa-memory-core/src/smart_ingest.rs`

The key missing automation: when `complete_fold()` is called with a summary, automatically extract entities from the summary text and ingest them via `smart_ingest`.

- [ ] **Step 1: Add simple entity extraction heuristic**

In `smart_ingest.rs`, add:
```rust
/// Extract candidate entities from text using simple heuristics.
/// Looks for: capitalized multi-word phrases, quoted terms, terms after
/// "is a", "called", "named".
pub fn extract_entity_candidates(text: &str) -> Vec<(String, String)> {
    let mut candidates = Vec::new();
    // Capitalized phrases (2+ words starting with uppercase)
    // Pattern markers ("is a", "called", "named")
    // Return (name, entity_type) pairs
    candidates
}
```

- [ ] **Step 2: Wire into complete_fold**

After fold completion, call `smart_ingest` for each extracted entity candidate:
```rust
// After successful fold completion, extract and ingest entities
let candidates = crate::smart_ingest::extract_entity_candidates(summary);
for (name, entity_type) in candidates {
    let _ = crate::smart_ingest::smart_ingest(
        storage, ctx, session_id, &name, &entity_type,
        None, Some(fold_id), &IngestConfig::default(),
    ).await;
}
```

- [ ] **Step 3: Write test, run, commit**

```bash
git add crates/ferrosa-memory-core/src/fold.rs crates/ferrosa-memory-core/src/smart_ingest.rs
git commit -m "feat: auto-extract entities from fold summaries on complete"
```

---

## Task 11: Rebuild Binary and Verify MCP Server

**Files:**
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs` (final tool count check)
- Possibly modify transport layer to pass SessionState

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 2: Build release binary**

Run: `cargo build --workspace`
Expected: Clean build.

- [ ] **Step 3: Verify tools/list returns all new tools**

Start the server, connect via MCP, verify `tools/list` returns the expected tool count (should be 27 tools: 13 original + 1 smart_ingest + 5 intention + 2 temporal + 1 graph + 1 hybrid_search + 1 consolidation + 2 promote/demote + 1 get_stats = 27).

- [ ] **Step 4: Commit any fixups**

```bash
git commit -m "chore: verify all new MCP tools functional"
```

---

## Summary

| Task | Tools Added | Effort |
|------|------------|--------|
| 1. Smart Ingest | `smart_ingest` | Low |
| 2. Intentions | `set_intention`, `check_intentions`, `complete_intention`, `list_intentions`, `snooze_intention` | Medium |
| 3. Temporal Facts | `write_temporal_fact`, `get_temporal_chain` | Low |
| 4. Graph Traversal | `explore_connections` | Medium |
| 5. Hybrid Search | `hybrid_search` | Medium |
| 6. Dream/Consolidation | `run_consolidation` | Medium |
| 7. Memory States | `promote_memory`, `demote_memory` | Low |
| 8. Stats | `get_stats` | Low |
| 9. Intention Persistence | (no new tools — CQL backing) | Medium |
| 10. Auto-Entity Extraction | (no new tools — wired into complete_fold) | Medium |
| 11. Verify | (integration) | Low |

**Total new MCP tools: 14** (from 13 → 27)
