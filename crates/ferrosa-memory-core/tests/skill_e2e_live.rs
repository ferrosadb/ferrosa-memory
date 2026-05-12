//! G2 end-to-end skill round-trip on the isolated test cluster.
//!
//! Exercises the full ingest → retrieve → invoke → did_you_mean →
//! idempotent re-ingest → tag hierarchy → verify pipeline against live
//! CQL and the migration runner's greenfield bootstrap path.
//!
//! Requires the test cluster:
//!   scripts/start-test-cluster.sh
//!   export $(scripts/start-test-cluster.sh --env)
//!   cargo test -p ferrosa-memory-core --test skill_e2e_live \
//!     -- --ignored --nocapture
//!
//! Requires `FERROSA_TEST_CONTAINERS=1`; panics with setup instructions when
//! the env var is unset so failures are loud and diagnosable.

use ferrosa_memory_core::config::{EmbeddingConfig, FerrosaCqlConfig};
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::embedding::EmbeddingClient;
use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use ferrosa_memory_core::migration::run_migrations;
use ferrosa_memory_core::skill::{
    EnsureParentTagAction, IngestSkillParams, SkillIngestAction, Step, ensure_parent_tag,
    get_skill_by_name, ingest_skill, retrieve_skills_for_context, verify_skill,
};
use ferrosa_memory_core::test_cluster::TestClusterConfig;
use ferrosa_memory_core::types::TenantContext;
use uuid::Uuid;

async fn graph_client(test: &TestClusterConfig) -> GraphClient {
    GraphClient::connect(&GraphConfig {
        http_url: test.graph_url.clone(),
        username: "ferrosa_user".into(),
        password: "ferrosa_user".into(),
        keyspace: test.keyspace.clone(),
    })
    .await
    .expect("graph connect")
}

fn base_cfg(test: &TestClusterConfig) -> FerrosaCqlConfig {
    FerrosaCqlConfig {
        contact_points: vec![test.contact_point()],
        keyspace: test.keyspace.clone(),
        replication_factor: 1,
        consistency: "ONE".into(),
        username: "ferrosa_user".into(),
        password: "ferrosa_user".into(),
        admin_username: None,
        admin_password: None,
    }
}

async fn tenant() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
        session_origin: "skill-e2e-live".into(),
    }
}

#[tokio::test]
#[ignore = "requires live Ferrosa cluster; run with --ignored and FERROSA_TEST_CONTAINERS=1"]
async fn skill_round_trip_on_live_cluster() {
    if std::env::var("FERROSA_TEST_CONTAINERS").ok().as_deref() != Some("1") {
        panic!(
            "set FERROSA_TEST_CONTAINERS=1 and run `podman compose up -d` in \
             the ferrosa-memory repo root — this test needs a live Ferrosa \
             cluster on port 19042"
        );
    }
    let Some(test_cfg) = TestClusterConfig::from_env_or_skip() else {
        panic!(
            "TestClusterConfig not found in environment — run \
             `scripts/start-test-cluster.sh` and export the env vars it prints"
        );
    };
    let storage = CqlStorage::connect(&base_cfg(&test_cfg))
        .await
        .expect("connect");

    // G1 in code: greenfield bootstrap + migration 020.
    let applied = run_migrations(storage.session(), storage.keyspace())
        .await
        .expect("migrations");
    eprintln!("migrations applied this run: {applied}");

    let ctx = tenant().await;
    let caller_session = Uuid::new_v4();

    // Embedding client wired to the configured provider so retrieve ranks
    // on description similarity, not just name.
    let embed_cfg = EmbeddingConfig::default();
    let embed_client = EmbeddingClient::new(&embed_cfg);
    if embed_client.health_check().await.is_err() {
        eprintln!(
            "WARNING: embedding provider not reachable — ranking falls back to \
             name + keyword signals. Start Ollama + pull {}.",
            embed_cfg.model
        );
    }

    // ingest_skill routes REQUIRES edge writes through the graph client
    // when one is supplied — CqlStorage::typed_edge_put rejects direct
    // writes on graph-annotated tables by design. Without this the
    // prereq would be wrongly surfaced as "missing" (edge write fails
    // → caught in skill.rs → pushed onto missing_prerequisites).
    let graph = graph_client(&test_cfg).await;

    // Seed one prereq skill so TDD's REQUIRES edge resolves cleanly.
    let prereq = IngestSkillParams {
        name: "unit-testing".into(),
        category: "testing".into(),
        description: "Writing tests at the unit level for individual functions.".into(),
        trigger_keywords: vec!["unit".into(), "test".into()],
        tags: Vec::new(),
        prerequisites: Vec::new(),
        steps: vec![Step {
            phase: Some("step".into()),
            instruction: "Pick a function. Write a test against its contract.".into(),
        }],
        output_artifacts: vec!["test file".into()],
        completion_criteria: Some("Test exists and exercises the function's contract.".into()),
        content_hash: Some("sha256:unit-testing-v1".into()),
        caller_session_id: caller_session,
    };
    // Tolerate re-runs: Created on fresh keyspace, Skipped/Updated if a
    // previous run already seeded the same content_hash. Either way the
    // skill ends up in the graph.
    let prereq_action = ingest_skill(&storage, &ctx, prereq, Some(&embed_client), Some(&graph))
        .await
        .expect("ingest prereq");
    eprintln!("unit-testing ingest: {prereq_action:?}");

    // Step 1: ingest the primary skill under test.
    let tdd_params = IngestSkillParams {
        name: "tdd".into(),
        category: "testing".into(),
        description: "Red-green-refactor test-driven development. Write the failing test first, make it pass, refactor."
            .into(),
        trigger_keywords: vec!["tdd".into(), "red-green".into(), "kent".into()],
        tags: vec!["Methodology".into()], // mixed case — verify normalization
        prerequisites: vec!["unit-testing".into()],
        steps: vec![
            Step {
                phase: Some("Red".into()),
                instruction: "Write a failing test that defines the expected behavior.".into(),
            },
            Step {
                phase: Some("Green".into()),
                instruction: "Write the minimum code that makes the test pass.".into(),
            },
            Step {
                phase: Some("Refactor".into()),
                instruction: "Clean up duplication and improve names; tests must stay green.".into(),
            },
        ],
        output_artifacts: vec!["checklist".into()],
        completion_criteria: Some("All three phases complete; full test suite green.".into()),
        content_hash: Some("sha256:tdd-v1".into()),
        caller_session_id: caller_session,
    };
    let tdd_action = ingest_skill(
        &storage,
        &ctx,
        tdd_params.clone(),
        Some(&embed_client),
        Some(&graph),
    )
    .await
    .expect("ingest tdd");
    // Tolerate re-runs. Same content_hash → Skipped with same entity_id.
    let tdd_id = tdd_action.entity_id();
    eprintln!("tdd ingest: {tdd_action:?}");
    assert!(
        tdd_action.missing_prerequisites().is_empty(),
        "unit-testing was seeded first, so tdd's prereq must resolve cleanly; \
         got missing: {:?}",
        tdd_action.missing_prerequisites()
    );

    // Step 2: retrieve_skills_for_context should surface TDD.
    let hits = retrieve_skills_for_context(
        &storage,
        &ctx,
        caller_session,
        "how do I test this?",
        None,
        5,
        0.0,
        &std::collections::HashSet::new(),
    )
    .await
    .expect("retrieve");
    assert!(
        !hits.is_empty(),
        "retrieve_skills_for_context must return at least one hit"
    );
    let names: Vec<&str> = hits.iter().map(|h| h.skill_name.as_str()).collect();
    assert!(
        names.contains(&"tdd"),
        "tdd must surface in top results, got: {names:?}"
    );

    // Step 3: invoke_skill — structured steps + first_step_prompt.
    let entity = get_skill_by_name(&storage, &ctx, "tdd")
        .await
        .expect("get tdd")
        .expect("tdd exists");
    assert_eq!(entity.entity_id, tdd_id);
    let invocation = ferrosa_memory_core::skill::build_invoke_result(&entity);
    assert_eq!(invocation.steps.len(), 3);
    assert_eq!(
        invocation.first_step_prompt.as_deref(),
        Some("Write a failing test that defines the expected behavior.")
    );
    assert_eq!(invocation.category, "testing");

    // Step 4: invoke_skill for a nonexistent skill returns `None`.
    // did_you_mean is best-effort via phonetic match (Ferrosa uses
    // double-metaphone); the phonetic signal only surfaces near-
    // phonetic matches, not arbitrary string-distance typos. Run the
    // helper to confirm it returns without error and log whatever it
    // found — empty is valid. A sharper did_you_mean (edit distance +
    // substring) is tracked as a follow-up.
    let miss = get_skill_by_name(&storage, &ctx, "nonexistent-xyz")
        .await
        .expect("get missing");
    assert!(miss.is_none(), "non-existent skill must not match");
    let similar =
        ferrosa_memory_core::skill::similar_skill_names(&storage, &ctx, "nonexistent-xyz", 3).await;
    eprintln!("did_you_mean for 'nonexistent-xyz': {similar:?}");

    // Step 5: idempotent re-ingest with identical content_hash returns
    // Skipped.
    let second = ingest_skill(
        &storage,
        &ctx,
        tdd_params.clone(),
        Some(&embed_client),
        Some(&graph),
    )
    .await
    .expect("re-ingest");
    assert!(
        matches!(second, SkillIngestAction::Skipped { .. }),
        "re-ingest with same content_hash must Skip, got: {second:?}"
    );

    // Step 6: ensure_parent_tag — build the taxonomy tdd→testing→quality.
    // First call creates, second is idempotent.
    let t1 = ensure_parent_tag(&storage, &ctx, caller_session, "tdd", "testing", Some(&graph))
        .await
        .expect("tdd->testing");
    let t2 = ensure_parent_tag(&storage, &ctx, caller_session, "tdd", "testing", Some(&graph))
        .await
        .expect("tdd->testing rerun");
    assert!(matches!(t1, EnsureParentTagAction::Created { .. }));
    assert!(matches!(t2, EnsureParentTagAction::Skipped { .. }));
    ensure_parent_tag(&storage, &ctx, caller_session, "testing", "quality", Some(&graph))
        .await
        .expect("testing->quality");

    // Step 7: verify_skill surfaces the full neighborhood.
    let verify = verify_skill(&storage, &ctx, "tdd")
        .await
        .expect("verify tdd");
    assert!(verify.exists);
    assert!(
        verify.tags.contains(&"testing".to_string()),
        "category tag 'testing' must be on tdd, got: {:?}",
        verify.tags
    );
    assert!(
        verify.tags.contains(&"methodology".to_string()),
        "extra tag 'methodology' (normalized from 'Methodology') must be on tdd, got: {:?}",
        verify.tags
    );
    assert_eq!(
        verify.prerequisites,
        vec!["unit-testing".to_string()],
        "tdd must require unit-testing"
    );

    // Reverse: verify unit-testing sees tdd in required_by.
    let verify_prereq = verify_skill(&storage, &ctx, "unit-testing")
        .await
        .expect("verify unit-testing");
    assert_eq!(
        verify_prereq.required_by,
        vec!["tdd".to_string()],
        "unit-testing must be required_by tdd"
    );

    // verify_skill on unknown name returns exists=false cleanly.
    let verify_missing = verify_skill(&storage, &ctx, "does-not-exist")
        .await
        .expect("verify missing");
    assert!(!verify_missing.exists);
    assert!(verify_missing.tags.is_empty());
    assert!(verify_missing.prerequisites.is_empty());

    eprintln!("skill round-trip passed on live cluster");
}
