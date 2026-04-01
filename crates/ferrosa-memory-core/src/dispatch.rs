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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::Value;
use tokio::sync::Mutex;

use crate::transport::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};

/// Rotating hint counter for memory formation encouragement.
static HINT_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Pick a hint from a pool, rotating through them.
fn pick_hint(hints: &[&str]) -> String {
    let idx = HINT_COUNTER.fetch_add(1, Ordering::Relaxed) % hints.len();
    hints[idx].to_string()
}

/// Hints shown after successful smart_ingest (one per response, rotating).
const INGEST_HINTS: &[&str] = &[
    "Did you learn something about the user's preferences or working style? Ingest that too.",
    "Technical decisions and their reasoning are high-value memories — don't skip those.",
    "User corrections ('no, do it this way') are especially important to remember.",
    "Project constraints, deadlines, or blockers? Those are worth remembering.",
    "Did you notice an architecture pattern or library gotcha? Ingest it.",
    "People, roles, and relationships mentioned? Those build context over time.",
    "Debugging insights — what caused a bug, what fixed it — save future you the trouble.",
    "Configuration gotchas and environment details are easy to forget. Ingest them.",
    "What did you learn about how this codebase works? That's worth a smart_ingest.",
    "Any surprising behavior or non-obvious design choices? Those are prime memory candidates.",
];

/// Tracks per-entity retrieval counts for anomaly detection (FMEA F19).
#[derive(Default)]
pub struct RetrievalTracker {
    counts: std::collections::HashMap<uuid::Uuid, usize>,
    /// Ordered list of recently accessed entity IDs (most recent last).
    recent: Vec<uuid::Uuid>,
}

impl RetrievalTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, entity_id: uuid::Uuid) {
        *self.counts.entry(entity_id).or_insert(0) += 1;
        // Deduplicate in recent list — keep only the latest position.
        self.recent.retain(|&id| id != entity_id);
        self.recent.push(entity_id);
        // Cap at 50 to bound memory.
        if self.recent.len() > 50 {
            self.recent.remove(0);
        }
    }

    pub fn count(&self, entity_id: &uuid::Uuid) -> usize {
        self.counts.get(entity_id).copied().unwrap_or(0)
    }

    /// Returns the most recently accessed entity IDs (most recent last).
    pub fn recent_ids(&self, limit: usize) -> Vec<uuid::Uuid> {
        let start = self.recent.len().saturating_sub(limit);
        self.recent[start..].to_vec()
    }

    pub fn mean(&self) -> f64 {
        if self.counts.is_empty() {
            return 0.0;
        }
        let sum: usize = self.counts.values().sum();
        sum as f64 / self.counts.len() as f64
    }

    pub fn stddev(&self) -> f64 {
        if self.counts.len() < 2 {
            return 0.0;
        }
        let mean = self.mean();
        let variance = self
            .counts
            .values()
            .map(|&c| {
                let diff = c as f64 - mean;
                diff * diff
            })
            .sum::<f64>()
            / self.counts.len() as f64;
        variance.sqrt()
    }
}

/// Per-session mutable state (not persisted in CQL).
pub struct SessionState {
    pub intentions: Arc<Mutex<crate::intention::IntentionStore>>,
    pub graph: Option<Arc<crate::graph::GraphClient>>,
    pub event_bus: Arc<crate::viz::EventBus>,
    pub retrieval_tracker: Arc<Mutex<RetrievalTracker>>,
    pub co_access: Arc<Mutex<crate::speculative::CoAccessTracker>>,
    /// Configured default session_id for cross-session memory continuity.
    /// Falls back to random UUID if not set.
    pub default_session_id: Option<uuid::Uuid>,
    /// Notified on every tool call; used by the idle consolidation timer.
    pub last_activity: Arc<tokio::sync::Notify>,
    /// Set to true when a write tool succeeds; cleared by idle consolidation.
    pub dirty: Arc<AtomicBool>,
    /// Base URL for the Ollama API (used for NER extraction).
    pub ollama_base_url: String,
    /// Model name for NER entity extraction via Ollama.
    pub ner_model: String,
    /// Dynamic entity types loaded from the type registry table.
    pub entity_types: Vec<String>,
    /// Dynamic edge types loaded from the type registry table.
    pub edge_types: Vec<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            intentions: Arc::new(Mutex::new(crate::intention::IntentionStore::new())),
            graph: None,
            event_bus: Arc::new(crate::viz::EventBus::new()),
            retrieval_tracker: Arc::new(Mutex::new(RetrievalTracker::new())),
            co_access: Arc::new(Mutex::new(crate::speculative::CoAccessTracker::new(10))),
            default_session_id: None,
            last_activity: Arc::new(tokio::sync::Notify::new()),
            dirty: Arc::new(AtomicBool::new(false)),
            ollama_base_url: "http://localhost:11434".to_string(),
            ner_model: "qwen3.5:27b".to_string(),
            entity_types: vec![
                "person", "place", "event", "concept", "org", "bug",
                "decision", "pattern", "preference",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            edge_types: Vec::new(),
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
/// Entity types are loaded dynamically from the type registry.
pub fn tool_definitions(entity_types: &[String]) -> Vec<ToolDef> {
    let entity_type_enum: Value = serde_json::json!(entity_types);
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
                    "query": { "type": "string", "maxLength": 4096, "description": "Optional text query for routing optimization. If provided, the router selects optimal k and include_raw." },
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
                    "entity_type": { "type": "string", "enum": entity_type_enum },
                    "context_snippet": { "type": "string", "maxLength": 4096 },
                    "embedding": { "type": "array", "items": { "type": "number" } },
                    "source_fold_id": { "type": "string", "format": "uuid" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                },
                "required": ["session_id", "entity_name", "entity_type", "context_snippet"]
            }),
        },
        ToolDef {
            name: "batch_ingest".into(),
            description: "Batch ingest multiple entities in a single call.\n\n\
                CALL WHEN:\n\
                - Ingesting 5+ entities at once (codebase indexing, document extraction, bulk import)\n\
                - Performance matters — single round-trip instead of N sequential calls\n\n\
                Each entity follows the same schema as upsert_entity. Returns array of results.\n\n\
                Cost: ~15ms + 5ms per entity (vs 15ms per entity with individual calls).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid", "description": "Session UUID" },
                    "entities": {
                        "type": "array",
                        "description": "Array of entities to ingest",
                        "items": {
                            "type": "object",
                            "properties": {
                                "entity_name": { "type": "string", "maxLength": 512 },
                                "entity_type": { "type": "string", "enum": entity_type_enum },
                                "context_snippet": { "type": "string", "maxLength": 4096 },
                                "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                            },
                            "required": ["entity_name", "entity_type", "context_snippet"]
                        },
                        "maxItems": 100
                    }
                },
                "required": ["session_id", "entities"]
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
                "required": ["query"]
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
            description: "YOUR PRIMARY TOOL FOR BUILDING LONG-TERM MEMORY. Automatically decides whether to CREATE, UPDATE, SUPERSEDE, or SKIP based on what you already know.\n\nCALL AGGRESSIVELY — every time you encounter something worth remembering:\n- User preferences, habits, or working style\n- Technical decisions and WHY they were made\n- Architecture patterns, library choices, configuration gotchas\n- People, roles, relationships mentioned in conversation\n- Project context: goals, constraints, deadlines, blockers\n- Debugging insights: what caused a bug, what fixed it\n- Tool/framework knowledge: 'X works well for Y', 'avoid Z because...'\n- Domain knowledge: business rules, API behaviors, data models\n- Corrections: 'user said X is wrong, Y is correct'\n\nDO NOT CALL for: ephemeral task state (use plan tools), raw code (derivable from files), or content the user explicitly marks as temporary.\n\nThe prediction error gate handles dedup — calling too often is better than missing important information. If in doubt, ingest it.\n\nRETURNS: action taken (Created/Updated/Superseded/Skipped) + entity_id.\nCost: ~15ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "content": { "type": "string", "maxLength": 8192, "description": "The content to ingest" },
                    "entity_type": { "type": "string", "enum": entity_type_enum },
                    "entity_name": { "type": "string", "maxLength": 256, "description": "Clean entity name (e.g. 'Ben Kearns', 'Ferrosa'). If omitted, extracted automatically from content via LLM or heuristic." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding vector" },
                    "source_fold_id": { "type": "string", "format": "uuid", "description": "Fold that produced this content" }
                },
                "required": ["content", "entity_type"]
            }),
        },
        // --- Intention tools (prospective memory) ---
        ToolDef {
            name: "set_intention".into(),
            description: "Prospective memory — 'remember to do X when Y happens.' Sets a deferred action that auto-triggers on context match.\n\nCALL WHEN you notice something to do later:\n- 'When we touch auth, check the error handling'\n- 'Next time we open database.rs, add that index'\n- 'When user mentions deployment, remind about the TLS cert'\n- 'In 30 minutes, check if the build finished'\n\nTrigger types: Topic (keyword match), FilePattern (file glob), Duration (minutes), Context (flexible condition).\n\nIntentions persist across the session and trigger automatically when check_intentions runs. Set liberally — they cost nothing until triggered.\nCost: ~1ms.".into(),
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
            description: "Checks pending intentions against current context. Call FREQUENTLY — at every topic change, file open, or new task start. Pass a brief description of what you're doing now as context. Returns triggered intentions you should act on.\n\nCost: ~1ms. Call often — it's free.".into(),
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
            description: "Records a timestamped fact about an entity. Auto-supersedes the previous fact, preserving history.\n\nCALL WHEN facts change over time — this is how you track evolution:\n- Role changes: 'Alice is now VP' supersedes 'Alice is Director'\n- Status updates: 'deploy succeeded' supersedes 'deploy in progress'\n- Project state: 'using Rust 1.82' supersedes 'using Rust 1.78'\n- Preference changes: 'user prefers dark mode' supersedes 'user likes light mode'\n- Bug status: 'fixed in commit abc' supersedes 'investigating OOM'\n\nFirst call smart_ingest to create the entity, then write_temporal_fact for facts that evolve. The supersession chain is queryable — you can answer 'what was X before?'\n\nReturns: event_id of the new fact.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "entity_id": { "type": "string", "format": "uuid" },
                    "fact_text": { "type": "string", "maxLength": 4096, "description": "The fact to record" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1, "description": "Confidence score (default: 1.0)" }
                },
                "required": ["entity_id", "fact_text"]
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
                "required": ["entity_id"]
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
            description: "Search across ALL memory types at once — entities, folds, and facts — using Reciprocal Rank Fusion to merge results.\n\nCALL AT THE START OF EVERY NEW TASK or when the user asks about something that might have prior context. This is your 'what do I already know about this?' tool.\n\nExamples of when to search:\n- User mentions a project, person, or concept → search for prior context\n- Starting implementation → search for related decisions and patterns\n- Debugging → search for prior bugs in the same area\n- User asks 'remember when...' → search for the memory\n\nProvide embedding for ANN strategies; without it only phonetic matching runs.\nCost: ~15ms.".into(),
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
                "required": ["query"]
            }),
        },
        // --- Dream consolidation ---
        ToolDef {
            name: "run_consolidation".into(),
            description: "Dream consolidation — discovers hidden connections between memories. Groups entities by shared context, creates CO_OCCURS graph edges, identifies clusters.\n\nCALL WHEN:\n- After ingesting 5+ new memories in a session\n- At the end of a productive work session\n- When the user says 'wrap up' or 'that's it for now'\n- Periodically during long sessions (every ~30 minutes of active work)\n\nThis is what makes the knowledge graph useful — individual memories become a connected web of knowledge. The more you consolidate, the richer the graph.\nCost: scales with entity count, typically <100ms.".into(),
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
                "required": ["entity_id"]
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
                "required": ["entity_id"]
            }),
        },
        // --- Importance scoring ---
        ToolDef {
            name: "importance_score".into(),
            description: "Computes a 4-channel importance score for a memory entity: novelty (how surprising), arousal (emotional intensity), reward (past retrieval success), attention (recency/frequency).\n\nCALL WHEN: Prioritizing which memories to surface, deciding whether to consolidate or prune, or ranking retrieval results by relevance.\nRETURNS: Per-channel scores (0-1) and a weighted composite score.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "entity_id": { "type": "string", "format": "uuid" }
                },
                "required": ["entity_id"]
            }),
        },
        // --- Memory chains ---
        ToolDef {
            name: "find_memory_chain".into(),
            description: "Discovers the shortest path between two entities through the knowledge graph using BFS traversal. Returns the chain of intermediate entities and edge types connecting source to destination.\n\nCALL WHEN: You need to understand HOW two concepts are related — not just whether they are, but the path of connections between them. Useful for explaining reasoning chains, tracing provenance, or finding indirect relationships.\nRETURNS: Ordered list of steps (entity_id + edge_type) forming the shortest path, plus hop count and confidence score.\nCost: ~5-20ms depending on graph density.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "source": { "type": "string", "format": "uuid", "description": "Entity ID to start from" },
                    "destination": { "type": "string", "format": "uuid", "description": "Entity ID to find path to" },
                    "max_hops": { "type": "integer", "minimum": 1, "maximum": 10, "description": "Maximum path length (default: 5)" }
                },
                "required": ["session_id", "source", "destination"]
            }),
        },
        // --- Speculative retrieval ---
        ToolDef {
            name: "predict_needed".into(),
            description: "Predicts which entities will be needed based on co-access patterns. Analyzes which entities are frequently retrieved together and suggests entities likely to be needed given recent access history.\n\nCALL WHEN: After retrieving entities, to prefetch or surface related memories before they are explicitly requested.\nCost: ~1ms (in-memory co-access analysis).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "threshold": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Minimum confidence threshold (default: 0.3)"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "Maximum predictions to return (default: 10)"
                    }
                },
                "required": ["session_id"]
            }),
        },
        // --- Spreading activation ---
        ToolDef {
            name: "spread_activation".into(),
            description: "Spreading activation search (Collins & Loftus). Propagates activation energy from seed entities through the knowledge graph, decaying at each hop. Returns the most activated non-seed entities.\n\nCALL WHEN: You have one or more known entities and want to discover related entities through graph structure — especially when semantic search alone misses structural relationships.\nPair with retrieve_entities for seeds, then spread to find indirect connections.\nCost: ~10-50ms depending on graph density and max_hops.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "seeds": {
                        "type": "array",
                        "items": { "type": "string", "format": "uuid" },
                        "minItems": 1,
                        "description": "Entity IDs to start activation from"
                    },
                    "max_hops": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Maximum traversal depth (default: 2)" },
                    "decay": { "type": "number", "minimum": 0.01, "maximum": 1.0, "description": "Activation decay per hop (default: 0.7)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Max results to return (default: 10)" }
                },
                "required": ["session_id", "seeds"]
            }),
        },
        // --- Duplicate detection ---
        ToolDef {
            name: "find_duplicates".into(),
            description: "Scans a session\'s entities for potential duplicates using text similarity (Jaccard coefficient) on context snippets. Returns pairs above the threshold, sorted by similarity descending.\n\nCALL WHEN: After bulk entity ingestion, or when you suspect duplicate entities exist in a session. Useful before consolidation to identify merge candidates.\nDO NOT CALL: On sessions with very few entities (< 3). Use retrieve_entities with phonetic matching for single-entity dedup.\nCost: O(n^2) comparisons -- fast for <1000 entities per session.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "threshold": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1,
                        "description": "Similarity threshold (0-1). Default: 0.7. Higher = fewer, more confident matches."
                    }
                },
                "required": ["session_id"]
            }),
        },
        // --- Recursive exploration ---
        ToolDef {
            name: "recursive_explore".into(),
            description: "Recursive multi-pass query exploration with Datalog-driven discovery.\n\n\
                CALL WHEN:\n\
                - Complex multi-hop queries that need connected knowledge clusters\n\
                - Queries involving relationships between entities\n\
                - When hybrid_search returns too few results\n\n\
                DO NOT CALL:\n\
                - For simple name lookups (use retrieve_entities)\n\
                - For direct entity retrieval by ID\n\n\
                Cost: Multiple passes × hybrid_search cost. Bounded by max_passes.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID" },
                    "query": { "type": "string", "description": "Search query to explore recursively" },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional query embedding vector" },
                    "max_passes": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Max exploration passes (default 3)" },
                    "convergence_threshold": { "type": "number", "minimum": 0.0, "maximum": 1.0, "description": "Novelty ratio for convergence (default 0.1)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Max results (default 20)" }
                },
                "required": ["query"]
            }),
        },
        // --- Datalog query ---
        ToolDef {
            name: "query_derived".into(),
            description: "Query Datalog-derived facts with provenance.\n\n\
                CALL WHEN:\n\
                - You need to explain why entity A relates to entity B\n\
                - You want transitive closure (related, reachable, isa)\n\
                - You need derived facts with explanation chains\n\n\
                DO NOT CALL:\n\
                - For raw entity retrieval (use retrieve_entities)\n\n\
                Cost: Cache hit is free. Cache miss computes Datalog evaluation.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID" },
                    "predicate": { "type": "string", "description": "Derived predicate to query (e.g., 'related', 'reachable', 'isa', 'cluster')" }
                },
                "required": ["predicate"]
            }),
        },
        // --- Datalog rule management ---
        ToolDef {
            name: "manage_rules".into(),
            description: "CRUD for Datalog rule registry.\n\n\
                CALL WHEN:\n\
                - Adding custom inference rules\n\
                - Listing active rules\n\
                - Deprecating old rules\n\n\
                Cost: Low (registry operations).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "put", "deprecate"], "description": "CRUD action" },
                    "rule_id": { "type": "string", "description": "Rule ID (for get/put/deprecate)" },
                    "family": { "type": "string", "description": "Rule family (for list/put)" },
                    "rule_body": { "type": "string", "description": "Datalog rule text (for put)" },
                    "name": { "type": "string", "description": "Human-readable name (for put)" },
                    "rule_weight": { "type": "number", "description": "Rule confidence weight (default 1.0)" }
                },
                "required": ["action"]
            }),
        },
        // --- Predicate promotion ---
        ToolDef {
            name: "promote_predicate".into(),
            description: "Promote a derived predicate to durable materialization.\n\n\
                CALL WHEN:\n\
                - A derived predicate is queried frequently and you want faster access\n\
                - You want to persist inference results beyond the ephemeral cache TTL\n\n\
                Cost: Runs Datalog evaluation + writes to durable tables.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID" },
                    "predicate": { "type": "string", "description": "Predicate to promote (e.g., 'related', 'isa', 'reachable')" }
                },
                "required": ["predicate"]
            }),
        },
        // --- Typed edge tools ---
        ToolDef {
            name: "create_edge".into(),
            description: "Create a typed, labeled edge between two entities.\n\n\
                CALL WHEN:\n\
                - Building a knowledge graph with semantic relationships\n\
                - Recording dependencies (depends_on), containment (contains), inheritance (subclass_of)\n\
                - Any time you discover a specific relationship between entities\n\n\
                Edge types: depends_on, contains, part_of, subclass_of, calls, implements, uses, related_to\n\n\
                Cost: ~5ms per edge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "src_entity_id": { "type": "string", "format": "uuid", "description": "Source entity UUID" },
                    "dst_entity_id": { "type": "string", "format": "uuid", "description": "Destination entity UUID" },
                    "edge_type": { "type": "string", "description": "Relationship type (depends_on, contains, part_of, subclass_of, calls, implements, uses)" },
                    "weight": { "type": "number", "minimum": 0, "maximum": 1, "description": "Edge strength (default 1.0)" },
                    "metadata": { "type": "string", "description": "Optional metadata about the relationship" }
                },
                "required": ["session_id", "src_entity_id", "dst_entity_id", "edge_type"]
            }),
        },
        ToolDef {
            name: "batch_create_edges".into(),
            description: "Create multiple typed edges in a single call.\n\n\
                Cost: ~5ms + 2ms per edge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "edges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "src_entity_id": { "type": "string", "format": "uuid" },
                                "dst_entity_id": { "type": "string", "format": "uuid" },
                                "edge_type": { "type": "string" },
                                "weight": { "type": "number" }
                            },
                            "required": ["src_entity_id", "dst_entity_id", "edge_type"]
                        },
                        "maxItems": 200
                    }
                },
                "required": ["session_id", "edges"]
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
            let tools = tool_definitions(&session.entity_types);
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

    let mut args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    // Inject configured default session_id when caller omits it.
    if args.get("session_id").and_then(|v| v.as_str()).is_none()
        && let Some(default_sid) = session.default_session_id
        && let Some(obj) = args.as_object_mut()
    {
        obj.insert("session_id".into(), Value::String(default_sid.to_string()));
    }

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
        "complete_fold" => handle_complete_fold(args, storage, ctx, session).await,
        "retrieve_fold_context" => handle_retrieve_fold(args, storage, ctx).await,
        "upsert_entity" => handle_upsert_entity(args, storage, ctx, session).await,
        "batch_ingest" => handle_batch_ingest(args, storage, ctx, session).await,
        "retrieve_entities" => handle_retrieve_entities(args, storage, ctx, session).await,
        "record_outcome" => handle_record_outcome(args, storage, ctx).await,
        "delete_session" => handle_delete_session(args, storage, ctx).await,
        "smart_ingest" => handle_smart_ingest(args, storage, ctx, session).await,
        "set_intention" => handle_set_intention(args, storage, ctx, session).await,
        "check_intentions" => handle_check_intentions(args, storage, ctx, session).await,
        "complete_intention" => handle_complete_intention(args, storage, ctx, session).await,
        "list_intentions" => handle_list_intentions(session).await,
        "snooze_intention" => handle_snooze_intention(args, storage, ctx, session).await,
        "write_temporal_fact" => handle_write_temporal_fact(args, storage, ctx, session).await,
        "get_temporal_chain" => handle_get_temporal_chain(args, storage, ctx).await,
        "explore_connections" => handle_explore_connections(args, session).await,
        "hybrid_search" => handle_hybrid_search(args, storage, ctx, session).await,
        "run_consolidation" => handle_run_consolidation(args, storage, ctx, session).await,
        "get_stats" => handle_get_stats(args, storage, ctx, session).await,
        "promote_memory" => handle_promote_memory(args, storage, ctx, session).await,
        "demote_memory" => handle_demote_memory(args, storage, ctx, session).await,
        "importance_score" => handle_importance_score(args, storage, ctx, session).await,
        "find_memory_chain" => handle_find_memory_chain(args, storage, ctx).await,
        "predict_needed" => handle_predict_needed(args, session).await,
        "spread_activation" => handle_spread_activation(args, storage, ctx).await,
        "find_duplicates" => handle_find_duplicates(args, storage, ctx).await,
        "recursive_explore" => handle_recursive_explore(args, storage, ctx, session).await,
        "query_derived" => handle_query_derived(args, storage, ctx).await,
        "manage_rules" => handle_manage_rules(args, storage, ctx).await,
        "promote_predicate" => handle_promote_predicate(args, storage, ctx).await,
        "create_edge" => handle_create_edge(args, storage, ctx, session).await,
        "batch_create_edges" => handle_batch_create_edges(args, storage, ctx, session).await,
        _ => Err((METHOD_NOT_FOUND, format!("unknown tool: {name}"))),
    };
    let elapsed = start.elapsed();
    match &result {
        Ok(v) => {
            let bytes = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
            tracing::debug!(
                tool = name,
                elapsed_ms = elapsed.as_millis() as u64,
                response_bytes = bytes,
                est_tokens = bytes / 4,
                "tool call OK"
            );
        }
        Err((code, msg)) => tracing::warn!(
            tool = name,
            code,
            msg,
            elapsed_ms = elapsed.as_millis() as u64,
            "tool call FAILED"
        ),
    }

    // Signal activity for idle consolidation timer.
    session.last_activity.notify_one();

    // Mark dirty on successful write operations so idle consolidation knows
    // there is new data worth processing.
    if result.is_ok() && is_write_tool(name) {
        session.dirty.store(true, Ordering::Relaxed);
    }

    // Wrap in MCP CallToolResult format: { content: [{type: "text", text: "..."}] }
    // MCP clients expect this structure; without it, tool output is invisible.
    result.map(|value| {
        let text = if value.is_string() {
            value.as_str().unwrap().to_string()
        } else {
            serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
        };
        serde_json::json!({
            "content": [{"type": "text", "text": text}]
        })
    })
}

/// Returns true for tools that modify stored data (writes, upserts, deletes).
/// Used to set the dirty flag for idle consolidation.
fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "store_memo_result"
            | "write_plan_node"
            | "update_plan_node"
            | "start_fold"
            | "append_to_fold"
            | "complete_fold"
            | "upsert_entity"
            | "batch_ingest"
            | "record_outcome"
            | "delete_session"
            | "smart_ingest"
            | "set_intention"
            | "complete_intention"
            | "snooze_intention"
            | "write_temporal_fact"
            | "promote_memory"
            | "demote_memory"
            | "manage_rules"
            | "promote_predicate"
            | "create_edge"
            | "batch_create_edges"
    )
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

    let mut json = serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    if let Some(obj) = json.as_object_mut() {
        let hint = if result.hit {
            "Cache hit — reusing prior result. Record outcome with record_outcome for routing optimization."
        } else {
            "Cache miss. After completing the sub-call, store the result with store_memo_result."
        };
        obj.insert("hint".into(), Value::String(hint.into()));
    }
    Ok(json)
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
        .map_err(map_quota_error)?;

    // Audit log (best-effort, no session_id for memos)
    let content_hash = result.content_hash.clone();
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "store",
        "memo_cache",
        &content_hash,
        uuid::Uuid::nil(),
    )
    .await;

    serde_json::to_value(result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_write_plan<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
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
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let fold_id = require_uuid(&args, "fold_id")?;
    let summary = require_str(&args, "summary")?;
    let embedding = require_f32_array(&args, "embedding")?;

    let (folded, compression_ratio) =
        crate::fold::complete_fold(storage, ctx, session_id, fold_id, summary, embedding)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Emit viz event for fold completion
    session.event_bus.emit(crate::viz::VizEvent::FoldCompleted {
        fold_id: fold_id.to_string(),
        summary: summary.to_string(),
        entity_count: 0, // entity count not tracked at fold level
    });

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "complete",
        "trajectory_folds",
        &fold_id.to_string(),
        session_id,
    )
    .await;

    Ok(serde_json::json!({ "folded": folded, "compression_ratio": compression_ratio }))
}

async fn handle_retrieve_fold<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let query_embedding = require_f32_array(&args, "query_embedding")?;
    let query_text = args.get("query").and_then(|v| v.as_str()).unwrap_or("");

    // Use router for strategy selection
    let decision = crate::router::route(&crate::router::RoutingContext {
        query_text,
        has_entity_name: false,
        has_content_hash: false,
        task_complexity: crate::router::TaskComplexity::Simple,
    });

    // User-provided k and include_raw override the router's suggestion
    let k = args
        .get("k")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .or(Some(decision.k));
    let include_raw = args
        .get("include_raw")
        .and_then(|v| v.as_bool())
        .unwrap_or(decision.include_raw);

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
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let entity_name = require_str(&args, "entity_name")?;
    let entity_type = require_str(&args, "entity_type")?;
    let context_snippet = require_str(&args, "context_snippet")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let source_fold_id = optional_uuid(&args, "source_fold_id")?;
    let confidence = args.get("confidence").and_then(|v| v.as_f64());

    // Auto-generate embedding if not provided and Ollama is configured.
    if embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(
            &crate::config::EmbeddingConfig {
                provider: "ollama".into(),
                ollama_base_url: session.ollama_base_url.clone(),
                model: "nomic-embed-text".into(),
                dimensions: 768,
                ner_model: String::new(),
            },
        );
        match client.embed(context_snippet).await {
            Ok(emb) => embedding = Some(emb),
            Err(e) => tracing::debug!("embedding generation skipped: {e}"),
        }
    }

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
    .map_err(map_quota_error)?;

    let result_json = serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Emit viz event for entity upsert
    let entity_id = result_json
        .get("entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_new = result_json
        .get("is_new")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let action = if is_new { "created" } else { "updated" };

    session.event_bus.emit(crate::viz::VizEvent::EntityChanged {
        node: crate::viz::VizNode {
            id: entity_id.clone(),
            label: entity_name.to_string(),
            node_type: "entity".into(),
            entity_type: entity_type.to_string(),
            state: "active".into(),
            confidence: confidence.unwrap_or(1.0),
            created_at: chrono::Utc::now().to_rfc3339(),
            context: context_snippet.to_string(),
        },
        action: action.into(),
    });

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "upsert",
        "entity_store",
        &entity_id,
        session_id,
    )
    .await;

    Ok(result_json)
}

async fn handle_batch_ingest<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let entities = args
        .get("entities")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "entities must be an array".to_string()))?;

    if entities.len() > 100 {
        return Err((
            INVALID_PARAMS,
            format!(
                "entities array length {} exceeds maximum of 100",
                entities.len()
            ),
        ));
    }

    let mut results = Vec::with_capacity(entities.len());
    let mut created: usize = 0;
    let mut skipped: usize = 0;
    let mut errors: usize = 0;

    for (i, entity_json) in entities.iter().enumerate() {
        let name = entity_json
            .get("entity_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let entity_type = entity_json
            .get("entity_type")
            .and_then(|v| v.as_str())
            .unwrap_or("concept");
        let context = entity_json
            .get("context_snippet")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let confidence = entity_json.get("confidence").and_then(|v| v.as_f64());

        if name.is_empty() || context.is_empty() {
            skipped += 1;
            results.push(serde_json::json!({
                "index": i, "status": "skipped", "reason": "empty name or context"
            }));
            continue;
        }

        match crate::entity::upsert_entity(
            storage,
            ctx,
            session_id,
            name,
            entity_type,
            context,
            None,
            None,
            confidence,
        )
        .await
        {
            Ok(result) => {
                let status = if result.is_new { "created" } else { "existing" };
                if result.is_new {
                    created += 1;
                } else {
                    skipped += 1;
                }

                // Emit viz event for each entity
                session.event_bus.emit(crate::viz::VizEvent::EntityChanged {
                    node: crate::viz::VizNode {
                        id: result.entity_id.to_string(),
                        label: name.to_string(),
                        node_type: "entity".into(),
                        entity_type: entity_type.to_string(),
                        state: "active".into(),
                        confidence: confidence.unwrap_or(1.0),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        context: context.to_string(),
                    },
                    action: status.into(),
                });

                results.push(serde_json::json!({
                    "index": i,
                    "status": status,
                    "entity_id": result.entity_id.to_string(),
                }));
            }
            Err(e) => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": i, "status": "error", "error": e.to_string()
                }));
            }
        }
    }

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "batch_ingest",
        "entity_store",
        &format!("{} entities", entities.len()),
        session_id,
    )
    .await;

    Ok(serde_json::json!({
        "created": created,
        "skipped": skipped,
        "errors": errors,
        "total": entities.len(),
        "results": results,
        "hint": format!(
            "Batch ingested {} entities. Run run_consolidation to build edges.",
            entities.len()
        )
    }))
}

async fn handle_retrieve_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let query = require_str(&args, "query")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;

    // Auto-generate query embedding for ANN search if Ollama is configured.
    if embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(
            &crate::config::EmbeddingConfig {
                provider: "ollama".into(),
                ollama_base_url: session.ollama_base_url.clone(),
                model: "nomic-embed-text".into(),
                dimensions: 768,
                ner_model: String::new(),
            },
        );
        match client.embed(query).await {
            Ok(emb) => embedding = Some(emb),
            Err(e) => tracing::debug!("query embedding generation skipped: {e}"),
        }
    }

    // Use router for strategy selection when user didn't specify
    let user_strategy = args.get("strategy").and_then(|v| v.as_str());
    let decision = crate::router::route(&crate::router::RoutingContext {
        query_text: query,
        has_entity_name: true, // entity retrieval implies entity context
        has_content_hash: false,
        task_complexity: crate::router::TaskComplexity::Simple,
    });
    let router_strategy = match decision.strategy {
        crate::router::Strategy::Phonetic => "phonetic",
        crate::router::Strategy::HnswAnn => "ann",
        _ => "both",
    };
    let strategy = user_strategy.unwrap_or(router_strategy);
    let k = args
        .get("k")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .or(Some(decision.k));

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

    // Track retrieval frequency and check for anomalies (STRIDE T1 / FMEA F19)
    let security_config = crate::config::SecurityConfig::default();
    let mut tracker = session.retrieval_tracker.lock().await;
    let mut co_access = session.co_access.lock().await;
    for entity in &entities {
        tracker.record(entity.entity_id);
        co_access.record(entity.entity_id);
        let count = tracker.count(&entity.entity_id);
        let mean = tracker.mean();
        let stddev = tracker.stddev();
        if crate::audit::check_anomaly(count, mean, stddev, &security_config, None) {
            tracing::warn!(
                entity_id = %entity.entity_id,
                count,
                "anomalous retrieval frequency"
            );
            // Emit anomaly alert via event bus (Sprint 4.9)
            if security_config.anomaly_alerts_enabled {
                session
                    .event_bus
                    .emit(crate::viz::VizEvent::AnomalyDetected {
                        entity_id: entity.entity_id.to_string(),
                        entity_name: entity.entity_name.clone(),
                        retrieval_count: count,
                        session_mean: mean,
                        session_stddev: stddev,
                        sigma_threshold: security_config.anomaly_sigma_threshold,
                    });
            }
        }
    }

    // Warmth boost for retrieved entities (fire-and-forget)
    let rmh_config = crate::config::RmhConfig::default();
    for entity in &entities {
        let _ = crate::warmth::boost_on_access(
            storage,
            ctx,
            entity.entity_id,
            session_id,
            &crate::types::DecayZone::Knowledge,
            &rmh_config,
        )
        .await;
    }

    // Strip embeddings and truncate context to reduce MCP response token cost.
    let slim: Vec<Value> = entities
        .iter()
        .map(|e| {
            let ctx = if e.context_snippet.len() > 200 {
                format!("{}...", &e.context_snippet[..e.context_snippet.floor_char_boundary(200)])
            } else {
                e.context_snippet.clone()
            };
            serde_json::json!({
                "entity_id": e.entity_id,
                "entity_name": e.entity_name,
                "entity_type": e.entity_type,
                "confidence": e.confidence,
                "state": e.state,
                "created_at": e.created_at,
                "context_snippet": ctx,
            })
        })
        .collect();
    serde_json::to_value(&slim).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Memory state handlers ---

async fn handle_promote_memory<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let entity_id = require_uuid(&args, "entity_id")?;

    let new_state = crate::entity::promote_memory(storage, ctx, session_id, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    session.event_bus.emit(crate::viz::VizEvent::StateChanged {
        entity_id: entity_id.to_string(),
        new_state: new_state.to_string(),
    });

    Ok(serde_json::json!({ "new_state": new_state.to_string() }))
}

async fn handle_demote_memory<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let entity_id = require_uuid(&args, "entity_id")?;

    let new_state = crate::entity::demote_memory(storage, ctx, session_id, entity_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    session.event_bus.emit(crate::viz::VizEvent::StateChanged {
        entity_id: entity_id.to_string(),
        new_state: new_state.to_string(),
    });

    Ok(serde_json::json!({ "new_state": new_state.to_string() }))
}

// --- Importance scoring handler ---

async fn handle_importance_score<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let entity_id = require_uuid(&args, "entity_id")?;

    // Look up the entity to get created_at for recency
    let entities = storage
        .entity_list_session(ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let entity = entities
        .iter()
        .find(|e| e.entity_id == entity_id)
        .ok_or_else(|| (INVALID_PARAMS, format!("entity not found: {entity_id}")))?;

    let last_accessed_seconds_ago = (chrono::Utc::now() - entity.created_at).num_seconds();

    // Use retrieval tracker for attention signal
    let tracker = session.retrieval_tracker.lock().await;
    let retrieval_count = tracker.count(&entity_id);
    drop(tracker);

    // Defaults for channels we cannot yet compute without embedding similarity
    let similarity_to_existing = 0.0;
    let feedback_success_rate = 0.0;

    let score = crate::importance::compute_importance(
        similarity_to_existing,
        retrieval_count,
        last_accessed_seconds_ago,
        feedback_success_rate,
    );

    serde_json::to_value(&score).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Feedback handler ---

async fn handle_record_outcome<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);

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
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let content = require_str(&args, "content")?;
    let entity_type = require_str(&args, "entity_type")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let source_fold_id = optional_uuid(&args, "source_fold_id")?;

    // Auto-generate embedding if not provided and Ollama is configured.
    if embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(
            &crate::config::EmbeddingConfig {
                provider: "ollama".into(),
                ollama_base_url: session.ollama_base_url.clone(),
                model: "nomic-embed-text".into(),
                dimensions: 768,
                ner_model: String::new(),
            },
        );
        match client.embed(content).await {
            Ok(emb) => embedding = Some(emb),
            Err(e) => tracing::debug!("embedding generation skipped: {e}"),
        }
    }

    let entity_name = args
        .get("entity_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let ner_config = crate::smart_ingest::NerConfig {
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default(),
        ollama_base_url: session.ollama_base_url.clone(),
        model: session.ner_model.clone(),
    };

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
        entity_name.as_deref(),
        Some(&ner_config),
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Emit viz event based on ingest decision
    let decision_json =
        serde_json::to_value(&decision).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let action = decision_json
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let entity_id = decision_json
        .get("entity_id")
        .or_else(|| decision_json.get("new_entity_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if !entity_id.is_empty() {
        session.event_bus.emit(crate::viz::VizEvent::EntityChanged {
            node: crate::viz::VizNode {
                id: entity_id.clone(),
                label: content.chars().take(64).collect(),
                node_type: "entity".into(),
                entity_type: entity_type.to_string(),
                state: "active".into(),
                confidence: 1.0,
                created_at: chrono::Utc::now().to_rfc3339(),
                context: content.chars().take(256).collect(),
            },
            action: action.clone(),
        });
    }

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "ingest",
        "entity_store",
        &entity_id,
        session_id,
    )
    .await;

    // Add rotating hint to encourage continued memory formation
    let mut result = decision_json;
    if let Some(obj) = result.as_object_mut() {
        let hint = match action.as_str() {
            "Skipped" => "Content too similar to existing memory. Try a different aspect or more specific insight.".to_string(),
            _ => pick_hint(INGEST_HINTS),
        };
        obj.insert("hint".into(), Value::String(hint));
    }
    Ok(result)
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
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
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

    // Emit viz event for temporal fact
    session.event_bus.emit(crate::viz::VizEvent::FactUpdated {
        entity_id: entity_id.to_string(),
        fact_text: fact_text.to_string(),
        superseded: None,
    });

    // Audit log (best-effort)
    let _ = crate::audit::log_write(
        storage,
        ctx,
        "write",
        "temporal_events",
        &event_id.to_string(),
        session_id,
    )
    .await;

    Ok(serde_json::json!({ "event_id": event_id.to_string() }))
}

async fn handle_get_temporal_chain<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let _session_id = optional_uuid(&args, "session_id")?;
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
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
            graph
                .get_fold_ancestors(fold_id, session_id, max_depth)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        }
        "related_entities" => {
            let entity_id = require_uuid(&args, "entity_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
            let mut r = graph
                .find_related_entities(entity_id, session_id, max_depth)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            r.truncate(limit);
            r
        }
        "entities_in_fold" => {
            let fold_id = require_uuid(&args, "fold_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
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
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let query = require_str(&args, "query")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    // Auto-generate query embedding for ANN search if Ollama is configured.
    if embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(
            &crate::config::EmbeddingConfig {
                provider: "ollama".into(),
                ollama_base_url: session.ollama_base_url.clone(),
                model: "nomic-embed-text".into(),
                dimensions: 768,
                ner_model: String::new(),
            },
        );
        match client.embed(query).await {
            Ok(emb) => embedding = Some(emb),
            Err(e) => tracing::debug!("query embedding generation skipped: {e}"),
        }
    }

    let results = crate::hybrid_search::hybrid_search(
        storage,
        ctx,
        session_id,
        query,
        embedding.as_deref(),
        limit,
        None,
        None,
        &crate::hybrid_search::FusionConfig::default(),
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let hint = if results.is_empty() {
        pick_hint(&[
            "No matches — this topic is new to memory. Good candidate for smart_ingest.",
            "Empty search. Ingest what you're learning in this conversation with smart_ingest.",
            "Nothing found. Have you captured the key insights from this session yet?",
        ])
    } else {
        pick_hint(&[
            "Found prior context. Use spread_activation on result entity_ids to discover related memories.",
            "Prior context found. Are there new insights to add with smart_ingest?",
            "Check if any of these memories need updating with new information from this conversation.",
        ])
    };

    Ok(serde_json::json!({
        "results": results,
        "count": results.len(),
        "hint": hint
    }))
}

// --- Dream consolidation handler ---

async fn handle_run_consolidation<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);

    let result = crate::dream::run_consolidation(storage, ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Emit viz events with actual entity pairs
    for (src, tgt) in &result.edges {
        session.event_bus.emit(crate::viz::VizEvent::EdgeCreated {
            edge: crate::viz::VizEdge {
                source: src.to_string(),
                target: tgt.to_string(),
                edge_type: "CO_OCCURS".into(),
                strength: None,
            },
        });
    }

    let mut json = serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    if let Some(obj) = json.as_object_mut() {
        obj.insert(
            "hint".into(),
            Value::String(
                "Consolidation complete. New connections are visible in the viz dashboard. Continue ingesting new learnings.".into(),
            ),
        );
    }
    Ok(json)
}

// --- Stats handler ---

async fn handle_get_stats<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?;

    let memo_count = storage.memo_count(ctx).await.unwrap_or(0);
    let memo_total_hits = storage.memo_total_hits(ctx).await.unwrap_or(0);
    let memo_hit_rate = if memo_count > 0 {
        memo_total_hits as f64 / memo_count as f64
    } else {
        0.0
    };

    let entity_count = match session_id {
        Some(sid) => storage.entity_count(ctx, sid).await.unwrap_or(0),
        None => 0,
    };

    let active_fold_count = storage
        .fold_count_by_status(ctx, crate::types::FoldStatus::Active)
        .await
        .unwrap_or(0);
    let folded_count = storage
        .fold_count_by_status(ctx, crate::types::FoldStatus::Folded)
        .await
        .unwrap_or(0);
    let archived_fold_count = storage
        .fold_count_by_status(ctx, crate::types::FoldStatus::Archived)
        .await
        .unwrap_or(0);

    let temporal_fact_count = storage.temporal_count(ctx).await.unwrap_or(0);
    let edge_count = storage.edge_count(ctx).await.unwrap_or(0);
    let intention_count = session.intentions.lock().await.list().len();

    let hint = if entity_count == 0 {
        "Memory is empty. Start ingesting entities, decisions, and patterns with smart_ingest."
            .to_string()
    } else if edge_count == 0 {
        "Entities exist but no connections. Run run_consolidation to discover relationships."
            .to_string()
    } else {
        pick_hint(&[
            "Memory healthy. Remember to ingest new insights from this conversation.",
            "Have you learned anything about the user's preferences? Ingest with smart_ingest.",
            "Project context, architecture decisions, and debugging insights are worth remembering.",
        ])
    };

    Ok(serde_json::json!({
        "memo_count": memo_count,
        "memo_total_hits": memo_total_hits,
        "memo_hit_rate": memo_hit_rate,
        "entity_count": entity_count,
        "fold_count": active_fold_count + folded_count + archived_fold_count,
        "active_fold_count": active_fold_count,
        "folded_count": folded_count,
        "archived_fold_count": archived_fold_count,
        "temporal_fact_count": temporal_fact_count,
        "edge_count": edge_count,
        "intention_count": intention_count,
        "hint": hint
    }))
}

// --- Spreading activation handler ---

async fn handle_find_memory_chain<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let source = require_uuid(&args, "source")?;
    let destination = require_uuid(&args, "destination")?;
    let max_hops = args
        .get("max_hops")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(5);

    let chain = crate::chains::find_chain(storage, ctx, source, destination, max_hops)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    match chain {
        Some(ref c) => {
            // Warmth boost for entities on the path (fire-and-forget)
            let rmh_config = crate::config::RmhConfig::default();
            for step in &c.steps {
                let _ = crate::warmth::boost_on_access(
                    storage,
                    ctx,
                    step.entity_id,
                    session_id,
                    &crate::types::DecayZone::Knowledge,
                    &rmh_config,
                )
                .await;
            }
            serde_json::to_value(c).map_err(|e| (INTERNAL_ERROR, e.to_string()))
        }
        None => Ok(serde_json::json!({
            "found": false,
            "source": source.to_string(),
            "destination": destination.to_string(),
            "message": "No path found within max_hops"
        })),
    }
}

async fn handle_predict_needed(
    args: Value,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let _session_id = optional_uuid(&args, "session_id")?;
    let threshold = args
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.3);
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    if !(0.0..=1.0).contains(&threshold) {
        return Err((
            INVALID_PARAMS,
            "threshold must be between 0.0 and 1.0".into(),
        ));
    }
    if !(1..=20).contains(&limit) {
        return Err((INVALID_PARAMS, "limit must be between 1 and 20".into()));
    }

    let tracker = session.retrieval_tracker.lock().await;
    let recent = tracker.recent_ids(10);
    drop(tracker);

    let co_access = session.co_access.lock().await;
    let predictions = co_access.predict(&recent, threshold, limit);
    drop(co_access);

    Ok(serde_json::json!({
        "predictions": predictions,
        "count": predictions.len(),
        "recent_entity_count": recent.len(),
    }))
}

async fn handle_spread_activation<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);

    let seeds_arr = args
        .get("seeds")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "missing required array: seeds".into()))?;
    let seeds: Vec<uuid::Uuid> = seeds_arr
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or((INVALID_PARAMS, "seed must be a uuid string".into()))
                .and_then(|s| {
                    uuid::Uuid::parse_str(s)
                        .map_err(|e| (INVALID_PARAMS, format!("invalid seed uuid: {e}")))
                })
        })
        .collect::<Result<_, _>>()?;

    let max_hops = args
        .get("max_hops")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(2);
    let decay = args.get("decay").and_then(|v| v.as_f64()).unwrap_or(0.7);
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(10);

    let results = crate::spreading::spread(storage, ctx, &seeds, max_hops, decay, limit)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    // Warmth boost for seeds and top activated results (fire-and-forget)
    let rmh_config = crate::config::RmhConfig::default();
    for seed_id in &seeds {
        let _ = crate::warmth::boost_on_access(
            storage,
            ctx,
            *seed_id,
            session_id,
            &crate::types::DecayZone::Knowledge,
            &rmh_config,
        )
        .await;
    }
    for activated in &results {
        let _ = crate::warmth::boost_on_access(
            storage,
            ctx,
            activated.entity_id,
            session_id,
            &crate::types::DecayZone::Knowledge,
            &rmh_config,
        )
        .await;
    }

    Ok(serde_json::json!({
        "activated": results,
        "count": results.len()
    }))
}

// --- Recursive explore / Datalog / Rule handlers ---

async fn handle_recursive_explore<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let query = require_str(&args, "query")?;
    let session_id = optional_uuid(&args, "session_id")?
        .or(session.default_session_id)
        .unwrap_or_else(uuid::Uuid::new_v4);

    // Parse optional embedding
    let embedding: Option<Vec<f32>> = args.get("embedding").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect()
    });
    let embedding_ref = embedding.as_deref();

    let mut rmh_config = crate::config::RmhConfig::default();
    if let Some(mp) = args.get("max_passes").and_then(|v| v.as_u64()) {
        rmh_config.max_explore_passes = mp as usize;
    }
    if let Some(ct) = args.get("convergence_threshold").and_then(|v| v.as_f64()) {
        rmh_config.convergence_threshold = ct;
    }
    let datalog_config = crate::config::DatalogConfig::default();

    let result = crate::recursive_explore::explore(
        storage,
        ctx,
        session_id,
        query,
        embedding_ref,
        &rmh_config,
        &datalog_config,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
    let results: Vec<Value> = result
        .results
        .iter()
        .take(limit)
        .map(|r| {
            serde_json::json!({
                "id": r.id.to_string(),
                "source": r.source,
                "content": r.content,
                "score": r.score,
                "result_type": r.result_type,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "results": results,
        "count": results.len(),
        "sub_queries": result.sub_queries.iter().map(|sq| {
            serde_json::json!({ "query": sq.query_text, "reasoning": sq.reasoning })
        }).collect::<Vec<_>>(),
        "passes": result.passes,
        "converged": result.converged,
        "derived_facts_count": result.derived_facts_count,
        "hint": if results.is_empty() {
            "No results found. Try smart_ingest to add entities first."
        } else {
            "Recursive exploration found connected knowledge clusters."
        }
    }))
}

async fn handle_query_derived<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let predicate = require_str(&args, "predicate")?;
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);

    let config = crate::config::DatalogConfig::default();
    let facts = crate::datalog::query_predicate(storage, ctx, session_id, predicate, &config)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let results: Vec<Value> = facts
        .iter()
        .map(|f| {
            serde_json::json!({
                "src_id": f.src_id,
                "predicate": f.pred,
                "dst_id": f.dst_id,
                "confidence": f.confidence,
                "rule_id": f.rule_id,
                "support_count": f.support_count,
                "provenance": f.provenance.iter().map(|p| {
                    serde_json::json!({
                        "parent_src": p.parent_src,
                        "parent_pred": p.parent_pred,
                        "parent_dst": p.parent_dst,
                        "parent_kind": p.parent_kind,
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "predicate": predicate,
        "derived_facts": results,
        "count": results.len(),
    }))
}

async fn handle_manage_rules<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let action = require_str(&args, "action")?;

    match action {
        "list" => {
            let family = args.get("family").and_then(|v| v.as_str()).unwrap_or("*");
            if family == "*" {
                // List all active rules -- return built-in rules
                let builtins = crate::datalog::builtin_rules();
                return Ok(serde_json::json!({
                    "action": "list",
                    "rules": builtins.iter().map(|r| {
                        serde_json::json!({ "head": r.head.predicate, "body_count": r.body.len() })
                    }).collect::<Vec<_>>(),
                    "count": builtins.len(),
                    "hint": "Showing built-in rules. Use family parameter to filter stored rules."
                }));
            }
            let rules = storage
                .rule_list_family(ctx, family, crate::types::RuleState::Active)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            let results: Vec<Value> = rules
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "rule_id": r.rule_id,
                        "version": r.version,
                        "name": r.name,
                        "family": r.family,
                        "state": r.state.to_string(),
                        "rule_body": r.rule_body,
                        "rule_weight": r.rule_weight,
                    })
                })
                .collect();

            Ok(serde_json::json!({ "action": "list", "rules": results, "count": results.len() }))
        }
        "get" => {
            let rule_id = require_str(&args, "rule_id")?;
            let rule = storage
                .rule_get(ctx, rule_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            match rule {
                Some(r) => Ok(serde_json::json!({
                    "action": "get",
                    "rule": {
                        "rule_id": r.rule_id, "version": r.version, "name": r.name,
                        "family": r.family, "rule_body": r.rule_body, "rule_weight": r.rule_weight
                    }
                })),
                None => Ok(serde_json::json!({ "action": "get", "rule": null })),
            }
        }
        "put" => {
            let rule_id = require_str(&args, "rule_id")?;
            let rule_body = require_str(&args, "rule_body")?;

            // Validate rule parses (STRIDE S7 -- reject malformed rules)
            crate::datalog::parse_rule(rule_body)
                .map_err(|e| (INVALID_PARAMS, format!("Invalid rule syntax: {e}")))?;

            let family = args
                .get("family")
                .and_then(|v| v.as_str())
                .unwrap_or("custom");
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or(rule_id);
            let weight = args
                .get("rule_weight")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0);

            // Get current version to auto-increment
            let version = match storage
                .rule_get(ctx, rule_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
            {
                Some(existing) => existing.version + 1,
                None => 1,
            };

            let entry = crate::types::RuleEntry {
                tenant_id: ctx.tenant_id,
                rule_id: rule_id.to_string(),
                version,
                name: name.to_string(),
                family: family.to_string(),
                state: crate::types::RuleState::Active,
                rule_body: rule_body.to_string(),
                rule_weight: weight,
                incremental: false,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };

            storage
                .rule_put(ctx, &entry)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            // Invalidate derived cache for affected predicates
            let parsed = crate::datalog::parse_rule(rule_body).unwrap();
            let _ = storage
                .derived_cache_clear(ctx, &parsed.head.predicate)
                .await;

            Ok(serde_json::json!({
                "action": "put",
                "rule_id": rule_id,
                "version": version,
                "hint": "Rule stored. Derived cache invalidated for affected predicate."
            }))
        }
        "deprecate" => {
            let rule_id = require_str(&args, "rule_id")?;
            match storage
                .rule_get(ctx, rule_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
            {
                Some(mut rule) => {
                    rule.state = crate::types::RuleState::Deprecated;
                    rule.updated_at = chrono::Utc::now();
                    storage
                        .rule_put(ctx, &rule)
                        .await
                        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
                    Ok(
                        serde_json::json!({ "action": "deprecate", "rule_id": rule_id, "deprecated": true }),
                    )
                }
                None => Ok(
                    serde_json::json!({ "action": "deprecate", "rule_id": rule_id, "deprecated": false, "error": "Rule not found" }),
                ),
            }
        }
        _ => Err((
            INVALID_PARAMS,
            format!("Unknown action: {action}. Use list/get/put/deprecate."),
        )),
    }
}

async fn handle_promote_predicate<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let predicate = require_str(&args, "predicate")?;
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);

    let config = crate::config::PromotionConfig::default();

    let count = crate::promotion::batch_materialize(storage, ctx, session_id, predicate, &config)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "predicate": predicate,
        "materialized_count": count,
        "status": if count > 0 { "promoted" } else { "no_facts_to_materialize" },
        "hint": if count > 0 {
            "Predicate promoted to durable storage. Future queries will use materialized facts."
        } else {
            "No derived facts found for this predicate. Run recursive_explore first to populate the graph."
        }
    }))
}

// --- Typed edge handlers ---

async fn handle_create_edge<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let src_id = require_uuid(&args, "src_entity_id")?;
    let dst_id = require_uuid(&args, "dst_entity_id")?;
    let edge_type = require_str(&args, "edge_type")?;
    let weight = args.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0);
    let metadata = args
        .get("metadata")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let edge = crate::types::TypedEdge {
        tenant_id: ctx.tenant_id,
        session_id,
        src_id,
        edge_type: edge_type.to_string(),
        dst_id,
        weight,
        metadata,
        created_at: chrono::Utc::now(),
    };

    storage
        .typed_edge_put(ctx, &edge)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    session.dirty.store(true, Ordering::Relaxed);
    session.last_activity.notify_waiters();

    Ok(serde_json::json!({
        "created": true,
        "src_id": src_id.to_string(),
        "edge_type": edge_type,
        "dst_id": dst_id.to_string(),
        "weight": weight,
    }))
}

async fn handle_batch_create_edges<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let edges = args
        .get("edges")
        .and_then(|v| v.as_array())
        .ok_or((INVALID_PARAMS, "edges must be an array".to_string()))?;

    if edges.len() > 200 {
        return Err((
            INVALID_PARAMS,
            format!("edges array length {} exceeds maximum of 200", edges.len()),
        ));
    }

    let mut created: usize = 0;
    let mut errors: usize = 0;

    for edge_json in edges {
        let src_id = match edge_json
            .get("src_entity_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        {
            Some(id) => id,
            None => {
                errors += 1;
                continue;
            }
        };
        let dst_id = match edge_json
            .get("dst_entity_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        {
            Some(id) => id,
            None => {
                errors += 1;
                continue;
            }
        };
        let edge_type = match edge_json.get("edge_type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                errors += 1;
                continue;
            }
        };
        let weight = edge_json
            .get("weight")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0);

        let edge = crate::types::TypedEdge {
            tenant_id: ctx.tenant_id,
            session_id,
            src_id,
            edge_type: edge_type.to_string(),
            dst_id,
            weight,
            metadata: None,
            created_at: chrono::Utc::now(),
        };

        match storage.typed_edge_put(ctx, &edge).await {
            Ok(()) => created += 1,
            Err(_) => errors += 1,
        }
    }

    session.dirty.store(true, Ordering::Relaxed);
    session.last_activity.notify_waiters();

    Ok(serde_json::json!({
        "created": created,
        "errors": errors,
        "total": edges.len(),
    }))
}

// --- Error mapping helpers ---

/// Map anyhow errors to JSON-RPC error codes, using INVALID_PARAMS for
/// quota violations (FMEA D1) and INTERNAL_ERROR for everything else.
fn map_quota_error(e: anyhow::Error) -> (i32, String) {
    if e.downcast_ref::<crate::quota::QuotaExceeded>().is_some() {
        (INVALID_PARAMS, e.to_string())
    } else {
        (INTERNAL_ERROR, e.to_string())
    }
}

async fn handle_find_duplicates<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or_else(uuid::Uuid::new_v4);
    let threshold = args
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.7);

    let pairs = crate::dedup::find_duplicates(storage, ctx, session_id, threshold)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(&pairs).map_err(|e| (INTERNAL_ERROR, e.to_string()))
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

    /// Extract the inner tool result from MCP CallToolResult wrapper.
    /// Dispatch wraps results as {"content": [{"type": "text", "text": "..."}]}.
    fn unwrap_tool_result(result: Value) -> Value {
        let text = result["content"][0]["text"]
            .as_str()
            .expect("CallToolResult missing content[0].text");
        serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
    }

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
        assert_eq!(tools.len(), 39);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"check_memo_cache"));
        assert!(names.contains(&"batch_ingest"));
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
        assert!(names.contains(&"importance_score"));
        assert!(names.contains(&"find_memory_chain"));
        assert!(names.contains(&"predict_needed"));
        assert!(names.contains(&"spread_activation"));
        assert!(names.contains(&"find_duplicates"));
        assert!(names.contains(&"recursive_explore"));
        assert!(names.contains(&"query_derived"));
        assert!(names.contains(&"manage_rules"));
        assert!(names.contains(&"promote_predicate"));
        assert!(names.contains(&"create_edge"));
        assert!(names.contains(&"batch_create_edges"));
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
        assert!(result["intention_id"].is_string());

        // Check — no match
        let params = serde_json::json!({
            "name": "check_intentions",
            "arguments": { "context": "working on database" }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["triggered"].as_array().unwrap().len(), 0);

        // Check — match
        let params = serde_json::json!({
            "name": "check_intentions",
            "arguments": { "context": "now looking at auth middleware" }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
        assert_eq!(result["entity_count"], 0);
        assert_eq!(result["fold_count"], 0);
        assert_eq!(result["memo_count"], 0);
        assert_eq!(result["memo_total_hits"], 0);
        assert_eq!(result["memo_hit_rate"], 0.0);
        assert_eq!(result["active_fold_count"], 0);
        assert_eq!(result["folded_count"], 0);
        assert_eq!(result["archived_fold_count"], 0);
        assert_eq!(result["temporal_fact_count"], 0);
        assert_eq!(result["edge_count"], 0);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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
        let result = unwrap_tool_result(result);
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

    #[test]
    fn retrieval_tracker_stats() {
        let mut tracker = RetrievalTracker::new();
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        // Empty tracker
        assert_eq!(tracker.mean(), 0.0);
        assert_eq!(tracker.stddev(), 0.0);

        // Single entity — stddev stays 0 (need 2+ for variance)
        tracker.record(id_a);
        assert_eq!(tracker.count(&id_a), 1);
        assert_eq!(tracker.mean(), 1.0);
        assert_eq!(tracker.stddev(), 0.0);

        // Two entities with divergent counts
        tracker.record(id_b);
        for _ in 0..9 {
            tracker.record(id_a);
        }
        assert_eq!(tracker.count(&id_a), 10);
        assert_eq!(tracker.count(&id_b), 1);
        assert_eq!(tracker.mean(), 5.5);
        assert!(tracker.stddev() > 0.0);
    }

    #[test]
    fn retrieval_tracker_anomaly_detection() {
        // Simulate a realistic scenario: many baseline entities at low counts,
        // one entity with anomalously high retrieval count.
        let mut tracker = RetrievalTracker::new();
        let config = crate::config::SecurityConfig::default();

        // 20 baseline entities each retrieved once
        let baseline_ids: Vec<Uuid> = (0..20).map(|_| Uuid::new_v4()).collect();
        for &id in &baseline_ids {
            tracker.record(id);
        }

        // One outlier entity retrieved 100 times
        let outlier_id = Uuid::new_v4();
        for _ in 0..100 {
            tracker.record(outlier_id);
        }

        let count = tracker.count(&outlier_id);
        let mean = tracker.mean();
        let stddev = tracker.stddev();

        // Baseline entities should NOT trigger anomaly
        let baseline_count = tracker.count(&baseline_ids[0]);
        assert!(
            !crate::audit::check_anomaly(baseline_count, mean, stddev, &config, None),
            "baseline entity should not be anomalous"
        );

        // Outlier SHOULD trigger anomaly
        assert!(
            crate::audit::check_anomaly(count, mean, stddev, &config, None),
            "outlier count {count} with mean={mean:.1} stddev={stddev:.1} should be anomalous"
        );
    }

    #[tokio::test]
    async fn retrieve_entities_records_to_tracker() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Upsert an entity
        let params = serde_json::json!({
            "name": "upsert_entity",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_name": "TrackedEntity",
                "entity_type": "concept",
                "context_snippet": "testing retrieval tracking",
                "confidence": 0.9
            }
        });
        dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();

        // Retrieve it several times
        let retrieve_params = serde_json::json!({
            "name": "retrieve_entities",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "TrackedEntity",
                "strategy": "phonetic"
            }
        });
        for _ in 0..5 {
            dispatch(
                "tools/call",
                retrieve_params.clone(),
                &store,
                &ctx,
                &session,
            )
            .await
            .unwrap();
        }

        // Verify the tracker recorded all retrievals
        let tracker = session.retrieval_tracker.lock().await;
        let entities = store.entities.lock().await;
        let entity_id = entities
            .iter()
            .find(|e| e.entity_name == "TrackedEntity")
            .expect("entity should exist")
            .entity_id;

        assert_eq!(
            tracker.count(&entity_id),
            5,
            "tracker should record each retrieval"
        );
    }
}

#[cfg(test)]
mod speculative_tests {
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

    fn unwrap_tool_result(result: Value) -> Value {
        let text = result["content"][0]["text"]
            .as_str()
            .expect("CallToolResult missing content[0].text");
        serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
    }

    #[tokio::test]
    async fn predict_needed_returns_empty_with_no_history() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "predict_needed",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 0);
        assert_eq!(result["recent_entity_count"], 0);
        assert!(result["predictions"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn predict_needed_with_targeted_recent() {
        let session = SessionState::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        // Build co-access history: a-b are strongly co-accessed
        {
            let mut co = session.co_access.lock().await;
            for _ in 0..5 {
                co.record(a);
                co.record(b);
            }
            // a-c weakly co-accessed
            co.record(a);
            co.record(c);
        }

        // Only mark 'a' as recently retrieved
        {
            let mut tracker = session.retrieval_tracker.lock().await;
            tracker.record(a);
        }

        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "predict_needed",
            "arguments": {
                "session_id": sid.to_string(),
                "threshold": 0.0,
                "limit": 10
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let predictions = result["predictions"].as_array().unwrap();
        assert!(!predictions.is_empty(), "should predict b and/or c");
        // b should be predicted with highest confidence
        let first = &predictions[0];
        assert_eq!(first["entity_id"].as_str().unwrap(), b.to_string());
        assert!((first["confidence"].as_f64().unwrap() - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn predict_needed_validates_params() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        // session_id is now optional — calling without it should succeed (empty predictions)
        let params = serde_json::json!({
            "name": "predict_needed",
            "arguments": {}
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["predictions"].is_array());
    }

    #[tokio::test]
    async fn anomaly_emits_event_on_bus() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Subscribe to event bus before triggering anomaly
        let mut rx = session.event_bus.subscribe();

        // Create 20 distinct entities to establish a wide baseline
        for i in 0..20 {
            let params = serde_json::json!({
                "name": "upsert_entity",
                "arguments": {
                    "session_id": sid.to_string(),
                    "entity_name": format!("Baseline{i:02}"),
                    "entity_type": "concept",
                    "context_snippet": "baseline entity",
                    "confidence": 0.9
                }
            });
            dispatch("tools/call", params, &store, &ctx, &session)
                .await
                .unwrap();
        }

        // Retrieve all baseline entities once each to seed tracker
        for i in 0..20 {
            let params = serde_json::json!({
                "name": "retrieve_entities",
                "arguments": {
                    "session_id": sid.to_string(),
                    "query": format!("Baseline{i:02}"),
                    "strategy": "phonetic"
                }
            });
            dispatch("tools/call", params, &store, &ctx, &session)
                .await
                .unwrap();
        }

        // Now retrieve one entity many more times to create a clear outlier.
        // With 20 entities at count 1 and outlier at count N, mean ~= N/21,
        // stddev driven mostly by the outlier. We need count > mean + 3*stddev.
        // At count=50 with 20 baselines at 1: mean=50+20/21=3.33, variance high
        // enough that the outlier itself will exceed 3-sigma once the distribution
        // has enough data points at the baseline.
        let outlier_params = serde_json::json!({
            "name": "retrieve_entities",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Baseline00",
                "strategy": "phonetic"
            }
        });
        for _ in 0..60 {
            dispatch("tools/call", outlier_params.clone(), &store, &ctx, &session)
                .await
                .unwrap();
        }

        // Drain events and check for at least one AnomalyDetected
        let mut found_anomaly = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, crate::viz::VizEvent::AnomalyDetected { .. }) {
                found_anomaly = true;
                let json = serde_json::to_string(&event).unwrap();
                assert!(json.contains("AnomalyDetected"));
                assert!(json.contains("Baseline00"));
                break;
            }
        }
        assert!(found_anomaly, "expected AnomalyDetected event on bus");
    }
}
