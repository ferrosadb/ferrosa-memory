//! Module: Verify migration 43's entity-content FTS path on an isolated Ferrosa cluster.
//! Correctness: Correct when an entity without an embedding is returned for a token that
//! exists only in `context_snippet`, proving the native content index is usable.
//! Last revised: 2026-07-24
//! Last changed: Added the C7 no-embedding content-retrieval regression.
//!
//! Run:
//!   scripts/start-test-cluster.sh
//!   export $(scripts/start-test-cluster.sh --env)
//!   cargo test -p ferrosa-memory-core --test migration_43_content_fts_live -- --ignored --nocapture

use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::test_cluster::TestClusterConfig;
use ferrosa_memory_core::types::{EntityEntry, TenantContext};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT set"]
async fn migration_43_finds_entity_content_without_an_embedding() {
    let Some(test) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let username = std::env::var("FERROSA_TEST_USERNAME").unwrap_or_else(|_| "ferrosa_user".into());
    let password = std::env::var("FERROSA_TEST_PASSWORD").unwrap_or_else(|_| "ferrosa_user".into());
    let config = FerrosaCqlConfig {
        contact_points: vec![test.contact_point()],
        keyspace: test.keyspace,
        replication_factor: 1,
        consistency: "ONE".into(),
        username,
        password,
        admin_username: None,
        admin_password: None,
    };
    let storage = CqlStorage::connect(&config)
        .await
        .expect("connect to the isolated Ferrosa test cluster");

    let ctx = TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "migration-43-content-fts-live".into(),
    };
    let session_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let token = format!("c7contentfts{}", Uuid::new_v4().simple());
    let entity_name = "C7 lexical content fixture";
    assert!(
        !entity_name.contains(&token),
        "the query token must remain absent from the entity name"
    );

    storage
        .entity_put(
            &ctx,
            &EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name: entity_name.into(),
                entity_type: "concept".into(),
                context_snippet: format!("Migration 43 content-FTS sentinel {token}."),
                entity_embedding: None,
                confidence: 1.0,
                created_at: chrono::Utc::now(),
                ..Default::default()
            },
        )
        .await
        .expect("write the no-embedding content-FTS fixture");

    let matches = storage
        .entity_find_content_fts(&ctx, session_id, &token, 10)
        .await;
    let _ = storage.entity_delete(&ctx, session_id, entity_id).await;
    let matches = matches.expect("migration 43 content FTS query must succeed");

    assert!(
        matches.iter().any(|entry| entry.entity_id == entity_id),
        "migration 43 must retrieve the content-only token without an embedding; got IDs: {:?}",
        matches
            .iter()
            .map(|entry| entry.entity_id)
            .collect::<Vec<_>>()
    );
}
