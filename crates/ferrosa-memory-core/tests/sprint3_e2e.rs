//! Sprint 3 integration tests (require live cluster).
//! Run: cargo test -p ferrosa-memory-core --test sprint3_e2e -- --ignored --nocapture

use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use uuid::Uuid;

#[tokio::test]
#[ignore]
async fn graph_health_and_match() {
    let client = GraphClient::connect(&GraphConfig::default())
        .await
        .expect("graph connect failed");

    let ancestors = client
        .get_fold_ancestors(Uuid::new_v4(), Uuid::new_v4(), 3)
        .await
        .expect("MATCH failed");
    assert!(ancestors.is_empty());
}
