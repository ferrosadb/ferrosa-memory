//! Ferrosa/cdrs-tokio compatibility tests.
//!
//! Fixed tests and remaining open issues.
//! Run: cargo test -p ferrosa-core --test ferrosa_bugs -- --ignored --nocapture

use std::sync::Arc;

use cdrs_tokio::authenticators::NoneAuthenticatorProvider;
use cdrs_tokio::cluster::NodeTcpConfigBuilder;
use cdrs_tokio::cluster::session::{SessionBuilder, TcpSessionBuilder};
use cdrs_tokio::load_balancing::RoundRobinLoadBalancingStrategy;
use cdrs_tokio::query_values;
use cdrs_tokio::types::ByName;

macro_rules! connect {
    () => {{
        let nc = NodeTcpConfigBuilder::new()
            .with_contact_point("127.0.0.1:19042".into())
            .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
            .build()
            .await
            .unwrap();
        TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), nc)
            .build()
            .await
            .unwrap()
    }};
}

// ---- FIXED: Vector PREPARE + ANN ----

#[tokio::test]
#[ignore]
async fn fixed_vector_prepare() {
    let s = connect!();
    s.prepare("INSERT INTO agent_memory.test_vector_blob (id, embedding) VALUES (?, ?)")
        .await
        .expect("PREPARE on vector column");
}

#[tokio::test]
#[ignore]
async fn fixed_vector_ann_query() {
    let s = connect!();
    s.prepare("SELECT id FROM agent_memory.test_vector_blob ORDER BY embedding ANN OF ? LIMIT 5")
        .await
        .expect("ANN query PREPARE");
}

// ---- OPEN: COUNT(*) column name ----

#[tokio::test]
#[ignore]
async fn open_count_column_name() {
    let s = connect!();
    let prepared = s
        .prepare(
            "SELECT COUNT(*) FROM agent_memory.entity_store WHERE tenant_id = ? AND session_id = ?",
        )
        .await
        .expect("PREPARE");
    let envelope = s
        .exec_with_values(
            &prepared,
            query_values!(uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
        )
        .await
        .expect("EXECUTE");
    let rows = envelope.response_body().unwrap().into_rows().unwrap();
    let count: i64 = rows[0]
        .r_by_name("count")
        .expect("column should be 'count'");
    assert_eq!(count, 0);
}

// ---- OPEN: SUBSCRIBE ----

#[tokio::test]
#[ignore]
async fn open_subscribe() {
    let s = connect!();
    s.query("SUBSCRIBE SELECT * FROM agent_memory.memo_cache EVERY '5s'")
        .await
        .expect("SUBSCRIBE should work");
}

// ---- OPEN: Phonetic index ----

#[tokio::test]
#[ignore]
async fn open_phonetic_match() {
    let s = connect!();
    s.query(
        "INSERT INTO agent_memory.entity_store \
         (tenant_id, session_id, entity_id, entity_name, entity_type, context_snippet, confidence, created_at) \
         VALUES (550e8400-e29b-41d4-a716-446655440000, d855258d-c5b7-41be-bf28-e8cfa0fc6b9e, \
                 11111111-1111-1111-1111-111111111111, 'John Smith', 'person', 'test', 0.9, 1711036800000)",
    )
    .await
    .expect("INSERT");

    let envelope = s
        .query(
            "SELECT entity_name FROM agent_memory.entity_store \
             WHERE tenant_id = 550e8400-e29b-41d4-a716-446655440000 \
             AND session_id = d855258d-c5b7-41be-bf28-e8cfa0fc6b9e \
             AND entity_name = 'Jon Smyth'",
        )
        .await
        .expect("phonetic query");
    let rows = envelope.response_body().unwrap().into_rows().unwrap();
    assert!(
        !rows.is_empty(),
        "phonetic match should find 'John Smith' for 'Jon Smyth'"
    );
}
