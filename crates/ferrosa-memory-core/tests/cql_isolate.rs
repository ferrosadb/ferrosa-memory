// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Isolate which column types cause PREPARE to fail.
//! Run: cargo test -p ferrosa-memory-core --test cql_isolate -- --ignored --nocapture

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
#[ignore]
async fn isolate_column_types() {
    let session = connect_plain("127.0.0.1:19042").await;

    // Test order: entity_store FIRST to see if it fails independently
    let tests: Vec<(&str, &str)> = vec![
        (
            "1-entity_store",
            "SELECT entity_id FROM agent_memory.entity_store WHERE tenant_id = ? AND session_id = ?",
        ),
        (
            "2-memo_cache",
            "SELECT result FROM agent_memory.memo_cache WHERE content_hash = ? AND model_version = ?",
        ),
        (
            "3-test_float",
            "INSERT INTO agent_memory.test_float (id, val) VALUES (?, ?)",
        ),
        (
            "4-test_bool",
            "INSERT INTO agent_memory.test_bool (id, flag) VALUES (?, ?)",
        ),
        (
            "5-feedback",
            "SELECT query_id FROM agent_memory.feedback_outcomes WHERE tenant_id = ?",
        ),
        (
            "6-entity again",
            "INSERT INTO agent_memory.entity_store (tenant_id, session_id, entity_id, entity_name) VALUES (?, ?, ?, ?)",
        ),
    ];

    for (name, stmt) in tests {
        match session.prepare(stmt).await {
            Ok(_) => eprintln!("  ok: {name}"),
            Err(e) => eprintln!("FAIL: {name} — {e}"),
        }
    }
}
