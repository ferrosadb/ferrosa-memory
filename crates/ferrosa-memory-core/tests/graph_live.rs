//! Live integration test against Ferrosa's public graph HTTP endpoint.
//!
//! Graph-owned writes should go through public Cypher mutations, not direct
//! INSERTs into graph backing tables.
//!
//! Targets the isolated test cluster via FERROSA_TEST_GRAPH_URL /
//! FERROSA_TEST_KEYSPACE so the suite stays portable across local dev
//! (test cluster on 17974) and CI (same env vars exported by the
//! cluster-int job).
//!
//! Run:
//!   scripts/start-test-cluster.sh
//!   export $(scripts/start-test-cluster.sh --env)
//!   cargo test -p ferrosa-memory-core --test graph_live -- --ignored

use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use ferrosa_memory_core::test_cluster::TestClusterConfig;
use uuid::Uuid;

async fn connect_graph(cfg: &TestClusterConfig, username: &str, password: &str) -> GraphClient {
    GraphClient::connect(&GraphConfig {
        http_url: cfg.graph_url.clone(),
        username: username.into(),
        password: password.into(),
        keyspace: cfg.keyspace.clone(),
    })
    .await
    .expect("graph connect failed — is the cluster running?")
}

/// Insert a fold vertex and edge via CQL, then traverse via graph.
#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn graph_health_check() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let _client = connect_graph(&cfg, "ferrosa_admin", "ferrosa_admin").await;
    // If we get here, health check passed
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn match_empty_returns_no_rows() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let client = connect_graph(&cfg, "ferrosa_admin", "ferrosa_admin").await;
    let fake_id = Uuid::new_v4();
    let ancestors = client
        .get_fold_ancestors(fake_id, Uuid::new_v4(), 5)
        .await
        .expect("MATCH query failed");
    assert!(ancestors.is_empty());
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn public_graph_write_round_trip_for_co_occurs_edges() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let client = connect_graph(&cfg, "ferrosa_admin", "ferrosa_admin").await;
    let tenant_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let entity_a = Uuid::new_v4();
    let entity_b = Uuid::new_v4();

    client
        .put_co_occurs_edge(tenant_id, session_id, entity_a, entity_b, 0.75)
        .await
        .expect("public graph mutation should succeed");

    let related = client
        .list_co_occurs_edges(tenant_id)
        .await
        .expect("MATCH query should succeed after public write");
    assert!(
        related.iter().any(|row| row.src_id == entity_a
            && row.dst_id == entity_b
            && (row.strength - 0.75).abs() < f32::EPSILON),
        "public graph reads must observe the co-occurs write"
    );

    client
        .set_co_occurs_strength(tenant_id, entity_a, entity_b, 0.5)
        .await
        .expect("relationship property update should succeed");
    let rows = client
        .list_co_occurs_edges(tenant_id)
        .await
        .expect("co-occurs listing should succeed");
    assert!(
        rows.iter().any(|row| row.src_id == entity_a
            && row.dst_id == entity_b
            && (row.strength - 0.5).abs() < f32::EPSILON),
        "updated strength must be visible through the public graph path"
    );

    client
        .delete_co_occurs_edge(tenant_id, entity_a, entity_b)
        .await
        .expect("relationship delete should succeed");
    let rows = client
        .list_co_occurs_edges(tenant_id)
        .await
        .expect("co-occurs listing should succeed");
    assert!(
        !rows
            .iter()
            .any(|row| row.src_id == entity_a && row.dst_id == entity_b),
        "deleted relationship must no longer appear"
    );
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn ferrosa_user_is_denied_direct_graph_mutations() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    if std::env::var("FERROSA_TEST_AUTH_ENABLED").ok().as_deref() != Some("1") {
        eprintln!("skipping graph authz assertion because FERROSA_TEST_AUTH_ENABLED=1 is not set");
        return;
    }
    let client = connect_graph(&cfg, "ferrosa_user", "ferrosa_user").await;
    let tenant_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let entity_a = Uuid::new_v4();
    let entity_b = Uuid::new_v4();
    let err = client
        .put_co_occurs_edge(tenant_id, session_id, entity_a, entity_b, 1.0)
        .await
        .expect_err("ferrosa_user should not be allowed to mutate the public graph directly");
    assert!(
        err.to_string().contains("permission denied"),
        "expected permission denied, got: {err}"
    );
}
