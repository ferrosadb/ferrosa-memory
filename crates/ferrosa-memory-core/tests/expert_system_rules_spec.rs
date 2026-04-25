//! Expert-system rule loading contract tests.

use chrono::Utc;
use ferrosa_memory_core::config::DatalogConfig;
use ferrosa_memory_core::datalog;
use ferrosa_memory_core::dispatch::{SessionState, dispatch};
use ferrosa_memory_core::expert_system;
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::storage::mock::MockStorage;
use ferrosa_memory_core::types::{
    ApprovalDecision, ArtifactKind, RuleEntry, RuleState, TenantContext, TypedEdge,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn test_ctx() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "test".into(),
    }
}

fn unwrap_tool_result(result: Value) -> Value {
    result
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| {
            items
                .first()
                .and_then(|item| item.get("text"))
                .and_then(|text| text.as_str())
        })
        .map(|text| serde_json::from_str(text).expect("tool result is valid json"))
        .expect("tool result present")
}

fn active_rule(ctx: &TenantContext, rule_id: &str, family: &str, rule_body: &str) -> RuleEntry {
    RuleEntry {
        tenant_id: ctx.tenant_id,
        rule_id: rule_id.to_string(),
        version: 1,
        name: rule_id.to_string(),
        family: family.to_string(),
        state: RuleState::Active,
        rule_body: rule_body.to_string(),
        rule_weight: 1.0,
        incremental: false,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

async fn approve_rule(store: &MockStorage, ctx: &TenantContext, rule_id: &str) {
    expert_system::record_approval(
        store,
        ctx,
        ArtifactKind::Rule,
        rule_id,
        ApprovalDecision::Approved,
        Some("approved in test".to_string()),
        "test".to_string(),
        None,
        None,
    )
    .await
    .unwrap();
}

/// T-U-005
/// Given synthetic built-ins and stored rules
/// When the effective rule loader builds the runtime set
/// Then all inference consumers see one merged source-tagged view.
#[tokio::test]
async fn tu005_effective_loader_merges_synthetic_and_stored_rules() {
    let store = MockStorage::new();
    let ctx = test_ctx();

    store
        .rule_put(
            &ctx,
            &active_rule(
                &ctx,
                "custom-reachable-shortcut",
                "reachable",
                r#"reachable(X, Z) :- edge(X, "shortcut", Z)."#,
            ),
        )
        .await
        .unwrap();
    approve_rule(&store, &ctx, "custom-reachable-shortcut").await;

    let rules = datalog::load_effective_rule_entries(&store, &ctx, Some("reachable"))
        .await
        .unwrap();

    assert!(
        rules
            .iter()
            .any(|rule| rule.source == datalog::RuleSource::Builtin)
    );
    assert!(rules.iter().any(|rule| {
        rule.source == datalog::RuleSource::Registry
            && rule.entry.rule_id == "custom-reachable-shortcut"
    }));
}

#[tokio::test]
async fn effective_loader_without_family_includes_registry_rules_across_families() {
    let store = MockStorage::new();
    let ctx = test_ctx();

    store
        .rule_put(
            &ctx,
            &active_rule(
                &ctx,
                "custom-twohop-shortcut",
                "twohop",
                r#"twohop(X, Z) :- edge(X, "shortcut", Y), edge(Y, "shortcut", Z), X != Z."#,
            ),
        )
        .await
        .unwrap();
    approve_rule(&store, &ctx, "custom-twohop-shortcut").await;

    let rules = datalog::load_effective_rule_entries(&store, &ctx, None)
        .await
        .unwrap();

    assert!(rules.iter().any(|rule| {
        rule.source == datalog::RuleSource::Registry
            && rule.entry.rule_id == "custom-twohop-shortcut"
    }));
}

/// T-U-006
/// Given draft, deprecated, and approved rules
/// When the runtime loader returns the default active set
/// Then only approved active rules are loaded.
#[tokio::test]
async fn tu006_runtime_loader_filters_to_approved_active_rules() {
    let store = MockStorage::new();
    let ctx = test_ctx();

    let approved = active_rule(
        &ctx,
        "custom-approved-rule",
        "related",
        r#"related(X, Z) :- edge(X, "shortcut", Z)."#,
    );
    let proposed = active_rule(
        &ctx,
        "custom-proposed-rule",
        "related",
        r#"related(X, Z) :- edge(X, "backchannel", Z)."#,
    );
    store.rule_put(&ctx, &approved).await.unwrap();
    store.rule_put(&ctx, &proposed).await.unwrap();
    approve_rule(&store, &ctx, &approved.rule_id).await;

    let rules = datalog::load_effective_rule_entries(&store, &ctx, Some("related"))
        .await
        .unwrap();

    assert!(
        rules
            .iter()
            .any(|entry| entry.entry.rule_id == approved.rule_id)
    );
    assert!(
        !rules
            .iter()
            .any(|entry| entry.entry.rule_id == proposed.rule_id)
    );
}

/// T-C-002
/// Effective rule loader contract: all inference entry points share one runtime source of truth.
#[tokio::test]
async fn tc002_all_inference_paths_share_one_rule_loader() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();

    store
        .rule_put(
            &ctx,
            &active_rule(
                &ctx,
                "custom-reachable-shortcut",
                "reachable",
                r#"reachable(X, Z) :- edge(X, "shortcut", Z)."#,
            ),
        )
        .await
        .unwrap();
    approve_rule(&store, &ctx, "custom-reachable-shortcut").await;

    let expected = datalog::load_effective_rule_entries(&store, &ctx, Some("reachable"))
        .await
        .unwrap();

    let params = json!({
        "name": "manage_rules",
        "arguments": {
            "action": "list",
            "family": "reachable",
            "source": "effective"
        }
    });
    let result = dispatch("tools/call", params, &store, &ctx, &session)
        .await
        .unwrap();
    let result = unwrap_tool_result(result);

    assert_eq!(result["count"].as_u64().unwrap(), expected.len() as u64);
    let rules = result["rules"].as_array().unwrap();
    assert!(rules.iter().any(|rule| rule["source"] == "builtin"));
    assert!(rules.iter().any(|rule| rule["source"] == "registry"));
}

#[tokio::test]
async fn query_predicate_uses_registered_active_rules() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session_id = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();

    store
        .typed_edge_put(
            &ctx,
            &TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id: a,
                edge_type: "shortcut".into(),
                dst_id: b,
                weight: 1.0,
                metadata: None,
                created_at: Utc::now(),
            },
        )
        .await
        .unwrap();
    store
        .typed_edge_put(
            &ctx,
            &TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id: b,
                edge_type: "shortcut".into(),
                dst_id: c,
                weight: 1.0,
                metadata: None,
                created_at: Utc::now(),
            },
        )
        .await
        .unwrap();
    store
        .rule_put(
            &ctx,
            &active_rule(
                &ctx,
                "custom-twohop-shortcut",
                "twohop",
                r#"twohop(X, Z) :- edge(X, "shortcut", Y), edge(Y, "shortcut", Z), X != Z."#,
            ),
        )
        .await
        .unwrap();
    approve_rule(&store, &ctx, "custom-twohop-shortcut").await;

    let results = datalog::query_predicate(
        &store,
        &ctx,
        session_id,
        "twohop",
        &DatalogConfig::default(),
    )
    .await
    .unwrap();

    assert!(results.iter().any(|fact| {
        fact.pred == "twohop" && fact.src_id == a.to_string() && fact.dst_id == c.to_string()
    }));
}

#[tokio::test]
async fn manage_rules_put_invalidates_all_cache_keys_for_affected_predicate() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();

    let affected = vec![ferrosa_memory_core::types::DerivedFact {
        src_id: Uuid::new_v4().to_string(),
        pred: "related".into(),
        dst_id: Uuid::new_v4().to_string(),
        confidence: 0.9,
        rule_id: "builtin:related:1".into(),
        support_count: 1,
        provenance: vec![],
    }];
    let unaffected = vec![ferrosa_memory_core::types::DerivedFact {
        src_id: Uuid::new_v4().to_string(),
        pred: "cluster".into(),
        dst_id: Uuid::new_v4().to_string(),
        confidence: 0.9,
        rule_id: "builtin:cluster:1".into(),
        support_count: 1,
        provenance: vec![],
    }];

    store
        .derived_cache_put(&ctx, "related:session-a", &affected)
        .await
        .unwrap();
    store
        .derived_cache_put(&ctx, "consolidation:session-a", &affected)
        .await
        .unwrap();
    store
        .derived_cache_put(&ctx, "cluster:session-a", &unaffected)
        .await
        .unwrap();

    let params = json!({
        "name": "manage_rules",
        "arguments": {
            "action": "put",
            "rule_id": "custom-related-1",
            "rule_body": r#"related(X, Z) :- edge(X, "shortcut", Z)."#
        }
    });
    let result = dispatch("tools/call", params, &store, &ctx, &session)
        .await
        .unwrap();
    let result = unwrap_tool_result(result);

    assert_eq!(result["action"], "put");
    assert!(
        store
            .derived_cache_get(&ctx, "related:session-a")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .derived_cache_get(&ctx, "consolidation:session-a")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .derived_cache_get(&ctx, "cluster:session-a")
            .await
            .unwrap()
            .len(),
        1
    );
}

/// T-P-001
/// Property: effective rule loading is permutation-invariant.
#[tokio::test]
async fn tp001_effective_loader_is_permutation_invariant() {
    let ctx = test_ctx();

    let store_a = MockStorage::new();
    for (rule_id, family, body) in [
        (
            "custom-related-a",
            "related",
            r#"related(X, Z) :- edge(X, "shortcut", Z)."#,
        ),
        (
            "custom-related-b",
            "related",
            r#"related(X, Z) :- edge(X, "backchannel", Z)."#,
        ),
    ] {
        store_a
            .rule_put(&ctx, &active_rule(&ctx, rule_id, family, body))
            .await
            .unwrap();
        approve_rule(&store_a, &ctx, rule_id).await;
    }

    let store_b = MockStorage::new();
    for (rule_id, family, body) in [
        (
            "custom-related-b",
            "related",
            r#"related(X, Z) :- edge(X, "backchannel", Z)."#,
        ),
        (
            "custom-related-a",
            "related",
            r#"related(X, Z) :- edge(X, "shortcut", Z)."#,
        ),
    ] {
        store_b
            .rule_put(&ctx, &active_rule(&ctx, rule_id, family, body))
            .await
            .unwrap();
        approve_rule(&store_b, &ctx, rule_id).await;
    }

    let mut ids_a: Vec<String> =
        datalog::load_effective_rule_entries(&store_a, &ctx, Some("related"))
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.entry.rule_id)
            .collect();
    let mut ids_b: Vec<String> =
        datalog::load_effective_rule_entries(&store_b, &ctx, Some("related"))
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.entry.rule_id)
            .collect();
    ids_a.sort();
    ids_b.sort();

    assert_eq!(ids_a, ids_b);
}
