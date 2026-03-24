//! Memory chains -- path discovery between concepts.
//!
//! BFS traversal to find shortest path between two entities via graph edges.
//! Used to explain how two concepts are connected through the knowledge graph.

use std::collections::{HashMap, VecDeque};

use serde::Serialize;
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

/// A single step in a memory chain path.
#[derive(Debug, Clone, Serialize)]
pub struct ChainStep {
    pub entity_id: Uuid,
    pub edge_type: String,
}

/// A discovered path between two entities through the knowledge graph.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryChain {
    pub source: Uuid,
    pub destination: Uuid,
    pub steps: Vec<ChainStep>,
    pub hop_count: usize,
    /// Confidence decays with path length: 1.0 / (1.0 + hop_count).
    pub confidence: f64,
}

/// Find shortest path between two entities via BFS.
///
/// Returns `None` if no path exists within `max_hops`. Returns a zero-step
/// chain if source and destination are the same entity.
pub async fn find_chain(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    source: Uuid,
    destination: Uuid,
    max_hops: usize,
) -> anyhow::Result<Option<MemoryChain>> {
    anyhow::ensure!((1..=10).contains(&max_hops), "max_hops must be 1..=10");

    if source == destination {
        return Ok(Some(MemoryChain {
            source,
            destination,
            steps: vec![],
            hop_count: 0,
            confidence: 1.0,
        }));
    }

    // visited: entity_id -> (parent_id, edge_type used to reach it)
    let mut visited: HashMap<Uuid, (Uuid, String)> = HashMap::new();
    let mut queue: VecDeque<(Uuid, usize)> = VecDeque::new();

    visited.insert(source, (source, String::new()));
    queue.push_back((source, 0));

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= max_hops {
            continue;
        }

        let neighbors = storage.edge_list_for_entity(ctx, current).await?;
        for (neighbor_id, edge_type) in neighbors {
            if visited.contains_key(&neighbor_id) {
                continue;
            }
            visited.insert(neighbor_id, (current, edge_type.clone()));

            if neighbor_id == destination {
                let steps = reconstruct_path(&visited, source, destination);
                let hop_count = steps.len();
                let confidence = 1.0 / (1.0 + hop_count as f64);
                return Ok(Some(MemoryChain {
                    source,
                    destination,
                    steps,
                    hop_count,
                    confidence,
                }));
            }

            queue.push_back((neighbor_id, depth + 1));
        }
    }

    Ok(None)
}

/// Walk the visited map backwards from destination to source to reconstruct
/// the path as a sequence of ChainSteps.
fn reconstruct_path(
    visited: &HashMap<Uuid, (Uuid, String)>,
    source: Uuid,
    destination: Uuid,
) -> Vec<ChainStep> {
    let mut steps = Vec::new();
    let mut node = destination;

    while node != source {
        let (parent, edge_type) = visited
            .get(&node)
            .expect("visited map must contain every node on the path")
            .clone();
        steps.push(ChainStep {
            entity_id: node,
            edge_type,
        });
        node = parent;
    }

    steps.reverse();
    steps
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
    async fn same_source_and_destination() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let id = Uuid::new_v4();

        let chain = find_chain(&store, &ctx, id, id, 5).await.unwrap();
        let chain = chain.expect("should find trivial self-path");
        assert_eq!(chain.hop_count, 0);
        assert!(chain.steps.is_empty());
        assert!((chain.confidence - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn direct_connection_one_hop() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        store.edge_co_occurs(&ctx, a, b, session).await.unwrap();

        let chain = find_chain(&store, &ctx, a, b, 5).await.unwrap();
        let chain = chain.expect("should find direct path");
        assert_eq!(chain.hop_count, 1);
        assert_eq!(chain.steps.len(), 1);
        assert_eq!(chain.steps[0].entity_id, b);
        assert_eq!(chain.steps[0].edge_type, "CO_OCCURS");
        assert!((chain.confidence - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn multi_hop_path() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        // a -> b -> c (no direct a -> c edge)
        store.edge_co_occurs(&ctx, a, b, session).await.unwrap();
        store.edge_co_occurs(&ctx, b, c, session).await.unwrap();

        let chain = find_chain(&store, &ctx, a, c, 5).await.unwrap();
        let chain = chain.expect("should find 2-hop path");
        assert_eq!(chain.hop_count, 2);
        assert_eq!(chain.steps.len(), 2);
        assert_eq!(chain.steps[0].entity_id, b);
        assert_eq!(chain.steps[1].entity_id, c);
        // confidence = 1/(1+2) = 0.333...
        assert!((chain.confidence - 1.0 / 3.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn no_path_found() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        // No edges at all
        let chain = find_chain(&store, &ctx, a, b, 5).await.unwrap();
        assert!(chain.is_none(), "should return None when no path exists");
    }

    #[tokio::test]
    async fn path_beyond_max_hops_not_found() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        // a -> b -> c requires 2 hops, but max_hops = 1
        store.edge_co_occurs(&ctx, a, b, session).await.unwrap();
        store.edge_co_occurs(&ctx, b, c, session).await.unwrap();

        let chain = find_chain(&store, &ctx, a, c, 1).await.unwrap();
        assert!(chain.is_none(), "should not find path beyond max_hops");
    }

    #[tokio::test]
    async fn max_hops_validation() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let err = find_chain(&store, &ctx, a, b, 0).await;
        assert!(err.is_err(), "max_hops=0 should be rejected");

        let err = find_chain(&store, &ctx, a, b, 11).await;
        assert!(err.is_err(), "max_hops=11 should be rejected");
    }
}
