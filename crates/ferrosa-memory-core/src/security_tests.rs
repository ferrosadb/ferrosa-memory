//! Security hardening tests (4.10 / STRIDE verification).
//!
//! These tests verify that the critical security invariants hold:
//! - Tenant isolation: tenant_id from auth, never client-supplied
//! - Confidence gating: low-confidence entities rejected
//! - Write-only feedback: no read path via MCP
//! - Session isolation: delete only affects own session

#[cfg(test)]
mod tests {
    use crate::auth;
    use crate::dispatch;
    use crate::entity;
    use crate::session;
    use crate::storage::mock::MockStorage;
    use crate::types::TenantContext;
    use uuid::Uuid;

    fn ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    // --- S2: tenant_id never client-supplied ---

    #[test]
    fn stdio_auth_sets_tenant_from_config() {
        let tid = Uuid::new_v4();
        let ctx = auth::authenticate_stdio(tid);
        assert_eq!(ctx.tenant_id, tid);
        assert_eq!(ctx.session_origin, "stdio");
    }

    #[test]
    fn http_auth_rejects_invalid_credentials() {
        let result = auth::authenticate_http("bad", "creds", |_, _| None);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn tool_call_without_name_rejected() {
        let store = MockStorage::new();
        let ctx = ctx();
        let params = serde_json::json!({ "arguments": {} });
        let err = dispatch::dispatch(
            "tools/call",
            params,
            &store,
            &ctx,
            &dispatch::SessionState::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, -32602); // INVALID_PARAMS
    }

    #[tokio::test]
    async fn unknown_tool_rejected() {
        let store = MockStorage::new();
        let ctx = ctx();
        let params = serde_json::json!({ "name": "hack_the_planet" });
        let err = dispatch::dispatch(
            "tools/call",
            params,
            &store,
            &ctx,
            &dispatch::SessionState::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, -32601); // METHOD_NOT_FOUND
    }

    // --- T1/F19: confidence gating ---

    #[tokio::test]
    async fn entity_rejected_below_confidence_gate() {
        let store = MockStorage::new();
        let ctx = ctx();
        let err = entity::upsert_entity(
            &store,
            &ctx,
            Uuid::new_v4(),
            "Suspicious",
            "person",
            "injected",
            None,
            None,
            Some(0.3),
        )
        .await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("confidence"));
    }

    #[tokio::test]
    async fn entity_accepted_at_confidence_gate() {
        let store = MockStorage::new();
        let ctx = ctx();
        let result = entity::upsert_entity(
            &store,
            &ctx,
            Uuid::new_v4(),
            "Trusted",
            "person",
            "verified",
            None,
            None,
            Some(0.7),
        )
        .await;
        assert!(result.is_ok());
    }

    // --- E2: feedback_outcomes write-only ---

    #[tokio::test]
    async fn no_read_tool_for_feedback() {
        let store = MockStorage::new();
        let ctx = ctx();
        // The only feedback tool is record_outcome (write).
        // There is no "get_feedback" or "query_feedback" tool.
        let params = serde_json::json!({
            "name": "query_feedback",
            "arguments": { "session_id": Uuid::new_v4().to_string() }
        });
        let err = dispatch::dispatch(
            "tools/call",
            params,
            &store,
            &ctx,
            &dispatch::SessionState::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, -32601); // METHOD_NOT_FOUND
    }

    // --- Session isolation ---

    #[tokio::test]
    async fn delete_session_only_affects_target() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid1 = Uuid::new_v4();
        let sid2 = Uuid::new_v4();

        crate::plan::write_plan_node(&store, &ctx, sid1, 0, "a", None, "goal1")
            .await
            .unwrap();
        crate::plan::write_plan_node(&store, &ctx, sid2, 0, "b", None, "goal2")
            .await
            .unwrap();

        session::delete_session(&store, &ctx, sid1).await.unwrap();

        let p1 = crate::plan::get_plan_context(&store, &ctx, sid1, None)
            .await
            .unwrap();
        assert!(p1.nodes.is_empty(), "sid1 should be deleted");

        let p2 = crate::plan::get_plan_context(&store, &ctx, sid2, None)
            .await
            .unwrap();
        assert_eq!(p2.nodes.len(), 1, "sid2 should be untouched");
    }

    // --- Origin validation ---

    #[test]
    fn known_origins_accepted() {
        assert!(auth::validate_origin("stdio").is_ok());
        assert!(auth::validate_origin("http").is_ok());
        assert!(auth::validate_origin("sse").is_ok());
    }

    #[test]
    fn unknown_origin_rejected() {
        assert!(auth::validate_origin("websocket").is_err());
        assert!(auth::validate_origin("grpc").is_err());
    }

    // --- Audit log persistence via dispatch ---

    #[tokio::test]
    async fn entity_write_creates_audit_entry() {
        let store = MockStorage::new();
        let ctx = ctx();
        let session = dispatch::SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "upsert_entity",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_name": "AuditTarget",
                "entity_type": "concept",
                "context_snippet": "testing audit log creation",
                "confidence": 0.9
            }
        });
        dispatch::dispatch("tools/call", params, &store, &ctx, &session)
            .await
            .unwrap();

        let entries = store.audit_entries.lock().await;
        assert!(
            !entries.is_empty(),
            "upsert_entity should create at least one audit entry"
        );
        assert_eq!(entries[0].operation, "upsert");
        assert_eq!(entries[0].target_table, "entity_store");
        assert_eq!(entries[0].session_id, sid);
        assert_eq!(entries[0].tenant_id, ctx.tenant_id);
    }

    // --- Quota enforcement at dispatch level ---

    #[tokio::test]
    async fn entity_quota_enforced() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        // Fill mock storage with entities up to the configurable limit using
        // upsert_entity_with_limit (limit = 3) so the test runs fast.
        for i in 0..3 {
            entity::upsert_entity_with_limit(
                &store,
                &ctx,
                sid,
                &format!("entity_{i}"),
                "concept",
                "fill",
                None,
                None,
                Some(0.9),
                3,
            )
            .await
            .unwrap();
        }

        // The next write must be rejected with a quota error.
        let err = entity::upsert_entity_with_limit(
            &store,
            &ctx,
            sid,
            "overflow",
            "concept",
            "too many",
            None,
            None,
            Some(0.9),
            3,
        )
        .await
        .unwrap_err();

        assert!(
            err.downcast_ref::<crate::quota::QuotaExceeded>().is_some(),
            "expected QuotaExceeded, got: {err}"
        );
    }

    // --- Right-to-deletion cascade ---

    #[tokio::test]
    async fn delete_session_cascades_all_tables() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        // 1. Entity
        let ent = entity::upsert_entity(
            &store,
            &ctx,
            sid,
            "Alice",
            "person",
            "ctx",
            None,
            None,
            Some(0.9),
        )
        .await
        .unwrap();

        // 2. Fold
        let _fold_id = crate::fold::start_fold(&store, &ctx, sid, 0, None, "fold context")
            .await
            .unwrap();

        // 3. Memo (memos are tenant-scoped, not session-scoped, so not deleted)
        crate::memo::store_memo_result(
            &store,
            &ctx,
            &crate::memo::StoreMemoParams {
                prompt: "test",
                context_slice: "ctx",
                model_version: "v1",
                result: "answer",
                embedding: None,
                ttl_days: None,
            },
        )
        .await
        .unwrap();

        // 4. Temporal fact
        crate::temporal::write_temporal_fact(
            &store,
            &ctx,
            ent.entity_id,
            "Alice works at Acme",
            sid,
            0.9,
        )
        .await
        .unwrap();

        // 5. Feedback
        crate::feedback::record_outcome(
            &store,
            &ctx,
            sid,
            Uuid::new_v4(),
            "phonetic",
            "simple",
            true,
            5,
            0,
        )
        .await
        .unwrap();

        // 6. Plan
        crate::plan::write_plan_node(&store, &ctx, sid, 0, "root", None, "goal")
            .await
            .unwrap();

        // Verify data exists before deletion
        assert!(!store.entities.lock().await.is_empty());
        assert!(!store.folds.lock().await.is_empty());
        assert!(!store.temporal_events.lock().await.is_empty());
        assert!(!store.feedback.lock().await.is_empty());
        assert!(!store.plans.lock().await.is_empty());

        // Delete session
        let result = session::delete_session(&store, &ctx, sid).await.unwrap();
        assert!(result.deleted);

        // Verify all session-scoped tables are empty for this session
        let entities: Vec<_> = store
            .entities
            .lock()
            .await
            .iter()
            .filter(|e| e.session_id == sid)
            .cloned()
            .collect();
        assert!(entities.is_empty(), "entities should be deleted");

        let folds: Vec<_> = store
            .folds
            .lock()
            .await
            .iter()
            .filter(|f| f.session_id == sid)
            .cloned()
            .collect();
        assert!(folds.is_empty(), "folds should be deleted");

        let events: Vec<_> = store
            .temporal_events
            .lock()
            .await
            .iter()
            .filter(|e| e.source_session == sid)
            .cloned()
            .collect();
        assert!(events.is_empty(), "temporal events should be deleted");

        let feedback: Vec<_> = store
            .feedback
            .lock()
            .await
            .iter()
            .filter(|f| f.session_id == sid)
            .cloned()
            .collect();
        assert!(feedback.is_empty(), "feedback should be deleted");

        let plans = crate::plan::get_plan_context(&store, &ctx, sid, None)
            .await
            .unwrap();
        assert!(plans.nodes.is_empty(), "plans should be deleted");
    }

    // --- Feedback write-only confirmation ---

    #[test]
    fn no_tool_reads_feedback_outcomes() {
        let tools = dispatch::tool_definitions();
        let feedback_readers: Vec<_> = tools
            .iter()
            .filter(|t| {
                let name_lower = t.name.to_lowercase();
                // No tool should query/read feedback_outcomes
                (name_lower.contains("feedback")
                    || name_lower.contains("get_feedback")
                    || name_lower.contains("query_feedback")
                    || name_lower.contains("list_feedback"))
                    && !name_lower.contains("record")
            })
            .collect();

        assert!(
            feedback_readers.is_empty(),
            "no tool should read feedback_outcomes, but found: {:?}",
            feedback_readers.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }
}
