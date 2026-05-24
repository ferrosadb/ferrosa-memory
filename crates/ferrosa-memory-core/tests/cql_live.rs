// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Live CQL integration test — minimal scylla connection.
//! Run with: cargo test -p ferrosa-memory-core --test cql_live -- --ignored --nocapture

use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::{CqlStorage, build_col_map, cql_get};
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::types::{EntityEntry, TenantContext, TypedEdge};
use futures_util::StreamExt;
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

fn local_cluster_config() -> FerrosaCqlConfig {
    FerrosaCqlConfig {
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
    }
}

/// Insert one entity, one typed_edge, and one co-occurrence edge so
/// downstream count/stream assertions have rows to find. RF=3 +
/// LOCAL_QUORUM means every coordinator will see the rows.
///
/// `tenant_id` matters for tenant-scoped reads: entity_stream_all,
/// typed_edge_stream_all, edge_list_all all bind `ctx.tenant_id`. Pass
/// the tenant the test will read under. Pass `Uuid::new_v4()` if the
/// assertion is unscoped (e.g. `COUNT(*)`).
///
/// `entity_put` goes through the Storage trait. `typed_edges` and
/// `co_occurs_with` are graph-annotated, so the Storage adapter
/// rejects direct writes by design — but a *test fixture* legitimately
/// needs the rows on disk for the streaming-read path to yield. Use
/// raw CQL INSERTs on an admin session for those two, matching the row
/// shape the graph engine would persist.
async fn seed_minimal_fixture(tenant_id: Uuid, session_origin: &str) -> (Uuid, Uuid, Uuid, Uuid) {
    let cfg = local_cluster_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("seed_minimal_fixture: CqlStorage::connect");
    let ctx = TenantContext {
        tenant_id,
        session_origin: session_origin.into(),
    };
    let session_id = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let now = chrono::Utc::now();
    for (entity_id, name) in [(a, "seed-a"), (b, "seed-b")] {
        storage
            .entity_put(
                &ctx,
                &EntityEntry {
                    tenant_id: ctx.tenant_id,
                    session_id,
                    entity_id,
                    entity_name: name.into(),
                    entity_type: "concept".into(),
                    confidence: 1.0,
                    created_at: now,
                    ..Default::default()
                },
            )
            .await
            .expect("seed_minimal_fixture: entity_put");
    }
    // typed_edges and co_occurs_with are graph-annotated; the Storage
    // trait blocks direct writes by design. For a *fixture* the rows
    // legitimately need to exist on disk so the streaming/count paths
    // have something to read — issue them via raw CQL on the admin
    // session, matching the schema the graph engine would persist.
    let admin = connect_plain("127.0.0.1:19042").await;
    let created_ts = scylla::frame::value::CqlTimestamp(now.timestamp_millis());
    #[allow(deprecated)]
    admin
        .query_unpaged(
            "INSERT INTO agent_memory.typed_edges \
             (tenant_id, session_id, src_id, edge_type, dst_id, weight, metadata, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            (
                ctx.tenant_id,
                session_id,
                a,
                "TAGGED_AS",
                b,
                0.5f64,
                None::<String>,
                created_ts,
            ),
        )
        .await
        .expect("seed_minimal_fixture: typed_edges raw insert");
    #[allow(deprecated)]
    admin
        .query_unpaged(
            "INSERT INTO agent_memory.co_occurs_with \
             (entity_a, entity_b, session_id, tenant_id, created_at) \
             VALUES (?, ?, ?, ?, ?)",
            (a, b, session_id, ctx.tenant_id, created_ts),
        )
        .await
        .expect("seed_minimal_fixture: co_occurs_with raw insert");

    (ctx.tenant_id, session_id, a, b)
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn derived_cache_count_streams_past_one_hundred_thousand_live_rows() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        eprintln!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             /Users/bkearns/src/ferrosa-suite/ferrosa-memory before this live test"
        );
        return;
    }

    init_test_tracing();
    let cfg = local_cluster_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect");
    let ctx = TenantContext {
        tenant_id: Uuid::parse_str("9a5f8fbf-d842-4d30-8ea5-1aa931e618a8").unwrap(),
        session_origin: "live-count-regression".into(),
    };

    let storage_count = storage
        .derived_cache_count(&ctx)
        .await
        .expect("derived_cache_count should stream all live pages");

    let raw = connect_plain("127.0.0.1:19042").await;
    let query = "SELECT cache_key, seq FROM agent_memory.derived_cache_by_query \
                 WHERE tenant_id = ? ALLOW FILTERING";
    let mut iter = raw
        .query_iter(query, (ctx.tenant_id,))
        .await
        .expect("raw derived_cache query_iter");
    let mut raw_count = 0usize;
    while let Some(row) = iter.next().await {
        row.expect("raw derived_cache row");
        raw_count += 1;
    }

    assert!(
        raw_count > 100_000,
        "live fixture must exercise the historical 100k page/cap boundary; raw_count={raw_count}"
    );
    assert_eq!(
        storage_count, raw_count,
        "derived_cache_count must stream every CQL page, not report a rounded cap"
    );
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

    // confidence_scores/typed_edges/co_occurs_with COUNT(*)s are unscoped,
    // so the tenant_id doesn't have to match anything specific.
    let _ = seed_minimal_fixture(Uuid::new_v4(), "confidence-prepare-live-test").await;

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

    // entity_stream_all and typed_edge_stream_all bind ctx.tenant_id, so
    // the fixture's rows MUST live under the same tenant the test reads.
    let viz_tenant = Uuid::parse_str("9a5f8fbf-d842-4d30-8ea5-1aa931e618a8").unwrap();
    let (_, _, seed_a, seed_b) = seed_minimal_fixture(viz_tenant, "viz-stream-live-test").await;

    let storage = Arc::new(
        CqlStorage::connect(&cfg)
            .await
            .expect("CqlStorage::connect should succeed on the auth-enabled local cluster"),
    );
    let ctx = TenantContext {
        tenant_id: viz_tenant,
        session_origin: "viz-live-test".to_string(),
    };

    // Assert the *specific* seeded entities and edge are visible — not
    // just "some row exists". A bare `count > 0` would silently pass on
    // a cluster that already has unrelated rows under this tenant, which
    // is exactly how an earlier fixture mismatch slipped past local runs.
    let (node_tx, mut node_rx) = tokio::sync::mpsc::channel::<anyhow::Result<Vec<EntityEntry>>>(4);
    let node_storage = storage.clone();
    let node_ctx = ctx.clone();
    tokio::spawn(async move {
        node_storage.entity_stream_all(node_ctx, 128, node_tx).await;
    });
    let mut seen_ids: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    while let Some(chunk) = node_rx.recv().await {
        let chunk = chunk.expect("entity stream chunk should decode");
        for e in chunk {
            seen_ids.insert(e.entity_id);
        }
        if seen_ids.contains(&seed_a) && seen_ids.contains(&seed_b) {
            break;
        }
    }
    assert!(
        seen_ids.contains(&seed_a) && seen_ids.contains(&seed_b),
        "viz entity stream must yield the seeded entities ({seed_a}, {seed_b}); saw {} ids",
        seen_ids.len(),
    );

    let (edge_tx, mut edge_rx) = tokio::sync::mpsc::channel::<anyhow::Result<Vec<TypedEdge>>>(4);
    let edge_storage = storage.clone();
    tokio::spawn(async move {
        edge_storage.typed_edge_stream_all(ctx, 128, edge_tx).await;
    });
    let mut seen_edge = false;
    while let Some(chunk) = edge_rx.recv().await {
        let chunk = chunk.expect("typed-edge stream chunk should decode");
        if chunk
            .iter()
            .any(|e| e.src_id == seed_a && e.dst_id == seed_b)
        {
            seen_edge = true;
            break;
        }
    }
    assert!(
        seen_edge,
        "viz typed-edge stream must yield the seeded edge ({seed_a} -> {seed_b})",
    );
}
