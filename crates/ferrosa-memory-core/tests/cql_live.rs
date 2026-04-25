//! Live CQL integration test — minimal cdrs-tokio connection.
//! Run with: cargo test -p ferrosa-memory-core --test cql_live -- --ignored --nocapture

use std::sync::Arc;

use cdrs_tokio::authenticators::{NoneAuthenticatorProvider, StaticPasswordAuthenticatorProvider};
use cdrs_tokio::cluster::NodeTcpConfigBuilder;
use cdrs_tokio::cluster::session::{SessionBuilder, TcpSessionBuilder};
use cdrs_tokio::load_balancing::RoundRobinLoadBalancingStrategy;
use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::CqlStorage;
use tracing_subscriber::EnvFilter;

fn init_test_tracing() {
    let filter = std::env::var("RUST_LOG")
        .ok()
        .unwrap_or_else(|| "cdrs_tokio=trace".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_test_writer()
        .try_init();
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn cdrs_connect_and_query() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    eprintln!("building node config...");
    let node_config = NodeTcpConfigBuilder::new()
        .with_contact_point("127.0.0.1:19042".into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .expect("node config failed");

    eprintln!("building session (this is where it hangs/fails)...");
    let session = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config).build(),
    )
    .await
    .expect("session build timed out")
    .expect("session build failed");

    eprintln!("connected! running query...");
    let envelope = session
        .query("SELECT keyspace_name FROM system_schema.keyspaces")
        .await
        .expect("query failed");

    let body = envelope.response_body().expect("response body");
    let rows = body.into_rows().expect("rows");
    eprintln!("got {} rows", rows.len());
    assert!(!rows.is_empty(), "should have at least system keyspaces");
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn cdrs_prepare_statement() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    let node_config = NodeTcpConfigBuilder::new()
        .with_contact_point("127.0.0.1:19042".into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .expect("node config");

    let session = TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config)
        .build()
        .await
        .expect("session");

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
    let node_config = NodeTcpConfigBuilder::new()
        .with_contact_point("127.0.0.1:19042".into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .expect("node config");
    let session = TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config)
        .build()
        .await
        .expect("session");

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
/// This uses:
/// - `StaticPasswordAuthenticatorProvider`
/// - all three local contact points
/// - `RoundRobinLoadBalancingStrategy`
/// - the real `CqlStorage::connect()` code path, which immediately prepares
///   ferrosa-memory's statement inventory after session build
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

    let envelope = storage
        .session()
        .query("SELECT keyspace_name FROM system_schema.keyspaces")
        .await
        .expect("query should succeed after connect");
    let rows = envelope
        .response_body()
        .expect("response body")
        .into_rows()
        .expect("rows");
    assert!(
        !rows.is_empty(),
        "system_schema.keyspaces should not be empty"
    );
}

/// If the problem is below `CqlStorage::connect`, this narrows it to the
/// authenticated multi-contact-point `cdrs-tokio` session builder itself.
#[tokio::test]
#[ignore]
async fn auth_enabled_multipoint_cdrs_session_build_succeeds() {
    init_test_tracing();

    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on ports 19042/19043/19044"
        );
    }

    let mut builder = NodeTcpConfigBuilder::new().with_authenticator_provider(Arc::new(
        StaticPasswordAuthenticatorProvider::new("ferrosa_admin", "ferrosa_admin"),
    ));
    for cp in ["127.0.0.1:19042", "127.0.0.1:19043", "127.0.0.1:19044"] {
        builder = builder.with_contact_point(cp.into());
    }
    let node_config = builder.build().await.expect("node config");

    let session = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config).build(),
    )
    .await
    .expect("session build timed out")
    .expect("session build failed");

    let prepared = session
        .prepare("SELECT * FROM agent_memory.memo_cache WHERE content_hash = ? AND model_version = ? AND tenant_id = ?")
        .await
        .expect("prepare should succeed after authenticated multi-point session build");
    drop(prepared);
}
