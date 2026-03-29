//! Personalized PageRank via power iteration.
//!
//! Computes authority scores over the entity graph seeded from recently
//! accessed entities. Writes scores to the warmth table for 5-signal fusion.

use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::config::RmhConfig;
use crate::storage::Storage;
use crate::types::{DecayZone, TenantContext, WarmthEntry};

/// Compute Personalized PageRank via power iteration.
///
/// Algorithm:
/// 1. Build adjacency from `edge_list_session`
/// 2. Initialize: seeds get their normalized weight, others get 0
/// 3. Power iteration: `pr[v] = (1-alpha) * sum(pr[u] / outdeg[u]) + alpha * seed[v]`
/// 4. Return final scores
pub async fn compute_ppr(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    config: &RmhConfig,
    seeds: &HashMap<Uuid, f64>,
) -> anyhow::Result<HashMap<Uuid, f64>> {
    let edges = storage.edge_list_session(ctx, session_id).await?;

    // Build adjacency and collect all nodes
    let mut outgoing: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut incoming: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut nodes: HashSet<Uuid> = HashSet::new();

    for (src, dst, _edge_type) in &edges {
        outgoing.entry(*src).or_default().push(*dst);
        incoming.entry(*dst).or_default().push(*src);
        nodes.insert(*src);
        nodes.insert(*dst);
    }

    if nodes.is_empty() {
        return Ok(HashMap::new());
    }

    let alpha = config.ppr_alpha;

    // Normalize seed weights to sum to 1.0, or use uniform if no seeds
    let seed_sum: f64 = seeds.values().sum();
    let personalization: HashMap<Uuid, f64> = if seed_sum > 0.0 {
        seeds.iter().map(|(k, v)| (*k, v / seed_sum)).collect()
    } else {
        let uniform = 1.0 / nodes.len() as f64;
        nodes.iter().map(|n| (*n, uniform)).collect()
    };

    // Initialize scores from personalization vector
    let mut scores: HashMap<Uuid, f64> = HashMap::with_capacity(nodes.len());
    for node in &nodes {
        let seed_val = personalization.get(node).unwrap_or(&0.0);
        scores.insert(*node, *seed_val);
    }

    // Power iteration
    for _iter in 0..config.ppr_iterations {
        let mut new_scores: HashMap<Uuid, f64> = HashMap::with_capacity(nodes.len());

        for node in &nodes {
            let walk_sum: f64 = incoming
                .get(node)
                .map(|in_neighbors| {
                    in_neighbors
                        .iter()
                        .map(|u| {
                            let out_deg = outgoing.get(u).map(|v| v.len()).unwrap_or(1) as f64;
                            scores.get(u).unwrap_or(&0.0) / out_deg
                        })
                        .sum()
                })
                .unwrap_or(0.0);

            let seed_val = personalization.get(node).unwrap_or(&0.0);
            new_scores.insert(*node, (1.0 - alpha) * walk_sum + alpha * seed_val);
        }

        scores = new_scores;
    }

    Ok(scores)
}

/// Write PPR scores to the warmth table's pagerank column.
///
/// For each entity in `ranks`, either updates an existing warmth entry or
/// creates a new one with the computed pagerank score.
pub async fn update_pagerank_scores(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    ranks: &HashMap<Uuid, f64>,
) -> anyhow::Result<()> {
    for (entity_id, score) in ranks {
        match storage.warmth_get(ctx, *entity_id).await? {
            Some(mut entry) => {
                entry.pagerank = *score;
                entry.updated_at = chrono::Utc::now();
                storage.warmth_put(ctx, &entry).await?;
            }
            None => {
                let entry = WarmthEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: *entity_id,
                    session_id,
                    warmth: 0.0,
                    pagerank: *score,
                    last_accessed_at: chrono::Utc::now(),
                    access_count: 0,
                    decay_zone: DecayZone::Knowledge,
                    updated_at: chrono::Utc::now(),
                };
                storage.warmth_put(ctx, &entry).await?;
            }
        }
    }
    Ok(())
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
    async fn test_empty_graph() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let config = RmhConfig::default();
        let ranks = compute_ppr(&storage, &ctx, Uuid::new_v4(), &config, &HashMap::new())
            .await
            .unwrap();
        assert!(ranks.is_empty());
    }

    #[tokio::test]
    async fn test_simple_chain() {
        // A -> B -> C
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        storage.edge_co_occurs(&ctx, a, b, sid, 1.0).await.unwrap();
        storage.edge_co_occurs(&ctx, b, c, sid, 1.0).await.unwrap();

        let config = RmhConfig::default();
        let mut seeds = HashMap::new();
        seeds.insert(a, 1.0);

        let ranks = compute_ppr(&storage, &ctx, sid, &config, &seeds)
            .await
            .unwrap();

        // A should have highest score (it's the seed)
        assert!(ranks.get(&a).unwrap_or(&0.0) > ranks.get(&c).unwrap_or(&0.0));
    }

    #[tokio::test]
    async fn test_scores_nonnegative() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        storage.edge_co_occurs(&ctx, a, b, sid, 1.0).await.unwrap();

        let config = RmhConfig::default();
        let ranks = compute_ppr(&storage, &ctx, sid, &config, &HashMap::new())
            .await
            .unwrap();

        for score in ranks.values() {
            assert!(*score >= 0.0);
        }
    }

    #[tokio::test]
    async fn test_personalization_effect() {
        // Star graph: center -> A, center -> B
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let center = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        storage
            .edge_co_occurs(&ctx, center, a, sid, 1.0)
            .await
            .unwrap();
        storage
            .edge_co_occurs(&ctx, center, b, sid, 1.0)
            .await
            .unwrap();

        let config = RmhConfig::default();
        let mut seeds = HashMap::new();
        seeds.insert(a, 1.0); // Seed only A

        let ranks = compute_ppr(&storage, &ctx, sid, &config, &seeds)
            .await
            .unwrap();

        // A (seeded) should have higher score than B (not seeded)
        assert!(ranks.get(&a).unwrap_or(&0.0) > ranks.get(&b).unwrap_or(&0.0));
    }

    #[tokio::test]
    async fn test_update_pagerank_scores_creates_entry() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let eid = Uuid::new_v4();

        let mut ranks = HashMap::new();
        ranks.insert(eid, 0.75);

        update_pagerank_scores(&storage, &ctx, sid, &ranks)
            .await
            .unwrap();

        let entry = storage.warmth_get(&ctx, eid).await.unwrap().unwrap();
        assert!((entry.pagerank - 0.75).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_update_pagerank_scores_updates_existing() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let eid = Uuid::new_v4();

        // Create an existing warmth entry
        let existing = WarmthEntry {
            tenant_id: ctx.tenant_id,
            entity_id: eid,
            session_id: sid,
            warmth: 5.0,
            pagerank: 0.1,
            last_accessed_at: chrono::Utc::now(),
            access_count: 3,
            decay_zone: DecayZone::Identity,
            updated_at: chrono::Utc::now(),
        };
        storage.warmth_put(&ctx, &existing).await.unwrap();

        // Update with new pagerank score
        let mut ranks = HashMap::new();
        ranks.insert(eid, 0.9);
        update_pagerank_scores(&storage, &ctx, sid, &ranks)
            .await
            .unwrap();

        let entry = storage.warmth_get(&ctx, eid).await.unwrap().unwrap();
        assert!((entry.pagerank - 0.9).abs() < f64::EPSILON);
        // Warmth and other fields should be preserved
        assert!((entry.warmth - 5.0).abs() < f64::EPSILON);
        assert_eq!(entry.access_count, 3);
        assert_eq!(entry.decay_zone, DecayZone::Identity);
    }
}
