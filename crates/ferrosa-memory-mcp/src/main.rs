//! # ferrosa-memory-mcp
//!
//! MCP server binary that exposes Ferrosa's memory tools via stdio or HTTP+SSE.
//!
//! Connects to a real Ferrosa cluster via CQL (cdrs-tokio) and Bolt (neo4rs)
//! when a config file is available. Falls back to in-memory mock storage
//! if CQL connection fails.

use std::sync::Arc;

use ferrosa_core::auth;
use ferrosa_core::cql_storage::CqlStorage;
use ferrosa_core::dispatch;
use ferrosa_core::http;
use ferrosa_core::storage::Storage;
use ferrosa_core::storage::mock::MockStorage;
use ferrosa_core::transport;
use ferrosa_core::types::*;
use tracing_subscriber::EnvFilter;

/// Enum dispatch wrapper — allows switching between real CQL and mock storage
/// without requiring dyn-compatible traits.
enum StorageBackend {
    Cql(Box<CqlStorage>),
    Mock(Box<MockStorage>),
}

/// Delegate all Storage methods to the inner variant.
impl Storage for StorageBackend {
    async fn memo_get(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<Option<MemoEntry>> {
        match self {
            Self::Cql(s) => s.memo_get(ctx, content_hash, model_version).await,
            Self::Mock(s) => s.memo_get(ctx, content_hash, model_version).await,
        }
    }

    async fn memo_touch(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.memo_touch(ctx, content_hash, model_version).await,
            Self::Mock(s) => s.memo_touch(ctx, content_hash, model_version).await,
        }
    }

    async fn memo_put(&self, ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.memo_put(ctx, entry).await,
            Self::Mock(s) => s.memo_put(ctx, entry).await,
        }
    }

    async fn plan_put(&self, ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.plan_put(ctx, node).await,
            Self::Mock(s) => s.plan_put(ctx, node).await,
        }
    }

    async fn plan_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        max_depth: Option<i32>,
    ) -> anyhow::Result<Vec<PlanNode>> {
        match self {
            Self::Cql(s) => s.plan_get(ctx, session_id, max_depth).await,
            Self::Mock(s) => s.plan_get(ctx, session_id, max_depth).await,
        }
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
        match self {
            Self::Cql(s) => {
                s.plan_update_status(ctx, session_id, depth, subtask_id, status, outcome_summary)
                    .await
            }
            Self::Mock(s) => {
                s.plan_update_status(ctx, session_id, depth, subtask_id, status, outcome_summary)
                    .await
            }
        }
    }

    async fn fold_put(&self, ctx: &TenantContext, entry: &FoldEntry) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.fold_put(ctx, entry).await,
            Self::Mock(s) => s.fold_put(ctx, entry).await,
        }
    }

    async fn fold_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        fold_id: uuid::Uuid,
    ) -> anyhow::Result<Option<FoldEntry>> {
        match self {
            Self::Cql(s) => s.fold_get(ctx, session_id, fold_id).await,
            Self::Mock(s) => s.fold_get(ctx, session_id, fold_id).await,
        }
    }

    async fn fold_append(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        fold_id: uuid::Uuid,
        text: &str,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.fold_append(ctx, session_id, fold_id, text).await,
            Self::Mock(s) => s.fold_append(ctx, session_id, fold_id, text).await,
        }
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
        match self {
            Self::Cql(s) => {
                s.fold_complete(
                    ctx,
                    session_id,
                    fold_id,
                    summary,
                    embedding,
                    compression_ratio,
                )
                .await
            }
            Self::Mock(s) => {
                s.fold_complete(
                    ctx,
                    session_id,
                    fold_id,
                    summary,
                    embedding,
                    compression_ratio,
                )
                .await
            }
        }
    }

    async fn fold_search(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query_embedding: &[f32],
        k: usize,
        include_raw: bool,
    ) -> anyhow::Result<Vec<FoldSummary>> {
        match self {
            Self::Cql(s) => {
                s.fold_search(ctx, session_id, query_embedding, k, include_raw)
                    .await
            }
            Self::Mock(s) => {
                s.fold_search(ctx, session_id, query_embedding, k, include_raw)
                    .await
            }
        }
    }

    async fn entity_put(&self, ctx: &TenantContext, entry: &EntityEntry) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.entity_put(ctx, entry).await,
            Self::Mock(s) => s.entity_put(ctx, entry).await,
        }
    }

    async fn entity_find_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        name: &str,
    ) -> anyhow::Result<Option<EntityEntry>> {
        match self {
            Self::Cql(s) => s.entity_find_phonetic(ctx, session_id, name).await,
            Self::Mock(s) => s.entity_find_phonetic(ctx, session_id, name).await,
        }
    }

    async fn entity_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        match self {
            Self::Cql(s) => {
                s.entity_search_ann(ctx, session_id, query_embedding, k)
                    .await
            }
            Self::Mock(s) => {
                s.entity_search_ann(ctx, session_id, query_embedding, k)
                    .await
            }
        }
    }

    async fn entity_count(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        match self {
            Self::Cql(s) => s.entity_count(ctx, session_id).await,
            Self::Mock(s) => s.entity_count(ctx, session_id).await,
        }
    }

    async fn fold_count(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        match self {
            Self::Cql(s) => s.fold_count(ctx, session_id).await,
            Self::Mock(s) => s.fold_count(ctx, session_id).await,
        }
    }

    async fn memo_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        match self {
            Self::Cql(s) => s.memo_count(ctx).await,
            Self::Mock(s) => s.memo_count(ctx).await,
        }
    }

    async fn entity_update_state(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
        state: MemoryState,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.entity_update_state(ctx, entity_id, state).await,
            Self::Mock(s) => s.entity_update_state(ctx, entity_id, state).await,
        }
    }

    async fn entity_list_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        match self {
            Self::Cql(s) => s.entity_list_session(ctx, session_id).await,
            Self::Mock(s) => s.entity_list_session(ctx, session_id).await,
        }
    }

    async fn temporal_put(&self, ctx: &TenantContext, event: &TemporalEvent) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.temporal_put(ctx, event).await,
            Self::Mock(s) => s.temporal_put(ctx, event).await,
        }
    }

    async fn temporal_get_current(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<Option<TemporalEvent>> {
        match self {
            Self::Cql(s) => s.temporal_get_current(ctx, entity_id).await,
            Self::Mock(s) => s.temporal_get_current(ctx, entity_id).await,
        }
    }

    async fn temporal_invalidate(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
        event_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.temporal_invalidate(ctx, entity_id, event_id).await,
            Self::Mock(s) => s.temporal_invalidate(ctx, entity_id, event_id).await,
        }
    }

    async fn feedback_put(
        &self,
        ctx: &TenantContext,
        outcome: &FeedbackOutcome,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.feedback_put(ctx, outcome).await,
            Self::Mock(s) => s.feedback_put(ctx, outcome).await,
        }
    }

    async fn delete_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        match self {
            Self::Cql(s) => s.delete_session(ctx, session_id).await,
            Self::Mock(s) => s.delete_session(ctx, session_id).await,
        }
    }

    async fn edge_folded_into(
        &self,
        ctx: &TenantContext,
        source: uuid::Uuid,
        target: uuid::Uuid,
        session: uuid::Uuid,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.edge_folded_into(ctx, source, target, session).await,
            Self::Mock(s) => s.edge_folded_into(ctx, source, target, session).await,
        }
    }

    async fn edge_mentioned_in(
        &self,
        ctx: &TenantContext,
        entity: uuid::Uuid,
        fold: uuid::Uuid,
        session: uuid::Uuid,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.edge_mentioned_in(ctx, entity, fold, session).await,
            Self::Mock(s) => s.edge_mentioned_in(ctx, entity, fold, session).await,
        }
    }

    async fn edge_co_occurs(
        &self,
        ctx: &TenantContext,
        a: uuid::Uuid,
        b: uuid::Uuid,
        session: uuid::Uuid,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.edge_co_occurs(ctx, a, b, session).await,
            Self::Mock(s) => s.edge_co_occurs(ctx, a, b, session).await,
        }
    }

    async fn edge_supersedes(
        &self,
        ctx: &TenantContext,
        new_id: uuid::Uuid,
        old_id: uuid::Uuid,
        entity: uuid::Uuid,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.edge_supersedes(ctx, new_id, old_id, entity).await,
            Self::Mock(s) => s.edge_supersedes(ctx, new_id, old_id, entity).await,
        }
    }

    async fn intention_put(
        &self,
        ctx: &TenantContext,
        intention: &ferrosa_core::intention::Intention,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => s.intention_put(ctx, intention).await,
            Self::Mock(s) => s.intention_put(ctx, intention).await,
        }
    }

    async fn intention_list(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<ferrosa_core::intention::Intention>> {
        match self {
            Self::Cql(s) => s.intention_list(ctx).await,
            Self::Mock(s) => s.intention_list(ctx).await,
        }
    }

    async fn intention_update_status(
        &self,
        ctx: &TenantContext,
        id: uuid::Uuid,
        status: &str,
        triggered_at: Option<chrono::DateTime<chrono::Utc>>,
        completed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Cql(s) => {
                s.intention_update_status(ctx, id, status, triggered_at, completed_at)
                    .await
            }
            Self::Mock(s) => {
                s.intention_update_status(ctx, id, status, triggered_at, completed_at)
                    .await
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let debug = std::env::args().any(|a| a == "--debug");

    let default_filter = if debug {
        "debug,cdrs_tokio=debug,hyper=info,reqwest=info"
    } else {
        "ferrosa_core=warn,ferrosa_memory_mcp=warn"
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

    let config = match ferrosa_core::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config not found ({e}), using defaults");
            ferrosa_core::config::parse_config(
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
    let metrics = Arc::new(ferrosa_core::metrics::MemoryMetrics::new()?);
    tracing::info!("metrics registered");

    // Connect to real Ferrosa, fall back to mock
    let storage: Arc<StorageBackend> = match CqlStorage::connect(&config.ferrosa).await {
        Ok(cql) => {
            tracing::info!("connected to Ferrosa CQL cluster");
            Arc::new(StorageBackend::Cql(Box::new(cql)))
        }
        Err(e) => {
            tracing::warn!("CQL connection failed ({e}), using in-memory mock storage");
            Arc::new(StorageBackend::Mock(Box::new(MockStorage::new())))
        }
    };

    // Connect graph client via HTTP (non-fatal if it fails)
    match ferrosa_core::graph::GraphClient::connect(&ferrosa_core::graph::GraphConfig {
        http_url: config.graph.http_url.clone(),
        username: config.graph.username.clone(),
        password: config.graph.password.clone(),
        keyspace: config.ferrosa.keyspace.clone(),
    })
    .await
    {
        Ok(_graph) => tracing::info!("connected to Ferrosa graph (HTTP)"),
        Err(e) => tracing::warn!("graph connection failed ({e}), graph traversals disabled"),
    };

    // Start visualization server if enabled
    let shared_event_bus = Arc::new(ferrosa_core::viz::EventBus::new());
    if config.viz.enabled {
        let viz_bus = Arc::clone(&shared_event_bus);
        let viz_port = config.viz.port;
        tokio::spawn(async move {
            if let Err(e) = http::serve_viz(viz_port, viz_bus).await {
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
                ..dispatch::SessionState::default()
            });
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
