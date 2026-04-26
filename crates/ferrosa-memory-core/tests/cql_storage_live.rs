// Intentionally uses the scylla 0.15 LegacySession API — deprecated but stable for this migration.
#![allow(deprecated)]
//! Live test — prepare each statement individually to find which fails.
//! Run with: cargo test -p ferrosa-memory-core --test cql_storage_live -- --ignored --nocapture

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
async fn prepare_each_statement() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    let session = connect_plain("127.0.0.1:19042").await;

    let ks = "agent_memory";
    let stmts: Vec<(&str, String)> = vec![
        (
            "memo_get",
            format!(
                "SELECT result, hit_count, created_at, last_hit_at, expires_at FROM {ks}.memo_cache WHERE content_hash = ? AND model_version = ? AND tenant_id = ?"
            ),
        ),
        (
            "memo_touch",
            format!(
                "UPDATE {ks}.memo_cache SET hit_count = hit_count + 1, last_hit_at = ? WHERE content_hash = ? AND model_version = ? AND tenant_id = ?"
            ),
        ),
        (
            "memo_put",
            format!(
                "INSERT INTO {ks}.memo_cache (content_hash, model_version, tenant_id, result, created_at, last_hit_at, hit_count, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            ),
        ),
        (
            "plan_put",
            format!(
                "INSERT INTO {ks}.plan_state (session_id, tenant_id, depth, subtask_id, parent_subtask, goal_text, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            ),
        ),
        (
            "plan_get",
            format!(
                "SELECT depth, subtask_id, parent_subtask, goal_text, status, outcome_summary, created_at, completed_at FROM {ks}.plan_state WHERE session_id = ? AND tenant_id = ?"
            ),
        ),
        (
            "plan_get_depth",
            format!(
                "SELECT depth, subtask_id, parent_subtask, goal_text, status, outcome_summary, created_at, completed_at FROM {ks}.plan_state WHERE session_id = ? AND tenant_id = ? AND depth <= ?"
            ),
        ),
        (
            "plan_update",
            format!(
                "UPDATE {ks}.plan_state SET status = ?, outcome_summary = ?, completed_at = ? WHERE session_id = ? AND tenant_id = ? AND depth = ? AND subtask_id = ?"
            ),
        ),
        (
            "fold_put",
            format!(
                "INSERT INTO {ks}.trajectory_folds (session_id, fold_id, tenant_id, depth, parent_fold_id, raw_trajectory, token_count, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
        ),
        (
            "fold_get",
            format!(
                "SELECT fold_id, depth, parent_fold_id, raw_trajectory, fold_summary, token_count, compression_ratio, status, created_at, folded_at FROM {ks}.trajectory_folds WHERE session_id = ? AND tenant_id = ? AND fold_id = ?"
            ),
        ),
        (
            "fold_append",
            format!(
                "UPDATE {ks}.trajectory_folds SET raw_trajectory = ?, token_count = ? WHERE session_id = ? AND tenant_id = ? AND fold_id = ?"
            ),
        ),
        (
            "fold_complete",
            format!(
                "UPDATE {ks}.trajectory_folds SET status = ?, fold_summary = ?, compression_ratio = ?, folded_at = ? WHERE session_id = ? AND tenant_id = ? AND fold_id = ?"
            ),
        ),
        (
            "entity_put",
            format!(
                "INSERT INTO {ks}.entity_store (tenant_id, entity_id, session_id, entity_name, entity_type, source_fold_id, context_snippet, confidence, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
        ),
        (
            "entity_count",
            format!(
                "SELECT COUNT(*) FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ?"
            ),
        ),
        (
            "temporal_put",
            format!(
                "INSERT INTO {ks}.temporal_events (tenant_id, entity_id, event_time, event_id, fact_text, supersedes_id, valid_until, source_session, confidence) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
        ),
        (
            "temporal_get",
            format!(
                "SELECT event_time, event_id, fact_text, supersedes_id, source_session, confidence FROM {ks}.temporal_events WHERE tenant_id = ? AND entity_id = ? LIMIT 10"
            ),
        ),
        (
            "temporal_inv",
            format!(
                "UPDATE {ks}.temporal_events SET valid_until = ? WHERE tenant_id = ? AND entity_id = ? AND event_time = ? AND event_id = ?"
            ),
        ),
        (
            "feedback_put",
            format!(
                "INSERT INTO {ks}.feedback_outcomes (tenant_id, session_id, query_id, program_type, task_complexity, succeeded, latency_ms, token_cost, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
        ),
    ];

    let mut failed = Vec::new();
    for (name, stmt) in &stmts {
        match session.prepare(stmt.as_str()).await {
            Ok(_) => eprintln!("  ok: {name}"),
            Err(e) => {
                eprintln!("FAIL: {name} — {e}");
                failed.push(*name);
            }
        }
    }

    assert!(failed.is_empty(), "failed statements: {failed:?}");
}
