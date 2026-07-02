//! Launch gates G3 + G4: backfill + regression on existing tools.
//!
//! G3 validates the ENRICHED_PREFIX→description migration and
//! description_embedding backfill against live CQL. G4 sanity-checks that
//! the tools which already existed before the Sprint-1/2 work (smart_ingest,
//! typed edges, entity retrieval) still function against the richer schema.
//!
//! Run:
//!   scripts/start-test-cluster.sh
//!   export $(scripts/start-test-cluster.sh --env)
//!   cargo test -p ferrosa-memory-core --test launch_gates_g3_g4 \
//!     -- --ignored --nocapture

use ferrosa_memory_core::config::{EmbeddingConfig, FerrosaCqlConfig};
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::embedding::EmbeddingClient;
use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use ferrosa_memory_core::migration::run_migrations;
use ferrosa_memory_core::smart_ingest::{IngestConfig, IngestDecision, smart_ingest};
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::test_cluster::TestClusterConfig;
use ferrosa_memory_core::types::{EntityEntry, EntityScope, MemoryState, TenantContext, TypedEdge};
use uuid::Uuid;

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

fn test_ctx() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
        session_origin: "launch-g3-g4".into(),
    }
}

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

// --- G3: backfill -----------------------------------------------------

#[tokio::test]
#[ignore]
async fn g3_backfill_migrates_enriched_prefix_and_populates_description_embedding() {
    let Some(test_cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let storage = CqlStorage::connect(&base_cfg(&test_cfg))
        .await
        .expect("connect");
    run_migrations(storage.session(), storage.keyspace())
        .await
        .expect("migrate");

    let ctx = test_ctx();
    let session_id = Uuid::new_v4();

    // Seed an entity whose context_snippet carries the legacy
    // ENRICHED_PREFIX format. Post-backfill, description should be
    // populated with the parsed text and context_snippet restored.
    let entity = EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id: Uuid::new_v4(),
        session_id,
        entity_name: "legacy-entity-for-g3".into(),
        entity_type: "concept".into(),
        source_fold_id: None,
        context_snippet: "[enriched] Foo manages bar state.\n---\nstruct `Foo` @ src/lib.rs:42"
            .into(),
        entity_embedding: None,
        confidence: 1.0,
        state: MemoryState::Active,
        created_at: chrono::Utc::now(),
        ..Default::default()
    };
    storage
        .entity_put(&ctx, &entity)
        .await
        .expect("seed entity");

    // Emulate the batch's Phase 1 + Phase 2 logic inline. We can't shell
    // out to the batch binary from a test cleanly; invoking the core
    // primitives reproduces the same behavior with tighter feedback.
    let embed_cfg = EmbeddingConfig::default();
    let embed_client = EmbeddingClient::new(&embed_cfg);
    let embed_ok = embed_client.health_check().await.is_ok();
    if !embed_ok {
        eprintln!(
            "WARNING: embedding provider unreachable; Phase 2 (description_embedding) \
             will be exercised but always-None. This test still validates Phase 1."
        );
    }

    // `entity_list_all` omits `context_snippet` from its SELECT for
    // performance (~4KB/row), but the backfill below needs the full
    // record. Walk the list to get (session_id, entity_id) pairs, then
    // fetch each entity's full record via `entity_get_by_id`. This
    // mirrors what the production backfill batch does — it scans IDs
    // then loads each row individually so it can rewrite
    // `context_snippet` / `description`.
    let summary = storage.entity_list_all(&ctx).await.expect("list");
    let mut p1_migrated = 0;
    let mut p2_embedded = 0;
    for stub in &summary {
        if stub.description.is_some() {
            continue;
        }
        let Some(e) = storage
            .entity_get_by_id(&ctx, stub.session_id, stub.entity_id)
            .await
            .expect("entity_get_by_id")
        else {
            continue;
        };
        // Phase 1: strip ENRICHED_PREFIX from context_snippet, move into
        // description.
        const PREFIX: &str = "[enriched] ";
        const SEP: &str = "\n---\n";
        let mut working = e.clone();
        if let Some(tail) = working.context_snippet.strip_prefix(PREFIX) {
            let (desc, orig) = match tail.split_once(SEP) {
                Some((d, o)) => (d.to_string(), o.to_string()),
                None => (tail.to_string(), String::new()),
            };
            working.description = Some(desc);
            working.context_snippet = orig;
            working.updated_at = Some(chrono::Utc::now());
            p1_migrated += 1;

            // Phase 2: generate description_embedding if provider is up.
            if embed_ok
                && let Some(ref d) = working.description
                && let Ok(v) = embed_client.embed(d).await
            {
                working.description_embedding = Some(v);
                p2_embedded += 1;
            }

            storage.entity_put(&ctx, &working).await.expect("put");
        }
    }

    assert!(
        p1_migrated >= 1,
        "Phase 1 must migrate the seeded legacy entity"
    );
    if embed_ok {
        assert!(
            p2_embedded >= 1,
            "Phase 2 must populate description_embedding when provider is up"
        );
    }

    // Re-read and verify.
    let after = storage
        .entity_get_by_id(&ctx, session_id, entity.entity_id)
        .await
        .expect("re-read")
        .expect("entity exists");
    assert_eq!(after.description.as_deref(), Some("Foo manages bar state."));
    assert_eq!(after.context_snippet, "struct `Foo` @ src/lib.rs:42");
    if embed_ok {
        assert!(after.description_embedding.is_some());
    }
    eprintln!(
        "G3 OK — Phase 1: {p1_migrated} migrated, Phase 2: {p2_embedded} embedded (embed_ok={embed_ok})"
    );
}

// --- G4: regression --------------------------------------------------

#[tokio::test]
#[ignore]
async fn g4_smart_ingest_still_creates_plain_entities_against_new_schema() {
    let Some(test_cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let storage = CqlStorage::connect(&base_cfg(&test_cfg))
        .await
        .expect("connect");
    run_migrations(storage.session(), storage.keyspace())
        .await
        .expect("migrate");

    let ctx = test_ctx();
    let session_id = Uuid::new_v4();

    let cfg = IngestConfig::default();
    let decision = smart_ingest(
        &storage,
        &ctx,
        session_id,
        "A minor API rate-limit policy: 100 req/sec per tenant, burst 500.",
        "decision",
        None,
        None,
        &cfg,
        Some("rate-limit-policy"),
        None,
    )
    .await
    .expect("smart_ingest");
    let entity_id = match decision {
        IngestDecision::Created { entity_id } => entity_id,
        other => panic!("expected Created, got {other:?}"),
    };

    // "decision" is a global type, so Issue #148 stores it under the tenant
    // global-sentinel partition, not the caller's session. Read it back from there.
    let fetched = storage
        .entity_get_by_id(
            &ctx,
            ferrosa_memory_core::scope::tenant_global_session_uuid(ctx.tenant_id),
            entity_id,
        )
        .await
        .expect("fetch")
        .expect("exists");
    // Plain smart_ingest must leave the rich fields untouched (defaults).
    assert!(fetched.description.is_none());
    assert!(fetched.description_embedding.is_none());
    assert!(fetched.tags.is_empty());
    // "decision" is a durable, cross-session type, so smart_ingest must scope it
    // Global via default_scope_for (Issue 13). This test previously asserted
    // Session, which codified the bug where every ingest fell through to the
    // Session default regardless of type.
    assert_eq!(fetched.scope, EntityScope::Global);
    // Core fields populated normally.
    assert_eq!(fetched.entity_name, "rate-limit-policy");
    assert_eq!(fetched.entity_type, "decision");
}

#[tokio::test]
#[ignore]
async fn g4_typed_edge_round_trip_still_works_against_new_keyspace_qualification() {
    let Some(test_cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let storage = CqlStorage::connect(&base_cfg(&test_cfg))
        .await
        .expect("connect");
    let graph = graph_client(&test_cfg).await;
    run_migrations(storage.session(), storage.keyspace())
        .await
        .expect("migrate");

    let ctx = test_ctx();
    let session_id = Uuid::new_v4();
    let (src_id, dst_id) = (Uuid::new_v4(), Uuid::new_v4());

    let edge = TypedEdge {
        tenant_id: ctx.tenant_id,
        session_id,
        src_id,
        edge_type: "depends_on".into(),
        dst_id,
        weight: 0.8,
        metadata: Some("g4-regression".into()),
        created_at: chrono::Utc::now(),
    };
    graph
        .put_typed_edge(
            ctx.tenant_id,
            edge.session_id,
            edge.src_id,
            &edge.edge_type,
            edge.dst_id,
            edge.weight,
            edge.metadata.as_deref(),
        )
        .await
        .expect("put edge");

    let from_src = storage
        .typed_edge_list_from(&ctx, session_id, src_id)
        .await
        .expect("list from");
    assert!(
        from_src
            .iter()
            .any(|e| e.src_id == src_id && e.dst_id == dst_id && e.edge_type == "depends_on"),
        "typed_edge_list_from must return the edge we just wrote"
    );

    let session_edges = storage
        .typed_edge_list_session(&ctx, session_id)
        .await
        .expect("list session");
    assert!(
        session_edges
            .iter()
            .any(|e| e.src_id == src_id && e.dst_id == dst_id),
        "typed_edge_list_session must include the edge"
    );
}

#[tokio::test]
#[ignore]
async fn g4_entity_list_all_reads_across_sessions() {
    let Some(test_cfg) = TestClusterConfig::from_env_or_skip() else {
        return;
    };
    let storage = CqlStorage::connect(&base_cfg(&test_cfg))
        .await
        .expect("connect");
    run_migrations(storage.session(), storage.keyspace())
        .await
        .expect("migrate");

    let ctx = test_ctx();
    // Two session-scoped entities in distinct sessions.
    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();
    let e1_id = Uuid::new_v4();
    let e2_id = Uuid::new_v4();
    for (sid, eid, name) in [(s1, e1_id, "g4-s1-entity"), (s2, e2_id, "g4-s2-entity")] {
        let entry = EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: eid,
            session_id: sid,
            entity_name: name.into(),
            entity_type: "concept".into(),
            source_fold_id: None,
            context_snippet: "regression fixture".into(),
            entity_embedding: None,
            confidence: 1.0,
            state: MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        storage.entity_put(&ctx, &entry).await.expect("put");
    }

    let all = storage.entity_list_all(&ctx).await.expect("list_all");
    // entity_list_all should surface both.
    let ids: std::collections::HashSet<Uuid> = all.iter().map(|e| e.entity_id).collect();
    assert!(ids.contains(&e1_id), "entity_list_all missed s1 entity");
    assert!(ids.contains(&e2_id), "entity_list_all missed s2 entity");
}
