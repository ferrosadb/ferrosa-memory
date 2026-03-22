//! Sprint 3 end-to-end integration test.
//! Tests entity discovery + temporal chain + graph traversal.
//! Run: cargo test -p ferrosa-core --test sprint3_e2e -- --ignored --nocapture

use ferrosa_core::entity;
use ferrosa_core::feedback;
use ferrosa_core::graph::{GraphClient, GraphConfig};
use ferrosa_core::storage::mock::MockStorage;
use ferrosa_core::temporal;
use ferrosa_core::types::TenantContext;
use uuid::Uuid;

fn ctx() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "test".into(),
    }
}

/// Full Sprint 3 workflow: entity discovery → temporal facts → feedback
#[tokio::test]
async fn entity_temporal_feedback_cycle() {
    let store = MockStorage::new();
    let ctx = ctx();
    let session = Uuid::new_v4();

    // 1. Create entities
    let alice = entity::upsert_entity(
        &store,
        &ctx,
        session,
        "Alice",
        "person",
        "works at Acme",
        None,
        None,
        Some(0.9),
    )
    .await
    .unwrap();
    assert!(alice.is_new);

    let bob = entity::upsert_entity(
        &store,
        &ctx,
        session,
        "Bob",
        "person",
        "works at Globex",
        None,
        None,
        Some(0.85),
    )
    .await
    .unwrap();
    assert!(bob.is_new);

    // 2. Duplicate entity is deduplicated
    let alice2 = entity::upsert_entity(
        &store,
        &ctx,
        session,
        "alice",
        "person",
        "different context",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!alice2.is_new);
    assert_eq!(alice2.entity_id, alice.entity_id);

    // 3. Confidence gating rejects low-confidence entity
    let low = entity::upsert_entity(
        &store,
        &ctx,
        session,
        "Unknown",
        "person",
        "ctx",
        None,
        None,
        Some(0.3),
    )
    .await;
    assert!(low.is_err());

    // 4. Write temporal facts with supersession
    let fact1 = temporal::write_temporal_fact(
        &store,
        &ctx,
        alice.entity_id,
        "Alice works at Acme",
        session,
        0.9,
    )
    .await
    .unwrap();

    let fact2 = temporal::write_temporal_fact(
        &store,
        &ctx,
        alice.entity_id,
        "Alice works at Globex",
        session,
        0.95,
    )
    .await
    .unwrap();

    // 5. Current fact should be the newer one
    let current = temporal::get_current_fact(&store, &ctx, alice.entity_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.event_id, fact2);
    assert_eq!(current.supersedes_id, Some(fact1));

    // 6. Record feedback
    let ok = feedback::record_outcome(
        &store,
        &ctx,
        session,
        Uuid::new_v4(),
        "phonetic",
        "simple",
        true,
        3,
        0,
    )
    .await
    .unwrap();
    assert!(ok);

    // 7. Retrieve entities
    let found = entity::retrieve_entities(&store, &ctx, session, "alice", None, "phonetic", None)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].entity_name, "Alice");
}

/// Graph health check (requires live cluster)
#[tokio::test]
#[ignore]
async fn graph_health_and_match() {
    let client = GraphClient::connect(&GraphConfig::default())
        .await
        .expect("graph connect failed");

    let ancestors = client
        .get_fold_ancestors(Uuid::new_v4(), Uuid::new_v4(), 3)
        .await
        .expect("MATCH failed");
    assert!(ancestors.is_empty());
}
