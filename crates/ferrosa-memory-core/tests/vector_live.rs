// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Live test: vector DDL/PREPARE compatibility.
//! Run: cargo test -p ferrosa-memory-core --test vector_live -- --ignored --nocapture
//!
//! The scylla 0.15 driver used by ferrosa-memory can prepare vector statements
//! but does not expose a Rust value serializer for `vector<float, N>`; a
//! `Vec<u8>` is correctly rejected because the live prepared column type is
//! VectorType, not Blob. This smoke verifies Ferrosa's vector table,
//! insert-prepare, ANN prepare, and type-checking surface without pretending
//! blob serialization is a vector roundtrip.

use scylla::{LegacySession, SessionBuilder};

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
async fn vector_prepare_and_typecheck_smoke() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    let session = connect_plain("127.0.0.1:19042").await;

    #[allow(deprecated)]
    session
        .query_unpaged(
            "CREATE TABLE IF NOT EXISTS agent_memory.test_vector_blob \
             (id uuid PRIMARY KEY, embedding vector<float, 4>)",
            (),
        )
        .await
        .expect("CREATE TABLE");

    let insert = session
        .prepare("INSERT INTO agent_memory.test_vector_blob (id, embedding) VALUES (?, ?)")
        .await
        .expect("PREPARE vector INSERT");

    session
        .prepare("SELECT id FROM agent_memory.test_vector_blob ORDER BY embedding ANN OF ? LIMIT 5")
        .await
        .expect("PREPARE ANN SELECT");

    let blob_insert = session
        .execute_unpaged(&insert, (uuid::Uuid::new_v4(), vec![0_u8; 16]))
        .await;
    let err = blob_insert.expect_err("Vec<u8> must not serialize as vector<float, 4>");
    let err_text = err.to_string();
    assert!(
        err_text.contains("VectorType") && err_text.contains("Blob"),
        "expected vector/blob type mismatch, got {err_text}"
    );

    eprintln!("Vector prepare/typecheck smoke PASSED");
}
