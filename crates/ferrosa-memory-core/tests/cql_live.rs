// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Live CQL integration test — minimal scylla connection.
//! Run with: cargo test -p ferrosa-memory-core --test cql_live -- --ignored --nocapture

use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::{CqlStorage, build_col_map};
use scylla::{LegacySession, SessionBuilder};
use tracing_subscriber::EnvFilter;

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
