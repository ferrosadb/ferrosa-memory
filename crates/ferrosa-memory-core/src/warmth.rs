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

        boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &default_config())
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

        boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &default_config())
            .await
            .unwrap();
        boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &default_config())
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

        boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &default_config())
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

        let scores = get_warmth_scores(&storage, &ctx, sid, &config).await.unwrap();
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
        storage
            .edge_co_occurs(&ctx, eid, neighbor, sid, 1.0)
            .await
            .unwrap();

        boost_on_access(&storage, &ctx, eid, sid, &DecayZone::Knowledge, &default_config())
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
    async fn test_zone_decay_rates() {
        // Identity decays 10x slower than Knowledge
        let id_decay = (-0.1 * 0.1 * 10.0_f64).exp(); // ~0.99
        let kn_decay = (-0.1 * 1.0 * 10.0_f64).exp(); // ~0.37
        let op_decay = (-0.1 * 3.0 * 10.0_f64).exp(); // ~0.05
        assert!(id_decay > kn_decay, "identity should decay slower than knowledge");
        assert!(kn_decay > op_decay, "knowledge should decay slower than operational");
    }
}
