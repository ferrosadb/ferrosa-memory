// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Live CQL integration test — minimal scylla connection.
//! Run with: cargo test -p ferrosa-memory-core --test cql_live -- --ignored --nocapture

use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::{CqlStorage, build_col_map, cql_get};
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::types::{EntityEntry, TenantContext, TypedEdge};
use scylla::{LegacySession, SessionBuilder};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

fn init_test_tracing() {
    let filter = std::env::var("RUST_LOG")
        .ok()
        .unwrap_or_else(|| "scylla=warn".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_test_writer()
        .try_init();
}

async fn connect_plain(contact_point: &str) -> LegacySession {
    #[allow(deprecated)]
    SessionBuilder::new()
        .known_node(contact_point)
        .user("ferrosa_admin", "ferrosa_admin")
        .build_legacy()
        .await
        .expect("session build failed")
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn scylla_connect_and_query() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    eprintln!("building session...");
    let session = connect_plain("127.0.0.1:19042").await;

    eprintln!("connected! running query...");
    #[allow(deprecated)]
    let result = session
        .query_unpaged("SELECT keyspace_name FROM system_schema.keyspaces", ())
        .await
        .expect("query failed");

    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    eprintln!("got {} rows", rows.len());
    assert!(!rows.is_empty(), "should have at least system keyspaces");
    let _ = col_map;
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn scylla_prepare_statement() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    let session = connect_plain("127.0.0.1:19042").await;

    eprintln!("preparing statement...");
    let prepared = session
        .prepare(
            "SELECT * FROM agent_memory.memo_cache WHERE content_hash = ? AND model_version = ?",
        )
        .await;
    match &prepared {
        Ok(_) => eprintln!("prepare succeeded!"),
        Err(e) => eprintln!("prepare failed: {e}"),
    }
    prepared.expect("prepare failed");
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn prepare_vector_column() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    let session = connect_plain("127.0.0.1:19042").await;

    // The `test_vector_blob` table is test-only (not in production DDL).
    // Create it here so this test is self-contained and not order-dependent
    // with `vector_live::vector_blob_workaround_roundtrip`, which also creates it.
    #[allow(deprecated)]
    session
        .query_unpaged(
            "CREATE TABLE IF NOT EXISTS agent_memory.test_vector_blob \
             (id uuid PRIMARY KEY, embedding vector<float, 4>)",
            (),
        )
        .await
        .expect("CREATE TABLE test_vector_blob");

    eprintln!("PREPARE vector INSERT...");
    match session
        .prepare("INSERT INTO agent_memory.test_vector_blob (id, embedding) VALUES (?, ?)")
        .await
    {
        Ok(_) => eprintln!("  OK"),
        Err(e) => panic!("PREPARE vector INSERT failed: {e}"),
    }
}

/// Better live repro for the auth-enabled cluster issue:
/// mirror ferrosa-memory's actual runtime path rather than only the
/// low-level STARTUP/AUTH handshake.
///
/// This uses the real `CqlStorage::connect()` code path, which immediately
/// prepares ferrosa-memory's statement inventory after session build.
#[tokio::test]
#[ignore]
async fn auth_enabled_multipoint_cql_storage_connect_matches_fmem_runtime_path() {
    init_test_tracing();

    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042/19043/19044"
        );
    }

    let cfg = FerrosaCqlConfig {
        contact_points: vec![
            "127.0.0.1:19042".into(),
            "127.0.0.1:19043".into(),
            "127.0.0.1:19044".into(),
        ],
        keyspace: "agent_memory".into(),
        replication_factor: 3,
        consistency: "LOCAL_QUORUM".into(),
        username: "ferrosa_admin".into(),
        password: "ferrosa_admin".into(),
        admin_username: None,
        admin_password: None,
    };

    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect should succeed on the auth-enabled local cluster");

    #[allow(deprecated)]
    let result = storage
        .session()
        .query_unpaged("SELECT keyspace_name FROM system_schema.keyspaces", ())
        .await
        .expect("query should succeed after connect");
    let rows = result.rows_or_empty();
    assert!(
        !rows.is_empty(),
        "system_schema.keyspaces should not be empty"
    );
}

/// If the problem is below `CqlStorage::connect`, this narrows it to the
/// authenticated multi-contact-point scylla session builder itself.
#[tokio::test]
#[ignore]
async fn auth_enabled_multipoint_scylla_session_build_succeeds() {
    init_test_tracing();

    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042/19043/19044"
        );
    }

    #[allow(deprecated)]
    let session = SessionBuilder::new()
        .known_node("127.0.0.1:19042")
        .known_node("127.0.0.1:19043")
        .known_node("127.0.0.1:19044")
        .user("ferrosa_admin", "ferrosa_admin")
        .build_legacy()
        .await
        .expect("session build failed");

    let prepared = session
        .prepare("SELECT * FROM agent_memory.memo_cache WHERE content_hash = ? AND model_version = ? AND tenant_id = ?")
        .await
        .expect("prepare should succeed after authenticated multi-point session build");
    drop(prepared);
}

#[tokio::test]
#[ignore]
async fn confidence_scores_prepares_on_each_live_node() {
    init_test_tracing();

    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042/19043/19044"
        );
    }

    for contact_point in ["127.0.0.1:19042", "127.0.0.1:19043", "127.0.0.1:19044"] {
        #[allow(deprecated)]
        let session = SessionBuilder::new()
            .known_node(contact_point)
            .user("ferrosa_admin", "ferrosa_admin")
            .build_legacy()
            .await
            .unwrap_or_else(|e| panic!("session build failed for {contact_point}: {e}"));

        for statement in [
            "SELECT confidence, source_count, last_confirmed_at, contradiction_count \
             FROM agent_memory.confidence_scores WHERE entity_id = ? AND fact_hash = ?",
            "INSERT INTO agent_memory.confidence_scores \
             (entity_id, fact_hash, confidence, source_count, last_confirmed_at, contradiction_count) \
             VALUES (?, ?, ?, ?, ?, ?)",
        ] {
            session
                .prepare(statement)
                .await
                .unwrap_or_else(|e| panic!("prepare failed for {contact_point}: {statement}: {e}"));
        }

        for table in ["confidence_scores", "typed_edges", "co_occurs_with"] {
            #[allow(deprecated)]
            let result = session
                .query_unpaged(format!("SELECT COUNT(*) FROM agent_memory.{table}"), ())
                .await
                .unwrap_or_else(|e| panic!("{table} count query failed for {contact_point}: {e}"));
            let col_map = build_col_map(result.col_specs());
            let rows = result.rows_or_empty();
            assert_eq!(
                rows.len(),
                1,
                "{table} count query should return one row for {contact_point}"
            );
            let count: i64 = cql_get(&rows[0], &col_map, "count")
                .unwrap_or_else(|e| panic!("{table} count decode failed for {contact_point}: {e}"));
            eprintln!("{contact_point} {table} rows={count}");
            if table != "confidence_scores" {
                assert!(
                    count > 0,
                    "{table} should have restored rows on {contact_point}"
                );
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn viz_streaming_queries_return_live_nodes_and_edges() {
    init_test_tracing();

    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042/19043/19044"
        );
    }

    let cfg = FerrosaCqlConfig {
        contact_points: vec![
            "127.0.0.1:19042".into(),
            "127.0.0.1:19043".into(),
            "127.0.0.1:19044".into(),
        ],
        keyspace: "agent_memory".into(),
        replication_factor: 3,
        consistency: "LOCAL_QUORUM".into(),
        username: "ferrosa_admin".into(),
        password: "ferrosa_admin".into(),
        admin_username: None,
        admin_password: None,
    };

    let storage = Arc::new(
        CqlStorage::connect(&cfg)
            .await
            .expect("CqlStorage::connect should succeed on the auth-enabled local cluster"),
    );
    let ctx = TenantContext {
        tenant_id: Uuid::parse_str("9a5f8fbf-d842-4d30-8ea5-1aa931e618a8").unwrap(),
        session_origin: "viz-live-test".to_string(),
    };

    let (node_tx, mut node_rx) = tokio::sync::mpsc::channel::<anyhow::Result<Vec<EntityEntry>>>(4);
    let node_storage = storage.clone();
    let node_ctx = ctx.clone();
    tokio::spawn(async move {
        node_storage.entity_stream_all(node_ctx, 128, node_tx).await;
    });
    let mut nodes = 0usize;
    while let Some(chunk) = node_rx.recv().await {
        let chunk = chunk.expect("entity stream chunk should decode");
        nodes += chunk.len();
        if nodes > 0 {
            break;
        }
    }
    assert!(nodes > 0, "viz entity stream should return live nodes");

    let (edge_tx, mut edge_rx) = tokio::sync::mpsc::channel::<anyhow::Result<Vec<TypedEdge>>>(4);
    let edge_storage = storage.clone();
    tokio::spawn(async move {
        edge_storage.typed_edge_stream_all(ctx, 128, edge_tx).await;
    });
    let mut edges = 0usize;
    while let Some(chunk) = edge_rx.recv().await {
        let chunk = chunk.expect("typed-edge stream chunk should decode");
        edges += chunk.len();
        if edges > 0 {
            break;
        }
    }
    assert!(edges > 0, "viz typed-edge stream should return live edges");
}
