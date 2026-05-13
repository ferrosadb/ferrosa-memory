//! Persistent warmth field with Ebbinghaus decay.
//!
//! Warmth accumulates on entity access and spreads to 1-hop neighbors.
//! Zone-based decay: Identity (0.1x), Knowledge (1.0x), Operational (3.0x).

use std::collections::HashMap;
use uuid::Uuid;

use crate::config::RmhConfig;
use crate::storage::Storage;
use crate::types::{DecayZone, TenantContext, WarmthEntry};

/// Boost an entity's warmth on access and spread activation to 1-hop neighbors.
///
/// If the entity already has a warmth entry, its warmth is incremented (capped at
/// `config.warmth_cap`), access_count is bumped, and last_accessed_at is updated.
/// If absent, a new entry is created with initial warmth = `config.warmth_boost_amount`.
///
/// After updating the primary entity, neighbor entities receive a fraction of the
/// boost (`warmth_boost_amount * warmth_neighbor_ratio`). Neighbor failures are
/// logged but do not fail the operation.
pub async fn boost_on_access(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
    session_id: Uuid,
    decay_zone: &DecayZone,
    config: &RmhConfig,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();

    let entry = match storage.warmth_get(ctx, entity_id).await? {
        Some(existing) => WarmthEntry {
            warmth: (existing.warmth + config.warmth_boost_amount).min(config.warmth_cap),
            access_count: existing.access_count + 1,
            last_accessed_at: now,
            updated_at: now,
            ..existing
        },
        None => WarmthEntry {
            tenant_id: ctx.tenant_id,
            entity_id,
            session_id,
            warmth: config.warmth_boost_amount,
            pagerank: 0.0,
            reputation: 0.0,
            last_accessed_at: now,
            access_count: 1,
            decay_zone: decay_zone.clone(),
            updated_at: now,
        },
    };

    storage.warmth_put(ctx, &entry).await?;

    // Spread to 1-hop neighbors
    let neighbor_amount = config.warmth_boost_amount * config.warmth_neighbor_ratio;
    let neighbors = storage.edge_list_for_entity(ctx, entity_id).await?;

    for (neighbor_id, _edge_type) in neighbors {
        if let Err(e) = storage
            .warmth_boost(ctx, neighbor_id, neighbor_amount, session_id)
            .await
        {
            tracing::warn!(
                %entity_id,
                %neighbor_id,
                error = %e,
                "failed to spread warmth to neighbor"
            );
        }
    }

    Ok(())
}

/// Compute the live warmth score for an entity applying Ebbinghaus decay.
///
/// Returns 0.0 if the entity has no warmth entry. Otherwise, applies
/// exponential decay: `warmth * exp(-decay_lambda * zone_multiplier * elapsed_hours)`.
pub async fn compute_warmth_score(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
    config: &RmhConfig,
) -> anyhow::Result<f64> {
    let entry = match storage.warmth_get(ctx, entity_id).await? {
        Some(e) => e,
        None => return Ok(0.0),
    };

    let elapsed_hours = chrono::Utc::now()
        .signed_duration_since(entry.last_accessed_at)
        .num_milliseconds() as f64
        / 3_600_000.0;

    let score = entry.warmth
        * (-config.decay_lambda * entry.decay_zone.decay_multiplier() * elapsed_hours).exp();

    Ok(score)
}

/// Run a decay pass over all warmth entries in a session.
///
/// Finds the maximum idle time among entries and applies bulk decay via
/// `storage.warmth_decay_all`. Returns the number of entries pruned
/// (those that fell below the prune threshold).
pub async fn run_decay_pass(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    config: &RmhConfig,
) -> anyhow::Result<usize> {
    let entries = storage.warmth_list_session(ctx, session_id).await?;

    if entries.is_empty() {
        return Ok(0);
    }

    let now = chrono::Utc::now();
    let elapsed_hours = entries
        .iter()
        .map(|e| {
            now.signed_duration_since(e.last_accessed_at)
                .num_milliseconds() as f64
                / 3_600_000.0
        })
        .fold(0.0_f64, f64::max);

    let _ = config; // config passed through for future threshold tuning
    let pruned = storage
        .warmth_decay_all(ctx, session_id, elapsed_hours)
        .await?;

    Ok(pruned)
}

/// Remove warmth entries below threshold (soft-delete).
pub async fn prune_forgotten(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    threshold: f64,
    config: &RmhConfig,
) -> anyhow::Result<usize> {
    let entries = storage.warmth_list_session(ctx, session_id).await?;
    let mut pruned = 0;
    for entry in entries {
        let score = compute_warmth_score(storage, ctx, entry.entity_id, config).await?;
        if score < threshold {
            if let Err(e) = storage.warmth_delete(ctx, entry.entity_id).await {
                tracing::warn!(entity_id=%entry.entity_id, error=%e, "failed to prune forgotten entity");
            } else {
                pruned += 1;
            }
        }
    }
    Ok(pruned)
}

/// Retrieve live warmth scores for all entities in a session.
///
/// Lists warmth entries from storage and applies Ebbinghaus decay to each,
/// returning a map of entity_id to current (decayed) warmth score.
pub async fn get_warmth_scores(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    config: &RmhConfig,
) -> anyhow::Result<HashMap<Uuid, f64>> {
    let entries = storage.warmth_list_session(ctx, session_id).await?;
    let now = chrono::Utc::now();

    let scores = entries
        .iter()
        .map(|entry| {
            let elapsed_hours = now
                .signed_duration_since(entry.last_accessed_at)
                .num_milliseconds() as f64
                / 3_600_000.0;

            let score = entry.warmth
                * (-config.decay_lambda * entry.decay_zone.decay_multiplier() * elapsed_hours)
                    .exp();

            (entry.entity_id, score)
        })
        .collect();

    Ok(scores)
}

/// Apply an outcome-based warmth boost or penalty to an entity.
///
/// When an entity is retrieved and the user reports success, boost its warmth
/// so the system prioritizes it in future retrieval.  On failure, penalize it
/// so the system deprioritizes the memory.
///
/// This closes the episodic feedback loop: `record_outcome` → `apply_outcome_boost`
/// → future `hybrid_search` ranking.
pub async fn apply_outcome_boost(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
    succeeded: bool,
    latency_ms: i32,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let entry = match storage.warmth_get(ctx, entity_id).await? {
        Some(mut existing) => {
            let delta = if succeeded {
                // Success + fast = larger boost
                if latency_ms < 50 { 0.30 } else { 0.15 }
            } else {
                // Failure = penalty (clamped to keep warmth positive in intent)
                -0.20
            };
            existing.warmth = (existing.warmth + delta).max(0.0);
            existing.last_accessed_at = now;
            existing.updated_at = now;
            existing
        }
        None => WarmthEntry {
            tenant_id: ctx.tenant_id,
            entity_id,
            session_id: Uuid::nil(),
            warmth: if succeeded { 0.15 } else { 0.0 },
            pagerank: 0.0,
            reputation: 0.0,
            last_accessed_at: now,
            access_count: 0,
            decay_zone: DecayZone::Knowledge,
            updated_at: now,
        },
    };
    storage.warmth_put(ctx, &entry).await
}

/// Convenience wrapper: penalize warmth for failed retrieval.
/// Useful when a user explicitly flags an entity as incorrect / unhelpful.
pub async fn warmth_penalty(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
    amount: f64,
) -> anyhow::Result<()> {
    apply_outcome_boost(storage, ctx, entity_id, false, 999)
        .await
        .ok();
    // If entity exists, subtract additional `amount` from warmth
    if let Some(mut existing) = storage.warmth_get(ctx, entity_id).await? {
        let now = chrono::Utc::now();
        existing.warmth = (existing.warmth - amount).max(0.0);
        existing.last_accessed_at = now;
        existing.updated_at = now;
        storage.warmth_put(ctx, &existing).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use crate::types::TenantContext;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    fn default_config() -> RmhConfig {
        RmhConfig::default()
    }

    #[tokio::test]
    async fn test_boost_creates_entry() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();

        boost_on_access(
            &storage,
            &ctx,
            eid,
            sid,
            &DecayZone::Knowledge,
            &default_config(),
        )
        .await
        .unwrap();

        let entry = storage.warmth_get(&ctx, eid).await.unwrap().unwrap();
        assert!(
            (entry.warmth - 0.3).abs() < 0.01,
            "expected warmth ~0.3, got {}",
            entry.warmth
        );
        assert_eq!(entry.access_count, 1);
    }

    #[tokio::test]
    async fn test_boost_accumulates() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();

        boost_on_access(
            &storage,
            &ctx,
            eid,
            sid,
            &DecayZone::Knowledge,
            &default_config(),
        )
        .await
        .unwrap();
        boost_on_access(
            &storage,
            &ctx,
            eid,
            sid,
            &DecayZone::Knowledge,
            &default_config(),
        )
        .await
        .unwrap();

        let entry = storage.warmth_get(&ctx, eid).await.unwrap().unwrap();
        assert!(
            (entry.warmth - 0.6).abs() < 0.01,
            "expected warmth ~0.6, got {}",
            entry.warmth
        );
        assert_eq!(entry.access_count, 2);
    }

    #[tokio::test]
    async fn test_warmth_cap() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();
        let config = RmhConfig {
            warmth_cap: 1.0,
            warmth_boost_amount: 0.5,
            ..Default::default()
        };

        for _ in 0..10 {
            boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &config)
                .await
                .unwrap();
        }

        let entry = storage.warmth_get(&ctx, eid).await.unwrap().unwrap();
        assert!(
            entry.warmth <= 1.0 + f64::EPSILON,
            "warmth {} exceeded cap 1.0",
            entry.warmth
        );
    }

    #[tokio::test]
    async fn test_compute_warmth_score_absent() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let score = compute_warmth_score(&storage, &ctx, Uuid::new_v4(), &default_config())
            .await
            .unwrap();
        assert!(
            (score - 0.0).abs() < f64::EPSILON,
            "expected 0.0 for absent entity, got {}",
            score
        );
    }

    #[tokio::test]
    async fn test_compute_warmth_score_recent() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();

        boost_on_access(
            &storage,
            &ctx,
            eid,
            sid,
            &DecayZone::Knowledge,
            &default_config(),
        )
        .await
        .unwrap();

        // Immediately after boost, score should be very close to warmth (near-zero elapsed)
        let score = compute_warmth_score(&storage, &ctx, eid, &default_config())
            .await
            .unwrap();
        assert!(
            (score - 0.3).abs() < 0.01,
            "expected score ~0.3, got {}",
            score
        );
    }

    #[tokio::test]
    async fn test_get_warmth_scores() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let eid1 = Uuid::new_v4();
        let eid2 = Uuid::new_v4();
        let config = default_config();

        boost_on_access(&storage, &ctx, eid1, sid, &DecayZone::Knowledge, &config)
            .await
            .unwrap();
        boost_on_access(&storage, &ctx, eid2, sid, &DecayZone::Identity, &config)
            .await
            .unwrap();

        let scores = get_warmth_scores(&storage, &ctx, sid, &config)
            .await
            .unwrap();
        assert_eq!(scores.len(), 2);
        assert!(scores.contains_key(&eid1));
        assert!(scores.contains_key(&eid2));
    }

    #[tokio::test]
    async fn test_run_decay_pass_empty() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let pruned = run_decay_pass(&storage, &ctx, sid, &default_config())
            .await
            .unwrap();
        assert_eq!(pruned, 0);
    }

    #[tokio::test]
    async fn test_boost_spreads_to_neighbors() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let eid = Uuid::new_v4();
        let neighbor = Uuid::new_v4();

        // Create an edge: eid --co_occurs_with--> neighbor
        crate::graph_write::reinforce_co_occurs_edge(&storage, &ctx, eid, neighbor, sid, 1.0)
            .await
            .unwrap();

        boost_on_access(
            &storage,
            &ctx,
            eid,
            sid,
            &DecayZone::Knowledge,
            &default_config(),
        )
        .await
        .unwrap();

        // Neighbor should have received warmth_boost_amount * warmth_neighbor_ratio = 0.3 * 0.5 = 0.15
        let neighbor_entry = storage.warmth_get(&ctx, neighbor).await.unwrap().unwrap();
        assert!(
            (neighbor_entry.warmth - 0.15).abs() < 0.01,
            "expected neighbor warmth ~0.15, got {}",
            neighbor_entry.warmth
        );
    }

    #[tokio::test]
    async fn test_outcome_boost_increases_warmth() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();
        let config = default_config();

        // Prime with base warmth
        boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &config)
            .await
            .unwrap();
        let before = storage.warmth_get(&ctx, eid).await.unwrap().unwrap().warmth;

        // Success outcome should add +0.15
        apply_outcome_boost(&storage, &ctx, eid, true, 100)
            .await
            .unwrap();
        let after = storage.warmth_get(&ctx, eid).await.unwrap().unwrap().warmth;
        assert!(
            (after - before - 0.15).abs() < 0.01,
            "expected +0.15 boost, before={before}, after={after}"
        );
    }

    #[tokio::test]
    async fn test_fast_success_bigger_boost() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();
        let config = default_config();

        boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &config)
            .await
            .unwrap();
        let baseline = storage.warmth_get(&ctx, eid).await.unwrap().unwrap().warmth;

        apply_outcome_boost(&storage, &ctx, eid, true, 30)
            .await
            .unwrap();
        let fast = storage.warmth_get(&ctx, eid).await.unwrap().unwrap().warmth;

        assert!(
            fast - baseline >= 0.29,
            "fast success should get ~0.30 boost: baseline={baseline}, fast={fast}"
        );
    }

    #[tokio::test]
    async fn test_outcome_penalty_decreases_warmth() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();
        let config = default_config();

        boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &config)
            .await
            .unwrap();
        let before = storage.warmth_get(&ctx, eid).await.unwrap().unwrap().warmth;

        // Failure should subtract 0.20
        apply_outcome_boost(&storage, &ctx, eid, false, 100)
            .await
            .unwrap();
        let after = storage.warmth_get(&ctx, eid).await.unwrap().unwrap().warmth;
        assert!(
            (before - after - 0.20).abs() < 0.01,
            "expected -0.20 penalty, before={before}, after={after}"
        );
    }

    #[tokio::test]
    async fn test_outcome_boost_for_absent() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();

        // Entity has no prior warmth entry
        assert!(storage.warmth_get(&ctx, eid).await.unwrap().is_none());

        apply_outcome_boost(&storage, &ctx, eid, true, 100)
            .await
            .unwrap();
        let entry = storage.warmth_get(&ctx, eid).await.unwrap().unwrap();
        assert!(
            (entry.warmth - 0.15).abs() < 0.01,
            "expected initial warmth 0.15 for success, got {}",
            entry.warmth
        );
    }

    #[tokio::test]
    async fn test_warmth_penalty_clamped() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();

        // Low initial warmth
        apply_outcome_boost(&storage, &ctx, eid, true, 100)
            .await
            .unwrap();

        // Large penalty should clamp at 0
        warmth_penalty(&storage, &ctx, eid, 10.0).await.unwrap();
        let after = storage.warmth_get(&ctx, eid).await.unwrap().unwrap().warmth;
        assert_eq!(after, 0.0, "warmth must never go negative");
    }
}
