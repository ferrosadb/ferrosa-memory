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

// ─── Negated rules through the registry ───────────────────────────

async fn put_edge(
    store: &MockStorage,
    ctx: &TenantContext,
    session_id: Uuid,
    src: Uuid,
    edge_type: &str,
    dst: Uuid,
) {
    store
        .typed_edge_put(
            ctx,
            &TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id: src,
                edge_type: edge_type.into(),
                dst_id: dst,
                weight: 1.0,
                metadata: None,
                created_at: Utc::now(),
            },
        )
        .await
        .unwrap();
}

/// The whole point of negation living in the rule *language* rather than in
/// Rust: a tenant can store an exclusion and the engine runs it.
#[tokio::test]
async fn a_negated_rule_stored_in_the_registry_is_loaded_parsed_and_evaluated() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session_id = Uuid::new_v4();
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    put_edge(&store, &ctx, session_id, a, "co_occurs", b).await;
    put_edge(&store, &ctx, session_id, b, "co_occurs", c).await;
    // b has been superseded by a.
    put_edge(&store, &ctx, session_id, a, "supersedes", b).await;

    store
        .rule_put(
            &ctx,
            &active_rule(
                &ctx,
                "custom-live",
                "live",
                "live(X, X) :- co_occurs(X, _), not supersedes(_, X).",
            ),
        )
        .await
        .unwrap();
    approve_rule(&store, &ctx, "custom-live").await;

    let results =
        datalog::query_predicate(&store, &ctx, session_id, "live", &DatalogConfig::default())
            .await
            .unwrap();

    let live: Vec<String> = results.iter().map(|r| r.src_id.clone()).collect();
    assert!(
        live.contains(&a.to_string()),
        "a co-occurs and nothing supersedes it"
    );
    assert!(
        !live.contains(&b.to_string()),
        "b is superseded, so the negated atom must exclude it"
    );

    // The absence is recorded, not silently dropped.
    let fact = results.iter().find(|r| r.src_id == a.to_string()).unwrap();
    assert!(
        fact.provenance
            .iter()
            .any(|s| s.parent_kind == "absence" && s.parent_pred == "supersedes"),
        "provenance should name the absent predicate"
    );

    // And it is never persisted: a later `supersedes` edge would falsify it,
    // and this cache is append-only.
    assert!(!fact.is_cacheable());
    let cached = store
        .derived_cache_get(&ctx, &format!("live:{session_id}"))
        .await
        .unwrap();
    assert!(
        cached.is_empty(),
        "a derivation resting on an absence must not reach the cache"
    );
}

/// A stored rule whose negation is unsafe must fail the load loudly rather
/// than silently contributing nothing to the rule set.
#[tokio::test]
async fn a_stored_rule_with_unsafe_negation_fails_the_load_and_names_the_variable() {
    let store = MockStorage::new();
    let ctx = test_ctx();

    store
        .rule_put(
            &ctx,
            &active_rule(
                &ctx,
                "custom-unsafe",
                "unsafe_neg",
                "unsafe_neg(X, X) :- co_occurs(X, _), not supersedes(Q, Z).",
            ),
        )
        .await
        .unwrap();
    approve_rule(&store, &ctx, "custom-unsafe").await;

    let err = datalog::load_effective_rules(&store, &ctx, Some("unsafe_neg"))
        .await
        .expect_err("unsafe negation must not load");
    let msg = err.to_string();
    assert!(msg.contains('Q') && msg.contains('Z'), "got: {msg}");
}

/// Documents today's behaviour, which negation makes easier to reach: a
/// stored rule set that cannot be stratified derives NOTHING and says so only
/// in a log line. The caller cannot tell this from "no rules matched".
///
/// This test changes when `evaluate` learns to return a typed error.
#[tokio::test]
async fn a_stored_rule_recursive_through_negation_currently_derives_nothing_silently() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session_id = Uuid::new_v4();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    put_edge(&store, &ctx, session_id, a, "co_occurs", b).await;

    store
        .rule_put(
            &ctx,
            &active_rule(
                &ctx,
                "custom-paradox",
                "paradox",
                "paradox(X, X) :- co_occurs(X, _), not paradox(X, X).",
            ),
        )
        .await
        .unwrap();
    approve_rule(&store, &ctx, "custom-paradox").await;

    // It parses and loads — the rejection is the stratifier's job.
    let rules = datalog::load_effective_rules(&store, &ctx, Some("paradox"))
        .await
        .expect("an unstratifiable rule still parses");
    assert!(matches!(
        datalog::stratify(&rules),
        Err(ferrosa_memory_core::types::StratifyError::RecursionThroughNegation { .. })
    ));

    let results = datalog::query_predicate(
        &store,
        &ctx,
        session_id,
        "paradox",
        &DatalogConfig::default(),
    )
    .await
    .unwrap();
    assert!(
        results.is_empty(),
        "known gap: rejection is indistinguishable from no match (see t_82f2fde7)"
    );
}

/// The tenant-facing path: `manage_rules put` validates with the same
/// `parse_rule` the loader uses, so a negated rule is accepted on write and an
/// unsafe one is refused there — at the point the tenant can still fix it,
/// rather than at load time in someone else's session.
#[tokio::test]
async fn manage_rules_put_accepts_a_negated_rule_and_refuses_an_unsafe_one() {
    let store = MockStorage::new();
    let ctx = test_ctx();
    let session = SessionState::default();

    let put = |rule_id: &'static str, body: &'static str| {
        json!({
            "name": "manage_rules",
            "arguments": {
                "action": "put",
                "rule_id": rule_id,
                "rule_body": body
            }
        })
    };

    // Accepted, and stored verbatim as text.
    let ok = dispatch(
        "tools/call",
        put(
            "custom-shareable",
            r#"shareable(E, E) :- tier(E, _), not tagged(E, "secret")."#,
        ),
        &store,
        &ctx,
        &session,
    )
    .await
    .expect("a negated rule is valid syntax");
    let body = unwrap_tool_result(ok);
    assert_eq!(body["action"], "put");

    let stored = store
        .rule_get(&ctx, "custom-shareable")
        .await
        .unwrap()
        .expect("rule was stored");
    assert!(
        stored.rule_body.contains("not tagged"),
        "the negation survives the round trip as text: {}",
        stored.rule_body
    );
    // And it parses back into a rule the engine will run.
    let reparsed = ferrosa_memory_core::datalog::parse_rule(&stored.rule_body).unwrap();
    assert_eq!(reparsed.negated.len(), 1);
    assert_eq!(reparsed.negated[0].predicate, "tagged");

    // Refused at write time, naming the unbound variable.
    let err = dispatch(
        "tools/call",
        put(
            "custom-unsafe-write",
            "bad(E, E) :- tier(E, _), not tagged(Other, \"secret\").",
        ),
        &store,
        &ctx,
        &session,
    )
    .await
    .expect_err("unsafe negation must be refused on write");
    assert!(
        err.1.contains("Other"),
        "the tenant should be told which variable is unbound, got: {}",
        err.1
    );
    assert!(
        store
            .rule_get(&ctx, "custom-unsafe-write")
            .await
            .unwrap()
            .is_none(),
        "a refused rule must not be stored"
    );
}
