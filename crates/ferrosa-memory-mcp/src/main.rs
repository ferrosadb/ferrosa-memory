//! # ferrosa-memory-mcp
//!
//! MCP server binary that exposes Ferrosa's memory tools via stdio or HTTP+SSE.
//!
//! Connects to a real Ferrosa cluster via CQL (cdrs-tokio). If the initial
//! connection fails, starts serving immediately with a "reconnecting" backend
//! that returns errors, while a background task retries with exponential backoff.
//! Never falls back to mock storage — mock silently loses data.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ferrosa_memory_core::auth;
use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::dispatch;
use ferrosa_memory_core::http;
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::transport;
use ferrosa_memory_core::types::*;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

/// Storage wrapper that holds an `Option<CqlStorage>` behind a `RwLock`.
///
/// When `inner` is `None`, the server is still reconnecting — all Storage
/// methods return a descriptive error. Once connected, the background task
/// swaps in `Some(cql)` and all subsequent calls route to the real backend.
///
/// If a connected session starts returning connection errors (e.g. rolling
/// restart), the delegate macro marks it disconnected and signals the
/// reconnect watcher to re-establish the session.
///
/// ## Cancel safety
///
/// A generation counter prevents a stale connection-error callback from
/// disconnecting a freshly reconnected session. `mark_disconnected` only
/// nulls the session when the generation matches the one captured before
/// the failed query. `notify_one` is called while the write lock is still
/// held so the signal cannot be lost to cancellation.
struct ReconnectingStorage {
    inner: RwLock<Option<CqlStorage>>,
    /// Monotonically increasing generation — bumped on every `set_connected`.
    /// Prevents stale errors from disconnecting a fresh session.
    generation: AtomicU64,
    /// Signalled when a connection error is detected, waking the reconnect loop.
    reconnect_signal: tokio::sync::Notify,
    /// Config needed to reconnect (stashed at creation time).
    cql_config: FerrosaCqlConfig,
}

impl ReconnectingStorage {
    /// Create with an already-connected CQL backend.
    fn connected(cql: CqlStorage, config: FerrosaCqlConfig) -> Self {
        Self {
            inner: RwLock::new(Some(cql)),
            generation: AtomicU64::new(1),
            reconnect_signal: tokio::sync::Notify::new(),
            cql_config: config,
        }
    }

    /// Create in "reconnecting" state — no backend available yet.
    fn disconnected(config: FerrosaCqlConfig) -> Self {
        Self {
            inner: RwLock::new(None),
            generation: AtomicU64::new(0),
            reconnect_signal: tokio::sync::Notify::new(),
            cql_config: config,
        }
    }

    /// Swap in a newly connected CQL backend and bump the generation.
    async fn set_connected(&self, cql: CqlStorage) {
        let mut guard = self.inner.write().await;
        *guard = Some(cql);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Read the current generation (captured before a query, checked after).
    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Mark as disconnected and signal the reconnect watcher, but only if the
    /// generation hasn't changed since the caller observed the error. This
    /// prevents a stale error from a pre-reconnect query from killing a fresh
    /// session.
    ///
    /// The signal is sent while the write lock is held so the pair
    /// (set-to-None, notify) is atomic w.r.t. cancellation.
    async fn mark_disconnected(&self, observed_gen: u64) {
        let mut guard = self.inner.write().await;
        let current = self.generation.load(Ordering::Acquire);
        if current != observed_gen {
            // A reconnect already happened — this is a stale error.
            return;
        }
        if guard.is_some() {
            tracing::warn!("CQL connection lost — entering reconnecting mode");
            *guard = None;
            // Signal while lock is held: even if this future is cancelled after
            // the signal, the None state is already visible to readers.
            self.reconnect_signal.notify_one();
        }
    }
}

/// Error returned when CQL is not yet connected.
const NOT_CONNECTED_MSG: &str = "CQL connection not yet established, retrying in background...";

/// Returns true if the error looks like a connection / transport failure
/// (as opposed to a query-level error like "table not found").
fn is_connection_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("broken pipe")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("transport")
        || msg.contains("channel closed")
        || msg.contains("io error")
        || msg.contains("timed out")
        || msg.contains("not connected")
        || msg.contains("eof")
}

/// Macro to delegate a Storage trait method through the RwLock.
///
/// Captures the generation before the query. On connection errors, calls
/// `mark_disconnected` with the captured generation so stale errors from
/// pre-reconnect queries cannot kill a fresh session.
macro_rules! delegate {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        let conn_gen = $self.current_generation();
        let guard = $self.inner.read().await;
        match guard.as_ref() {
            Some(cql) => {
                let result = cql.$method($($arg),*).await;
                if let Err(ref e) = result {
                    if is_connection_error(e) {
                        drop(guard); // release read lock before taking write lock
                        $self.mark_disconnected(conn_gen).await;
                    }
                }
                result
            }
            None => Err(anyhow::anyhow!(NOT_CONNECTED_MSG)),
        }
    }};
}

/// Delegate all Storage methods through the RwLock<Option<CqlStorage>>.
impl Storage for ReconnectingStorage {
    async fn memo_get(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<Option<MemoEntry>> {
        delegate!(self, memo_get, ctx, content_hash, model_version)
    }

    async fn memo_touch(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<()> {
        delegate!(self, memo_touch, ctx, content_hash, model_version)
    }

    async fn memo_put(&self, ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()> {
        delegate!(self, memo_put, ctx, entry)
    }

    async fn plan_put(&self, ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()> {
        delegate!(self, plan_put, ctx, node)
    }

    async fn plan_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        max_depth: Option<i32>,
    ) -> anyhow::Result<Vec<PlanNode>> {
        delegate!(self, plan_get, ctx, session_id, max_depth)
    }

    async fn plan_update_status(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        depth: i32,
        subtask_id: &str,
        status: PlanStatus,
        outcome_summary: Option<&str>,
    ) -> anyhow::Result<()> {
        delegate!(
            self,
            plan_update_status,
            ctx,
            session_id,
            depth,
            subtask_id,
            status,
            outcome_summary
        )
    }

    async fn fold_put(&self, ctx: &TenantContext, entry: &FoldEntry) -> anyhow::Result<()> {
        delegate!(self, fold_put, ctx, entry)
    }

    async fn fold_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        fold_id: uuid::Uuid,
    ) -> anyhow::Result<Option<FoldEntry>> {
        delegate!(self, fold_get, ctx, session_id, fold_id)
    }

    async fn fold_append(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        fold_id: uuid::Uuid,
        text: &str,
    ) -> anyhow::Result<()> {
        delegate!(self, fold_append, ctx, session_id, fold_id, text)
    }

    async fn fold_complete(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        fold_id: uuid::Uuid,
        summary: &str,
        embedding: Vec<f32>,
        compression_ratio: f64,
    ) -> anyhow::Result<()> {
        delegate!(
            self,
            fold_complete,
            ctx,
            session_id,
            fold_id,
            summary,
            embedding,
            compression_ratio
        )
    }

    async fn fold_search(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query_embedding: &[f32],
        k: usize,
        include_raw: bool,
    ) -> anyhow::Result<Vec<FoldSummary>> {
        delegate!(
            self,
            fold_search,
            ctx,
            session_id,
            query_embedding,
            k,
            include_raw
        )
    }

    async fn entity_put(&self, ctx: &TenantContext, entry: &EntityEntry) -> anyhow::Result<()> {
        delegate!(self, entity_put, ctx, entry)
    }

    async fn entity_find_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        name: &str,
    ) -> anyhow::Result<Option<EntityEntry>> {
        delegate!(self, entity_find_phonetic, ctx, session_id, name)
    }

    async fn entity_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        delegate!(self, entity_search_ann, ctx, session_id, query_embedding, k)
    }

    async fn entity_count(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, entity_count, ctx, session_id)
    }

    async fn fold_count(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, fold_count, ctx, session_id)
    }

    async fn memo_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        delegate!(self, memo_count, ctx)
    }

    async fn entity_update_state(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
        state: MemoryState,
    ) -> anyhow::Result<()> {
        delegate!(self, entity_update_state, ctx, entity_id, state)
    }

    async fn entity_list_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        delegate!(self, entity_list_session, ctx, session_id)
    }

    async fn entity_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<EntityEntry>> {
        delegate!(self, entity_list_all, ctx)
    }

    async fn temporal_put(&self, ctx: &TenantContext, event: &TemporalEvent) -> anyhow::Result<()> {
        delegate!(self, temporal_put, ctx, event)
    }

    async fn temporal_get_current(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<Option<TemporalEvent>> {
        delegate!(self, temporal_get_current, ctx, entity_id)
    }

    async fn temporal_invalidate(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
        event_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        delegate!(self, temporal_invalidate, ctx, entity_id, event_id)
    }

    async fn feedback_put(
        &self,
        ctx: &TenantContext,
        outcome: &FeedbackOutcome,
    ) -> anyhow::Result<()> {
        delegate!(self, feedback_put, ctx, outcome)
    }

    async fn feedback_list_all(&self) -> anyhow::Result<Vec<FeedbackOutcome>> {
        let guard = self.inner.read().await;
        match guard.as_ref() {
            Some(cql) => cql.feedback_list_all().await,
            None => Err(anyhow::anyhow!(NOT_CONNECTED_MSG)),
        }
    }

    async fn delete_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, delete_session, ctx, session_id)
    }

    async fn edge_folded_into(
        &self,
        ctx: &TenantContext,
        source: uuid::Uuid,
        target: uuid::Uuid,
        session: uuid::Uuid,
    ) -> anyhow::Result<()> {
        delegate!(self, edge_folded_into, ctx, source, target, session)
    }

    async fn edge_mentioned_in(
        &self,
        ctx: &TenantContext,
        entity: uuid::Uuid,
        fold: uuid::Uuid,
        session: uuid::Uuid,
    ) -> anyhow::Result<()> {
        delegate!(self, edge_mentioned_in, ctx, entity, fold, session)
    }

    async fn edge_co_occurs(
        &self,
        ctx: &TenantContext,
        a: uuid::Uuid,
        b: uuid::Uuid,
        session: uuid::Uuid,
        strength: f32,
    ) -> anyhow::Result<()> {
        delegate!(self, edge_co_occurs, ctx, a, b, session, strength)
    }

    async fn edge_prune_stale(
        &self,
        ctx: &TenantContext,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<usize> {
        delegate!(self, edge_prune_stale, ctx, cutoff)
    }

    async fn edge_decay_weights(&self, ctx: &TenantContext, factor: f64) -> anyhow::Result<usize> {
        delegate!(self, edge_decay_weights, ctx, factor)
    }

    async fn edge_supersedes(
        &self,
        ctx: &TenantContext,
        new_id: uuid::Uuid,
        old_id: uuid::Uuid,
        entity: uuid::Uuid,
    ) -> anyhow::Result<()> {
        delegate!(self, edge_supersedes, ctx, new_id, old_id, entity)
    }

    async fn edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<(uuid::Uuid, uuid::Uuid, String)>> {
        delegate!(self, edge_list_session, ctx, session_id)
    }

    async fn edge_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<(uuid::Uuid, uuid::Uuid, String)>> {
        delegate!(self, edge_list_all, ctx)
    }

    async fn edge_list_for_entity(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<(uuid::Uuid, String)>> {
        delegate!(self, edge_list_for_entity, ctx, entity_id)
    }

    async fn intention_put(
        &self,
        ctx: &TenantContext,
        intention: &ferrosa_memory_core::intention::Intention,
    ) -> anyhow::Result<()> {
        delegate!(self, intention_put, ctx, intention)
    }

    async fn intention_list(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::intention::Intention>> {
        delegate!(self, intention_list, ctx)
    }

    async fn intention_update_status(
        &self,
        ctx: &TenantContext,
        id: uuid::Uuid,
        status: &str,
        triggered_at: Option<chrono::DateTime<chrono::Utc>>,
        completed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<()> {
        delegate!(
            self,
            intention_update_status,
            ctx,
            id,
            status,
            triggered_at,
            completed_at
        )
    }

    async fn audit_put(
        &self,
        ctx: &TenantContext,
        entry: &ferrosa_memory_core::types::AuditEntry,
    ) -> anyhow::Result<()> {
        delegate!(self, audit_put, ctx, entry)
    }

    async fn memo_total_hits(&self, ctx: &TenantContext) -> anyhow::Result<i64> {
        delegate!(self, memo_total_hits, ctx)
    }

    async fn fold_count_by_status(
        &self,
        ctx: &TenantContext,
        status: ferrosa_memory_core::types::FoldStatus,
    ) -> anyhow::Result<usize> {
        delegate!(self, fold_count_by_status, ctx, status)
    }

    async fn temporal_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        delegate!(self, temporal_count, ctx)
    }

    async fn edge_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        delegate!(self, edge_count, ctx)
    }
}

/// Backoff schedule for CQL reconnection: 1s, 2s, 4s, 8s, 16s, then 30s.
fn next_backoff(attempt: u32) -> Duration {
    if attempt < 5 {
        Duration::from_secs(1 << attempt)
    } else {
        Duration::from_secs(30)
    }
}

/// Persistent reconnection watcher.
///
/// Waits for `reconnect_signal` (fired on initial failure or mid-operation
/// connection loss), then retries with exponential backoff until connected.
/// After connecting, goes back to waiting for the next signal — survives
/// rolling restarts, network blips, etc.
///
/// ## Cancel safety
///
/// This task is spawned via `tokio::spawn` and never aborted, so
/// cancellation within the retry loop is not a concern. The `notified()`
/// call at the top is cancel-safe per tokio docs. After successful
/// reconnection the inner state is checked to avoid spurious retry
/// cycles from stale permits.
async fn cql_reconnect_watcher(storage: Arc<ReconnectingStorage>) {
    loop {
        // Wait until someone signals that reconnection is needed.
        storage.reconnect_signal.notified().await;

        // Check if we actually need to reconnect — a permit may have been
        // stored by a stale mark_disconnected that lost the generation race,
        // or by a second error while we were already reconnecting.
        {
            let guard = storage.inner.read().await;
            if guard.is_some() {
                tracing::debug!("reconnect watcher: spurious signal, already connected");
                continue;
            }
        }

        tracing::info!("reconnect watcher: connection loss detected, starting reconnection");

        let mut attempt: u32 = 0;
        loop {
            let delay = next_backoff(attempt);
            tracing::info!(
                attempt = attempt + 1,
                delay_secs = delay.as_secs(),
                "CQL reconnection attempt scheduled"
            );
            tokio::time::sleep(delay).await;

            match CqlStorage::connect(&storage.cql_config).await {
                Ok(cql) => {
                    tracing::info!("CQL reconnection successful");
                    storage.set_connected(cql).await;
                    break; // back to waiting for next signal
                }
                Err(e) => {
                    attempt = attempt.saturating_add(1);
                    tracing::warn!(attempt, "CQL reconnection failed: {e}");
                }
            }
        }
    }
}

/// Idle-consolidation configuration passed from `main()`.
struct IdleConsolidationConfig {
    idle_seconds: u64,
    stale_edge_max_days: u64,
    edge_decay_factor: f64,
}

/// Background task that runs dream consolidation and edge maintenance after
/// a period of tool-call inactivity. Resets the timer on every tool call.
/// Only runs when the dirty flag indicates new writes since the last run.
async fn idle_consolidation_loop<S: Storage + Send + Sync + 'static>(
    session: Arc<dispatch::SessionState>,
    storage: Arc<S>,
    ctx: Arc<TenantContext>,
    cfg: IdleConsolidationConfig,
) {
    let timeout_dur = Duration::from_secs(cfg.idle_seconds);
    loop {
        // Wait for the first tool call activity.
        session.last_activity.notified().await;

        // Reset timer on each subsequent activity until we hit a timeout.
        while tokio::time::timeout(timeout_dur, session.last_activity.notified())
            .await
            .is_ok()
        {}

        // Only consolidate if there were writes since the last run.
        if !session.dirty.swap(false, Ordering::Relaxed) {
            continue;
        }

        let sid = match session.default_session_id {
            Some(id) => id,
            None => continue,
        };

        run_idle_consolidation(storage.as_ref(), &ctx, sid, &cfg).await;
    }
}

/// Executes consolidation, edge weight decay, and optional stale-edge pruning.
async fn run_idle_consolidation<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: uuid::Uuid,
    cfg: &IdleConsolidationConfig,
) {
    // 1. Decay existing edge weights so unreinforced edges lose strength.
    if cfg.edge_decay_factor < 1.0 {
        match storage.edge_decay_weights(ctx, cfg.edge_decay_factor).await {
            Ok(n) if n > 0 => tracing::info!(
                decayed = n,
                factor = cfg.edge_decay_factor,
                "edge decay applied"
            ),
            Err(e) => tracing::warn!("edge decay failed: {e}"),
            _ => {}
        }
    }

    // 2. Run consolidation — rediscovered edges get fresh weights.
    match ferrosa_memory_core::dream::run_consolidation(storage, ctx, session_id).await {
        Ok(r) => tracing::info!(
            entities = r.entities_processed,
            connections = r.connections_created,
            "idle consolidation complete"
        ),
        Err(e) => tracing::warn!("idle consolidation failed: {e}"),
    }

    // 3. Optionally prune stale edges (0 = disabled).
    if cfg.stale_edge_max_days > 0 {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(cfg.stale_edge_max_days as i64);
        match storage.edge_prune_stale(ctx, cutoff).await {
            Ok(pruned) if pruned > 0 => {
                tracing::info!(pruned, "idle edge pruning complete")
            }
            Err(e) => tracing::warn!("idle edge pruning failed: {e}"),
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let debug = std::env::args().any(|a| a == "--debug");

    let default_filter = if debug {
        "debug,cdrs_tokio=debug,hyper=info,reqwest=info"
    } else {
        "ferrosa_memory_core=warn,ferrosa_memory_mcp=warn"
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .init();

    if debug {
        tracing::info!("ferrosa-memory-mcp starting (debug mode)");
    }

    let config = match ferrosa_memory_core::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config not found ({e}), using defaults");
            ferrosa_memory_core::config::parse_config(
                "[ferrosa]\ncontact_points = [\"localhost:19042\"]\n",
            )?
        }
    };

    let tenant_id = config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .unwrap_or_else(uuid::Uuid::new_v4);
    let default_session_id = config
        .server
        .session_id
        .as_ref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok());
    let metrics = Arc::new(ferrosa_memory_core::metrics::MemoryMetrics::new()?);
    tracing::info!("metrics registered");

    // Connect to real Ferrosa — retry in background if initial connect fails.
    // Never fall back to mock storage (mock silently loses data).
    let storage: Arc<ReconnectingStorage> = match CqlStorage::connect(&config.ferrosa).await {
        Ok(cql) => {
            tracing::info!("connected to Ferrosa CQL cluster");
            Arc::new(ReconnectingStorage::connected(cql, config.ferrosa.clone()))
        }
        Err(e) => {
            tracing::warn!(
                "CQL connection failed ({e}), starting in reconnecting mode — \
                 tools will return errors until connection is established"
            );
            let storage = Arc::new(ReconnectingStorage::disconnected(config.ferrosa.clone()));
            // Signal immediately so the watcher starts its first attempt.
            storage.reconnect_signal.notify_one();
            storage
        }
    };

    // Always spawn the reconnect watcher — it handles both initial failure
    // and mid-operation connection loss (rolling restarts, network blips).
    tokio::spawn(cql_reconnect_watcher(Arc::clone(&storage)));

    // Connect graph client via HTTP (non-fatal if it fails)
    match ferrosa_memory_core::graph::GraphClient::connect(
        &ferrosa_memory_core::graph::GraphConfig {
            http_url: config.graph.http_url.clone(),
            username: config.graph.username.clone(),
            password: config.graph.password.clone(),
            keyspace: config.ferrosa.keyspace.clone(),
        },
    )
    .await
    {
        Ok(_graph) => tracing::info!("connected to Ferrosa graph (HTTP)"),
        Err(e) => tracing::warn!("graph connection failed ({e}), graph traversals disabled"),
    };

    // Start visualization server if enabled
    let shared_event_bus = Arc::new(ferrosa_memory_core::viz::EventBus::new());
    if config.viz.enabled {
        let viz_bus = Arc::clone(&shared_event_bus);
        let viz_port = config.viz.port;
        let viz_storage = Arc::clone(&storage);
        let viz_ctx = Arc::new(auth::authenticate_stdio(tenant_id));
        let viz_session_id = default_session_id.unwrap_or_else(uuid::Uuid::nil);
        tokio::spawn(async move {
            if let Err(e) =
                http::serve_viz(viz_port, viz_bus, viz_storage, viz_ctx, viz_session_id).await
            {
                tracing::warn!("viz server error: {e}");
            }
        });
        tracing::info!("viz dashboard at http://localhost:{}/viz", config.viz.port);
    }

    match config.server.transport.as_str() {
        "stdio" => {
            let ctx = Arc::new(auth::authenticate_stdio(tenant_id));
            tracing::info!(tenant_id = %tenant_id, "serving on stdio");

            let storage_ref = Arc::clone(&storage);
            let ctx_ref = Arc::clone(&ctx);
            let session = Arc::new(dispatch::SessionState {
                event_bus: Arc::clone(&shared_event_bus),
                default_session_id,
                ..dispatch::SessionState::default()
            });
            if let Some(sid) = default_session_id {
                tracing::info!(session_id = %sid, "using configured default session_id");
            }
            let session_ref = Arc::clone(&session);

            let handler: transport::Handler = Box::new(move |method: &str, params| {
                let storage = Arc::clone(&storage_ref);
                let ctx = Arc::clone(&ctx_ref);
                let session = Arc::clone(&session_ref);
                let method = method.to_string();
                Box::pin(async move {
                    dispatch::dispatch(&method, params, storage.as_ref(), ctx.as_ref(), &session)
                        .await
                })
            });

            // Spawn idle consolidation background task.
            if config.server.idle_consolidation_enabled {
                let idle_cfg = IdleConsolidationConfig {
                    idle_seconds: config.server.idle_consolidation_seconds,
                    stale_edge_max_days: config.server.stale_edge_max_days,
                    edge_decay_factor: config.server.edge_decay_factor,
                };
                let idle_session = Arc::clone(&session);
                let idle_storage = Arc::clone(&storage);
                let idle_ctx = Arc::clone(&ctx);
                tokio::spawn(idle_consolidation_loop(
                    idle_session,
                    idle_storage,
                    idle_ctx,
                    idle_cfg,
                ));
                tracing::info!(
                    idle_seconds = config.server.idle_consolidation_seconds,
                    decay_factor = config.server.edge_decay_factor,
                    prune_days = config.server.stale_edge_max_days,
                    "idle consolidation enabled"
                );
            }

            transport::serve_stdio(handler).await?;
        }
        "http" => {
            tracing::info!("serving on HTTP port {}", config.server.http_port);

            let validator: Arc<http::CredentialValidator> =
                Arc::new(move |_user: &str, _pass: &str| Some(tenant_id));

            http::serve_http(
                http::HttpConfig {
                    port: config.server.http_port,
                    require_tls: config.server.require_tls,
                    cert_path: config.server.cert_path.clone(),
                    key_path: config.server.key_path.clone(),
                },
                storage,
                metrics,
                validator,
            )
            .await?;
        }
        other => {
            anyhow::bail!("unsupported transport: {other}");
        }
    }

    Ok(())
}
