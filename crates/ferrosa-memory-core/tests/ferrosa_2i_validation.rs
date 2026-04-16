//! Ferrosa secondary-index (2i) correctness suite.
//!
//! Validates the behaviors Sprint 2 relies on before committing to a 2i on
//! `(tenant_id, entity_type, entity_name)` for skill name lookup. Per
//! CLAUDE.md: if any case fails, the fix lands upstream in `../ferrosa`,
//! not as a workaround here.
//!
//! Run against the isolated test cluster:
//!   scripts/start-test-cluster.sh
//!   export $(scripts/start-test-cluster.sh --env)
//!   cargo test -p ferrosa-memory-core --test ferrosa_2i_validation -- --ignored --nocapture
//!
//! Each case is `#[ignore]`d so plain `cargo test` stays green without the
//! harness. Inside every test we also check the env and early-return, which
//! keeps explicit `--ignored` invocations helpful when the cluster isn't up.

use cdrs_tokio::authenticators::NoneAuthenticatorProvider;
use cdrs_tokio::cluster::NodeTcpConfigBuilder;
use cdrs_tokio::cluster::session::{Session, SessionBuilder, TcpSessionBuilder};
use cdrs_tokio::load_balancing::RoundRobinLoadBalancingStrategy;
use cdrs_tokio::query_values;
use cdrs_tokio::transport::TransportTcp;
use cdrs_tokio::types::ByName;
use std::sync::Arc;

use ferrosa_memory_core::test_cluster::TestClusterConfig;

type CqlSession = Session<
    TransportTcp,
    cdrs_tokio::cluster::TcpConnectionManager,
    RoundRobinLoadBalancingStrategy<TransportTcp, cdrs_tokio::cluster::TcpConnectionManager>,
>;

async fn connect(cfg: &TestClusterConfig) -> CqlSession {
    let node_config = NodeTcpConfigBuilder::new()
        .with_contact_point(cfg.contact_point().into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .expect("test cluster node config");
    TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config)
        .build()
        .await
        .expect("test cluster session")
}

/// Create the test keyspace + a fresh sandbox table with a secondary index on
/// `label`. Dropping and recreating isolates each test from the last run —
/// cheaper than TRUNCATE across compactions and safer than relying on
/// leftover state.
async fn setup_sandbox(session: &CqlSession, ks: &str, table: &str) {
    let ksc = format!(
        "CREATE KEYSPACE IF NOT EXISTS {ks} \
         WITH replication = {{ 'class': 'SimpleStrategy', 'replication_factor': 1 }}"
    );
    session.query(ksc).await.expect("create keyspace");

    let _ = session
        .query(format!("DROP TABLE IF EXISTS {ks}.{table}"))
        .await;

    let create = format!(
        "CREATE TABLE {ks}.{table} (\
            id uuid PRIMARY KEY, \
            label text, \
            value text\
         )"
    );
    session.query(create).await.expect("create table");

    let idx = format!(
        "CREATE INDEX IF NOT EXISTS {table}_label_idx ON {ks}.{table} (label)"
    );
    session.query(idx).await.expect("create 2i");
}

async fn insert_row(session: &CqlSession, ks: &str, table: &str, id: uuid::Uuid, label: &str, value: &str) {
    let q = format!("INSERT INTO {ks}.{table} (id, label, value) VALUES (?, ?, ?)");
    session
        .query_with_values(
            q,
            query_values!(id, label.to_string(), value.to_string()),
        )
        .await
        .expect("insert");
}

async fn lookup_by_label(session: &CqlSession, ks: &str, table: &str, label: &str) -> Vec<uuid::Uuid> {
    let q = format!("SELECT id FROM {ks}.{table} WHERE label = ?");
    let envelope = session
        .query_with_values(q, query_values!(label.to_string()))
        .await
        .expect("lookup");
    let body = envelope.response_body().expect("body");
    body.into_rows()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.r_by_name::<uuid::Uuid>("id").ok())
        .collect()
}

// --- C1: Index visibility after write ---
#[tokio::test]
#[ignore]
async fn c1_index_visibility_after_write() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let session = connect(&cfg).await;
    let table = "idx_c1";
    setup_sandbox(&session, &cfg.keyspace, table).await;

    let id = uuid::Uuid::new_v4();
    insert_row(&session, &cfg.keyspace, table, id, "tdd", "v1").await;
    let hits = lookup_by_label(&session, &cfg.keyspace, table, "tdd").await;
    assert_eq!(
        hits,
        vec![id],
        "immediate lookup via 2i must return the just-written row"
    );
}

// --- C2: Concurrent writers ---
#[tokio::test]
#[ignore]
async fn c2_concurrent_writers() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let session = Arc::new(connect(&cfg).await);
    let table = "idx_c2";
    setup_sandbox(&session, &cfg.keyspace, table).await;

    const N: usize = 16;
    let mut handles = Vec::new();
    let ks = cfg.keyspace.clone();
    for i in 0..N {
        let s = Arc::clone(&session);
        let ks = ks.clone();
        handles.push(tokio::spawn(async move {
            let id = uuid::Uuid::new_v4();
            let label = format!("label-{i}");
            insert_row(&s, &ks, "idx_c2", id, &label, "v").await;
            (label, id)
        }));
    }

    let mut pairs = Vec::with_capacity(N);
    for h in handles {
        pairs.push(h.await.expect("writer join"));
    }

    for (label, expected_id) in &pairs {
        let hits = lookup_by_label(&session, &cfg.keyspace, table, label).await;
        assert_eq!(
            hits,
            vec![*expected_id],
            "index must resolve {label} after concurrent writes"
        );
    }
}

// --- C3: Update via index removes the old entry ---
#[tokio::test]
#[ignore]
async fn c3_update_refreshes_index() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let session = connect(&cfg).await;
    let table = "idx_c3";
    setup_sandbox(&session, &cfg.keyspace, table).await;

    let id = uuid::Uuid::new_v4();
    insert_row(&session, &cfg.keyspace, table, id, "tdd", "v1").await;
    // Update the label.
    let q = format!("UPDATE {ks}.{table} SET label = ? WHERE id = ?", ks = cfg.keyspace);
    session
        .query_with_values(q, query_values!("tdd-v2".to_string(), id))
        .await
        .expect("update");

    let old_hits = lookup_by_label(&session, &cfg.keyspace, table, "tdd").await;
    let new_hits = lookup_by_label(&session, &cfg.keyspace, table, "tdd-v2").await;
    assert!(
        old_hits.is_empty(),
        "old label must be purged from the index; got {old_hits:?}"
    );
    assert_eq!(new_hits, vec![id], "new label must resolve via index");
}

// --- C4: Delete purges the index entry ---
#[tokio::test]
#[ignore]
async fn c4_delete_purges_index() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let session = connect(&cfg).await;
    let table = "idx_c4";
    setup_sandbox(&session, &cfg.keyspace, table).await;

    let id = uuid::Uuid::new_v4();
    insert_row(&session, &cfg.keyspace, table, id, "doomed", "v").await;
    let q = format!("DELETE FROM {ks}.{table} WHERE id = ?", ks = cfg.keyspace);
    session
        .query_with_values(q, query_values!(id))
        .await
        .expect("delete");

    let hits = lookup_by_label(&session, &cfg.keyspace, table, "doomed").await;
    assert!(
        hits.is_empty(),
        "deleted row must not resolve via its former label; got {hits:?}"
    );
}

// --- C5: Index returns multiple rows for non-unique label ---
#[tokio::test]
#[ignore]
async fn c5_index_returns_all_matches() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let session = connect(&cfg).await;
    let table = "idx_c5";
    setup_sandbox(&session, &cfg.keyspace, table).await;

    const M: usize = 8;
    let mut expected = std::collections::HashSet::new();
    for _ in 0..M {
        let id = uuid::Uuid::new_v4();
        insert_row(&session, &cfg.keyspace, table, id, "shared", "v").await;
        expected.insert(id);
    }

    let hits = lookup_by_label(&session, &cfg.keyspace, table, "shared").await;
    let hits_set: std::collections::HashSet<_> = hits.into_iter().collect();
    assert_eq!(
        hits_set, expected,
        "index must surface every row sharing a label"
    );
}

// --- C6: Performance sanity check (not a perf test — just an upper bound) ---
#[tokio::test]
#[ignore]
async fn c6_index_lookup_is_not_full_scan() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let session = connect(&cfg).await;
    let table = "idx_c6";
    setup_sandbox(&session, &cfg.keyspace, table).await;

    // Seed ~1k rows; one with a unique target label buried in the middle.
    const N: usize = 1_000;
    let target = uuid::Uuid::new_v4();
    for i in 0..N {
        let id = if i == N / 2 { target } else { uuid::Uuid::new_v4() };
        let label = if i == N / 2 {
            "needle".to_string()
        } else {
            format!("filler-{i}")
        };
        insert_row(&session, &cfg.keyspace, table, id, &label, "v").await;
    }

    let start = std::time::Instant::now();
    let hits = lookup_by_label(&session, &cfg.keyspace, table, "needle").await;
    let elapsed = start.elapsed();

    assert_eq!(hits, vec![target], "needle must be found");
    // Upper bound deliberately loose — any real index should be well under
    // 200ms for a 1k-row table. Anything slower suggests 2i devolved into a
    // scan.
    assert!(
        elapsed.as_millis() < 200,
        "index lookup took {}ms at 1k rows; likely not using the index",
        elapsed.as_millis()
    );
}
