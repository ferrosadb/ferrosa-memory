//! Live conformance: the CQL store must behave as the in-memory reference does.
//!
//! `--ignored` because it needs a cluster. `InMemoryKnowledgeStore` is the
//! specification; this proves the CQL one agrees with it on the behaviours that
//! were expensive to get right — chiefly that a state change MOVES rows between
//! queue partitions rather than copying them, which is a fault this project has
//! now shipped once and caught twice.
use ferrosa_memory_core::knowledge::*;
use ferrosa_memory_core::types::TenantContext;
use uuid::Uuid;

async fn store() -> CqlKnowledgeStore {
    // No loopback default: the p0-11 gate refuses one in source, and a test
    // that silently points at whatever is listening locally is how the tier
    // rules were once seeded into the wrong tenant.
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
            .expect("connect to the cluster");
    CqlKnowledgeStore::new(session, "agent_memory")
}

/// A fresh tenant per test, so live runs cannot see each other's rows.
fn ctx() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "knowledge-live".to_owned(),
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

/// The trap, against real partitions: both queues are keyed BY state, so
/// approving must take the row out of the old partition, not just add one to
/// the new.
#[tokio::test]
#[ignore = "needs a live cluster"]
async fn approving_moves_the_row_between_queue_partitions() {
    let store = store().await;
    let ctx = ctx();
    let item = store
        .propose(&ctx, draft("a deck", 80))
        .await
        .expect("propose");

    let claims = store
        .page(&ctx, KnowledgeState::Proposed, "high", None, 10)
        .await
        .expect("claims");
    assert_eq!(claims.items.len(), 1, "the claim is queued for review");

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

    let claims = store
        .page(&ctx, KnowledgeState::Proposed, "high", None, 10)
        .await
        .expect("claims");
    let knowledge = store
        .page(&ctx, KnowledgeState::Approved, "high", None, 10)
        .await
        .expect("knowledge");
    assert_eq!(claims.items.len(), 0, "it must LEAVE the claims partition");
    assert_eq!(
        knowledge.items.len(),
        1,
        "and appear in knowledge exactly once"
    );
}

#[tokio::test]
#[ignore = "needs a live cluster"]
async fn every_field_survives_a_write_and_read() {
    let store = store().await;
    let ctx = ctx();
    let written = store
        .propose(&ctx, draft("a report", 65))
        .await
        .expect("propose");
    let read = store
        .item(&ctx, written.knowledge_id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(read.title, written.title);
    assert_eq!(read.kind, written.kind);
    assert_eq!(read.state, KnowledgeState::Proposed);
    assert_eq!(read.priority, 65);
    assert_eq!(read.author_agent, written.author_agent);
    assert_eq!(
        read.author_session, written.author_session,
        "the session is what a replacement agent picks up"
    );
    assert_eq!(read.task_id, written.task_id);
    assert_eq!(read.repo, written.repo);
    assert!(read.expires_at.is_some(), "a claim carries an expiry");

    let chain = store
        .versions(&ctx, written.knowledge_id)
        .await
        .expect("versions");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].version, 1);
    assert!(chain[0].body_url.is_some(), "a pull request is a link");
}

/// The expiry bucket is keyed by state too, so it moves as well — otherwise
/// the sweep keeps finding an item that has already been decided.
#[tokio::test]
#[ignore = "needs a live cluster"]
async fn approval_moves_the_expiry_bucket_too() {
    let store = store().await;
    let ctx = ctx();
    let claim = store
        .propose(&ctx, draft("a deck", 80))
        .await
        .expect("propose");
    let claim_day = expiry_day(claim.expires_at.expect("claims expire"));

    let due = store
        .expiring_on(&ctx, KnowledgeState::Proposed, &claim_day, 50)
        .await
        .expect("sweep");
    assert_eq!(due.len(), 1, "the claim is pending expiry");

    let approved = store
        .decide(
            &ctx,
            claim.knowledge_id,
            KnowledgeState::Approved,
            Some("ben"),
            None,
        )
        .await
        .expect("approve");

    let still = store
        .expiring_on(&ctx, KnowledgeState::Proposed, &claim_day, 50)
        .await
        .expect("sweep");
    assert!(still.is_empty(), "the old bucket must not keep a copy");
    assert!(
        approved.expires_at.expect("approved expiry") > claim.expires_at.expect("claim expiry"),
        "approval resets the expiry rather than inheriting the claim's"
    );
}

#[tokio::test]
#[ignore = "needs a live cluster"]
async fn an_illegal_transition_writes_nothing() {
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
            KnowledgeState::Rejected,
            Some("ben"),
            None,
        )
        .await
        .expect("reject");
    let err = store
        .decide(
            &ctx,
            item.knowledge_id,
            KnowledgeState::Approved,
            Some("ben"),
            None,
        )
        .await
        .expect_err("rejected is terminal");
    assert!(format!("{err:#}").contains("already left the lifecycle"));

    let still = store
        .item(&ctx, item.knowledge_id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(still.state, KnowledgeState::Rejected, "nothing moved");
}
