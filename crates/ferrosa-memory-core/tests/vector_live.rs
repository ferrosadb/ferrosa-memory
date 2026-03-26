//! Live test: vector INSERT/SELECT via blob workaround.
//! Run: cargo test -p ferrosa-memory-core --test vector_live -- --ignored --nocapture

use std::sync::Arc;

use cdrs_tokio::authenticators::NoneAuthenticatorProvider;
use cdrs_tokio::cluster::NodeTcpConfigBuilder;
use cdrs_tokio::cluster::session::{SessionBuilder, TcpSessionBuilder};
use cdrs_tokio::load_balancing::RoundRobinLoadBalancingStrategy;
use cdrs_tokio::query_values;
use cdrs_tokio::types::blob::Blob;
use ferrosa_memory_core::vector;

#[tokio::test]
#[ignore]
async fn vector_blob_workaround_roundtrip() {
    let nc = NodeTcpConfigBuilder::new()
        .with_contact_point("127.0.0.1:19042".into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .unwrap();
    let session = TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), nc)
        .build()
        .await
        .unwrap();

    // Create table with vector column
    session
        .query(
            "CREATE TABLE IF NOT EXISTS agent_memory.test_vector_blob \
             (id uuid PRIMARY KEY, embedding vector<float, 4>)",
        )
        .await
        .expect("CREATE TABLE");

    // Encode vector as raw bytes wrapped in Blob
    let embedding = vec![0.1_f32, 0.2, 0.3, 0.4];
    let blob = Blob::new(vector::encode_vector(&embedding));
    let id = uuid::Uuid::new_v4();

    // INSERT using blob bytes — the VECTOR column should accept raw bytes
    let prepared = session
        .prepare("INSERT INTO agent_memory.test_vector_blob (id, embedding) VALUES (?, ?)")
        .await
        .expect("PREPARE INSERT");

    session
        .exec_with_values(&prepared, query_values!(id, blob))
        .await
        .expect("INSERT with vector as blob");

    eprintln!("INSERT succeeded");

    // Read back
    let read = session
        .prepare("SELECT embedding FROM agent_memory.test_vector_blob WHERE id = ?")
        .await
        .expect("PREPARE SELECT");

    let envelope = session
        .exec_with_values(&read, query_values!(id))
        .await
        .expect("SELECT");

    let rows = envelope.response_body().unwrap().into_rows().unwrap();
    assert_eq!(rows.len(), 1);

    // Read vector using ByIndex — column 0 is embedding (only column selected)
    use cdrs_tokio::types::ByIndex;
    let raw: Blob = rows[0].r_by_index(0).expect("read vector by index as blob");
    let decoded = vector::decode_vector(&raw.into_vec());

    eprintln!("Decoded: {:?}", decoded);
    assert_eq!(decoded.len(), 4);
    for (a, b) in embedding.iter().zip(decoded.iter()) {
        assert!((a - b).abs() < 1e-6, "mismatch: {} vs {}", a, b);
    }

    eprintln!("Vector blob roundtrip PASSED!");
}
