// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Live test: vector DDL/PREPARE compatibility.
//! Run: cargo test -p ferrosa-memory-core --test vector_live -- --ignored --nocapture
//!
//! The ferrosadb scylla 0.15 fork accepts `Vec<u8>` / `&[u8]` bindings against
//! Cassandra 5.0 `vector<inner, dim>` columns (CEP-30 advertises them as
//! `Custom("...VectorType(...)")`). The wire format is exactly `dim`
//! fixed-size big-endian elements with no per-element length prefix, which is
//! the same byte layout an application produces when it packs an embedding
//! into a `Vec<u8>`. This smoke verifies the table DDL, INSERT/ANN PREPARE,
//! and end-to-end byte-aligned INSERT against a real `vector<float, 4>`.

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

    // 4 dims × 4 bytes per float = 16 byte payload aligned with the column
    // declaration. The fork accepts Vec<u8> as a binding for VectorType; the
    // server rejects mismatched sizes, so we cover both arms below.
    session
        .execute_unpaged(&insert, (uuid::Uuid::new_v4(), vec![0_u8; 16]))
        .await
        .expect("Vec<u8> with dim*sizeof(float) bytes must serialize as vector<float, 4>");

    // Mismatched length: 12 bytes != 4 floats. Server-side validation must
    // reject — the driver type-check is intentionally permissive (no
    // per-binding dim-aware length check), so the failure surfaces from
    // ferrosa, not the client.
    let wrong_len = session
        .execute_unpaged(&insert, (uuid::Uuid::new_v4(), vec![0_u8; 12]))
        .await;
    assert!(
        wrong_len.is_err(),
        "12-byte payload must be rejected against vector<float, 4>; got Ok"
    );

    eprintln!("Vector prepare/typecheck smoke PASSED");
}
