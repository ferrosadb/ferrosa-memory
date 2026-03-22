//! Ferrosa compatibility bug reproduction tests.
//!
//! These tests document known Ferrosa/cdrs-tokio issues that block
//! specific features. Each test is #[ignore] and will FAIL until the
//! upstream bug is fixed. Run with:
//!
//!   cargo test -p ferrosa-core --test ferrosa_bugs -- --ignored --nocapture

use std::sync::Arc;

use cdrs_tokio::authenticators::NoneAuthenticatorProvider;
use cdrs_tokio::cluster::NodeTcpConfigBuilder;
use cdrs_tokio::cluster::session::{SessionBuilder, TcpSessionBuilder};
use cdrs_tokio::load_balancing::RoundRobinLoadBalancingStrategy;
use cdrs_tokio::query_values;
use cdrs_tokio::types::ByName;

// ---------------------------------------------------------------------------
// BUG 1: vector<float, N> CQL type not serializable via cdrs-tokio
//
// Ferrosa implements vector<float, N> (Cassandra 5.0 spec, commit a9a7e43).
// cdrs-tokio v9 has partial vector support in cassandra-protocol v4 but
// the INSERT/SELECT round-trip fails because:
//   a) INSERT: Vec<f32> doesn't serialize to the VECTOR wire format
//   b) SELECT: VECTOR column deserialization is untested against Ferrosa
//
// Impact: All embedding columns stored as NULL. ANN queries
//   (ORDER BY embedding ANN OF ?) non-functional. fold_search and
//   entity_search_ann return empty. Semantic search completely broken.
//
// Blocked on: cdrs-tokio PR for VECTOR type serialization, or a custom
//   serializer in ferrosa-memory-mcp.
//
// Severity: CRITICAL (FMEA F31, RPN 180)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn bug_vector_type_insert_roundtrip() {
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

    // Create a table with a vector column
    session
        .query(
            "CREATE TABLE IF NOT EXISTS agent_memory.test_vector \
             (id uuid PRIMARY KEY, embedding vector<float, 4>)",
        )
        .await
        .expect("CREATE TABLE with vector column");

    // INSERT a vector value
    let embedding: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4];
    let id = uuid::Uuid::new_v4();

    // This is where it will fail — Vec<f32> doesn't serialize to VECTOR wire format
    let prepared = session
        .prepare("INSERT INTO agent_memory.test_vector (id, embedding) VALUES (?, ?)")
        .await
        .expect("PREPARE with vector column");

    session
        .exec_with_values(&prepared, query_values!(id, embedding))
        .await
        .expect("INSERT with vector value — EXPECTED TO FAIL until cdrs-tokio supports VECTOR serialization");

    // Read it back
    let read_prepared = session
        .prepare("SELECT embedding FROM agent_memory.test_vector WHERE id = ?")
        .await
        .expect("PREPARE SELECT vector");

    let envelope = session
        .exec_with_values(&read_prepared, query_values!(id))
        .await
        .expect("SELECT vector value");

    let rows = envelope.response_body().unwrap().into_rows().unwrap();
    assert_eq!(rows.len(), 1);

    // Deserialize vector — can't even compile because Row doesn't implement
    // IntoRustByName<Vec<f32>>. This IS the bug: cdrs-tokio has no VECTOR deserialization.
    //
    // let row = &rows[0];
    // let _result: Vec<f32> = row.r_by_name("embedding").expect("deserialize vector");
    //
    // When this compiles and passes, BUG-1 is fixed.
    eprintln!("Vector INSERT succeeded but deserialization not yet possible");
}

#[tokio::test]
#[ignore]
async fn bug_vector_ann_query() {
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

    // Requires: test_vector table with HNSW index and data
    // This tests ORDER BY ... ANN OF ? syntax
    let prepared = session
        .prepare(
            "SELECT id FROM agent_memory.test_vector \
             ORDER BY embedding ANN OF ? LIMIT 5",
        )
        .await
        .expect("PREPARE ANN query — may fail if Ferrosa doesn't support ANN syntax in prepared statements");

    let query_vec: Vec<f32> = vec![0.15, 0.25, 0.35, 0.45];

    let envelope = session
        .exec_with_values(&prepared, query_values!(query_vec))
        .await
        .expect("EXECUTE ANN query");

    let rows = envelope.response_body().unwrap().into_rows().unwrap();
    // Should return results ordered by cosine similarity
    eprintln!("ANN query returned {} rows", rows.len());
}

// ---------------------------------------------------------------------------
// BUG 2: SUBSCRIBE for real-time change streams
//
// Ferrosa supports SUBSCRIBE SELECT ... DELTA for real-time streaming of
// table mutations. ferrosa-memory-mcp needs this for real-time anomaly
// alerting (spec Section 9.3):
//
//   SUBSCRIBE SELECT * FROM system_observability.memory_summary
//   WHERE tenant_id = ? DELTA;
//
// This would enable live alerting on memory poisoning detection events
// and cache efficiency regressions without polling.
//
// Impact: Anomaly detection is batch-only (no real-time alerts).
//   Memory poisoning attacks detected with delay.
//
// Blocked on: Testing SUBSCRIBE via cdrs-tokio. The protocol extension
//   may require a custom frame handler since SUBSCRIBE is not standard CQL.
//
// Severity: MEDIUM (nice-to-have for v1, required for v2)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn bug_subscribe_change_stream() {
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

    // Attempt SUBSCRIBE — will likely fail since cdrs-tokio doesn't
    // have a SUBSCRIBE opcode handler
    let result = session
        .query("SUBSCRIBE SELECT * FROM agent_memory.memo_cache DELTA")
        .await;

    match result {
        Ok(envelope) => {
            eprintln!("SUBSCRIBE returned a response");
            // If this works, we need to implement a streaming reader
        }
        Err(e) => {
            eprintln!("SUBSCRIBE failed (expected): {e}");
            // Document what error Ferrosa returns
            panic!(
                "SUBSCRIBE not supported yet. Error: {e}\n\
                 This test will pass when SUBSCRIBE is wired through cdrs-tokio."
            );
        }
    }
}

// ---------------------------------------------------------------------------
// BUG 3: COUNT(*) column name mismatch (WORKAROUND IN PLACE)
//
// Ferrosa returns COUNT(*) result column as "system.count" instead of "count".
// cdrs-tokio's r_by_name("count") fails to find it.
//
// Current workaround: entity_count uses SELECT entity_id + client-side len()
// instead of COUNT(*).
//
// Ferrosa fix: commit 523483e "fix(cql): COUNT(*) column name should be
// 'count' not 'system.count'" — but as of testing, the column is still
// returned as "system.count" in some cases.
//
// Severity: LOW (workaround in place)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn bug_count_column_name() {
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

    let prepared = session
        .prepare(
            "SELECT COUNT(*) FROM agent_memory.entity_store \
             WHERE tenant_id = ? AND session_id = ?",
        )
        .await
        .expect("PREPARE COUNT(*)");

    let envelope = session
        .exec_with_values(
            &prepared,
            query_values!(uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
        )
        .await
        .expect("EXECUTE COUNT(*)");

    let rows = envelope.response_body().unwrap().into_rows().unwrap();
    assert_eq!(rows.len(), 1);

    // This should work — column should be named "count", not "system.count"
    let count: i64 = rows[0]
        .r_by_name("count")
        .expect("COUNT(*) column should be named 'count'");
    assert_eq!(count, 0);
}

// ---------------------------------------------------------------------------
// BUG 4: Phonetic index (Double Metaphone) not tested
//
// The spec calls for Ferrosa's phonetic index on entity_name for fuzzy
// name matching. The DDL includes:
//
//   CREATE INDEX idx_entity_name_phonetic ON entity_store (entity_name)
//       USING 'phonetic' WITH OPTIONS = {'algorithm': 'double_metaphone'};
//
// Currently using case-insensitive string match as fallback because:
//   a) The index DDL isn't applied (vector columns blocked it earlier)
//   b) cdrs-tokio query syntax for phonetic search is unknown
//
// Impact: Entity deduplication only catches exact case-insensitive matches.
//   "Jon Smith" and "John Smyth" would be treated as different entities.
//
// Severity: MEDIUM (reduces entity deduplication quality)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn bug_phonetic_index_query() {
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

    // First, create the phonetic index
    let index_result = session
        .query(
            "CREATE INDEX IF NOT EXISTS idx_entity_name_phonetic \
             ON agent_memory.entity_store (entity_name) \
             USING 'phonetic' \
             WITH OPTIONS = {'algorithm': 'double_metaphone'}",
        )
        .await;

    match index_result {
        Ok(_) => eprintln!("Phonetic index created"),
        Err(e) => {
            panic!(
                "Phonetic index creation failed: {e}\nFerrosa may not support phonetic indexes yet."
            );
        }
    }

    // Insert test entities
    session
        .query(
            "INSERT INTO agent_memory.entity_store \
             (tenant_id, session_id, entity_id, entity_name, entity_type, context_snippet, confidence, created_at) \
             VALUES (550e8400-e29b-41d4-a716-446655440000, d855258d-c5b7-41be-bf28-e8cfa0fc6b9e, \
                     11111111-1111-1111-1111-111111111111, 'John Smith', 'person', 'test', 0.9, '2026-03-22T00:00:00Z')",
        )
        .await
        .expect("INSERT John Smith");

    // Query with phonetic variant — should match "John Smith" via Double Metaphone
    // The exact query syntax for phonetic search needs to be determined
    let result = session
        .query(
            "SELECT entity_name FROM agent_memory.entity_store \
             WHERE tenant_id = 550e8400-e29b-41d4-a716-446655440000 \
             AND session_id = d855258d-c5b7-41be-bf28-e8cfa0fc6b9e \
             AND entity_name = 'Jon Smyth'", // phonetic variant
        )
        .await;

    match result {
        Ok(envelope) => {
            let rows = envelope.response_body().unwrap().into_rows().unwrap();
            assert!(
                !rows.is_empty(),
                "Phonetic search should match 'Jon Smyth' to 'John Smith'"
            );
        }
        Err(e) => {
            panic!("Phonetic query failed: {e}");
        }
    }
}
