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
    AuditEntry, DerivedFact, EntityEntry, FeedbackOutcome, FoldEntry, FoldSummary,
    MaterializedEdge, MemoEntry, MemoryState, PlanNode, PlanStatus, PromotedPredicate,
    ProvenanceStep, RuleEntry, RuleState, TemporalEvent, TenantContext, ToolUsageRow, TypedEdge,
    WarmthEntry,
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

    /// Find entities by name match, ranked by relevance.
    /// Matches on exact name, :: segment, and substring (in that priority order).
    async fn entity_find_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
    ) -> anyhow::Result<Vec<EntityEntry>>;

    /// Get a single entity by primary key (targeted lookup, no scan).
    async fn entity_get_by_id(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        entity_id: Uuid,
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

    /// List all folds for a tenant (sync/export use only — uses ALLOW FILTERING).
    async fn fold_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<FoldEntry>>;

    /// List all temporal events for a tenant (sync/export use only — uses ALLOW FILTERING).
    async fn temporal_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<TemporalEvent>>;

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

    /// Create or reinforce a CO_OCCURS_WITH edge (entity <-> entity).
    /// `strength` is the similarity score (0.0-1.0).
    async fn edge_co_occurs(
        &self,
        ctx: &TenantContext,
        entity_a: Uuid,
        entity_b: Uuid,
        session_id: Uuid,
        strength: f32,
    ) -> anyhow::Result<()>;

    /// Delete CO_OCCURS edges not reinforced since `cutoff`.
    async fn edge_prune_stale(
        &self,
        ctx: &TenantContext,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<usize>;

    /// Multiply all CO_OCCURS edge weights by `factor` (0.0–1.0).
    /// Returns the number of edges decayed.
    async fn edge_decay_weights(&self, ctx: &TenantContext, factor: f64) -> anyhow::Result<usize>;

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

    // --- Observability operations (Sprint 4) ---

    /// Sum of hit_count across all memos for this tenant.
    async fn memo_total_hits(&self, ctx: &TenantContext) -> anyhow::Result<i64>;

    /// Count folds by status for a tenant.
    async fn fold_count_by_status(
        &self,
        ctx: &TenantContext,
        status: crate::types::FoldStatus,
    ) -> anyhow::Result<usize>;

    /// Count temporal events for a tenant.
    async fn temporal_count(&self, ctx: &TenantContext) -> anyhow::Result<usize>;

    /// Count graph edges for a tenant (0 if graph backend not connected).
    async fn edge_count(&self, ctx: &TenantContext) -> anyhow::Result<usize>;

    // --- Intention operations ---

    /// Store a new intention (repo is on the Intention struct).
    async fn intention_put(
        &self,
        ctx: &TenantContext,
        intention: &crate::intention::Intention,
    ) -> anyhow::Result<()>;

    /// List intentions for a tenant, scoped to a specific repo.
    async fn intention_list(
        &self,
        ctx: &TenantContext,
        repo: &str,
    ) -> anyhow::Result<Vec<crate::intention::Intention>>;

    /// List all intentions for a tenant across all repos (for sync/admin).
    async fn intention_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<crate::intention::Intention>>;

    /// Update an intention's status and optional timestamps.
    async fn intention_update_status(
        &self,
        ctx: &TenantContext,
        repo: &str,
        id: Uuid,
        status: &str,
        triggered_at: Option<chrono::DateTime<chrono::Utc>>,
        completed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<()>;

    // --- Tool usage logging ---

    /// Log a tool call's token usage (fire-and-forget, best-effort).
    #[allow(clippy::too_many_arguments)]
    async fn tool_usage_put(
        &self,
        ctx: &TenantContext,
        tool_name: &str,
        repo: &str,
        input_bytes: i32,
        output_bytes: i32,
        estimated_tokens: i32,
        latency_ms: i32,
        error: bool,
    ) -> anyhow::Result<()>;

    /// Query tool usage for a given day (YYYY-MM-DD string).
    async fn tool_usage_query(
        &self,
        ctx: &TenantContext,
        day: &str,
    ) -> anyhow::Result<Vec<ToolUsageRow>>;

    // --- Audit log operations ---

    /// Persist an audit log entry (append-only, STRIDE R1).
    async fn audit_put(&self, ctx: &TenantContext, entry: &AuditEntry) -> anyhow::Result<()>;

    // --- Warmth operations (Sprint 5) ---

    /// Get the warmth entry for an entity.
    async fn warmth_get(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<WarmthEntry>>;

    /// Store or replace a warmth entry.
    async fn warmth_put(&self, ctx: &TenantContext, entry: &WarmthEntry) -> anyhow::Result<()>;

    /// Boost an entity's warmth score by `amount`, creating the entry if needed.
    async fn warmth_boost(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        amount: f64,
        session_id: Uuid,
    ) -> anyhow::Result<()>;

    /// List all warmth entries for a session.
    async fn warmth_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<WarmthEntry>>;

    /// Apply time-based decay to all warmth entries in a session.
    /// Returns the number of entries pruned (dropped below threshold).
    async fn warmth_decay_all(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        elapsed_hours: f64,
    ) -> anyhow::Result<usize>;

    // --- Rule registry operations (Sprint 5) ---

    /// Store a rule entry.
    async fn rule_put(&self, ctx: &TenantContext, entry: &RuleEntry) -> anyhow::Result<()>;

    /// List rules matching a family and state, sorted by version descending.
    async fn rule_list_family(
        &self,
        ctx: &TenantContext,
        family: &str,
        state: RuleState,
    ) -> anyhow::Result<Vec<RuleEntry>>;

    /// Get the highest-version rule entry by rule_id.
    async fn rule_get(
        &self,
        ctx: &TenantContext,
        rule_id: &str,
    ) -> anyhow::Result<Option<RuleEntry>>;

    // --- Derived cache operations (Sprint 5) ---

    /// Get cached derived facts by cache key.
    async fn derived_cache_get(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
    ) -> anyhow::Result<Vec<DerivedFact>>;

    /// Store derived facts under a cache key.
    async fn derived_cache_put(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
        facts: &[DerivedFact],
    ) -> anyhow::Result<()>;

    /// Clear derived cache entries whose key starts with `pred`.
    async fn derived_cache_clear(&self, ctx: &TenantContext, pred: &str) -> anyhow::Result<()>;

    // --- Provenance operations (Sprint 5) ---

    /// Store provenance steps for a derived edge.
    async fn provenance_put(
        &self,
        ctx: &TenantContext,
        derived_edge_id: &str,
        steps: &[ProvenanceStep],
    ) -> anyhow::Result<()>;

    /// Get provenance steps for a derived edge.
    async fn provenance_get(
        &self,
        ctx: &TenantContext,
        derived_edge_id: &str,
    ) -> anyhow::Result<Vec<ProvenanceStep>>;

    // --- Heat telemetry operations (Sprint 5) ---

    /// Record a heat telemetry event for a predicate.
    async fn heat_record(
        &self,
        ctx: &TenantContext,
        pred: &str,
        hit: bool,
        compute_ms: Option<i64>,
    ) -> anyhow::Result<()>;

    /// Get aggregated heat telemetry for a predicate over `days`.
    /// Returns (count_of_hits, sum_of_compute_ms).
    async fn heat_get(
        &self,
        ctx: &TenantContext,
        pred: &str,
        days: u32,
    ) -> anyhow::Result<(i64, i64)>;

    // --- Typed edge operations ---

    /// Create a typed, labeled edge between two entities.
    async fn typed_edge_put(&self, ctx: &TenantContext, edge: &TypedEdge) -> anyhow::Result<()>;

    /// List all typed edges for a session.
    async fn typed_edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<TypedEdge>>;

    /// List typed edges from a specific source entity.
    async fn typed_edge_list_from(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        src_id: Uuid,
    ) -> anyhow::Result<Vec<TypedEdge>>;

    // --- Durable materialization operations (B10) ---

    /// Store a materialized edge (durable).
    async fn materialized_edge_put(
        &self,
        ctx: &TenantContext,
        edge: &MaterializedEdge,
    ) -> anyhow::Result<()>;

    /// Query materialized edges by source ID.
    async fn materialized_edges_by_src(
        &self,
        ctx: &TenantContext,
        src_id: &str,
        pred: Option<&str>,
    ) -> anyhow::Result<Vec<MaterializedEdge>>;

    /// Query materialized edges by predicate.
    async fn materialized_edges_by_pred(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> anyhow::Result<Vec<MaterializedEdge>>;

    /// Delete all materialized edges for a predicate (for rematerialization).
    async fn materialized_edges_clear(&self, ctx: &TenantContext, pred: &str)
    -> anyhow::Result<()>;

    // --- Promotion registry operations (B10) ---

    /// Get promotion status for a predicate.
    async fn promoted_predicate_get(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> anyhow::Result<Option<PromotedPredicate>>;

    /// Set promotion status for a predicate.
    async fn promoted_predicate_put(
        &self,
        ctx: &TenantContext,
        entry: &PromotedPredicate,
    ) -> anyhow::Result<()>;

    /// List all promoted predicates.
    async fn promoted_predicate_list(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<PromotedPredicate>>;
}

/// In-memory mock storage for unit tests.
///
/// Stores data in `Vec`s behind a `tokio::sync::Mutex`. Not for production use.
#[cfg(any(test, feature = "mock-storage"))]
pub mod mock {
    use super::*;
    use crate::types::{DecayZone, FoldStatus};
    use std::collections::HashMap;
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
        pub warmth_entries: Mutex<Vec<WarmthEntry>>,
        pub rules: Mutex<Vec<RuleEntry>>,
        pub derived_cache: Mutex<HashMap<String, Vec<DerivedFact>>>,
        pub provenance: Mutex<HashMap<String, Vec<ProvenanceStep>>>,
        pub heat_records: Mutex<Vec<(String, bool, Option<i64>)>>,
        pub materialized_edges: Mutex<Vec<MaterializedEdge>>,
        pub promoted_predicates: Mutex<Vec<PromotedPredicate>>,
        pub typed_edges: Mutex<Vec<TypedEdge>>,
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
        ) -> anyhow::Result<Vec<EntityEntry>> {
            let entities = self.entities.lock().await;
            let lower = name.to_lowercase();
            let mut scored: Vec<(u8, &EntityEntry)> = entities
                .iter()
                .filter(|e| e.session_id == session_id)
                .filter_map(|e| {
                    let en = e.entity_name.to_lowercase();
                    if en == lower {
                        Some((0, e)) // exact match
                    } else if en.split("::").any(|seg| seg == lower) {
                        Some((1, e)) // segment match
                    } else if en.contains(&lower) {
                        Some((2, e)) // substring match
                    } else {
                        None
                    }
                })
                .collect();
            scored.sort_by_key(|(rank, _)| *rank);
            Ok(scored.into_iter().map(|(_, e)| e.clone()).collect())
        }

        async fn entity_get_by_id(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            entity_id: Uuid,
        ) -> anyhow::Result<Option<EntityEntry>> {
            let entities = self.entities.lock().await;
            Ok(entities
                .iter()
                .find(|e| e.session_id == session_id && e.entity_id == entity_id)
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

        async fn fold_list_all(&self, _ctx: &TenantContext) -> anyhow::Result<Vec<FoldEntry>> {
            let folds = self.folds.lock().await;
            Ok(folds.clone())
        }

        async fn temporal_list_all(
            &self,
            _ctx: &TenantContext,
        ) -> anyhow::Result<Vec<TemporalEvent>> {
            let events = self.temporal_events.lock().await;
            Ok(events.clone())
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
            _strength: f32,
        ) -> anyhow::Result<()> {
            self.edges.lock().await.push(MockEdge {
                source: a,
                target: b,
                edge_type: "CO_OCCURS".into(),
                session_id: session,
            });
            Ok(())
        }

        async fn edge_prune_stale(
            &self,
            _ctx: &TenantContext,
            _cutoff: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn edge_decay_weights(
            &self,
            _ctx: &TenantContext,
            _factor: f64,
        ) -> anyhow::Result<usize> {
            Ok(0)
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

        // --- Observability operations ---

        async fn memo_total_hits(&self, _ctx: &TenantContext) -> anyhow::Result<i64> {
            let memos = self.memos.lock().await;
            Ok(memos.iter().map(|m| m.hit_count).sum())
        }

        async fn fold_count_by_status(
            &self,
            _ctx: &TenantContext,
            status: FoldStatus,
        ) -> anyhow::Result<usize> {
            let folds = self.folds.lock().await;
            Ok(folds.iter().filter(|f| f.status == status).count())
        }

        async fn temporal_count(&self, _ctx: &TenantContext) -> anyhow::Result<usize> {
            Ok(self.temporal_events.lock().await.len())
        }

        async fn edge_count(&self, _ctx: &TenantContext) -> anyhow::Result<usize> {
            Ok(self.edges.lock().await.len())
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
            repo: &str,
        ) -> anyhow::Result<Vec<crate::intention::Intention>> {
            Ok(self
                .intentions
                .lock()
                .await
                .iter()
                .filter(|i| i.repo == repo)
                .cloned()
                .collect())
        }

        async fn intention_list_all(
            &self,
            _ctx: &TenantContext,
        ) -> anyhow::Result<Vec<crate::intention::Intention>> {
            Ok(self.intentions.lock().await.clone())
        }

        async fn intention_update_status(
            &self,
            _ctx: &TenantContext,
            _repo: &str,
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

        async fn tool_usage_put(
            &self,
            _ctx: &TenantContext,
            _tool_name: &str,
            _repo: &str,
            _input_bytes: i32,
            _output_bytes: i32,
            _estimated_tokens: i32,
            _latency_ms: i32,
            _error: bool,
        ) -> anyhow::Result<()> {
            Ok(()) // no-op in tests
        }

        async fn tool_usage_query(
            &self,
            _ctx: &TenantContext,
            _day: &str,
        ) -> anyhow::Result<Vec<ToolUsageRow>> {
            Ok(vec![])
        }

        // --- Warmth operations ---

        async fn warmth_get(
            &self,
            ctx: &TenantContext,
            entity_id: Uuid,
        ) -> anyhow::Result<Option<WarmthEntry>> {
            let entries = self.warmth_entries.lock().await;
            Ok(entries
                .iter()
                .find(|e| e.tenant_id == ctx.tenant_id && e.entity_id == entity_id)
                .cloned())
        }

        async fn warmth_put(
            &self,
            _ctx: &TenantContext,
            entry: &WarmthEntry,
        ) -> anyhow::Result<()> {
            let mut entries = self.warmth_entries.lock().await;
            if let Some(existing) = entries
                .iter_mut()
                .find(|e| e.tenant_id == entry.tenant_id && e.entity_id == entry.entity_id)
            {
                *existing = entry.clone();
            } else {
                entries.push(entry.clone());
            }
            Ok(())
        }

        async fn warmth_boost(
            &self,
            ctx: &TenantContext,
            entity_id: Uuid,
            amount: f64,
            session_id: Uuid,
        ) -> anyhow::Result<()> {
            let mut entries = self.warmth_entries.lock().await;
            if let Some(existing) = entries
                .iter_mut()
                .find(|e| e.tenant_id == ctx.tenant_id && e.entity_id == entity_id)
            {
                existing.warmth += amount;
                existing.access_count += 1;
                existing.last_accessed_at = chrono::Utc::now();
                existing.updated_at = chrono::Utc::now();
            } else {
                entries.push(WarmthEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id,
                    session_id,
                    warmth: amount,
                    pagerank: 0.0,
                    last_accessed_at: chrono::Utc::now(),
                    access_count: 1,
                    decay_zone: DecayZone::Knowledge,
                    updated_at: chrono::Utc::now(),
                });
            }
            Ok(())
        }

        async fn warmth_list_session(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<Vec<WarmthEntry>> {
            let entries = self.warmth_entries.lock().await;
            Ok(entries
                .iter()
                .filter(|e| e.tenant_id == ctx.tenant_id && e.session_id == session_id)
                .cloned()
                .collect())
        }

        async fn warmth_decay_all(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            elapsed_hours: f64,
        ) -> anyhow::Result<usize> {
            let mut entries = self.warmth_entries.lock().await;
            for entry in entries.iter_mut().filter(|e| e.session_id == session_id) {
                entry.warmth *=
                    (-elapsed_hours * entry.decay_zone.decay_multiplier() * 0.1_f64).exp();
            }
            let before = entries.len();
            entries.retain(|e| e.session_id != session_id || e.warmth >= 0.01);
            Ok(before - entries.len())
        }

        // --- Rule registry operations ---

        async fn rule_put(&self, _ctx: &TenantContext, entry: &RuleEntry) -> anyhow::Result<()> {
            self.rules.lock().await.push(entry.clone());
            Ok(())
        }

        async fn rule_list_family(
            &self,
            _ctx: &TenantContext,
            family: &str,
            state: RuleState,
        ) -> anyhow::Result<Vec<RuleEntry>> {
            let rules = self.rules.lock().await;
            let mut matched: Vec<RuleEntry> = rules
                .iter()
                .filter(|r| r.family == family && r.state == state)
                .cloned()
                .collect();
            matched.sort_by(|a, b| b.version.cmp(&a.version));
            Ok(matched)
        }

        async fn rule_get(
            &self,
            _ctx: &TenantContext,
            rule_id: &str,
        ) -> anyhow::Result<Option<RuleEntry>> {
            let rules = self.rules.lock().await;
            let mut matched: Vec<&RuleEntry> =
                rules.iter().filter(|r| r.rule_id == rule_id).collect();
            matched.sort_by(|a, b| b.version.cmp(&a.version));
            Ok(matched.first().cloned().cloned())
        }

        // --- Derived cache operations ---

        async fn derived_cache_get(
            &self,
            _ctx: &TenantContext,
            cache_key: &str,
        ) -> anyhow::Result<Vec<DerivedFact>> {
            let cache = self.derived_cache.lock().await;
            Ok(cache.get(cache_key).cloned().unwrap_or_default())
        }

        async fn derived_cache_put(
            &self,
            _ctx: &TenantContext,
            cache_key: &str,
            facts: &[DerivedFact],
        ) -> anyhow::Result<()> {
            let mut cache = self.derived_cache.lock().await;
            cache.insert(cache_key.to_string(), facts.to_vec());
            Ok(())
        }

        async fn derived_cache_clear(
            &self,
            _ctx: &TenantContext,
            pred: &str,
        ) -> anyhow::Result<()> {
            let mut cache = self.derived_cache.lock().await;
            cache.retain(|k, _| !k.starts_with(pred));
            Ok(())
        }

        // --- Provenance operations ---

        async fn provenance_put(
            &self,
            _ctx: &TenantContext,
            derived_edge_id: &str,
            steps: &[ProvenanceStep],
        ) -> anyhow::Result<()> {
            let mut prov = self.provenance.lock().await;
            prov.insert(derived_edge_id.to_string(), steps.to_vec());
            Ok(())
        }

        async fn provenance_get(
            &self,
            _ctx: &TenantContext,
            derived_edge_id: &str,
        ) -> anyhow::Result<Vec<ProvenanceStep>> {
            let prov = self.provenance.lock().await;
            Ok(prov.get(derived_edge_id).cloned().unwrap_or_default())
        }

        // --- Heat telemetry operations ---

        async fn heat_record(
            &self,
            _ctx: &TenantContext,
            pred: &str,
            hit: bool,
            compute_ms: Option<i64>,
        ) -> anyhow::Result<()> {
            self.heat_records
                .lock()
                .await
                .push((pred.to_string(), hit, compute_ms));
            Ok(())
        }

        async fn heat_get(
            &self,
            _ctx: &TenantContext,
            pred: &str,
            _days: u32,
        ) -> anyhow::Result<(i64, i64)> {
            let records = self.heat_records.lock().await;
            let mut hit_count: i64 = 0;
            let mut compute_sum: i64 = 0;
            for (p, hit, ms) in records.iter() {
                if p == pred {
                    if *hit {
                        hit_count += 1;
                    }
                    if let Some(val) = ms {
                        compute_sum += val;
                    }
                }
            }
            Ok((hit_count, compute_sum))
        }

        // --- Durable materialization operations (B10) ---

        async fn materialized_edge_put(
            &self,
            _ctx: &TenantContext,
            edge: &MaterializedEdge,
        ) -> anyhow::Result<()> {
            self.materialized_edges.lock().await.push(edge.clone());
            Ok(())
        }

        async fn materialized_edges_by_src(
            &self,
            ctx: &TenantContext,
            src_id: &str,
            pred: Option<&str>,
        ) -> anyhow::Result<Vec<MaterializedEdge>> {
            let edges = self.materialized_edges.lock().await;
            Ok(edges
                .iter()
                .filter(|e| {
                    e.tenant_id == ctx.tenant_id
                        && e.src_id == src_id
                        && pred.is_none_or(|p| e.pred == p)
                })
                .cloned()
                .collect())
        }

        async fn materialized_edges_by_pred(
            &self,
            ctx: &TenantContext,
            pred: &str,
        ) -> anyhow::Result<Vec<MaterializedEdge>> {
            let edges = self.materialized_edges.lock().await;
            Ok(edges
                .iter()
                .filter(|e| e.tenant_id == ctx.tenant_id && e.pred == pred)
                .cloned()
                .collect())
        }

        async fn materialized_edges_clear(
            &self,
            ctx: &TenantContext,
            pred: &str,
        ) -> anyhow::Result<()> {
            let mut edges = self.materialized_edges.lock().await;
            edges.retain(|e| !(e.tenant_id == ctx.tenant_id && e.pred == pred));
            Ok(())
        }

        // --- Promotion registry operations (B10) ---

        async fn promoted_predicate_get(
            &self,
            ctx: &TenantContext,
            pred: &str,
        ) -> anyhow::Result<Option<PromotedPredicate>> {
            let entries = self.promoted_predicates.lock().await;
            Ok(entries
                .iter()
                .find(|e| e.tenant_id == ctx.tenant_id && e.pred == pred)
                .cloned())
        }

        async fn promoted_predicate_put(
            &self,
            _ctx: &TenantContext,
            entry: &PromotedPredicate,
        ) -> anyhow::Result<()> {
            let mut entries = self.promoted_predicates.lock().await;
            entries.retain(|e| !(e.tenant_id == entry.tenant_id && e.pred == entry.pred));
            entries.push(entry.clone());
            Ok(())
        }

        async fn promoted_predicate_list(
            &self,
            ctx: &TenantContext,
        ) -> anyhow::Result<Vec<PromotedPredicate>> {
            let entries = self.promoted_predicates.lock().await;
            Ok(entries
                .iter()
                .filter(|e| e.tenant_id == ctx.tenant_id)
                .cloned()
                .collect())
        }

        // --- Typed edge operations ---

        async fn typed_edge_put(
            &self,
            _ctx: &TenantContext,
            edge: &TypedEdge,
        ) -> anyhow::Result<()> {
            let mut edges = self.typed_edges.lock().await;
            edges.push(edge.clone());
            Ok(())
        }

        async fn typed_edge_list_session(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<Vec<TypedEdge>> {
            let edges = self.typed_edges.lock().await;
            Ok(edges
                .iter()
                .filter(|e| e.tenant_id == ctx.tenant_id && e.session_id == session_id)
                .cloned()
                .collect())
        }

        async fn typed_edge_list_from(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
            src_id: Uuid,
        ) -> anyhow::Result<Vec<TypedEdge>> {
            let edges = self.typed_edges.lock().await;
            Ok(edges
                .iter()
                .filter(|e| {
                    e.tenant_id == ctx.tenant_id && e.session_id == session_id && e.src_id == src_id
                })
                .cloned()
                .collect())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn test_warmth_crud() {
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };
            let eid = Uuid::new_v4();
            let sid = Uuid::new_v4();

            // Initially empty
            assert!(storage.warmth_get(&ctx, eid).await.unwrap().is_none());

            // Boost creates entry
            storage.warmth_boost(&ctx, eid, 0.3, sid).await.unwrap();
            let entry = storage.warmth_get(&ctx, eid).await.unwrap().unwrap();
            assert!((entry.warmth - 0.3).abs() < f64::EPSILON);
            assert_eq!(entry.access_count, 1);

            // Second boost increments
            storage.warmth_boost(&ctx, eid, 0.3, sid).await.unwrap();
            let entry = storage.warmth_get(&ctx, eid).await.unwrap().unwrap();
            assert!((entry.warmth - 0.6).abs() < f64::EPSILON);
            assert_eq!(entry.access_count, 2);

            // List session
            let entries = storage.warmth_list_session(&ctx, sid).await.unwrap();
            assert_eq!(entries.len(), 1);
        }

        #[tokio::test]
        async fn test_warmth_decay() {
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };
            let sid = Uuid::new_v4();

            // Create entry with high warmth
            storage
                .warmth_boost(&ctx, Uuid::new_v4(), 5.0, sid)
                .await
                .unwrap();

            // Decay should reduce warmth but not prune
            let pruned = storage.warmth_decay_all(&ctx, sid, 10.0).await.unwrap();
            assert_eq!(pruned, 0); // 5.0 * exp(-0.1 * 10 * 1.0) ~ 1.84 > 0.01

            // Heavy decay should prune
            let pruned = storage.warmth_decay_all(&ctx, sid, 100.0).await.unwrap();
            assert_eq!(pruned, 1);
        }

        #[tokio::test]
        async fn test_rule_registry() {
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };

            let rule = RuleEntry {
                tenant_id: ctx.tenant_id,
                rule_id: "test_rule".into(),
                version: 1,
                name: "Test".into(),
                family: "test_family".into(),
                state: RuleState::Active,
                rule_body: "related(X, Y) :- co_occurs(X, Y).".into(),
                rule_weight: 1.0,
                incremental: false,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            storage.rule_put(&ctx, &rule).await.unwrap();

            let found = storage.rule_get(&ctx, "test_rule").await.unwrap();
            assert!(found.is_some());
            assert_eq!(found.unwrap().version, 1);

            let family = storage
                .rule_list_family(&ctx, "test_family", RuleState::Active)
                .await
                .unwrap();
            assert_eq!(family.len(), 1);
        }

        #[tokio::test]
        async fn test_derived_cache() {
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };

            let facts = vec![DerivedFact {
                src_id: "a".into(),
                pred: "related".into(),
                dst_id: "b".into(),
                confidence: 0.9,
                rule_id: "r1".into(),
                support_count: 1,
                provenance: vec![],
            }];

            // Miss
            assert!(
                storage
                    .derived_cache_get(&ctx, "key1")
                    .await
                    .unwrap()
                    .is_empty()
            );

            // Put + hit
            storage
                .derived_cache_put(&ctx, "key1", &facts)
                .await
                .unwrap();
            let cached = storage.derived_cache_get(&ctx, "key1").await.unwrap();
            assert_eq!(cached.len(), 1);

            // Clear by prefix
            storage.derived_cache_clear(&ctx, "key").await.unwrap();
            assert!(
                storage
                    .derived_cache_get(&ctx, "key1")
                    .await
                    .unwrap()
                    .is_empty()
            );
        }

        #[tokio::test]
        async fn test_provenance() {
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };

            let steps = vec![ProvenanceStep {
                parent_src: "a".into(),
                parent_pred: "co_occurs".into(),
                parent_dst: "b".into(),
                parent_kind: "base".into(),
            }];

            storage.provenance_put(&ctx, "edge1", &steps).await.unwrap();
            let got = storage.provenance_get(&ctx, "edge1").await.unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].parent_pred, "co_occurs");
        }

        #[tokio::test]
        async fn test_heat_telemetry() {
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };

            storage
                .heat_record(&ctx, "co_occurs", true, Some(50))
                .await
                .unwrap();
            storage
                .heat_record(&ctx, "co_occurs", true, Some(30))
                .await
                .unwrap();
            storage
                .heat_record(&ctx, "co_occurs", false, Some(10))
                .await
                .unwrap();

            let (hits, compute) = storage.heat_get(&ctx, "co_occurs", 7).await.unwrap();
            assert_eq!(hits, 2);
            assert_eq!(compute, 90);
        }

        #[tokio::test]
        async fn test_materialized_edge_crud() {
            use crate::types::MaterializedEdge;

            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };

            let edge = MaterializedEdge {
                tenant_id: ctx.tenant_id,
                src_id: "entity_a".into(),
                shard: 0,
                pred: "related_to".into(),
                dst_id: "entity_b".into(),
                rule_id: "r1".into(),
                support_count: 2,
                confidence: 0.85,
                batch_id: "batch_001".into(),
                materialized_at: chrono::Utc::now(),
            };

            // Initially empty
            let results = storage
                .materialized_edges_by_src(&ctx, "entity_a", None)
                .await
                .unwrap();
            assert!(results.is_empty());

            // Put + query by src
            storage.materialized_edge_put(&ctx, &edge).await.unwrap();
            let results = storage
                .materialized_edges_by_src(&ctx, "entity_a", None)
                .await
                .unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].dst_id, "entity_b");

            // Query by src + pred filter
            let results = storage
                .materialized_edges_by_src(&ctx, "entity_a", Some("related_to"))
                .await
                .unwrap();
            assert_eq!(results.len(), 1);
            let results = storage
                .materialized_edges_by_src(&ctx, "entity_a", Some("other_pred"))
                .await
                .unwrap();
            assert!(results.is_empty());

            // Query by pred
            let results = storage
                .materialized_edges_by_pred(&ctx, "related_to")
                .await
                .unwrap();
            assert_eq!(results.len(), 1);

            // Clear
            storage
                .materialized_edges_clear(&ctx, "related_to")
                .await
                .unwrap();
            let results = storage
                .materialized_edges_by_pred(&ctx, "related_to")
                .await
                .unwrap();
            assert!(results.is_empty());
        }

        #[tokio::test]
        async fn test_promoted_predicate_crud() {
            use crate::types::{PromotedPredicate, PromotionStatus};

            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };

            // Initially empty
            let result = storage
                .promoted_predicate_get(&ctx, "related_to")
                .await
                .unwrap();
            assert!(result.is_none());

            let entry = PromotedPredicate {
                tenant_id: ctx.tenant_id,
                pred: "related_to".into(),
                promotion_score: 1500.0,
                estimated_rows: 500,
                materialized_at: None,
                batch_id: None,
                status: PromotionStatus::Candidate,
            };

            // Put + get
            storage.promoted_predicate_put(&ctx, &entry).await.unwrap();
            let result = storage
                .promoted_predicate_get(&ctx, "related_to")
                .await
                .unwrap();
            assert!(result.is_some());
            assert_eq!(result.unwrap().status, PromotionStatus::Candidate);

            // List
            let list = storage.promoted_predicate_list(&ctx).await.unwrap();
            assert_eq!(list.len(), 1);

            // Upsert (update status)
            let updated = PromotedPredicate {
                status: PromotionStatus::Promoted,
                materialized_at: Some(chrono::Utc::now()),
                batch_id: Some("batch_001".into()),
                ..entry.clone()
            };
            storage
                .promoted_predicate_put(&ctx, &updated)
                .await
                .unwrap();
            let list = storage.promoted_predicate_list(&ctx).await.unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].status, PromotionStatus::Promoted);
            assert!(list[0].batch_id.is_some());
        }
    }
}
