//! Tool-schema catalog — `ToolDef` plus the per-family builders that
//! assemble the full MCP tool registry returned by `tools/list`.
//!
//! Extracted verbatim from `dispatch.rs` as a behavior-preserving split;
//! the serialized catalog is guarded by the `tool_definitions_catalog_snapshot`
//! characterization test.
//! Correctness: Correct when lazy and collected traversals serialize identically.
//! Last revised: 2026-08-12
//! Last changed: Added a family-lazy iterator for bounded catalog discovery.

use serde::ser::SerializeStruct;
use serde_json::Value;

use super::{MAX_RETRIEVAL_LIMIT, MIN_RETRIEVAL_LIMIT, short_tool_name};

/// MCP tool definition for `tools/list`.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl serde::Serialize for ToolDef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut tool = serializer.serialize_struct("ToolDef", 4)?;
        tool.serialize_field("name", &self.name)?;
        tool.serialize_field("description", &self.description)?;
        tool.serialize_field("inputSchema", &self.input_schema)?;
        tool.serialize_field("outputSchema", &tool_output_schema())?;
        tool.end()
    }
}

fn tool_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tool": {
                "type": "string",
                "description": "Canonical tool name that handled the call."
            },
            "requested_tool": {
                "type": "string",
                "description": "Tool name requested by the client, after alias resolution."
            },
            "duration_ms": {
                "type": "integer",
                "minimum": 0,
                "description": "Server-side tool execution duration in milliseconds."
            },
            "is_error": {
                "type": "boolean",
                "description": "False for successful tool results."
            }
        },
        "required": ["tool", "requested_tool", "duration_ms", "is_error"],
        "additionalProperties": true
    })
}

/// The `all_tools` catalog-expansion tool definition.
fn all_tools_def() -> ToolDef {
    ToolDef {
        name: "all_tools".into(),
        description: "Search and page through the Ferrosa Memory tool catalog when the compact default tools are not enough. Use compact discovery first, then request schema detail by exact name.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "detail": { "type": "string", "enum": ["compact", "schema"], "default": "compact" },
                "query": { "type": "string", "minLength": 1, "maxLength": 256 },
                "categories": { "type": "array", "items": { "type": "string", "minLength": 1 }, "maxItems": 16, "uniqueItems": true },
                "names": { "type": "array", "items": { "type": "string", "minLength": 1 }, "maxItems": 20, "uniqueItems": true },
                "cursor": { "type": "string", "minLength": 1, "maxLength": 2048 }
            },
            "required": []
        }),
    }
}

// --- Remote teacher/learner memory tools ---
fn remote_memory_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "feedback_record".into(),
            description: "Record terse feedback about a remote-memory candidate, classify it into a structured Packet H signal, and persist a queryable feedback explanation under the authenticated tenant.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Must match the authenticated tenant context." },
                    "remote_id": { "type": "string" },
                    "target_id": { "type": "string", "description": "Remote item/entity/candidate UUID receiving feedback." },
                    "source_namespace": { "type": "string", "minLength": 1 },
                    "scope": { "type": "string", "minLength": 1 },
                    "feedback": { "type": "string", "minLength": 1, "maxLength": 4096 }
                },
                "required": ["tenant_id", "remote_id", "target_id", "source_namespace", "scope", "feedback"]
            }),
        },
        ToolDef {
            name: "usage_mark".into(),
            description: "Mark a remote-memory item as selected, confirmed, or successful and return a scoped trust reinforcement preview. Tenant id must match authenticated context.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "remote_id": { "type": "string" },
                    "target_id": { "type": "string" },
                    "source_namespace": { "type": "string", "minLength": 1 },
                    "scope": { "type": "string", "minLength": 1 },
                    "usage": { "type": "string", "enum": ["chosen", "confirmed", "success"] }
                },
                "required": ["tenant_id", "remote_id", "target_id", "source_namespace", "scope", "usage"]
            }),
        },
        ToolDef {
            name: "trust_update".into(),
            description: "Apply scoped Packet H trust reinforcements for one remote namespace/scope and persist a not_trusted_for policy fact when repeated strong negatives cross threshold.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "remote_id": { "type": "string" },
                    "source_namespace": { "type": "string", "minLength": 1 },
                    "scope": { "type": "string", "minLength": 1 },
                    "reinforcements": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "string", "enum": ["chosen", "policy_chosen", "confirmed", "user_confirmed", "success", "wrong_scope", "strong_negative"] }
                    }
                },
                "required": ["tenant_id", "remote_id", "source_namespace", "scope", "reinforcements"]
            }),
        },
        ToolDef {
            name: "teach_query_stream".into(),
            description: "Teacher-side remote memory query stream. Returns a transport-neutral JSON event array beginning with a start event before retrieval completion; raw context/detail/skill output requires explicit grants.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "remote_id": { "type": "string", "format": "uuid" },
                    "learner_instance_id": { "type": "string", "format": "uuid" },
                    "query": { "type": "string", "maxLength": 4096 },
                    "namespaces": { "type": "array", "items": { "type": "string" } },
                    "max_items": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "query_embedding": { "type": "array", "items": { "type": "number" } },
                    "grants": { "type": "array", "items": { "type": "string", "enum": ["raw_context", "detail", "skill"] } },
                    "include_raw_context": { "type": "boolean" },
                    "include_detail": { "type": "boolean" },
                    "include_skill": { "type": "boolean" }
                },
                "required": ["remote_id", "query"]
            }),
        },
        ToolDef {
            name: "pull_preview".into(),
            description: "Learner-side remote memory pull preview. Verifies a signed teaching packet, evaluates dry-run import policy, and reports duplicate/conflict candidates without mutating local storage.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "remote_id": { "type": "string", "format": "uuid" },
                    "remote_name": { "type": "string", "maxLength": 256 },
                    "query": { "type": "string", "maxLength": 4096 },
                    "public_identity": { "type": "object", "description": "Teacher InstancePublicIdentity used to verify signed_packet" },
                    "signed_packet": { "type": "object", "description": "SignedEnvelope<TeachingPacket> from teach_query_stream or remote transport" },
                    "local_applicability": { "type": "object" },
                    "preview_ttl_seconds": { "type": "integer", "minimum": 1, "maximum": 86400 }
                },
                "required": ["remote_id", "remote_name", "query", "public_identity", "signed_packet"]
            }),
        },
        ToolDef {
            name: "pull_commit".into(),
            description: "Commit an accepted learner-side remote memory pull preview. Writes active imports with provenance, persists stubs/quarantine decisions, and records an import batch.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "preview": { "type": "object", "description": "PullPreviewPlan returned by pull_preview" },
                    "learner_decision": { "type": "object", "description": "SignedEnvelope<ImportDecisionPayload> authorizing this commit" }
                },
                "required": ["preview", "learner_decision"]
            }),
        },
        ToolDef {
            name: "remote_list".into(),
            description: "List tenant-scoped configured remote memory providers without exposing credentials.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                }
            }),
        },
        ToolDef {
            name: "remote_add".into(),
            description: "Register or replace a tenant-scoped remote memory endpoint, trust class, instance id, and public-key fingerprint. Endpoints must be HTTPS/HTTP URLs; secrets are not accepted.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "format": "uuid" },
                    "remote_id": { "type": "string", "format": "uuid" },
                    "instance_id": { "type": "string", "format": "uuid" },
                    "name": { "type": "string", "minLength": 1, "maxLength": 256 },
                    "endpoint": { "type": "string", "description": "HTTP(S) endpoint for the remote MCP server; do not include credentials." },
                    "trust_class": { "type": "string", "enum": ["personal", "team", "partner", "public", "archive"] },
                    "public_key_fingerprint": { "type": "string", "minLength": 1, "maxLength": 256 }
                },
                "required": ["tenant_id", "instance_id", "name", "endpoint", "trust_class", "public_key_fingerprint"]
            }),
        },
        ToolDef {
            name: "remote_update_policy".into(),
            description: "Append tenant-scoped Datalog policy facts for a configured remote. Supported actions: read, detail_fetch, autocommit, requires_activation, should_consult, trusted_for, not_trusted_for, fallback_enabled.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "format": "uuid" },
                    "remote_id": { "type": "string", "format": "uuid" },
                    "facts": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["grant", "deny"] },
                                "namespace": { "type": "string", "minLength": 1 },
                                "action": { "type": "string", "minLength": 1 },
                                "expires_at": { "type": "string", "format": "date-time" }
                            },
                            "required": ["kind", "namespace", "action"]
                        }
                    }
                },
                "required": ["tenant_id", "remote_id", "facts"]
            }),
        },
        ToolDef {
            name: "remote_remove".into(),
            description: "Disable a configured remote while preserving import provenance and policy audit rows.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "format": "uuid" },
                    "remote_id": { "type": "string", "format": "uuid" }
                },
                "required": ["tenant_id", "remote_id"]
            }),
        },
        ToolDef {
            name: "remote_health".into(),
            description: "Report local configuration health for one remote without dialing or leaking credentials.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "remote_id": { "type": "string", "format": "uuid" } },
                "required": ["remote_id"]
            }),
        },
        ToolDef {
            name: "remote_capabilities".into(),
            description: "Return the remote-memory MCP capabilities expected for a configured remote.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "remote_id": { "type": "string", "format": "uuid" } },
                "required": ["remote_id"]
            }),
        },
        ToolDef {
            name: "remote_detail".into(),
            description: "Return configured remote-memory details plus the transport/security capabilities required by remote pull smokes.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "remote_id": { "type": "string", "format": "uuid" } },
                "required": ["remote_id"]
            }),
        },
        ToolDef {
            name: "remote_explain_policy".into(),
            description: "Evaluate and explain Datalog-backed remote policy for a configured remote, action, and namespace.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "remote_id": { "type": "string", "format": "uuid" },
                    "action": { "type": "string", "enum": ["read", "detail_fetch", "autocommit", "requires_activation", "should_consult"] },
                    "namespace": { "type": "string", "minLength": 1 }
                },
                "required": ["remote_id", "action", "namespace"]
            }),
        },
    ]
}

fn session_continuity_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "ingest_context_segments".into(),
            description: "Persist raw pre-compaction conversation context as deterministic semantic segments, with Nomic embeddings when configured and temporal prev/next links for later expansion.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "conversation_id": { "type": "string", "maxLength": 512 },
                    "messages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": { "type": "string" },
                                "content": { "type": "string", "maxLength": 131072 },
                                "turn_index": { "type": "integer" },
                                "created_at": { "type": "string", "description": "Optional RFC3339 timestamp" },
                                "metadata": { "type": "object" }
                            },
                            "required": ["role", "content", "turn_index"]
                        },
                        "minItems": 1
                    },
                    "segmentation": { "type": "object" },
                    "embed_missing": { "type": "boolean" }
                },
                "required": ["conversation_id", "messages"]
            }),
        },
        ToolDef {
            name: "search_context_segments".into(),
            description: "Hybrid-search raw context segments with lexical BM25 fallback plus Nomic vector ANN, optionally returning bounded prev/next temporal expansion windows.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "query": { "type": "string", "maxLength": 4096 },
                    "query_embedding": { "type": "array", "items": { "type": "number" } },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "expand": {
                        "type": "object",
                        "properties": {
                            "prev": { "type": "integer", "minimum": 0, "maximum": 10 },
                            "next": { "type": "integer", "minimum": 0, "maximum": 10 },
                            "max_tokens": { "type": "integer", "minimum": 1, "maximum": 50000 }
                        }
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "get_context_window".into(),
            description: "Return ordered previous/hit/next context segment pages around a retrieved segment using temporal edges, bounded by token budget.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "segment_id": { "type": "string", "format": "uuid" },
                    "prev": { "type": "integer", "minimum": 0, "maximum": 20 },
                    "next": { "type": "integer", "minimum": 0, "maximum": 20 },
                    "max_tokens": { "type": "integer", "minimum": 1, "maximum": 100000 }
                },
                "required": ["segment_id"]
            }),
        },
        ToolDef {
            name: "get_turn_chain".into(),
            description: "Walk the next_turn temporal edge chain from a starting turn entity, returning turns in forward (chronological arrival) order.\n\nCALL WHEN: You need to reconstruct what happened in an agent session after a known turn, follow a conversation thread, or inspect the sequence of turns the harness hook captured.\nRETURNS: ordered list of turn entities from start_turn_id forward, up to limit turns.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session partition for the turn chain. Omit or pass \"default\" to use the configured default session." },
                    "start_turn_id": { "type": "string", "format": "uuid", "description": "Entity ID of the first turn to include" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 20, "description": "Maximum number of turns to return" }
                },
                "required": ["start_turn_id"]
            }),
        },
        ToolDef {
            name: "get_chunk_context".into(),
            description: "Expand a retrieved document chunk through semantic prev/next links. Use after search returns a document_chunk hit whose answer may sit in adjacent chunks or split list items.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "chunk_id": { "type": "string", "format": "uuid" },
                    "prev": { "type": "integer", "minimum": 0, "maximum": 10 },
                    "next": { "type": "integer", "minimum": 0, "maximum": 10 },
                    "max_tokens": { "type": "integer", "minimum": 1, "maximum": 50000 }
                },
                "required": ["chunk_id"]
            }),
        },
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
        ToolDef {
            name: "session_task_put".into(),
            description: "Creates or upserts a durable fmem-owned session task. If task_id is omitted, fmem generates the canonical id. Use aliases only as scoped client-visible references.\n\nCALL WHEN: Starting work, updating visible work-item metadata, or switching focus to a new task. Prefer this over plan tools for current-task continuity across compaction.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid", "description": "Optional canonical id returned by fmem for updates; omit on create." },
                    "title": { "type": "string", "maxLength": 512 },
                    "description": { "type": "string", "maxLength": 8192 },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "blocked", "completed", "cancelled", "superseded"] },
                    "priority": { "type": "integer", "minimum": 0, "maximum": 1000 },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "parent_task_id": { "type": "string", "format": "uuid" },
                    "alias_scope": { "type": "string", "maxLength": 256 },
                    "alias": { "type": "string", "maxLength": 256 },
                    "focus": { "type": "boolean", "description": "Default true. Pushes current focus down the stack and focuses this task." },
                    "client_agent": { "type": "string" },
                    "workspace": { "type": "string" },
                    "thread_id": { "type": "string" },
                    "external_session_id": { "type": "string" }
                },
                "required": ["title"]
            }),
        },
        ToolDef {
            name: "session_task_get".into(),
            description: "Reads a durable session task by canonical task_id, or by scoped alias when task_id is omitted.\n\nCALL WHEN: Rehydrating task detail after compaction or resolving a client-visible work-item alias.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid" },
                    "alias_scope": { "type": "string" },
                    "alias": { "type": "string" }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "session_task_current".into(),
            description: "Returns the deterministic current-task snapshot: foreground task, active working set, focus stack, and recovery hints.\n\nCALL WHEN: Session starts, after compaction, before writing if the agent may be lost, or before deciding to plan more work.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "session_id": { "type": "string" } },
                "required": []
            }),
        },
        ToolDef {
            name: "session_task_list".into(),
            description: "Lists durable session tasks, optionally filtered by lifecycle status. Returns focus/priority sorted tasks.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "blocked", "completed", "cancelled", "superseded"] }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "session_task_complete".into(),
            description: "Marks a task completed without hard delete. If a suspended task is on the focus stack, returns a resume candidate and action according to policy.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid" },
                    "outcome_summary": { "type": "string", "maxLength": 4096 }
                },
                "required": ["task_id"]
            }),
        },
        ToolDef {
            name: "session_task_cancel".into(),
            description: "Marks a task cancelled without hard delete and updates focus stack recovery state.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid" },
                    "outcome_summary": { "type": "string", "maxLength": 4096 }
                },
                "required": ["task_id"]
            }),
        },
        ToolDef {
            name: "session_task_focus".into(),
            description: "Moves an existing non-terminal task to foreground and pushes the previous foreground down the focus stack.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "task_id": { "type": "string", "format": "uuid" },
                    "reason": { "type": "string", "maxLength": 512 }
                },
                "required": ["task_id"]
            }),
        },
        ToolDef {
            name: "session_task_observe".into(),
            description: "Deterministic v1 observation hook for clients/hook code. Handles explicit task-shift, completion, and lost-agent signals; returns actions and hints without requiring an LLM judge.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "event_type": { "type": "string", "enum": ["user_requested_new_task", "user_requested_switch", "task_completed", "agent_lost", "context_reset"] },
                    "task_id": { "type": "string", "format": "uuid" },
                    "title": { "type": "string" },
                    "payload": { "type": "object" }
                },
                "required": ["event_type"]
            }),
        },
    ]
}

// --- Fold tools (Sprint 2) ---
fn fold_tools() -> Vec<ToolDef> {
    vec![
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
                    "k": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "include_raw": { "type": "boolean" }
                },
                "required": ["query_embedding"]
            }),
        },
    ]
}

// --- Entity tools (Sprint 3) ---
fn entity_tools(entity_type_enum: &Value) -> Vec<ToolDef> {
    vec![
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
                    "k": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "list_entities".into(),
            description: "List entities with structured equality predicates over entity fields and properties. Use this for kanban/task-style queries such as all task entities with status=ready and assignee=claude; use hybrid_search for semantic recall.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID to target only with scope=session. Supplying it alone does not change the tenant-wide default." },
                    "entity_type": { "type": "string", "description": "Optional entity_type filter, e.g. task" },
                    "filters": {
                        "type": "object",
                        "description": "Equality predicates. Known entity fields include entity_id/id, session_id, entity_name/name, entity_type, state, scope, tags, content_hash, confidence. Other keys match properties.<key>."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["session", "global", "both", "all"],
                        "description": "session=the supplied session_id; global=tenant global plus legacy nil session; both=session+global; all=tenant-wide scan. Default all; session_id alone never changes scope."
                    },
                    "include_cross_session": {
                        "type": "boolean",
                        "description": "Compatibility flag. true is equivalent to scope=all; false is equivalent to scope=session when scope is omitted and requires session_id."
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Max results to return (default 50)" }
                },
                "required": []
            }),
        },
    ]
}

// --- Feedback tool (Sprint 3) ---
fn feedback_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "record_outcome".into(),
            description: "Records the result of a retrieval operation for offline routing improvement.\n\nCALL WHEN: After every retrieval operation (retrieve_fold_context, retrieve_entities, check_memo_cache). Provide program_type, task_complexity, succeeded, latency_ms, token_cost.\nThis is write-only (~1ms). No effect on current task but improves routing for future sessions.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "query_id": { "type": "string", "format": "uuid" },
                    "program_type": { "type": "string", "enum": ["hnsw_ann", "phonetic", "cypher_hop", "btree_range", "memo_hit", "hybrid_search", "hybrid_search_auto", "workspace", "retrieval_miss"] },
                    "task_complexity": { "type": "string", "enum": ["simple", "linear", "quadratic"] },
                    "succeeded": { "type": "boolean" },
                    "latency_ms": { "type": "integer", "minimum": 0 },
                    "token_cost": { "type": "integer", "minimum": 0 },
                    "entity_ids": { "type": "array", "items": { "type": "string", "format": "uuid" }, "description": "Entity IDs this outcome applies to. Success → warmth/workspace boost. Failure → warmth/workspace penalty." },
                    "cwd": { "type": "string", "maxLength": 1024, "description": "Working directory where the retrieval was evaluated. Enables workspace-specific reranking feedback." },
                    "retrieval_sources": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional retrieval mechanisms/sources involved, e.g. entity_phonetic, entity_ann, workspace."
                    }
                },
                "required": ["query_id", "program_type", "task_complexity", "succeeded", "latency_ms", "token_cost"]
            }),
        },
        ToolDef {
            name: "record_feedback".into(),
            description: "Records feedback on the most recent hybrid_search result set for this session.\n\nCALL WHEN: Retrieved memories were clearly helpful, irrelevant, wrong, or impossible to judge for the current working directory. Cheapest form: pass scores in last-result order, where 1=helpful, -1=irrelevant/wrong, 0=neutral, and \"-\"=judge abstained/failed. Include cwd so future searches in the same directory are reranked dynamically.\nCost: ~5ms + small entity property updates.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "relevant": { "type": "boolean", "description": "Fallback when scores is omitted: apply one relevance label to all last results." },
                    "scores": {
                        "type": "array",
                        "items": {},
                        "description": "Per-result feedback in last retrieval order. 1=helpful, -1=irrelevant/wrong, 0=neutral, \"-\" or null=judge abstained/failed."
                    },
                    "judge": { "type": "string", "maxLength": 64, "description": "Who made this judgment, e.g. caller_llm, human, judge_model. Scores from multiple judges are summed; abstentions are tracked separately." },
                    "cwd": { "type": "string", "maxLength": 1024 },
                    "reason": { "type": "string", "maxLength": 1024 },
                    "entity_ids": {
                        "type": "array",
                        "items": { "type": "string", "format": "uuid" },
                        "description": "Optional subset of last results to score. Omit to score all entity results from the last retrieval."
                    }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "configure".into(),
            description: "Read or update compact runtime defaults. Session-start hooks call this to let fmem create and store the active session_id; retrieval_limit controls default search result count.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_start": {
                        "oneOf": [
                            { "type": "boolean" },
                            {
                                "type": "object",
                                "properties": {
                                    "agent": { "type": "string", "maxLength": 128 },
                                    "agent_session_id": { "type": "string", "maxLength": 512 },
                                    "external_session_id": { "type": "string", "maxLength": 512 },
                                    "thread_id": { "type": "string", "maxLength": 512 },
                                    "workspace": { "type": "string", "maxLength": 2048 },
                                    "cwd": { "type": "string", "maxLength": 2048 }
                                }
                            }
                        ],
                        "description": "Set by a deterministic agent SessionStart hook. fmem creates/stores the active session_id from this metadata."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional explicit fmem UUID to install as the active runtime session. Hooks normally omit this."
                    },
                    "agent": { "type": "string", "maxLength": 128 },
                    "agent_session_id": { "type": "string", "maxLength": 512 },
                    "external_session_id": { "type": "string", "maxLength": 512 },
                    "thread_id": { "type": "string", "maxLength": 512 },
                    "workspace": { "type": "string", "maxLength": 2048 },
                    "cwd": { "type": "string", "maxLength": 2048 },
                    "retrieval_limit": {
                        "type": "integer",
                        "minimum": MIN_RETRIEVAL_LIMIT,
                        "maximum": MAX_RETRIEVAL_LIMIT,
                        "description": "Default ranked results returned by retrieval tools when k/limit is omitted."
                    },
                    "default_limit": {
                        "type": "integer",
                        "minimum": MIN_RETRIEVAL_LIMIT,
                        "maximum": MAX_RETRIEVAL_LIMIT,
                        "description": "Alias for retrieval_limit."
                    },
                    "debug_stop": {
                        "type": "boolean",
                        "description": "Hidden dev toggle. When true, tool responses carry a degraded-cluster alert (and fail on critical degradation) so you STOP and investigate instead of building on a broken cluster. Leave unset in normal use."
                    }
                },
                "required": []
            }),
        },
    ]
}

// --- Session lifecycle ---
fn session_lifecycle_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Cognitive memory tools ---
fn cognitive_memory_tools(entity_type_enum: &Value) -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "smart_ingest".into(),
            description: "YOUR PRIMARY TOOL FOR BUILDING LONG-TERM MEMORY. Automatically decides whether to CREATE, UPDATE, SUPERSEDE, or SKIP based on what you already know.\n\nCALL AGGRESSIVELY — every time you encounter something worth remembering:\n- User preferences, habits, or working style\n- Technical decisions and WHY they were made\n- Architecture patterns, library choices, configuration gotchas\n- People, roles, relationships mentioned in conversation\n- Project context: goals, constraints, deadlines, blockers\n- Debugging insights: what caused a bug, what fixed it\n- Tool/framework knowledge: 'X works well for Y', 'avoid Z because...'\n- Domain knowledge: business rules, API behaviors, data models\n- Corrections: 'user said X is wrong, Y is correct'\n\nDO NOT CALL for: ephemeral task state (use plan tools), raw code (derivable from files), or content the user explicitly marks as temporary.\n\nThe prediction error gate handles dedup — calling too often is better than missing important information. If in doubt, ingest it.\n\nRETURNS: action taken (Created/Updated/Superseded/Skipped) + entity_id.\nCost: ~15ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Caller session UUID retained as ingest provenance. It does not move an existing entity or override its type-defined storage scope." },
                    "content": { "type": "string", "maxLength": 8192, "description": "The content to ingest" },
                    "entity_type": { "type": "string", "enum": entity_type_enum },
                    "entity_name": { "type": "string", "maxLength": 256, "description": "Clean entity name (e.g. 'Ben Kearns', 'Ferrosa'). If omitted, extracted automatically from content via LLM or heuristic." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding vector" },
                    "source_fold_id": { "type": "string", "format": "uuid", "description": "Optional: UUID of the fold (conversation thread) that produced this content. Omit or pass null if not in a fold context. DO NOT pass a file path — this field expects a fold UUID from start_fold, or null." }
                },
                "required": ["content", "entity_type"]
            }),
        },
    ]
}

// --- Skills layer ---
fn skills_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Intention tools (prospective memory, repo-scoped) ---
fn intention_tools() -> Vec<ToolDef> {
    vec![
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
            name: "set_foresight".into(),
            description: "Time-bounded memory — declare a planned-future fact or temporary constraint with a validity window. Search surfaces it ONLY while valid at the current time; expired and not-yet-active facts are filtered out automatically, so stale deadlines never pollute context.\n\nCALL WHEN a fact only holds for a window:\n- 'Code freeze until 2026-07-01' (valid_until)\n- 'Migration plan goes live on 2026-06-30' (valid_from)\n- 'API v1 is deprecated as of today' (valid_until open-ended past the cutover)\n- 'Use the staging cluster this week' (valid_from + valid_until)\n\nvalid_from/valid_until are optional RFC3339 timestamps; omit either for an open-ended bound. Cost: ~1ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "maxLength": 4096, "description": "The time-bounded fact or constraint" },
                    "valid_from": { "type": "string", "description": "RFC3339 timestamp; the fact becomes active at this time (optional — omit for 'active now')" },
                    "valid_until": { "type": "string", "description": "RFC3339 timestamp; the fact expires at this time (optional — omit for 'no expiry')" },
                    "session_id": { "type": "string", "description": "Session UUID to scope the fact to (defaults to the current session)" }
                },
                "required": ["content"]
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
    ]
}

// --- Temporal fact tools ---
fn temporal_fact_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "write_temporal_fact".into(),
            description: "Records a timestamped fact about an entity. Auto-supersedes the previous fact, preserving history.\n\nCALL WHEN facts change over time — this is how you track evolution:\n- Role changes: 'Alice is now VP' supersedes 'Alice is Director'\n- Status updates: 'deploy succeeded' supersedes 'deploy in progress'\n- Project state: 'using Rust 1.82' supersedes 'using Rust 1.78'\n- Preference changes: 'user prefers dark mode' supersedes 'user likes light mode'\n- Bug status: 'fixed in commit abc' supersedes 'investigating OOM'\n\nFirst call ingest to create the entity, then write_temporal_fact for facts that evolve. The supersession chain is queryable — you can answer 'what was X before?'\n\nReturns: event_id of the new fact.\nCost: ~5ms.".into(),
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
    ]
}

// --- Graph traversal tool ---
fn graph_traversal_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Hybrid search ---
fn hybrid_search_tools() -> Vec<ToolDef> {
    vec![
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
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Max results to return (default: 10)" },
                    "offset": { "type": "integer", "minimum": 0, "maximum": 49, "description": "Skip this many fused results for pagination. Use offset=5 after scoring the first 5 as irrelevant." },
                    "candidate_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Per-source candidate fanout before fusion. Defaults to min(limit*2, 50); lower it to reduce retrieval work."
                    },
                    "min_score": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Drop fused results below this score before returning them. Useful for hooks where silence is better than weak recall."
                    },
                    "memory_kinds": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["episodic", "procedural", "semantic"] },
                        "description": "Optional result category filter applied before return."
                    },
                    "datalog_frontier": {
                        "type": "boolean",
                        "description": "Enable bounded Datalog-style graph frontier expansion from entity seeds. Default true when the fusion profile includes datalog_frontier."
                    },
                    "datalog_frontier_seed_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Maximum entity seeds to expand from initial candidates. Defaults to candidate source limit."
                    },
                    "datalog_frontier_edge_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Maximum typed edges considered per frontier node. Defaults to 12."
                    },
                    "datalog_frontier_max_hops": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 3,
                        "description": "Maximum graph hops for inferred recall. Defaults to 2."
                    },
                    "datalog_frontier_min_confidence": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Suppress derived frontier candidates below this edge/derived confidence. Defaults to 0.30."
                    },
                    "fusion_profile": {
                        "type": "string",
                        "enum": ["auto", "default", "all", "bm25-only", "semantic-only", "bm25-semantic", "bm25-semantic-workspace", "bm25-semantic-phonetic", "bm25-semantic-phonetic-workspace"],
                        "description": "Named source-weight profile. Defaults to auto, which cheaply routes query intent to a fast effective profile. Use explicit profiles for deterministic ablations; use all/phonetic profiles for recall-heavy runs."
                    },
                    "fusion_weights": {
                        "type": "object",
                        "description": "Optional numeric source weight overrides, e.g. {\"document_bm25\":2.5,\"document_ann\":1.5,\"document_phonetic\":0}."
                    },
                    "query_decomposition": {
                        "type": "string",
                        "enum": ["none", "heuristic", "llm"],
                        "description": "Generate bounded query variants and RRF-union their candidate sets before reranking. llm uses the configured judge model. Default none."
                    },
                    "query_task": {
                        "type": "string",
                        "enum": ["general", "bright_pro", "memorybench"],
                        "description": "Task hint for query decomposition prompt shaping. Default general."
                    },
                    "query_variants": {
                        "type": "array",
                        "maxItems": 8,
                        "items": {"type": "string", "maxLength": 2048},
                        "description": "Caller-provided extra query variants to union with the primary query."
                    },
                    "query_variant_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 8,
                        "description": "Maximum total query variants, including the original query. Default 5."
                    },
                    "query_embed_variants": {
                        "type": "boolean",
                        "description": "When true and an embedding provider is configured, embed each query variant separately. Default false."
                    },
                    "chunk_expansion": {
                        "type": "string",
                        "enum": ["none", "neighbors"],
                        "description": "Expand document_chunk hits before reranking/returning. neighbors adds bounded prev/next chunks."
                    },
                    "chunk_prev": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 5,
                        "description": "Previous document chunks to include when chunk_expansion=neighbors."
                    },
                    "chunk_next": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 5,
                        "description": "Next document chunks to include when chunk_expansion=neighbors."
                    },
                    "chunk_max_tokens": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 8000,
                        "description": "Approximate added-token budget per result for chunk expansion."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["session", "global", "both"],
                        "description": "session=current session only; global=tenant global plus legacy nil session; both=session+global. Default both, so curated global/skill corpus is retrievable; pass session to restrict to the current session."
                    },
                    "include_cross_session": {
                        "type": "boolean",
                        "description": "Compatibility flag, overridden by an explicit scope. When scope is omitted: the default already spans session+global; pass false to restrict to the current session, true to force both."
                    },
                    "cwd": {
                        "type": "string",
                        "maxLength": 1024,
                        "description": "Current agent working directory. Results learned in the same directory tree receive a bounded reranking boost."
                    },
                    "workspace_cwd": {
                        "type": "string",
                        "maxLength": 1024,
                        "description": "Alias for cwd; explicit workspace path used for reranking affinity."
                    },
                    "rerank": {
                        "type": "boolean",
                        "description": "Override live LLM reranking for this call. Defaults to [judge].enabled."
                    },
                    "rerank_candidates": {
                        "type": "integer",
                        "minimum": 2,
                        "maximum": 50,
                        "description": "Override how many top candidates the judge reranker sees. Keep small for token economy; evals may use 25."
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "manage_authority".into(),
            description: "Set user-managed authority for retrieved memories. Use this to mark curated corpus chunks, skills, or other memory IDs as high reputation/PageRank, or to demote known clutter. Authority is applied to future hybrid_search ranking after normal relevance scoring.\n\nCALL WHEN: The user explicitly says a result/source is curated, authoritative, trusted, or noisy. Prefer global scope for curated corpus/skills and session scope for local one-off preferences.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "target_id": { "type": "string", "description": "Memory result ID to update." },
                    "target_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Multiple memory result IDs to update with the same authority values."
                    },
                    "reputation": {
                        "type": "number",
                        "minimum": -1.0,
                        "maximum": 1.0,
                        "description": "User-managed trust score. 1.0=curated/highest trust, 0=neutral, -1.0=known clutter."
                    },
                    "pagerank": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Authority/PageRank seed. 1.0 strongly boosts authoritative curated material."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["session", "global"],
                        "description": "Where to store this authority. global applies to tenant-global searches; session applies to the current session. Default session unless global=true."
                    },
                    "global": {
                        "type": "boolean",
                        "description": "Compatibility shortcut for scope=global."
                    },
                    "reason": { "type": "string", "maxLength": 2048 }
                }
            }),
        },
    ]
}

// --- Dream consolidation ---
fn dream_consolidation_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "run_consolidation".into(),
            description: "Dream consolidation — discovers hidden connections between memories. Groups entities by shared context, creates CO_OCCURS graph edges, identifies clusters.\n\nCALL WHEN:\n- At the end of a productive work session\n- When the user says 'wrap up' or 'that's it for now'\n- When you want to force background consolidation for the current session\n\nSmart ingest automatically queues consolidation after enough new entities; you do not need to count memories manually.\nCost: request path only queues work; the background worker does the consolidation.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": []
            }),
        },
    ]
}

// --- Enrichment pipeline ---
fn enrichment_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Stats tool ---
fn stats_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "get_stats".into(),
            description: "Returns memory system statistics. Entity/node and edge counts are tenant-wide by default. Use scope=session with session_id to scope both counts to one session.\n\nCALL WHEN: For health monitoring, debugging, or when the user asks about memory usage.\nCost: ~5ms (runs count queries).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID required when scope=session. Supplying it alone does not change the tenant-wide default." },
                    "scope": { "type": "string", "enum": ["tenant", "session"], "description": "tenant is the default; session scopes all counts to the supplied session_id." }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "memory_metrics".into(),
            description: "Returns a compact tenant-wide memory size report: total node/edge counts plus node and edge buckets, including legacy nil-session knowledge in the tenant totals.\n\nCALL WHEN: A user asks how much knowledge is stored, how many nodes/edges memory has, or whether database-backed memory has outgrown flat files.\nCost: ~10-100ms (tenant-scoped count queries).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDef {
            name: "migration_status".into(),
            description: "Returns read-only schema migration status for the connected memory database: db_version, binary_version, pending versions, and last applied timestamp.\n\nCALL WHEN: Startup logs or graph writes suggest schema drift, or an operator asks whether the database schema is current.\nCost: ~5ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDef {
            name: "describe".into(),
            description: "Read-only, management-safe self-description of this ferrosa-memory server (contract ferrosa-memory.system.describe.v1): identity, runtime health, redacted effective config, dependent-store health, live ferrosa cluster info (queried from the CQL system tables), summary memory statistics, schema drift, binary/release state, capabilities, and allowed management actions.\n\nCALL WHEN: A management client (e.g. Ferrosa Workbench) discovers or is pointed at this endpoint and needs the authoritative cluster descriptor instead of inferring it from local files. Secrets are never returned; their key paths appear under configuration.redactedKeys.\nCost: ~10-3000ms (bounded dependency probes).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "include": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": [
                                "identity", "runtime", "configuration", "stores",
                                "schema", "statistics", "binaries", "harnesses",
                                "capabilities", "managementActions"
                            ]
                        },
                        "description": "Optional list of sections to return. Omit for all sections."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session to scope the statistics section to (defaults to the nil session)."
                    },
                    "redaction": {
                        "type": "string",
                        "enum": ["management-safe"],
                        "description": "Redaction mode. Only management-safe is supported; secrets are always redacted."
                    },
                    "caller": {
                        "type": "object",
                        "description": "Optional calling client identity, logged for diagnostics.",
                        "properties": {
                            "name": { "type": "string" },
                            "version": { "type": "string" }
                        }
                    }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "forget".into(),
            description: "Candidate-confirmed forgetting, in two phases. PROPOSE (pass `query`, no token): searches memory across sessions for candidates matching the intent, returns each candidate's blast radius (edges/temporal/derived it references) plus a signed `forget_token` — mutates nothing. CONFIRM (pass `forget_token` + `selected_ids` + `confirm: true`): forgets only the approved ids. Defaults to reversible RETRACT (excluded from recall, audited, restorable via restore_forgotten for `retract_purge_days`); pass `mode: \"hard\"` for permanent deletion. Never forgets without explicit confirmation; skips any candidate that changed since proposal.\n\nCALL WHEN: the user asks to forget/remove specific memories. Always propose first, show the candidates, and only confirm the ids the user approves — never pass confirm:true on the user's behalf.\nCost: propose ~20ms + search; confirm ~10-50ms per item.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Propose phase: natural-language description of what to forget." },
                    "scope": { "type": "array", "items": { "type": "string" }, "description": "Optional candidate filters (entity types, etc.)." },
                    "session_id": { "type": "string" },
                    "limit": { "type": "integer", "description": "Max candidates to propose." },
                    "forget_token": { "type": "string", "description": "Confirm phase: the token returned by a prior propose call." },
                    "selected_ids": { "type": "array", "items": { "type": "string", "format": "uuid" }, "description": "Confirm phase: the candidate ids the user approved." },
                    "mode": { "type": "string", "enum": ["retract", "hard"], "description": "retract (default, reversible) or hard (permanent)." },
                    "acknowledge_high_impact": { "type": "boolean", "description": "Required to forget a high-impact (highly-connected) candidate." },
                    "reason": { "type": "string" },
                    "confirm": { "type": "boolean", "description": "Must be true (with forget_token) to execute the forget." }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "restore_forgotten".into(),
            description: "Reverse a retraction: restore a soft-forgotten entity to its prior memory state so it is recalled again. Works only for retract-mode forgets that have not yet been purged (within retract_purge_days); hard deletes are irreversible. Note: edges removed at forget time are not auto-recreated in v1.\n\nCALL WHEN: the user wants to undo a forget / bring back a retracted memory.\nCost: ~10ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "entity_id": { "type": "string", "format": "uuid", "description": "The entity to restore." }
                },
                "required": ["entity_id"]
            }),
        },
        ToolDef {
            name: "count_entities_by_type".into(),
            description: "Return an entity histogram broken down by entity_type, by state, and by the joint (type,state) buckets. Counts are tenant-wide by default. Use scope=session with session_id to scope the histogram to one session.\n\nCALL WHEN: You need status/diagnostic counts like 'how many bugs are active in this session?' without coupling the client to entity_store columns.\nCost: ~5-10ms.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session UUID required when scope=session. Supplying it alone does not change the tenant-wide default." },
                    "scope": { "type": "string", "enum": ["tenant", "session"], "description": "tenant is the default; session scopes the histogram to the supplied session_id." }
                },
                "required": []
            }),
        },
    ]
}

// --- Memory state management ---
fn memory_state_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Importance scoring ---
fn importance_scoring_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Memory chains ---
fn memory_chain_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Speculative retrieval ---
fn speculative_retrieval_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Spreading activation ---
fn spreading_activation_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Duplicate detection ---
fn duplicate_detection_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Recursive exploration ---
fn recursive_exploration_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "recursive_explore".into(),
        description: "Recursive multi-pass query exploration with Datalog-driven discovery.\n\n\
                CALL WHEN:\n\
                - Complex multi-hop queries that need connected knowledge clusters\n\
                - Queries involving relationships between entities\n\
                - When hybrid_search returns too few results\n\n\
                DO NOT CALL:\n\
                - For simple name lookups (use retrieve_entities)\n\
                - For direct entity retrieval by ID\n\n\
                Cost: Multiple passes × hybrid_search cost. Bounded by max_passes."
            .into(),
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
    }]
}

// --- Datalog query ---
fn datalog_query_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "query_derived".into(),
        description: "Query Datalog-derived facts with provenance.\n\n\
                CALL WHEN:\n\
                - You need to explain why entity A relates to entity B\n\
                - You want transitive closure (related, reachable, isa)\n\
                - You need derived facts with explanation chains\n\n\
                DO NOT CALL:\n\
                - For raw entity retrieval (use retrieve_entities)\n\n\
                Cost: Cache hit is free. Cache miss computes Datalog evaluation."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Session UUID" },
                "predicate": { "type": "string", "description": "Derived predicate to query (e.g., 'related', 'reachable', 'isa', 'cluster')" }
            },
            "required": ["predicate"]
        }),
    }]
}

// --- Datalog rule management ---
fn datalog_rule_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Predicate promotion ---
fn predicate_promotion_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "promote_predicate".into(),
        description: "Promote a derived predicate to durable materialization.\n\n\
                CALL WHEN:\n\
                - A derived predicate is queried frequently and you want faster access\n\
                - You want to persist inference results beyond the ephemeral cache TTL\n\n\
                Cost: Runs Datalog evaluation + writes to durable tables."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Session UUID" },
                "predicate": { "type": "string", "description": "Predicate to promote (e.g., 'related', 'isa', 'reachable')" }
            },
            "required": ["predicate"]
        }),
    }]
}

// --- Typed edge tools ---
fn typed_edge_tools() -> Vec<ToolDef> {
    vec![
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
    ]
}

// --- Derived cache listing ---
fn derived_cache_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "list_derived_cache".into(),
        description: "List all derived cache entries for inspection/debugging.\n\n\
                Returns up to `limit` rows sorted by computed_at DESC.\n\n\
                Use for: audit trail, debugging derivation results, reviewing cache state."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tenant_id": { "type": "string", "description": "Tenant UUID" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Max rows to return (default 100)" }
            },
            "required": ["tenant_id"]
        }),
    }]
}

/// One lazily-produced tool definition with stable discovery metadata.
pub(super) struct ToolRecord {
    pub(super) tool: ToolDef,
    pub(super) category: &'static str,
}

/// Iterator that retains at most one bounded tool family at a time.
pub(super) struct ToolDefinitionIter {
    entity_type_enum: Value,
    family: usize,
    current: std::vec::IntoIter<ToolRecord>,
}

impl ToolDefinitionIter {
    fn family_records(&self, family: usize) -> Vec<ToolRecord> {
        let (category, tools) = match family {
            0 => ("discovery", vec![all_tools_def()]),
            1 => ("remotes", remote_memory_tools()),
            2 => ("sessions", session_continuity_tools()),
            3 => ("folds", fold_tools()),
            4 => ("entities", entity_tools(&self.entity_type_enum)),
            5 => ("feedback", feedback_tools()),
            6 => ("sessions", session_lifecycle_tools()),
            7 => ("cognitive", cognitive_memory_tools(&self.entity_type_enum)),
            8 => ("skills", skills_tools()),
            9 => ("intentions", intention_tools()),
            10 => ("temporal", temporal_fact_tools()),
            11 => ("graph", graph_traversal_tools()),
            12 => ("search", hybrid_search_tools()),
            13 => ("consolidation", dream_consolidation_tools()),
            14 => ("enrichment", enrichment_tools()),
            15 => ("operations", stats_tools()),
            16 => ("lifecycle", memory_state_tools()),
            17 => ("scoring", importance_scoring_tools()),
            18 => ("graph", memory_chain_tools()),
            19 => ("retrieval", speculative_retrieval_tools()),
            20 => ("retrieval", spreading_activation_tools()),
            21 => ("entities", duplicate_detection_tools()),
            22 => ("retrieval", recursive_exploration_tools()),
            23 => ("reasoning", datalog_query_tools()),
            24 => ("governance", datalog_rule_tools()),
            25 => ("reasoning", predicate_promotion_tools()),
            26 => ("graph", typed_edge_tools()),
            27 => ("reasoning", derived_cache_tools()),
            _ => return Vec::new(),
        };
        tools
            .into_iter()
            .map(|mut tool| {
                if let Some(short) = short_tool_name(&tool.name) {
                    tool.name = short.to_string();
                }
                ToolRecord { tool, category }
            })
            .collect()
    }
}

impl Iterator for ToolDefinitionIter {
    type Item = ToolRecord;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(record) = self.current.next() {
                return Some(record);
            }
            let records = self.family_records(self.family);
            self.family += 1;
            if records.is_empty() {
                return None;
            }
            self.current = records.into_iter();
        }
    }
}

/// Traverse definitions without materializing the complete catalog.
pub(super) fn tool_definition_records(entity_types: &[String]) -> ToolDefinitionIter {
    ToolDefinitionIter {
        entity_type_enum: serde_json::json!(entity_types),
        family: 0,
        current: Vec::new().into_iter(),
    }
}

/// Build all definitions for compatibility tests and explicit collectors.
/// Production catalog discovery uses `tool_definition_records` directly.
pub fn tool_definitions(entity_types: &[String]) -> Vec<ToolDef> {
    tool_definition_records(entity_types)
        .map(|record| record.tool)
        .collect()
}
