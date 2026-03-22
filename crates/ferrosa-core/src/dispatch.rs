//! Tool dispatch — registry of MCP tools with schema validation.
//!
//! Maps MCP tool names to handler functions. Validates input schemas before
//! dispatch. Returns tool definitions for `tools/list`.
//!
//! ## MCP protocol methods handled
//!
//! - `initialize` — server capability handshake
//! - `tools/list` — returns all tool schemas
//! - `tools/call` — dispatches to the named tool handler
//! - `notifications/initialized` — client acknowledgment (no-op)

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::transport::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};

/// Per-session mutable state (not persisted in CQL).
pub struct SessionState {
    pub intentions: Arc<Mutex<crate::intention::IntentionStore>>,
    pub graph: Option<Arc<crate::graph::GraphClient>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            intentions: Arc::new(Mutex::new(crate::intention::IntentionStore::new())),
            graph: None,
        }
    }
}

/// MCP tool definition for `tools/list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Build all tool definitions for the memory server.
pub fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "check_memo_cache".into(),
            description: "Looks up a prior sub-call result by content hash. Returns cached result if found, or miss signal if not.\n\nCALL WHEN: Before every sub-LLM invocation within a long-horizon task. This is the first step in the usage loop.\nDO NOT CALL: For top-level queries or tasks where you are not making sub-calls. Do not call more than once per sub-call.\nON HIT: Use the cached result directly. Do not invoke the sub-LLM. Call record_outcome with program_type='memo_hit'.\nON MISS: Proceed with the sub-call. After it completes, call store_memo_result.\nCost: ~1ms. Zero token cost.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "maxLength": 4096, "description": "The prompt text" },
                    "context_slice": { "type": "string", "maxLength": 131072, "description": "Context slice for cache key" },
                    "model_version": { "type": "string", "maxLength": 64, "description": "Model version string" }
                },
                "required": ["prompt", "context_slice", "model_version"]
            }),
        },
        ToolDef {
            name: "store_memo_result".into(),
            description: "Stores a completed sub-call result for future reuse.\n\nCALL WHEN: Immediately after any sub-call completes on a task where the same chunk might be processed again.\nDO NOT CALL: For top-level responses or ephemeral computations. Do not call if check_memo_cache returned a hit.\nCost: ~5ms write.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "maxLength": 4096 },
                    "context_slice": { "type": "string", "maxLength": 131072 },
                    "model_version": { "type": "string", "maxLength": 64 },
                    "result": { "type": "string", "maxLength": 131072, "description": "The sub-call result to cache" },
                    "embedding": {
                        "type": "array", "items": { "type": "number" },
                        "description": "Optional embedding vector"
                    },
                    "ttl_days": { "type": "integer", "minimum": 1, "maximum": 365, "description": "TTL in days (default: 7)" }
                },
                "required": ["prompt", "context_slice", "model_version", "result"]
            }),
        },
        ToolDef {
            name: "write_plan_node".into(),
            description: "Records a sub-task node in the hierarchical plan tree. Enables structured re-injection of parent plan context on recursive return.\n\nCALL WHEN: At the start of each sub-task, before execution. Always call when decomposing a complex task into sub-tasks. Depth=0 is the root goal.\nDO NOT CALL: For single-step tasks with no decomposition.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "subtask_id": { "type": "string", "maxLength": 256 },
                    "parent_subtask": { "type": "string", "maxLength": 256 },
                    "goal_text": { "type": "string", "maxLength": 4096 }
                },
                "required": ["session_id", "depth", "subtask_id", "goal_text"]
            }),
        },
        ToolDef {
            name: "get_plan_context".into(),
            description: "Returns the full plan tree for the current session as compact JSON. Use to re-inject parent context when returning from recursive sub-tasks.\n\nCALL WHEN: At the start of each sub-task execution and on return from a sub-task call.\nInclude the returned plan tree in your prompt preamble with 'Current task hierarchy:' to prevent goal drift.\nCost: ~2ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "max_depth": { "type": "integer", "minimum": 0, "maximum": 100 }
                },
                "required": ["session_id"]
            }),
        },
        ToolDef {
            name: "update_plan_node".into(),
            description: "Marks a plan node complete or failed and records an outcome summary.\n\nCALL WHEN: When a sub-task finishes (success or failure). Always provide outcome_summary — this is what parent nodes will see.\nWrite outcome_summary describing what was found, not the process used.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "subtask_id": { "type": "string", "maxLength": 256 },
                    "status": { "type": "string", "enum": ["pending", "active", "complete", "failed"] },
                    "outcome_summary": { "type": "string", "maxLength": 4096 }
                },
                "required": ["session_id", "depth", "subtask_id", "status"]
            }),
        },
        // --- Fold tools (Sprint 2) ---
        ToolDef {
            name: "start_fold".into(),
            description: "Opens a new trajectory fold for a sub-task. Returns fold_id to append REPL turns as the sub-task executes.\n\nCALL WHEN: Starting any sub-task that involves multiple steps and whose results you want retrievable later. Always call write_plan_node first.\nA fold is the durable equivalent of a REPL scope.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "parent_fold_id": { "type": "string", "format": "uuid" },
                    "initial_context": { "type": "string", "maxLength": 131072 }
                },
                "required": ["session_id", "depth", "initial_context"]
            }),
        },
        ToolDef {
            name: "append_to_fold".into(),
            description: "Appends a REPL turn to an active fold. Returns current token_count.\n\nCALL WHEN: After each step within an active fold.\nMONITOR token_count: If it exceeds ~80000, open a nested fold for the next phase.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fold_id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string", "format": "uuid" },
                    "repl_turn": { "type": "string", "maxLength": 131072 }
                },
                "required": ["fold_id", "session_id", "repl_turn"]
            }),
        },
        ToolDef {
            name: "complete_fold".into(),
            description: "Seals a fold with summary and embedding. Creates FOLDED_INTO graph edge to parent. Queues trajectory for compression.\n\nCALL WHEN: When a sub-task is fully complete. Always call before returning from a recursive level.\nWrite summary as dense NL capsule: key findings, state changes, answers. Summarize outcomes, not process.\nCost: ~10ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fold_id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string", "format": "uuid" },
                    "summary": { "type": "string", "maxLength": 131072 },
                    "embedding": { "type": "array", "items": { "type": "number" } }
                },
                "required": ["fold_id", "session_id", "summary", "embedding"]
            }),
        },
        ToolDef {
            name: "retrieve_fold_context".into(),
            description: "ANN vector search over prior fold summaries. Returns k most semantically similar fold summaries.\n\nCALL WHEN: Starting a new task where prior work might be relevant. Also call when stuck — prior folds often contain relevant evidence.\nRETRIEVAL LOOP: If results partially answer but leave gaps, call again with a more specific query targeting the gap. 2-3 rounds is normal.\nCost: ~10ms (HNSW). include_raw adds ~200-2000ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "query_embedding": { "type": "array", "items": { "type": "number" } },
                    "k": { "type": "integer", "minimum": 1, "maximum": 100 },
                    "include_raw": { "type": "boolean" }
                },
                "required": ["session_id", "query_embedding"]
            }),
        },
        // --- Entity tools (Sprint 3) ---
        ToolDef {
            name: "upsert_entity".into(),
            description: "Writes a discovered named entity to the entity store. Deduplicates via phonetic matching.\n\nCALL WHEN: Any time you identify a named entity (person, place, org, event, concept) from content. Always link to source_fold_id.\nCheck is_new in response: if false, entity already exists — use the returned entity_id to attach new facts.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "entity_name": { "type": "string", "maxLength": 512 },
                    "entity_type": { "type": "string", "enum": ["person", "place", "event", "concept", "org"] },
                    "context_snippet": { "type": "string", "maxLength": 4096 },
                    "embedding": { "type": "array", "items": { "type": "number" } },
                    "source_fold_id": { "type": "string", "format": "uuid" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                },
                "required": ["session_id", "entity_name", "entity_type", "context_snippet"]
            }),
        },
        ToolDef {
            name: "retrieve_entities".into(),
            description: "Retrieves named entities by name (phonetic fuzzy match), semantic similarity (ANN), or both.\n\nCALL WHEN: Need to find entities related to current query. Use strategy='phonetic' for known names with possible variants. Use strategy='ann' for semantic search. Use strategy='both' for maximum recall.\nCost: phonetic ~5ms, ann ~10ms, both ~15ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "query": { "type": "string", "maxLength": 4096 },
                    "embedding": { "type": "array", "items": { "type": "number" } },
                    "strategy": { "type": "string", "enum": ["ann", "phonetic", "both"] },
                    "k": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": ["session_id", "query"]
            }),
        },
        // --- Feedback tool (Sprint 3) ---
        ToolDef {
            name: "record_outcome".into(),
            description: "Records the result of a retrieval operation for offline routing improvement.\n\nCALL WHEN: After every retrieval operation (retrieve_fold_context, retrieve_entities, check_memo_cache). Provide program_type, task_complexity, succeeded, latency_ms, token_cost.\nThis is write-only (~1ms). No effect on current task but improves routing for future sessions.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "query_id": { "type": "string", "format": "uuid" },
                    "program_type": { "type": "string", "enum": ["hnsw_ann", "phonetic", "cypher_hop", "btree_range", "memo_hit"] },
                    "task_complexity": { "type": "string", "enum": ["simple", "linear", "quadratic"] },
                    "succeeded": { "type": "boolean" },
                    "latency_ms": { "type": "integer", "minimum": 0 },
                    "token_cost": { "type": "integer", "minimum": 0 }
                },
                "required": ["session_id", "query_id", "program_type", "task_complexity", "succeeded", "latency_ms", "token_cost"]
            }),
        },
        // --- Session lifecycle ---
        ToolDef {
            name: "delete_session".into(),
            description: "Deletes all memory objects for a session across all tables (right-to-deletion).\n\nCALL WHEN: User explicitly requests data deletion, or session cleanup is needed.\nDO NOT CALL: During normal operation. This is destructive and irreversible.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id"]
            }),
        },
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
        // --- Intention tools (prospective memory) ---
        ToolDef {
            name: "set_intention".into(),
            description: "Sets a prospective memory intention — a deferred action that triggers when a context condition is met.\n\nCALL WHEN: You or the user identify something to remember to do later when a specific context arises (e.g., 'when we work on auth, review error handling').\nDO NOT CALL: For immediate tasks. Use plan tools for current task state.\nReturns: intention_id for tracking.\nCost: ~1ms (in-memory).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "maxLength": 4096, "description": "What to do when triggered" },
                    "trigger": {
                        "type": "object",
                        "description": "Trigger condition",
                        "properties": {
                            "type": { "type": "string", "enum": ["Topic", "FilePattern", "Duration", "Context"] },
                            "keywords": { "type": "array", "items": { "type": "string" }, "description": "For Topic triggers" },
                            "pattern": { "type": "string", "description": "For FilePattern triggers" },
                            "minutes": { "type": "integer", "minimum": 1, "description": "For Duration triggers" },
                            "condition": { "type": "string", "description": "For Context triggers" }
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
            description: "Checks all pending intentions against the current context. Returns any that trigger.\n\nCALL WHEN: At the start of each new task or context switch. Lightweight scan of pending intentions.\nCost: ~1ms (in-memory).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "context": { "type": "string", "maxLength": 8192, "description": "Current context to check against" }
                },
                "required": ["context"]
            }),
        },
        ToolDef {
            name: "complete_intention".into(),
            description: "Marks a triggered intention as completed.\n\nCALL WHEN: After you have acted on a triggered intention.\nCost: ~1ms.".into(),
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
            description: "Lists all intentions (pending, triggered, completed, snoozed).\n\nCALL WHEN: User asks about pending intentions, or for debugging intention state.\nCost: ~1ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "snooze_intention".into(),
            description: "Snoozes a triggered intention — resets it to pending so it can trigger again later.\n\nCALL WHEN: An intention triggered but you want to defer action. Resets to pending state.\nCost: ~1ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "intention_id": { "type": "string", "format": "uuid" }
                },
                "required": ["intention_id"]
            }),
        },
        // --- Temporal fact tools ---
        ToolDef {
            name: "write_temporal_fact".into(),
            description: "Records a timestamped fact about an entity. Auto-supersedes the previous current fact for the same entity.\n\nCALL WHEN: You learn a new fact about an entity that may change over time (e.g., role, location, status). The old fact is preserved with a valid_until timestamp.\nDO NOT CALL: For static attributes unlikely to change. Use upsert_entity for those.\nReturns: event_id of the new fact.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "entity_id": { "type": "string", "format": "uuid" },
                    "fact_text": { "type": "string", "maxLength": 4096, "description": "The fact to record" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1, "description": "Confidence score (default: 1.0)" }
                },
                "required": ["session_id", "entity_id", "fact_text"]
            }),
        },
        ToolDef {
            name: "get_temporal_chain".into(),
            description: "Returns the current (most recent valid) fact for an entity.\n\nCALL WHEN: You need to check the latest known fact about an entity before writing a new one, or to answer a question about current state.\nReturns: The current fact object, or {\"fact\": null} if no facts exist.\nCost: ~2ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "entity_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id", "entity_id"]
            }),
        },
        // --- Graph traversal tool ---
        ToolDef {
            name: "explore_connections".into(),
            description: "Traverses the knowledge graph. Supports 4 traversal types:\n- fold_ancestors: walk the fold hierarchy upward from a fold\n- related_entities: find entities connected within N hops\n- entities_in_fold: list all entities mentioned in a fold\n- supersession_chain: follow temporal supersession links from a fact\n\nCALL WHEN: You need to understand relationships between entities or folds, or trace how facts evolved over time.\nRequires a graph connection to be configured.\nCost: ~10-50ms depending on traversal depth.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "traversal": {
                        "type": "string",
                        "enum": ["fold_ancestors", "related_entities", "entities_in_fold", "supersession_chain"],
                        "description": "The type of graph traversal to perform"
                    },
                    "entity_id": { "type": "string", "format": "uuid", "description": "Entity or event ID (required for related_entities, supersession_chain)" },
                    "fold_id": { "type": "string", "format": "uuid", "description": "Fold ID (required for fold_ancestors, entities_in_fold)" },
                    "session_id": { "type": "string", "format": "uuid", "description": "Session ID (required for fold_ancestors, related_entities, entities_in_fold)" },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Maximum traversal depth (default: 2)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum results to return (default: 10)" }
                },
                "required": ["traversal"]
            }),
        },
        // --- Hybrid search ---
        ToolDef {
            name: "hybrid_search".into(),
            description: "Multi-strategy search combining phonetic entity lookup, ANN entity search, and ANN fold search with Reciprocal Rank Fusion.\n\nCALL WHEN: You need maximum recall across all memory types — entities and folds. Prefer this over separate retrieve_entities + retrieve_fold_context when you want a single ranked result set.\nProvide embedding for ANN strategies; without it only phonetic matching runs.\nCost: ~15ms (runs up to 3 strategies in sequence).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "query": { "type": "string", "maxLength": 4096, "description": "Search query text (used for phonetic matching)" },
                    "embedding": {
                        "type": "array",
                        "items": { "type": "number" },
                        "description": "Optional embedding vector for ANN search strategies"
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Max results to return (default: 10)" }
                },
                "required": ["session_id", "query"]
            }),
        },
        // --- Dream consolidation ---
        ToolDef {
            name: "run_consolidation".into(),
            description: "Runs dream consolidation over a session's entities. Groups entities by source fold, creates CO_OCCURS edges between co-occurring entities, and identifies clusters (3+ entities in the same fold).\n\nCALL WHEN: After a session accumulates many entities — typically at session end or during idle periods. Strengthens the knowledge graph by discovering implicit connections.\nCost: O(entities) reads + O(pairs) edge writes.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id"]
            }),
        },
        // --- Stats tool ---
        ToolDef {
            name: "get_stats".into(),
            description: "Returns memory system statistics for the session: entity count, fold count, memo count, and intention count.\n\nCALL WHEN: For health monitoring, debugging, or when the user asks about memory usage.\nCost: ~5ms (runs 3 count queries).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id"]
            }),
        },
        // --- Memory state management ---
        ToolDef {
            name: "promote_memory".into(),
            description: "Promotes an entity's memory state one level: dormant->active, silent->dormant, unavailable->silent. Active stays active.\n\nCALL WHEN: A dormant or silent memory becomes relevant again — e.g., an entity is referenced in new context after a period of inactivity.\nRETURNS: The new memory state after promotion.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "entity_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id", "entity_id"]
            }),
        },
        ToolDef {
            name: "demote_memory".into(),
            description: "Demotes an entity's memory state one level: active->dormant, dormant->silent, silent->unavailable. Unavailable stays unavailable.\n\nCALL WHEN: A memory is no longer relevant to the current context, or during periodic decay sweeps. Demoted memories are still retrievable but with lower priority.\nRETURNS: The new memory state after demotion.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "entity_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id", "entity_id"]
            }),
        },
    ]
}

/// Server info returned during initialize.
fn server_info() -> Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "ferrosa-memory-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

/// Dispatch an MCP method call. Returns `Ok(result)` or `Err((code, message))`.
///
/// This is the top-level entry point called by the transport layer for each
/// incoming JSON-RPC request.
pub async fn dispatch<S: crate::storage::Storage>(
    method: &str,
    params: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    match method {
        "initialize" => Ok(server_info()),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => {
            let tools = tool_definitions();
            Ok(serde_json::json!({ "tools": tools }))
        }
        "tools/call" => dispatch_tool(params, storage, ctx, session).await,
        _ => Err((METHOD_NOT_FOUND, format!("unknown method: {method}"))),
    }
}

/// Dispatch a tools/call request to the appropriate handler.
async fn dispatch_tool<S: crate::storage::Storage>(
    params: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((INVALID_PARAMS, "missing tool name".into()))?;

    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    tracing::debug!(tool = name, "dispatching tool call");
    let start = std::time::Instant::now();
    let result = match name {
        "check_memo_cache" => handle_check_memo(args, storage, ctx).await,
        "store_memo_result" => handle_store_memo(args, storage, ctx).await,
        "write_plan_node" => handle_write_plan(args, storage, ctx).await,
        "get_plan_context" => handle_get_plan(args, storage, ctx).await,
        "update_plan_node" => handle_update_plan(args, storage, ctx).await,
        "start_fold" => handle_start_fold(args, storage, ctx).await,
        "append_to_fold" => handle_append_fold(args, storage, ctx).await,
        "complete_fold" => handle_complete_fold(args, storage, ctx).await,
        "retrieve_fold_context" => handle_retrieve_fold(args, storage, ctx).await,
        "upsert_entity" => handle_upsert_entity(args, storage, ctx).await,
        "retrieve_entities" => handle_retrieve_entities(args, storage, ctx).await,
        "record_outcome" => handle_record_outcome(args, storage, ctx).await,
        "delete_session" => handle_delete_session(args, storage, ctx).await,
        "smart_ingest" => handle_smart_ingest(args, storage, ctx).await,
        "set_intention" => handle_set_intention(args, storage, ctx, session).await,
        "check_intentions" => handle_check_intentions(args, storage, ctx, session).await,
        "complete_intention" => handle_complete_intention(args, storage, ctx, session).await,
        "list_intentions" => handle_list_intentions(session).await,
        "snooze_intention" => handle_snooze_intention(args, storage, ctx, session).await,
        "write_temporal_fact" => handle_write_temporal_fact(args, storage, ctx).await,
        "get_temporal_chain" => handle_get_temporal_chain(args, storage, ctx).await,
        "explore_connections" => handle_explore_connections(args, session).await,
        "hybrid_search" => handle_hybrid_search(args, storage, ctx).await,
        "run_consolidation" => handle_run_consolidation(args, storage, ctx).await,
        "get_stats" => handle_get_stats(args, storage, ctx, session).await,
        "promote_memory" => handle_promote_memory(args, storage, ctx).await,
        "demote_memory" => handle_demote_memory(args, storage, ctx).await,
        _ => Err((METHOD_NOT_FOUND, format!("unknown tool: {name}"))),
    };
    let elapsed = start.elapsed();
    match &result {
        Ok(_) => tracing::debug!(
            tool = name,
            elapsed_ms = elapsed.as_millis() as u64,
            "tool call OK"
        ),
        Err((code, msg)) => tracing::warn!(
            tool = name,
            code,
            msg,
            elapsed_ms = elapsed.as_millis() as u64,
            "tool call FAILED"
        ),
    }
    result
}

// --- Tool handlers ---

async fn handle_check_memo<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let prompt = require_str(&args, "prompt")?;
    let context_slice = require_str(&args, "context_slice")?;
    let model_version = require_str(&args, "model_version")?;

    let result = crate::memo::check_memo_cache(storage, ctx, prompt, context_slice, model_version)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_store_memo<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let prompt = require_str(&args, "prompt")?;
    let context_slice = require_str(&args, "context_slice")?;
    let model_version = require_str(&args, "model_version")?;
    let result_text = require_str(&args, "result")?;

    let embedding: Option<Vec<f32>> = args.get("embedding").and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_f64().map(|f| f as f32))
                .collect()
        })
    });

    let ttl_days = args
        .get("ttl_days")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let params = crate::memo::StoreMemoParams {
        prompt,
        context_slice,
        model_version,
        result: result_text,
        embedding,
        ttl_days,
    };

    let result = crate::memo::store_memo_result(storage, ctx, &params)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_write_plan<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let depth = require_i32(&args, "depth")?;
    let subtask_id = require_str(&args, "subtask_id")?;
    let parent_subtask = args.get("parent_subtask").and_then(|v| v.as_str());
    let goal_text = require_str(&args, "goal_text")?;

    let written = crate::plan::write_plan_node(
        storage,
        ctx,
        session_id,
        depth,
        subtask_id,
        parent_subtask,
        goal_text,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "written": written }))
}

async fn handle_get_plan<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);

    let plan = crate::plan::get_plan_context(storage, ctx, session_id, max_depth)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(plan).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_update_plan<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let depth = require_i32(&args, "depth")?;
    let subtask_id = require_str(&args, "subtask_id")?;
    let status_str = require_str(&args, "status")?;
    let outcome_summary = args.get("outcome_summary").and_then(|v| v.as_str());

    let status: crate::types::PlanStatus =
        serde_json::from_value(Value::String(status_str.to_string()))
            .map_err(|_| (INVALID_PARAMS, format!("invalid status: {status_str}")))?;

    let updated = crate::plan::update_plan_node(
        storage,
        ctx,
        session_id,
        depth,
        subtask_id,
        status,
        outcome_summary,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "updated": updated }))
}

// --- Fold handlers ---

async fn handle_start_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let depth = require_i32(&args, "depth")?;
    let parent_fold_id = optional_uuid(&args, "parent_fold_id")?;
    let initial_context = require_str(&args, "initial_context")?;

    let fold_id = crate::fold::start_fold(
        storage,
        ctx,
        session_id,
        depth,
        parent_fold_id,
        initial_context,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "fold_id": fold_id.to_string() }))
}

async fn handle_append_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let fold_id = require_uuid(&args, "fold_id")?;
    let repl_turn = require_str(&args, "repl_turn")?;

    let (appended, token_count) =
        crate::fold::append_to_fold(storage, ctx, session_id, fold_id, repl_turn)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "appended": appended, "token_count": token_count }))
}

async fn handle_complete_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let fold_id = require_uuid(&args, "fold_id")?;
    let summary = require_str(&args, "summary")?;
    let embedding = require_f32_array(&args, "embedding")?;

    let (folded, compression_ratio) =
        crate::fold::complete_fold(storage, ctx, session_id, fold_id, summary, embedding)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "folded": folded, "compression_ratio": compression_ratio }))
}

async fn handle_retrieve_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let query_embedding = require_f32_array(&args, "query_embedding")?;
    let k = args.get("k").and_then(|v| v.as_u64()).map(|v| v as usize);
    let include_raw = args
        .get("include_raw")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let folds = crate::fold::retrieve_fold_context(
        storage,
        ctx,
        session_id,
        &query_embedding,
        k,
        include_raw,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(&folds).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Entity handlers ---

async fn handle_upsert_entity<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let entity_name = require_str(&args, "entity_name")?;
    let entity_type = require_str(&args, "entity_type")?;
    let context_snippet = require_str(&args, "context_snippet")?;
    let embedding = optional_f32_array(&args, "embedding")?;
    let source_fold_id = optional_uuid(&args, "source_fold_id")?;
    let confidence = args.get("confidence").and_then(|v| v.as_f64());

    let result = crate::entity::upsert_entity(
        storage,
        ctx,
        session_id,
        entity_name,
        entity_type,
        context_snippet,
        embedding,
        source_fold_id,
        confidence,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_retrieve_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let query = require_str(&args, "query")?;
    let embedding = optional_f32_array(&args, "embedding")?;
    let strategy = args
        .get("strategy")
        .and_then(|v| v.as_str())
        .unwrap_or("both");
    let k = args.get("k").and_then(|v| v.as_u64()).map(|v| v as usize);

    let entities = crate::entity::retrieve_entities(
        storage,
        ctx,
        session_id,
        query,
        embedding.as_deref(),
        strategy,
        k,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(&entities).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Memory state handlers ---

async fn handle_promote_memory<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let entity_id = require_uuid(&args, "entity_id")?;

    let new_state = crate::entity::promote_memory(storage, ctx, session_id, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "new_state": new_state.to_string() }))
}

async fn handle_demote_memory<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let entity_id = require_uuid(&args, "entity_id")?;

    let new_state = crate::entity::demote_memory(storage, ctx, session_id, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "new_state": new_state.to_string() }))
}

// --- Feedback handler ---

async fn handle_record_outcome<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let query_id = require_uuid(&args, "query_id")?;
    let program_type = require_str(&args, "program_type")?;
    let task_complexity = require_str(&args, "task_complexity")?;
    let succeeded = args
        .get("succeeded")
        .and_then(|v| v.as_bool())
        .ok_or((INVALID_PARAMS, "missing required bool: succeeded".into()))?;
    let latency_ms = require_i32(&args, "latency_ms")?;
    let token_cost = require_i32(&args, "token_cost")?;

    let recorded = crate::feedback::record_outcome(
        storage,
        ctx,
        session_id,
        query_id,
        program_type,
        task_complexity,
        succeeded,
        latency_ms,
        token_cost,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({ "recorded": recorded }))
}

// --- Session lifecycle handler ---

async fn handle_delete_session<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;

    let result = crate::session::delete_session(storage, ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Cognitive memory handler ---

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

// --- Intention handlers ---

async fn handle_set_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let description = require_str(&args, "description")?;
    let trigger_json = args
        .get("trigger")
        .ok_or((INVALID_PARAMS, "missing required object: trigger".into()))?;
    let trigger: crate::intention::IntentionTrigger = serde_json::from_value(trigger_json.clone())
        .map_err(|e| (INVALID_PARAMS, format!("invalid trigger: {e}")))?;
    let priority: crate::intention::Priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .map(|s| {
            serde_json::from_value(Value::String(s.to_string()))
                .unwrap_or(crate::intention::Priority::Normal)
        })
        .unwrap_or(crate::intention::Priority::Normal);

    let mut store = session.intentions.lock().await;
    let intention = store.set(description, trigger, priority);
    let id = intention.id;

    // Persist to storage (best-effort -- in-memory is primary)
    if let Err(e) = storage.intention_put(ctx, &intention).await {
        tracing::warn!(error = %e, "failed to persist intention to storage");
    }

    Ok(serde_json::json!({ "intention_id": id.to_string() }))
}

async fn handle_check_intentions<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let context = require_str(&args, "context")?;
    let mut store = session.intentions.lock().await;
    let triggered = store.check(context);
    let triggered_json: Vec<Value> = triggered
        .iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();

    // Persist status changes for triggered intentions
    for intention in &triggered {
        let status_str = serde_json::to_string(&intention.status)
            .unwrap_or_else(|_| "\"triggered\"".into())
            .trim_matches('"')
            .to_string();
        if let Err(e) = storage
            .intention_update_status(ctx, intention.id, &status_str, intention.triggered_at, None)
            .await
        {
            tracing::warn!(id = %intention.id, error = %e, "failed to persist intention trigger");
        }
    }

    Ok(serde_json::json!({ "triggered": triggered_json }))
}

async fn handle_complete_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let id = require_uuid(&args, "intention_id")?;
    let mut store = session.intentions.lock().await;
    let completed = store.complete(id);

    if completed
        && let Err(e) = storage
            .intention_update_status(ctx, id, "completed", None, Some(chrono::Utc::now()))
            .await
    {
        tracing::warn!(%id, error = %e, "failed to persist intention completion");
    }

    Ok(serde_json::json!({ "completed": completed }))
}

async fn handle_list_intentions(session: &SessionState) -> Result<Value, (i32, String)> {
    let store = session.intentions.lock().await;
    let intentions = store.list();
    let json: Vec<Value> = intentions
        .iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();
    Ok(serde_json::json!({ "intentions": json }))
}

async fn handle_snooze_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let id = require_uuid(&args, "intention_id")?;
    let mut store = session.intentions.lock().await;
    let snoozed = store.snooze(id);

    if snoozed
        && let Err(e) = storage
            .intention_update_status(ctx, id, "pending", None, None)
            .await
    {
        tracing::warn!(%id, error = %e, "failed to persist intention snooze");
    }

    Ok(serde_json::json!({ "snoozed": snoozed }))
}

// --- Temporal fact handlers ---

async fn handle_write_temporal_fact<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let entity_id = require_uuid(&args, "entity_id")?;
    let fact_text = require_str(&args, "fact_text")?;
    let confidence = args
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);

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

    let fact = crate::temporal::get_current_fact(storage, ctx, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    match fact {
        Some(event) => serde_json::to_value(&event).map_err(|e| (INTERNAL_ERROR, e.to_string())),
        None => Ok(serde_json::json!({ "fact": null })),
    }
}

// --- Graph traversal handler ---

async fn handle_explore_connections(
    args: Value,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let graph = session
        .graph
        .as_ref()
        .ok_or((INTERNAL_ERROR, "graph client not configured".into()))?;

    let traversal = require_str(&args, "traversal")?;
    let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let results = match traversal {
        "fold_ancestors" => {
            let fold_id = require_uuid(&args, "fold_id")?;
            let session_id = require_uuid(&args, "session_id")?;
            graph
                .get_fold_ancestors(fold_id, session_id, max_depth)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        }
        "related_entities" => {
            let entity_id = require_uuid(&args, "entity_id")?;
            let session_id = require_uuid(&args, "session_id")?;
            let mut r = graph
                .find_related_entities(entity_id, session_id, max_depth)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            r.truncate(limit);
            r
        }
        "entities_in_fold" => {
            let fold_id = require_uuid(&args, "fold_id")?;
            let session_id = require_uuid(&args, "session_id")?;
            let mut r = graph
                .get_entities_in_fold(fold_id, session_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            r.truncate(limit);
            r
        }
        "supersession_chain" => {
            let entity_id = require_uuid(&args, "entity_id")?;
            let event_id = entity_id; // event_id is passed via entity_id field
            // For supersession_chain, we need both event_id and entity_id.
            // The tool schema uses entity_id for the event, and fold_id is repurposed
            // as the actual entity_id context. But the graph method signature is
            // (event_id, entity_id). We pass entity_id as event_id since the Cypher
            // query uses it as the starting fact node.
            let fact_entity_id = optional_uuid(&args, "fold_id")?.unwrap_or(event_id);
            graph
                .get_supersession_chain(event_id, fact_entity_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        }
        _ => {
            return Err((
                INVALID_PARAMS,
                format!("unknown traversal type: {traversal}"),
            ));
        }
    };

    Ok(serde_json::json!({
        "traversal": traversal,
        "results": results,
        "count": results.len()
    }))
}

// --- Hybrid search handler ---

async fn handle_hybrid_search<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let query = require_str(&args, "query")?;
    let embedding = optional_f32_array(&args, "embedding")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let results = crate::hybrid_search::hybrid_search(
        storage,
        ctx,
        session_id,
        query,
        embedding.as_deref(),
        limit,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "results": results,
        "count": results.len()
    }))
}

// --- Dream consolidation handler ---

async fn handle_run_consolidation<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;

    let result = crate::dream::run_consolidation(storage, ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Stats handler ---

async fn handle_get_stats<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let entity_count = storage.entity_count(ctx, session_id).await.unwrap_or(0);
    let fold_count = storage.fold_count(ctx, session_id).await.unwrap_or(0);
    let memo_count = storage.memo_count(ctx).await.unwrap_or(0);
    let intention_count = session.intentions.lock().await.list().len();

    Ok(serde_json::json!({
        "entity_count": entity_count,
        "fold_count": fold_count,
        "memo_count": memo_count,
        "intention_count": intention_count
    }))
}

// --- Parameter extraction helpers ---

fn optional_uuid(args: &Value, field: &str) -> Result<Option<uuid::Uuid>, (i32, String)> {
    match args.get(field).and_then(|v| v.as_str()) {
        Some(s) => uuid::Uuid::parse_str(s)
            .map(Some)
            .map_err(|e| (INVALID_PARAMS, format!("invalid uuid {field}: {e}"))),
        None => Ok(None),
    }
}

fn require_f32_array(args: &Value, field: &str) -> Result<Vec<f32>, (i32, String)> {
    args.get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_f64().map(|f| f as f32))
                .collect()
        })
        .ok_or((INVALID_PARAMS, format!("missing required array: {field}")))
}

fn optional_f32_array(args: &Value, field: &str) -> Result<Option<Vec<f32>>, (i32, String)> {
    match args.get(field) {
        Some(v) if v.is_array() => Ok(Some(
            v.as_array()
                .unwrap()
                .iter()
                .filter_map(|n| n.as_f64().map(|f| f as f32))
                .collect(),
        )),
        Some(_) => Err((INVALID_PARAMS, format!("{field} must be an array"))),
        None => Ok(None),
    }
}

fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, (i32, String)> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or((INVALID_PARAMS, format!("missing required string: {field}")))
}

fn require_uuid(args: &Value, field: &str) -> Result<uuid::Uuid, (i32, String)> {
    let s = require_str(args, field)?;
    uuid::Uuid::parse_str(s).map_err(|e| (INVALID_PARAMS, format!("invalid uuid {field}: {e}")))
}

fn require_i32(args: &Value, field: &str) -> Result<i32, (i32, String)> {
    args.get(field)
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or((INVALID_PARAMS, format!("missing required integer: {field}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use crate::types::TenantContext;
    use uuid::Uuid;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let result = dispatch("initialize", Value::Null, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["serverInfo"]["name"], "ferrosa-memory-mcp");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_returns_all_tools() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let result = dispatch("tools/list", Value::Null, &store, &ctx, &session)
            .await
            .unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 27);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"check_memo_cache"));
        assert!(names.contains(&"store_memo_result"));
        assert!(names.contains(&"write_plan_node"));
        assert!(names.contains(&"get_plan_context"));
        assert!(names.contains(&"update_plan_node"));
        assert!(names.contains(&"set_intention"));
        assert!(names.contains(&"check_intentions"));
        assert!(names.contains(&"complete_intention"));
        assert!(names.contains(&"list_intentions"));
        assert!(names.contains(&"snooze_intention"));
        assert!(names.contains(&"hybrid_search"));
        assert!(names.contains(&"promote_memory"));
        assert!(names.contains(&"demote_memory"));
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let err = dispatch("bogus/method", Value::Null, &store, &ctx, &session)
            .await
            .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_call_unknown_tool() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let params = serde_json::json!({ "name": "nonexistent_tool" });
        let err = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_call_missing_params() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let params = serde_json::json!({
            "name": "check_memo_cache",
            "arguments": {}
        });
        let err = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn memo_round_trip_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        // Store
        let store_params = serde_json::json!({
            "name": "store_memo_result",
            "arguments": {
                "prompt": "test prompt",
                "context_slice": "ctx",
                "model_version": "v1",
                "result": "cached answer"
            }
        });
        let result = dispatch("tools/call", store_params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["stored"], true);

        // Check
        let check_params = serde_json::json!({
            "name": "check_memo_cache",
            "arguments": {
                "prompt": "test prompt",
                "context_slice": "ctx",
                "model_version": "v1"
            }
        });
        let result = dispatch("tools/call", check_params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["hit"], true);
        assert_eq!(result["result"], "cached answer");
    }

    #[tokio::test]
    async fn plan_round_trip_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Write
        let write_params = serde_json::json!({
            "name": "write_plan_node",
            "arguments": {
                "session_id": sid.to_string(),
                "depth": 0,
                "subtask_id": "root",
                "goal_text": "solve the problem"
            }
        });
        let result = dispatch("tools/call", write_params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["written"], true);

        // Get
        let get_params = serde_json::json!({
            "name": "get_plan_context",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", get_params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["nodes"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn smart_ingest_creates_on_new_content() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Ferrosa uses LSM-tree storage with S3 tiering",
                "entity_type": "concept"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["action"], "Created");
        assert!(result["entity_id"].is_string());
    }

    #[tokio::test]
    async fn set_intention_and_check() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        // Set
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

    #[tokio::test]
    async fn temporal_fact_round_trip() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let entity_id = Uuid::new_v4();

        // Write a temporal fact
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

        // Get the current fact
        let params = serde_json::json!({
            "name": "get_temporal_chain",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_id": entity_id.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["fact_text"], "Alice is VP of Engineering");
        assert_eq!(result["confidence"], 0.95);

        // Get for unknown entity returns null fact
        let unknown = Uuid::new_v4();
        let params = serde_json::json!({
            "name": "get_temporal_chain",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_id": unknown.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        assert!(result["fact"].is_null());
    }

    #[tokio::test]
    async fn explore_connections_requires_graph() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default(); // graph is None
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "traversal": "related_entities",
                "entity_id": Uuid::new_v4().to_string()
            }
        });
        let err = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap_err();
        assert_eq!(err.0, INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn hybrid_search_phonetic_only() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Seed an entity
        let entity = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Alice".into(),
            entity_type: "person".into(),
            source_fold_id: None,
            context_snippet: "Alice is the project lead".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
        };
        store.entities.lock().await.push(entity.clone());

        // Search without embedding — only phonetic strategy runs
        let params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Alice",
                "limit": 5
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        assert!(result["count"].as_u64().unwrap() >= 1);
        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["source"], "entity_phonetic");
        assert_eq!(results[0]["result_type"], "entity");
    }

    #[tokio::test]
    async fn hybrid_search_with_embedding() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Seed an entity
        let entity = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Bob".into(),
            entity_type: "person".into(),
            source_fold_id: None,
            context_snippet: "Bob is the CTO".into(),
            entity_embedding: Some(vec![0.1, 0.2, 0.3]),
            confidence: 0.95,
            state: Default::default(),
            created_at: chrono::Utc::now(),
        };
        store.entities.lock().await.push(entity);

        // Seed a completed fold
        let fold = crate::types::FoldEntry {
            session_id: sid,
            fold_id: Uuid::new_v4(),
            tenant_id: ctx.tenant_id,
            depth: 0,
            parent_fold_id: None,
            raw_trajectory: "discussed architecture".into(),
            fold_summary: Some("Architecture discussion summary".into()),
            fold_embedding: Some(vec![0.1, 0.2, 0.3]),
            token_count: 100,
            compression_ratio: Some(0.5),
            status: crate::types::FoldStatus::Folded,
            created_at: chrono::Utc::now(),
            folded_at: Some(chrono::Utc::now()),
        };
        store.folds.lock().await.push(fold);

        // Search with embedding — all 3 strategies can run
        let params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Bob",
                "embedding": [0.1, 0.2, 0.3],
                "limit": 10
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        // Should have results from multiple strategies
        let count = result["count"].as_u64().unwrap();
        assert!(count >= 2, "expected at least 2 results, got {count}");

        // Bob entity should appear (boosted by phonetic + ann fusion)
        let results = result["results"].as_array().unwrap();
        let sources: Vec<&str> = results
            .iter()
            .map(|r| r["source"].as_str().unwrap())
            .collect();
        assert!(
            sources.contains(&"entity_phonetic") || sources.contains(&"entity_ann"),
            "expected entity results"
        );
        assert!(sources.contains(&"fold_ann"), "expected fold results");
    }

    #[tokio::test]
    async fn get_stats_returns_zeroes_for_empty_session() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "get_stats",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["entity_count"], 0);
        assert_eq!(result["fold_count"], 0);
        assert_eq!(result["memo_count"], 0);
        assert_eq!(result["intention_count"], 0);
    }

    #[tokio::test]
    async fn intention_persists_to_storage() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        // Set an intention — should persist to mock storage
        let params = serde_json::json!({
            "name": "set_intention",
            "arguments": {
                "description": "Check SQL injection patterns",
                "trigger": { "type": "Topic", "keywords": ["sql", "injection"] },
                "priority": "high"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let intention_id = result["intention_id"].as_str().unwrap().to_string();

        // Verify it was persisted to storage
        let stored = store.intentions.lock().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id.to_string(), intention_id);
        assert_eq!(stored[0].description, "Check SQL injection patterns");
        drop(stored);

        // Read back from storage trait
        use crate::storage::Storage as _;
        let loaded = store.intention_list(&ctx).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id.to_string(), intention_id);
    }

    #[tokio::test]
    async fn intention_complete_persists_status() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        // Set an intention
        let params = serde_json::json!({
            "name": "set_intention",
            "arguments": {
                "description": "Review auth module",
                "trigger": { "type": "Topic", "keywords": ["auth"] },
                "priority": "normal"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let intention_id = result["intention_id"].as_str().unwrap().to_string();

        // Trigger it
        let params = serde_json::json!({
            "name": "check_intentions",
            "arguments": { "context": "working on auth middleware" }
        });
        dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();

        // Verify triggered status was persisted
        let stored = store.intentions.lock().await;
        assert_eq!(
            stored[0].status,
            crate::intention::IntentionStatus::Triggered
        );
        drop(stored);

        // Complete it
        let params = serde_json::json!({
            "name": "complete_intention",
            "arguments": { "intention_id": intention_id }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        assert_eq!(result["completed"], true);

        // Verify completed status was persisted
        let stored = store.intentions.lock().await;
        assert_eq!(
            stored[0].status,
            crate::intention::IntentionStatus::Completed
        );
        assert!(stored[0].completed_at.is_some());
    }

    #[tokio::test]
    async fn intention_load_from_storage() {
        use crate::intention::*;
        use crate::storage::Storage as _;

        let store = MockStorage::new();
        let ctx = test_ctx();

        // Simulate pre-existing intentions in storage (from previous session)
        let intention = Intention {
            id: Uuid::new_v4(),
            description: "Previously stored intention".into(),
            trigger: IntentionTrigger::Topic {
                keywords: vec!["rust".into()],
            },
            priority: Priority::High,
            status: IntentionStatus::Pending,
            created_at: chrono::Utc::now(),
            triggered_at: None,
            completed_at: None,
        };
        store.intention_put(&ctx, &intention).await.unwrap();

        // Load from storage into IntentionStore
        let loaded = store.intention_list(&ctx).await.unwrap();
        let mut intention_store = IntentionStore::new();
        intention_store.load(loaded);

        // Verify the loaded intention triggers correctly
        let triggered = intention_store.check("writing rust code");
        assert_eq!(triggered.len(), 1);
        assert_eq!(triggered[0].description, "Previously stored intention");
    }
}
