//! Contract tests for bounded MCP tool-catalog discovery.
//! Correctness: Correct when every surface uses the same normalized selection,
//! versioned cursor, semantic entry boundary, and caller-visible byte ceiling.
//! Last revised: 2026-08-12
//! Last changed: Covered bounded compact/schema pages and restart guidance.

use ferrosa_memory_core::dispatch::tool_catalog::{
    CatalogDetail, CatalogQuery, CatalogSurface, CatalogVisibility, MAX_CATALOG_RESPONSE_BYTES,
};
use ferrosa_memory_core::dispatch::{SessionState, dispatch, dispatch_modern};
use ferrosa_memory_core::storage::mock::MockStorage;
use ferrosa_memory_core::types::TenantContext;
use serde_json::{Value, json};
use uuid::Uuid;

// Test list:
// - [ ] Compact discovery is the all_tools default.
// - [ ] Exact names select schema detail without scanning unrelated entries.
// - [ ] Lexical query and exact categories filter before pagination.
// - [ ] Cursors bind version, surface, visibility, detail, and normalized filters.
// - [ ] Stale cursors include safe restart arguments.
// - [ ] Pages split only between entries and final encoded results stay <= 16 KiB.
// - [ ] A single oversized entry fails without cursor progress.
// - [ ] Every page includes an actionable continuation or completion hint.

#[test]
fn all_tools_query_defaults_to_compact_discovery() {
    // Given an empty all_tools request,
    let query = CatalogQuery::for_surface(
        CatalogSurface::AllTools,
        CatalogVisibility::Full,
        serde_json::json!({}),
    )
    .expect("an empty all_tools request is valid");

    // Then discovery is compact and the protocol budget is exactly 16 KiB.
    assert_eq!(query.detail(), CatalogDetail::Compact);
    assert_eq!(MAX_CATALOG_RESPONSE_BYTES, 16_384);
}

fn context() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "tool-catalog-contract".into(),
    }
}

#[tokio::test]
async fn all_tools_pages_are_bounded_searchable_and_restartable() {
    // Given the real public catalog and a broad lexical search,
    let storage = MockStorage::new();
    let session = SessionState::default();
    let ctx = context();
    let mut arguments = json!({"query": "memory"});
    let mut seen = Vec::new();

    loop {
        // When the caller requests the current page,
        let result = dispatch(
            "tools/call",
            json!({"name": "all_tools", "arguments": arguments}),
            &storage,
            &ctx,
            &session,
        )
        .await
        .expect("catalog page succeeds");

        // Then the actual CallToolResult stays under the hard final-result cap.
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_CATALOG_RESPONSE_BYTES);
        let page: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
            .expect("text fallback contains the complete bounded page");
        let structured = &result["structuredContent"];
        assert_eq!(page["catalog_version"], structured["catalog_version"]);
        assert_eq!(page["next_cursor"], structured["next_cursor"]);
        assert!(structured["tools"].is_null());
        assert!(page["hint"].is_object());
        for tool in page["tools"].as_array().unwrap() {
            seen.push(tool["name"].as_str().unwrap().to_string());
            assert!(tool["category"].is_string());
            assert!(tool["summary"].is_string());
            assert!(tool["schema_digest"].is_string());
        }
        if !page["has_more"].as_bool().unwrap() {
            assert!(page["hint"]["schema_lookup_arguments"].is_object());
            break;
        }
        arguments = page["hint"]["next_arguments"].clone();
    }

    seen.sort();
    seen.dedup();
    assert!(!seen.is_empty());
}

#[tokio::test]
async fn tools_list_uses_protocol_cursor_and_never_returns_full_catalog_at_once() {
    let storage = MockStorage::new();
    let session = SessionState::default();
    let ctx = context();
    let first = dispatch(
        "tools/list",
        json!({"include_all": true}),
        &storage,
        &ctx,
        &session,
    )
    .await
    .unwrap();

    assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_CATALOG_RESPONSE_BYTES);
    assert!(first["tools"].as_array().unwrap().len() < 95);
    assert!(first["nextCursor"].is_string());
    assert!(first["_meta"]["paginationHint"].is_object());
    assert_eq!(
        first["_meta"]["paginationHint"]["next_arguments"]["include_all"],
        true
    );
}

#[tokio::test]
async fn tools_list_traversal_is_complete_stable_and_duplicate_free() {
    let storage = MockStorage::new();
    let session = SessionState::default();
    let ctx = context();
    let mut params = json!({"include_all": true});
    let mut names = Vec::new();

    loop {
        let page = dispatch("tools/list", params, &storage, &ctx, &session)
            .await
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_CATALOG_RESPONSE_BYTES);
        names.extend(
            page["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_string()),
        );
        if page["nextCursor"].is_null() {
            assert!(page["_meta"]["paginationHint"]["schema_lookup_arguments"].is_object());
            break;
        }
        params = page["_meta"]["paginationHint"]["next_arguments"].clone();
    }

    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(names.len(), unique.len());
    assert!(names.len() >= 95);
    assert!(names.contains(&"all_tools".to_string()));
    assert!(names.contains(&"search".to_string()));
}

#[tokio::test]
async fn modern_all_tools_counts_its_complete_call_result_envelope() {
    let storage = MockStorage::new();
    let session = SessionState::default();
    let ctx = context();
    let result = dispatch_modern(
        "tools/call",
        json!({"name": "all_tools", "arguments": {"detail": "schema"}}),
        &storage,
        &ctx,
        &session,
    )
    .await
    .unwrap();

    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_CATALOG_RESPONSE_BYTES);
    assert_eq!(result["resultType"], "complete");
    assert!(result["structuredContent"]["next_cursor"].is_string());
    assert!(result["structuredContent"]["hint"]["next_arguments"].is_object());
    assert!(result["structuredContent"]["tools"].is_null());
}
