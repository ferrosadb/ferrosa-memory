//! Spreading activation -- Collins & Loftus semantic network retrieval.
//!
//! Propagates activation energy from seed entities through the knowledge graph,
//! decaying at each hop. Returns the most activated non-seed entities, enabling
//! associative recall through multi-hop graph structure.

use std::collections::HashMap;

use serde::Serialize;
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

/// A node that received activation energy during spreading.
#[derive(Debug, Clone, Serialize)]
pub struct ActivatedNode {
    pub entity_id: Uuid,
    pub activation: f64,
    pub hops: usize,
}

/// Spread activation from seed entities through the knowledge graph.
///
/// Seeds start with activation 1.0. At each hop, activation is multiplied by
/// `decay` and added to neighbors. Nodes below 0.01 activation are pruned.
/// Returns the top `limit` non-seed nodes sorted by activation (descending).
pub async fn spread(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    seeds: &[Uuid],
    max_hops: usize,
    decay: f64,
    limit: usize,
) -> anyhow::Result<Vec<ActivatedNode>> {
    anyhow::ensure!(!seeds.is_empty(), "seeds must not be empty");
    anyhow::ensure!((1..=5).contains(&max_hops), "max_hops must be 1..=5");
    anyhow::ensure!(decay > 0.0 && decay <= 1.0, "decay must be in (0, 1]");
    anyhow::ensure!((1..=50).contains(&limit), "limit must be 1..=50");

    // activation_map: entity_id -> (total_activation, min_hops)
    let mut activation_map: HashMap<Uuid, (f64, usize)> = HashMap::new();
    // frontier: (node_id, current_activation, hop_depth)
    let mut frontier: Vec<(Uuid, f64, usize)> = seeds.iter().map(|id| (*id, 1.0, 0)).collect();

    for seed in seeds {
        activation_map.insert(*seed, (1.0, 0));
    }

    while let Some((node_id, current_activation, hop)) = frontier.pop() {
        if hop >= max_hops || current_activation < 0.01 {
            continue;
        }

        let neighbors = storage.edge_list_for_entity(ctx, node_id).await?;
        let spread_activation = current_activation * decay;

        for (neighbor_id, _edge_type) in neighbors {
            let entry = activation_map.entry(neighbor_id).or_insert((0.0, hop + 1));
            entry.0 += spread_activation;
            if entry.1 > hop + 1 {
                entry.1 = hop + 1;
            }
            if hop + 1 < max_hops {
                frontier.push((neighbor_id, spread_activation, hop + 1));
            }
        }
    }

    let mut results: Vec<ActivatedNode> = activation_map
        .into_iter()
        .filter(|(id, _)| !seeds.contains(id))
        .map(|(entity_id, (act, hops))| ActivatedNode {
            entity_id,
            activation: act,
            hops,
        })
        .collect();
    results.sort_by(|a, b| {
        b.activation
            .partial_cmp(&a.activation)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
    Ok(results)
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

    #[tokio::test]
    async fn spread_single_hop_decays_correctly() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();

        let seed = Uuid::new_v4();
        let neighbor_a = Uuid::new_v4();
        let neighbor_b = Uuid::new_v4();

        // seed --co_occurs_with--> neighbor_a
        // seed --co_occurs_with--> neighbor_b
        store
            .edge_co_occurs(&ctx, seed, neighbor_a, session, 1.0)
            .await
            .unwrap();
        store
            .edge_co_occurs(&ctx, seed, neighbor_b, session, 1.0)
            .await
            .unwrap();

        let results = spread(&store, &ctx, &[seed], 2, 0.7, 10).await.unwrap();
        assert_eq!(results.len(), 2);

        // Both neighbors should have activation = 1.0 * 0.7 = 0.7
        for node in &results {
            assert!(
                (node.activation - 0.7).abs() < 1e-9,
                "expected 0.7, got {}",
                node.activation
            );
            assert_eq!(node.hops, 1);
        }
    }

    #[tokio::test]
    async fn spread_two_hops_accumulates() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();

        let seed = Uuid::new_v4();
        let middle = Uuid::new_v4();
        let leaf = Uuid::new_v4();

        // seed --CO_OCCURS--> middle --CO_OCCURS--> leaf
        store
            .edge_co_occurs(&ctx, seed, middle, session, 1.0)
            .await
            .unwrap();
        store
            .edge_co_occurs(&ctx, middle, leaf, session, 1.0)
            .await
            .unwrap();

        let results = spread(&store, &ctx, &[seed], 3, 0.5, 10).await.unwrap();

        // middle: activation = 0.5 (from seed)
        // leaf: activation = 0.5 * 0.5 = 0.25 (from middle)
        let middle_node = results.iter().find(|n| n.entity_id == middle).unwrap();
        assert!(
            middle_node.activation >= 0.5,
            "middle activation: {}",
            middle_node.activation
        );
        assert_eq!(middle_node.hops, 1);

        let leaf_node = results.iter().find(|n| n.entity_id == leaf);
        assert!(leaf_node.is_some(), "leaf should be activated");
    }

    #[tokio::test]
    async fn spread_excludes_seeds() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();

        let seed = Uuid::new_v4();
        let neighbor = Uuid::new_v4();

        store
            .edge_co_occurs(&ctx, seed, neighbor, session, 1.0)
            .await
            .unwrap();

        let results = spread(&store, &ctx, &[seed], 2, 0.7, 10).await.unwrap();
        assert!(!results.iter().any(|n| n.entity_id == seed));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entity_id, neighbor);
    }

    #[tokio::test]
    async fn spread_respects_limit() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();

        let seed = Uuid::new_v4();
        // Create 5 neighbors
        for _ in 0..5 {
            let neighbor = Uuid::new_v4();
            store
                .edge_co_occurs(&ctx, seed, neighbor, session, 1.0)
                .await
                .unwrap();
        }

        let results = spread(&store, &ctx, &[seed], 2, 0.7, 3).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn spread_validates_inputs() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        // Empty seeds
        let err = spread(&store, &ctx, &[], 2, 0.7, 10).await;
        assert!(err.is_err());

        // max_hops out of range
        let seed = Uuid::new_v4();
        let err = spread(&store, &ctx, &[seed], 0, 0.7, 10).await;
        assert!(err.is_err());
        let err = spread(&store, &ctx, &[seed], 6, 0.7, 10).await;
        assert!(err.is_err());

        // decay out of range
        let err = spread(&store, &ctx, &[seed], 2, 0.0, 10).await;
        assert!(err.is_err());
        let err = spread(&store, &ctx, &[seed], 2, 1.5, 10).await;
        assert!(err.is_err());

        // limit out of range
        let err = spread(&store, &ctx, &[seed], 2, 0.7, 0).await;
        assert!(err.is_err());
        let err = spread(&store, &ctx, &[seed], 2, 0.7, 51).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn spread_no_neighbors_returns_empty() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let seed = Uuid::new_v4();

        let results = spread(&store, &ctx, &[seed], 2, 0.7, 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn spread_sorted_by_activation_descending() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();

        let seed = Uuid::new_v4();
        let neighbor_a = Uuid::new_v4();
        let neighbor_b = Uuid::new_v4();

        // seed --CO_OCCURS--> neighbor_a
        // seed --CO_OCCURS--> neighbor_b
        // neighbor_a --CO_OCCURS--> neighbor_b (gives b extra activation)
        store
            .edge_co_occurs(&ctx, seed, neighbor_a, session, 1.0)
            .await
            .unwrap();
        store
            .edge_co_occurs(&ctx, seed, neighbor_b, session, 1.0)
            .await
            .unwrap();
        store
            .edge_co_occurs(&ctx, neighbor_a, neighbor_b, session, 1.0)
            .await
            .unwrap();

        let results = spread(&store, &ctx, &[seed], 3, 0.5, 10).await.unwrap();
        // neighbor_b should have higher activation (direct + indirect via neighbor_a)
        assert!(results.len() >= 2);
        // Verify descending sort
        for pair in results.windows(2) {
            assert!(pair[0].activation >= pair[1].activation);
        }
    }
}
