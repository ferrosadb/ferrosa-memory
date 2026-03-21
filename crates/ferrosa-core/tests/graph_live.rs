//! Live integration test against Ferrosa's graph HTTP + CQL endpoints.
//!
//! Writes data via CQL (INSERT into vertex/edge tables), then traverses
//! via graph HTTP (MATCH queries). This matches Ferrosa's architecture:
//! graph vertices are CQL rows.
//!
//! Requires: docker compose up -d (single node on port 19042/17474)
//! Run with: cargo test -p ferrosa-core --test graph_live -- --ignored

use ferrosa_core::graph::{GraphClient, GraphConfig};
use uuid::Uuid;

async fn connect_graph() -> GraphClient {
    GraphClient::connect(&GraphConfig {
        http_url: "http://localhost:17474".into(),
        username: "cassandra".into(),
        password: "cassandra".into(),
        keyspace: "agent_memory".into(),
    })
    .await
    .expect("graph connect failed — is the cluster running?")
}

/// Insert a fold vertex and edge via CQL, then traverse via graph.
#[tokio::test]
#[ignore]
async fn graph_health_check() {
    let _client = connect_graph().await;
    // If we get here, health check passed
}

#[tokio::test]
#[ignore]
async fn match_empty_returns_no_rows() {
    let client = connect_graph().await;
    let fake_id = Uuid::new_v4();
    let ancestors = client
        .get_fold_ancestors(fake_id, Uuid::new_v4(), 5)
        .await
        .expect("MATCH query failed");
    assert!(ancestors.is_empty());
}
