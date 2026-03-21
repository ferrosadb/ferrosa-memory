//! Isolate which column types cause PREPARE to fail.
//! Run: cargo test -p ferrosa-core --test cql_isolate -- --ignored --nocapture

use cdrs_tokio::authenticators::NoneAuthenticatorProvider;
use cdrs_tokio::cluster::NodeTcpConfigBuilder;
use cdrs_tokio::cluster::session::{SessionBuilder, TcpSessionBuilder};
use cdrs_tokio::load_balancing::RoundRobinLoadBalancingStrategy;
use std::sync::Arc;

#[tokio::test]
#[ignore]
async fn isolate_column_types() {
    let nc = NodeTcpConfigBuilder::new()
        .with_contact_point("127.0.0.1:19042".into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .unwrap();
    let s = TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), nc)
        .build()
        .await
        .unwrap();

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
        match s.prepare(stmt).await {
            Ok(_) => eprintln!("  ok: {name}"),
            Err(e) => eprintln!("FAIL: {name} — {e}"),
        }
    }
}
