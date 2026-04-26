// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Live test: vector INSERT/SELECT via blob workaround.
//! Run: cargo test -p ferrosa-memory-core --test vector_live -- --ignored --nocapture

use ferrosa_memory_core::cql_storage::build_col_map;
use ferrosa_memory_core::vector;
use scylla::{LegacySession, SessionBuilder};

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
async fn vector_blob_workaround_roundtrip() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    let session = connect_plain("127.0.0.1:19042").await;

    // Create table with vector column
    #[allow(deprecated)]
    session
        .query_unpaged(
            "CREATE TABLE IF NOT EXISTS agent_memory.test_vector_blob \
             (id uuid PRIMARY KEY, embedding vector<float, 4>)",
            (),
        )
        .await
        .expect("CREATE TABLE");

    // Encode vector as raw bytes — scylla accepts Vec<u8> for blob columns
    let embedding = vec![0.1_f32, 0.2, 0.3, 0.4];
    let blob_bytes: Vec<u8> = vector::encode_vector(&embedding);
    let id = uuid::Uuid::new_v4();

    // INSERT using blob bytes — the VECTOR column should accept raw bytes
    let prepared = session
        .prepare("INSERT INTO agent_memory.test_vector_blob (id, embedding) VALUES (?, ?)")
        .await
        .expect("PREPARE INSERT");

    #[allow(deprecated)]
    session
        .execute_unpaged(&prepared, (id, blob_bytes))
        .await
        .expect("INSERT with vector as blob");

    eprintln!("INSERT succeeded");

    // Read back
    let read = session
        .prepare("SELECT embedding FROM agent_memory.test_vector_blob WHERE id = ?")
        .await
        .expect("PREPARE SELECT");

    #[allow(deprecated)]
    let result = session.execute_unpaged(&read, (id,)).await.expect("SELECT");

    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    assert_eq!(rows.len(), 1);

    // Read vector as blob bytes — column 0 is embedding (only column selected)
    let raw: Vec<u8> =
        ferrosa_memory_core::cql_storage::cql_get::<Vec<u8>>(&rows[0], &col_map, "embedding")
            .expect("read vector by name as blob");
    let decoded = vector::decode_vector(&raw);

    eprintln!("Decoded: {:?}", decoded);
    assert_eq!(decoded.len(), 4);
    for (a, b) in embedding.iter().zip(decoded.iter()) {
        assert!((a - b).abs() < 1e-6, "mismatch: {} vs {}", a, b);
    }

    eprintln!("Vector blob roundtrip PASSED!");
}
