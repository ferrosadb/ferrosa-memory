//! Storage trait — abstract CQL operations for testability.
//!
//! All tool modules depend on this trait, not a concrete CQL client. This is
//! the critical abstraction identified by the DSM analysis: `cql_client` has
//! 86% propagation cost, so a stable trait interface prevents cascading changes.
//!
//! ## Design
//!
//! The trait defines typed operations at the domain level (memo, plan) rather
//! than raw CQL. This keeps the CQL dialect inside the concrete implementation
//! and lets us swap backends (mock, embedded, real Ferrosa) without changing
//! tool handler code.

use uuid::Uuid;

use crate::types::{
    AuditEntry, EntityEntry, FeedbackOutcome, FoldEntry, FoldSummary, MemoEntry, MemoryState,
    PlanNode, PlanStatus, TemporalEvent, TenantContext,
};

/// Core storage operations for the memory system.
///
/// All methods are async and take `&self` (shared reference) because the
/// underlying CQL client manages its own connection pool.
///
/// Every method that accesses tenant data requires a [`TenantContext`] to
/// enforce tenant isolation at the trait boundary.
#[allow(async_fn_in_trait)]
pub trait Storage: Send + Sync {
    /// Check memo cache by content hash.
    async fn memo_get(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<Option<MemoEntry>>;

    /// Increment hit count and update last_hit_at on cache hit.
    async fn memo_touch(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<()>;

    /// Store a new memo cache entry.
    async fn memo_put(&self, ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()>;

    /// Write a plan node.
    async fn plan_put(&self, ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()>;

    /// Get all plan nodes for a session up to max_depth.
    async fn plan_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        max_depth: Option<i32>,
    ) -> anyhow::Result<Vec<PlanNode>>;

    /// Update a plan node's status and optional outcome summary.
    async fn plan_update_status(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        depth: i32,
        subtask_id: &str,
        status: PlanStatus,
        outcome_summary: Option<&str>,
    ) -> anyhow::Result<()>;

    // --- Fold operations (Sprint 2) ---

    /// Create a new active fold.
    async fn fold_put(&self, ctx: &TenantContext, entry: &FoldEntry) -> anyhow::Result<()>;

    /// Get a fold by ID.
    async fn fold_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
    ) -> anyhow::Result<Option<FoldEntry>>;

    /// Append text to a fold's raw_trajectory.
    async fn fold_append(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
        text: &str,
    ) -> anyhow::Result<()>;

    /// Update fold status, summary, embedding, and compression info.
    async fn fold_complete(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
        summary: &str,
        embedding: Vec<f32>,
        compression_ratio: f64,
    ) -> anyhow::Result<()>;

    /// Retrieve fold summaries by embedding similarity (ANN search).
    async fn fold_search(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
        include_raw: bool,
    ) -> anyhow::Result<Vec<FoldSummary>>;

    // --- Entity operations (Sprint 3) ---

    /// Store a new entity.
    async fn entity_put(&self, ctx: &TenantContext, entry: &EntityEntry) -> anyhow::Result<()>;

    /// Find entity by phonetic match on name.
    async fn entity_find_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
    ) -> anyhow::Result<Option<EntityEntry>>;

    /// Search entities by embedding similarity.
    async fn entity_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<EntityEntry>>;

    /// Count entities in a session (for rate limiting).
    async fn entity_count(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<usize>;

    /// Count folds in a session.
    async fn fold_count(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<usize>;

    /// Count memo cache entries for the tenant.
    async fn memo_count(&self, ctx: &TenantContext) -> anyhow::Result<usize>;

    /// List all entities for a session (for consolidation).
    async fn entity_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<EntityEntry>>;

    /// List all entities for a tenant (for viz snapshot).
    async fn entity_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<EntityEntry>>;

    /// Update an entity's memory state (promote/demote lifecycle).
    async fn entity_update_state(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        state: MemoryState,
    ) -> anyhow::Result<()>;

    // --- Temporal event operations (Sprint 3) ---

    /// Store a temporal event.
    async fn temporal_put(&self, ctx: &TenantContext, event: &TemporalEvent) -> anyhow::Result<()>;

    /// Get the current (valid_until IS NULL) fact for an entity.
    async fn temporal_get_current(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<TemporalEvent>>;

    /// Invalidate a temporal event (set valid_until).
    async fn temporal_invalidate(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        event_id: Uuid,
    ) -> anyhow::Result<()>;

    // --- Feedback operations (Sprint 3) ---

    /// Record a feedback outcome.
    async fn feedback_put(
        &self,
        ctx: &TenantContext,
        outcome: &FeedbackOutcome,
    ) -> anyhow::Result<()>;

    /// List all feedback outcomes across tenants (batch job use only).
    ///
    /// Returns all rows from `feedback_outcomes`. In production this would
    /// use token-range scanning; the current implementation issues a single
    /// full-table query suitable for moderate data volumes.
    async fn feedback_list_all(&self) -> anyhow::Result<Vec<FeedbackOutcome>>;

    // --- Session lifecycle ---

    /// Delete all data for a session (right-to-deletion).
    async fn delete_session(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<usize>;

    // --- Graph edge operations ---

    /// Create a FOLDED_INTO edge (child fold -> parent fold).
    async fn edge_folded_into(
        &self,
        ctx: &TenantContext,
        source_fold_id: Uuid,
        target_fold_id: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<()>;

    /// Create a MENTIONED_IN edge (entity -> fold).
    async fn edge_mentioned_in(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        fold_id: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<()>;

    /// Create a CO_OCCURS_WITH edge (entity <-> entity).
    async fn edge_co_occurs(
        &self,
        ctx: &TenantContext,
        entity_a: Uuid,
        entity_b: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<()>;

    /// Create a SUPERSEDES edge (new fact -> old fact).
    async fn edge_supersedes(
        &self,
        ctx: &TenantContext,
        new_event_id: Uuid,
        old_event_id: Uuid,
        entity_id: Uuid,
    ) -> anyhow::Result<()>;

    /// List all edges for a session as (source, target, edge_type) triples.
    ///
    /// Queries all four edge tables (folded_into, mentioned_in, co_occurs_with,
    /// supersedes) and returns a unified list for visualization snapshots.
    async fn edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<(Uuid, Uuid, String)>>;

    /// List all edges for a tenant (for viz snapshot when session is unknown).
    async fn edge_list_all(&self, ctx: &TenantContext)
    -> anyhow::Result<Vec<(Uuid, Uuid, String)>>;

    /// List all neighbors of an entity as (neighbor_id, edge_type) pairs.
    ///
    /// Searches mentioned_in, co_occurs_with, and supersedes edges where the
    /// given entity_id appears as source or target. Used for spreading activation.
    async fn edge_list_for_entity(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Vec<(Uuid, String)>>;

    // --- Intention operations ---

    /// Store a new intention.
    async fn intention_put(
        &self,
        ctx: &TenantContext,
        intention: &crate::intention::Intention,
    ) -> anyhow::Result<()>;

    /// List all intentions for a tenant.
    async fn intention_list(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<crate::intention::Intention>>;

    /// Update an intention's status and optional timestamps.
    async fn intention_update_status(
        &self,
        ctx: &TenantContext,
        id: Uuid,
        status: &str,
        triggered_at: Option<chrono::DateTime<chrono::Utc>>,
        completed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<()>;

    // --- Audit log operations ---

    /// Persist an audit log entry (append-only, STRIDE R1).
    async fn audit_put(&self, ctx: &TenantContext, entry: &AuditEntry) -> anyhow::Result<()>;
}

/// In-memory mock storage for unit tests.
///
/// Stores data in `Vec`s behind a `tokio::sync::Mutex`. Not for production use.
#[cfg(any(test, feature = "mock-storage"))]
pub mod mock {
    use super::*;
    use crate::types::FoldStatus;
    use tokio::sync::Mutex;

    /// An edge stored in mock storage: (source, target, edge_type, session_id).
    #[derive(Clone)]
    pub struct MockEdge {
        pub source: Uuid,
        pub target: Uuid,
        pub edge_type: String,
        pub session_id: Uuid,
    }

    #[derive(Default)]
    pub struct MockStorage {
        pub memos: Mutex<Vec<MemoEntry>>,
        pub plans: Mutex<Vec<PlanNode>>,
        pub folds: Mutex<Vec<FoldEntry>>,
        pub entities: Mutex<Vec<EntityEntry>>,
        pub temporal_events: Mutex<Vec<TemporalEvent>>,
        pub feedback: Mutex<Vec<FeedbackOutcome>>,
        pub intentions: Mutex<Vec<crate::intention::Intention>>,
        pub edges: Mutex<Vec<MockEdge>>,
        pub audit_entries: Mutex<Vec<AuditEntry>>,
    }

    impl MockStorage {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl Storage for MockStorage {
        async fn memo_get(
            &self,
            ctx: &TenantContext,
            content_hash: &str,
            model_version: &str,
        ) -> anyhow::Result<Option<MemoEntry>> {
            let memos = self.memos.lock().await;
            let _ = ctx; // tenant scoping would filter here in real impl
            Ok(memos
                .iter()
                .find(|m| m.content_hash == content_hash && m.model_version == model_version)
                .cloned())
        }

        async fn memo_touch(
            &self,
            _ctx: &TenantContext,
            content_hash: &str,
            model_version: &str,
        ) -> anyhow::Result<()> {
            let mut memos = self.memos.lock().await;
            if let Some(m) = memos
                .iter_mut()
                .find(|m| m.content_hash == content_hash && m.model_version == model_version)
            {
                m.hit_count += 1;
                m.last_hit_at = Some(chrono::Utc::now());
            }
            Ok(())
        }

        async fn memo_put(&self, _ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()> {
            let mut memos = self.memos.lock().await;
            memos.push(entry.clone());
            Ok(())
        }

        async fn plan_put(&self, _ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()> {
            let mut plans = self.plans.lock().await;
            plans.push(node.clone());
            Ok(())
        }

        async fn plan_get(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            max_depth: Option<i32>,
        ) -> anyhow::Result<Vec<PlanNode>> {
            let plans = self.plans.lock().await;
            Ok(plans
                .iter()
                .filter(|p| p.session_id == session_id && max_depth.is_none_or(|d| p.depth <= d))
                .cloned()
                .collect())
        }

        async fn plan_update_status(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            depth: i32,
            subtask_id: &str,
            status: PlanStatus,
            outcome_summary: Option<&str>,
        ) -> anyhow::Result<()> {
            let mut plans = self.plans.lock().await;
            if let Some(p) = plans.iter_mut().find(|p| {
                p.session_id == session_id && p.depth == depth && p.subtask_id == subtask_id
            }) {
                p.status = status;
                if let Some(summary) = outcome_summary {
                    p.outcome_summary = Some(summary.to_string());
                }
                if p.status == PlanStatus::Complete || p.status == PlanStatus::Failed {
                    p.completed_at = Some(chrono::Utc::now());
                }
            }
            Ok(())
        }

        // --- Fold operations ---

        async fn fold_put(&self, _ctx: &TenantContext, entry: &FoldEntry) -> anyhow::Result<()> {
            self.folds.lock().await.push(entry.clone());
            Ok(())
        }

        async fn fold_get(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            fold_id: Uuid,
        ) -> anyhow::Result<Option<FoldEntry>> {
            let folds = self.folds.lock().await;
            Ok(folds
                .iter()
                .find(|f| f.session_id == session_id && f.fold_id == fold_id)
                .cloned())
        }

        async fn fold_append(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            fold_id: Uuid,
            text: &str,
        ) -> anyhow::Result<()> {
            let mut folds = self.folds.lock().await;
            if let Some(f) = folds
                .iter_mut()
                .find(|f| f.session_id == session_id && f.fold_id == fold_id)
            {
                f.raw_trajectory.push('\n');
                f.raw_trajectory.push_str(text);
                f.token_count = f.raw_trajectory.split_whitespace().count() as i32;
            }
            Ok(())
        }

        async fn fold_complete(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            fold_id: Uuid,
            summary: &str,
            embedding: Vec<f32>,
            compression_ratio: f64,
        ) -> anyhow::Result<()> {
            let mut folds = self.folds.lock().await;
            if let Some(f) = folds
                .iter_mut()
                .find(|f| f.session_id == session_id && f.fold_id == fold_id)
            {
                f.status = FoldStatus::Folded;
                f.fold_summary = Some(summary.to_string());
                f.fold_embedding = Some(embedding);
                f.compression_ratio = Some(compression_ratio);
                f.folded_at = Some(chrono::Utc::now());
            }
            Ok(())
        }

        async fn fold_search(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            _query_embedding: &[f32],
            k: usize,
            include_raw: bool,
        ) -> anyhow::Result<Vec<FoldSummary>> {
            let folds = self.folds.lock().await;
            Ok(folds
                .iter()
                .filter(|f| f.session_id == session_id && f.fold_summary.is_some())
                .take(k)
                .map(|f| FoldSummary {
                    fold_id: f.fold_id,
                    depth: f.depth,
                    fold_summary: f.fold_summary.clone().unwrap_or_default(),
                    token_count: f.token_count,
                    similarity: Some(0.9), // mock similarity
                    raw_trajectory: if include_raw {
                        Some(f.raw_trajectory.clone())
                    } else {
                        None
                    },
                })
                .collect())
        }

        // --- Entity operations ---

        async fn entity_put(
            &self,
            _ctx: &TenantContext,
            entry: &EntityEntry,
        ) -> anyhow::Result<()> {
            self.entities.lock().await.push(entry.clone());
            Ok(())
        }

        async fn entity_find_phonetic(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            name: &str,
        ) -> anyhow::Result<Option<EntityEntry>> {
            let entities = self.entities.lock().await;
            let lower = name.to_lowercase();
            Ok(entities
                .iter()
                .find(|e| e.session_id == session_id && e.entity_name.to_lowercase() == lower)
                .cloned())
        }

        async fn entity_search_ann(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            _query_embedding: &[f32],
            k: usize,
        ) -> anyhow::Result<Vec<EntityEntry>> {
            let entities = self.entities.lock().await;
            Ok(entities
                .iter()
                .filter(|e| e.session_id == session_id)
                .take(k)
                .cloned()
                .collect())
        }

        async fn entity_count(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<usize> {
            let entities = self.entities.lock().await;
            Ok(entities
                .iter()
                .filter(|e| e.session_id == session_id)
                .count())
        }

        async fn fold_count(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<usize> {
            let folds = self.folds.lock().await;
            Ok(folds.iter().filter(|f| f.session_id == session_id).count())
        }

        async fn memo_count(&self, _ctx: &TenantContext) -> anyhow::Result<usize> {
            let memos = self.memos.lock().await;
            Ok(memos.len())
        }

        async fn entity_list_session(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<Vec<EntityEntry>> {
            let entities = self.entities.lock().await;
            Ok(entities
                .iter()
                .filter(|e| e.session_id == session_id)
                .cloned()
                .collect())
        }

        async fn entity_list_all(&self, _ctx: &TenantContext) -> anyhow::Result<Vec<EntityEntry>> {
            let entities = self.entities.lock().await;
            Ok(entities.clone())
        }

        async fn entity_update_state(
            &self,
            _ctx: &TenantContext,
            entity_id: Uuid,
            state: MemoryState,
        ) -> anyhow::Result<()> {
            let mut entities = self.entities.lock().await;
            if let Some(e) = entities.iter_mut().find(|e| e.entity_id == entity_id) {
                e.state = state;
                Ok(())
            } else {
                anyhow::bail!("entity not found: {entity_id}")
            }
        }

        // --- Temporal operations ---

        async fn temporal_put(
            &self,
            _ctx: &TenantContext,
            event: &TemporalEvent,
        ) -> anyhow::Result<()> {
            self.temporal_events.lock().await.push(event.clone());
            Ok(())
        }

        async fn temporal_get_current(
            &self,
            _ctx: &TenantContext,
            entity_id: Uuid,
        ) -> anyhow::Result<Option<TemporalEvent>> {
            let events = self.temporal_events.lock().await;
            Ok(events
                .iter()
                .rfind(|e| e.entity_id == entity_id && e.valid_until.is_none())
                .cloned())
        }

        async fn temporal_invalidate(
            &self,
            _ctx: &TenantContext,
            entity_id: Uuid,
            event_id: Uuid,
        ) -> anyhow::Result<()> {
            let mut events = self.temporal_events.lock().await;
            if let Some(e) = events
                .iter_mut()
                .find(|e| e.entity_id == entity_id && e.event_id == event_id)
            {
                e.valid_until = Some(chrono::Utc::now());
            }
            Ok(())
        }

        // --- Feedback operations ---

        async fn feedback_put(
            &self,
            _ctx: &TenantContext,
            outcome: &FeedbackOutcome,
        ) -> anyhow::Result<()> {
            self.feedback.lock().await.push(outcome.clone());
            Ok(())
        }

        async fn feedback_list_all(&self) -> anyhow::Result<Vec<FeedbackOutcome>> {
            Ok(self.feedback.lock().await.clone())
        }

        async fn delete_session(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<usize> {
            let mut count = 0;
            {
                let mut plans = self.plans.lock().await;
                let before = plans.len();
                plans.retain(|p| p.session_id != session_id);
                count += before - plans.len();
            }
            {
                let mut folds = self.folds.lock().await;
                let before = folds.len();
                folds.retain(|f| f.session_id != session_id);
                count += before - folds.len();
            }
            {
                let mut entities = self.entities.lock().await;
                let before = entities.len();
                entities.retain(|e| e.session_id != session_id);
                count += before - entities.len();
            }
            {
                let mut events = self.temporal_events.lock().await;
                let before = events.len();
                events.retain(|e| e.source_session != session_id);
                count += before - events.len();
            }
            {
                let mut feedback = self.feedback.lock().await;
                let before = feedback.len();
                feedback.retain(|f| f.session_id != session_id);
                count += before - feedback.len();
            }
            Ok(count)
        }

        // --- Edge operations ---

        async fn edge_folded_into(
            &self,
            _ctx: &TenantContext,
            source: Uuid,
            target: Uuid,
            session: Uuid,
        ) -> anyhow::Result<()> {
            self.edges.lock().await.push(MockEdge {
                source,
                target,
                edge_type: "FOLDED_INTO".into(),
                session_id: session,
            });
            Ok(())
        }

        async fn edge_mentioned_in(
            &self,
            _ctx: &TenantContext,
            entity: Uuid,
            fold: Uuid,
            session: Uuid,
        ) -> anyhow::Result<()> {
            self.edges.lock().await.push(MockEdge {
                source: entity,
                target: fold,
                edge_type: "MENTIONED_IN".into(),
                session_id: session,
            });
            Ok(())
        }

        async fn edge_co_occurs(
            &self,
            _ctx: &TenantContext,
            a: Uuid,
            b: Uuid,
            session: Uuid,
        ) -> anyhow::Result<()> {
            self.edges.lock().await.push(MockEdge {
                source: a,
                target: b,
                edge_type: "CO_OCCURS".into(),
                session_id: session,
            });
            Ok(())
        }

        async fn edge_supersedes(
            &self,
            _ctx: &TenantContext,
            new_id: Uuid,
            old_id: Uuid,
            _entity: Uuid,
        ) -> anyhow::Result<()> {
            // Use new_event_id as source, old_event_id as target
            // (entity_id is context, not an edge endpoint for the viz graph)
            self.edges.lock().await.push(MockEdge {
                source: new_id,
                target: old_id,
                edge_type: "SUPERSEDES".into(),
                session_id: Uuid::nil(), // supersedes edges aren't session-scoped
            });
            Ok(())
        }

        async fn edge_list_session(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<Vec<(Uuid, Uuid, String)>> {
            let edges = self.edges.lock().await;
            Ok(edges
                .iter()
                .filter(|e| e.session_id == session_id)
                .map(|e| (e.source, e.target, e.edge_type.clone()))
                .collect())
        }

        async fn edge_list_all(
            &self,
            _ctx: &TenantContext,
        ) -> anyhow::Result<Vec<(Uuid, Uuid, String)>> {
            let edges = self.edges.lock().await;
            Ok(edges
                .iter()
                .map(|e| (e.source, e.target, e.edge_type.clone()))
                .collect())
        }

        async fn edge_list_for_entity(
            &self,
            _ctx: &TenantContext,
            entity_id: Uuid,
        ) -> anyhow::Result<Vec<(Uuid, String)>> {
            let edges = self.edges.lock().await;
            let mut neighbors = Vec::new();
            for e in edges.iter() {
                if e.source == entity_id {
                    neighbors.push((e.target, e.edge_type.clone()));
                } else if e.target == entity_id {
                    neighbors.push((e.source, e.edge_type.clone()));
                }
            }
            Ok(neighbors)
        }

        // --- Intention operations ---

        async fn intention_put(
            &self,
            _ctx: &TenantContext,
            intention: &crate::intention::Intention,
        ) -> anyhow::Result<()> {
            self.intentions.lock().await.push(intention.clone());
            Ok(())
        }

        async fn intention_list(
            &self,
            _ctx: &TenantContext,
        ) -> anyhow::Result<Vec<crate::intention::Intention>> {
            Ok(self.intentions.lock().await.clone())
        }

        async fn intention_update_status(
            &self,
            _ctx: &TenantContext,
            id: Uuid,
            status: &str,
            triggered_at: Option<chrono::DateTime<chrono::Utc>>,
            completed_at: Option<chrono::DateTime<chrono::Utc>>,
        ) -> anyhow::Result<()> {
            let mut intentions = self.intentions.lock().await;
            if let Some(i) = intentions.iter_mut().find(|i| i.id == id) {
                i.status = serde_json::from_str(&format!("\"{status}\""))
                    .unwrap_or(crate::intention::IntentionStatus::Pending);
                i.triggered_at = triggered_at;
                i.completed_at = completed_at;
            }
            Ok(())
        }

        // --- Audit log operations ---

        async fn audit_put(&self, _ctx: &TenantContext, entry: &AuditEntry) -> anyhow::Result<()> {
            self.audit_entries.lock().await.push(entry.clone());
            Ok(())
        }
    }
}
