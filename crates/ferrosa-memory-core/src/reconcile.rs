//! Detect dangling graph edges — edges whose source or destination entity id
//! no longer exists in `entity_store`.
//!
//! These orphaned edges are the server-side root cause of the viz crash
//! ("Cannot create property 'vx' on string"): the snapshot streams an edge to
//! an id that has no node, and the id string leaks into the d3 node list. The
//! `viz` page now defends against them (see `assets/graph-sanitize.mjs`), but
//! they should also be removed at the source. This module holds the pure
//! detection logic; the `reconcile_dangling_edges` binary is the thin IO layer.

use std::collections::HashSet;
use uuid::Uuid;

/// A graph edge identified by the entity ids it connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeEndpoints {
    pub src: Uuid,
    pub dst: Uuid,
}

/// True when an edge references an entity id absent from `existing` — i.e. the
/// edge is orphaned and would crash the viz. Shared by the detector and the
/// `reconcile-dangling-edges` binary so the rule lives in exactly one place.
pub fn edge_is_dangling(src: Uuid, dst: Uuid, existing: &HashSet<Uuid>) -> bool {
    !existing.contains(&src) || !existing.contains(&dst)
}

/// Return the subset of `edges` that reference at least one entity id absent
/// from `existing`. Order-preserving so callers can report deterministically.
pub fn find_dangling_edges(
    edges: &[EdgeEndpoints],
    existing: &HashSet<Uuid>,
) -> Vec<EdgeEndpoints> {
    edges
        .iter()
        .filter(|e| edge_is_dangling(e.src, e.dst, existing))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn edge_is_dangling_when_either_endpoint_missing() {
        let existing: HashSet<Uuid> = [u(1), u(2)].into_iter().collect();
        assert!(!edge_is_dangling(u(1), u(2), &existing));
        assert!(edge_is_dangling(u(1), u(9), &existing));
        assert!(edge_is_dangling(u(9), u(2), &existing));
        assert!(edge_is_dangling(u(8), u(9), &existing));
    }

    #[test]
    fn keeps_fully_connected_edges() {
        let existing: HashSet<Uuid> = [u(1), u(2)].into_iter().collect();
        let edges = vec![EdgeEndpoints {
            src: u(1),
            dst: u(2),
        }];
        assert!(find_dangling_edges(&edges, &existing).is_empty());
    }

    #[test]
    fn flags_edge_with_missing_source_or_target() {
        let existing: HashSet<Uuid> = [u(1), u(2)].into_iter().collect();
        let edges = vec![
            EdgeEndpoints {
                src: u(1),
                dst: u(2),
            }, // both present
            EdgeEndpoints {
                src: u(1),
                dst: u(9),
            }, // dst missing
            EdgeEndpoints {
                src: u(8),
                dst: u(2),
            }, // src missing
            EdgeEndpoints {
                src: u(7),
                dst: u(6),
            }, // both missing
        ];
        let dangling = find_dangling_edges(&edges, &existing);
        assert_eq!(
            dangling,
            vec![
                EdgeEndpoints {
                    src: u(1),
                    dst: u(9)
                },
                EdgeEndpoints {
                    src: u(8),
                    dst: u(2)
                },
                EdgeEndpoints {
                    src: u(7),
                    dst: u(6)
                },
            ]
        );
    }
}
