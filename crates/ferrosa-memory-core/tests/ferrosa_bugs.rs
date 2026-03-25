//! Ferrosa/cdrs-tokio compatibility tests.
//!
//! Fixed tests and remaining open issues.
//! Run: cargo test -p ferrosa-memory-core --test ferrosa_bugs -- --ignored --nocapture

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

// ---- DEBUG: retrieve_entities dynamic QUERY path ----

#[tokio::test]
#[ignore]
async fn debug_dynamic_query_with_bind_values() {
    let s = connect!();
    let tid = uuid::Uuid::new_v4();
    let sid = uuid::Uuid::new_v4();
    let eid = uuid::Uuid::new_v4();

    // Insert via prepared (known working)
    let ins = s
        .prepare(
            "INSERT INTO agent_memory.entity_store \
             (tenant_id, session_id, entity_id, entity_name, entity_type, \
              context_snippet, confidence, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .await
        .expect("PREPARE insert");
    s.exec_with_values(
        &ins,
        query_values!(
            tid,
            sid,
            eid,
            "test-entity".to_string(),
            "concept".to_string(),
            "test context".to_string(),
            1.0_f32,
            chrono::Utc::now().naive_utc()
        ),
    )
    .await
    .expect("EXECUTE insert");

    eprintln!("  inserted entity {eid} in partition ({tid}, {sid})");

    // Now try the EXACT query that entity_find_phonetic uses (dynamic QUERY + bind values)
    let query = "SELECT entity_id, entity_name, entity_type, source_fold_id, \
                 context_snippet, confidence, created_at \
                 FROM agent_memory.entity_store WHERE tenant_id = ? AND session_id = ? ALLOW FILTERING";
    eprintln!("  sending dynamic QUERY with positional bind values...");
    let envelope = s
        .query_with_values(query, query_values!(tid, sid))
        .await
        .expect("dynamic QUERY with bind values should work");

    let rows = envelope
        .response_body()
        .unwrap()
        .into_rows()
        .unwrap_or_default();
    eprintln!("  got {} rows", rows.len());
    assert!(!rows.is_empty(), "should find the inserted entity");

    let name: String = rows[0]
        .r_by_name("entity_name")
        .expect("entity_name column");
    assert_eq!(name, "test-entity");
    eprintln!("  entity_name = {name} — dynamic QUERY path works!");
}

#[tokio::test]
#[ignore]
async fn debug_query_bind_values_vs_inline() {
    let s = connect!();
    let tid = uuid::Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
    let sid = uuid::Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
    let eid = uuid::Uuid::parse_str("66666666-7777-8888-9999-aaaaaaaaaaaa").unwrap();

    // Insert with inline values (no bind markers)
    s.query(
        "INSERT INTO agent_memory.entity_store \
         (tenant_id, session_id, entity_id, entity_name, entity_type, \
          context_snippet, confidence, created_at) \
         VALUES (aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 11111111-2222-3333-4444-555555555555, \
                 66666666-7777-8888-9999-aaaaaaaaaaaa, 'inline-test', 'concept', \
                 'ctx', 1.0, 1711036800000)",
    )
    .await
    .expect("INSERT with inline values");
    eprintln!("  inserted with inline values");

    // 1) Read back with inline values (no bind markers)
    let envelope = s
        .query(
            "SELECT entity_name FROM agent_memory.entity_store \
             WHERE tenant_id = aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee \
             AND session_id = 11111111-2222-3333-4444-555555555555",
        )
        .await
        .expect("inline QUERY");
    let rows = envelope
        .response_body()
        .unwrap()
        .into_rows()
        .unwrap_or_default();
    eprintln!("  inline query: {} rows", rows.len());

    // 2) Read back with bind values
    let envelope2 = s
        .query_with_values(
            "SELECT entity_name FROM agent_memory.entity_store \
             WHERE tenant_id = ? AND session_id = ?",
            query_values!(tid, sid),
        )
        .await
        .expect("bind value QUERY");
    let rows2 = envelope2
        .response_body()
        .unwrap()
        .into_rows()
        .unwrap_or_default();
    eprintln!("  bind value query: {} rows", rows2.len());

    // 3) Read back via prepared + execute
    let prep = s
        .prepare(
            "SELECT entity_name FROM agent_memory.entity_store \
             WHERE tenant_id = ? AND session_id = ?",
        )
        .await
        .expect("PREPARE");
    let envelope3 = s
        .exec_with_values(&prep, query_values!(tid, sid))
        .await
        .expect("EXECUTE");
    let rows3 = envelope3
        .response_body()
        .unwrap()
        .into_rows()
        .unwrap_or_default();
    eprintln!("  prepared+execute: {} rows", rows3.len());

    assert!(!rows.is_empty(), "inline query should find data");
    assert!(
        !rows2.is_empty(),
        "bind value query should find data (QUERY frame bind values broken?)"
    );
    assert!(!rows3.is_empty(), "prepared+execute should find data");
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
