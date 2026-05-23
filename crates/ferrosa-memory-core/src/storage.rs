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

use crate::context_segment::{ContextSegment, TemporalEdge};
use crate::types::{
    AliasEntry, ApprovalEntry, AuditEntry, ConfidenceScore, DerivedFact, EntityEntry,
    EntityListQuery, EntityListScope, EntityTypeStateCount, FeedbackOutcome, FoldEntry,
    FoldSummary, MaterializedEdge, MemoEntry, MemoryState, PlanNode, PlanStatus, PromotedPredicate,
    ProvenanceStep, RuleEntry, RuleState, TemporalEvent, TenantContext, ToolUsageRow, TypedEdge,
    WarmthEntry,
};

fn entity_list_sessions(
    tenant_id: Uuid,
    caller_session: Uuid,
    scope: EntityListScope,
) -> Option<Vec<Uuid>> {
    let global = crate::scope::tenant_global_session_uuid(tenant_id);
    let nil = Uuid::nil();
    let mut sessions = match scope {
        EntityListScope::Session => vec![caller_session],
        EntityListScope::Global => vec![global, nil],
        EntityListScope::Both => vec![caller_session, global, nil],
        EntityListScope::All => return None,
    };
    sessions.sort_unstable();
    sessions.dedup();
    Some(sessions)
}

fn json_scalar_matches(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (actual, expected) {
        (serde_json::Value::Array(values), _) => values.iter().any(|value| value == expected),
        (serde_json::Value::String(actual), serde_json::Value::String(expected)) => {
            actual == expected
        }
        (serde_json::Value::Number(actual), serde_json::Value::Number(expected)) => {
            actual == expected
        }
        (serde_json::Value::Bool(actual), serde_json::Value::Bool(expected)) => actual == expected,
        _ => actual == expected,
    }
}

fn property_path_value<'a>(
    properties: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = properties;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

fn entity_matches_list_query(
    entry: &EntityEntry,
    ctx: &TenantContext,
    query: &EntityListQuery,
) -> bool {
    if entry.tenant_id != ctx.tenant_id {
        return false;
    }
    if let Some(entity_type) = query.entity_type.as_deref()
        && entry.entity_type != entity_type
    {
        return false;
    }

    query
        .filters
        .iter()
        .all(|(key, expected)| match key.as_str() {
            "entity_id" | "id" => expected == &serde_json::json!(entry.entity_id.to_string()),
            "session_id" => expected == &serde_json::json!(entry.session_id.to_string()),
            "entity_name" | "name" => expected == &serde_json::json!(entry.entity_name),
            "entity_type" => expected == &serde_json::json!(entry.entity_type),
            "state" => expected == &serde_json::json!(entry.state.to_string()),
            "scope" => {
                let scope = match entry.scope {
                    crate::types::EntityScope::Session => "session",
                    crate::types::EntityScope::Global => "global",
                };
                expected == &serde_json::json!(scope)
            }
            "content_hash" => entry
                .content_hash
                .as_ref()
                .is_some_and(|hash| expected == &serde_json::json!(hash)),
            "tags" => match expected {
                serde_json::Value::Array(required) => required.iter().all(|tag| {
                    tag.as_str()
                        .is_some_and(|tag| entry.tags.iter().any(|actual| actual == tag))
                }),
                serde_json::Value::String(tag) => entry.tags.iter().any(|actual| actual == tag),
                _ => false,
            },
            "confidence" => expected
                .as_f64()
                .is_some_and(|expected| (entry.confidence - expected).abs() < f64::EPSILON),
            key => {
                let property_key = key.strip_prefix("properties.").unwrap_or(key);
                property_path_value(&entry.properties, property_key)
                    .is_some_and(|actual| json_scalar_matches(actual, expected))
            }
        })
}

/// Core storage operations for the memory system.
///
/// All methods are async and take `&self` (shared reference) because the
/// underlying CQL client manages its own connection pool.
///
/// Every method that accesses tenant data requires a [`TenantContext`] to
/// enforce tenant isolation at the trait boundary.
///
/// Futures are `Send` so `tokio::spawn` can move a storage call to a
/// worker thread — the HTTP accept loop depends on this to handle
/// connections concurrently. `async fn` in trait position would not
/// imply `Send`; the explicit `impl Future + Send` form is what the
/// spawn site needs.
#[allow(clippy::manual_async_fn)]
pub trait Storage: Send + Sync {
    /// Check memo cache by content hash.
    fn memo_get(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<MemoEntry>>> + Send;

    /// Increment hit count and update last_hit_at on cache hit.
    fn memo_touch(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Store a new memo cache entry.
    fn memo_put(
        &self,
        ctx: &TenantContext,
        entry: &MemoEntry,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Write a plan node.
    fn plan_put(
        &self,
        ctx: &TenantContext,
        node: &PlanNode,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Get all plan nodes for a session up to max_depth.
    fn plan_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        max_depth: Option<i32>,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<PlanNode>>> + Send;

    /// Update a plan node's status and optional outcome summary.
    fn plan_update_status(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        depth: i32,
        subtask_id: &str,
        status: PlanStatus,
        outcome_summary: Option<&str>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    // --- Fold operations (Sprint 2) ---

    /// Create a new active fold.
    fn fold_put(
        &self,
        ctx: &TenantContext,
        entry: &FoldEntry,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Get a fold by ID.
    fn fold_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<FoldEntry>>> + Send;

    /// Append text to a fold's raw_trajectory.
    fn fold_append(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
        text: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Update fold status, summary, embedding, and compression info.
    fn fold_complete(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
        summary: &str,
        embedding: Vec<f32>,
        compression_ratio: f64,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Retrieve fold summaries by embedding similarity (ANN search).
    fn fold_search(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
        include_raw: bool,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<FoldSummary>>> + Send;

    // --- Entity operations (Sprint 3) ---

    /// Store a new entity.
    fn entity_put(
        &self,
        ctx: &TenantContext,
        entry: &EntityEntry,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Find entities by name match, ranked by relevance.
    /// Matches on exact name, :: segment, and substring (in that priority order).
    fn entity_find_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<EntityEntry>>> + Send;

    /// Exact `(entity_name, entity_type)` lookup inside a session. Returns
    /// the fully-populated entity or `None`. Used as the idempotency key for
    /// by-name writers like `ingest_skill`: a substring/fuzzy scan cannot
    /// distinguish a skill from a like-named tag and must not be used to
    /// decide create-vs-update.
    fn entity_find_by_exact_name(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<EntityEntry>>> + Send;

    /// Get a single entity by primary key (targeted lookup, no scan).
    fn entity_get_by_id(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<EntityEntry>>> + Send;

    /// Batch get multiple entities by their IDs (single query).
    fn entity_get_batch(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        entity_ids: &[Uuid],
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<EntityEntry>>> + Send;

    /// Search entities by embedding similarity.
    fn entity_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<EntityEntry>>> + Send;

    /// Count entities in a session (for rate limiting).
    fn entity_count(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// Count folds in a session.
    fn fold_count(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// Count memo cache entries for the tenant.
    fn memo_count(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// List all entities for a session (for consolidation).
    fn entity_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<EntityEntry>>> + Send;

    /// List entities with structured equality predicates over entity columns
    /// and JSON `properties`.
    fn entity_list_matching(
        &self,
        ctx: &TenantContext,
        query: EntityListQuery,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<EntityEntry>>> + Send {
        async move {
            let mut candidates = Vec::new();
            if let Some(sessions) =
                entity_list_sessions(ctx.tenant_id, query.session_id, query.scope)
            {
                for session_id in sessions {
                    candidates.extend(self.entity_list_session(ctx, session_id).await?);
                }
            } else {
                candidates = self.entity_list_all(ctx).await?;
            }

            let limit = query.limit.max(1);
            candidates.sort_by(|a, b| {
                let a_time = a.updated_at.unwrap_or(a.created_at);
                let b_time = b.updated_at.unwrap_or(b.created_at);
                b_time
                    .cmp(&a_time)
                    .then_with(|| a.entity_name.cmp(&b.entity_name))
                    .then_with(|| a.entity_id.cmp(&b.entity_id))
            });
            Ok(candidates
                .into_iter()
                .filter(|entry| entity_matches_list_query(entry, ctx, &query))
                .take(limit)
                .collect())
        }
    }

    /// Return a flat histogram over entity_type and state for one session.
    fn entity_counts_by_type_and_state(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<EntityTypeStateCount>>> + Send;

    /// List all entities for a tenant (for viz snapshot).
    fn entity_list_all(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<EntityEntry>>> + Send;

    /// Stream all entities for a tenant in bounded chunks.
    ///
    /// The default implementation preserves compatibility for non-CQL test
    /// backends by chunking `entity_list_all()`. CQL storage overrides this
    /// with a paged driver iterator so tenant-wide viz SELECT rows can flow to
    /// the websocket without waiting for a full in-memory result set first.
    fn entity_stream_all(
        &self,
        ctx: TenantContext,
        chunk_size: usize,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<Vec<EntityEntry>>>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let chunk_size = chunk_size.max(1);
            match self.entity_list_all(&ctx).await {
                Ok(entities) => {
                    for chunk in entities.chunks(chunk_size) {
                        if tx.send(Ok(chunk.to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                }
            }
        }
    }

    /// List all folds for a tenant (sync/export use only — uses ALLOW FILTERING).
    fn fold_list_all(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<FoldEntry>>> + Send;

    /// Stream all folds for a tenant in bounded chunks.
    ///
    /// The default implementation chunks `fold_list_all()` for in-memory test
    /// backends. CQL storage overrides this with paged driver iteration and a
    /// viz-safe projection that omits heavy raw trajectory/embedding columns.
    fn fold_stream_all(
        &self,
        ctx: TenantContext,
        chunk_size: usize,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<Vec<FoldEntry>>>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let chunk_size = chunk_size.max(1);
            match self.fold_list_all(&ctx).await {
                Ok(folds) => {
                    for chunk in folds.chunks(chunk_size) {
                        if tx.send(Ok(chunk.to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                }
            }
        }
    }

    /// List all temporal events for a tenant (sync/export use only — uses ALLOW FILTERING).
    fn temporal_list_all(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<TemporalEvent>>> + Send;

    /// Update an entity's memory state (promote/demote lifecycle).
    fn entity_update_state(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        state: MemoryState,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Delete a single entity row by primary key.
    fn entity_delete(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send;

    // --- Temporal event operations (Sprint 3) ---

    /// Store a temporal event.
    fn temporal_put(
        &self,
        ctx: &TenantContext,
        event: &TemporalEvent,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Get the current (valid_until IS NULL) fact for an entity.
    fn temporal_get_current(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<TemporalEvent>>> + Send;

    /// Invalidate a temporal event (set valid_until).
    fn temporal_invalidate(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        event_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    // --- Feedback operations (Sprint 3) ---

    /// Record a feedback outcome.
    fn feedback_put(
        &self,
        ctx: &TenantContext,
        outcome: &FeedbackOutcome,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List all feedback outcomes across tenants (batch job use only).
    ///
    /// Returns all rows from `feedback_outcomes`. In production this would
    /// use token-range scanning; the current implementation issues a single
    /// full-table query suitable for moderate data volumes.
    fn feedback_list_all(
        &self,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<FeedbackOutcome>>> + Send;

    // --- Session lifecycle ---

    /// Delete all data for a session (right-to-deletion).
    fn delete_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    // --- Graph edge operations ---

    /// Create a FOLDED_INTO edge (child fold -> parent fold).
    fn edge_folded_into(
        &self,
        ctx: &TenantContext,
        source_fold_id: Uuid,
        target_fold_id: Uuid,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Create a MENTIONED_IN edge (entity -> fold).
    fn edge_mentioned_in(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        fold_id: Uuid,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Create or reinforce a CO_OCCURS_WITH edge (entity <-> entity).
    /// `strength` is the similarity score (0.0-1.0).
    fn edge_co_occurs(
        &self,
        ctx: &TenantContext,
        entity_a: Uuid,
        entity_b: Uuid,
        session_id: Uuid,
        strength: f32,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Delete CO_OCCURS edges not reinforced since `cutoff`.
    fn edge_prune_stale(
        &self,
        ctx: &TenantContext,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// Multiply all CO_OCCURS edge weights by `factor` (0.0–1.0).
    /// Returns the number of edges decayed.
    fn edge_decay_weights(
        &self,
        ctx: &TenantContext,
        factor: f64,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// Create a SUPERSEDES edge (new fact -> old fact).
    fn edge_supersedes(
        &self,
        ctx: &TenantContext,
        new_event_id: Uuid,
        old_event_id: Uuid,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List all edges for a session as (source, target, edge_type) triples.
    ///
    /// Queries all four edge tables (folded_into, mentioned_in, co_occurs_with,
    /// supersedes) and returns a unified list for visualization snapshots.
    fn edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<(Uuid, Uuid, String)>>> + Send;

    /// List all edges for a tenant (for viz snapshot when session is unknown).
    fn edge_list_all(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<(Uuid, Uuid, String)>>> + Send;

    /// Stream all graph edges for a tenant in bounded chunks.
    ///
    /// The default implementation chunks `edge_list_all()` for in-memory test
    /// backends. CQL storage overrides this with paged driver iteration.
    fn edge_stream_all(
        &self,
        ctx: TenantContext,
        chunk_size: usize,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<Vec<(Uuid, Uuid, String)>>>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let chunk_size = chunk_size.max(1);
            match self.edge_list_all(&ctx).await {
                Ok(edges) => {
                    for chunk in edges.chunks(chunk_size) {
                        if tx.send(Ok(chunk.to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                }
            }
        }
    }

    /// List all neighbors of an entity as (neighbor_id, edge_type) pairs.
    ///
    /// Searches mentioned_in, co_occurs_with, and supersedes edges where the
    /// given entity_id appears as source or target. Used for spreading activation.
    fn edge_list_for_entity(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<(Uuid, String)>>> + Send;

    // --- Observability operations (Sprint 4) ---

    /// Sum of hit_count across all memos for this tenant.
    fn memo_total_hits(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<i64>> + Send;

    /// Count folds by status for a tenant.
    fn fold_count_by_status(
        &self,
        ctx: &TenantContext,
        status: crate::types::FoldStatus,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// Count temporal events for a tenant.
    fn temporal_count(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// Count graph edges for a tenant (0 if graph backend not connected).
    fn edge_count(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    // --- Intention operations ---

    /// Store a new intention (repo is on the Intention struct).
    fn intention_put(
        &self,
        ctx: &TenantContext,
        intention: &crate::intention::Intention,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List intentions for a tenant, scoped to a specific repo.
    fn intention_list(
        &self,
        ctx: &TenantContext,
        repo: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<crate::intention::Intention>>> + Send;

    /// List all intentions for a tenant across all repos (for sync/admin).
    fn intention_list_all(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<crate::intention::Intention>>> + Send;

    /// Update an intention's status and optional timestamps.
    fn intention_update_status(
        &self,
        ctx: &TenantContext,
        repo: &str,
        id: Uuid,
        status: &str,
        triggered_at: Option<chrono::DateTime<chrono::Utc>>,
        completed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    // --- Tool usage logging ---

    /// Log a tool call's token usage (fire-and-forget, best-effort).
    #[allow(clippy::too_many_arguments)]
    fn tool_usage_put(
        &self,
        ctx: &TenantContext,
        tool_name: &str,
        repo: &str,
        input_bytes: i32,
        output_bytes: i32,
        estimated_tokens: i32,
        latency_ms: i32,
        error: bool,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Query tool usage for a given day (YYYY-MM-DD string).
    fn tool_usage_query(
        &self,
        ctx: &TenantContext,
        day: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<ToolUsageRow>>> + Send;

    // --- Audit log operations ---

    /// Persist an audit log entry (append-only, STRIDE R1).
    fn audit_put(
        &self,
        ctx: &TenantContext,
        entry: &AuditEntry,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    // --- Warmth operations (Sprint 5) ---

    /// Get the warmth entry for an entity.
    fn warmth_get(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<WarmthEntry>>> + Send;

    /// Store or replace a warmth entry.
    fn warmth_put(
        &self,
        ctx: &TenantContext,
        entry: &WarmthEntry,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Boost an entity's warmth score by `amount`, creating the entry if needed.
    fn warmth_boost(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        amount: f64,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List all warmth entries for a session.
    fn warmth_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<WarmthEntry>>> + Send;

    /// Apply time-based decay to all warmth entries in a session.
    /// Returns the number of entries pruned (dropped below threshold).
    fn warmth_decay_all(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        elapsed_hours: f64,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// Delete a warmth entry by entity_id.
    fn warmth_delete(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    // --- Rule registry operations (Sprint 5) ---

    /// Store a rule entry.
    fn rule_put(
        &self,
        ctx: &TenantContext,
        entry: &RuleEntry,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List rules matching a family and state, sorted by version descending.
    fn rule_list_family(
        &self,
        ctx: &TenantContext,
        family: &str,
        state: RuleState,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<RuleEntry>>> + Send;

    /// List all rules matching a state across families, sorted by family then version descending.
    fn rule_list_active(
        &self,
        ctx: &TenantContext,
        state: RuleState,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<RuleEntry>>> + Send;

    /// Get the highest-version rule entry by rule_id.
    fn rule_get(
        &self,
        ctx: &TenantContext,
        rule_id: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<RuleEntry>>> + Send;

    // --- Approval log operations (Sprint 8) ---

    /// Append an approval decision for an artifact. Append-only authority.
    fn approval_append(
        &self,
        ctx: &TenantContext,
        entry: &ApprovalEntry,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List approval decisions for one artifact, newest first.
    fn approval_list(
        &self,
        ctx: &TenantContext,
        artifact_kind: &str,
        artifact_ref: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<ApprovalEntry>>> + Send;

    /// Get the latest approval decision for one artifact.
    fn approval_latest(
        &self,
        ctx: &TenantContext,
        artifact_kind: &str,
        artifact_ref: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<ApprovalEntry>>> + Send;

    // --- Alias registry operations (Sprint 8) ---

    /// Upsert one alias row for an exact alias + scope pair.
    fn alias_put(
        &self,
        ctx: &TenantContext,
        entry: &AliasEntry,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List all alias rows for one alias name across scopes.
    fn alias_list(
        &self,
        ctx: &TenantContext,
        alias_name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<AliasEntry>>> + Send;

    // --- Derived cache operations (Sprint 5) ---

    /// Get cached derived facts by cache key.
    fn derived_cache_get(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<DerivedFact>>> + Send;

    /// Store derived facts under a cache key.
    fn derived_cache_put(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
        facts: &[DerivedFact],
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Clear derived cache entries whose key starts with `pred`.
    fn derived_cache_clear(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List all derived cache facts for a tenant (used for inspection/debugging).
    /// Returns up to `limit` rows sorted by computed_at DESC.
    fn derived_cache_list_all(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<crate::types::DerivedFactRow>>> + Send;

    /// Store TTL tracking entries for derived facts.
    /// Called when facts are written to record their TTL rule and next maintenance window.
    fn derived_cache_ttl_track_put(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
        facts: &[crate::types::TtlTrackEntry],
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Get TTL tracking entries for a cache key.
    /// Returns (seq, ttl_seconds) tuples.
    fn derived_cache_ttl_track_get(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<(i32, i32)>>> + Send;

    // --- Provenance operations (Sprint 5) ---

    /// Store provenance steps for a derived edge.
    fn provenance_put(
        &self,
        ctx: &TenantContext,
        derived_edge_id: &str,
        steps: &[ProvenanceStep],
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Get provenance steps for a derived edge.
    fn provenance_get(
        &self,
        ctx: &TenantContext,
        derived_edge_id: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<ProvenanceStep>>> + Send;

    // --- Heat telemetry operations (Sprint 5) ---

    /// Record a heat telemetry event for a predicate.
    fn heat_record(
        &self,
        ctx: &TenantContext,
        pred: &str,
        hit: bool,
        compute_ms: Option<i64>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Get aggregated heat telemetry for a predicate over `days`.
    /// Returns (count_of_hits, sum_of_compute_ms).
    fn heat_get(
        &self,
        ctx: &TenantContext,
        pred: &str,
        days: u32,
    ) -> impl std::future::Future<Output = anyhow::Result<(i64, i64)>> + Send;

    // --- Typed edge operations ---

    /// Create a typed, labeled edge between two entities.
    fn typed_edge_put(
        &self,
        ctx: &TenantContext,
        edge: &TypedEdge,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List all typed edges for a session.
    fn typed_edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<TypedEdge>>> + Send;

    /// List every typed edge across all sessions for the tenant. Used by viz
    /// so skills (tenant-global-session), codebase ingests (nil session), and
    /// per-session runs all appear together without the caller having to
    /// enumerate every session_id.
    fn typed_edge_list_all(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<TypedEdge>>> + Send;

    /// Stream all typed edges for a tenant in bounded chunks.
    ///
    /// The default implementation chunks `typed_edge_list_all()` for in-memory
    /// test backends. CQL storage overrides this with paged driver iteration.
    fn typed_edge_stream_all(
        &self,
        ctx: TenantContext,
        chunk_size: usize,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<Vec<TypedEdge>>>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let chunk_size = chunk_size.max(1);
            match self.typed_edge_list_all(&ctx).await {
                Ok(edges) => {
                    for chunk in edges.chunks(chunk_size) {
                        if tx.send(Ok(chunk.to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                }
            }
        }
    }

    /// List typed edges from a specific source entity.
    fn typed_edge_list_from(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        src_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<TypedEdge>>> + Send;

    /// Delete a typed edge by composite key.
    fn typed_edge_delete(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        src_id: Uuid,
        edge_type: &str,
        dst_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send;

    // --- Durable materialization operations (B10) ---

    /// Store a materialized edge (durable).
    fn materialized_edge_put(
        &self,
        ctx: &TenantContext,
        edge: &MaterializedEdge,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Query materialized edges by source ID.
    fn materialized_edges_by_src(
        &self,
        ctx: &TenantContext,
        src_id: &str,
        pred: Option<&str>,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<MaterializedEdge>>> + Send;

    /// Query materialized edges by predicate.
    fn materialized_edges_by_pred(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<MaterializedEdge>>> + Send;

    /// Delete all materialized edges for a predicate (for rematerialization).
    fn materialized_edges_clear(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    // --- Promotion registry operations (B10) ---

    /// Get promotion status for a predicate.
    fn promoted_predicate_get(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<PromotedPredicate>>> + Send;

    /// Set promotion status for a predicate.
    fn promoted_predicate_put(
        &self,
        ctx: &TenantContext,
        entry: &PromotedPredicate,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List all promoted predicates.
    fn promoted_predicate_list(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<PromotedPredicate>>> + Send;

    // --- Context segment operations ---

    /// Store a raw, ordered context segment.
    fn context_segment_put(
        &self,
        ctx: &TenantContext,
        segment: &ContextSegment,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Get a context segment by ID.
    fn context_segment_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        segment_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<ContextSegment>>> + Send;

    /// Get a context segment by stable content hash.
    fn context_segment_get_by_hash(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        content_hash: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<ContextSegment>>> + Send;

    /// Lexical/BM25-style context segment search.
    fn context_segment_search_bm25(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query: &str,
        k: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<ContextSegment>>> + Send;

    /// ANN context segment search.
    fn context_segment_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<ContextSegment>>> + Send;

    /// Store a temporal edge between two context artifacts.
    fn temporal_edge_put(
        &self,
        ctx: &TenantContext,
        edge: &TemporalEdge,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// List temporal edges of a type from a source ID.
    fn temporal_edge_list_from(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        src_id: Uuid,
        edge_type: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<TemporalEdge>>> + Send;

    // --- Confidence operations ---

    /// Store or update a confidence score for a fact.
    fn confidence_put(
        &self,
        ctx: &TenantContext,
        score: &ConfidenceScore,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Retrieve a confidence score by entity and fact hash.
    fn confidence_get(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        fact_hash: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<ConfidenceScore>>> + Send;
}

/// In-memory mock storage for unit tests.
///
/// Stores data in `Vec`s behind a `tokio::sync::Mutex`. Not for production use.
#[cfg(any(test, feature = "mock-storage"))]
pub mod mock {
    use super::*;
    use crate::http::OperatorQuerySurface;
    use crate::types::{DecayZone, FoldStatus};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        pub approvals: Mutex<Vec<ApprovalEntry>>,
        pub aliases: Mutex<Vec<AliasEntry>>,
        pub derived_cache: Mutex<HashMap<String, Vec<DerivedFact>>>,
        pub ttl_track: Mutex<HashMap<String, Vec<crate::types::TtlTrackEntry>>>,
        pub provenance: Mutex<HashMap<String, Vec<ProvenanceStep>>>,
        pub heat_records: Mutex<Vec<(String, bool, Option<i64>)>>,
        pub materialized_edges: Mutex<Vec<MaterializedEdge>>,
        pub promoted_predicates: Mutex<Vec<PromotedPredicate>>,
        pub typed_edges: Mutex<Vec<TypedEdge>>,
        pub confidence_scores: Mutex<Vec<ConfidenceScore>>,
        pub context_segments: Mutex<Vec<ContextSegment>>,
        pub temporal_edges: Mutex<Vec<TemporalEdge>>,
        pub edge_list_all_calls: AtomicUsize,
        pub edge_list_session_calls: AtomicUsize,
        pub edge_list_for_entity_calls: AtomicUsize,
        /// Test hook: when Some, `entity_find_phonetic` returns Err with
        /// this message. Used to verify callers propagate phonetic-scan
        /// errors instead of fail-quieting them into empty results, and
        /// to verify that read paths which shouldn't depend on the fuzzy
        /// scan don't route through it.
        pub force_phonetic_error: Mutex<Option<String>>,
        /// Test hook: when Some, `entity_find_by_exact_name` returns Err
        /// with this message. Parallel to `force_phonetic_error` so a
        /// test can target one lookup path without disabling the other.
        pub force_exact_name_error: Mutex<Option<String>>,
        /// Test hook: when Some((entity_type, msg)), `entity_put` returns
        /// Err(msg) for any entry whose `entity_type` matches. Lets a
        /// test simulate transient CQL write failures targeting a
        /// specific kind of entity (e.g., tag upserts failing under
        /// concurrent contention) without breaking unrelated writes.
        pub force_entity_put_error: Mutex<Option<(String, String)>>,
        /// Test hook: entity types whose `entity_put` call returns Ok but
        /// intentionally leaves storage unchanged. This models storage-layer
        /// false positives where a client reports success even though the row
        /// is not visible after the write.
        pub silently_drop_entity_put_types: Mutex<Vec<String>>,
    }

    impl MockStorage {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl OperatorQuerySurface for MockStorage {
        async fn cql_query_passthrough(
            &self,
            _ctx: &TenantContext,
            query: &str,
            limit: usize,
        ) -> anyhow::Result<serde_json::Value> {
            if query.trim().is_empty() {
                anyhow::bail!("query must not be empty");
            }
            Ok(serde_json::json!({
                "query": query,
                "columns": ["value"],
                "rows": [{"value": format!("limit:{limit}")}],
                "count": 1,
                "total_rows": 1,
                "truncated": false,
                "source": "mock-cql",
            }))
        }

        async fn sparql_query_passthrough(
            &self,
            _ctx: &TenantContext,
            query: &str,
            limit: usize,
        ) -> anyhow::Result<serde_json::Value> {
            if query.trim().is_empty() {
                anyhow::bail!("query must not be empty");
            }
            Ok(serde_json::json!({
                "query": query,
                "columns": ["value"],
                "rows": [{"value": format!("limit:{limit}")}],
                "count": 1,
                "total_rows": 1,
                "truncated": false,
                "source": "mock-sparql",
            }))
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
            if let Some((target_type, msg)) = self.force_entity_put_error.lock().await.as_ref()
                && entry.entity_type == *target_type
            {
                anyhow::bail!("{msg}");
            }
            if self
                .silently_drop_entity_put_types
                .lock()
                .await
                .iter()
                .any(|target_type| target_type == &entry.entity_type)
            {
                return Ok(());
            }
            let mut entities = self.entities.lock().await;
            // Upsert by (session_id, entity_id) — CQL INSERT on the same
            // primary key replaces the row, so the mock should mirror that.
            // Previously this pushed unconditionally, leaving stale entries
            // behind after updates and causing `entity_get_by_id` to
            // return the pre-update value.
            if let Some(pos) = entities
                .iter()
                .position(|e| e.session_id == entry.session_id && e.entity_id == entry.entity_id)
            {
                entities[pos] = entry.clone();
            } else {
                entities.push(entry.clone());
            }
            Ok(())
        }

        async fn entity_find_phonetic(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            name: &str,
        ) -> anyhow::Result<Vec<EntityEntry>> {
            if let Some(msg) = self.force_phonetic_error.lock().await.as_ref() {
                anyhow::bail!("{msg}");
            }
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

        async fn entity_find_by_exact_name(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            name: &str,
            entity_type: &str,
        ) -> anyhow::Result<Option<EntityEntry>> {
            if let Some(msg) = self.force_exact_name_error.lock().await.as_ref() {
                anyhow::bail!("{msg}");
            }
            let entities = self.entities.lock().await;
            Ok(entities
                .iter()
                .find(|e| {
                    e.session_id == session_id
                        && e.entity_name == name
                        && e.entity_type == entity_type
                })
                .cloned())
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

        async fn entity_get_batch(
            &self,
            _ctx: &TenantContext,
            session_id: Uuid,
            entity_ids: &[Uuid],
        ) -> anyhow::Result<Vec<EntityEntry>> {
            let entities = self.entities.lock().await;
            Ok(entities
                .iter()
                .filter(|e| e.session_id == session_id && entity_ids.contains(&e.entity_id))
                .cloned()
                .collect())
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
            ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<Vec<EntityEntry>> {
            let entities = self.entities.lock().await;
            Ok(entities
                .iter()
                .filter(|e| e.tenant_id == ctx.tenant_id && e.session_id == session_id)
                .cloned()
                .collect())
        }

        async fn entity_counts_by_type_and_state(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
        ) -> anyhow::Result<Vec<EntityTypeStateCount>> {
            let entities = self.entities.lock().await;
            let mut counts: std::collections::BTreeMap<(String, String), usize> =
                std::collections::BTreeMap::new();
            for entity in entities
                .iter()
                .filter(|e| e.tenant_id == ctx.tenant_id && e.session_id == session_id)
            {
                *counts
                    .entry((entity.entity_type.clone(), entity.state.to_string()))
                    .or_insert(0) += 1;
            }
            Ok(counts
                .into_iter()
                .map(|((entity_type, state), count)| EntityTypeStateCount {
                    entity_type,
                    state: serde_json::from_str(&format!("\"{state}\""))
                        .expect("known MemoryState string"),
                    count,
                })
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

        async fn entity_delete(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
            entity_id: Uuid,
        ) -> anyhow::Result<bool> {
            let mut entities = self.entities.lock().await;
            let before = entities.len();
            entities.retain(|entry| {
                !(entry.tenant_id == ctx.tenant_id
                    && entry.session_id == session_id
                    && entry.entity_id == entity_id)
            });
            Ok(entities.len() != before)
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
            self.edge_list_session_calls.fetch_add(1, Ordering::Relaxed);
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
            self.edge_list_all_calls.fetch_add(1, Ordering::Relaxed);
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
            self.edge_list_for_entity_calls
                .fetch_add(1, Ordering::Relaxed);
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
                    reputation: 0.0,
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

        async fn warmth_delete(&self, ctx: &TenantContext, entity_id: Uuid) -> anyhow::Result<()> {
            let mut entries = self.warmth_entries.lock().await;
            entries.retain(|e| !(e.tenant_id == ctx.tenant_id && e.entity_id == entity_id));
            Ok(())
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
            matched.sort_by_key(|r| std::cmp::Reverse(r.version));
            Ok(matched)
        }

        async fn rule_list_active(
            &self,
            _ctx: &TenantContext,
            state: RuleState,
        ) -> anyhow::Result<Vec<RuleEntry>> {
            let rules = self.rules.lock().await;
            let mut matched: Vec<RuleEntry> =
                rules.iter().filter(|r| r.state == state).cloned().collect();
            matched.sort_by(|a, b| {
                a.family
                    .cmp(&b.family)
                    .then_with(|| b.version.cmp(&a.version))
                    .then_with(|| a.rule_id.cmp(&b.rule_id))
            });
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
            matched.sort_by_key(|r| std::cmp::Reverse(r.version));
            Ok(matched.first().cloned().cloned())
        }

        async fn approval_append(
            &self,
            _ctx: &TenantContext,
            entry: &ApprovalEntry,
        ) -> anyhow::Result<()> {
            self.approvals.lock().await.push(entry.clone());
            Ok(())
        }

        async fn approval_list(
            &self,
            _ctx: &TenantContext,
            artifact_kind: &str,
            artifact_ref: &str,
        ) -> anyhow::Result<Vec<ApprovalEntry>> {
            let approvals = self.approvals.lock().await;
            let mut matched: Vec<ApprovalEntry> = approvals
                .iter()
                .filter(|entry| {
                    entry.artifact_kind.to_string() == artifact_kind
                        && entry.artifact_ref == artifact_ref
                })
                .cloned()
                .collect();
            matched.sort_by(|left, right| {
                right
                    .created_at
                    .cmp(&left.created_at)
                    .then_with(|| right.approval_id.cmp(&left.approval_id))
            });
            Ok(matched)
        }

        async fn approval_latest(
            &self,
            ctx: &TenantContext,
            artifact_kind: &str,
            artifact_ref: &str,
        ) -> anyhow::Result<Option<ApprovalEntry>> {
            Ok(self
                .approval_list(ctx, artifact_kind, artifact_ref)
                .await?
                .into_iter()
                .next())
        }

        async fn alias_put(&self, _ctx: &TenantContext, entry: &AliasEntry) -> anyhow::Result<()> {
            let mut aliases = self.aliases.lock().await;
            if let Some(pos) = aliases.iter().position(|existing| {
                existing.alias_name == entry.alias_name
                    && existing.scope_kind == entry.scope_kind
                    && existing.scope_ref == entry.scope_ref
            }) {
                aliases[pos] = entry.clone();
            } else {
                aliases.push(entry.clone());
            }
            Ok(())
        }

        async fn alias_list(
            &self,
            _ctx: &TenantContext,
            alias_name: &str,
        ) -> anyhow::Result<Vec<AliasEntry>> {
            let aliases = self.aliases.lock().await;
            let mut matched: Vec<AliasEntry> = aliases
                .iter()
                .filter(|entry| entry.alias_name == alias_name)
                .cloned()
                .collect();
            matched.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
            Ok(matched)
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

        async fn derived_cache_list_all(
            &self,
            _ctx: &TenantContext,
            limit: usize,
        ) -> anyhow::Result<Vec<crate::types::DerivedFactRow>> {
            let cache = self.derived_cache.lock().await;
            let mut all_rows: Vec<crate::types::DerivedFactRow> = Vec::new();
            for (cache_key, facts) in cache.iter() {
                for fact in facts.iter() {
                    all_rows.push(crate::types::DerivedFactRow {
                        source_id: fact.src_id.clone(),
                        predicate: fact.pred.clone(),
                        target_id: fact.dst_id.clone(),
                        confidence: fact.confidence,
                        rule_id: fact.rule_id.clone(),
                        cache_key: Some(cache_key.clone()),
                        computed_at: chrono::Utc::now().to_string(),
                    });
                }
            }
            all_rows.truncate(limit);
            Ok(all_rows)
        }

        async fn derived_cache_ttl_track_put(
            &self,
            _ctx: &TenantContext,
            cache_key: &str,
            facts: &[crate::types::TtlTrackEntry],
        ) -> anyhow::Result<()> {
            let mut track = self.ttl_track.lock().await;
            track.insert(cache_key.to_string(), facts.to_vec());
            Ok(())
        }

        async fn derived_cache_ttl_track_get(
            &self,
            _ctx: &TenantContext,
            cache_key: &str,
        ) -> anyhow::Result<Vec<(i32, i32)>> {
            let track = self.ttl_track.lock().await;
            Ok(track
                .get(cache_key)
                .map(|entries| entries.iter().map(|e| (e.seq, e.ttl_seconds)).collect())
                .unwrap_or_default())
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

        async fn typed_edge_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<TypedEdge>> {
            let edges = self.typed_edges.lock().await;
            Ok(edges
                .iter()
                .filter(|e| e.tenant_id == ctx.tenant_id)
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

        async fn typed_edge_delete(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
            src_id: Uuid,
            edge_type: &str,
            dst_id: Uuid,
        ) -> anyhow::Result<bool> {
            let mut edges = self.typed_edges.lock().await;
            let before = edges.len();
            edges.retain(|edge| {
                !(edge.tenant_id == ctx.tenant_id
                    && edge.session_id == session_id
                    && edge.src_id == src_id
                    && edge.edge_type == edge_type
                    && edge.dst_id == dst_id)
            });
            Ok(edges.len() != before)
        }

        async fn context_segment_put(
            &self,
            ctx: &TenantContext,
            segment: &ContextSegment,
        ) -> anyhow::Result<()> {
            let mut segments = self.context_segments.lock().await;
            if let Some(existing) = segments.iter_mut().find(|s| {
                s.tenant_id == ctx.tenant_id
                    && s.session_id == segment.session_id
                    && s.segment_id == segment.segment_id
            }) {
                *existing = segment.clone();
            } else {
                segments.push(segment.clone());
            }
            Ok(())
        }

        async fn context_segment_get(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
            segment_id: Uuid,
        ) -> anyhow::Result<Option<ContextSegment>> {
            let segments = self.context_segments.lock().await;
            Ok(segments
                .iter()
                .find(|s| {
                    s.tenant_id == ctx.tenant_id
                        && s.session_id == session_id
                        && s.segment_id == segment_id
                })
                .cloned())
        }

        async fn context_segment_get_by_hash(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
            content_hash: &str,
        ) -> anyhow::Result<Option<ContextSegment>> {
            let segments = self.context_segments.lock().await;
            Ok(segments
                .iter()
                .find(|s| {
                    s.tenant_id == ctx.tenant_id
                        && s.session_id == session_id
                        && s.content_hash == content_hash
                })
                .cloned())
        }

        async fn context_segment_search_bm25(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
            query: &str,
            k: usize,
        ) -> anyhow::Result<Vec<ContextSegment>> {
            let q_terms: Vec<String> = query
                .split_whitespace()
                .map(|s| s.to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            let segments = self.context_segments.lock().await;
            let mut scored: Vec<(usize, ContextSegment)> = segments
                .iter()
                .filter(|s| s.tenant_id == ctx.tenant_id && s.session_id == session_id)
                .filter_map(|s| {
                    let text = s.bm25_text.to_lowercase();
                    let score = q_terms
                        .iter()
                        .filter(|term| text.contains(term.as_str()))
                        .count();
                    (score > 0).then(|| (score, s.clone()))
                })
                .collect();
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.segment_index.cmp(&b.1.segment_index))
            });
            Ok(scored.into_iter().take(k).map(|(_, s)| s).collect())
        }

        async fn context_segment_search_ann(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
            query_embedding: &[f32],
            k: usize,
        ) -> anyhow::Result<Vec<ContextSegment>> {
            let segments = self.context_segments.lock().await;
            let mut scored: Vec<(ordered_float::OrderedFloat<f64>, ContextSegment)> = segments
                .iter()
                .filter(|s| s.tenant_id == ctx.tenant_id && s.session_id == session_id)
                .filter_map(|s| {
                    s.segment_embedding.as_ref().map(|embedding| {
                        (
                            ordered_float::OrderedFloat(crate::context_segment::cosine(
                                query_embedding,
                                embedding,
                            )),
                            s.clone(),
                        )
                    })
                })
                .collect();
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            Ok(scored.into_iter().take(k).map(|(_, s)| s).collect())
        }

        async fn temporal_edge_put(
            &self,
            ctx: &TenantContext,
            edge: &TemporalEdge,
        ) -> anyhow::Result<()> {
            let mut edges = self.temporal_edges.lock().await;
            if !edges.iter().any(|e| {
                e.tenant_id == ctx.tenant_id
                    && e.session_id == edge.session_id
                    && e.src_id == edge.src_id
                    && e.edge_type == edge.edge_type
                    && e.dst_id == edge.dst_id
            }) {
                edges.push(edge.clone());
            }
            Ok(())
        }

        async fn temporal_edge_list_from(
            &self,
            ctx: &TenantContext,
            session_id: Uuid,
            src_id: Uuid,
            edge_type: &str,
        ) -> anyhow::Result<Vec<TemporalEdge>> {
            let edges = self.temporal_edges.lock().await;
            let mut found: Vec<_> = edges
                .iter()
                .filter(|e| {
                    e.tenant_id == ctx.tenant_id
                        && e.session_id == session_id
                        && e.src_id == src_id
                        && e.edge_type == edge_type
                })
                .cloned()
                .collect();
            found.sort_by_key(|e| e.ordinal);
            Ok(found)
        }

        async fn confidence_put(
            &self,
            _ctx: &TenantContext,
            score: &ConfidenceScore,
        ) -> anyhow::Result<()> {
            let mut scores = self.confidence_scores.lock().await;
            if let Some(existing) = scores
                .iter_mut()
                .find(|s| s.entity_id == score.entity_id && s.fact_hash == score.fact_hash)
            {
                *existing = score.clone();
            } else {
                scores.push(score.clone());
            }
            Ok(())
        }

        async fn confidence_get(
            &self,
            _ctx: &TenantContext,
            entity_id: Uuid,
            fact_hash: &str,
        ) -> anyhow::Result<Option<ConfidenceScore>> {
            let scores = self.confidence_scores.lock().await;
            Ok(scores
                .iter()
                .find(|s| s.entity_id == entity_id && s.fact_hash == fact_hash)
                .cloned())
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

        // --- entity_find_by_exact_name ---
        //
        // Regression for bug-ingest-skill-bulk-nondeterminism: the phonetic
        // scan did substring/type-blind matching, which meant
        // `ingest_skill` could allocate a duplicate entity_id when the
        // existing row's name collided with another entity type or when
        // the scan's filtering view was stale. The exact-name path takes
        // `entity_type` as part of the key so no tag/skill crosstalk can
        // happen, and keeps the lookup to a single logical row.
        async fn put_named_entity(
            storage: &MockStorage,
            ctx: &TenantContext,
            session_id: Uuid,
            name: &str,
            entity_type: &str,
        ) -> Uuid {
            let entity_id = Uuid::new_v4();
            let entry = EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name: name.into(),
                entity_type: entity_type.into(),
                source_fold_id: None,
                context_snippet: String::new(),
                entity_embedding: None,
                confidence: 1.0,
                state: crate::types::MemoryState::default(),
                created_at: chrono::Utc::now(),
                description: None,
                description_embedding: None,
                tags: Vec::new(),
                properties: serde_json::json!({}),
                content_hash: None,
                updated_at: None,
                scope: crate::types::EntityScope::Global,
                ingested_by_session: None,
            };
            storage.entity_put(ctx, &entry).await.unwrap();
            entity_id
        }

        #[tokio::test]
        async fn entity_find_by_exact_name_returns_hit() {
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };
            let session_id = Uuid::new_v4();
            let id = put_named_entity(&storage, &ctx, session_id, "tdd", "skill").await;

            let found = storage
                .entity_find_by_exact_name(&ctx, session_id, "tdd", "skill")
                .await
                .unwrap()
                .expect("skill must be returned");
            assert_eq!(found.entity_id, id);
        }

        #[tokio::test]
        async fn entity_find_by_exact_name_returns_none_on_miss() {
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };
            let session_id = Uuid::new_v4();
            let found = storage
                .entity_find_by_exact_name(&ctx, session_id, "never-ingested", "skill")
                .await
                .unwrap();
            assert!(found.is_none());
        }

        #[tokio::test]
        async fn entity_find_by_exact_name_filters_by_entity_type() {
            // Same name, two types. Exact lookup must not cross types;
            // otherwise the skill/tag auto-created by ingest_skill can be
            // confused for the skill itself.
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };
            let session_id = Uuid::new_v4();
            let tag_id = put_named_entity(&storage, &ctx, session_id, "refactor", "tag").await;
            let skill_id = put_named_entity(&storage, &ctx, session_id, "refactor", "skill").await;

            let as_skill = storage
                .entity_find_by_exact_name(&ctx, session_id, "refactor", "skill")
                .await
                .unwrap()
                .expect("skill row");
            let as_tag = storage
                .entity_find_by_exact_name(&ctx, session_id, "refactor", "tag")
                .await
                .unwrap()
                .expect("tag row");
            assert_eq!(as_skill.entity_id, skill_id);
            assert_eq!(as_tag.entity_id, tag_id);
        }

        #[tokio::test]
        async fn entity_find_by_exact_name_ignores_substring_matches() {
            // Phonetic scan matched on substring, which could return the
            // wrong entity under bulk load. Exact match must be exact.
            let storage = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };
            let session_id = Uuid::new_v4();
            put_named_entity(&storage, &ctx, session_id, "tdd-extended", "skill").await;

            let miss = storage
                .entity_find_by_exact_name(&ctx, session_id, "tdd", "skill")
                .await
                .unwrap();
            assert!(miss.is_none(), "substring match must not count as exact");
        }
    }
}
