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
    let _eid = uuid::Uuid::parse_str("66666666-7777-8888-9999-aaaaaaaaaaaa").unwrap();

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

// ---- OPEN: Secondary index queries return only first page ----
//
// On a table with ~20K rows, secondary index queries return only ~3,500 rows
// (first result page) instead of all matching rows. Full table scan returns all.
//
// Observed on Ferrosa with cdrs-tokio. May be:
// a) Ferrosa not auto-paging secondary index queries
// b) cdrs-tokio not following paging state from RESULT frames
// c) Interaction between ALLOW FILTERING and secondary index result sets
//
// Impact: edge_list_all returned 3,521/17,604 rows, causing the viz to show
// a sparse graph. edge_list_session returned 0/17,604 before adding a
// session_id secondary index (ddl/018_edge_session_indexes.cql).
//
// Requires: co_occurs_with table with >5000 rows (run consolidation first).
// Run: cargo test -p ferrosa-memory-core --test ferrosa_bugs -- --ignored open_secondary_index_paging --nocapture

#[tokio::test]
#[ignore]
async fn open_secondary_index_paging() {
    let s = connect!();
    let tid = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let sid = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();

    // Full table scan — ground truth
    let r_all = s
        .query("SELECT entity_a FROM agent_memory.co_occurs_with")
        .await
        .expect("full scan");
    let total = r_all
        .response_body()
        .unwrap()
        .into_rows()
        .unwrap_or_default()
        .len();
    eprintln!("  full scan (no WHERE): {total} rows");
    if total < 5000 {
        eprintln!("  SKIP: need >5000 rows to trigger paging. Run consolidation first.");
        return;
    }

    // Query 1: secondary index on tenant_id (idx_co_occurs_by_tenant)
    let r_tenant = s
        .query_with_values(
            "SELECT entity_a FROM agent_memory.co_occurs_with \
             WHERE tenant_id = ? ALLOW FILTERING",
            query_values!(tid),
        )
        .await
        .expect("tenant filter");
    let tenant_rows = r_tenant
        .response_body()
        .unwrap()
        .into_rows()
        .unwrap_or_default()
        .len();
    eprintln!("  WHERE tenant_id (indexed): {tenant_rows} rows");

    // Query 2: secondary indexes on both columns
    let r_both = s
        .query_with_values(
            "SELECT entity_a FROM agent_memory.co_occurs_with \
             WHERE session_id = ? AND tenant_id = ? ALLOW FILTERING",
            query_values!(sid, tid),
        )
        .await
        .expect("session+tenant filter");
    let both_rows = r_both
        .response_body()
        .unwrap()
        .into_rows()
        .unwrap_or_default()
        .len();
    eprintln!("  WHERE session_id + tenant_id (both indexed): {both_rows} rows");

    // BUG: tenant_id-only query returns first page (~3500 rows) instead of all
    assert_eq!(
        tenant_rows, both_rows,
        "tenant_id-only query returned {tenant_rows} rows but combined query returned \
         {both_rows} — secondary index queries return inconsistent result counts. \
         Expected both to return the same number of matching rows."
    );
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

/// Ghost rows with NULL required fields must not crash row-scanning queries.
///
/// The Python CQL loader can create rows where clustering columns (entity_id,
/// src_id) are NULL. These ghost rows caused entity_list_session,
/// entity_find_phonetic, and typed_edge_list_session to return Err, which
/// broke the viz snapshot and all entity writes (via dedup check).
///
/// This test inserts ghost rows into both entity_store and typed_edges,
/// then verifies that CqlStorage methods skip them gracefully.
#[tokio::test]
#[ignore] // Requires live Ferrosa cluster on port 19042
async fn ghost_rows_do_not_crash_queries() {
    use ferrosa_memory_core::config::FerrosaCqlConfig;
    use ferrosa_memory_core::cql_storage::CqlStorage;
    use ferrosa_memory_core::storage::Storage;
    use ferrosa_memory_core::types::TenantContext;
    use uuid::Uuid;

    let config = FerrosaCqlConfig {
        contact_points: vec!["localhost:19042".into()],
        keyspace: "agent_memory".into(),
        replication_factor: 3,
        consistency: "ONE".into(),
    };
    let storage = match CqlStorage::connect(&config).await {
        Ok(s) => s,
        Err(_) => {
            eprintln!("SKIP: no CQL connection");
            return;
        }
    };

    let tenant_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let ctx = TenantContext {
        tenant_id,
        session_origin: "stdio".to_string(),
    };

    // Insert a valid entity
    let valid_id = Uuid::new_v4();
    let session = storage.session();
    session
        .query_with_values(
            "INSERT INTO agent_memory.entity_store \
             (tenant_id, session_id, entity_id, entity_name, entity_type, \
              context_snippet, confidence, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, toTimestamp(now()))"
                .to_string(),
            query_values!(
                tenant_id,
                session_id,
                valid_id,
                "valid-entity".to_string(),
                "concept".to_string(),
                "a real entity".to_string(),
                1.0_f32
            ),
        )
        .await
        .expect("insert valid entity");

    // Insert a ghost entity row (NULL entity_name via incomplete insert)
    session
        .query_with_values(
            "INSERT INTO agent_memory.entity_store \
             (tenant_id, session_id, entity_id, confidence, created_at) \
             VALUES (?, ?, ?, ?, toTimestamp(now()))"
                .to_string(),
            query_values!(tenant_id, session_id, Uuid::new_v4(), 0.5_f32),
        )
        .await
        .expect("insert ghost entity");

    // Insert a valid typed edge
    session
        .query_with_values(
            "INSERT INTO agent_memory.typed_edges \
             (tenant_id, session_id, src_id, edge_type, dst_id, weight, metadata, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, toTimestamp(now()))"
                .to_string(),
            query_values!(
                tenant_id,
                session_id,
                valid_id,
                "contains".to_string(),
                Uuid::new_v4(),
                0.9_f64,
                "".to_string()
            ),
        )
        .await
        .expect("insert valid edge");

    // Insert a ghost typed edge (NULL edge_type)
    session
        .query_with_values(
            "INSERT INTO agent_memory.typed_edges \
             (tenant_id, session_id, src_id, edge_type, dst_id, weight, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, toTimestamp(now()))"
                .to_string(),
            query_values!(
                tenant_id,
                session_id,
                Uuid::new_v4(),
                "".to_string(),
                Uuid::new_v4(),
                0.0_f64
            ),
        )
        .await
        .expect("insert ghost edge");

    // entity_list_session must not crash — should return the valid entity only
    let entities = storage
        .entity_list_session(&ctx, session_id)
        .await
        .expect("entity_list_session should not crash on ghost rows");
    assert!(
        entities.iter().any(|e| e.entity_name == "valid-entity"),
        "should find the valid entity, got: {:?}",
        entities.iter().map(|e| &e.entity_name).collect::<Vec<_>>()
    );

    // entity_find_phonetic must not crash
    let matches = storage
        .entity_find_phonetic(&ctx, session_id, "valid")
        .await
        .expect("entity_find_phonetic should not crash on ghost rows");
    assert!(
        matches.iter().any(|e| e.entity_name == "valid-entity"),
        "phonetic search should find valid-entity"
    );

    // typed_edge_list_session must not crash — should return the valid edge only
    let edges = storage
        .typed_edge_list_session(&ctx, session_id)
        .await
        .expect("typed_edge_list_session should not crash on ghost rows");
    assert_eq!(
        edges.len(),
        1,
        "should have 1 valid edge, got {}",
        edges.len()
    );
    assert_eq!(edges[0].edge_type, "contains");

    // Cleanup
    let _ = session.query(format!(
        "DELETE FROM agent_memory.entity_store WHERE tenant_id = {tenant_id} AND session_id = {session_id}"
    )).await;
    let _ = session.query(format!(
        "DELETE FROM agent_memory.typed_edges WHERE tenant_id = {tenant_id} AND session_id = {session_id}"
    )).await;
}
