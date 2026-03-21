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

use serde_json::Value;

use crate::transport::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};

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
            description: "Check if a sub-call result is cached".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The prompt text" },
                    "context_slice": { "type": "string", "description": "Context slice for cache key" },
                    "model_version": { "type": "string", "description": "Model version string" }
                },
                "required": ["prompt", "context_slice", "model_version"]
            }),
        },
        ToolDef {
            name: "store_memo_result".into(),
            description: "Store a sub-call result in the memo cache".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "context_slice": { "type": "string" },
                    "model_version": { "type": "string" },
                    "result": { "type": "string", "description": "The sub-call result to cache" },
                    "embedding": {
                        "type": "array", "items": { "type": "number" },
                        "description": "Optional embedding vector"
                    },
                    "ttl_days": { "type": "integer", "description": "TTL in days (default: 7)" }
                },
                "required": ["prompt", "context_slice", "model_version", "result"]
            }),
        },
        ToolDef {
            name: "write_plan_node".into(),
            description: "Write a new node in the plan hierarchy".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "depth": { "type": "integer" },
                    "subtask_id": { "type": "string" },
                    "parent_subtask": { "type": "string" },
                    "goal_text": { "type": "string" }
                },
                "required": ["session_id", "depth", "subtask_id", "goal_text"]
            }),
        },
        ToolDef {
            name: "get_plan_context".into(),
            description: "Retrieve the plan tree for a session".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "max_depth": { "type": "integer" }
                },
                "required": ["session_id"]
            }),
        },
        ToolDef {
            name: "update_plan_node".into(),
            description: "Update a plan node's status".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "depth": { "type": "integer" },
                    "subtask_id": { "type": "string" },
                    "status": { "type": "string", "enum": ["pending", "active", "complete", "failed"] },
                    "outcome_summary": { "type": "string" }
                },
                "required": ["session_id", "depth", "subtask_id", "status"]
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
) -> Result<Value, (i32, String)> {
    match method {
        "initialize" => Ok(server_info()),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => {
            let tools = tool_definitions();
            Ok(serde_json::json!({ "tools": tools }))
        }
        "tools/call" => dispatch_tool(params, storage, ctx).await,
        _ => Err((METHOD_NOT_FOUND, format!("unknown method: {method}"))),
    }
}

/// Dispatch a tools/call request to the appropriate handler.
async fn dispatch_tool<S: crate::storage::Storage>(
    params: Value,
    storage: &S,
    ctx: &crate::types::TenantContext,
) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((INVALID_PARAMS, "missing tool name".into()))?;

    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    match name {
        "check_memo_cache" => handle_check_memo(args, storage, ctx).await,
        "store_memo_result" => handle_store_memo(args, storage, ctx).await,
        "write_plan_node" => handle_write_plan(args, storage, ctx).await,
        "get_plan_context" => handle_get_plan(args, storage, ctx).await,
        "update_plan_node" => handle_update_plan(args, storage, ctx).await,
        _ => Err((METHOD_NOT_FOUND, format!("unknown tool: {name}"))),
    }
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

// --- Parameter extraction helpers ---

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
        let result = dispatch("initialize", Value::Null, &store, &ctx)
            .await
            .unwrap();
        assert_eq!(result["serverInfo"]["name"], "ferrosa-memory-mcp");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_returns_all_tools() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let result = dispatch("tools/list", Value::Null, &store, &ctx)
            .await
            .unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"check_memo_cache"));
        assert!(names.contains(&"store_memo_result"));
        assert!(names.contains(&"write_plan_node"));
        assert!(names.contains(&"get_plan_context"));
        assert!(names.contains(&"update_plan_node"));
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let err = dispatch("bogus/method", Value::Null, &store, &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_call_unknown_tool() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let params = serde_json::json!({ "name": "nonexistent_tool" });
        let err = dispatch("tools/call", params, &store, &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_call_missing_params() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let params = serde_json::json!({
            "name": "check_memo_cache",
            "arguments": {}
        });
        let err = dispatch("tools/call", params, &store, &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn memo_round_trip_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();

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
        let result = dispatch("tools/call", store_params, &store, &ctx)
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
        let result = dispatch("tools/call", check_params, &store, &ctx)
            .await
            .unwrap();
        assert_eq!(result["hit"], true);
        assert_eq!(result["result"], "cached answer");
    }

    #[tokio::test]
    async fn plan_round_trip_through_dispatch() {
        let store = MockStorage::new();
        let ctx = test_ctx();
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
        let result = dispatch("tools/call", write_params, &store, &ctx)
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
        let result = dispatch("tools/call", get_params, &store, &ctx)
            .await
            .unwrap();
        assert_eq!(result["nodes"].as_array().unwrap().len(), 1);
    }
}
