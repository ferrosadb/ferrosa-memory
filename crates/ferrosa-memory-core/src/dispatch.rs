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

use serde::Deserialize;
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
    /// Repository path for intention scoping (from CLAUDE_PROJECT_DIR, config, or MCP initialize roots).
    pub repo: std::sync::OnceLock<String>,
    /// Notified on every tool call; used by the idle consolidation timer.
    pub last_activity: Arc<tokio::sync::Notify>,
    /// Set to true when a write tool succeeds; cleared by idle consolidation.
    pub dirty: Arc<AtomicBool>,
    /// Base URL for the Ollama API (used for embeddings and NER extraction).
    pub ollama_base_url: String,
    /// Model name for NER entity extraction via Ollama.
    pub ner_model: String,
    /// Model name for text embedding via Ollama (default nomic-embed-text-v2-moe).
    pub embed_model: String,
    /// Expected embedding dimensions (default 768).
    pub embed_dimensions: u32,
    /// Dynamic entity types loaded from the type registry table.
    pub entity_types: Vec<String>,
    /// Dynamic edge types loaded from the type registry table.
    pub edge_types: Vec<String>,
    /// Base URL for the enrichment LLM (OpenAI-compatible API).
    pub enrich_llm_url: String,
    /// Model name for enrichment LLM.
    pub enrich_llm_model: String,
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
            repo: std::sync::OnceLock::new(),
            last_activity: Arc::new(tokio::sync::Notify::new()),
            dirty: Arc::new(AtomicBool::new(false)),
            ollama_base_url: "http://127.0.0.1:11434".to_string(),
            ner_model: "qwen3.5:27b".to_string(),
            embed_model: "nomic-embed-text-v2-moe".to_string(),
            embed_dimensions: 768,
            entity_types: vec![
                "person",
                "place",
                "event",
                "concept",
                "org",
                "bug",
                "decision",
                "pattern",
                "preference",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            edge_types: Vec::new(),
            enrich_llm_url: "http://localhost:1234".to_string(),
            enrich_llm_model: "google/gemma-4-31b".to_string(),
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
                    "session_id": { "type": "string" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "subtask_id": { "type": "string", "maxLength": 256 },
                    "parent_subtask": { "type": "string", "maxLength": 256 },
                    "goal_text": { "type": "string", "maxLength": 4096 }
                },
                "required": ["depth", "subtask_id", "goal_text"]
            }),
        },
        ToolDef {
            name: "get_plan_context".into(),
            description: "Returns the full plan tree for the current session as compact JSON. Use to re-inject parent context when returning from recursive sub-tasks.\n\nCALL WHEN: At the start of each sub-task execution and on return from a sub-task call.\nInclude the returned plan tree in your prompt preamble with 'Current task hierarchy:' to prevent goal drift.\nCost: ~2ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "max_depth": { "type": "integer", "minimum": 0, "maximum": 100 }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "update_plan_node".into(),
            description: "Marks a plan node complete or failed and records an outcome summary.\n\nCALL WHEN: When a sub-task finishes (success or failure). Always provide outcome_summary — this is what parent nodes will see.\nWrite outcome_summary describing what was found, not the process used.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "subtask_id": { "type": "string", "maxLength": 256 },
                    "status": { "type": "string", "enum": ["pending", "active", "complete", "failed"] },
                    "outcome_summary": { "type": "string", "maxLength": 4096 }
                },
                "required": ["depth", "subtask_id", "status"]
            }),
        },
        // --- Fold tools (Sprint 2) ---
        ToolDef {
            name: "start_fold".into(),
            description: "Opens a new trajectory fold for a sub-task. Returns fold_id to append REPL turns as the sub-task executes.\n\nCALL WHEN: Starting any sub-task that involves multiple steps and whose results you want retrievable later. Always call write_plan_node first.\nA fold is the durable equivalent of a REPL scope.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "parent_fold_id": { "type": "string", "format": "uuid" },
                    "initial_context": { "type": "string", "maxLength": 131072 }
                },
                "required": ["depth", "initial_context"]
            }),
        },
        ToolDef {
            name: "append_to_fold".into(),
            description: "Appends a REPL turn to an active fold. Returns current token_count.\n\nCALL WHEN: After each step within an active fold.\nMONITOR token_count: If it exceeds ~80000, open a nested fold for the next phase.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fold_id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string" },
                    "repl_turn": { "type": "string", "maxLength": 131072 }
                },
                "required": ["fold_id", "repl_turn"]
            }),
        },
        ToolDef {
            name: "complete_fold".into(),
            description: "Seals a fold with summary and embedding. Creates FOLDED_INTO graph edge to parent. Queues trajectory for compression.\n\nCALL WHEN: When a sub-task is fully complete. Always call before returning from a recursive level.\nWrite summary as dense NL capsule: key findings, state changes, answers. Summarize outcomes, not process.\nCost: ~10ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fold_id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string" },
                    "summary": { "type": "string", "maxLength": 131072 },
                    "embedding": { "type": "array", "items": { "type": "number" } }
                },
                "required": ["fold_id", "summary", "embedding"]
            }),
        },
        ToolDef {
            name: "retrieve_fold_context".into(),
            description: "ANN vector search over prior fold summaries. Returns k most semantically similar fold summaries.\n\nCALL WHEN: Starting a new task where prior work might be relevant. Also call when stuck — prior folds often contain relevant evidence.\nRETRIEVAL LOOP: If results partially answer but leave gaps, call again with a more specific query targeting the gap. 2-3 rounds is normal.\nCost: ~10ms (HNSW). include_raw adds ~200-2000ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "query_embedding": { "type": "array", "items": { "type": "number" } },
                    "query": { "type": "string", "maxLength": 4096, "description": "Optional text query for routing optimization. If provided, the router selects optimal k and include_raw." },
                    "k": { "type": "integer", "minimum": 1, "maximum": 100 },
                    "include_raw": { "type": "boolean" }
                },
                "required": ["query_embedding"]
            }),
        },
        // --- Entity tools (Sprint 3) ---
        ToolDef {
            name: "upsert_entity".into(),
            description: "Writes a discovered named entity to the entity store. Deduplicates via phonetic matching.\n\nCALL WHEN: Any time you identify a named entity (person, place, org, event, concept) from content.\nCheck is_new in response: if false, entity already exists — use the returned entity_id to attach new facts.\n\nNote: source_fold_id is optional — omit if not in a fold context.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entity_name": { "type": "string", "maxLength": 512 },
                    "entity_type": { "type": "string", "enum": entity_type_enum },
                    "context_snippet": { "type": "string", "maxLength": 4096 },
                    "embedding": { "type": "array", "items": { "type": "number" } },
                    "source_fold_id": { "type": "string", "format": "uuid", "description": "Optional: fold UUID from start_fold. Omit if not in a fold context." },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                },
                "required": ["entity_name", "entity_type", "context_snippet"]
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
                    "session_id": { "type": "string", "description": "Session UUID" },
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
                "required": ["entities"]
            }),
        },
        ToolDef {
            name: "batch_update_entities".into(),
            description: "Batch update entities by entity_id with explicit patch fields.\n\nReturns per-row success/failure and supports partial update.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entities": {
                        "type": "array",
                        "description": "Array of entity patches keyed by entity_id",
                        "items": {
                            "type": "object",
                            "properties": {
                                "entity_id": { "type": "string" },
                                "entity_name": { "type": "string", "maxLength": 512 },
                                "entity_type": { "type": "string" },
                                "context_snippet": { "type": "string", "maxLength": 4096 },
                                "source_fold_id": { "type": "string" },
                                "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                                "state": { "type": "string", "enum": ["active", "dormant", "silent", "unavailable"] },
                                "description": { "type": "string", "maxLength": 4096 },
                                "tags": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "properties": { "type": "object" },
                                "embedding": {
                                    "type": "array",
                                    "items": { "type": "number" },
                                    "description": "Replacement embedding vector"
                                }
                            },
                            "required": ["entity_id"]
                        },
                        "maxItems": 100
                    }
                },
                "required": ["entities"]
            }),
        },
        ToolDef {
            name: "batch_delete_entities".into(),
            description: "Batch delete entities by id with per-row success/failure reporting. Existing rows are hard-deleted from ferrosa-memory owned storage.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "entities": {
                        "type": "array",
                        "description": "Entity IDs to delete",
                        "items": {
                            "type": "object",
                            "properties": {
                                "entity_id": { "type": "string", "description": "Target entity UUID" }
                            },
                            "required": ["entity_id"]
                        },
                        "maxItems": 100
                    }
                },
                "required": ["entities"]
            }),
        },
        ToolDef {
            name: "ingest_entities".into(),
            description: "Bulk-ingest entities and typed edges in a single call. The server owns schema mapping, conflict semantics, optional embedding generation, and structured per-row failures.\n\nCALL WHEN: You already have a batch of stable entity IDs and typed edges and want one fail-loud ingest call instead of direct CQL writes or multiple tool calls.\nRETURNS: counts plus structured failed[] arrays for entities, edges, and embeddings. dry_run validates without writing.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string" },
                    "entities": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "format": "uuid" },
                                "name": { "type": "string", "maxLength": 512 },
                                "entity_type": { "type": "string", "enum": entity_type_enum },
                                "context": { "type": "string", "maxLength": 16384 },
                                "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                                "state": { "type": "string" },
                                "embedding": { "type": "array", "items": { "type": "number" } },
                                "attrs": { "type": "object" }
                            },
                            "required": ["id", "name", "entity_type", "context"]
                        }
                    },
                    "edges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "src_id": { "type": "string", "format": "uuid" },
                                "dst_id": { "type": "string", "format": "uuid" },
                                "edge_type": { "type": "string" },
                                "weight": { "type": "number" },
                                "metadata": { "type": "object" }
                            },
                            "required": ["src_id", "dst_id", "edge_type"]
                        }
                    },
                    "options": {
                        "type": "object",
                        "properties": {
                            "embed_missing": { "type": "boolean" },
                            "embedding_model": { "type": "string" },
                            "on_conflict": { "type": "string", "enum": ["update", "skip", "error"] },
                            "strict_edges": { "type": "boolean" },
                            "dry_run": { "type": "boolean" }
                        }
                    }
                },
                "required": ["tenant_id", "entities"]
            }),
        },
        ToolDef {
            name: "retrieve_entities".into(),
            description: "Retrieves named entities by name (phonetic fuzzy match), semantic similarity (ANN), or both.\n\nCALL WHEN: Need to find entities related to current query. Use strategy='phonetic' for known names with possible variants. Use strategy='ann' for semantic search. Use strategy='both' for maximum recall.\nCost: phonetic ~5ms, ann ~10ms, both ~15ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
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
                    "session_id": { "type": "string" },
                    "query_id": { "type": "string", "format": "uuid" },
                    "program_type": { "type": "string", "enum": ["hnsw_ann", "phonetic", "cypher_hop", "btree_range", "memo_hit"] },
                    "task_complexity": { "type": "string", "enum": ["simple", "linear", "quadratic"] },
                    "succeeded": { "type": "boolean" },
                    "latency_ms": { "type": "integer", "minimum": 0 },
                    "token_cost": { "type": "integer", "minimum": 0 }
                },
                "required": ["query_id", "program_type", "task_complexity", "succeeded", "latency_ms", "token_cost"]
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
                    "session_id": { "type": "string" },
                    "content": { "type": "string", "maxLength": 8192, "description": "The content to ingest" },
                    "entity_type": { "type": "string", "enum": entity_type_enum },
                    "entity_name": { "type": "string", "maxLength": 256, "description": "Clean entity name (e.g. 'Ben Kearns', 'Ferrosa'). If omitted, extracted automatically from content via LLM or heuristic." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding vector" },
                    "source_fold_id": { "type": "string", "format": "uuid", "description": "Optional: UUID of the fold (conversation thread) that produced this content. Omit or pass null if not in a fold context. DO NOT pass a file path — this field expects a fold UUID from start_fold, or null." }
                },
                "required": ["content", "entity_type"]
            }),
        },
        // --- Skills layer ---
        ToolDef {
            name: "ingest_skill".into(),
            description: "Ingest a methodology into the global skill catalog. Skills are shared across all sessions and tenants' queries.\n\nCALL WHEN: You encounter or refine a reusable methodology — TDD, STRIDE threat modeling, debugging process, refactoring pattern, etc.\n\nThe server generates the version (YYYYMMDDNN) — do not pass it. Pass content_hash for idempotent re-ingest; re-running with an unchanged hash is a no-op.\n\nSkills are stored with entity_type='skill', scope='global'. Category and additional tags become tag entities + TAGGED_AS edges. Prerequisites become REQUIRES edges. If a prerequisite skill doesn't exist yet, its name is recorded in `missing_prerequisites` on the response — the skill itself still lands. Either ingest the missing prereqs and re-run this skill, or accept the partial graph.\n\nTAG NORMALIZATION: category and tags are normalized to lowercase, alphanumeric + dash only. Any other character (including underscore, space, slash) becomes `-`; consecutive dashes collapse and leading/trailing dashes are stripped. Example: 'Chaos Engineering' → 'chaos-engineering', 'unit_testing' → 'unit-testing', 'foo/bar/baz' → 'foo-bar-baz'. Use the same normalized form when calling retrieve_skills_for_context or ensure_parent_tag.\n\nLEARN AND REFINE: If you use a skill and discover a better step, a missing prerequisite, or a clearer description, call ingest_skill again to refine it. Your changes persist across all sessions.\nCost: ~20ms + one embed call for description.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller's session UUID (optional, recorded for audit). Omit or pass 'default' to use the configured default session." },
                    "name": { "type": "string", "maxLength": 256, "description": "Unique skill identifier (e.g., 'tdd', 'threat-model')" },
                    "category": { "type": "string", "maxLength": 128, "description": "Primary tag (e.g., 'testing', 'security'). Becomes a tag entity + TAGGED_AS edge." },
                    "description": { "type": "string", "maxLength": 4096, "description": "2-4 sentence description of what the skill does and when to use it. Embedded for retrieval." },
                    "trigger_keywords": { "type": "array", "items": { "type": "string" }, "description": "Keywords that indicate this skill is relevant." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Additional tags beyond category." },
                    "prerequisites": { "type": "array", "items": { "type": "string" }, "description": "Names of other skills this requires." },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "phase": { "type": "string" },
                                "instruction": { "type": "string" }
                            },
                            "required": ["instruction"]
                        },
                        "description": "Ordered steps to follow when invoking the skill."
                    },
                    "output_artifacts": { "type": "array", "items": { "type": "string" }, "description": "Artifacts the skill produces (e.g., 'checklist', 'diagram')." },
                    "completion_criteria": { "type": "string", "maxLength": 1024, "description": "How to tell when the skill's work is done." },
                    "content_hash": { "type": "string", "maxLength": 128, "description": "Caller-computed content hash for idempotent re-ingest. Passing the same hash as the stored skill is a no-op." }
                },
                "required": ["name", "category", "description"]
            }),
        },
        ToolDef {
            name: "retrieve_skills_for_context".into(),
            description: "Find methodologies relevant to your current task from the global skill catalog.\n\nCALL AT TASK START or whenever you encounter a problem you've solved before — 'how do I test this?', 'how should I refactor this?', 'what's the threat model here?'\n\nReturns ranked skills with description, category, version, and a used_in_session flag. Match scoring combines description-embedding similarity, trigger_keyword overlap, tag overlap, and name hits.\n\nThese skills are GLOBAL — shared across every session. If a result is marked used_in_session=true, you've already touched it this session, which is a strong relevance signal.\nCost: O(catalog size) — typically <20ms for 100s of skills.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller's session UUID (optional, used for the used_in_session flag)." },
                    "context": { "type": "string", "maxLength": 8192, "description": "Current task context — what you're working on, the problem statement, or a natural-language question." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional context embedding. When present, enables semantic matching against skill description_embeddings." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Max results (default 5)." },
                    "min_score": { "type": "number", "minimum": 0.0, "maximum": 2.0, "description": "Minimum score threshold (default 0.0 returns all)." }
                },
                "required": ["context"]
            }),
        },
        ToolDef {
            name: "invoke_skill".into(),
            description: "Fetch the structured steps for a named skill. Returns {description, steps, first_step_prompt, completion_criteria, output_artifacts}.\n\nCALL WHEN: You've decided to apply a skill by name (e.g., after retrieve_skills_for_context returned it, or the user explicitly asked 'use TDD').\n\nThe response is pure data. Execute the steps yourself — invoke_skill does not orchestrate tool calls. Start with first_step_prompt. Check completion_criteria when you finish.\n\nMissed skill returns INVALID_PARAMS with a did_you_mean list of similar skill names (phonetic match). Ingest the skill with ingest_skill if it genuinely doesn't exist yet.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller's session UUID (optional; used for prerequisite-satisfaction tracking)." },
                    "skill_name": { "type": "string", "maxLength": 256, "description": "Exact name of the skill to invoke (case-sensitive)." },
                    "current_context": { "type": "string", "maxLength": 4096, "description": "Optional context hint — what you're working on right now." }
                },
                "required": ["skill_name"]
            }),
        },
        ToolDef {
            name: "ensure_parent_tag".into(),
            description: "Idempotently create a PARENT_TAG edge between two tags in the global taxonomy, resolving tags by name (creating them if missing).\n\nCALL WHEN: Building or extending the tag hierarchy — e.g. declaring that 'tdd' is a sub-category of 'testing', or that 'testing' is a sub-category of 'quality'. forge's fmem-skill-ingest uses this when ingesting `tag-hierarchy.yaml`.\n\nTAG NORMALIZATION: names are normalized to lowercase, alphanumeric + dash only. Any other character (underscore, space, slash, etc.) becomes `-`; consecutive dashes collapse, leading/trailing dashes strip. 'Chaos Engineering' → 'chaos-engineering', 'unit_testing' → 'unit-testing'. Pre-normalize on the caller side if you want full control; otherwise the server's normalization is deterministic.\n\nReturns action=Created on first call, action=Skipped on subsequent identical calls. Cycles are rejected via the graph client's DAG check.\nCost: ~5ms for idempotent re-runs, ~20ms when creating both tags + edge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller session UUID (optional; used for ingested_by_session audit)." },
                    "child_tag": { "type": "string", "maxLength": 256, "description": "Narrower tag name (e.g. 'tdd')." },
                    "parent_tag": { "type": "string", "maxLength": 256, "description": "Broader tag name (e.g. 'testing')." }
                },
                "required": ["child_tag", "parent_tag"]
            }),
        },
        ToolDef {
            name: "verify_skill".into(),
            description: "Verify a skill's graph neighborhood for ingest pipelines and audits. Returns resolved tags, prerequisites (outgoing REQUIRES), required_by (incoming REQUIRES), and missing_prerequisites (raw names declared at ingest that never landed as edges).\n\nCALL WHEN: A bulk ingest finishes and the caller wants to confirm every skill's edges are intact. Safe to call for unknown skill names — returns {exists: false} cleanly, not an error.\n\nThis is an administrative read. For executing a skill, use invoke_skill.\nCost: ~10ms (one entity lookup + two edge scans).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller session UUID (optional)." },
                    "skill_name": { "type": "string", "maxLength": 256, "description": "Exact skill name (case-sensitive)." }
                },
                "required": ["skill_name"]
            }),
        },
        // --- Intention tools (prospective memory, repo-scoped) ---
        ToolDef {
            name: "set_intention".into(),
            description: "Prospective memory — 'remember to do X when Y happens.' Sets a deferred action that auto-triggers on context match.\n\nCALL WHEN you notice something to do later:\n- 'When we touch auth, check the error handling'\n- 'Next time we open database.rs, add that index'\n- 'When user mentions deployment, remind about the TLS cert'\n- 'In 30 minutes, check if the build finished'\n\nTrigger types: Topic (keyword match), FilePattern (file glob), Duration (minutes), Context (flexible condition).\n\nIntentions are repo-scoped and persist across sessions. They trigger automatically when check_intentions runs. Set liberally — they cost nothing until triggered.\nCost: ~1ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "maxLength": 4096, "description": "What to do when triggered" },
                    "repo": { "type": "string", "maxLength": 512, "description": "Repository path for scoping (defaults to server's configured repo)" },
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
            description: "Checks pending intentions against current context. Call FREQUENTLY — at every topic change, file open, or new task start. Pass a brief description of what you're doing now as context. Returns triggered intentions you should act on.\n\nIntentions are repo-scoped — only intentions for the current repo are checked.\nCost: ~1ms. Call often — it's free.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "context": { "type": "string", "maxLength": 8192, "description": "Current context to check against" },
                    "repo": { "type": "string", "maxLength": 512, "description": "Repository path (defaults to server's configured repo)" }
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
            description: "Lists intentions. By default lists current repo's intentions from the in-memory store.\n\nPass all_repos: true to list intentions across ALL repos from durable storage — useful for seeing all threads you're coordinating across projects.\n\nCALL WHEN: User asks about pending intentions, wants a cross-project overview, or for debugging intention state.\nCost: ~1ms (in-memory), ~15ms (all_repos).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "all_repos": { "type": "boolean", "description": "If true, list intentions across ALL repos from storage (not just current session)" }
                }
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
                    "session_id": { "type": "string" },
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
                    "session_id": { "type": "string" },
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
                    "session_id": { "type": "string", "description": "Session ID (required for fold_ancestors, related_entities, entities_in_fold)" },
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
                    "session_id": { "type": "string" },
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
                    "session_id": { "type": "string" }
                },
                "required": []
            }),
        },
        // --- Enrichment pipeline ---
        ToolDef {
            name: "enrich_entities".into(),
            description: "Post-ingest enrichment: generates LLM descriptions for code entities, \
                annotates edge relationships, and lints the knowledge graph.\n\n\
                CALL WHEN: After frg ingest populates the graph with structural entities. \
                Transforms shallow structural facts into searchable semantic knowledge.\n\n\
                Operations: enrich (LLM descriptions), annotate (edge explanations), lint (graph analysis).\n\
                Idempotent — safe to run multiple times. Already-enriched entities are skipped.\n\
                Cost: ~2-5 min for 1000 entities (local LLM). Lint is instant.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "operations": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["enrich", "annotate", "lint"] },
                        "description": "Which operations to run. Default: all three."
                    },
                    "entity_types": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Filter: only enrich entities of these types"
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Re-enrich already-enriched entities. Default: false."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Lint only, don't write changes. Default: false."
                    }
                },
                "required": []
            }),
        },
        // --- Stats tool ---
        ToolDef {
            name: "get_stats".into(),
            description: "Returns memory system statistics for the session: entity count, fold count, memo count, and intention count.\n\nCALL WHEN: For health monitoring, debugging, or when the user asks about memory usage.\nCost: ~5ms (runs 3 count queries).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "count_entities_by_type".into(),
            description: "Return a per-session entity histogram broken down by entity_type, by state, and by the joint (type,state) buckets.\n\nCALL WHEN: You need status/diagnostic counts like 'how many bugs are active in this session?' without coupling the client to entity_store columns.\nCost: ~5-10ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": []
            }),
        },
        // --- Memory state management ---
        ToolDef {
            name: "promote_memory".into(),
            description: "Promotes an entity's memory state one level: dormant->active, silent->dormant, unavailable->silent. Active stays active.\n\nCALL WHEN: A dormant or silent memory becomes relevant again — e.g., an entity is referenced in new context after a period of inactivity.\nRETURNS: The new memory state after promotion.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
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
                    "session_id": { "type": "string" },
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
                    "session_id": { "type": "string" },
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
                    "session_id": { "type": "string" },
                    "source": { "type": "string", "format": "uuid", "description": "Entity ID to start from" },
                    "destination": { "type": "string", "format": "uuid", "description": "Entity ID to find path to" },
                    "max_hops": { "type": "integer", "minimum": 1, "maximum": 10, "description": "Maximum path length (default: 5)" }
                },
                "required": ["source", "destination"]
            }),
        },
        // --- Speculative retrieval ---
        ToolDef {
            name: "predict_needed".into(),
            description: "Predicts which entities will be needed based on co-access patterns. Analyzes which entities are frequently retrieved together and suggests entities likely to be needed given recent access history.\n\nCALL WHEN: After retrieving entities, to prefetch or surface related memories before they are explicitly requested.\nCost: ~1ms (in-memory co-access analysis).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
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
                "required": []
            }),
        },
        // --- Spreading activation ---
        ToolDef {
            name: "spread_activation".into(),
            description: "Spreading activation search (Collins & Loftus). Propagates activation energy from seed entities through the knowledge graph, decaying at each hop. Returns the most activated non-seed entities.\n\nCALL WHEN: You have one or more known entities and want to discover related entities through graph structure — especially when semantic search alone misses structural relationships.\nPair with retrieve_entities for seeds, then spread to find indirect connections.\nCost: ~10-50ms depending on graph density and max_hops.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
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
                "required": ["seeds"]
            }),
        },
        // --- Duplicate detection ---
        ToolDef {
            name: "find_duplicates".into(),
            description: "Scans a session\'s entities for potential duplicates using text similarity (Jaccard coefficient) on context snippets. Returns pairs above the threshold, sorted by similarity descending.\n\nCALL WHEN: After bulk entity ingestion, or when you suspect duplicate entities exist in a session. Useful before consolidation to identify merge candidates.\nDO NOT CALL: On sessions with very few entities (< 3). Use retrieve_entities with phonetic matching for single-entity dedup.\nCost: O(n^2) comparisons -- fast for <1000 entities per session.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "threshold": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1,
                        "description": "Similarity threshold (0-1). Default: 0.7. Higher = fewer, more confident matches."
                    }
                },
                "required": []
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
        ToolDef {
            name: "manage_claims".into(),
            description: "Manage expert-system claims stored as entity-backed review artifacts.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "put"] },
                    "claim_id": { "type": "string" },
                    "claim_text": { "type": "string" },
                    "domain": { "type": "string" },
                    "status": { "type": "string", "enum": ["proposed", "approved", "rejected"] },
                    "confidence": { "type": "number" },
                    "source_ref": { "type": "string" },
                    "support_count": { "type": "integer" },
                    "workspace_scope": { "type": "string" },
                    "session_id": { "type": "string" },
                    "include_unapproved": { "type": "boolean" }
                },
                "required": ["action"]
            }),
        },
        ToolDef {
            name: "manage_approvals".into(),
            description: "Append and inspect approval decisions for rules, claims, aliases, and other governed artifacts.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "record", "latest"] },
                    "artifact_kind": { "type": "string", "enum": ["rule", "claim", "alias", "skill"] },
                    "artifact_ref": { "type": "string" },
                    "decision": { "type": "string", "enum": ["proposed", "approved", "rejected"] },
                    "review_note": { "type": "string" },
                    "scope": { "type": "string" },
                    "workspace_scope": { "type": "string" },
                    "session_scope": { "type": "string" },
                    "reviewer": { "type": "string", "description": "Ignored; reviewer is always auth-derived." }
                },
                "required": ["action", "artifact_kind", "artifact_ref"]
            }),
        },
        ToolDef {
            name: "manage_aliases".into(),
            description: "Manage exact-scope tool aliases for deterministic execution-time rewrites.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "put", "resolve"] },
                    "alias_name": { "type": "string" },
                    "scope_kind": { "type": "string", "enum": ["global", "workspace", "session"] },
                    "scope_ref": { "type": "string" },
                    "canonical_tool": { "type": "string" },
                    "parameter_map": { "type": "object" },
                    "fixed_arguments": { "type": "object" },
                    "args_templates": { "type": "object" },
                    "status": { "type": "string", "enum": ["proposed", "approved", "rejected"] },
                    "workspace_scope": { "type": "string" },
                    "session_scope": { "type": "string" }
                },
                "required": ["action", "alias_name"]
            }),
        },
        ToolDef {
            name: "explain_derived".into(),
            description: "Return a bounded explanation for derived facts, including support chain and approval metadata.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "predicate": { "type": "string" },
                    "session_id": { "type": "string" },
                    "src_id": { "type": "string" },
                    "dst_id": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 64 }
                },
                "required": ["predicate"]
            }),
        },
        ToolDef {
            name: "get_effective_rule_set".into(),
            description: "Inspect the merged runtime-effective rule set, including synthetic built-ins and approved registry rules.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "family": { "type": "string" }
                },
                "required": []
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
                    "session_id": { "type": "string" },
                    "src_entity_id": { "type": "string", "format": "uuid", "description": "Source entity UUID" },
                    "dst_entity_id": { "type": "string", "format": "uuid", "description": "Destination entity UUID" },
                    "edge_type": { "type": "string", "description": "Relationship type (depends_on, contains, part_of, subclass_of, calls, implements, uses)" },
                    "weight": { "type": "number", "minimum": 0, "maximum": 1, "description": "Edge strength (default 1.0)" },
                    "metadata": { "type": "string", "description": "Optional metadata about the relationship" }
                },
                "required": ["src_entity_id", "dst_entity_id", "edge_type"]
            }),
        },
        ToolDef {
            name: "batch_create_edges".into(),
            description: "Create multiple typed edges in a single call.\n\n\
                Cost: ~5ms + 2ms per edge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
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
                "required": ["edges"]
            }),
        },
        ToolDef {
            name: "batch_update_edges".into(),
            description: "Update typed edges in bulk by (src_entity_id, dst_entity_id, edge_type).\n\n\
                Current storage semantics write through `typed_edge_put`; this is treated as upsert/update-compatible where supported."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "edges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "src_entity_id": { "type": "string", "format": "uuid" },
                                "dst_entity_id": { "type": "string", "format": "uuid" },
                                "edge_type": { "type": "string" },
                                "weight": { "type": "number" },
                                "metadata": { "type": "string" }
                            },
                            "required": ["src_entity_id", "dst_entity_id", "edge_type"]
                        },
                        "maxItems": 200
                    }
                },
                "required": ["edges"]
            }),
        },
        ToolDef {
            name: "batch_delete_edges".into(),
            description: "Delete typed edges in bulk.\n\n\
                Uses the current graph-backed delete path and returns structured per-row success/failure results."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "edges": {
                        "type": "array",
                        "description": "Typed edges to delete",
                        "items": {
                            "type": "object",
                            "properties": {
                                "src_entity_id": { "type": "string", "format": "uuid" },
                                "dst_entity_id": { "type": "string", "format": "uuid" },
                                "edge_type": { "type": "string" }
                            },
                            "required": ["src_entity_id", "dst_entity_id", "edge_type"]
                        },
                        "maxItems": 200
                    }
                },
                "required": ["edges"]
            }),
        },
        // --- Derived cache listing ---
        ToolDef {
            name: "list_derived_cache".into(),
            description: "List all derived cache entries for inspection/debugging.\n\n\
                Returns up to `limit` rows sorted by computed_at DESC.\n\n\
                Use for: audit trail, debugging derivation results, reviewing cache state.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Tenant UUID" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Max rows to return (default 100)" }
                },
                "required": ["tenant_id"]
            }),
        },
    ]
}

/// Memory guide included in initialize instructions.
/// Teaches the LLM how to use the memory system to build knowledge.
const MEMORY_GUIDE: &str = r#"You have a semantic memory system. Use it BEFORE grep, find, or reading files. It should be your first source of context, not a fallback.

SESSION START: (1) check_intentions with current context, (2) hybrid_search for what you're working on, (3) tell user what you remember. Do this BEFORE reading files.

SEARCHING: hybrid_search first. If it returns what you need, you're done — no need to grep or read files. If results are insufficient, the response will suggest deeper tools (recursive_explore, spread_activation). Only fall back to grep/find/read if memory genuinely doesn't have what you need.

STORING: Use smart_ingest for new knowledge. It decides CREATE/UPDATE/SUPERSEDE/SKIP. Store insights, decisions, relationships, and facts — not raw file contents.

CONNECTING: After learning 2+ related facts, use create_edge to link them. Types: depends_on, contains, part_of, related_to, calls, implements, uses, references. Connected facts are knowledge; isolated facts are just data.

INTENTIONS: set_intention for deferred actions. check_intentions at session start. Triggers: Topic, FilePattern, Duration, Context.

CONSOLIDATION: After significant learning (10+ entities), run run_consolidation to discover CO_OCCURS patterns.

FEEDBACK: If you had to use grep, find, or read files to get context that SHOULD have been in memory, call record_outcome with program_type="retrieval_miss" and include what you were looking for. This trains the system to store that kind of information in the future. Every retrieval miss is a signal to improve."#;

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
        },
        "instructions": MEMORY_GUIDE
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
        "initialize" => {
            // Extract repo from client roots (MCP spec: roots[].uri).
            if session.repo.get().is_none()
                && let Some(uri) = params
                    .get("roots")
                    .and_then(|v| v.as_array())
                    .and_then(|roots| roots.first())
                    .and_then(|r| r.get("uri"))
                    .and_then(|u| u.as_str())
            {
                let path = uri.strip_prefix("file://").unwrap_or(uri).to_string();
                let _ = session.repo.set(path.clone());
                tracing::info!(repo = %path, "repo set from MCP initialize roots");
            }
            Ok(server_info())
        }
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => {
            let include_all = params
                .get("include_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let all_tools = tool_definitions(&session.entity_types);
            let tools: Vec<ToolDef> = if include_all {
                all_tools
            } else {
                all_tools
                    .into_iter()
                    .filter(|t| is_tier1(&t.name))
                    .collect()
            };
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

    resolve_session_id(&mut args, session.default_session_id)?;

    tracing::debug!(tool = name, "dispatching tool call");
    let input_bytes = serde_json::to_string(&args).map(|s| s.len()).unwrap_or(0) as i32;
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
        "ingest_entities" => handle_ingest_entities(args, storage, ctx, session).await,
        "retrieve_entities" => handle_retrieve_entities(args, storage, ctx, session).await,
        "record_outcome" => handle_record_outcome(args, storage, ctx).await,
        "delete_session" => handle_delete_session(args, storage, ctx).await,
        "smart_ingest" => handle_smart_ingest(args, storage, ctx, session).await,
        "ingest_skill" => handle_ingest_skill(args, storage, ctx, session).await,
        "retrieve_skills_for_context" => {
            handle_retrieve_skills_for_context(args, storage, ctx, session).await
        }
        "invoke_skill" => handle_invoke_skill(args, storage, ctx, session).await,
        "ensure_parent_tag" => handle_ensure_parent_tag(args, storage, ctx, session).await,
        "verify_skill" => handle_verify_skill(args, storage, ctx, session).await,
        "set_intention" => handle_set_intention(args, storage, ctx, session).await,
        "check_intentions" => handle_check_intentions(args, storage, ctx, session).await,
        "complete_intention" => handle_complete_intention(args, storage, ctx, session).await,
        "list_intentions" => handle_list_intentions(args, storage, ctx, session).await,
        "snooze_intention" => handle_snooze_intention(args, storage, ctx, session).await,
        "write_temporal_fact" => handle_write_temporal_fact(args, storage, ctx, session).await,
        "get_temporal_chain" => handle_get_temporal_chain(args, storage, ctx).await,
        "explore_connections" => handle_explore_connections(args, storage, ctx, session).await,
        "hybrid_search" => handle_hybrid_search(args, storage, ctx, session).await,
        "run_consolidation" => handle_run_consolidation(args, storage, ctx, session).await,
        "enrich_entities" => handle_enrich_entities(args, storage, ctx, session).await,
        "get_stats" => handle_get_stats(args, storage, ctx, session).await,
        "count_entities_by_type" => handle_count_entities_by_type(args, storage, ctx).await,
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
        "manage_claims" => handle_manage_claims(args, storage, ctx).await,
        "manage_approvals" => handle_manage_approvals(args, storage, ctx).await,
        "manage_aliases" => handle_manage_aliases(args, storage, ctx).await,
        "explain_derived" => handle_explain_derived(args, storage, ctx).await,
        "get_effective_rule_set" => handle_get_effective_rule_set(args, storage, ctx).await,
        "promote_predicate" => handle_promote_predicate(args, storage, ctx).await,
        "batch_update_entities" => handle_batch_update_entities(args, storage, ctx, session).await,
        "batch_delete_entities" => handle_batch_delete_entities(args, storage, ctx, session).await,
        "create_edge" => handle_create_edge(args, storage, ctx, session).await,
        "batch_create_edges" => handle_batch_create_edges(args, storage, ctx, session).await,
        "batch_update_edges" => handle_batch_update_edges(args, storage, ctx, session).await,
        "batch_delete_edges" => handle_batch_delete_edges(args, storage, ctx, session).await,
        "list_derived_cache" => handle_list_derived_cache(args, storage, ctx).await,
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
    let is_err = result.is_err();
    let wrapped = result.map(|value| {
        let text = if value.is_string() {
            value.as_str().unwrap().to_string()
        } else {
            serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
        };
        serde_json::json!({
            "content": [{"type": "text", "text": text}]
        })
    });

    // Log tool usage (best-effort).
    let output_bytes = wrapped
        .as_ref()
        .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
        .unwrap_or(0) as i32;
    let estimated_tokens = (input_bytes + output_bytes) / 4;
    let latency_ms = start.elapsed().as_millis() as i32;
    let repo = session.repo.get().map(|s| s.as_str()).unwrap_or("");
    if let Err(e) = storage
        .tool_usage_put(
            ctx,
            name,
            repo,
            input_bytes,
            output_bytes,
            estimated_tokens,
            latency_ms,
            is_err,
        )
        .await
    {
        tracing::debug!(error = %e, "tool usage logging failed");
    }

    wrapped
}

/// Returns true for tier-1 tools (always visible in tools/list).
/// Tier 2 tools are only returned when `include_all: true` is passed.
fn is_tier1(name: &str) -> bool {
    matches!(
        name,
        "smart_ingest"
            | "ingest_entities"
            | "ingest_skill"
            | "retrieve_skills_for_context"
            | "invoke_skill"
            | "ensure_parent_tag"
            | "verify_skill"
            | "hybrid_search"
            | "create_edge"
            | "batch_create_edges"
            | "batch_update_edges"
            | "batch_delete_edges"
            | "batch_update_entities"
            | "batch_delete_entities"
            | "explore_connections"
            | "check_intentions"
            | "set_intention"
            | "complete_intention"
            | "get_stats"
            | "count_entities_by_type"
            | "write_temporal_fact"
            | "get_temporal_chain"
            | "retrieve_entities"
            | "find_memory_chain"
            | "run_consolidation"
            | "record_outcome"
    )
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
            | "ingest_entities"
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
            | "batch_update_edges"
            | "batch_delete_edges"
            | "batch_update_entities"
            | "batch_delete_entities"
            | "enrich_entities"
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let entity_name = require_str(&args, "entity_name")?;
    let entity_type = require_str(&args, "entity_type")?;
    let context_snippet = require_str(&args, "context_snippet")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let source_fold_id = optional_uuid(&args, "source_fold_id")?;
    let confidence = args.get("confidence").and_then(|v| v.as_f64());

    // Auto-generate embedding if not provided and Ollama is configured.
    if embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(&crate::config::EmbeddingConfig {
            provider: "ollama".into(),
            ollama_base_url: session.ollama_base_url.clone(),
            model: session.embed_model.clone(),
            dimensions: session.embed_dimensions,
            ner_model: String::new(),
        });
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
            child_count: None,
            ..Default::default()
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
                        child_count: None,
                        ..Default::default()
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

fn default_ingest_confidence() -> f64 {
    0.9
}

fn default_edge_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum IngestOnConflict {
    #[default]
    Update,
    Skip,
    Error,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestEntitiesOptions {
    embed_missing: bool,
    embedding_model: Option<String>,
    on_conflict: IngestOnConflict,
    strict_edges: bool,
    dry_run: bool,
}

impl Default for IngestEntitiesOptions {
    fn default() -> Self {
        Self {
            embed_missing: true,
            embedding_model: None,
            on_conflict: IngestOnConflict::Update,
            strict_edges: true,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestEntityInput {
    id: uuid::Uuid,
    name: String,
    entity_type: String,
    context: String,
    #[serde(default = "default_ingest_confidence")]
    confidence: f64,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    embedding: Option<Vec<f32>>,
    #[serde(default)]
    attrs: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestEdgeInput {
    src_id: uuid::Uuid,
    dst_id: uuid::Uuid,
    edge_type: String,
    #[serde(default = "default_edge_weight")]
    weight: f64,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestEntitiesRequest {
    tenant_id: uuid::Uuid,
    session_id: uuid::Uuid,
    entities: Vec<IngestEntityInput>,
    #[serde(default)]
    edges: Vec<IngestEdgeInput>,
    #[serde(default)]
    options: IngestEntitiesOptions,
}

fn parse_ingest_state(state: Option<&str>) -> Result<crate::types::MemoryState, String> {
    match state {
        Some(raw) => serde_json::from_value::<crate::types::MemoryState>(Value::String(raw.into()))
            .map_err(|_| {
                format!(
                    "invalid_state: expected one of active|dormant|silent|unavailable, got {raw}"
                )
            }),
        None => Ok(crate::types::MemoryState::Active),
    }
}

fn validate_json_object(value: Option<&Value>, field: &str) -> Result<(), String> {
    match value {
        None => Ok(()),
        Some(Value::Object(_)) => Ok(()),
        Some(Value::Null) => Ok(()),
        Some(_) => Err(format!("{field} must be an object")),
    }
}

fn build_ingest_embedding_client(
    session: &SessionState,
    override_model: Option<&str>,
) -> Option<crate::embedding::EmbeddingClient> {
    if session.ollama_base_url.is_empty() {
        return None;
    }
    let model = override_model
        .filter(|m| !m.is_empty())
        .unwrap_or(&session.embed_model)
        .to_string();
    if model.is_empty() {
        return None;
    }
    Some(crate::embedding::EmbeddingClient::new(
        &crate::config::EmbeddingConfig {
            provider: "ollama".into(),
            ollama_base_url: session.ollama_base_url.clone(),
            model,
            dimensions: session.embed_dimensions,
            ner_model: String::new(),
        },
    ))
}

async fn handle_ingest_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let request: IngestEntitiesRequest = serde_json::from_value(args).map_err(|e| {
        (
            INVALID_PARAMS,
            format!("invalid ingest_entities request: {e}"),
        )
    })?;
    if request.tenant_id != ctx.tenant_id {
        return Err((
            INVALID_PARAMS,
            format!(
                "tenant_id {} does not match authenticated tenant {}",
                request.tenant_id, ctx.tenant_id
            ),
        ));
    }

    let started = std::time::Instant::now();
    let embedding_client =
        build_ingest_embedding_client(session, request.options.embedding_model.as_deref());

    let mut entity_inserted = 0usize;
    let mut entity_updated = 0usize;
    let mut entity_skipped = 0usize;
    let mut entity_failed = Vec::new();
    let mut edge_inserted = 0usize;
    let mut edge_skipped_duplicate = 0usize;
    let mut edge_failed = Vec::new();
    let mut embeddings_computed = 0usize;
    let mut embeddings_received = 0usize;
    let mut embeddings_failed = Vec::new();

    let mut available_entities = std::collections::HashSet::new();
    let mut seen_entity_ids = std::collections::HashSet::new();

    for entity in &request.entities {
        if !seen_entity_ids.insert(entity.id) {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": "duplicate_entity_id_in_batch"
            }));
            continue;
        }

        if !(0.0..=1.0).contains(&entity.confidence) {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": format!("invalid_confidence: {}", entity.confidence)
            }));
            continue;
        }
        if entity.name.trim().is_empty() {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": "missing_name"
            }));
            continue;
        }
        if entity.context.trim().is_empty() {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": "missing_context"
            }));
            continue;
        }
        if let Err(reason) = validate_json_object(entity.attrs.as_ref(), "attrs") {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": reason
            }));
            continue;
        }
        let state = match parse_ingest_state(entity.state.as_deref()) {
            Ok(state) => state,
            Err(reason) => {
                entity_failed.push(serde_json::json!({
                    "id": entity.id.to_string(),
                    "reason": reason
                }));
                continue;
            }
        };

        let existing = storage
            .entity_get_by_id(ctx, request.session_id, entity.id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

        if existing.is_some() {
            match request.options.on_conflict {
                IngestOnConflict::Skip => {
                    entity_skipped += 1;
                    available_entities.insert(entity.id);
                    continue;
                }
                IngestOnConflict::Error => {
                    entity_failed.push(serde_json::json!({
                        "id": entity.id.to_string(),
                        "reason": "conflict: entity already exists"
                    }));
                    continue;
                }
                IngestOnConflict::Update => {}
            }
        }

        let resolved_embedding = if let Some(embedding) = entity.embedding.clone() {
            embeddings_received += 1;
            Some(embedding)
        } else if let Some(existing) = existing.as_ref().and_then(|e| e.entity_embedding.clone()) {
            Some(existing)
        } else if request.options.embed_missing {
            match &embedding_client {
                Some(client) => match client.embed(&entity.context).await {
                    Ok(embedding) => {
                        embeddings_computed += 1;
                        Some(embedding)
                    }
                    Err(err) => {
                        let reason = format!("embedding_failed: {err}");
                        embeddings_failed.push(serde_json::json!({
                            "id": entity.id.to_string(),
                            "reason": reason
                        }));
                        entity_failed.push(serde_json::json!({
                            "id": entity.id.to_string(),
                            "reason": "embedding_failed"
                        }));
                        continue;
                    }
                },
                None => {
                    embeddings_failed.push(serde_json::json!({
                        "id": entity.id.to_string(),
                        "reason": "embedding_unavailable: embed_missing requested but embedding endpoint is not configured"
                    }));
                    entity_failed.push(serde_json::json!({
                        "id": entity.id.to_string(),
                        "reason": "embedding_unavailable"
                    }));
                    continue;
                }
            }
        } else {
            None
        };

        let now = chrono::Utc::now();
        let entry = crate::types::EntityEntry {
            tenant_id: request.tenant_id,
            entity_id: entity.id,
            session_id: request.session_id,
            entity_name: entity.name.clone(),
            entity_type: entity.entity_type.clone(),
            source_fold_id: existing.as_ref().and_then(|e| e.source_fold_id),
            context_snippet: entity.context.clone(),
            entity_embedding: resolved_embedding,
            confidence: entity.confidence,
            state: existing
                .as_ref()
                .map(|e| {
                    if entity.state.is_some() {
                        state.clone()
                    } else {
                        e.state.clone()
                    }
                })
                .unwrap_or(state),
            created_at: existing.as_ref().map(|e| e.created_at).unwrap_or(now),
            description: existing.as_ref().and_then(|e| e.description.clone()),
            description_embedding: existing
                .as_ref()
                .and_then(|e| e.description_embedding.clone()),
            tags: existing
                .as_ref()
                .map(|e| e.tags.clone())
                .unwrap_or_default(),
            properties: entity.attrs.clone().unwrap_or_else(|| {
                existing
                    .as_ref()
                    .map(|e| e.properties.clone())
                    .unwrap_or_else(|| serde_json::json!({}))
            }),
            content_hash: existing.as_ref().and_then(|e| e.content_hash.clone()),
            updated_at: Some(now),
            scope: existing
                .as_ref()
                .map(|e| e.scope)
                .unwrap_or(crate::types::EntityScope::Session),
            ingested_by_session: existing
                .as_ref()
                .and_then(|e| e.ingested_by_session)
                .or(Some(request.session_id)),
        };

        if !request.options.dry_run
            && let Err(err) = storage.entity_put(ctx, &entry).await
        {
            entity_failed.push(serde_json::json!({
                "id": entity.id.to_string(),
                "reason": err.to_string()
            }));
            continue;
        }

        if existing.is_some() {
            entity_updated += 1;
        } else {
            entity_inserted += 1;
        }
        available_entities.insert(entity.id);
    }

    let mut resident_cache = std::collections::HashMap::<uuid::Uuid, bool>::new();
    let mut existing_edge_cache =
        std::collections::HashMap::<uuid::Uuid, Vec<crate::types::TypedEdge>>::new();
    let mut seen_edges = std::collections::HashSet::<(uuid::Uuid, uuid::Uuid, String)>::new();

    for edge in &request.edges {
        if edge.edge_type.trim().is_empty() {
            edge_failed.push(serde_json::json!({
                "src_id": edge.src_id.to_string(),
                "dst_id": edge.dst_id.to_string(),
                "edge_type": edge.edge_type,
                "reason": "missing_edge_type"
            }));
            continue;
        }
        if let Err(reason) = validate_json_object(edge.metadata.as_ref(), "metadata") {
            edge_failed.push(serde_json::json!({
                "src_id": edge.src_id.to_string(),
                "dst_id": edge.dst_id.to_string(),
                "edge_type": edge.edge_type,
                "reason": reason
            }));
            continue;
        }
        let dedupe_key = (edge.src_id, edge.dst_id, edge.edge_type.clone());
        if !seen_edges.insert(dedupe_key) {
            edge_skipped_duplicate += 1;
            continue;
        }

        if request.options.strict_edges {
            let src_ok = if available_entities.contains(&edge.src_id) {
                true
            } else if let Some(found) = resident_cache.get(&edge.src_id) {
                *found
            } else {
                let found = storage
                    .entity_get_by_id(ctx, request.session_id, edge.src_id)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                    .is_some();
                resident_cache.insert(edge.src_id, found);
                found
            };
            if !src_ok {
                edge_failed.push(serde_json::json!({
                    "src_id": edge.src_id.to_string(),
                    "dst_id": edge.dst_id.to_string(),
                    "edge_type": edge.edge_type,
                    "reason": "endpoint_not_found: src_id not resident and not in batch"
                }));
                continue;
            }

            let dst_ok = if available_entities.contains(&edge.dst_id) {
                true
            } else if let Some(found) = resident_cache.get(&edge.dst_id) {
                *found
            } else {
                let found = storage
                    .entity_get_by_id(ctx, request.session_id, edge.dst_id)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                    .is_some();
                resident_cache.insert(edge.dst_id, found);
                found
            };
            if !dst_ok {
                edge_failed.push(serde_json::json!({
                    "src_id": edge.src_id.to_string(),
                    "dst_id": edge.dst_id.to_string(),
                    "edge_type": edge.edge_type,
                    "reason": "endpoint_not_found: dst_id not resident and not in batch"
                }));
                continue;
            }
        }

        let existing_edges = if let Some(edges) = existing_edge_cache.get(&edge.src_id) {
            edges.clone()
        } else {
            let edges = storage
                .typed_edge_list_from(ctx, request.session_id, edge.src_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            existing_edge_cache.insert(edge.src_id, edges.clone());
            edges
        };
        if existing_edges
            .iter()
            .any(|existing| existing.dst_id == edge.dst_id && existing.edge_type == edge.edge_type)
        {
            edge_skipped_duplicate += 1;
            continue;
        }

        if !request.options.dry_run {
            let metadata = edge.metadata.as_ref().map(|value| value.to_string());
            match crate::graph_write::create_typed_edge(
                storage,
                ctx,
                request.session_id,
                edge.src_id,
                edge.edge_type.clone(),
                edge.dst_id,
                edge.weight,
                metadata,
            )
            .await
            {
                Ok(created) => {
                    existing_edge_cache
                        .entry(edge.src_id)
                        .or_default()
                        .push(created);
                }
                Err(err) => {
                    edge_failed.push(serde_json::json!({
                        "src_id": edge.src_id.to_string(),
                        "dst_id": edge.dst_id.to_string(),
                        "edge_type": edge.edge_type,
                        "reason": err.to_string()
                    }));
                    continue;
                }
            }
        }

        edge_inserted += 1;
    }

    let _ = crate::audit::log_write(
        storage,
        ctx,
        "ingest_entities",
        "entity_store",
        &format!(
            "{} entities, {} edges",
            request.entities.len(),
            request.edges.len()
        ),
        request.session_id,
    )
    .await;

    Ok(serde_json::json!({
        "entities": {
            "inserted": entity_inserted,
            "updated": entity_updated,
            "skipped": entity_skipped,
            "failed": entity_failed,
        },
        "edges": {
            "inserted": edge_inserted,
            "skipped_duplicate": edge_skipped_duplicate,
            "failed": edge_failed,
        },
        "embeddings": {
            "computed": embeddings_computed,
            "received": embeddings_received,
            "failed": embeddings_failed,
        },
        "schema_version": "2026-03-01",
        "duration_ms": started.elapsed().as_millis() as u64,
    }))
}

async fn handle_batch_update_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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

    let mut updated: usize = 0;
    let mut unchanged: usize = 0;
    let mut not_found: usize = 0;
    let mut errors: usize = 0;
    let mut results = Vec::with_capacity(entities.len());

    for entity_json in entities {
        let idx = results.len();
        let Some(row) = entity_json.as_object() else {
            errors += 1;
            results.push(serde_json::json!({
                "index": idx,
                "status": "error",
                "reason": format!("batch_update_entities[{idx}] must be an object")
            }));
            continue;
        };

        let entity_id = match row
            .get("entity_id")
            .and_then(|v| v.as_str())
            .and_then(|v| uuid::Uuid::parse_str(v).ok())
        {
            Some(id) => id,
            None => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": format!("batch_update_entities[{idx}] missing/invalid entity_id")
                }));
                continue;
            }
        };

        let mut entity = match storage
            .entity_get_by_id(ctx, session_id, entity_id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        {
            Some(entity) => entity,
            None => {
                not_found += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "not_found"
                }));
                continue;
            }
        };

        let mut mutated = false;
        if let Some(v) = row.get("entity_name") {
            match v.as_str() {
                Some(v) => {
                    entity.entity_name = v.to_string();
                    mutated = true;
                }
                None => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": "entity_name must be a string"
                    }));
                    continue;
                }
            }
        }
        if let Some(v) = row.get("entity_type") {
            match v.as_str() {
                Some(v) => {
                    entity.entity_type = v.to_string();
                    mutated = true;
                }
                None => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": "entity_type must be a string"
                    }));
                    continue;
                }
            }
        }
        if let Some(v) = row.get("context_snippet") {
            match v.as_str() {
                Some(v) => {
                    entity.context_snippet = v.to_string();
                    mutated = true;
                }
                None => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": "context_snippet must be a string"
                    }));
                    continue;
                }
            }
        }

        if let Some(v) = row.get("source_fold_id") {
            if v.is_null() {
                entity.source_fold_id = None;
                mutated = true;
            } else if let Some(raw) = v.as_str() {
                entity.source_fold_id = match uuid::Uuid::parse_str(raw) {
                    Ok(id) => {
                        mutated = true;
                        Some(id)
                    }
                    Err(err) => {
                        errors += 1;
                        results.push(serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": format!("source_fold_id invalid uuid: {err}")
                        }));
                        continue;
                    }
                };
            } else {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "error",
                    "reason": "source_fold_id must be string UUID or null"
                }));
                continue;
            }
        }

        if let Some(v) = row.get("confidence") {
            let confidence = match v.as_f64() {
                Some(value) => value,
                None => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": "confidence must be a number"
                    }));
                    continue;
                }
            };
            if !(0.0..=1.0).contains(&confidence) {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "error",
                    "reason": "confidence must be between 0 and 1"
                }));
                continue;
            }
            entity.confidence = confidence;
            mutated = true;
        }

        if let Some(v) = row.get("state") {
            let state = match v.as_str() {
                Some(state) => state,
                None => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": format!("batch_update_entities[{idx}] state must be a string")
                    }));
                    continue;
                }
            };
            let state = match parse_ingest_state(Some(state)) {
                Ok(state) => state,
                Err(reason) => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": reason
                    }));
                    continue;
                }
            };
            if entity.state != state {
                entity.state = state;
                mutated = true;
            }
        }

        if row.contains_key("description") {
            match row.get("description") {
                Some(value) if value.is_null() => {
                    entity.description = None;
                    mutated = true;
                }
                Some(value) => match value.as_str() {
                    Some(value) => {
                        entity.description = Some(value.to_string());
                        mutated = true;
                    }
                    None => {
                        errors += 1;
                        results.push(serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": "description must be a string or null"
                        }));
                        continue;
                    }
                },
                None => {}
            }
        }

        if row.contains_key("tags") {
            let tags = row.get("tags").unwrap();
            match tags {
                Value::Array(values) => {
                    let mut parsed_tags = Vec::with_capacity(values.len());
                    let mut invalid = false;
                    for v in values {
                        match v.as_str() {
                            Some(tag) => parsed_tags.push(tag.to_string()),
                            None => {
                                errors += 1;
                                results.push(serde_json::json!({
                                    "index": idx,
                                    "entity_id": entity_id.to_string(),
                                    "status": "error",
                                    "reason": "tags must be an array of strings"
                                }));
                                invalid = true;
                                break;
                            }
                        }
                    }
                    if invalid {
                        continue;
                    }
                    entity.tags = parsed_tags;
                    mutated = true;
                }
                Value::Null => {
                    entity.tags = Vec::new();
                    mutated = true;
                }
                _ => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": "tags must be an array of strings"
                    }));
                    continue;
                }
            }
        }

        if row.contains_key("properties") {
            match row.get("properties") {
                Some(value) if value.is_object() || value.is_null() => {
                    entity.properties = value.clone();
                    mutated = true;
                }
                _ => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "entity_id": entity_id.to_string(),
                        "status": "error",
                        "reason": "properties must be an object"
                    }));
                    continue;
                }
            }
        }

        if row.contains_key("embedding") {
            match row.get("embedding") {
                Some(value) if value.is_null() => {
                    entity.entity_embedding = None;
                    mutated = true;
                }
                Some(value) => match value.as_array() {
                    Some(values) => {
                        let mut embedding = Vec::with_capacity(values.len());
                        let mut invalid = false;
                        for value in values {
                            match value.as_f64() {
                                Some(v) => embedding.push(v as f32),
                                None => {
                                    invalid = true;
                                    break;
                                }
                            }
                        }
                        if invalid {
                            errors += 1;
                            results.push(serde_json::json!({
                                "index": idx,
                                "entity_id": entity_id.to_string(),
                                "status": "error",
                                "reason": "embedding must be a number array"
                            }));
                            continue;
                        }
                        entity.entity_embedding = Some(embedding);
                        mutated = true;
                    }
                    None => {
                        errors += 1;
                        results.push(serde_json::json!({
                            "index": idx,
                            "entity_id": entity_id.to_string(),
                            "status": "error",
                            "reason": "embedding must be an array"
                        }));
                        continue;
                    }
                },
                None => {}
            }
        }

        if !mutated {
            unchanged += 1;
            results.push(serde_json::json!({
                "index": idx,
                "entity_id": entity_id.to_string(),
                "status": "unchanged"
            }));
            continue;
        }

        entity.updated_at = Some(chrono::Utc::now());
        match storage.entity_put(ctx, &entity).await {
            Ok(_) => {
                updated += 1;
                session.dirty.store(true, Ordering::Relaxed);
                session.last_activity.notify_waiters();
                results.push(serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "updated"
                }));
            }
            Err(err) => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "error",
                    "reason": err.to_string()
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "updated": updated,
        "unchanged": unchanged,
        "not_found": not_found,
        "errors": errors,
        "total": entities.len(),
        "results": results,
    }))
}

async fn handle_batch_delete_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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

    let mut deleted: usize = 0;
    let mut not_found: usize = 0;
    let mut errors: usize = 0;
    let mut results = Vec::with_capacity(entities.len());

    for entity_json in entities {
        let idx = results.len();
        let Some(row) = entity_json.as_object() else {
            errors += 1;
            results.push(serde_json::json!({
                "index": idx,
                "status": "error",
                "reason": format!("batch_delete_entities[{idx}] must be an object")
            }));
            continue;
        };

        let entity_id = match row
            .get("entity_id")
            .and_then(|v| v.as_str())
            .and_then(|v| uuid::Uuid::parse_str(v).ok())
        {
            Some(id) => id,
            None => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": format!(
                        "batch_delete_entities[{idx}] missing/invalid entity_id"
                    )
                }));
                continue;
            }
        };

        match storage.entity_delete(ctx, session_id, entity_id).await {
            Ok(true) => {
                deleted += 1;
                session.dirty.store(true, Ordering::Relaxed);
                session.last_activity.notify_waiters();
                results.push(serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "deleted"
                }));
            }
            Ok(false) => {
                not_found += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "not_found"
                }));
            }
            Err(err) => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "entity_id": entity_id.to_string(),
                    "status": "error",
                    "reason": err.to_string()
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "deleted": deleted,
        "not_found": not_found,
        "errors": errors,
        "total": entities.len(),
        "results": results,
    }))
}

async fn handle_retrieve_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let query = require_str(&args, "query")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;

    // Auto-generate query embedding for ANN search if Ollama is configured.
    if embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(&crate::config::EmbeddingConfig {
            provider: "ollama".into(),
            ollama_base_url: session.ollama_base_url.clone(),
            model: session.embed_model.clone(),
            dimensions: session.embed_dimensions,
            ner_model: String::new(),
        });
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
                format!(
                    "{}...",
                    &e.context_snippet[..e.context_snippet.floor_char_boundary(200)]
                )
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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

    // Apply small negative reputation to entities that caused a retrieval miss
    if program_type == "retrieval_miss"
        && let Some(entity_ids) = args.get("entity_ids").and_then(|v| v.as_array())
    {
        let mut deltas = std::collections::HashMap::new();
        for id_val in entity_ids {
            if let Some(id_str) = id_val.as_str()
                && let Ok(eid) = id_str.parse::<uuid::Uuid>()
            {
                deltas.insert(eid, -0.05);
            }
        }
        if !deltas.is_empty()
            && let Err(e) =
                crate::pagerank::update_reputation_scores(storage, ctx, session_id, &deltas).await
        {
            tracing::warn!("failed to penalize retrieval-miss entity reputation: {e}");
        }
    }

    let mut response = serde_json::json!({ "recorded": recorded });
    if program_type == "retrieval_miss" {
        response["_hint"] = serde_json::json!(
            "Retrieval miss logged. The system will learn to store this kind of information. Consider using smart_ingest now to store what you found via grep/read."
        );
    } else {
        response["_hint"] = serde_json::json!(
            "Outcome recorded. This feedback improves retrieval routing over time."
        );
    }
    Ok(response)
}

// --- Session lifecycle handler ---

async fn handle_delete_session<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let content = require_str(&args, "content")?;
    let entity_type = require_str(&args, "entity_type")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let source_fold_id = optional_uuid(&args, "source_fold_id")?;

    // Auto-generate embedding if not provided and Ollama is configured.
    if embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(&crate::config::EmbeddingConfig {
            provider: "ollama".into(),
            ollama_base_url: session.ollama_base_url.clone(),
            model: session.embed_model.clone(),
            dimensions: session.embed_dimensions,
            ner_model: String::new(),
        });
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
                child_count: None,
                ..Default::default()
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

    // Penalize reputation of superseded entity — it was wrong/outdated
    if action == "Superseded"
        && let Some(old_id_str) = decision_json.get("old_entity_id").and_then(|v| v.as_str())
        && let Ok(old_id) = old_id_str.parse::<uuid::Uuid>()
    {
        let mut deltas = std::collections::HashMap::new();
        deltas.insert(old_id, -0.2);
        if let Err(e) =
            crate::pagerank::update_reputation_scores(storage, ctx, session_id, &deltas).await
        {
            tracing::warn!("failed to penalize superseded entity reputation: {e}");
        }
    }

    // Add rotating hint to encourage continued memory formation
    let mut result = decision_json;
    if let Some(obj) = result.as_object_mut() {
        let hint = match action.as_str() {
            "Skipped" => "Content too similar to existing memory. Try a different aspect or more specific insight.".to_string(),
            _ => pick_hint(INGEST_HINTS),
        };
        obj.insert("hint".into(), Value::String(hint));

        // Progressive disclosure hints
        match action.as_str() {
            "Created" => {
                obj.insert("_hint".into(), serde_json::json!(
                    "Entity created. Use create_edge to connect it to related entities. After 10+ entities, run_consolidation discovers patterns."
                ));
            }
            "Superseded" => {
                obj.insert("_hint".into(), serde_json::json!(
                    "Previous fact superseded. Use get_temporal_chain to see the full fact evolution for this entity."
                ));
            }
            _ => {}
        }
    }
    Ok(result)
}

// --- Skills handlers ---

async fn handle_ingest_skill<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    // Reject unknown top-level keys. Fail-loud per the project rule: silent
    // field drops hide schema drift for weeks. Every key a caller passes must
    // be one the server recognizes, or the caller needs to learn about the
    // mismatch the first time they make it.
    const KNOWN_KEYS: &[&str] = &[
        "name",
        "category",
        "description",
        "session_id",
        "trigger_keywords",
        "tags",
        "prerequisites",
        "steps",
        "output_artifacts",
        "completion_criteria",
        "content_hash",
    ];
    if let Some(obj) = args.as_object() {
        let unknown: Vec<&str> = obj
            .keys()
            .map(|s| s.as_str())
            .filter(|k| !KNOWN_KEYS.contains(k))
            .collect();
        if !unknown.is_empty() {
            return Err((
                -32602,
                format!(
                    "unknown field(s) on ingest_skill: {}. Known: {}",
                    unknown.join(", "),
                    KNOWN_KEYS.join(", ")
                ),
            ));
        }
    }

    let name = require_str(&args, "name")?.to_string();
    let category = require_str(&args, "category")?.to_string();
    let description = require_str(&args, "description")?.to_string();
    let caller_session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let trigger_keywords = args
        .get("trigger_keywords")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let tags = args
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let prerequisites = args
        .get("prerequisites")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // Propagate serde errors — never default-on-failure. A malformed step
    // array with keys like {title, body} instead of {phase, instruction}
    // must surface as an error, not a silently empty `steps: []`.
    let steps: Vec<crate::skill::Step> = match args.get("steps").cloned() {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| (-32602, format!("invalid `steps` payload: {e}")))?,
        None => Vec::new(),
    };
    let output_artifacts = args
        .get("output_artifacts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let completion_criteria = args
        .get("completion_criteria")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let content_hash = args
        .get("content_hash")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let params = crate::skill::IngestSkillParams {
        name,
        category,
        description,
        trigger_keywords,
        tags,
        prerequisites,
        steps,
        output_artifacts,
        completion_criteria,
        content_hash,
        caller_session_id,
    };

    // Build an embedding client from session config so description_embedding
    // is populated alongside the skill entity.
    let embed_client = if !session.ollama_base_url.is_empty() {
        Some(crate::embedding::EmbeddingClient::new(
            &crate::config::EmbeddingConfig {
                provider: "ollama".into(),
                ollama_base_url: session.ollama_base_url.clone(),
                model: session.embed_model.clone(),
                dimensions: session.embed_dimensions,
                ner_model: String::new(),
            },
        ))
    } else {
        None
    };

    let action = crate::skill::ingest_skill(
        storage,
        ctx,
        params,
        embed_client.as_ref(),
        session.graph.as_deref(),
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let mut result = serde_json::to_value(&action).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "_hint".into(),
            Value::String(
                "Skills are global knowledge shared across every session. If you \
                 use this skill and learn something new — a better step, a missing \
                 prerequisite, a clearer description — call ingest_skill again with \
                 the refinement. Your changes persist."
                    .into(),
            ),
        );
    }
    Ok(result)
}

async fn handle_retrieve_skills_for_context<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let context = require_str(&args, "context")?.to_string();
    let caller_session = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(5)
        .clamp(1, 20);
    let min_score = args
        .get("min_score")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let mut query_embedding = args.get("embedding").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|n| n.as_f64().map(|f| f as f32))
            .collect::<Vec<f32>>()
    });
    if query_embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(&crate::config::EmbeddingConfig {
            provider: "ollama".into(),
            ollama_base_url: session.ollama_base_url.clone(),
            model: session.embed_model.clone(),
            dimensions: session.embed_dimensions,
            ner_model: String::new(),
        });
        if let Ok(emb) = client.embed(&context).await {
            query_embedding = Some(emb);
        }
    }

    // Used-in-session signal — retrieval_tracker records which entity_ids
    // have been touched in this session.
    let used_ids: std::collections::HashSet<uuid::Uuid> = {
        let tracker = session.retrieval_tracker.lock().await;
        tracker.recent_ids(50).into_iter().collect()
    };

    let hits = crate::skill::retrieve_skills_for_context(
        storage,
        ctx,
        caller_session,
        &context,
        query_embedding.as_deref(),
        limit,
        min_score,
        &used_ids,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let hint = if hits.is_empty() {
        "No skills matched. Ingest one with ingest_skill, or broaden the context."
    } else {
        "These skills are shared across all sessions. If you successfully apply one, \
         remember it. If you discover a refinement, call ingest_skill to persist it."
    };

    Ok(serde_json::json!({
        "results": hits,
        "_hint": hint,
    }))
}

async fn handle_invoke_skill<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    _session: &SessionState,
) -> Result<Value, (i32, String)> {
    let name = require_str(&args, "skill_name")?.to_string();

    let entity = crate::skill::get_skill_by_name(storage, ctx, &name)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let entity = match entity {
        Some(e) => e,
        None => {
            let similar = crate::skill::similar_skill_names(storage, ctx, &name, 3).await;
            let payload = serde_json::json!({
                "error": format!("skill not found: '{}'", name),
                "did_you_mean": similar,
                "hint": "Call retrieve_skills_for_context to discover available skills, \
                        or ingest_skill to add this one.",
            });
            return Err((INVALID_PARAMS, payload.to_string()));
        }
    };

    let result = crate::skill::build_invoke_result(&entity);
    serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_ensure_parent_tag<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let child_tag = require_str(&args, "child_tag")?.to_string();
    let parent_tag = require_str(&args, "parent_tag")?.to_string();
    let caller_session = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let action = crate::skill::ensure_parent_tag(
        storage,
        ctx,
        caller_session,
        &child_tag,
        &parent_tag,
        session.graph.as_deref(),
    )
    .await
    .map_err(|e| (INVALID_PARAMS, e.to_string()))?;

    serde_json::to_value(&action).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

async fn handle_verify_skill<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    _session: &SessionState,
) -> Result<Value, (i32, String)> {
    let name = require_str(&args, "skill_name")?.to_string();
    let result = crate::skill::verify_skill(storage, ctx, &name)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Intention handlers ---

/// Resolve repo from tool args, falling back to session default.
fn resolve_repo<'a>(args: &'a Value, session: &'a SessionState) -> &'a str {
    args.get("repo")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| session.repo.get().map(|s| s.as_str()).unwrap_or(""))
}

async fn handle_set_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let repo = resolve_repo(&args, session);
    if repo.is_empty() {
        return Err((
            INVALID_PARAMS,
            "repo is required (pass explicitly or configure server repo)".into(),
        ));
    }
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
    let intention = store.set(repo, description, trigger, priority);
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
    let repo = resolve_repo(&args, session);
    let context = require_str(&args, "context")?;
    let mut store = session.intentions.lock().await;
    let triggered = store.check(context, repo);
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
            .intention_update_status(
                ctx,
                repo,
                intention.id,
                &status_str,
                intention.triggered_at,
                None,
            )
            .await
        {
            tracing::warn!(id = %intention.id, error = %e, "failed to persist intention trigger");
        }
    }

    let triggered_count = triggered_json.len();
    let mut response = serde_json::json!({ "triggered": triggered_json });
    if triggered_count > 0 {
        response["_hint"] = serde_json::json!(format!(
            "{triggered_count} intentions triggered. Act on them, then call complete_intention for each."
        ));
    }
    Ok(response)
}

async fn handle_complete_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let repo = resolve_repo(&args, session);
    let id = require_uuid(&args, "intention_id")?;
    let mut store = session.intentions.lock().await;
    let completed = store.complete(id);

    if completed
        && let Err(e) = storage
            .intention_update_status(ctx, repo, id, "completed", None, Some(chrono::Utc::now()))
            .await
    {
        tracing::warn!(%id, error = %e, "failed to persist intention completion");
    }

    Ok(serde_json::json!({ "completed": completed }))
}

async fn handle_list_intentions<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let all_repos = args
        .get("all_repos")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if all_repos {
        // Load from CQL across all repos
        let intentions = storage
            .intention_list_all(ctx)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
        let json: Vec<Value> = intentions
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        Ok(serde_json::json!({ "intentions": json, "source": "all_repos" }))
    } else {
        // In-memory store (current repo only)
        let store = session.intentions.lock().await;
        let intentions = store.list();
        let json: Vec<Value> = intentions
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        Ok(serde_json::json!({ "intentions": json, "source": "session" }))
    }
}

async fn handle_snooze_intention<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let repo = resolve_repo(&args, session);
    let id = require_uuid(&args, "intention_id")?;
    let mut store = session.intentions.lock().await;
    let snoozed = store.snooze(id);

    if snoozed
        && let Err(e) = storage
            .intention_update_status(ctx, repo, id, "pending", None, None)
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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

async fn handle_explore_connections<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let traversal = require_str(&args, "traversal")?;
    let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let results = match traversal {
        "fold_ancestors" => {
            let graph = session
                .graph
                .as_ref()
                .ok_or((INTERNAL_ERROR, "graph client not configured".into()))?;
            let fold_id = require_uuid(&args, "fold_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            graph
                .get_fold_ancestors(fold_id, session_id, max_depth)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
        }
        "related_entities" => {
            let entity_id = require_uuid(&args, "entity_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            // Try graph backend first, fall back to CQL typed_edges if empty.
            // A graph error is a DESIGNED fallback (the graph client may be
            // disabled, unreachable, or temporarily degraded) — but it must
            // be logged so operators can see silent fall-through.
            let graph_results = if let Some(graph) = session.graph.as_ref() {
                match graph
                    .find_related_entities(entity_id, session_id, max_depth)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            %entity_id,
                            %session_id,
                            error = %e,
                            "related_entities: graph backend failed; falling back to CQL typed_edges"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            if !graph_results.is_empty() {
                let mut r = graph_results;
                r.truncate(limit);
                r
            } else {
                // CQL fallback: query typed_edges for direct connections
                let edges = storage
                    .edge_list_for_entity(ctx, entity_id)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
                let mut results: Vec<String> = Vec::new();
                for (other_id, edge_type) in edges.into_iter().take(limit) {
                    let name = storage
                        .entity_get_by_id(ctx, session_id, other_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|e| e.entity_name)
                        .unwrap_or_else(|| other_id.to_string());
                    results.push(
                        serde_json::to_string(&serde_json::json!({
                            "entity_id": other_id.to_string(),
                            "entity_name": name,
                            "edge_type": edge_type,
                        }))
                        .unwrap_or_default(),
                    );
                }
                results
            }
        }
        "entities_in_fold" => {
            let graph = session
                .graph
                .as_ref()
                .ok_or((INTERNAL_ERROR, "graph client not configured".into()))?;
            let fold_id = require_uuid(&args, "fold_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            let mut r = graph
                .get_entities_in_fold(fold_id, session_id)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            r.truncate(limit);
            r
        }
        "supersession_chain" => {
            let graph = session
                .graph
                .as_ref()
                .ok_or((INTERNAL_ERROR, "graph client not configured".into()))?;
            let entity_id = require_uuid(&args, "entity_id")?;
            let event_id = entity_id;
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

    let result_count = results.len();
    let mut response = serde_json::json!({
        "traversal": traversal,
        "results": results,
        "count": result_count
    });
    if result_count < 2 {
        response["_hint"] = serde_json::json!(
            "Few connections found. Try spread_activation for broader associative recall, or run_consolidation to discover CO_OCCURS patterns."
        );
    }
    Ok(response)
}

// --- Hybrid search handler ---

async fn handle_hybrid_search<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let query = require_str(&args, "query")?;
    let mut embedding = optional_f32_array(&args, "embedding")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    // Auto-generate query embedding for ANN search if Ollama is configured.
    if embedding.is_none() && !session.ollama_base_url.is_empty() {
        let client = crate::embedding::EmbeddingClient::new(&crate::config::EmbeddingConfig {
            provider: "ollama".into(),
            ollama_base_url: session.ollama_base_url.clone(),
            model: session.embed_model.clone(),
            dimensions: session.embed_dimensions,
            ner_model: String::new(),
        });
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
        None,
        &crate::hybrid_search::FusionConfig::default(),
        None,
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let result_count = results.len();
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

    let mut response = serde_json::json!({
        "results": results,
        "count": result_count,
        "hint": hint
    });
    if result_count == 0 {
        response["_hint"] = serde_json::json!(
            "No results. Try retrieve_entities with a different name spelling, or check if the information has been stored yet."
        );
    } else if result_count < 3 {
        response["_hint"] = serde_json::json!(
            "Few results found. Try recursive_explore for multi-pass decomposed search, or spread_activation for broader graph-based discovery."
        );
    }
    Ok(response)
}

// --- Dream consolidation handler ---

async fn handle_run_consolidation<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

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

// --- Enrichment handler ---

async fn handle_enrich_entities<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    let session_id = require_uuid(&args, "session_id")?;
    let operations = args
        .get("operations")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["enrich".into(), "annotate".into(), "lint".into()]);
    let entity_type_filter = args
        .get("entity_types")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let enrich_config = crate::enrich::EnrichRunConfig {
        llm_base_url: session.enrich_llm_url.clone(),
        llm_model: session.enrich_llm_model.clone(),
        operations,
        entity_type_filter,
        force,
        dry_run,
        batch_size: 10,
        ollama_base_url: session.ollama_base_url.clone(),
        embed_model: session.embed_model.clone(),
        embed_dimensions: session.embed_dimensions,
    };

    let result = crate::enrich::run_enrichment(storage, ctx, session_id, &enrich_config)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    serde_json::to_value(&result).map_err(|e| (INTERNAL_ERROR, e.to_string()))
}

// --- Stats handler ---

async fn handle_get_stats<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
    session: &SessionState,
) -> Result<Value, (i32, String)> {
    // Default to the nil UUID session — this matches what smart_ingest /
    // hybrid_search / retrieve_entities use when session_id is omitted, so
    // get_stats reports the same data those tools see. Returning 0 here
    // (the prior behavior) made default-session entities look like
    // phantoms even when they were correctly persisted.
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

    let memo_count = storage.memo_count(ctx).await.unwrap_or(0);
    let memo_total_hits = storage.memo_total_hits(ctx).await.unwrap_or(0);
    let memo_hit_rate = if memo_count > 0 {
        memo_total_hits as f64 / memo_count as f64
    } else {
        0.0
    };

    let entity_count = storage.entity_count(ctx, session_id).await.unwrap_or(0);

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

    let mut response = serde_json::json!({
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
    });
    if entity_count > 0 && edge_count == 0 {
        response["_hint"] = serde_json::json!(
            "Entities exist but no connections. Use create_edge to link related entities — connected facts are knowledge, isolated facts are data."
        );
    } else if entity_count > 20 && edge_count < 5 {
        response["_hint"] = serde_json::json!(
            "Many entities but few connections. Run run_consolidation to discover CO_OCCURS relationships automatically."
        );
    }
    Ok(response)
}

async fn handle_count_entities_by_type<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let start = std::time::Instant::now();
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let rows = storage
        .entity_counts_by_type_and_state(ctx, session_id)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let mut total = 0usize;
    let mut by_entity_type = serde_json::Map::new();
    let mut by_state = serde_json::Map::new();
    let mut by_type_and_state: std::collections::BTreeMap<String, serde_json::Map<String, Value>> =
        std::collections::BTreeMap::new();

    for row in rows {
        total += row.count;

        let type_entry = by_entity_type
            .entry(row.entity_type.clone())
            .or_insert_with(|| serde_json::json!(0));
        let type_total = type_entry.as_u64().unwrap_or(0) + row.count as u64;
        *type_entry = serde_json::json!(type_total);

        let state_key = row.state.to_string();
        let state_entry = by_state
            .entry(state_key.clone())
            .or_insert_with(|| serde_json::json!(0));
        let state_total = state_entry.as_u64().unwrap_or(0) + row.count as u64;
        *state_entry = serde_json::json!(state_total);

        by_type_and_state
            .entry(row.entity_type)
            .or_default()
            .insert(state_key, serde_json::json!(row.count));
    }

    let sum_by_type: usize = by_entity_type
        .values()
        .map(|v| v.as_u64().unwrap_or(0) as usize)
        .sum();
    let sum_by_state: usize = by_state
        .values()
        .map(|v| v.as_u64().unwrap_or(0) as usize)
        .sum();
    let sum_joint: usize = by_type_and_state
        .values()
        .flat_map(|inner| inner.values())
        .map(|v| v.as_u64().unwrap_or(0) as usize)
        .sum();

    if total != sum_by_type || total != sum_by_state || total != sum_joint {
        tracing::error!(
            total,
            sum_by_type,
            sum_by_state,
            sum_joint,
            "count_entities_by_type invariant mismatch"
        );
        return Err((
            INTERNAL_ERROR,
            "count_entities_by_type invariant mismatch".to_string(),
        ));
    }
    debug_assert_eq!(total, sum_by_type);
    debug_assert_eq!(total, sum_by_state);
    debug_assert_eq!(total, sum_joint);

    Ok(serde_json::json!({
        "total": total,
        "by_entity_type": by_entity_type,
        "by_state": by_state,
        "by_type_and_state": by_type_and_state,
        "duration_ms": start.elapsed().as_millis() as u64,
    }))
}

// --- Spreading activation handler ---

async fn handle_find_memory_chain<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

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
        .unwrap_or(uuid::Uuid::nil());

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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

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
            let source = args
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or(if family == "*" { "builtin" } else { "registry" });

            let results: Vec<Value> = match source {
                "builtin" => crate::datalog::synthetic_builtin_rule_entries(ctx.tenant_id)
                    .into_iter()
                    .filter(|r| family == "*" || r.family == family)
                    .map(|r| {
                        serde_json::json!({
                            "source": "builtin",
                            "rule_id": r.rule_id,
                            "version": r.version,
                            "name": r.name,
                            "family": r.family,
                            "state": r.state.to_string(),
                            "rule_body": r.rule_body,
                            "rule_weight": r.rule_weight,
                        })
                    })
                    .collect(),
                "registry" => {
                    let stored_rules = if family == "*" {
                        storage
                            .rule_list_active(ctx, crate::types::RuleState::Active)
                            .await
                            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                    } else {
                        storage
                            .rule_list_family(ctx, family, crate::types::RuleState::Active)
                            .await
                            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                    };
                    let mut rows = Vec::with_capacity(stored_rules.len());
                    for r in stored_rules {
                        let approval_state = crate::expert_system::approval_state(
                            storage,
                            ctx,
                            crate::types::ArtifactKind::Rule,
                            &r.rule_id,
                        )
                        .await
                        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
                        rows.push(serde_json::json!({
                            "source": "registry",
                            "rule_id": r.rule_id,
                            "version": r.version,
                            "name": r.name,
                            "family": r.family,
                            "state": r.state.to_string(),
                            "approval_state": approval_state.map(|state| state.to_string()),
                            "rule_body": r.rule_body,
                            "rule_weight": r.rule_weight,
                        }));
                    }
                    rows
                }
                "effective" => {
                    crate::datalog::load_effective_rule_entries(storage, ctx, Some(family))
                        .await
                        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                        .into_iter()
                        .map(|rule| {
                            serde_json::json!({
                                "source": match rule.source {
                                    crate::datalog::RuleSource::Builtin => "builtin",
                                    crate::datalog::RuleSource::Registry => "registry",
                                },
                                "rule_id": rule.entry.rule_id,
                                "version": rule.entry.version,
                                "name": rule.entry.name,
                                "family": rule.entry.family,
                                "state": rule.entry.state.to_string(),
                                "rule_body": rule.entry.rule_body,
                                "rule_weight": rule.entry.rule_weight,
                            })
                        })
                        .collect()
                }
                _ => {
                    return Err((
                        INVALID_PARAMS,
                        "Unknown source. Use builtin, registry, or effective.".into(),
                    ));
                }
            };

            Ok(serde_json::json!({
                "action": "list",
                "source": source,
                "rules": results,
                "count": results.len()
            }))
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

            let parsed = crate::datalog::parse_rule(rule_body)
                .map_err(|e| (INVALID_PARAMS, format!("Invalid rule syntax: {e}")))?;
            let family = args
                .get("family")
                .and_then(|v| v.as_str())
                .unwrap_or(&parsed.head.predicate);
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

            let approval = crate::expert_system::record_approval(
                storage,
                ctx,
                crate::types::ArtifactKind::Rule,
                rule_id,
                crate::types::ApprovalDecision::Proposed,
                Some("rule submitted for review".to_string()),
                "rule".to_string(),
                None,
                None,
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            invalidate_predicate_cache(storage, ctx, &parsed.head.predicate).await;

            Ok(serde_json::json!({
                "action": "put",
                "rule_id": rule_id,
                "version": version,
                "approval": approval_json(&approval),
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

async fn invalidate_predicate_cache<S: crate::storage::Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    predicate: &str,
) {
    let mut keys_to_clear = std::collections::BTreeSet::new();
    keys_to_clear.insert(predicate.to_string());

    if let Ok(rows) = storage.derived_cache_list_all(ctx, 10_000).await {
        for row in rows {
            if row.predicate == predicate
                && let Some(key) = row.cache_key
            {
                keys_to_clear.insert(key);
            }
        }
    }

    for key in keys_to_clear {
        let _ = storage.derived_cache_clear(ctx, &key).await;
    }
}

fn approval_json(entry: &crate::types::ApprovalEntry) -> Value {
    serde_json::json!({
        "approval_id": entry.approval_id,
        "artifact_kind": entry.artifact_kind,
        "artifact_ref": entry.artifact_ref,
        "decision": entry.decision,
        "review_note": entry.review_note,
        "reviewer": entry.reviewer,
        "scope": entry.scope,
        "workspace_scope": entry.workspace_scope,
        "session_scope": entry.session_scope,
        "mirror_entity_id": entry.mirror_entity_id,
        "created_at": entry.created_at,
    })
}

fn claim_json(entry: &crate::types::EntityEntry) -> Value {
    serde_json::json!({
        "claim_id": entry.entity_name,
        "claim_entity_id": entry.entity_id,
        "claim_text": entry.properties.get("claim_text").and_then(|v| v.as_str()).unwrap_or(&entry.context_snippet),
        "domain": entry.properties.get("domain").and_then(|v| v.as_str()),
        "status": crate::expert_system::claim_status_from_entity(entry).to_string(),
        "confidence": entry.properties.get("confidence").and_then(|v| v.as_f64()).unwrap_or(entry.confidence),
        "source_ref": entry.properties.get("source_ref"),
        "support_count": entry.properties.get("support_count").and_then(|v| v.as_i64()).unwrap_or_default(),
        "workspace_scope": entry.properties.get("workspace_scope"),
        "session_scope": entry.properties.get("session_scope"),
        "updated_at": entry.updated_at.unwrap_or(entry.created_at),
    })
}

fn alias_json(entry: &crate::types::AliasEntry) -> Value {
    serde_json::json!({
        "alias_id": entry.alias_id,
        "alias_name": entry.alias_name,
        "scope_kind": entry.scope_kind,
        "scope_ref": entry.scope_ref,
        "canonical_tool": entry.canonical_tool,
        "parameter_map": entry.parameter_map,
        "fixed_arguments": entry.fixed_arguments,
        "args_templates": entry.args_templates,
        "status": entry.status,
        "created_at": entry.created_at,
        "updated_at": entry.updated_at,
    })
}

async fn handle_manage_claims<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let action = require_str(&args, "action")?;
    match action {
        "list" => {
            let include_unapproved = args
                .get("include_unapproved")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let session_id = optional_uuid(&args, "session_id")?;
            let mut claims: Vec<crate::types::EntityEntry> = storage
                .entity_list_all(ctx)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                .into_iter()
                .filter(|entry| entry.entity_type == crate::expert_system::CLAIM_ENTITY_TYPE)
                .filter(|entry| session_id.is_none_or(|session_id| entry.session_id == session_id))
                .filter(|entry| {
                    include_unapproved
                        || matches!(
                            crate::expert_system::claim_status_from_entity(entry),
                            crate::types::ClaimStatus::Approved
                        )
                })
                .collect();
            claims.sort_by(|left, right| {
                right
                    .updated_at
                    .unwrap_or(right.created_at)
                    .cmp(&left.updated_at.unwrap_or(left.created_at))
            });
            let rows: Vec<Value> = claims.iter().map(claim_json).collect();
            Ok(serde_json::json!({
                "action": "list",
                "claims": rows,
                "count": rows.len(),
            }))
        }
        "get" => {
            let claim_id = require_str(&args, "claim_id")?;
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            let claim = storage
                .entity_get_by_id(
                    ctx,
                    session_id,
                    crate::expert_system::claim_entity_id(claim_id),
                )
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({
                "action": "get",
                "claim": claim.as_ref().map(claim_json),
            }))
        }
        "put" => {
            let claim_id = require_str(&args, "claim_id")?;
            let claim_text = require_str(&args, "claim_text")?;
            let domain = args
                .get("domain")
                .and_then(|value| value.as_str())
                .unwrap_or("general");
            let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
            let status = args
                .get("status")
                .and_then(|value| value.as_str())
                .map(crate::expert_system::parse_claim_status)
                .transpose()
                .map_err(|e| (INVALID_PARAMS, e.to_string()))?
                .unwrap_or(crate::types::ClaimStatus::Proposed);
            let confidence = args
                .get("confidence")
                .and_then(|value| value.as_f64())
                .unwrap_or(1.0);
            let support_count = args
                .get("support_count")
                .and_then(|value| value.as_i64())
                .unwrap_or(0) as i32;
            let source_ref = args.get("source_ref").and_then(|value| value.as_str());
            let workspace_scope = args.get("workspace_scope").and_then(|value| value.as_str());

            let claim = crate::expert_system::claim_entity(
                ctx,
                claim_id,
                session_id,
                claim_text,
                domain,
                status,
                confidence,
                source_ref,
                support_count,
                workspace_scope,
            );

            storage
                .entity_put(ctx, &claim)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            let decision = match status {
                crate::types::ClaimStatus::Proposed => crate::types::ApprovalDecision::Proposed,
                crate::types::ClaimStatus::Approved => crate::types::ApprovalDecision::Approved,
                crate::types::ClaimStatus::Rejected => crate::types::ApprovalDecision::Rejected,
            };
            let approval = crate::expert_system::record_approval(
                storage,
                ctx,
                crate::types::ArtifactKind::Claim,
                claim_id,
                decision,
                Some(format!("claim status set to {status}")),
                "claim".to_string(),
                workspace_scope.map(str::to_string),
                Some(session_id),
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            Ok(serde_json::json!({
                "action": "put",
                "claim": claim_json(&claim),
                "approval": approval_json(&approval),
            }))
        }
        _ => Err((
            INVALID_PARAMS,
            format!("Unknown action: {action}. Use list/get/put."),
        )),
    }
}

async fn handle_manage_approvals<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let action = require_str(&args, "action")?;
    let artifact_kind =
        crate::expert_system::parse_artifact_kind(require_str(&args, "artifact_kind")?)
            .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
    let artifact_ref = require_str(&args, "artifact_ref")?;

    match action {
        "list" => {
            let rows = storage
                .approval_list(ctx, &artifact_kind.to_string(), artifact_ref)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            let rows: Vec<Value> = rows.iter().map(approval_json).collect();
            Ok(serde_json::json!({
                "action": "list",
                "approvals": rows,
                "count": rows.len(),
            }))
        }
        "latest" => {
            let latest = storage
                .approval_latest(ctx, &artifact_kind.to_string(), artifact_ref)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({
                "action": "latest",
                "approval": latest.as_ref().map(approval_json),
            }))
        }
        "record" => {
            let decision = args
                .get("decision")
                .and_then(|value| value.as_str())
                .ok_or((INVALID_PARAMS, "missing required string: decision".into()))
                .and_then(|value| {
                    crate::expert_system::parse_approval_decision(value)
                        .map_err(|e| (INVALID_PARAMS, e.to_string()))
                })?;
            let review_note = args
                .get("review_note")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let scope = args
                .get("scope")
                .and_then(|value| value.as_str())
                .unwrap_or("operator")
                .to_string();
            let workspace_scope = args
                .get("workspace_scope")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let session_scope = optional_uuid(&args, "session_scope")?;

            let approval = crate::expert_system::record_approval(
                storage,
                ctx,
                artifact_kind,
                artifact_ref,
                decision,
                review_note,
                scope,
                workspace_scope,
                session_scope,
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            if matches!(artifact_kind, crate::types::ArtifactKind::Claim)
                && let Some(session_id) = session_scope
                && let Some(mut claim) = storage
                    .entity_get_by_id(
                        ctx,
                        session_id,
                        crate::expert_system::claim_entity_id(artifact_ref),
                    )
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
            {
                if let Some(properties) = claim.properties.as_object_mut() {
                    properties.insert("status".into(), Value::String(decision.to_string()));
                }
                claim.updated_at = Some(chrono::Utc::now());
                storage
                    .entity_put(ctx, &claim)
                    .await
                    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            }

            Ok(serde_json::json!({
                "action": "record",
                "approval": approval_json(&approval),
            }))
        }
        _ => Err((
            INVALID_PARAMS,
            format!("Unknown action: {action}. Use list/latest/record."),
        )),
    }
}

async fn handle_manage_aliases<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let action = require_str(&args, "action")?;
    let alias_name = require_str(&args, "alias_name")?;
    match action {
        "list" => {
            let aliases = storage
                .alias_list(ctx, alias_name)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            let rows: Vec<Value> = aliases.iter().map(alias_json).collect();
            Ok(serde_json::json!({
                "action": "list",
                "aliases": rows,
                "count": rows.len(),
            }))
        }
        "resolve" => {
            let workspace_scope = args.get("workspace_scope").and_then(|value| value.as_str());
            let session_scope = optional_uuid(&args, "session_scope")?;
            let alias = crate::expert_system::resolve_alias(
                storage,
                ctx,
                alias_name,
                workspace_scope,
                session_scope,
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            Ok(serde_json::json!({
                "action": "resolve",
                "alias": alias.as_ref().map(alias_json),
            }))
        }
        "put" => {
            let canonical_tool = require_str(&args, "canonical_tool")?;
            let scope_kind = crate::expert_system::parse_alias_scope_kind(
                args.get("scope_kind")
                    .and_then(|value| value.as_str())
                    .unwrap_or("global"),
            )
            .map_err(|e| (INVALID_PARAMS, e.to_string()))?;
            let session_scope = optional_uuid(&args, "session_scope")?;
            let workspace_scope = args
                .get("workspace_scope")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let scope_ref = match scope_kind {
                crate::types::AliasScopeKind::Global => args
                    .get("scope_ref")
                    .and_then(|value| value.as_str())
                    .unwrap_or("*")
                    .to_string(),
                crate::types::AliasScopeKind::Workspace => workspace_scope
                    .clone()
                    .or_else(|| {
                        args.get("scope_ref")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                    .ok_or((
                        INVALID_PARAMS,
                        "workspace alias requires workspace_scope".into(),
                    ))?,
                crate::types::AliasScopeKind::Session => session_scope
                    .map(|value| value.to_string())
                    .or_else(|| {
                        args.get("scope_ref")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                    .ok_or((
                        INVALID_PARAMS,
                        "session alias requires session_scope".into(),
                    ))?,
            };
            let status = args
                .get("status")
                .and_then(|value| value.as_str())
                .map(crate::expert_system::parse_claim_status)
                .transpose()
                .map_err(|e| (INVALID_PARAMS, e.to_string()))?
                .unwrap_or(crate::types::ClaimStatus::Proposed);
            let now = chrono::Utc::now();
            let alias = crate::types::AliasEntry {
                tenant_id: ctx.tenant_id,
                alias_id: uuid::Uuid::now_v7(),
                alias_name: alias_name.to_string(),
                scope_kind,
                scope_ref,
                canonical_tool: canonical_tool.to_string(),
                parameter_map: args
                    .get("parameter_map")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                fixed_arguments: args
                    .get("fixed_arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                args_templates: args
                    .get("args_templates")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                status,
                created_at: now,
                updated_at: now,
            };

            storage
                .alias_put(ctx, &alias)
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            storage
                .entity_put(
                    ctx,
                    &crate::expert_system::alias_mirror_entity(&alias, session_scope),
                )
                .await
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            let artifact_ref = format!("{alias_name}:{}:{}", alias.scope_kind, alias.scope_ref);
            let decision = match status {
                crate::types::ClaimStatus::Proposed => crate::types::ApprovalDecision::Proposed,
                crate::types::ClaimStatus::Approved => crate::types::ApprovalDecision::Approved,
                crate::types::ClaimStatus::Rejected => crate::types::ApprovalDecision::Rejected,
            };
            let approval = crate::expert_system::record_approval(
                storage,
                ctx,
                crate::types::ArtifactKind::Alias,
                &artifact_ref,
                decision,
                Some(format!("alias status set to {status}")),
                "alias".to_string(),
                workspace_scope.clone(),
                session_scope,
            )
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

            Ok(serde_json::json!({
                "action": "put",
                "alias": alias_json(&alias),
                "approval": approval_json(&approval),
            }))
        }
        _ => Err((
            INVALID_PARAMS,
            format!("Unknown action: {action}. Use list/put/resolve."),
        )),
    }
}

async fn handle_explain_derived<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let predicate = require_str(&args, "predicate")?;
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
    let src_id = args.get("src_id").and_then(|value| value.as_str());
    let dst_id = args.get("dst_id").and_then(|value| value.as_str());
    let limit = args
        .get("limit")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or(16)
        .clamp(1, 64);

    let start = std::time::Instant::now();
    let derived = crate::datalog::query_predicate(
        storage,
        ctx,
        session_id,
        predicate,
        &crate::config::DatalogConfig::default(),
    )
    .await
    .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    let explanations: Vec<Value> = derived
        .iter()
        .filter(|fact| src_id.is_none_or(|src| fact.src_id == src))
        .filter(|fact| dst_id.is_none_or(|dst| fact.dst_id == dst))
        .map(|fact| {
            let truncated = fact.provenance.len() > limit;
            let chain: Vec<Value> = fact
                .provenance
                .iter()
                .take(limit)
                .map(|step| {
                    serde_json::json!({
                        "parent_src": step.parent_src,
                        "parent_pred": step.parent_pred,
                        "parent_dst": step.parent_dst,
                        "parent_kind": step.parent_kind,
                    })
                })
                .collect();
            serde_json::json!({
                "predicate": fact.pred,
                "src_id": fact.src_id,
                "dst_id": fact.dst_id,
                "rule_id": fact.rule_id,
                "support_count": fact.support_count,
                "support_chain": chain,
                "approval_state": Value::Null,
                "fanout": fact.provenance.len(),
                "truncated": truncated,
            })
        })
        .collect();
    let elapsed_ms = start.elapsed().as_millis() as i64;
    let metric_predicate = format!("explain:{predicate}");
    storage
        .heat_record(ctx, &metric_predicate, false, Some(elapsed_ms))
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "predicate": predicate,
        "explanations": explanations,
        "count": explanations.len(),
        "latency_ms": elapsed_ms,
        "limit": limit,
    }))
}

async fn handle_get_effective_rule_set<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let family = args.get("family").and_then(|value| value.as_str());
    let rules = crate::datalog::load_effective_rule_entries(storage, ctx, family)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
    let rows: Vec<Value> = rules
        .iter()
        .map(|entry| {
            serde_json::json!({
                "source": match entry.source {
                    crate::datalog::RuleSource::Builtin => "builtin",
                    crate::datalog::RuleSource::Registry => "registry",
                },
                "rule_id": entry.entry.rule_id,
                "family": entry.entry.family,
                "name": entry.entry.name,
                "version": entry.entry.version,
                "state": entry.entry.state,
                "rule_body": entry.entry.rule_body,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "family": family,
        "rules": rows,
        "count": rows.len(),
    }))
}

async fn handle_promote_predicate<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let predicate = require_str(&args, "predicate")?;
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());

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

    crate::graph_write::create_typed_edge(
        storage, ctx, session_id, src_id, edge_type, dst_id, weight, metadata,
    )
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

        match crate::graph_write::create_typed_edge(
            storage, ctx, session_id, src_id, edge_type, dst_id, weight, None,
        )
        .await
        {
            Ok(_) => created += 1,
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

async fn handle_batch_update_edges<S: crate::storage::Storage>(
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

    let mut upserted: usize = 0;
    let mut unchanged: usize = 0;
    let mut errors: usize = 0;
    let mut results = Vec::with_capacity(edges.len());

    for edge_json in edges {
        let idx = results.len();
        let Some(edge_row) = edge_json.as_object() else {
            errors += 1;
            results.push(serde_json::json!({
                "index": idx,
                "status": "error",
                "reason": format!("batch_update_edges[{idx}] must be an object")
            }));
            continue;
        };

        let src_id = match edge_row
            .get("src_entity_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        {
            Some(id) => id,
            None => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": "invalid or missing src_entity_id"
                }));
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
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": "invalid or missing dst_entity_id"
                }));
                continue;
            }
        };
        let edge_type = match edge_row.get("edge_type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": "missing edge_type"
                }));
                continue;
            }
        };

        let metadata_override_set = edge_row.contains_key("metadata");
        let weight_override_set = edge_row.contains_key("weight");

        let weight_override = if weight_override_set {
            match edge_row.get("weight").and_then(|v| v.as_f64()) {
                Some(weight) => Some(weight),
                None => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": "weight must be a number"
                    }));
                    continue;
                }
            }
        } else {
            None
        };

        let metadata_override = if metadata_override_set {
            match edge_row.get("metadata").unwrap() {
                Value::String(value) => Some(value.to_string()),
                Value::Null => None,
                _ => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": "metadata must be a string"
                    }));
                    continue;
                }
            }
        } else {
            None
        };

        if weight_override_set && !weight_override.unwrap().is_finite() {
            errors += 1;
            results.push(serde_json::json!({
                "index": idx,
                "status": "error",
                "reason": "weight must be finite"
            }));
            continue;
        }

        let existing = storage
            .typed_edge_list_from(ctx, session_id, src_id)
            .await
            .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
            .into_iter()
            .find(|edge| edge.dst_id == dst_id && edge.edge_type == edge_type);

        let Some(existing_edge) = existing else {
            let final_weight = weight_override.unwrap_or(1.0);
            if !weight_override_set && !metadata_override_set {
                // No existing edge and no replacement data => we can only upsert defaults.
                // Preserve current semantics and allow this path to act as a create.
            }
            let final_metadata = metadata_override;

            match crate::graph_write::create_typed_edge(
                storage,
                ctx,
                session_id,
                src_id,
                edge_type,
                dst_id,
                final_weight,
                final_metadata,
            )
            .await
            {
                Ok(created) => {
                    upserted += 1;
                    session.dirty.store(true, Ordering::Relaxed);
                    session.last_activity.notify_waiters();
                    results.push(serde_json::json!({
                        "index": idx,
                        "status": "upserted",
                        "created_at": created.created_at.to_rfc3339(),
                        "weight": created.weight,
                    }));
                }
                Err(err) => {
                    errors += 1;
                    results.push(serde_json::json!({
                        "index": idx,
                        "status": "error",
                        "reason": err.to_string()
                    }));
                }
            }
            continue;
        };

        let final_weight = weight_override.unwrap_or(existing_edge.weight);
        let final_metadata = if metadata_override_set {
            metadata_override
        } else {
            existing_edge.metadata.clone()
        };

        let unchanged_weight = !weight_override_set || final_weight == existing_edge.weight;
        let unchanged_metadata = !metadata_override_set || final_metadata == existing_edge.metadata;
        if unchanged_weight && unchanged_metadata {
            unchanged += 1;
            results.push(serde_json::json!({
                "index": idx,
                "status": "unchanged"
            }));
            continue;
        }

        match crate::graph_write::create_typed_edge(
            storage,
            ctx,
            session_id,
            src_id,
            edge_type,
            dst_id,
            final_weight,
            final_metadata,
        )
        .await
        {
            Ok(updated) => {
                upserted += 1;
                session.dirty.store(true, Ordering::Relaxed);
                session.last_activity.notify_waiters();
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "updated",
                    "weight": updated.weight
                }));
            }
            Err(err) => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": err.to_string()
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "upserted": upserted,
        "unchanged": unchanged,
        "errors": errors,
        "total": edges.len(),
        "results": results,
    }))
}

async fn handle_batch_delete_edges<S: crate::storage::Storage>(
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

    let mut deleted = 0usize;
    let mut missing = 0usize;
    let mut invalid = 0usize;
    let mut errors = 0usize;
    let mut results = Vec::with_capacity(edges.len());

    for edge_json in edges {
        let idx = results.len();
        let Some(edge_row) = edge_json.as_object() else {
            invalid += 1;
            results.push(serde_json::json!({
                "index": idx,
                "status": "error",
                "reason": format!("batch_delete_edges[{idx}] must be an object")
            }));
            continue;
        };

        let src_id = match edge_row
            .get("src_entity_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        {
            Some(id) => id,
            None => {
                invalid += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": "invalid or missing src_entity_id"
                }));
                continue;
            }
        };
        let dst_id = match edge_row
            .get("dst_entity_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        {
            Some(id) => id,
            None => {
                invalid += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": "invalid or missing dst_entity_id"
                }));
                continue;
            }
        };
        let edge_type = match edge_row.get("edge_type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                invalid += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "status": "error",
                    "reason": "missing edge_type"
                }));
                continue;
            }
        };

        match storage
            .typed_edge_delete(ctx, session_id, src_id, edge_type, dst_id)
            .await
        {
            Ok(true) => {
                deleted += 1;
                session.dirty.store(true, Ordering::Relaxed);
                session.last_activity.notify_waiters();
                results.push(serde_json::json!({
                    "index": idx,
                    "src_id": src_id.to_string(),
                    "dst_id": dst_id.to_string(),
                    "edge_type": edge_type,
                    "status": "deleted"
                }));
            }
            Ok(false) => {
                missing += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "src_id": src_id.to_string(),
                    "dst_id": dst_id.to_string(),
                    "edge_type": edge_type,
                    "status": "not_found"
                }));
            }
            Err(err) => {
                errors += 1;
                results.push(serde_json::json!({
                    "index": idx,
                    "src_id": src_id.to_string(),
                    "dst_id": dst_id.to_string(),
                    "edge_type": edge_type,
                    "status": "error",
                    "reason": err.to_string()
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "deleted": deleted,
        "missing": missing,
        "invalid": invalid,
        "errors": errors,
        "total": edges.len(),
        "results": results,
    }))
}

async fn handle_list_derived_cache<S: crate::storage::Storage>(
    args: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize)
        .unwrap_or(100)
        .clamp(1, 500);

    let rows = storage
        .derived_cache_list_all(ctx, limit)
        .await
        .map_err(|e| (INTERNAL_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "count": rows.len(),
        "limit": limit,
        "entries": rows,
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
    let session_id = optional_uuid(&args, "session_id")?.unwrap_or(uuid::Uuid::nil());
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

/// Resolve `session_id` in tool-call arguments using the configured default.
///
/// Without this, the dispatcher only treated a *missing* `session_id` as a
/// fallback trigger — so callers passing `"default"`, `""`, `null`, or an
/// invalid UUID would either hit MCP client-side schema rejection or land in
/// the nil-UUID scope (silent wrong-session bug).
///
/// Rules:
/// - missing / null / empty / `"default"` / non-UUID string → inject the
///   configured default when one exists
/// - valid UUID string → passthrough
/// - non-UUID input AND no configured default → `INVALID_PARAMS` with a
///   message naming the field and the offending value (fail loud)
/// - missing AND no configured default → leave alone; downstream handler
///   decides whether the field was required
fn resolve_session_id(args: &mut Value, default: Option<uuid::Uuid>) -> Result<(), (i32, String)> {
    let Some(obj) = args.as_object_mut() else {
        return Ok(());
    };

    let caller_value = obj.get("session_id").cloned();
    let needs_fallback = match &caller_value {
        None => true,
        Some(Value::Null) => true,
        Some(Value::String(s)) => {
            s.is_empty() || s.eq_ignore_ascii_case("default") || uuid::Uuid::parse_str(s).is_err()
        }
        Some(_) => true,
    };

    if !needs_fallback {
        return Ok(());
    }

    // Observability: if the caller provided a non-empty value that failed to
    // parse, warn. Missing/null is the common, intentional path and stays
    // silent. Per the fail-loud rules, fallbacks must be observable.
    let caller_explicitly_bad = matches!(
        &caller_value,
        Some(Value::String(s)) if !s.is_empty()
    ) || matches!(&caller_value, Some(v) if !v.is_null() && !v.is_string());

    if let Some(sid) = default {
        if caller_explicitly_bad {
            tracing::warn!(
                provided = %caller_value.as_ref().unwrap(),
                default = %sid,
                "substituted configured default for caller-provided session_id"
            );
        }
        obj.insert("session_id".into(), Value::String(sid.to_string()));
        return Ok(());
    }

    match caller_value {
        None | Some(Value::Null) => Ok(()),
        Some(v) => Err((
            INVALID_PARAMS,
            format!(
                "session_id {} is not a valid UUID and no default session is configured; \
                 pass a valid UUID or configure server.default_session_id",
                v
            ),
        )),
    }
}

fn optional_uuid(args: &Value, field: &str) -> Result<Option<uuid::Uuid>, (i32, String)> {
    match args.get(field).and_then(|v| v.as_str()) {
        Some(s) => {
            // For optional UUID fields, treat invalid UUIDs as None rather than erroring.
            // This handles cases where callers pass non-UUID values (e.g., file paths) for
            // optional parameters - they're simply ignored with a debug log.
            match uuid::Uuid::parse_str(s) {
                Ok(u) => Ok(Some(u)),
                Err(_) => {
                    tracing::debug!(field = %field, value = %s, "optional_uuid: ignoring invalid UUID, treating as None");
                    Ok(None)
                }
            }
        }
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
    use crate::storage::Storage;
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
    async fn tools_list_returns_tier1_by_default() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let result = dispatch("tools/list", Value::Null, &store, &ctx, &session)
            .await
            .unwrap();
        let tools = result["tools"].as_array().unwrap();
        let expected_tier1 = tool_definitions(&session.entity_types)
            .iter()
            .filter(|tool| is_tier1(&tool.name))
            .count();
        assert_eq!(
            tools.len(),
            expected_tier1,
            "default tools/list should return all tier-1 tools"
        );

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // All tier-1 tools must be present
        assert!(names.contains(&"smart_ingest"));
        assert!(names.contains(&"hybrid_search"));
        assert!(names.contains(&"create_edge"));
        assert!(names.contains(&"batch_create_edges"));
        assert!(names.contains(&"batch_update_entities"));
        assert!(names.contains(&"batch_delete_entities"));
        assert!(names.contains(&"batch_update_edges"));
        assert!(names.contains(&"batch_delete_edges"));
        assert!(names.contains(&"explore_connections"));
        assert!(names.contains(&"check_intentions"));
        assert!(names.contains(&"set_intention"));
        assert!(names.contains(&"complete_intention"));
        assert!(names.contains(&"get_stats"));
        assert!(names.contains(&"count_entities_by_type"));
        assert!(names.contains(&"write_temporal_fact"));
        assert!(names.contains(&"get_temporal_chain"));
        assert!(names.contains(&"retrieve_entities"));
        assert!(names.contains(&"find_memory_chain"));
        assert!(names.contains(&"run_consolidation"));
        assert!(names.contains(&"record_outcome"));

        // Tier-2 tools must NOT be present
        assert!(!names.contains(&"check_memo_cache"));
        assert!(!names.contains(&"spread_activation"));
        assert!(!names.contains(&"recursive_explore"));
        assert!(!names.contains(&"promote_memory"));
        assert!(!names.contains(&"list_derived_cache"));
    }

    #[tokio::test]
    async fn tools_list_returns_all_with_include_all() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let params = serde_json::json!({ "include_all": true });
        let result = dispatch("tools/list", params, &store, &ctx, &session)
            .await
            .unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            tool_definitions(&session.entity_types).len(),
            "include_all should return all tools"
        );

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Check tier-1 tools still present
        assert!(names.contains(&"smart_ingest"));
        assert!(names.contains(&"hybrid_search"));
        // Check tier-2 tools now included
        assert!(names.contains(&"check_memo_cache"));
        assert!(names.contains(&"batch_ingest"));
        assert!(names.contains(&"ingest_entities"));
        assert!(names.contains(&"store_memo_result"));
        assert!(names.contains(&"write_plan_node"));
        assert!(names.contains(&"get_plan_context"));
        assert!(names.contains(&"update_plan_node"));
        assert!(names.contains(&"list_intentions"));
        assert!(names.contains(&"snooze_intention"));
        assert!(names.contains(&"promote_memory"));
        assert!(names.contains(&"demote_memory"));
        assert!(names.contains(&"importance_score"));
        assert!(names.contains(&"predict_needed"));
        assert!(names.contains(&"spread_activation"));
        assert!(names.contains(&"list_derived_cache"));
        assert!(names.contains(&"find_duplicates"));
        assert!(names.contains(&"recursive_explore"));
        assert!(names.contains(&"query_derived"));
        assert!(names.contains(&"manage_rules"));
        assert!(names.contains(&"manage_claims"));
        assert!(names.contains(&"manage_approvals"));
        assert!(names.contains(&"manage_aliases"));
        assert!(names.contains(&"explain_derived"));
        assert!(names.contains(&"get_effective_rule_set"));
        assert!(names.contains(&"promote_predicate"));
        assert!(names.contains(&"create_edge"));
        assert!(names.contains(&"batch_create_edges"));
        assert!(names.contains(&"batch_update_entities"));
        assert!(names.contains(&"batch_delete_entities"));
        assert!(names.contains(&"batch_update_edges"));
        assert!(names.contains(&"batch_delete_edges"));
        assert!(names.contains(&"count_entities_by_type"));
    }

    #[tokio::test]
    async fn batch_update_entities_updates_rows_and_reports_counts() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let update_id = Uuid::new_v4();
        let unchanged_id = Uuid::new_v4();
        let missing_id = Uuid::new_v4();

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: update_id,
            session_id: sid,
            entity_name: "Original".into(),
            entity_type: "person".into(),
            source_fold_id: Some(Uuid::new_v4()),
            context_snippet: "original context".into(),
            entity_embedding: Some(vec![0.2, 0.4]),
            confidence: 0.7,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            description: Some("old desc".into()),
            tags: vec!["old".into()],
            properties: serde_json::json!({"legacy": true}),
            ..Default::default()
        });

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: unchanged_id,
            session_id: sid,
            entity_name: "NoChange".into(),
            entity_type: "person".into(),
            context_snippet: "same".into(),
            confidence: 0.5,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "batch_update_entities",
                "arguments": {
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "entity_id": update_id.to_string(),
                            "entity_name": "Updated Name",
                            "context_snippet": "updated context",
                            "confidence": 0.9,
                            "state": "silent",
                            "description": "new description",
                            "tags": ["tag-a", "tag-b"],
                            "properties": {"score": 42},
                            "embedding": [0.9, 0.8, 0.7],
                            "source_fold_id": null
                        },
                        {
                            "entity_id": unchanged_id.to_string()
                        },
                        {
                            "entity_id": "not-a-uuid"
                        },
                        {
                            "entity_id": missing_id.to_string()
                        }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["updated"], 1);
        assert_eq!(result["unchanged"], 1);
        assert_eq!(result["not_found"], 1);
        assert_eq!(result["errors"], 1);

        let updated = store
            .entity_get_by_id(&ctx, sid, update_id)
            .await
            .unwrap()
            .expect("updated entity should exist");
        assert_eq!(updated.entity_name, "Updated Name");
        assert_eq!(updated.state, crate::types::MemoryState::Silent);
        assert_eq!(updated.description.as_deref(), Some("new description"));
        assert_eq!(updated.tags, vec!["tag-a", "tag-b"]);
        assert_eq!(updated.properties["score"], 42);
        assert_eq!(updated.source_fold_id, None);
    }

    #[tokio::test]
    async fn batch_delete_entities_hard_deletes_and_reports_counts() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let deleted_id = Uuid::new_v4();
        let already_unavailable_id = Uuid::new_v4();
        let missing_id = Uuid::new_v4();

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: deleted_id,
            session_id: sid,
            entity_name: "ToDelete".into(),
            entity_type: "person".into(),
            context_snippet: "to delete".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: already_unavailable_id,
            session_id: sid,
            entity_name: "AlreadyGone".into(),
            entity_type: "person".into(),
            context_snippet: "already gone".into(),
            confidence: 0.8,
            state: crate::types::MemoryState::Unavailable,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "batch_delete_entities",
                "arguments": {
                    "session_id": sid.to_string(),
                    "entities": [
                        { "entity_id": deleted_id.to_string() },
                        { "entity_id": already_unavailable_id.to_string() },
                        { "entity_id": missing_id.to_string() },
                        { "entity_id": "invalid" }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["deleted"], 2);
        assert_eq!(result["not_found"], 1);
        assert_eq!(result["errors"], 1);

        let after_delete = store.entity_get_by_id(&ctx, sid, deleted_id).await.unwrap();
        assert!(after_delete.is_none());
        let after_unavailable_delete = store
            .entity_get_by_id(&ctx, sid, already_unavailable_id)
            .await
            .unwrap();
        assert!(after_unavailable_delete.is_none());
    }

    #[tokio::test]
    async fn batch_update_edges_reports_upsert_and_update_statuses() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let existing_src = Uuid::new_v4();
        let existing_dst = Uuid::new_v4();
        let missing_src = Uuid::new_v4();
        let missing_dst = Uuid::new_v4();

        let _ = crate::graph_write::create_typed_edge(
            &store,
            &ctx,
            sid,
            existing_src,
            "references",
            existing_dst,
            1.5,
            Some("start".into()),
        )
        .await
        .unwrap();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "batch_update_edges",
                "arguments": {
                    "session_id": sid.to_string(),
                    "edges": [
                        {
                            "src_entity_id": existing_src.to_string(),
                            "dst_entity_id": existing_dst.to_string(),
                            "edge_type": "references",
                            "weight": 1.5,
                            "metadata": "start"
                        },
                        {
                            "src_entity_id": existing_src.to_string(),
                            "dst_entity_id": existing_dst.to_string(),
                            "edge_type": "references",
                            "weight": 2.2
                        },
                        {
                            "src_entity_id": missing_src.to_string(),
                            "dst_entity_id": missing_dst.to_string(),
                            "edge_type": "references",
                            "weight": 0.5
                        },
                        {
                            "src_entity_id": "not-a-uuid",
                            "dst_entity_id": missing_dst.to_string(),
                            "edge_type": "references"
                        }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["upserted"], 2);
        assert_eq!(result["unchanged"], 1);
        assert_eq!(result["errors"], 1);

        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["status"], "unchanged");
        assert_eq!(results[1]["status"], "updated");
        assert_eq!(results[2]["status"], "upserted");
        assert_eq!(results[3]["status"], "error");
    }

    #[tokio::test]
    async fn batch_delete_edges_hard_deletes_and_reports_missing() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let existing_src = Uuid::new_v4();
        let existing_dst = Uuid::new_v4();

        let _ = crate::graph_write::create_typed_edge(
            &store,
            &ctx,
            sid,
            existing_src,
            "references",
            existing_dst,
            1.0,
            None,
        )
        .await
        .unwrap();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "batch_delete_edges",
                "arguments": {
                    "session_id": sid.to_string(),
                    "edges": [
                        {
                            "src_entity_id": existing_src.to_string(),
                            "dst_entity_id": existing_dst.to_string(),
                            "edge_type": "references"
                        },
                        {
                            "src_entity_id": Uuid::new_v4().to_string(),
                            "dst_entity_id": Uuid::new_v4().to_string(),
                            "edge_type": "references"
                        },
                        {
                            "src_entity_id": "invalid",
                            "dst_entity_id": existing_dst.to_string(),
                            "edge_type": "references"
                        }
                    ]
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["deleted"], 1);
        assert_eq!(result["missing"], 1);
        assert_eq!(result["invalid"], 1);
        assert_eq!(result["errors"], 0);

        let remaining = store
            .typed_edge_list_from(&ctx, sid, existing_src)
            .await
            .unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn ingest_entities_upserts_entities_and_skips_duplicate_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["bug".into(), "document".into()],
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let first = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "id": a.to_string(),
                            "name": "Bug A",
                            "entity_type": "bug",
                            "context": "first bug",
                            "attrs": {"severity": "high"}
                        },
                        {
                            "id": b.to_string(),
                            "name": "Doc B",
                            "entity_type": "document",
                            "context": "supporting doc"
                        }
                    ],
                    "edges": [
                        {
                            "src_id": a.to_string(),
                            "dst_id": b.to_string(),
                            "edge_type": "references",
                            "metadata": {"source": "batch"}
                        }
                    ],
                    "options": {
                        "embed_missing": false
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let first = unwrap_tool_result(first);
        assert_eq!(first["entities"]["inserted"], 2);
        assert_eq!(first["entities"]["updated"], 0);
        assert_eq!(first["edges"]["inserted"], 1);
        assert_eq!(store.entities.lock().await.len(), 2);
        assert_eq!(store.typed_edges.lock().await.len(), 1);

        let second = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "id": a.to_string(),
                            "name": "Bug A",
                            "entity_type": "bug",
                            "context": "updated bug context",
                            "confidence": 0.5,
                            "attrs": {"severity": "critical"}
                        },
                        {
                            "id": b.to_string(),
                            "name": "Doc B",
                            "entity_type": "document",
                            "context": "supporting doc updated"
                        }
                    ],
                    "edges": [
                        {
                            "src_id": a.to_string(),
                            "dst_id": b.to_string(),
                            "edge_type": "references"
                        }
                    ],
                    "options": {
                        "embed_missing": false,
                        "on_conflict": "update"
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let second = unwrap_tool_result(second);
        assert_eq!(second["entities"]["inserted"], 0);
        assert_eq!(second["entities"]["updated"], 2);
        assert_eq!(second["edges"]["inserted"], 0);
        assert_eq!(second["edges"]["skipped_duplicate"], 1);

        let stored = store
            .entity_get_by_id(&ctx, sid, a)
            .await
            .unwrap()
            .expect("entity a should exist");
        assert_eq!(stored.context_snippet, "updated bug context");
        assert_eq!(stored.properties["severity"], "critical");
        assert_eq!(store.typed_edges.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn ingest_entities_dry_run_validates_without_writing() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["bug".into()],
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [
                        {
                            "id": a.to_string(),
                            "name": "Bug A",
                            "entity_type": "bug",
                            "context": "dry run entity"
                        },
                        {
                            "id": b.to_string(),
                            "name": "Bug B",
                            "entity_type": "bug",
                            "context": "dry run entity two"
                        }
                    ],
                    "edges": [
                        {
                            "src_id": a.to_string(),
                            "dst_id": b.to_string(),
                            "edge_type": "references"
                        }
                    ],
                    "options": {
                        "embed_missing": false,
                        "dry_run": true
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["entities"]["inserted"], 2);
        assert_eq!(result["edges"]["inserted"], 1);
        assert_eq!(store.entities.lock().await.len(), 0);
        assert_eq!(store.typed_edges.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn ingest_entities_strict_edges_fail_loudly_for_missing_endpoints() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            entity_types: vec!["bug".into()],
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        let result = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": ctx.tenant_id.to_string(),
                    "session_id": sid.to_string(),
                    "entities": [],
                    "edges": [
                        {
                            "src_id": Uuid::new_v4().to_string(),
                            "dst_id": Uuid::new_v4().to_string(),
                            "edge_type": "references"
                        }
                    ],
                    "options": {
                        "embed_missing": false,
                        "strict_edges": true
                    }
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
        let result = unwrap_tool_result(result);
        let failed = result["edges"]["failed"].as_array().unwrap();
        assert_eq!(result["edges"]["inserted"], 0);
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0]["reason"]
                .as_str()
                .unwrap()
                .contains("endpoint_not_found"),
            "expected endpoint_not_found failure, got: {}",
            failed[0]
        );
    }

    #[tokio::test]
    async fn ingest_entities_rejects_tenant_mismatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let err = dispatch(
            "tools/call",
            serde_json::json!({
                "name": "ingest_entities",
                "arguments": {
                    "tenant_id": Uuid::new_v4().to_string(),
                    "session_id": Uuid::new_v4().to_string(),
                    "entities": []
                }
            }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS);
        assert!(err.1.contains("tenant_id"));
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
        let session = SessionState {
            repo: {
                let l = std::sync::OnceLock::new();
                let _ = l.set("/test/repo".to_string());
                l
            },
            ..SessionState::default()
        };

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
    async fn explore_connections_related_entities_cql_fallback() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default(); // graph is None — uses CQL fallback
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "traversal": "related_entities",
                "entity_id": Uuid::new_v4().to_string()
            }
        });
        // Should succeed with empty results (CQL fallback, no edges in mock)
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 0);
    }

    #[tokio::test]
    async fn explore_connections_fold_requires_graph() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default(); // graph is None
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "traversal": "fold_ancestors",
                "fold_id": Uuid::new_v4().to_string()
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
            ..Default::default()
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
            ..Default::default()
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
    async fn get_stats_without_session_id_counts_default_session_entities() {
        // Regression: get_stats previously returned entity_count=0 hardcoded
        // when session_id was omitted, instead of querying the default
        // (nil) session that smart_ingest writes to when session_id is
        // omitted from its arguments. The two tools must agree on the
        // implicit session — otherwise ingested entities look like phantoms.
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        for i in 0..2 {
            let entity = crate::types::EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: Uuid::new_v4(),
                session_id: Uuid::nil(),
                entity_name: format!("DefaultSessionEntity{i}"),
                entity_type: "concept".into(),
                source_fold_id: None,
                context_snippet: format!("entity {i}"),
                entity_embedding: None,
                confidence: 0.9,
                state: Default::default(),
                created_at: chrono::Utc::now(),
                ..Default::default()
            };
            store.entities.lock().await.push(entity);
        }

        let params = serde_json::json!({
            "name": "get_stats",
            "arguments": {}
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(
            result["entity_count"], 2,
            "get_stats with no session_id should count default-session entities, not return 0"
        );
    }

    #[tokio::test]
    async fn count_entities_by_type_returns_empty_breakdowns_for_empty_session() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "count_entities_by_type",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["total"], 0);
        assert_eq!(result["by_entity_type"], serde_json::json!({}));
        assert_eq!(result["by_state"], serde_json::json!({}));
        assert_eq!(result["by_type_and_state"], serde_json::json!({}));
        assert!(result["duration_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn count_entities_by_type_buckets_known_fixture() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let mut entities = store.entities.lock().await;
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Bug A".into(),
            entity_type: "bug".into(),
            context_snippet: "a".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Bug B".into(),
            entity_type: "bug".into(),
            context_snippet: "b".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Dormant,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Doc".into(),
            entity_type: "document".into(),
            context_snippet: "c".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Fn".into(),
            entity_type: "function".into(),
            context_snippet: "d".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Fn 2".into(),
            entity_type: "function".into(),
            context_snippet: "e".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Silent,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        entities.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Fn 3".into(),
            entity_type: "function".into(),
            context_snippet: "f".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        drop(entities);

        let params = serde_json::json!({
            "name": "count_entities_by_type",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);

        assert_eq!(result["total"], 6);
        assert_eq!(result["by_entity_type"]["bug"], 2);
        assert_eq!(result["by_entity_type"]["document"], 1);
        assert_eq!(result["by_entity_type"]["function"], 3);
        assert_eq!(result["by_state"]["active"], 4);
        assert_eq!(result["by_state"]["dormant"], 1);
        assert_eq!(result["by_state"]["silent"], 1);
        assert_eq!(result["by_type_and_state"]["bug"]["active"], 1);
        assert_eq!(result["by_type_and_state"]["bug"]["dormant"], 1);
        assert_eq!(result["by_type_and_state"]["document"]["active"], 1);
        assert_eq!(result["by_type_and_state"]["function"]["active"], 2);
        assert_eq!(result["by_type_and_state"]["function"]["silent"], 1);
    }

    #[tokio::test]
    async fn count_entities_by_type_respects_tenant_isolation() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let other_ctx = crate::types::TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "other".into(),
        };
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let mut entities = store.entities.lock().await;
        entities.push(crate::types::EntityEntry {
            tenant_id: other_ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "Other Tenant Bug".into(),
            entity_type: "bug".into(),
            context_snippet: "other".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });
        drop(entities);

        let params = serde_json::json!({
            "name": "count_entities_by_type",
            "arguments": {
                "session_id": sid.to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["total"], 0);
        assert_eq!(result["by_entity_type"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn count_entities_by_type_defaults_to_nil_session_without_dirtying_session() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();

        store.entities.lock().await.push(crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            entity_name: "Nil Session Bug".into(),
            entity_type: "bug".into(),
            context_snippet: "nil".into(),
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        });

        let params = serde_json::json!({
            "name": "count_entities_by_type",
            "arguments": {}
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["total"], 1);
        assert_eq!(result["by_entity_type"]["bug"], 1);
        assert!(!session.dirty.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn intention_persists_to_storage() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            repo: {
                let l = std::sync::OnceLock::new();
                let _ = l.set("/test/repo".to_string());
                l
            },
            ..SessionState::default()
        };

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
        assert_eq!(stored[0].repo, "/test/repo");
        drop(stored);

        // Read back from storage trait (repo-scoped)
        use crate::storage::Storage as _;
        let loaded = store.intention_list(&ctx, "/test/repo").await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id.to_string(), intention_id);

        // Different repo returns empty
        let other = store.intention_list(&ctx, "/other/repo").await.unwrap();
        assert!(other.is_empty());
    }

    #[tokio::test]
    async fn intention_complete_persists_status() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            repo: {
                let l = std::sync::OnceLock::new();
                let _ = l.set("/test/repo".to_string());
                l
            },
            ..SessionState::default()
        };

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
            repo: "/test/repo".into(),
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

        // Load from storage into IntentionStore (repo-scoped)
        let loaded = store.intention_list(&ctx, "/test/repo").await.unwrap();
        let mut intention_store = IntentionStore::new();
        intention_store.load(loaded);

        // Verify the loaded intention triggers correctly
        let triggered = intention_store.check("writing rust code", "/test/repo");
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

    #[tokio::test]
    async fn hybrid_search_hint_few_results() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Seed one entity so we get 1 result (< 3)
        let entity = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: Uuid::new_v4(),
            session_id: sid,
            entity_name: "HintTest".into(),
            entity_type: "concept".into(),
            source_fold_id: None,
            context_snippet: "testing hint triggers".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        store.entities.lock().await.push(entity);

        let params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "HintTest"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        let count = result["count"].as_u64().unwrap();
        assert!((1..3).contains(&count), "expected 1-2 results, got {count}");
        assert!(
            result["_hint"]
                .as_str()
                .unwrap()
                .contains("recursive_explore"),
            "expected _hint suggesting recursive_explore"
        );
    }

    #[tokio::test]
    async fn hybrid_search_hint_no_results() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "NonexistentTopic"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 0);
        assert!(
            result["_hint"]
                .as_str()
                .unwrap()
                .contains("retrieve_entities"),
            "expected _hint suggesting retrieve_entities for zero results"
        );
    }

    #[tokio::test]
    async fn explore_connections_hint_few_results() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "traversal": "related_entities",
                "entity_id": Uuid::new_v4().to_string()
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["count"], 0);
        assert!(
            result["_hint"]
                .as_str()
                .unwrap()
                .contains("spread_activation"),
            "expected _hint suggesting spread_activation"
        );
    }

    #[tokio::test]
    async fn smart_ingest_hint_on_created() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Progressive disclosure reduces cognitive load for LLM tool selection",
                "entity_type": "concept"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["action"], "Created");
        assert!(
            result["_hint"].as_str().unwrap().contains("create_edge"),
            "expected _hint suggesting create_edge after Created"
        );
    }

    #[tokio::test]
    async fn get_stats_hint_entities_no_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Seed entities but no edges
        for i in 0..3 {
            let entity = crate::types::EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: Uuid::new_v4(),
                session_id: sid,
                entity_name: format!("Entity{i}"),
                entity_type: "concept".into(),
                source_fold_id: None,
                context_snippet: format!("entity {i}"),
                entity_embedding: None,
                confidence: 0.9,
                state: Default::default(),
                created_at: chrono::Utc::now(),
                ..Default::default()
            };
            store.entities.lock().await.push(entity);
        }

        let params = serde_json::json!({
            "name": "get_stats",
            "arguments": { "session_id": sid.to_string() }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["entity_count"].as_u64().unwrap() > 0);
        assert_eq!(result["edge_count"], 0);
        assert!(
            result["_hint"].as_str().unwrap().contains("create_edge"),
            "expected _hint suggesting create_edge when entities exist but no edges"
        );
    }

    #[tokio::test]
    async fn check_intentions_hint_on_triggered() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            repo: {
                let l = std::sync::OnceLock::new();
                let _ = l.set("/test/repo".to_string());
                l
            },
            ..SessionState::default()
        };

        // Set an intention
        let params = serde_json::json!({
            "name": "set_intention",
            "arguments": {
                "description": "Review hint implementation",
                "trigger": { "type": "Topic", "keywords": ["hint", "disclosure"] },
                "priority": "normal"
            }
        });
        dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();

        // Trigger it
        let params = serde_json::json!({
            "name": "check_intentions",
            "arguments": { "context": "implementing progressive disclosure hints" }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["triggered"].as_array().unwrap().len(), 1);
        let hint = result["_hint"].as_str().unwrap();
        assert!(
            hint.contains("complete_intention"),
            "expected _hint suggesting complete_intention"
        );
        assert!(
            hint.contains("1 intentions triggered"),
            "expected _hint to include triggered count"
        );
    }

    #[tokio::test]
    async fn record_outcome_hint_always_present() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "record_outcome",
            "arguments": {
                "session_id": sid.to_string(),
                "query_id": Uuid::new_v4().to_string(),
                "program_type": "hnsw_ann",
                "task_complexity": "simple",
                "succeeded": true,
                "latency_ms": 10,
                "token_cost": 50
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(
            result["_hint"]
                .as_str()
                .unwrap()
                .contains("feedback improves retrieval routing"),
            "expected _hint about routing improvement"
        );
    }

    #[tokio::test]
    async fn record_outcome_hint_retrieval_miss() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();

        // Note: record_outcome requires program_type to be one of the enum values.
        // "retrieval_miss" is not in the enum, so we test the regular path here
        // and validate the _hint field is present regardless.
        let params = serde_json::json!({
            "name": "record_outcome",
            "arguments": {
                "session_id": sid.to_string(),
                "query_id": Uuid::new_v4().to_string(),
                "program_type": "phonetic",
                "task_complexity": "simple",
                "succeeded": false,
                "latency_ms": 5,
                "token_cost": 20
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(
            result["_hint"].is_string(),
            "expected _hint field on record_outcome"
        );
    }

    #[tokio::test]
    async fn smart_ingest_embedding_fallback_when_ollama_unavailable() {
        // When ollama_base_url is set but Ollama is not running, smart_ingest
        // should gracefully fall back to phonetic search (no embedding).
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: "http://127.0.0.1:1".to_string(),
            ..SessionState::default()
        };
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
    }

    #[tokio::test]
    async fn smart_ingest_skips_embedding_when_ollama_empty() {
        // When ollama_base_url is empty, no embedding request is attempted
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: String::new(),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Test content for embedding skip",
                "entity_type": "concept"
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["action"], "Created");
    }

    #[tokio::test]
    async fn hybrid_search_embedding_fallback_when_ollama_unavailable() {
        // When ollama_base_url is set but unreachable, hybrid_search falls back
        // to phonetic-only search.
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState {
            ollama_base_url: "http://127.0.0.1:1".to_string(),
            ..SessionState::default()
        };
        let sid = Uuid::new_v4();

        // First ingest something
        let ingest_params = serde_json::json!({
            "name": "smart_ingest",
            "arguments": {
                "session_id": sid.to_string(),
                "content": "Ferrosa is a database engine",
                "entity_type": "concept",
                "entity_name": "Ferrosa"
            }
        });
        dispatch("tools/call", ingest_params, &store, &ctx, &session)
            .await
            .unwrap();

        // Search should still work via phonetic even without embeddings
        let search_params = serde_json::json!({
            "name": "hybrid_search",
            "arguments": {
                "session_id": sid.to_string(),
                "query": "Ferrosa"
            }
        });
        let result = dispatch("tools/call", search_params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(
            result["count"].as_u64().unwrap() > 0,
            "should find via phonetic fallback"
        );
    }

    #[tokio::test]
    async fn record_outcome_retrieval_miss_penalizes_entity_reputation() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = SessionState::default();
        let sid = Uuid::new_v4();
        let eid1 = Uuid::new_v4();
        let eid2 = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "record_outcome",
            "arguments": {
                "session_id": sid.to_string(),
                "query_id": Uuid::new_v4().to_string(),
                "program_type": "retrieval_miss",
                "task_complexity": "simple",
                "succeeded": false,
                "latency_ms": 100,
                "token_cost": 50,
                "entity_ids": [eid1.to_string(), eid2.to_string()]
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert_eq!(result["recorded"], true);

        // Both entities should have received -0.05 reputation penalty
        let w1 = store.warmth_get(&ctx, eid1).await.unwrap().unwrap();
        assert!(
            (w1.reputation - (-0.05)).abs() < f64::EPSILON,
            "expected -0.05 reputation, got {}",
            w1.reputation
        );
        let w2 = store.warmth_get(&ctx, eid2).await.unwrap().unwrap();
        assert!(
            (w2.reputation - (-0.05)).abs() < f64::EPSILON,
            "expected -0.05 reputation, got {}",
            w2.reputation
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

    // --- Session ID resolution (bug: explore_connections rejects non-UUID session_id) ---
    //
    // The dispatcher injects the configured default_session_id when the caller
    // passes a placeholder (missing, null, empty, literal "default") or an
    // invalid UUID. Callers should never silently get Uuid::nil() scope.

    #[test]
    fn resolve_session_id_injects_default_when_absent() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({});
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_injects_default_when_null() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": null });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_injects_default_when_empty_string() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": "" });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_injects_default_when_literal_default() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": "default" });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_injects_default_when_invalid_uuid() {
        let default_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": "not-a-uuid" });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], default_sid.to_string());
    }

    #[test]
    fn resolve_session_id_passes_through_valid_uuid() {
        let default_sid = Uuid::new_v4();
        let caller_sid = Uuid::new_v4();
        let mut args = serde_json::json!({ "session_id": caller_sid.to_string() });
        resolve_session_id(&mut args, Some(default_sid)).unwrap();
        assert_eq!(args["session_id"], caller_sid.to_string());
    }

    #[test]
    fn resolve_session_id_fails_loud_when_invalid_and_no_default() {
        let mut args = serde_json::json!({ "session_id": "default" });
        let err = resolve_session_id(&mut args, None).unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS);
        assert!(
            err.1.contains("session_id") && err.1.contains("default"),
            "error should name the field and bad value, got: {}",
            err.1
        );
    }

    #[test]
    fn resolve_session_id_leaves_absent_alone_when_no_default() {
        // No default and no caller value — let the handler decide whether to
        // error on missing session_id. (Some tools don't need one.)
        let mut args = serde_json::json!({});
        resolve_session_id(&mut args, None).unwrap();
        assert!(args.get("session_id").is_none());
    }

    #[test]
    fn resolve_session_id_handles_non_object_args() {
        // Defensive: some tool calls arrive with Value::Null as arguments.
        let mut args = Value::Null;
        resolve_session_id(&mut args, Some(Uuid::new_v4())).unwrap();
    }

    // Schema invariants: after the refactor-session-id-schemas sweep, only
    // delete_session (destructive) may keep `format:uuid` and `required: session_id`.
    // Every other tool must advertise session_id as a plain optional string so
    // the dispatcher's resolve_session_id fallback can fire for "default", "",
    // or omitted values.

    const STRICT_SESSION_TOOLS: &[&str] = &["delete_session"];

    #[test]
    fn tool_schemas_do_not_require_uuid_format_on_session_id() {
        let tools = tool_definitions(&["person".to_string()]);
        for tool in &tools {
            if STRICT_SESSION_TOOLS.contains(&tool.name.as_str()) {
                continue;
            }
            let sid = &tool.input_schema["properties"]["session_id"];
            if sid.is_null() {
                continue;
            }
            assert_ne!(
                sid.get("format"),
                Some(&serde_json::json!("uuid")),
                "tool {}: session_id must not carry format:uuid — blocks config fallback in strict MCP clients",
                tool.name
            );
        }
    }

    #[test]
    fn tool_schemas_do_not_list_session_id_as_required() {
        let tools = tool_definitions(&["person".to_string()]);
        for tool in &tools {
            if STRICT_SESSION_TOOLS.contains(&tool.name.as_str()) {
                continue;
            }
            let required = tool
                .input_schema
                .get("required")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            assert!(
                !required.iter().any(|v| v == "session_id"),
                "tool {}: session_id must not be in required — server falls back to default_session_id",
                tool.name
            );
        }
    }

    #[test]
    fn delete_session_schema_stays_strict() {
        let tools = tool_definitions(&["person".to_string()]);
        let tool = tools.iter().find(|t| t.name == "delete_session").unwrap();
        let sid = &tool.input_schema["properties"]["session_id"];
        assert_eq!(
            sid.get("format"),
            Some(&serde_json::json!("uuid")),
            "delete_session must keep format:uuid to prevent accidental fallback to default on typo"
        );
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v == "session_id"),
            "delete_session must keep session_id as required"
        );
    }

    #[tokio::test]
    async fn explore_connections_accepts_default_string_when_session_configured() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let default_sid = Uuid::new_v4();
        let session = SessionState {
            default_session_id: Some(default_sid),
            ..SessionState::default()
        };
        let params = serde_json::json!({
            "name": "explore_connections",
            "arguments": {
                "session_id": "default",
                "traversal": "related_entities",
                "entity_id": Uuid::new_v4().to_string(),
                "max_depth": 1
            }
        });
        let result = dispatch("tools/call", params, &store, &ctx, &session).await;
        assert!(
            result.is_ok(),
            "expected 'default' to resolve to configured session, got: {:?}",
            result
        );
    }
}
