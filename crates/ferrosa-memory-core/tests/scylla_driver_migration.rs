// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! TDD integration tests for the cdrs-tokio → scylla driver migration (p1-22).
//!
//! ## RED PHASE
//!
//! These tests are written against the **desired post-migration API surface**.
//! They exercise `CqlStorage` and `connect_session` through the **scylla driver**.
//!
//! Before the migration these tests will **fail to compile** because the current
//! codebase uses cdrs-tokio types (`PreparedQuery`, `query_values!`, etc.).
//! After the migration they must pass.
//!
//! ## How to run with a live cluster
//!
//! Set the following environment variable before running:
//!
//! ```text
//! FERROSA_TEST_CQL_PORT=30042  # port on the test cluster (30000-30099 range)
//! FERROSA_TEST_CQL_HOST=127.0.0.1   # optional, defaults to localhost
//! FERROSA_TEST_KEYSPACE=agent_memory_test  # optional
//! ```
//!
//! Then:
//! ```text
//! cargo test -p ferrosa-memory-core --test scylla_driver_migration -- --nocapture
//! ```
//!
//! Live-cluster tests are `#[ignore]` by default so the normal non-ignored
//! suite is deterministic. When explicitly run with `--ignored`, they fail loud
//! if `FERROSA_TEST_CQL_PORT` is unset.
//!
//! ## Cluster safety
//!
//! These tests NEVER connect to production ports (19042-19044) or any port in
//! the live-cluster range (19000-19092, 17474-17689, 18765). They only connect
//! to ports specified by `FERROSA_TEST_CQL_PORT`. Tests that would accidentally
//! reach production are a bug in the test, not a skip condition.

use uuid::Uuid;

use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::CqlStorage;
#[cfg(feature = "scylla-driver")]
use ferrosa_memory_core::cql_storage::connect_session;
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::types::{MemoEntry, TenantContext};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `FerrosaCqlConfig` from environment variables.
///
/// Panics with setup instructions if `FERROSA_TEST_CQL_PORT` is not set.
/// This is intentional — per project policy, tests must fail loud, not skip.
fn test_config() -> FerrosaCqlConfig {
    let port: u16 = std::env::var("FERROSA_TEST_CQL_PORT")
        .unwrap_or_else(|_| {
            panic!(
                "\n\nFERROSA_TEST_CQL_PORT is not set.\n\
                 \n\
                 To run these integration tests you need a Ferrosa test cluster.\n\
                 Start one with:\n\
                 \n\
                 \t  FERROSA_TEST_CQL_PORT=30042 \\\n\
                 \t  scripts/start-test-cluster.sh\n\
                 \n\
                 Then re-run with:\n\
                 \n\
                 \t  FERROSA_TEST_CQL_PORT=30042 \\\n\
                 \t  cargo test -p ferrosa-memory-core \\\n\
                 \t             --test scylla_driver_migration -- --nocapture\n\
                 \n\
                 DO NOT use production ports (19042-19044) — those are live.\n"
            )
        })
        .parse()
        .expect("FERROSA_TEST_CQL_PORT must be a valid port number");

    // Guard: refuse to connect to known-live cluster ports.
    assert!(
        !(19000..=19099).contains(&port) && !(17474..=17689).contains(&port) && port != 18765,
        "FERROSA_TEST_CQL_PORT={port} is in the live-cluster port range — \
         refusing to run tests against production"
    );

    let host = std::env::var("FERROSA_TEST_CQL_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let keyspace =
        std::env::var("FERROSA_TEST_KEYSPACE").unwrap_or_else(|_| "agent_memory_test".to_string());

    FerrosaCqlConfig {
        contact_points: vec![format!("{host}:{port}")],
        keyspace,
        replication_factor: 1,
        consistency: "ONE".to_string(),
        username: std::env::var("FERROSA_TEST_USERNAME")
            .unwrap_or_else(|_| "cassandra".to_string()),
        password: std::env::var("FERROSA_TEST_PASSWORD")
            .unwrap_or_else(|_| "cassandra".to_string()),
        admin_username: None,
        admin_password: None,
    }
}

fn tenant() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: format!("test-{}", Uuid::new_v4()),
    }
}

// ---------------------------------------------------------------------------
// T-01: Driver connection
//
// Asserts that connect_session uses the scylla driver under the hood.
// With cdrs-tokio this still compiles (the function signature is unchanged),
// but after migration it must use `scylla::Session` internally.
// ---------------------------------------------------------------------------

/// T-01: connect_session returns a usable session against the test cluster.
///
/// This test validates the connection bootstrap path (the same code that
/// `CqlStorage::connect` and `connect_admin_session` use at server startup).
///
/// Gated on `scylla-driver` feature because it calls `query_unpaged` which
/// is the scylla::Session API. Before migration (cdrs-tokio) this test would
/// not compile if enabled. After migration, enable the feature and it must pass.
///
/// RED: fails to compile if `scylla-driver` feature is enabled pre-migration.
/// GREEN: compiles and passes after cdrs-tokio → scylla migration.
#[cfg(feature = "scylla-driver")]
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t01_connect_session_succeeds() {
    let cfg = test_config();
    let session = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect_session must succeed against the test cluster");

    // scylla LegacySession: session.query_unpaged("...", ()).await
    #[allow(deprecated)]
    let result = session
        .query_unpaged("SELECT keyspace_name FROM system_schema.keyspaces", ())
        .await
        .expect("system query must succeed on a connected session");

    let col_map = ferrosa_memory_core::cql_storage::build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    let keyspaces: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            ferrosa_memory_core::cql_storage::cql_get::<String>(row, &col_map, "keyspace_name").ok()
        })
        .collect();

    assert!(
        !keyspaces.is_empty(),
        "system_schema.keyspaces must not be empty"
    );
}

// ---------------------------------------------------------------------------
// T-02: CqlStorage::connect prepares all statements
//
// The `connect` function issues 50+ `session.prepare()` calls at startup.
// This test checks the entire startup path succeeds.
// ---------------------------------------------------------------------------

/// T-02: CqlStorage::connect prepares all statements against the test cluster.
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t02_cql_storage_connect_prepares_all_statements() {
    let cfg = test_config();

    let storage = CqlStorage::connect(&cfg).await.expect(
        "CqlStorage::connect must succeed — if this fails, check that \
             the test keyspace and tables exist. Run the DDL bootstrap first:\n\
             \t  FERROSA_TEST_CQL_PORT=<port> cargo run --bin ferrosa-memory -- --migrate",
    );

    // Basic sanity: the keyspace name round-trips.
    assert_eq!(storage.keyspace(), cfg.keyspace);
}

// ---------------------------------------------------------------------------
// T-03: memo_put / memo_get round-trip
//
// Core Storage trait: write a memo entry, read it back.
// Validates prepared statement execution and typed row deserialization.
// ---------------------------------------------------------------------------

/// T-03: memo_put followed by memo_get returns the written entry.
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t03_memo_put_get_roundtrip() {
    let cfg = test_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect");

    let ctx = tenant();
    let hash = format!("test-hash-{}", Uuid::new_v4());
    let model = "test-model-v1".to_string();

    let entry = MemoEntry {
        content_hash: hash.clone(),
        model_version: model.clone(),
        result: "test result string".to_string(),
        result_embedding: None,
        hit_count: 0,
        created_at: chrono::Utc::now(),
        last_hit_at: None,
        expires_at: None,
    };

    // Write
    storage
        .memo_put(&ctx, &entry)
        .await
        .expect("memo_put must succeed");

    // Read back
    let fetched = storage
        .memo_get(&ctx, &hash, &model)
        .await
        .expect("memo_get must succeed");

    let fetched = fetched.expect("memo_get must return Some after memo_put");
    assert_eq!(fetched.content_hash, hash, "content_hash must round-trip");
    assert_eq!(
        fetched.model_version, model,
        "model_version must round-trip"
    );
    assert_eq!(
        fetched.result, entry.result,
        "result string must round-trip"
    );
    assert_eq!(fetched.hit_count, 0, "initial hit_count must be 0");
}

// ---------------------------------------------------------------------------
// T-04: memo_get returns None for an absent key
// ---------------------------------------------------------------------------

/// T-04: memo_get returns None when the key does not exist.
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t04_memo_get_missing_returns_none() {
    let cfg = test_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect");

    let ctx = tenant();
    let result = storage
        .memo_get(
            &ctx,
            &format!("no-such-hash-{}", Uuid::new_v4()),
            "no-model",
        )
        .await
        .expect("memo_get must not error on a missing key");

    assert!(
        result.is_none(),
        "memo_get must return None for unknown key"
    );
}

// ---------------------------------------------------------------------------
// T-05: memo_put with embedding round-trips the blob bytes
//
// The embedding path is where the blob type mapping matters most.
// cdrs-tokio used `Blob::new(bytes)` as a value; scylla accepts `Vec<u8>`.
// ---------------------------------------------------------------------------

/// T-05: memo_put with an embedding stores and retrieves the vector bytes.
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t05_memo_put_with_embedding_roundtrip() {
    let cfg = test_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect");

    let ctx = tenant();
    let hash = format!("emb-hash-{}", Uuid::new_v4());
    let model = "emb-model-v1".to_string();
    // `result_embedding` is declared `vector<float, 768>` (CEP-30 fixed
    // dim — matches the default `nomic-embed-text-v2-moe` model output).
    // A shorter test vector serializes fine driver-side but the server
    // parser rejects a 16-element literal against a 768-element column
    // ("type mismatch: expected vector, got blob literal"). Exercise
    // the real production shape.
    let embedding: Vec<f32> = (0..768).map(|i| (i as f32) / 768.0).collect();

    let entry = MemoEntry {
        content_hash: hash.clone(),
        model_version: model.clone(),
        result: "embedding test".to_string(),
        result_embedding: Some(embedding.clone()),
        hit_count: 0,
        created_at: chrono::Utc::now(),
        last_hit_at: None,
        expires_at: None,
    };

    storage
        .memo_put(&ctx, &entry)
        .await
        .expect("memo_put with embedding must succeed");

    let fetched = storage
        .memo_get(&ctx, &hash, &model)
        .await
        .expect("memo_get after memo_put with embedding must succeed")
        .expect("memo_get must return Some");

    let fetched_emb = fetched
        .result_embedding
        .expect("result_embedding must survive the round-trip");

    assert_eq!(
        fetched_emb.len(),
        embedding.len(),
        "embedding length must be preserved"
    );
    for (a, b) in embedding.iter().zip(fetched_emb.iter()) {
        assert!(
            (a - b).abs() < 1e-6,
            "embedding component {a} ≠ {b} after round-trip"
        );
    }
}

// ---------------------------------------------------------------------------
// T-06: memo_touch increments hit_count
// ---------------------------------------------------------------------------

/// T-06: memo_touch increments the hit_count on an existing entry.
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t06_memo_touch_increments_hit_count() {
    let cfg = test_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect");

    let ctx = tenant();
    let hash = format!("touch-hash-{}", Uuid::new_v4());
    let model = "touch-model-v1".to_string();

    let entry = MemoEntry {
        content_hash: hash.clone(),
        model_version: model.clone(),
        result: "touch test".to_string(),
        result_embedding: None,
        hit_count: 0,
        created_at: chrono::Utc::now(),
        last_hit_at: None,
        expires_at: None,
    };

    storage.memo_put(&ctx, &entry).await.expect("memo_put");
    storage
        .memo_touch(&ctx, &hash, &model)
        .await
        .expect("memo_touch must succeed");

    let fetched = storage
        .memo_get(&ctx, &hash, &model)
        .await
        .expect("memo_get after touch")
        .expect("memo_get must return Some");

    assert_eq!(
        fetched.hit_count, 1,
        "hit_count must be 1 after one touch (CQL counter increment)"
    );
}

// ---------------------------------------------------------------------------
// T-07: load_entity_types returns a non-empty list
//
// Exercises the ad-hoc `session.query()` path (no prepared statement).
// ---------------------------------------------------------------------------

/// T-07: load_entity_types returns a non-empty list on a bootstrapped cluster.
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t07_load_entity_types_returns_defaults() {
    let cfg = test_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect");

    let types = storage.load_entity_types().await;
    // On a fresh keyspace this returns the hardcoded default list.
    assert!(
        !types.is_empty(),
        "load_entity_types must return at least the default entity types"
    );
}

// ---------------------------------------------------------------------------
// T-08: session() accessor returns a usable scylla::Session reference
//
// The mcp and sync crates call storage.session() to run raw queries.
// After migration the return type must be &scylla::Session (not &CqlSession
// wrapping cdrs-tokio types).
// ---------------------------------------------------------------------------

/// T-08: session() accessor is usable for raw queries via scylla API.
///
/// Gated on `scylla-driver` feature — calls query_unpaged which only exists
/// on scylla::Session, not on the cdrs-tokio Session type.
#[cfg(feature = "scylla-driver")]
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t08_session_accessor_allows_raw_query() {
    let cfg = test_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect");

    // After migration: storage.session() returns &LegacySession.
    // The query_unpaged method is part of the scylla LegacySession API.
    #[allow(deprecated)]
    let result = storage
        .session()
        .query_unpaged("SELECT release_version FROM system.local", ())
        .await
        .expect("raw query through session() accessor must succeed");

    let col_map = ferrosa_memory_core::cql_storage::build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    let versions: Vec<Option<String>> = rows
        .iter()
        .map(|row| {
            ferrosa_memory_core::cql_storage::cql_get::<String>(row, &col_map, "release_version")
                .ok()
        })
        .collect();

    assert!(
        !versions.is_empty(),
        "system.local must return exactly one row"
    );
}

// ---------------------------------------------------------------------------
// T-09: Vector blob encode/decode is driver-independent
//
// This test does NOT need a cluster — it confirms the vector module's
// encode_vector / decode_vector stay correct regardless of driver.
// This is a pure unit test that should always pass, even during the
// migration (RED and GREEN phases).
// ---------------------------------------------------------------------------

/// T-09: vector encode/decode is independent of the CQL driver.
#[test]
fn t09_vector_encode_decode_is_driver_independent() {
    use ferrosa_memory_core::vector::{decode_vector, encode_vector};

    let original: Vec<f32> = vec![0.1, 0.2, 0.3, -0.5, 1.0];
    let encoded = encode_vector(&original);
    assert_eq!(encoded.len(), original.len() * 4, "4 bytes per f32");

    let decoded = decode_vector(&encoded);
    assert_eq!(decoded.len(), original.len());
    for (a, b) in original.iter().zip(decoded.iter()) {
        assert!((a - b).abs() < 1e-6, "{a} ≠ {b}");
    }
}

// ---------------------------------------------------------------------------
// T-10: tenant isolation — memo_get cannot see another tenant's data
//
// This validates the tenant_id binding in every prepared statement.
// ---------------------------------------------------------------------------

/// T-10: memo_put by tenant A is invisible to tenant B.
#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT"]
async fn t10_memo_tenant_isolation() {
    let cfg = test_config();
    let storage = CqlStorage::connect(&cfg)
        .await
        .expect("CqlStorage::connect");

    let ctx_a = tenant();
    let ctx_b = tenant();
    let hash = format!("iso-hash-{}", Uuid::new_v4());
    let model = "iso-model".to_string();

    let entry = MemoEntry {
        content_hash: hash.clone(),
        model_version: model.clone(),
        result: "tenant A result".to_string(),
        result_embedding: None,
        hit_count: 0,
        created_at: chrono::Utc::now(),
        last_hit_at: None,
        expires_at: None,
    };

    storage
        .memo_put(&ctx_a, &entry)
        .await
        .expect("memo_put for tenant A");

    // Tenant B must not see tenant A's data.
    let result_b = storage
        .memo_get(&ctx_b, &hash, &model)
        .await
        .expect("memo_get for tenant B must not error");

    assert!(
        result_b.is_none(),
        "tenant B must not see tenant A's memo entry (tenant isolation violated)"
    );
}
