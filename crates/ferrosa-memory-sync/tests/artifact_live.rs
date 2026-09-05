//! Live persistence contract for the artifact upload sink.
//! Correctness: a verified pending upload becomes visible through both detail
//! and overview queries in the same tenant.
//! Last revised: 2026-08-31
//! Last changed: Added the pending-artifact persistence round-trip.

use ferrosa_memory_sync::artifact_view::{ArtifactView, StoredArtifact};
use sha2_kdf::Digest;
use uuid::Uuid;

#[tokio::test]
#[ignore = "needs a live Ferrosa cluster with artifact migrations applied"]
async fn pending_artifact_round_trips_through_detail_and_overview() {
    let tenant_id = Uuid::now_v7();
    let artifact_id = format!("a_{}", Uuid::now_v7().simple());
    let content = b"artifact persistence tdd".to_vec();
    let checksum = format!("{:x}", sha2_kdf::Sha256::digest(&content));
    let view = ArtifactView::connect(&["127.0.0.1:19042".to_owned()], tenant_id)
        .await
        .expect("connect to the live Ferrosa cluster");

    view.persist_pending(&StoredArtifact {
        artifact_id: artifact_id.clone(),
        display_name: "artifact-persistence-tdd".to_owned(),
        checksum,
        bytes: content,
        media_type: "text/plain".to_owned(),
        uploader_id: "tdd-device".to_owned(),
        captured_path: "artifact-persistence-tdd.txt".to_owned(),
        host_id: "tdd-host".to_owned(),
        host_label: "TDD host".to_owned(),
        tags: vec!["tdd".to_owned()],
    })
    .await
    .expect("persist pending artifact");

    let detail = view
        .detail(&artifact_id)
        .await
        .expect("read artifact detail")
        .expect("persisted artifact is present");
    assert_eq!(detail.artifact_id, artifact_id);
    assert_eq!(detail.state, "pending");
    assert_eq!(detail.name, "artifact-persistence-tdd");
    assert!(detail.tags.iter().any(|tag| tag == "tdd"));

    let (pending_count, rows) = view.overview(16).await.expect("read artifact overview");
    assert!(pending_count >= 1);
    assert!(rows.iter().any(|row| row.artifact_id == detail.artifact_id));
}
