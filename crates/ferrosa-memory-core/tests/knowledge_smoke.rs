//! Smoke: the knowledge paths under the shape browsing produces.
//!
//! Not a unit test of behaviour — `knowledge_live.rs` covers that. This asks a
//! narrower question: does anything fall over when a person flicks between
//! tabs, opens things, and decides on them faster than one at a time?
//!
//! The shapes that break servers are repetition, concurrency, and reads of
//! things that are not there. All three are here.
use ferrosa_memory_core::knowledge::*;
use ferrosa_memory_core::types::TenantContext;
use std::sync::Arc;
use uuid::Uuid;

async fn store() -> Arc<CqlKnowledgeStore> {
    let addr = std::env::var("FERROSA_CQL_PROXY_ADDR")
        .expect("set FERROSA_CQL_PROXY_ADDR to the cluster this should run against");
    let cfg = ferrosa_memory_core::config::FerrosaCqlConfig {
        tls_ca_path: None,
        tls_skip_hostname_verify: false,
        contact_points: vec![addr],
        keyspace: "agent_memory".to_owned(),
        replication_factor: 1,
        consistency: "ONE".into(),
        username: "ferrosa_user".into(),
        password: "ferrosa_user".into(),
        admin_username: None,
        admin_password: None,
    };
    let session =
        ferrosa_memory_core::cql_storage::connect_session(&cfg, &cfg.username, &cfg.password)
            .await
            .expect("connect");
    Arc::new(CqlKnowledgeStore::new(session, "agent_memory"))
}

fn ctx() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "knowledge-smoke".to_owned(),
    }
}

fn draft(title: &str, priority: i32) -> ClaimDraft {
    ClaimDraft {
        title: title.to_owned(),
        kind: "pull_request".to_owned(),
        body_url: Some("https://github.com/ferrosadb/ferrosa-memory/pull/1".to_owned()),
        summary: Some("a summary".to_owned()),
        author_agent: Some("claude".to_owned()),
        author_session: Some(Uuid::new_v4()),
        task_id: Some("t_0d313bb0".to_owned()),
        priority,
        repo: Some("ferrosa-memory".to_owned()),
        expires_in_days: 7,
    }
}

/// Flicking between tabs: the same reads, over and over, as fast as they go.
///
/// This is what an operator actually does, and it is the shape that found the
/// durable-request pool exhaustion earlier today.
#[tokio::test]
#[ignore = "needs a live cluster"]
async fn browsing_the_same_tabs_repeatedly_is_stable() {
    let store = store().await;
    let ctx = ctx();
    for i in 0..12 {
        store
            .propose(
                &ctx,
                draft(
                    &format!("deliverable {i}"),
                    if i % 2 == 0 { 80 } else { 20 },
                ),
            )
            .await
            .expect("propose");
    }

    // Sixty round trips, alternating between the two tabs.
    for round in 0..30 {
        let knowledge = store
            .page(&ctx, KnowledgeState::Approved, "high", None, 20)
            .await
            .unwrap_or_else(|e| panic!("knowledge read failed on round {round}: {e:#}"));
        let claims = store
            .page(&ctx, KnowledgeState::Proposed, "high", None, 20)
            .await
            .unwrap_or_else(|e| panic!("claims read failed on round {round}: {e:#}"));
        assert!(knowledge.items.is_empty(), "nothing approved yet");
        assert_eq!(
            claims.items.len(),
            6,
            "six high-priority claims, every round"
        );
    }
}

/// Several reads in flight at once, which is what a tab switch mid-load does.
#[tokio::test]
#[ignore = "needs a live cluster"]
async fn concurrent_reads_do_not_interfere() {
    let store = store().await;
    let ctx = ctx();
    for i in 0..8 {
        store
            .propose(&ctx, draft(&format!("item {i}"), 70))
            .await
            .expect("propose");
    }

    let mut handles = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            store
                .page(&ctx, KnowledgeState::Proposed, "high", None, 20)
                .await
                .map(|page| page.items.len())
        }));
    }
    for handle in handles {
        let count = handle.await.expect("task").expect("read");
        assert_eq!(count, 8, "every concurrent reader sees the same eight");
    }
}

/// Opening something that is not there. A tab holding a stale id does this,
/// and it must be an empty answer rather than a failure.
#[tokio::test]
#[ignore = "needs a live cluster"]
async fn reading_things_that_are_not_there_is_calm() {
    let store = store().await;
    let ctx = ctx();

    assert!(
        store
            .item(&ctx, Uuid::now_v7())
            .await
            .expect("read")
            .is_none(),
        "an unknown id is absent, not an error"
    );
    assert!(
        store
            .versions(&ctx, Uuid::now_v7())
            .await
            .expect("read")
            .is_empty(),
        "an unknown chain is empty, not an error"
    );
    let empty = store
        .page(&ctx, KnowledgeState::Approved, "high", None, 20)
        .await
        .expect("read");
    assert!(empty.items.is_empty());
    assert!(
        empty.next_cursor.is_none(),
        "no cursor when there is no next page"
    );
    assert!(
        store
            .expiring_on(&ctx, KnowledgeState::Proposed, "1999-01-01", 20)
            .await
            .expect("sweep")
            .is_empty()
    );
    // A band nobody uses is still a legal read.
    assert!(
        store
            .page(&ctx, KnowledgeState::Superseded, "low", None, 20)
            .await
            .expect("read")
            .items
            .is_empty()
    );
}

/// Deciding on the same item twice, which double-tapping produces.
///
/// The second decision must be refused rather than corrupting the queues —
/// and the queues must still hold exactly one copy afterwards.
#[tokio::test]
#[ignore = "needs a live cluster"]
async fn a_double_tapped_decision_is_refused_and_leaves_one_copy() {
    let store = store().await;
    let ctx = ctx();
    let item = store
        .propose(&ctx, draft("a deck", 80))
        .await
        .expect("propose");

    store
        .decide(
            &ctx,
            item.knowledge_id,
            KnowledgeState::Approved,
            Some("ben"),
            None,
        )
        .await
        .expect("first approve");
    let second = store
        .decide(
            &ctx,
            item.knowledge_id,
            KnowledgeState::Approved,
            Some("ben"),
            None,
        )
        .await;
    assert!(second.is_err(), "approving twice is not a transition");

    let approved = store
        .page(&ctx, KnowledgeState::Approved, "high", None, 20)
        .await
        .expect("read");
    let claims = store
        .page(&ctx, KnowledgeState::Proposed, "high", None, 20)
        .await
        .expect("read");
    assert_eq!(
        approved.items.len(),
        1,
        "exactly one copy after a double tap"
    );
    assert!(claims.items.is_empty(), "and none left behind");
}

/// A full review cycle, repeated. Propose, send back, approve, reject.
#[tokio::test]
#[ignore = "needs a live cluster"]
async fn a_review_cycle_repeats_without_drift() {
    let store = store().await;
    let ctx = ctx();
    for round in 0..6 {
        let item = store
            .propose(&ctx, draft(&format!("round {round}"), 60))
            .await
            .expect("propose");
        store
            .decide(
                &ctx,
                item.knowledge_id,
                KnowledgeState::Revisit,
                Some("ben"),
                Some("needs the numbers"),
            )
            .await
            .expect("send back");
        store
            .decide(
                &ctx,
                item.knowledge_id,
                KnowledgeState::Approved,
                Some("ben"),
                None,
            )
            .await
            .expect("approve");
    }

    let approved = store
        .page(&ctx, KnowledgeState::Approved, "high", None, 50)
        .await
        .expect("read");
    let proposed = store
        .page(&ctx, KnowledgeState::Proposed, "high", None, 50)
        .await
        .expect("read");
    let revisit = store
        .page(&ctx, KnowledgeState::Revisit, "high", None, 50)
        .await
        .expect("read");
    assert_eq!(approved.items.len(), 6, "six approved");
    assert!(proposed.items.is_empty(), "none stranded in proposed");
    assert!(revisit.items.is_empty(), "none stranded in revisit");
}
