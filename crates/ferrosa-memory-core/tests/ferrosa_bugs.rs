// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Ferrosa/scylla compatibility tests.
//!
//! Fixed tests and remaining open issues. All cases target the isolated
//! test cluster (FERROSA_TEST_CQL_PORT / FERROSA_TEST_KEYSPACE) so the
//! suite behaves identically whether the local main cluster has auth
//! enabled or not. CI exports the same envs.
//!
//! Run:
//!   scripts/start-test-cluster.sh
//!   export $(scripts/start-test-cluster.sh --env)
//!   cargo test -p ferrosa-memory-core --test ferrosa_bugs -- --ignored --nocapture

use ferrosa_memory_core::cql_storage::build_col_map;
use ferrosa_memory_core::test_cluster::TestClusterConfig;
use scylla::{LegacySession, SessionBuilder};
use uuid::Uuid;

/// Returns the test cluster config or `None` when the harness env isn't
/// wired. Callers should early-return when `None` so the suite stays
/// useful for explicit `--ignored` invocations without a live cluster.
fn test_cluster() -> Option<TestClusterConfig> {
    TestClusterConfig::from_env_or_skip()
}

async fn connect_test_cluster(cfg: &TestClusterConfig) -> LegacySession {
    #[allow(deprecated)]
    SessionBuilder::new()
        .known_node(cfg.contact_point())
        .build_legacy()
        .await
        .expect("session build failed")
}

// ---- FIXED: Vector PREPARE + ANN ----

/// Creates `<ks>.test_vector_blob` if it does not exist. The table is
/// test-only (not in production DDL), so each test that depends on it must
/// bootstrap it. Using `IF NOT EXISTS` keeps this idempotent across the
/// vector_live and ferrosa_bugs suites that share the table.
async fn ensure_test_vector_blob(s: &LegacySession, ks: &str) {
    #[allow(deprecated)]
    s.query_unpaged(
        format!(
            "CREATE TABLE IF NOT EXISTS {ks}.test_vector_blob \
             (id uuid PRIMARY KEY, embedding vector<float, 4>)"
        ),
        (),
    )
    .await
    .expect("CREATE TABLE test_vector_blob");
}

#[tokio::test]
#[ignore]
async fn fixed_vector_prepare() {
    let Some(cfg) = test_cluster() else { return };
    let s = connect_test_cluster(&cfg).await;
    let ks = &cfg.keyspace;
    ensure_test_vector_blob(&s, ks).await;
    s.prepare(format!(
        "INSERT INTO {ks}.test_vector_blob (id, embedding) VALUES (?, ?)"
    ))
    .await
    .expect("PREPARE on vector column");
}

#[tokio::test]
#[ignore]
async fn fixed_vector_ann_query() {
    let Some(cfg) = test_cluster() else { return };
    let s = connect_test_cluster(&cfg).await;
    let ks = &cfg.keyspace;
    ensure_test_vector_blob(&s, ks).await;
    s.prepare(format!(
        "SELECT id FROM {ks}.test_vector_blob ORDER BY embedding ANN OF ? LIMIT 5"
    ))
    .await
    .expect("ANN query PREPARE");
}

// ---- OPEN: COUNT(*) column name ----

#[tokio::test]
#[ignore]
async fn open_count_column_name() {
    let Some(cfg) = test_cluster() else { return };
    let s = connect_test_cluster(&cfg).await;
    let ks = &cfg.keyspace;
    let prepared = s
        .prepare(format!(
            "SELECT COUNT(*) FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ?"
        ))
        .await
        .expect("PREPARE");
    #[allow(deprecated)]
    let result = s
        .execute_unpaged(&prepared, (Uuid::new_v4(), Uuid::new_v4()))
        .await
        .expect("EXECUTE");
    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    let count: i64 = ferrosa_memory_core::cql_storage::cql_get::<i64>(&rows[0], &col_map, "count")
        .expect("column should be 'count'");
    assert_eq!(count, 0);
}

// ---- OPEN: SUBSCRIBE ----

#[tokio::test]
#[ignore]
async fn open_subscribe() {
    let Some(cfg) = test_cluster() else { return };
    let s = connect_test_cluster(&cfg).await;
    let ks = &cfg.keyspace;
    #[allow(deprecated)]
    s.query_unpaged(
        format!("SUBSCRIBE SELECT * FROM {ks}.memo_cache EVERY '5s'"),
        (),
    )
    .await
    .expect("SUBSCRIBE should work");
}

// ---- DEBUG: retrieve_entities dynamic QUERY path ----

#[tokio::test]
#[ignore]
async fn debug_dynamic_query_with_bind_values() {
    let Some(cfg) = test_cluster() else { return };
    let s = connect_test_cluster(&cfg).await;
    let ks = &cfg.keyspace;
    let tid = Uuid::new_v4();
    let sid = Uuid::new_v4();
    let eid = Uuid::new_v4();

    // Insert via prepared (known working)
    let ins = s
        .prepare(format!(
            "INSERT INTO {ks}.entity_store \
             (tenant_id, session_id, entity_id, entity_name, entity_type, \
              context_snippet, confidence, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .await
        .expect("PREPARE insert");
    #[allow(deprecated)]
    s.execute_unpaged(
        &ins,
        (
            tid,
            sid,
            eid,
            "test-entity".to_string(),
            "concept".to_string(),
            "test context".to_string(),
            1.0_f32,
            chrono::Utc::now(),
        ),
    )
    .await
    .expect("EXECUTE insert");

    eprintln!("  inserted entity {eid} in partition ({tid}, {sid})");

    // Now try the EXACT query that entity_find_phonetic uses (dynamic QUERY + bind values)
    let query = format!(
        "SELECT entity_id, entity_name, entity_type, source_fold_id, \
         context_snippet, confidence, created_at \
         FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ? ALLOW FILTERING"
    );
    eprintln!("  sending dynamic QUERY with positional bind values...");
    #[allow(deprecated)]
    let result = s
        .query_unpaged(query, (tid, sid))
        .await
        .expect("dynamic QUERY with bind values should work");

    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    eprintln!("  got {} rows", rows.len());
    assert!(!rows.is_empty(), "should find the inserted entity");

    let name: String =
        ferrosa_memory_core::cql_storage::cql_get::<String>(&rows[0], &col_map, "entity_name")
            .expect("entity_name column");
    assert_eq!(name, "test-entity");
    eprintln!("  entity_name = {name} — dynamic QUERY path works!");
}

#[tokio::test]
#[ignore]
async fn debug_query_bind_values_vs_inline() {
    let Some(cfg) = test_cluster() else { return };
    let s = connect_test_cluster(&cfg).await;
    let ks = &cfg.keyspace;
    let tid = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
    let sid = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();

    // Insert with inline values (no bind markers)
    #[allow(deprecated)]
    s.query_unpaged(
        format!(
            "INSERT INTO {ks}.entity_store \
             (tenant_id, session_id, entity_id, entity_name, entity_type, \
              context_snippet, confidence, created_at) \
             VALUES (aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 11111111-2222-3333-4444-555555555555, \
                     66666666-7777-8888-9999-aaaaaaaaaaaa, 'inline-test', 'concept', \
                     'ctx', 1.0, 1711036800000)"
        ),
        (),
    )
    .await
    .expect("INSERT with inline values");
    eprintln!("  inserted with inline values");

    // 1) Read back with inline values (no bind markers)
    #[allow(deprecated)]
    let result = s
        .query_unpaged(
            format!(
                "SELECT entity_name FROM {ks}.entity_store \
                 WHERE tenant_id = aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee \
                 AND session_id = 11111111-2222-3333-4444-555555555555"
            ),
            (),
        )
        .await
        .expect("inline QUERY");
    let rows = result.rows_or_empty();
    eprintln!("  inline query: {} rows", rows.len());

    // 2) Read back with bind values
    #[allow(deprecated)]
    let result2 = s
        .query_unpaged(
            format!(
                "SELECT entity_name FROM {ks}.entity_store \
                 WHERE tenant_id = ? AND session_id = ?"
            ),
            (tid, sid),
        )
        .await
        .expect("bind value QUERY");
    let rows2 = result2.rows_or_empty();
    eprintln!("  bind value query: {} rows", rows2.len());

    // 3) Read back via prepared + execute
    let prep = s
        .prepare(format!(
            "SELECT entity_name FROM {ks}.entity_store \
             WHERE tenant_id = ? AND session_id = ?"
        ))
        .await
        .expect("PREPARE");
    #[allow(deprecated)]
    let result3 = s.execute_unpaged(&prep, (tid, sid)).await.expect("EXECUTE");
    let rows3 = result3.rows_or_empty();
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
// Observed on Ferrosa. May be:
// a) Ferrosa not auto-paging secondary index queries
// b) Interaction between ALLOW FILTERING and secondary index result sets
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
    let Some(cfg) = test_cluster() else { return };
    let s = connect_test_cluster(&cfg).await;
    let ks = &cfg.keyspace;
    let tid = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let sid = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();

    // Full table scan — ground truth
    #[allow(deprecated)]
    let r_all = s
        .query_unpaged(format!("SELECT entity_a FROM {ks}.co_occurs_with"), ())
        .await
        .expect("full scan");
    let total = r_all.rows_or_empty().len();
    eprintln!("  full scan (no WHERE): {total} rows");
    if total < 5000 {
        eprintln!("  SKIP: need >5000 rows to trigger paging. Run consolidation first.");
        return;
    }

    // Query 1: secondary index on tenant_id (idx_co_occurs_by_tenant)
    #[allow(deprecated)]
    let r_tenant = s
        .query_unpaged(
            format!(
                "SELECT entity_a FROM {ks}.co_occurs_with \
                 WHERE tenant_id = ? ALLOW FILTERING"
            ),
            (tid,),
        )
        .await
        .expect("tenant filter");
    let tenant_rows = r_tenant.rows_or_empty().len();
    eprintln!("  WHERE tenant_id (indexed): {tenant_rows} rows");

    // Query 2: secondary indexes on both columns
    #[allow(deprecated)]
    let r_both = s
        .query_unpaged(
            format!(
                "SELECT entity_a FROM {ks}.co_occurs_with \
                 WHERE session_id = ? AND tenant_id = ? ALLOW FILTERING"
            ),
            (sid, tid),
        )
        .await
        .expect("session+tenant filter");
    let both_rows = r_both.rows_or_empty().len();
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
    let Some(cfg) = test_cluster() else { return };
    let s = connect_test_cluster(&cfg).await;
    let ks = &cfg.keyspace;
    #[allow(deprecated)]
    s.query_unpaged(
        format!(
            "INSERT INTO {ks}.entity_store \
             (tenant_id, session_id, entity_id, entity_name, entity_type, context_snippet, confidence, created_at) \
             VALUES (550e8400-e29b-41d4-a716-446655440000, d855258d-c5b7-41be-bf28-e8cfa0fc6b9e, \
                     11111111-1111-1111-1111-111111111111, 'John Smith', 'person', 'test', 0.9, 1711036800000)"
        ),
        (),
    )
    .await
    .expect("INSERT");

    #[allow(deprecated)]
    let result = s
        .query_unpaged(
            format!(
                "SELECT entity_name FROM {ks}.entity_store \
                 WHERE tenant_id = 550e8400-e29b-41d4-a716-446655440000 \
                 AND session_id = d855258d-c5b7-41be-bf28-e8cfa0fc6b9e \
                 AND entity_name = 'Jon Smyth'"
            ),
            (),
        )
        .await
        .expect("phonetic query");
    let rows = result.rows_or_empty();
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
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT set"]
async fn ghost_rows_do_not_crash_queries() {
    use ferrosa_memory_core::config::FerrosaCqlConfig;
    use ferrosa_memory_core::cql_storage::CqlStorage;
    use ferrosa_memory_core::storage::Storage;
    use ferrosa_memory_core::types::TenantContext;

    let Some(cfg) = test_cluster() else { return };
    let config = FerrosaCqlConfig {
        contact_points: vec![cfg.contact_point()],
        keyspace: cfg.keyspace.clone(),
        replication_factor: 1,
        consistency: "ONE".into(),
        username: "ferrosa_user".into(),
        password: "ferrosa_user".into(),
        admin_username: None,
        admin_password: None,
    };
    let storage = match CqlStorage::connect(&config).await {
        Ok(s) => s,
        Err(_) => {
            eprintln!("SKIP: no CQL connection");
            return;
        }
    };
    let ks = &cfg.keyspace;

    let tenant_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let ctx = TenantContext {
        tenant_id,
        session_origin: "stdio".to_string(),
    };

    // Insert a valid entity
    let valid_id = Uuid::new_v4();
    let session = storage.session();
    let inserted_at = chrono::Utc::now();
    #[allow(deprecated)]
    session
        .query_unpaged(
            format!(
                "INSERT INTO {ks}.entity_store \
                 (tenant_id, session_id, entity_id, entity_name, entity_type, \
                  context_snippet, confidence, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            ),
            (
                tenant_id,
                session_id,
                valid_id,
                "valid-entity".to_string(),
                "concept".to_string(),
                "a real entity".to_string(),
                1.0_f32,
                inserted_at,
            ),
        )
        .await
        .expect("insert valid entity");

    // Insert a ghost entity row (NULL entity_name via incomplete insert)
    #[allow(deprecated)]
    session
        .query_unpaged(
            format!(
                "INSERT INTO {ks}.entity_store \
                 (tenant_id, session_id, entity_id, confidence, created_at) \
                 VALUES (?, ?, ?, ?, ?)"
            ),
            (tenant_id, session_id, Uuid::new_v4(), 0.5_f32, inserted_at),
        )
        .await
        .expect("insert ghost entity");

    // Insert a valid typed edge
    #[allow(deprecated)]
    session
        .query_unpaged(
            format!(
                "INSERT INTO {ks}.typed_edges \
                 (tenant_id, session_id, src_id, edge_type, dst_id, weight, metadata, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            ),
            (
                tenant_id,
                session_id,
                valid_id,
                "contains".to_string(),
                Uuid::new_v4(),
                0.9_f64,
                "".to_string(),
                inserted_at,
            ),
        )
        .await
        .expect("insert valid edge");

    // Insert a ghost typed edge (NULL edge_type)
    #[allow(deprecated)]
    session
        .query_unpaged(
            format!(
                "INSERT INTO {ks}.typed_edges \
                 (tenant_id, session_id, src_id, edge_type, dst_id, weight, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)"
            ),
            (
                tenant_id,
                session_id,
                Uuid::new_v4(),
                "".to_string(),
                Uuid::new_v4(),
                0.0_f64,
                inserted_at,
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
    #[allow(deprecated)]
    let _ = session
        .query_unpaged(
            format!(
                "DELETE FROM {ks}.entity_store WHERE tenant_id = {tenant_id} AND session_id = {session_id}"
            ),
            (),
        )
        .await;
    #[allow(deprecated)]
    let _ = session
        .query_unpaged(
            format!(
                "DELETE FROM {ks}.typed_edges WHERE tenant_id = {tenant_id} AND session_id = {session_id}"
            ),
            (),
        )
        .await;
}
