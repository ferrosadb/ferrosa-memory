//! Spreading activation -- Collins & Loftus semantic network retrieval.
//!
//! Propagates activation energy from seed entities through the knowledge graph,
//! decaying at each hop. Returns the most activated non-seed entities, enabling
//! associative recall through multi-hop graph structure.

use std::collections::{HashMap, HashSet};

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
    session_id: Option<Uuid>,
    max_hops: usize,
    decay: f64,
    limit: usize,
) -> anyhow::Result<Vec<ActivatedNode>> {
    anyhow::ensure!(!seeds.is_empty(), "seeds must not be empty");
    anyhow::ensure!((1..=5).contains(&max_hops), "max_hops must be 1..=5");
    anyhow::ensure!(decay > 0.0 && decay <= 1.0, "decay must be in (0, 1]");
    anyhow::ensure!((1..=50).contains(&limit), "limit must be 1..=50");

    // Bound traversal work to keep high-fanout graphs from turning a small
    // result request into unbounded N+1 edge-list scans.
    let max_expansions = limit.saturating_mul(2).max(seeds.len()).min(100);
    let mut expanded_count = 0usize;

    // Preload the tenant graph once and do traversal in memory. The previous
    // implementation called edge_list_for_entity() for each frontier node,
    // which turns dense/cyclic graphs into hundreds of serial CQL reads. On
    // the current small DB, one bounded edge preload is much cheaper and also
    // avoids saturating the MCP request budget.
    let mut adjacency: HashMap<Uuid, Vec<(Uuid, String)>> = HashMap::new();
    let edge_rows = match session_id {
        Some(session_id) => storage.edge_list_session(ctx, session_id).await?,
        None => storage.edge_list_all(ctx).await?,
    };
    for (src, dst, edge_type) in edge_rows {
        adjacency
            .entry(src)
            .or_default()
            .push((dst, edge_type.clone()));
        adjacency.entry(dst).or_default().push((src, edge_type));
    }

    // activation_map: entity_id -> (total_activation, min_hops)
    let mut activation_map: HashMap<Uuid, (f64, usize)> = HashMap::new();
    let mut expanded: HashSet<Uuid> = HashSet::new();
    // frontier: (node_id, current_activation, hop_depth)
    let mut frontier: Vec<(Uuid, f64, usize)> = seeds.iter().map(|id| (*id, 1.0, 0)).collect();

    for seed in seeds {
        activation_map.insert(*seed, (1.0, 0));
    }

    while let Some((node_id, current_activation, hop)) = frontier.pop() {
        if hop >= max_hops || current_activation < 0.01 {
            continue;
        }
        if expanded_count >= max_expansions {
            continue;
        }
        if !expanded.insert(node_id) {
            continue;
        }
        expanded_count += 1;

        let neighbors = adjacency.get(&node_id).cloned().unwrap_or_default();
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

        let results = spread(&store, &ctx, &[seed], None, 2, 0.7, 10)
            .await
            .unwrap();
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

        let results = spread(&store, &ctx, &[seed], None, 3, 0.5, 10)
            .await
            .unwrap();

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

        let results = spread(&store, &ctx, &[seed], None, 2, 0.7, 10)
            .await
            .unwrap();
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

        let results = spread(&store, &ctx, &[seed], None, 2, 0.7, 3)
            .await
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn spread_validates_inputs() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        // Empty seeds
        let err = spread(&store, &ctx, &[], None, 2, 0.7, 10).await;
        assert!(err.is_err());

        // max_hops out of range
        let seed = Uuid::new_v4();
        let err = spread(&store, &ctx, &[seed], None, 0, 0.7, 10).await;
        assert!(err.is_err());
        let err = spread(&store, &ctx, &[seed], None, 6, 0.7, 10).await;
        assert!(err.is_err());

        // decay out of range
        let err = spread(&store, &ctx, &[seed], None, 2, 0.0, 10).await;
        assert!(err.is_err());
        let err = spread(&store, &ctx, &[seed], None, 2, 1.5, 10).await;
        assert!(err.is_err());

        // limit out of range
        let err = spread(&store, &ctx, &[seed], None, 2, 0.7, 0).await;
        assert!(err.is_err());
        let err = spread(&store, &ctx, &[seed], None, 2, 0.7, 51).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn spread_no_neighbors_returns_empty() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let seed = Uuid::new_v4();

        let results = spread(&store, &ctx, &[seed], None, 2, 0.7, 10)
            .await
            .unwrap();
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

        let results = spread(&store, &ctx, &[seed], None, 3, 0.5, 10)
            .await
            .unwrap();
        // neighbor_b should have higher activation (direct + indirect via neighbor_a)
        assert!(results.len() >= 2);
        // Verify descending sort
        for pair in results.windows(2) {
            assert!(pair[0].activation >= pair[1].activation);
        }
    }

    #[tokio::test]
    async fn spread_preloads_tenant_edges_once_instead_of_n_plus_one() {
        use std::sync::atomic::Ordering;

        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();

        let seed = Uuid::new_v4();
        let middle_a = Uuid::new_v4();
        let middle_b = Uuid::new_v4();
        let leaf_a = Uuid::new_v4();
        let leaf_b = Uuid::new_v4();

        for (left, right) in [
            (seed, middle_a),
            (seed, middle_b),
            (middle_a, leaf_a),
            (middle_b, leaf_b),
        ] {
            store
                .edge_co_occurs(&ctx, left, right, session, 1.0)
                .await
                .unwrap();
        }

        let results = spread(&store, &ctx, &[seed], None, 3, 0.5, 10)
            .await
            .unwrap();

        assert!(results.iter().any(|node| node.entity_id == leaf_a));
        assert!(results.iter().any(|node| node.entity_id == leaf_b));
        assert_eq!(store.edge_list_all_calls.load(Ordering::Relaxed), 1);
        assert_eq!(store.edge_list_session_calls.load(Ordering::Relaxed), 0);
        assert_eq!(store.edge_list_for_entity_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn spread_dense_cyclic_graph_returns_promptly_with_single_preload() {
        use std::sync::atomic::Ordering;
        use tokio::time::{Duration, timeout};

        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();

        let seed = Uuid::new_v4();
        let mut nodes = vec![seed];
        nodes.extend((0..35).map(|_| Uuid::new_v4()));

        // Dense cyclic graph: every node connects to the next five nodes,
        // wrapping around to create many repeated paths back to already-seen
        // nodes. The spread must still use one tenant-edge preload and no
        // per-frontier edge_list_for_entity scans.
        for idx in 0..nodes.len() {
            for offset in 1..=5 {
                let right = nodes[(idx + offset) % nodes.len()];
                store
                    .edge_co_occurs(&ctx, nodes[idx], right, session, 1.0)
                    .await
                    .unwrap();
            }
        }

        let results = timeout(
            Duration::from_secs(2),
            spread(&store, &ctx, &[seed], None, 5, 0.9, 10),
        )
        .await
        .expect("dense cyclic spread should return promptly")
        .unwrap();

        assert_eq!(results.len(), 10);
        assert_eq!(store.edge_list_all_calls.load(Ordering::Relaxed), 1);
        assert_eq!(store.edge_list_session_calls.load(Ordering::Relaxed), 0);
        assert_eq!(store.edge_list_for_entity_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn spread_obeys_limit_and_expansion_cap_affects_output() {
        use std::sync::atomic::Ordering;

        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();

        let seed = Uuid::new_v4();
        let sink = Uuid::new_v4();
        for _ in 0..30 {
            let neighbor = Uuid::new_v4();
            store
                .edge_co_occurs(&ctx, seed, neighbor, session, 1.0)
                .await
                .unwrap();
            store
                .edge_co_occurs(&ctx, neighbor, sink, session, 1.0)
                .await
                .unwrap();
        }

        let results = spread(&store, &ctx, &[seed], None, 2, 0.9, 10)
            .await
            .unwrap();

        assert_eq!(results.len(), 10);
        let sink_node = results
            .iter()
            .find(|node| node.entity_id == sink)
            .expect("sink should be activated by expanded middle nodes");
        let capped_sink_activation = 19.0 * 0.9 * 0.9;
        assert!(
            (sink_node.activation - capped_sink_activation).abs() < 1e-9,
            "sink activation should reflect seed + 19 middle-node expansions under limit*2 cap; got {}",
            sink_node.activation
        );
        assert!(
            sink_node.activation < 30.0 * 0.9 * 0.9,
            "sink activation should be lower than uncapped traversal through all 30 middle nodes"
        );
        assert_eq!(store.edge_list_all_calls.load(Ordering::Relaxed), 1);
        assert_eq!(store.edge_list_session_calls.load(Ordering::Relaxed), 0);
        assert_eq!(store.edge_list_for_entity_calls.load(Ordering::Relaxed), 0);
    }
}
