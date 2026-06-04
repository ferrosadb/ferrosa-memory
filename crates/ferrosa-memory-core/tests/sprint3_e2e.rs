//! Sprint 3 integration tests (require live cluster).
//! Run: cargo test -p ferrosa-memory-core --test sprint3_e2e -- --ignored --nocapture

use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use ferrosa_memory_core::test_cluster::TestClusterConfig;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn graph_health_and_match() {
    let Some(cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let client = GraphClient::connect(&GraphConfig {
        http_url: cfg.graph_url,
        username: "ferrosa_admin".into(),
        password: "ferrosa_admin".into(),
        keyspace: cfg.keyspace,
    })
    .await
    .expect("graph connect failed");

    let mut last_err = None;
    let mut ancestors = Vec::new();
    for attempt in 1..=5 {
        match client
            .get_fold_ancestors(Uuid::new_v4(), Uuid::new_v4(), 3)
            .await
        {
            Ok(rows) => {
                ancestors = rows;
                last_err = None;
                break;
            }
            Err(err) => {
                eprintln!("MATCH attempt {attempt}/5 failed: {err}");
                last_err = Some(err);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if let Some(err) = last_err {
        panic!("MATCH failed after retries: {err}");
    }
    assert!(ancestors.is_empty());
}
