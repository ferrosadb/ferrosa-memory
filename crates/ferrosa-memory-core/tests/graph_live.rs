//! Live integration test against Ferrosa's public graph HTTP endpoint.
//!
//! Graph-owned writes should go through public Cypher mutations, not direct
//! INSERTs into graph backing tables.
//!
//! Requires: podman compose up -d (single node on port 19042/17474)
//! Run with: cargo test -p ferrosa-memory-core --test graph_live -- --ignored

use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use uuid::Uuid;

async fn connect_graph(username: &str, password: &str) -> GraphClient {
    GraphClient::connect(&GraphConfig {
        http_url: "http://localhost:17474".into(),
        username: username.into(),
        password: password.into(),
        keyspace: "agent_memory".into(),
    })
    .await
    .expect("graph connect failed — is the cluster running?")
}

/// Insert a fold vertex and edge via CQL, then traverse via graph.
#[tokio::test]
async fn graph_health_check() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042 (CQL) and 17474 (graph HTTP)"
        );
    }
    let _client = connect_graph("ferrosa_admin", "ferrosa_admin").await;
    // If we get here, health check passed
}

#[tokio::test]
async fn match_empty_returns_no_rows() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042 (CQL) and 17474 (graph HTTP)"
        );
    }
    let client = connect_graph("ferrosa_admin", "ferrosa_admin").await;
    let fake_id = Uuid::new_v4();
    let ancestors = client
        .get_fold_ancestors(fake_id, Uuid::new_v4(), 5)
        .await
        .expect("MATCH query failed");
    assert!(ancestors.is_empty());
}

#[tokio::test]
async fn public_graph_write_round_trip_for_co_occurs_edges() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042 (CQL) and 17474 (graph HTTP)"
        );
    }
    let client = connect_graph("ferrosa_admin", "ferrosa_admin").await;
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
async fn ferrosa_user_is_denied_direct_graph_mutations() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042 (CQL) and 17474 (graph HTTP)"
        );
    }
    let client = connect_graph("ferrosa_user", "ferrosa_user").await;
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
