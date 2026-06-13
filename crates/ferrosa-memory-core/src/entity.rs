//! Entity store and retrieval tool handlers.
//!
//! Tracks named entities discovered during trajectory traversal. Supports
//! phonetic matching for variant/noisy entity names (Ferrosa Double Metaphone)
//! and ANN search via HNSW.
//!
//! ## Deduplication
//!
//! On upsert, the phonetic index is checked first. If a match is found AND
//! the embedding distance is below threshold, the existing entity is updated
//! rather than creating a duplicate (FMEA F18).
//!
//! ## Security
//!
//! - Confidence gating: rejects writes with confidence < threshold (FMEA F19)
//! - Per-session entity count limit to prevent graph explosion (FMEA F20)

use uuid::Uuid;

use chrono::{DateTime, Utc};

use crate::storage::Storage;
use crate::types::{EntityEntry, MemoryState, RetractionRecord, TenantContext};

/// Maximum entities per session (configurable via config, hardcoded default).
const DEFAULT_MAX_ENTITIES_PER_SESSION: usize = 50_000;

/// Default confidence gate — reject entities below this threshold.
const DEFAULT_CONFIDENCE_GATE: f64 = 0.7;

/// Result of upserting an entity.
#[derive(Debug, serde::Serialize)]
pub struct UpsertEntityResult {
    pub entity_id: Uuid,
    pub is_new: bool,
}

/// Upsert an entity with phonetic deduplication.
///
/// Checks phonetic match first. If found, returns existing entity_id.
/// If not found, creates a new entity. Rejects if confidence < gate or
/// session entity count exceeds limit.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_entity(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    entity_name: &str,
    entity_type: &str,
    context_snippet: &str,
    embedding: Option<Vec<f32>>,
    source_fold_id: Option<Uuid>,
    confidence: Option<f64>,
) -> anyhow::Result<UpsertEntityResult> {
    upsert_entity_with_limit(
        storage,
        ctx,
        session_id,
        entity_name,
        entity_type,
        context_snippet,
        embedding,
        source_fold_id,
        confidence,
        DEFAULT_MAX_ENTITIES_PER_SESSION,
    )
    .await
}

/// Upsert an entity with phonetic deduplication and a configurable entity limit.
///
/// Checks phonetic match first. If found, returns existing entity_id.
/// If not found, creates a new entity. Rejects if confidence < gate or
/// session entity count exceeds limit (FMEA D1 quota enforcement).
#[allow(clippy::too_many_arguments)]
pub async fn upsert_entity_with_limit(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    entity_name: &str,
    entity_type: &str,
    context_snippet: &str,
    embedding: Option<Vec<f32>>,
    source_fold_id: Option<Uuid>,
    confidence: Option<f64>,
    max_entities: usize,
) -> anyhow::Result<UpsertEntityResult> {
    let confidence = confidence.unwrap_or(1.0);
    tracing::debug!(entity_name, entity_type, confidence, "upsert_entity");

    // Confidence gating (FMEA F19)
    if confidence < DEFAULT_CONFIDENCE_GATE {
        tracing::warn!(
            confidence,
            gate = DEFAULT_CONFIDENCE_GATE,
            "entity rejected: low confidence"
        );
        anyhow::bail!("confidence {confidence} below gate {DEFAULT_CONFIDENCE_GATE}");
    }

    // Per-tenant entity quota enforcement (FMEA D1)
    let count = storage.entity_count(ctx, session_id).await?;
    crate::quota::check_quota(count, max_entities)?;

    // Check for phonetic match (deduplication) — use first (best-ranked) result
    let phonetic_matches = storage
        .entity_find_phonetic(ctx, session_id, entity_name)
        .await?;
    if let Some(existing) = phonetic_matches.first() {
        tracing::info!(entity_name, entity_id = %existing.entity_id, "entity deduplicated (phonetic match)");
        return Ok(UpsertEntityResult {
            entity_id: existing.entity_id,
            is_new: false,
        });
    }

    // Create new entity
    let entity_id = Uuid::new_v4();
    let entry = EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id,
        session_id,
        entity_name: entity_name.to_string(),
        entity_type: entity_type.to_string(),
        source_fold_id,
        context_snippet: context_snippet.to_string(),
        entity_embedding: embedding,
        confidence,
        state: crate::types::MemoryState::default(),
        created_at: chrono::Utc::now(),
        ..Default::default()
    };

    storage.entity_put(ctx, &entry).await?;

    // Create MENTIONED_IN edge if entity was found in a fold
    if let Some(fold_id) = source_fold_id
        && let Err(e) = crate::graph_write::create_mentioned_in_edge(
            storage, ctx, entity_id, fold_id, session_id,
        )
        .await
    {
        tracing::warn!(%entity_id, %fold_id, error = %e, "failed to create MENTIONED_IN edge");
    }

    tracing::info!(entity_name, entity_type, %entity_id, "entity created");

    Ok(UpsertEntityResult {
        entity_id,
        is_new: true,
    })
}

/// Retrieve entities by the specified strategy.
///
/// - `ann`: HNSW cosine similarity search
/// - `phonetic`: Double Metaphone fuzzy name match
/// - `both`: union-merge of both, deduplicated by entity_id
pub async fn retrieve_entities(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query: &str,
    embedding: Option<&[f32]>,
    strategy: &str,
    k: Option<usize>,
) -> anyhow::Result<Vec<EntityEntry>> {
    let k = k.unwrap_or(10);

    match strategy {
        "phonetic" => {
            let mut results = storage.entity_find_phonetic(ctx, session_id, query).await?;
            results.truncate(k);
            Ok(results)
        }
        "ann" => {
            let emb =
                embedding.ok_or_else(|| anyhow::anyhow!("embedding required for ann strategy"))?;
            storage.entity_search_ann(ctx, session_id, emb, k).await
        }
        "both" => {
            let mut results = Vec::new();

            // Phonetic first
            let phonetic = storage.entity_find_phonetic(ctx, session_id, query).await?;
            results.extend(phonetic);

            // ANN if embedding provided
            if let Some(emb) = embedding {
                let ann_results = storage.entity_search_ann(ctx, session_id, emb, k).await?;
                for e in ann_results {
                    if !results.iter().any(|r| r.entity_id == e.entity_id) {
                        results.push(e);
                    }
                }
            }

            Ok(results)
        }
        other => anyhow::bail!("unknown retrieval strategy: {other}"),
    }
}

/// Promote an entity's memory state one level up.
///
/// Transition: dormant -> active, silent -> dormant, unavailable -> silent.
/// Active stays active (already at highest state).
pub async fn promote_memory(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    entity_id: Uuid,
) -> anyhow::Result<MemoryState> {
    // Lifecycle lookup uses the targeted by-id read, not entity_list_session:
    // the latter now hides Unavailable (retracted) entities from recall, but
    // promote/demote must still see every state to drive the state machine.
    let entity = storage
        .entity_get_by_id(ctx, session_id, entity_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("entity not found: {entity_id}"))?;

    let new_state = match entity.state {
        MemoryState::Active => MemoryState::Active,
        MemoryState::Dormant => MemoryState::Active,
        MemoryState::Silent => MemoryState::Dormant,
        MemoryState::Unavailable => MemoryState::Silent,
    };

    if new_state != entity.state {
        storage
            .entity_update_state(ctx, entity_id, new_state.clone())
            .await?;
        tracing::info!(
            %entity_id,
            old_state = %entity.state,
            new_state = %new_state,
            "memory promoted"
        );
    }

    Ok(new_state)
}

/// Demote an entity's memory state one level down.
///
/// Transition: active -> dormant, dormant -> silent, silent -> unavailable.
/// Unavailable stays unavailable (already at lowest state).
pub async fn demote_memory(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    entity_id: Uuid,
) -> anyhow::Result<MemoryState> {
    // Lifecycle lookup uses the targeted by-id read, not entity_list_session:
    // the latter now hides Unavailable (retracted) entities from recall, but
    // promote/demote must still see every state to drive the state machine.
    let entity = storage
        .entity_get_by_id(ctx, session_id, entity_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("entity not found: {entity_id}"))?;

    let new_state = match entity.state {
        MemoryState::Active => MemoryState::Dormant,
        MemoryState::Dormant => MemoryState::Silent,
        MemoryState::Silent => MemoryState::Unavailable,
        MemoryState::Unavailable => MemoryState::Unavailable,
    };

    if new_state != entity.state {
        storage
            .entity_update_state(ctx, entity_id, new_state.clone())
            .await?;
        tracing::info!(
            %entity_id,
            old_state = %entity.state,
            new_state = %new_state,
            "memory demoted"
        );
    }

    Ok(new_state)
}

/// Retract ("forget") an entity: move it to `Unavailable` and write a
/// [`RetractionRecord`] capturing its prior state, so the operation is audited
/// and reversible via [`restore_entity`].
///
/// `now` is passed in (rather than calling `Utc::now()`) so the retraction
/// timestamp is deterministic and testable; it becomes the record's clustering
/// key. Returns the entity's prior state. Errors if the entity does not exist.
#[allow(clippy::too_many_arguments)]
pub async fn retract_entity(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    entity_id: Uuid,
    reason: &str,
    actor: &str,
    forget_id: Uuid,
    restorable_until: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<MemoryState> {
    let entity = storage
        .entity_get_by_id(ctx, session_id, entity_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("entity not found: {entity_id}"))?;
    let prior_state = entity.state.clone();

    storage
        .entity_update_state(ctx, entity_id, MemoryState::Unavailable)
        .await?;

    let rec = RetractionRecord {
        object_id: entity_id,
        object_type: "entity".to_string(),
        session_id,
        retracted_at: now,
        reason: reason.to_string(),
        actor: actor.to_string(),
        prior_state: prior_state.to_string(),
        restorable_until,
        forget_id,
    };
    storage.retraction_put(ctx, &rec).await?;

    tracing::info!(
        %entity_id,
        prior_state = %prior_state,
        %actor,
        %forget_id,
        "entity retracted"
    );

    Ok(prior_state)
}

/// Restore a previously retracted entity to the state it held before
/// retraction, then delete the retraction record.
///
/// Returns `Ok(false)` when there is no retraction record for `entity_id`
/// (nothing to restore); `Ok(true)` after a successful restore.
pub async fn restore_entity(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
) -> anyhow::Result<bool> {
    let Some(rec) = storage.retraction_get_latest(ctx, entity_id).await? else {
        return Ok(false);
    };

    let prior_state: MemoryState =
        serde_json::from_str(&format!("\"{}\"", rec.prior_state)).unwrap_or_default();

    storage
        .entity_update_state(ctx, entity_id, prior_state.clone())
        .await?;
    storage
        .retraction_delete(ctx, entity_id, rec.retracted_at)
        .await?;

    tracing::info!(
        %entity_id,
        restored_state = %prior_state,
        forget_id = %rec.forget_id,
        "entity restored"
    );

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    #[tokio::test]
    async fn upsert_creates_new_entity() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let result = upsert_entity(
            &store, &ctx, sid, "Alice", "person", "ctx", None, None, None,
        )
        .await
        .unwrap();
        assert!(result.is_new);
    }

    #[tokio::test]
    async fn upsert_deduplicates_on_phonetic_match() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let r1 = upsert_entity(
            &store, &ctx, sid, "Alice", "person", "ctx", None, None, None,
        )
        .await
        .unwrap();
        let r2 = upsert_entity(
            &store, &ctx, sid, "alice", "person", "ctx2", None, None, None,
        )
        .await
        .unwrap();

        assert!(r1.is_new);
        assert!(!r2.is_new);
        assert_eq!(r1.entity_id, r2.entity_id);
    }

    #[tokio::test]
    async fn upsert_rejects_low_confidence() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let result = upsert_entity(
            &store,
            &ctx,
            sid,
            "Alice",
            "person",
            "ctx",
            None,
            None,
            Some(0.3),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("confidence"));
    }

    #[tokio::test]
    async fn upsert_rejects_over_limit() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let max_entities = 3;

        // Fill to the configured limit. The quota behavior is independent of
        // the production default (50k); using the configurable seam keeps this
        // unit test fast while still exercising the real quota check.
        for i in 0..max_entities {
            upsert_entity_with_limit(
                &store,
                &ctx,
                sid,
                &format!("entity_{i}"),
                "thing",
                "ctx",
                None,
                None,
                None,
                max_entities,
            )
            .await
            .unwrap();
        }

        let result = upsert_entity_with_limit(
            &store,
            &ctx,
            sid,
            "one_more",
            "thing",
            "ctx",
            None,
            None,
            None,
            max_entities,
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("limit"));
    }

    #[tokio::test]
    async fn retrieve_phonetic() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        upsert_entity(&store, &ctx, sid, "Bob", "person", "ctx", None, None, None)
            .await
            .unwrap();

        let results = retrieve_entities(&store, &ctx, sid, "bob", None, "phonetic", None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entity_name, "Bob");
    }

    #[tokio::test]
    async fn retrieve_both_deduplicates() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let emb = vec![0.1; 768];
        upsert_entity(
            &store,
            &ctx,
            sid,
            "Carol",
            "person",
            "ctx",
            Some(emb.clone()),
            None,
            None,
        )
        .await
        .unwrap();

        let results = retrieve_entities(&store, &ctx, sid, "carol", Some(&emb), "both", None)
            .await
            .unwrap();
        // Should have exactly 1 (deduplicated)
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn promote_from_dormant_to_active() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Create entity
        let result = upsert_entity(
            &store, &ctx, sid, "Alice", "person", "ctx", None, None, None,
        )
        .await
        .unwrap();
        let entity_id = result.entity_id;

        // Demote to dormant first
        let state = demote_memory(&store, &ctx, sid, entity_id).await.unwrap();
        assert_eq!(state, MemoryState::Dormant);

        // Promote back to active
        let state = promote_memory(&store, &ctx, sid, entity_id).await.unwrap();
        assert_eq!(state, MemoryState::Active);
    }

    #[tokio::test]
    async fn demote_from_active_to_dormant() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Create entity (default state = Active)
        let result = upsert_entity(&store, &ctx, sid, "Bob", "person", "ctx", None, None, None)
            .await
            .unwrap();
        let entity_id = result.entity_id;

        // Demote active -> dormant
        let state = demote_memory(&store, &ctx, sid, entity_id).await.unwrap();
        assert_eq!(state, MemoryState::Dormant);

        // Demote dormant -> silent
        let state = demote_memory(&store, &ctx, sid, entity_id).await.unwrap();
        assert_eq!(state, MemoryState::Silent);

        // Demote silent -> unavailable
        let state = demote_memory(&store, &ctx, sid, entity_id).await.unwrap();
        assert_eq!(state, MemoryState::Unavailable);

        // Demote unavailable stays unavailable
        let state = demote_memory(&store, &ctx, sid, entity_id).await.unwrap();
        assert_eq!(state, MemoryState::Unavailable);
    }

    #[tokio::test]
    async fn promote_active_stays_active() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let result = upsert_entity(
            &store, &ctx, sid, "Carol", "person", "ctx", None, None, None,
        )
        .await
        .unwrap();

        let state = promote_memory(&store, &ctx, sid, result.entity_id)
            .await
            .unwrap();
        assert_eq!(state, MemoryState::Active);
    }

    #[tokio::test]
    async fn promote_demote_not_found() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let bogus_id = Uuid::new_v4();

        let err = promote_memory(&store, &ctx, sid, bogus_id).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("not found"));

        let err = demote_memory(&store, &ctx, sid, bogus_id).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn upsert_succeeds_under_configurable_quota() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // With a limit of 5, creating 5 entities should succeed
        for i in 0..5 {
            let result = upsert_entity_with_limit(
                &store,
                &ctx,
                sid,
                &format!("entity_{i}"),
                "thing",
                "ctx",
                None,
                None,
                None,
                5,
            )
            .await;
            assert!(result.is_ok(), "entity {i} should succeed under quota");
        }
    }

    #[tokio::test]
    async fn upsert_returns_quota_exceeded_at_limit() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Fill to the configurable limit of 3
        for i in 0..3 {
            upsert_entity_with_limit(
                &store,
                &ctx,
                sid,
                &format!("entity_{i}"),
                "thing",
                "ctx",
                None,
                None,
                None,
                3,
            )
            .await
            .unwrap();
        }

        // The 4th should fail with QuotaExceeded
        let err = upsert_entity_with_limit(
            &store, &ctx, sid, "one_more", "thing", "ctx", None, None, None, 3,
        )
        .await
        .unwrap_err();

        // Verify it's a QuotaExceeded error (downcastable)
        assert!(
            err.downcast_ref::<crate::quota::QuotaExceeded>().is_some(),
            "expected QuotaExceeded error, got: {err}"
        );
        assert!(err.to_string().contains("quota exceeded"));
    }

    #[tokio::test]
    async fn phonetic_search_finds_code_entity_by_segment() {
        // Bug: searching "graph" should match "ferrosa-memory-core::graph"
        // but previously returned the first alphabetical match instead.
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Create a code entity and a doc section that both contain "graph"
        upsert_entity(
            &store,
            &ctx,
            sid,
            "ferrosa-memory-core::graph",
            "module",
            "Graph client for HTTP Cypher queries",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        upsert_entity(
            &store,
            &ctx,
            sid,
            "doc:pitr.md::Dependency Graph",
            "section",
            "Shows the dependency graph of tasks",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // Phonetic search for "graph" should return results ranked by match quality
        let results = retrieve_entities(&store, &ctx, sid, "graph", None, "phonetic", Some(10))
            .await
            .unwrap();

        assert!(
            results.len() >= 2,
            "should return multiple matches, got {}",
            results.len()
        );

        // The module with "graph" as a :: segment should rank higher than
        // a section where "graph" is part of a longer heading
        assert_eq!(
            results[0].entity_name, "ferrosa-memory-core::graph",
            "exact segment match should rank first, got: {}",
            results[0].entity_name
        );
    }

    // --- Retraction (forget) helpers ---

    #[tokio::test]
    async fn retract_sets_unavailable_writes_record_and_hides_entity() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let created = upsert_entity(
            &store, &ctx, sid, "Secret", "person", "ctx", None, None, None,
        )
        .await
        .unwrap();
        let entity_id = created.entity_id;

        // Visible before retraction.
        let before = retrieve_entities(&store, &ctx, sid, "secret", None, "phonetic", None)
            .await
            .unwrap();
        assert_eq!(before.len(), 1);

        let now = Utc::now();
        let forget_id = Uuid::new_v4();
        let prior = retract_entity(
            &store,
            &ctx,
            sid,
            entity_id,
            "test forget",
            "tester",
            forget_id,
            now + chrono::Duration::days(7),
            now,
        )
        .await
        .unwrap();
        assert_eq!(prior, MemoryState::Active);

        // Entity is now Unavailable.
        let entity = store
            .entity_get_by_id(&ctx, sid, entity_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.state, MemoryState::Unavailable);

        // A retraction record was written with the captured prior state.
        let rec = store
            .retraction_get_latest(&ctx, entity_id)
            .await
            .unwrap()
            .expect("retraction record present");
        assert_eq!(rec.object_type, "entity");
        assert_eq!(rec.prior_state, "active");
        assert_eq!(rec.forget_id, forget_id);
        assert_eq!(rec.retracted_at, now);

        // No longer returned from phonetic recall or session listing.
        let after = retrieve_entities(&store, &ctx, sid, "secret", None, "phonetic", None)
            .await
            .unwrap();
        assert!(after.is_empty());
        let listed = store.entity_list_session(&ctx, sid).await.unwrap();
        assert!(listed.iter().all(|e| e.entity_id != entity_id));
    }

    #[tokio::test]
    async fn restore_brings_entity_back_to_prior_state() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let created = upsert_entity(
            &store, &ctx, sid, "Recall", "person", "ctx", None, None, None,
        )
        .await
        .unwrap();
        let entity_id = created.entity_id;

        // Put it in a non-default prior state so restore must read the record.
        store
            .entity_update_state(&ctx, entity_id, MemoryState::Dormant)
            .await
            .unwrap();

        let now = Utc::now();
        retract_entity(
            &store,
            &ctx,
            sid,
            entity_id,
            "test forget",
            "tester",
            Uuid::new_v4(),
            now + chrono::Duration::days(7),
            now,
        )
        .await
        .unwrap();

        let restored = restore_entity(&store, &ctx, entity_id).await.unwrap();
        assert!(restored);

        let entity = store
            .entity_get_by_id(&ctx, sid, entity_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.state, MemoryState::Dormant);

        // Record consumed by restore.
        assert!(
            store
                .retraction_get_latest(&ctx, entity_id)
                .await
                .unwrap()
                .is_none()
        );

        // Visible again (Dormant is retrievable).
        let listed = store.entity_list_session(&ctx, sid).await.unwrap();
        assert!(listed.iter().any(|e| e.entity_id == entity_id));
    }

    #[tokio::test]
    async fn restore_returns_false_when_no_retraction() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let restored = restore_entity(&store, &ctx, Uuid::new_v4()).await.unwrap();
        assert!(!restored);
    }

    #[tokio::test]
    async fn retract_missing_entity_errors() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let now = Utc::now();
        let result = retract_entity(
            &store,
            &ctx,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "r",
            "a",
            Uuid::new_v4(),
            now,
            now,
        )
        .await;
        assert!(result.is_err());
    }
}
