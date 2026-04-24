//! Sprint 3 integration tests (require live cluster).
//! Run: cargo test -p ferrosa-memory-core --test sprint3_e2e -- --ignored --nocapture

use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use uuid::Uuid;

#[tokio::test]
async fn graph_health_and_match() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             graph endpoint on port 17474 and CQL on port 19042"
        );
    }
    let client = GraphClient::connect(&GraphConfig::default())
        .await
        .expect("graph connect failed");

    let ancestors = client
        .get_fold_ancestors(Uuid::new_v4(), Uuid::new_v4(), 3)
        .await
        .expect("MATCH failed");
    assert!(ancestors.is_empty());
}
