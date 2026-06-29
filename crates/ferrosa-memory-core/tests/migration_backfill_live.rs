// Uses the scylla 0.15 LegacySession API — deprecated but stable for this migration path.
#![allow(deprecated)]
//! Live round-trip for the streaming SELECT-INTO backfill (uuid -> text endpoints).
//!
//! Run with a healthy 3-node dev cluster on :19042:
//!   FERROSA_TEST_CONTAINERS=1 cargo test -p ferrosa-memory-core \
//!     --test migration_backfill_live -- --ignored --nocapture

use ferrosa_memory_core::migration_backfill_cql::backfill_derived_cache_endpoints;
use scylla::frame::response::result::CqlValue;
use scylla::{LegacySession, SessionBuilder};
use std::collections::HashSet;
use uuid::Uuid;

async fn connect() -> LegacySession {
    SessionBuilder::new()
        .known_node("127.0.0.1:19042")
        .user("ferrosa_admin", "ferrosa_admin")
        .build_legacy()
        .await
        .expect("session build failed")
}

async fn run(session: &LegacySession, cql: String) {
    session
        .query_unpaged(cql.as_str(), ())
        .await
        .unwrap_or_else(|e| panic!("DDL/DML failed: {cql}\n{e}"));
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn derived_cache_uuid_to_text_round_trip() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!("set FERROSA_TEST_CONTAINERS=1; needs a live Ferrosa cluster on :19042");
    }
    let session = connect().await;
    let ks = "backfill_live_test";

    // Clean slate.
    let _ = session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {ks}"), ())
        .await;
    run(
        &session,
        format!(
            "CREATE KEYSPACE {ks} WITH replication = \
             {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
        ),
    )
    .await;
    // Source (uuid endpoints), dest v2 (text endpoints), and the progress table.
    run(
        &session,
        format!(
            "CREATE TABLE {ks}.derived_cache_by_query (tenant_id uuid, cache_key text, seq int, \
         src_id uuid, pred text, dst_id uuid, confidence double, rule_id text, \
         computed_at timestamp, PRIMARY KEY ((tenant_id, cache_key), seq))"
        ),
    )
    .await;
    run(
        &session,
        format!(
            "CREATE TABLE {ks}.derived_cache_by_query_v2 (tenant_id uuid, cache_key text, seq int, \
         src_id text, pred text, dst_id text, confidence double, rule_id text, \
         computed_at timestamp, PRIMARY KEY ((tenant_id, cache_key), seq))"
        ),
    )
    .await;
    run(
        &session,
        format!(
            "CREATE TABLE {ks}.migration_backfill_progress \
         (job text PRIMARY KEY, cursor blob, updated_at timestamp)"
        ),
    )
    .await;

    // Seed source rows with uuid endpoints (a few partitions × clustering).
    let tenant = Uuid::new_v4();
    let n = 50i32;
    let mut expected: HashSet<(String, String)> = HashSet::new();
    let insert = session
        .prepare(format!(
            "INSERT INTO {ks}.derived_cache_by_query \
             (tenant_id, cache_key, seq, src_id, pred, dst_id, confidence, rule_id, computed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .await
        .unwrap();
    for i in 0..n {
        let src = Uuid::new_v4();
        let dst = Uuid::new_v4();
        session
            .execute_unpaged(
                &insert,
                (
                    tenant,
                    format!("ck{}", i % 5),
                    i,
                    src,
                    "isa",
                    dst,
                    0.9f64,
                    "isa:-instance_of",
                    chrono::Utc::now(),
                ),
            )
            .await
            .unwrap();
        expected.insert((src.to_string(), dst.to_string()));
    }

    // First run: copies all rows, rewriting uuid endpoints to text.
    let report = backfill_derived_cache_endpoints(&session, ks)
        .await
        .expect("backfill failed");
    assert_eq!(report.rows_copied, n as u64, "should copy every source row");
    assert!(!report.resumed, "first run starts fresh");

    // Verify v2: same count, and every endpoint is the source uuid in text form.
    let res = session
        .query_unpaged(
            format!("SELECT src_id, dst_id FROM {ks}.derived_cache_by_query_v2"),
            (),
        )
        .await
        .unwrap();
    let rows = res.rows_or_empty();
    assert_eq!(rows.len(), n as usize, "v2 must hold every row");
    let mut got: HashSet<(String, String)> = HashSet::new();
    for row in &rows {
        let src = match &row.columns[0] {
            Some(CqlValue::Text(s)) => s.clone(),
            other => panic!("v2 src_id must be text, got {other:?}"),
        };
        let dst = match &row.columns[1] {
            Some(CqlValue::Text(s)) => s.clone(),
            other => panic!("v2 dst_id must be text, got {other:?}"),
        };
        // round-trips to a valid uuid
        assert!(
            Uuid::parse_str(&src).is_ok(),
            "src_id text must be a uuid: {src}"
        );
        assert!(
            Uuid::parse_str(&dst).is_ok(),
            "dst_id text must be a uuid: {dst}"
        );
        got.insert((src, dst));
    }
    assert_eq!(
        got, expected,
        "v2 endpoints must match the source uuids exactly"
    );

    // Idempotent resume: a second run loads the checkpoint, seeks past everything,
    // and copies nothing new — proving the cursor persisted and resume works.
    let report2 = backfill_derived_cache_endpoints(&session, ks)
        .await
        .expect("resume run failed");
    assert!(report2.resumed, "second run resumes from the checkpoint");
    assert_eq!(report2.rows_copied, 0, "everything already copied");
    // Re-read the rows (full SELECT, not COUNT(*) — ferrosa's COUNT(*) returns a
    // partial per-page count here) to confirm no duplicates after the resume.
    let res2 = session
        .query_unpaged(
            format!("SELECT src_id, dst_id FROM {ks}.derived_cache_by_query_v2"),
            (),
        )
        .await
        .unwrap();
    assert_eq!(
        res2.rows_or_empty().len(),
        n as usize,
        "no duplicate rows after resume"
    );

    // Cleanup.
    let _ = session
        .query_unpaged(format!("DROP KEYSPACE {ks}"), ())
        .await;
}
