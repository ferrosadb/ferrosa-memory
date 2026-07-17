//! Visualization types and event bus for the memory graph dashboard.
//!
//! Provides typed events emitted by tool handlers and a broadcast channel
//! that the WebSocket endpoint subscribes to. The browser receives a JSON
//! stream of `VizEvent` variants — first a full `Snapshot`, then incremental
//! deltas as the memory graph mutates.

use serde::{Deserialize, Serialize};
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
    /// Number of children when this is an aggregate (crate/module) node.
    /// Absent for leaf nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_count: Option<usize>,
    /// The session this node belongs to (or the tenant-global sentinel for
    /// global-scope entities). Lets the UI render a per-node badge and
    /// filter client-side.
    pub session_id: String,
    /// True when the node lives in the tenant-global partition. Convenience
    /// for UI rendering — derivable from `session_id`, but spared the
    /// round-trip check.
    pub is_global: bool,
}

impl Default for VizNode {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            node_type: String::new(),
            entity_type: String::new(),
            state: String::new(),
            confidence: 0.0,
            created_at: String::new(),
            context: String::new(),
            child_count: None,
            session_id: String::new(),
            is_global: false,
        }
    }
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

/// Hierarchical drill-down level for the visualization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VizLevel {
    #[default]
    Crate,
    Module,
    Function,
}

/// Message sent from the browser client to the server over WebSocket.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VizClientMessage {
    /// Drill into a child level.
    DrillDown {
        level: VizLevel,
        #[serde(default)]
        parent: Option<String>,
    },
    /// Return to the parent level.
    DrillUp,
    /// Toggle between overview (clustered) and detail (flat) view.
    ToggleView { mode: String },
    /// Explore the neighborhood of an entity via BFS.
    ExploreNeighborhood {
        entity_id: String,
        #[serde(default = "default_hops")]
        hops: usize,
    },
}

fn default_hops() -> usize {
    2
}

/// Typed event pushed to all connected WebSocket clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum VizEvent {
    /// Full graph snapshot (sent on WebSocket connect).
    Snapshot {
        nodes: Vec<VizNode>,
        edges: Vec<VizEdge>,
        /// Current drill-down level.
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<String>,
        /// Parent context for drill-down (e.g. crate name).
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        /// Total entities in the full graph (not just this level).
        #[serde(skip_serializing_if = "Option::is_none")]
        total_nodes: Option<usize>,
        /// Total edges in the full graph (not just this level).
        #[serde(skip_serializing_if = "Option::is_none")]
        total_edges: Option<usize>,
    },
    /// Begin an incremental snapshot stream. The browser should clear local
    /// graph state and append following chunks until `SnapshotStreamEnd`.
    SnapshotStreamStart {
        level: Option<String>,
        parent: Option<String>,
    },
    /// Chunk of an initial snapshot streamed from paged storage reads.
    SnapshotStreamChunk {
        nodes: Vec<VizNode>,
        edges: Vec<VizEdge>,
    },
    /// End of an incremental snapshot stream.
    SnapshotStreamEnd {
        total_nodes: usize,
        total_edges: usize,
        /// Present when the snapshot was truncated because a server-side
        /// paged cursor stopped progressing (see [`CursorProgressGuard`]).
        /// The UI renders this as a loud "snapshot truncated" status instead
        /// of silently showing a partial graph.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
    /// Durable task state changed and task-list resources should be refreshed.
    SessionTaskChanged {
        session_id: String,
        task_id: Option<String>,
        action: String,
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

// ─── Snapshot cursor progress guard ─────────────────────────────────────────

/// Tunables for [`CursorProgressGuard`].
#[derive(Debug, Clone, Copy)]
pub struct CursorGuardConfig {
    /// Trip when byte-identical page content has been served this many times.
    pub max_page_repeats: u32,
    /// Trip when `rows_delivered > duplication_factor * unique_rows` …
    pub duplication_factor: usize,
    /// … but only once at least this many rows were delivered, so small
    /// healthy streams with a few boundary duplicates can never trip.
    pub duplication_floor_rows: usize,
    /// Absolute backstop on pages consumed from a single cursor. No cheap
    /// server-side row estimate is available to the viz snapshot builder, so
    /// this is a generous fixed bound rather than a COUNT-derived one.
    pub max_pages: usize,
    /// Above this many tracked unique keys, dedup and duplication-ratio
    /// detection degrade (loudly logged); repeated-page and page-bound
    /// detection stay active. Bounds guard memory to ~8 bytes x cap.
    pub seen_key_cap: usize,
}

impl Default for CursorGuardConfig {
    fn default() -> Self {
        Self {
            max_page_repeats: 3,
            duplication_factor: 3,
            duplication_floor_rows: 5_000,
            max_pages: 100_000,
            seen_key_cap: 1_000_000,
        }
    }
}

/// Why a [`CursorProgressGuard`] stopped a paged snapshot stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStall {
    /// The exact same page content was served repeatedly.
    RepeatedPage,
    /// Rows delivered greatly exceed unique rows (cursor re-serves a window).
    DuplicationRatio,
    /// Absolute page-count backstop exceeded.
    PageBound,
}

impl std::fmt::Display for CursorStall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CursorStall::RepeatedPage => write!(f, "identical page served repeatedly"),
            CursorStall::DuplicationRatio => {
                write!(f, "delivered rows greatly exceed unique rows")
            }
            CursorStall::PageBound => write!(f, "page-count backstop exceeded"),
        }
    }
}

/// Counters exposed by [`CursorProgressGuard`] for logs and error frames.
#[derive(Debug, Clone, Copy)]
pub struct CursorGuardStats {
    pub pages: usize,
    pub delivered: usize,
    pub unique: usize,
}

/// Detects a paged storage cursor that has stopped making progress — e.g. a
/// server whose `paging_state` cycles and re-serves the same row window
/// forever (ferrosa bug tracked separately as t_a0f922a3). The viz snapshot
/// builder feeds every page through the guard; when the guard trips, the
/// builder stops paging, flushes the deduplicated rows collected so far, and
/// ends the stream with an explicit error frame. This is a designed, loudly
/// logged fallback — never a silent partial result.
///
/// Memory bound: the seen-key set holds at most `seen_key_cap` u64 hashes
/// (~8 MB at the 1M default; ~50k at current graph scales) and the page
/// fingerprint map at most `max_pages` entries.
pub struct CursorProgressGuard {
    config: CursorGuardConfig,
    seen: std::collections::HashSet<u64>,
    fingerprints: std::collections::HashMap<u64, u32>,
    pages: usize,
    delivered: usize,
    page_hasher: std::hash::DefaultHasher,
    page_rows: usize,
    dedup_degraded: bool,
}

impl CursorProgressGuard {
    pub fn new() -> Self {
        Self::with_config(CursorGuardConfig::default())
    }

    pub fn with_config(config: CursorGuardConfig) -> Self {
        Self {
            config,
            seen: std::collections::HashSet::new(),
            fingerprints: std::collections::HashMap::new(),
            pages: 0,
            delivered: 0,
            page_hasher: std::hash::DefaultHasher::new(),
            page_rows: 0,
            dedup_degraded: false,
        }
    }

    /// Begin observing one page (one batch from the paged cursor).
    pub fn begin_page(&mut self) {
        self.pages += 1;
        self.page_hasher = std::hash::DefaultHasher::new();
        self.page_rows = 0;
    }

    /// Record one row of the current page. Returns `true` when the row's key
    /// has not been seen before on this stream (callers emit only those).
    pub fn note_row(&mut self, key: u64) -> bool {
        use std::hash::Hasher;
        self.delivered += 1;
        self.page_rows += 1;
        self.page_hasher.write_u64(key);
        if self.dedup_degraded {
            return true;
        }
        if self.seen.len() >= self.config.seen_key_cap && !self.seen.contains(&key) {
            // Designed, observable degradation: beyond the cap we stop
            // tracking keys (bounded memory), which disables dedup and the
            // duplication-ratio trigger. Repeated-page and page-bound
            // detection still terminate a non-progressing cursor.
            self.dedup_degraded = true;
            tracing::warn!(
                unique_rows = self.seen.len(),
                seen_key_cap = self.config.seen_key_cap,
                "viz cursor guard: seen-key cap reached; dedup and \
                 duplication-ratio detection degraded (repeated-page and \
                 page-bound detection remain active)"
            );
            return true;
        }
        self.seen.insert(key)
    }

    /// Finish the current page; returns a stall reason if the guard tripped.
    pub fn end_page(&mut self) -> Option<CursorStall> {
        use std::hash::Hasher;
        if self.page_rows > 0 {
            self.page_hasher.write_usize(self.page_rows);
            let fingerprint = self.page_hasher.finish();
            let count = self.fingerprints.entry(fingerprint).or_insert(0);
            *count += 1;
            if *count >= self.config.max_page_repeats {
                return Some(CursorStall::RepeatedPage);
            }
        }
        if !self.dedup_degraded
            && self.delivered >= self.config.duplication_floor_rows
            && self.delivered
                > self
                    .config
                    .duplication_factor
                    .saturating_mul(self.seen.len())
        {
            return Some(CursorStall::DuplicationRatio);
        }
        if self.pages >= self.config.max_pages {
            return Some(CursorStall::PageBound);
        }
        None
    }

    pub fn stats(&self) -> CursorGuardStats {
        CursorGuardStats {
            pages: self.pages,
            delivered: self.delivered,
            unique: self.seen.len(),
        }
    }
}

impl Default for CursorProgressGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic 64-bit key for a row, used by [`CursorProgressGuard`] for
/// dedup and progress accounting. SipHash via `DefaultHasher` — collisions
/// are astronomically unlikely at viz scales (~50k rows) and at worst
/// suppress one row that merely looks like a duplicate.
pub fn row_key<T: std::hash::Hash>(row: &T) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::hash::DefaultHasher::new();
    row.hash(&mut hasher);
    hasher.finish()
}

/// Convert an `EntityEntry` into a `VizNode` for the visualization graph.
pub fn entity_to_viz_node(entry: &crate::types::EntityEntry) -> VizNode {
    let is_global = matches!(entry.scope, crate::types::EntityScope::Global)
        || entry.session_id == crate::scope::tenant_global_session_uuid(entry.tenant_id);
    VizNode {
        id: entry.entity_id.to_string(),
        label: entry.entity_name.clone(),
        node_type: "entity".into(),
        entity_type: entry.entity_type.clone(),
        state: entry.state.to_string(),
        confidence: entry.confidence,
        created_at: entry.created_at.to_rfc3339(),
        context: entry.context_snippet.clone(),
        child_count: None,
        session_id: entry.session_id.to_string(),
        is_global,
    }
}

/// Convert a `FoldEntry` into a `VizNode` for the visualization graph.
pub fn fold_to_viz_node(entry: &crate::types::FoldEntry) -> VizNode {
    let label = entry
        .fold_summary
        .as_deref()
        .unwrap_or("(unfold)")
        .chars()
        .take(60)
        .collect::<String>();
    VizNode {
        id: entry.fold_id.to_string(),
        label,
        node_type: "fold".into(),
        entity_type: format!("fold-d{}", entry.depth),
        state: format!("{:?}", entry.status).to_lowercase(),
        confidence: entry.compression_ratio.unwrap_or(0.0),
        created_at: entry.created_at.to_rfc3339(),
        context: String::new(),
        child_count: None,
        session_id: entry.session_id.to_string(),
        is_global: false,
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
                child_count: None,
                ..Default::default()
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
                child_count: None,
                ..Default::default()
            }],
            edges: vec![VizEdge {
                source: "n1".into(),
                target: "n2".into(),
                edge_type: "CO_OCCURS".into(),
                strength: Some(0.85),
            }],
            level: None,
            parent: None,
            total_nodes: None,
            total_edges: None,
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

    fn test_guard() -> CursorProgressGuard {
        CursorProgressGuard::with_config(CursorGuardConfig {
            max_page_repeats: 3,
            duplication_factor: 3,
            duplication_floor_rows: 100,
            max_pages: 1_000,
            seen_key_cap: 10,
        })
    }

    /// Serve one page of `keys` through the guard; returns the stall verdict.
    fn serve_page(
        guard: &mut CursorProgressGuard,
        keys: impl Iterator<Item = u64>,
    ) -> Option<CursorStall> {
        guard.begin_page();
        for k in keys {
            guard.note_row(k);
        }
        guard.end_page()
    }

    #[test]
    fn guard_trips_on_third_serving_of_identical_page() {
        let mut guard = CursorProgressGuard::new();
        let mut tripped_at = None;
        for page in 0..5 {
            if let Some(stall) = serve_page(&mut guard, 0..10u64) {
                tripped_at = Some((page, stall));
                break;
            }
        }
        assert_eq!(
            tripped_at,
            Some((2, CursorStall::RepeatedPage)),
            "the third serving of an identical page must trip the guard"
        );
    }

    #[test]
    fn guard_trips_on_duplication_ratio_when_page_boundaries_drift() {
        // A cycling ~30-row window served in 7-row pages: page boundaries
        // drift across the cycle, so fingerprints repeat only every 30 pages.
        // The unique-vs-delivered ratio must catch it much earlier.
        let mut guard = CursorProgressGuard::with_config(CursorGuardConfig {
            duplication_floor_rows: 100,
            ..CursorGuardConfig::default()
        });
        let mut i = 0u64;
        let mut trip = None;
        for _ in 0..40 {
            guard.begin_page();
            for _ in 0..7 {
                guard.note_row(i % 30);
                i += 1;
            }
            if let Some(stall) = guard.end_page() {
                trip = Some(stall);
                break;
            }
        }
        assert_eq!(trip, Some(CursorStall::DuplicationRatio));
        let stats = guard.stats();
        assert!(
            stats.delivered <= 200,
            "ratio trigger must fire promptly, not after megabytes; delivered={}",
            stats.delivered
        );
        assert_eq!(stats.unique, 30);
    }

    #[test]
    fn guard_never_trips_on_progressing_stream_with_boundary_duplicates() {
        // Healthy but slow: every page re-serves the previous page's last row
        // (duplicate ROWS across page boundaries) while still advancing.
        let mut guard = CursorProgressGuard::with_config(CursorGuardConfig {
            duplication_floor_rows: 100,
            ..CursorGuardConfig::default()
        });
        let mut next = 0u64;
        for page in 0..50 {
            guard.begin_page();
            if page > 0 {
                assert!(
                    !guard.note_row(next - 1),
                    "boundary duplicate must be deduplicated, not re-emitted"
                );
            }
            for _ in 0..10 {
                assert!(guard.note_row(next), "fresh rows must be emitted");
                next += 1;
            }
            assert_eq!(
                guard.end_page(),
                None,
                "a progressing stream must never trip the guard (page {page})"
            );
        }
        assert_eq!(guard.stats().unique, 500);
        assert_eq!(guard.stats().delivered, 549);
    }

    #[test]
    fn guard_trips_on_page_bound_backstop() {
        let mut guard = CursorProgressGuard::with_config(CursorGuardConfig {
            max_pages: 5,
            ..CursorGuardConfig::default()
        });
        let mut trip = None;
        for page in 0..10u64 {
            // All-unique rows: neither fingerprint nor ratio can trip.
            if let Some(stall) = serve_page(&mut guard, page * 10..page * 10 + 10) {
                trip = Some((page, stall));
                break;
            }
        }
        assert_eq!(trip, Some((4, CursorStall::PageBound)));
    }

    #[test]
    fn guard_dedup_degrades_loudly_at_seen_key_cap_and_disables_ratio() {
        let mut guard = test_guard(); // seen_key_cap: 10
        guard.begin_page();
        for k in 0..10u64 {
            assert!(guard.note_row(k));
        }
        // Cap reached: the 11th unique key flips dedup into degraded mode.
        assert!(guard.note_row(10));
        // Degraded mode: duplicates are no longer suppressed…
        assert!(guard.note_row(0), "degraded dedup must emit everything");
        assert_eq!(guard.end_page(), None);
        // …and the duplication-ratio trigger is disabled (unique is frozen,
        // so the ratio would otherwise false-positive on a healthy stream).
        for page in 0..30u64 {
            let base = 100 + page * 10;
            let verdict = serve_page(&mut guard, base..base + 10);
            assert_ne!(
                verdict,
                Some(CursorStall::DuplicationRatio),
                "ratio detection must be disabled once dedup degraded"
            );
        }
    }

    #[test]
    fn row_key_is_deterministic_and_distinguishes_rows() {
        let a = row_key(&("s1", 1u64));
        let b = row_key(&("s1", 1u64));
        let c = row_key(&("s1", 2u64));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn snapshot_stream_end_serializes_optional_error() {
        let healthy = VizEvent::SnapshotStreamEnd {
            total_nodes: 10,
            total_edges: 20,
            error: None,
        };
        let json = serde_json::to_string(&healthy).unwrap();
        assert!(
            !json.contains("error"),
            "healthy end must omit error: {json}"
        );

        let truncated = VizEvent::SnapshotStreamEnd {
            total_nodes: 10,
            total_edges: 20,
            error: Some("snapshot truncated: server cursor not progressing".into()),
        };
        let json = serde_json::to_string(&truncated).unwrap();
        assert!(json.contains(r#""error":"snapshot truncated"#), "{json}");
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
            ..Default::default()
        };

        let node = entity_to_viz_node(&entry);
        assert_eq!(node.label, "TestEntity");
        assert_eq!(node.node_type, "entity");
        assert_eq!(node.state, "dormant");
        assert!((node.confidence - 0.85).abs() < f64::EPSILON);
    }
}
