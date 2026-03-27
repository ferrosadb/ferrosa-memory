//! Visualization types and event bus for the memory graph dashboard.
//!
//! Provides typed events emitted by tool handlers and a broadcast channel
//! that the WebSocket endpoint subscribes to. The browser receives a JSON
//! stream of `VizEvent` variants — first a full `Snapshot`, then incremental
//! deltas as the memory graph mutates.

use serde::Serialize;
use tokio::sync::broadcast;

/// A node in the visualization graph (entity or fold).
#[derive(Debug, Clone, Serialize)]
pub struct VizNode {
    pub id: String,
    pub label: String,
    /// "entity" or "fold"
    pub node_type: String,
    /// "concept", "person", "place", "event", "org", etc.
    pub entity_type: String,
    /// Memory lifecycle state: "active", "dormant", "silent"
    pub state: String,
    pub confidence: f64,
    pub created_at: String,
    /// Context snippet for the detail panel.
    pub context: String,
}

/// An edge in the visualization graph.
#[derive(Debug, Clone, Serialize)]
pub struct VizEdge {
    pub source: String,
    pub target: String,
    /// "CO_OCCURS", "MENTIONED_IN", "FOLDED_INTO", "SUPERSEDES"
    pub edge_type: String,
    /// Similarity strength (0.0–1.0) for CO_OCCURS edges.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strength: Option<f32>,
}

/// Typed event pushed to all connected WebSocket clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum VizEvent {
    /// Full graph snapshot (sent on WebSocket connect).
    Snapshot {
        nodes: Vec<VizNode>,
        edges: Vec<VizEdge>,
    },
    /// Entity created or updated.
    EntityChanged { node: VizNode, action: String },
    /// New edge created.
    EdgeCreated { edge: VizEdge },
    /// Entity state changed (promote/demote).
    StateChanged {
        entity_id: String,
        new_state: String,
    },
    /// Temporal fact updated.
    FactUpdated {
        entity_id: String,
        fact_text: String,
        superseded: Option<String>,
    },
    /// Fold completed (may link entities).
    FoldCompleted {
        fold_id: String,
        summary: String,
        entity_count: usize,
    },
    /// Anomaly detected on entity retrieval frequency (STRIDE T1).
    AnomalyDetected {
        entity_id: String,
        entity_name: String,
        retrieval_count: usize,
        session_mean: f64,
        session_stddev: f64,
        sigma_threshold: f64,
    },
}

/// Broadcast channel for visualization events.
///
/// Tool handlers call `emit()` after successful mutations.
/// The WebSocket handler calls `subscribe()` to receive the event stream.
/// If no subscribers are connected, emitted events are silently dropped.
pub struct EventBus {
    tx: broadcast::Sender<VizEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }

    /// Emit an event to all connected subscribers.
    ///
    /// Returns silently if no subscribers are connected.
    pub fn emit(&self, event: VizEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe to the event stream. Returns a receiver that yields
    /// each `VizEvent` emitted after this call.
    pub fn subscribe(&self) -> broadcast::Receiver<VizEvent> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert an `EntityEntry` into a `VizNode` for the visualization graph.
pub fn entity_to_viz_node(entry: &crate::types::EntityEntry) -> VizNode {
    VizNode {
        id: entry.entity_id.to_string(),
        label: entry.entity_name.clone(),
        node_type: "entity".into(),
        entity_type: entry.entity_type.clone(),
        state: entry.state.to_string(),
        confidence: entry.confidence,
        created_at: entry.created_at.to_rfc3339(),
        context: entry.context_snippet.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_emit_without_subscribers_does_not_panic() {
        let bus = EventBus::new();
        bus.emit(VizEvent::StateChanged {
            entity_id: "test".into(),
            new_state: "active".into(),
        });
        // No panic means success — events are silently dropped.
    }

    #[tokio::test]
    async fn event_bus_subscribe_receives_events() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.emit(VizEvent::StateChanged {
            entity_id: "e1".into(),
            new_state: "dormant".into(),
        });

        let event = rx.recv().await.expect("should receive event");
        let json = serde_json::to_string(&event).expect("should serialize");
        assert!(json.contains("StateChanged"));
        assert!(json.contains("dormant"));
    }

    #[tokio::test]
    async fn event_bus_multiple_subscribers_all_receive() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.emit(VizEvent::EntityChanged {
            node: VizNode {
                id: "n1".into(),
                label: "Test".into(),
                node_type: "entity".into(),
                entity_type: "concept".into(),
                state: "active".into(),
                confidence: 0.9,
                created_at: "2026-01-01T00:00:00Z".into(),
                context: "test context".into(),
            },
            action: "created".into(),
        });

        let e1 = rx1.recv().await.expect("rx1 should receive");
        let e2 = rx2.recv().await.expect("rx2 should receive");

        let j1 = serde_json::to_string(&e1).unwrap();
        let j2 = serde_json::to_string(&e2).unwrap();
        assert!(j1.contains("EntityChanged"));
        assert!(j2.contains("EntityChanged"));
    }

    #[test]
    fn viz_event_snapshot_serializes_with_tag() {
        let event = VizEvent::Snapshot {
            nodes: vec![VizNode {
                id: "n1".into(),
                label: "Node".into(),
                node_type: "entity".into(),
                entity_type: "concept".into(),
                state: "active".into(),
                confidence: 1.0,
                created_at: "2026-01-01T00:00:00Z".into(),
                context: "ctx".into(),
            }],
            edges: vec![VizEdge {
                source: "n1".into(),
                target: "n2".into(),
                edge_type: "CO_OCCURS".into(),
                strength: Some(0.85),
            }],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"Snapshot"#));
        assert!(json.contains(r#""edge_type":"CO_OCCURS"#));
    }

    #[test]
    fn viz_event_fold_completed_serializes() {
        let event = VizEvent::FoldCompleted {
            fold_id: "f1".into(),
            summary: "completed fold".into(),
            entity_count: 3,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"FoldCompleted"#));
        assert!(json.contains(r#""entity_count":3"#));
    }

    #[test]
    fn viz_event_fact_updated_serializes() {
        let event = VizEvent::FactUpdated {
            entity_id: "e1".into(),
            fact_text: "new fact".into(),
            superseded: Some("old-event-id".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"FactUpdated"#));
        assert!(json.contains("old-event-id"));
    }

    #[test]
    fn viz_event_anomaly_detected_serializes() {
        let event = VizEvent::AnomalyDetected {
            entity_id: "e1".into(),
            entity_name: "SuspiciousEntity".into(),
            retrieval_count: 25,
            session_mean: 5.0,
            session_stddev: 2.0,
            sigma_threshold: 3.0,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"AnomalyDetected"#));
        assert!(json.contains("SuspiciousEntity"));
        assert!(json.contains("25"));
        assert!(json.contains("5.0"));
    }

    #[tokio::test]
    async fn event_bus_anomaly_received_by_subscriber() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.emit(VizEvent::AnomalyDetected {
            entity_id: "e42".into(),
            entity_name: "Outlier".into(),
            retrieval_count: 50,
            session_mean: 10.0,
            session_stddev: 3.0,
            sigma_threshold: 3.0,
        });

        let event = rx.recv().await.expect("should receive anomaly event");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("AnomalyDetected"));
        assert!(json.contains("Outlier"));
    }

    #[test]
    fn entity_to_viz_node_converts_correctly() {
        let entry = crate::types::EntityEntry {
            tenant_id: uuid::Uuid::new_v4(),
            entity_id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            entity_name: "TestEntity".into(),
            entity_type: "concept".into(),
            source_fold_id: None,
            context_snippet: "some context".into(),
            entity_embedding: None,
            confidence: 0.85,
            state: crate::types::MemoryState::Dormant,
            created_at: chrono::Utc::now(),
        };

        let node = entity_to_viz_node(&entry);
        assert_eq!(node.label, "TestEntity");
        assert_eq!(node.node_type, "entity");
        assert_eq!(node.state, "dormant");
        assert!((node.confidence - 0.85).abs() < f64::EPSILON);
    }
}
