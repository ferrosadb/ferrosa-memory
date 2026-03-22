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
    use serde_json::Value;
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
}
