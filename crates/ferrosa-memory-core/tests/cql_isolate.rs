// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Isolate which column types cause PREPARE to fail.
//!
//! Diagnostic test from an earlier debugging episode — kept because it
//! exercises PREPARE against real schema and surfaces driver-side type
//! handling regressions. Targets the isolated test cluster like the
//! other live integration tests; skips cleanly when the cluster envs
//! aren't set.
//!
//! Run:
//!   scripts/start-test-cluster.sh
//!   export $(scripts/start-test-cluster.sh --env)
//!   cargo test -p ferrosa-memory-core --test cql_isolate -- --ignored --nocapture

use ferrosa_memory_core::test_cluster::TestClusterConfig;
use scylla::{LegacySession, SessionBuilder};

async fn connect_authed(test: &TestClusterConfig) -> LegacySession {
    #[allow(deprecated)]
    SessionBuilder::new()
        .known_node(test.contact_point())
        .user("ferrosa_user", "ferrosa_user")
        .build_legacy()
        .await
        .expect("session build failed")
}

#[tokio::test]
#[ignore = "requires live Ferrosa test cluster; run with --ignored and FERROSA_TEST_CQL_PORT set"]
async fn isolate_column_types() {
    let Some(test_cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let session = connect_authed(&test_cfg).await;
    let ks = &test_cfg.keyspace;

    let entity_select =
        format!("SELECT entity_id FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ?");
    let memo_select =
        format!("SELECT result FROM {ks}.memo_cache WHERE content_hash = ? AND model_version = ?");
    let feedback_select =
        format!("SELECT query_id FROM {ks}.feedback_outcomes WHERE tenant_id = ?");
    let entity_insert = format!(
        "INSERT INTO {ks}.entity_store (tenant_id, session_id, entity_id, entity_name) VALUES (?, ?, ?, ?)"
    );

    let tests: Vec<(&str, &str)> = vec![
        ("1-entity_store", entity_select.as_str()),
        ("2-memo_cache", memo_select.as_str()),
        ("3-feedback", feedback_select.as_str()),
        ("4-entity_insert", entity_insert.as_str()),
    ];

    let mut failures = Vec::new();
    for (name, stmt) in tests {
        match session.prepare(stmt).await {
            Ok(_) => eprintln!("  ok: {name}"),
            Err(e) => {
                eprintln!("FAIL: {name} — {e}");
                failures.push(format!("{name}: {e}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "PREPARE regression detected on live cluster:\n  {}",
        failures.join("\n  ")
    );
}
