//! Expert-system governance contract tests.

use chrono::Utc;
use ferrosa_memory_core::dispatch::{SessionState, dispatch};
use ferrosa_memory_core::expert_system;
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::storage::mock::MockStorage;
use ferrosa_memory_core::types::{ClaimStatus, TenantContext, TypedEdge};
use serde_json::{Value, json};
use uuid::Uuid;

fn test_ctx() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "http:alice".into(),
    }
}

fn unwrap_tool_result(result: Value) -> Value {
    result
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|text| text.as_str())
        .map(|text| serde_json::from_str(text).expect("tool result is valid json"))
        .expect("tool result present")
}

/// T-U-007
/// Claim status transitions must align with runtime gating.
#[tokio::test]
async fn tu007_claim_state_transitions_enforce_runtime_gating() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();
    let session_id = Uuid::new_v4();

    let put = dispatch(
        "tools/call",
        json!({
            "name": "manage_claims",
            "arguments": {
                "action": "put",
                "claim_id": "claim-1",
                "claim_text": "Alice maintains the operator console",
                "domain": "ownership",
                "status": "proposed",
                "session_id": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let put = unwrap_tool_result(put);
    assert_eq!(put["claim"]["status"], "proposed");

    let default_list = dispatch(
        "tools/call",
        json!({
            "name": "manage_claims",
            "arguments": {
                "action": "list",
                "session_id": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let default_list = unwrap_tool_result(default_list);
    assert_eq!(default_list["count"], 0);

    let approved = dispatch(
        "tools/call",
        json!({
            "name": "manage_approvals",
            "arguments": {
                "action": "record",
                "artifact_kind": "claim",
                "artifact_ref": "claim-1",
                "decision": "approved",
                "session_scope": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let approved = unwrap_tool_result(approved);
    assert_eq!(approved["approval"]["decision"], "approved");

    let approved_list = dispatch(
        "tools/call",
        json!({
            "name": "manage_claims",
            "arguments": {
                "action": "list",
                "session_id": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let approved_list = unwrap_tool_result(approved_list);
    assert_eq!(approved_list["count"], 1);
    assert_eq!(approved_list["claims"][0]["status"], "approved");
}

/// T-U-008
/// Approval reviewer identity must come from auth context only.
#[tokio::test]
async fn tu008_approval_reviewer_is_auth_derived() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();

    let result = dispatch(
        "tools/call",
        json!({
            "name": "manage_approvals",
            "arguments": {
                "action": "record",
                "artifact_kind": "rule",
                "artifact_ref": "custom-related-1",
                "decision": "approved",
                "reviewer": "mallory",
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let result = unwrap_tool_result(result);
    assert_eq!(result["approval"]["reviewer"], "alice");
}

/// T-U-009
/// Alias exact scope precedence must be deterministic.
#[tokio::test]
async fn tu009_alias_exact_scope_precedence_is_deterministic() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();
    let session_id = Uuid::new_v4();

    for args in [
        json!({
            "action": "put",
            "alias_name": "run-query",
            "canonical_tool": "query_derived",
            "scope_kind": "global",
            "status": "approved",
        }),
        json!({
            "action": "put",
            "alias_name": "run-query",
            "canonical_tool": "manage_rules",
            "scope_kind": "session",
            "status": "approved",
            "session_scope": session_id,
        }),
    ] {
        dispatch(
            "tools/call",
            json!({ "name": "manage_aliases", "arguments": args }),
            &store,
            &ctx,
            &session,
        )
        .await
        .unwrap();
    }

    let resolved = dispatch(
        "tools/call",
        json!({
            "name": "manage_aliases",
            "arguments": {
                "action": "resolve",
                "alias_name": "run-query",
                "session_scope": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let resolved = unwrap_tool_result(resolved);
    assert_eq!(resolved["alias"]["canonical_tool"], "manage_rules");
}

/// T-U-010
/// Explanation bounding must truncate safely and record metrics.
#[tokio::test]
async fn tu010_explanation_bounds_emit_metrics() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();
    let session_id = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();

    for (src, dst) in [(a, b), (b, c)] {
        store
            .typed_edge_put(
                &ctx,
                &TypedEdge {
                    tenant_id: ctx.tenant_id,
                    session_id,
                    src_id: src,
                    edge_type: "co_occurs".into(),
                    dst_id: dst,
                    weight: 1.0,
                    metadata: None,
                    created_at: Utc::now(),
                },
            )
            .await
            .unwrap();
    }

    let result = dispatch(
        "tools/call",
        json!({
            "name": "explain_derived",
            "arguments": {
                "predicate": "related",
                "session_id": session_id,
                "limit": 1,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let result = unwrap_tool_result(result);
    assert_eq!(result["count"], 1);
    assert_eq!(result["explanations"][0]["truncated"], true);
    assert_eq!(
        result["explanations"][0]["support_chain"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let (hits, compute_ms) = store.heat_get(&ctx, "explain:related", 7).await.unwrap();
    assert_eq!(hits, 0);
    assert!(compute_ms >= 0);
    assert!(
        store
            .heat_records
            .lock()
            .await
            .iter()
            .any(|(pred, _, _)| pred == "explain:related")
    );
}

/// T-C-003
/// Approval table remains authoritative over any entity mirror.
#[tokio::test]
async fn tc003_approval_log_stays_authoritative_over_mirror() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();
    let session_id = Uuid::new_v4();

    dispatch(
        "tools/call",
        json!({
            "name": "manage_approvals",
            "arguments": {
                "action": "record",
                "artifact_kind": "claim",
                "artifact_ref": "claim-authority",
                "decision": "approved",
                "session_scope": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();

    let rejected = dispatch(
        "tools/call",
        json!({
            "name": "manage_approvals",
            "arguments": {
                "action": "record",
                "artifact_kind": "claim",
                "artifact_ref": "claim-authority",
                "decision": "rejected",
                "session_scope": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let rejected = unwrap_tool_result(rejected);
    let mirror_entity_id = Uuid::parse_str(
        rejected["approval"]["mirror_entity_id"]
            .as_str()
            .expect("mirror_entity_id string"),
    )
    .unwrap();

    let stale_mirror = ferrosa_memory_core::types::EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id: mirror_entity_id,
        session_id,
        entity_name: "approval:claim:claim-authority".into(),
        entity_type: expert_system::APPROVAL_MIRROR_ENTITY_TYPE.into(),
        source_fold_id: None,
        context_snippet: "stale mirror".into(),
        entity_embedding: None,
        confidence: 1.0,
        state: ferrosa_memory_core::types::MemoryState::Active,
        created_at: Utc::now(),
        description: None,
        description_embedding: None,
        tags: vec![],
        properties: json!({ "decision": ClaimStatus::Approved }),
        content_hash: None,
        updated_at: Some(Utc::now()),
        scope: ferrosa_memory_core::types::EntityScope::Global,
        ingested_by_session: Some(session_id),
    };
    store.entity_put(&ctx, &stale_mirror).await.unwrap();

    let latest = dispatch(
        "tools/call",
        json!({
            "name": "manage_approvals",
            "arguments": {
                "action": "latest",
                "artifact_kind": "claim",
                "artifact_ref": "claim-authority",
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let latest = unwrap_tool_result(latest);
    assert_eq!(latest["approval"]["decision"], "rejected");
}

/// T-C-004
/// Exact alias lookup owns execution semantics.
#[tokio::test]
async fn tc004_exact_alias_lookup_owns_execution_semantics() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();
    let session_id = Uuid::new_v4();

    dispatch(
        "tools/call",
        json!({
            "name": "manage_aliases",
            "arguments": {
                "action": "put",
                "alias_name": "status",
                "canonical_tool": "manage_rules",
                "scope_kind": "global",
                "status": "approved",
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    dispatch(
        "tools/call",
        json!({
            "name": "manage_aliases",
            "arguments": {
                "action": "put",
                "alias_name": "status",
                "canonical_tool": "query_derived",
                "scope_kind": "session",
                "status": "approved",
                "session_scope": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();

    let resolved = dispatch(
        "tools/call",
        json!({
            "name": "manage_aliases",
            "arguments": {
                "action": "resolve",
                "alias_name": "status",
                "session_scope": session_id,
            }
        }),
        &store,
        &ctx,
        &session,
    )
    .await
    .unwrap();
    let resolved = unwrap_tool_result(resolved);
    assert_eq!(resolved["alias"]["canonical_tool"], "query_derived");
}

/// T-C-005
/// Query-surface backend rejects writes and scope breaks consistently.
#[tokio::test]
async fn tc005_query_surfaces_reject_writes_and_scope_breaks() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();

    let claim = expert_system::claim_entity(
        &ctx,
        "claim-qs-1",
        session_a,
        "Shared HTTP must stay read only",
        "security",
        ClaimStatus::Approved,
        0.95,
        None,
        1,
        None,
    );
    store.entity_put(&ctx, &claim).await.unwrap();

    let write_err = expert_system::run_readonly_cql(
        &store,
        &ctx,
        "UPDATE agent_memory.rules_by_id SET name='x' WHERE tenant_id = 1",
        25,
    )
    .await
    .expect_err("write query must be rejected");
    assert!(
        write_err
            .to_string()
            .contains("only SELECT queries are supported")
            || write_err.to_string().contains("write CQL is not allowed")
    );

    let scoped = expert_system::run_readonly_cql(
        &store,
        &ctx,
        &format!(
            "SELECT * FROM entity_store WHERE session_id = '{}' LIMIT 25",
            session_b
        ),
        25,
    )
    .await
    .unwrap();
    assert_eq!(scoped["count"], 0);
    assert_eq!(scoped["rows"].as_array().unwrap().len(), 0);
}
