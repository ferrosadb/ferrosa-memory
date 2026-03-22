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

    let tenant_id = uuid::Uuid::new_v4();
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

    match config.server.transport.as_str() {
        "stdio" => {
            let ctx = Arc::new(auth::authenticate_stdio(tenant_id));
            tracing::info!(tenant_id = %tenant_id, "serving on stdio");

            let storage_ref = Arc::clone(&storage);
            let ctx_ref = Arc::clone(&ctx);

            let handler: transport::Handler = Box::new(move |method: &str, params| {
                let storage = Arc::clone(&storage_ref);
                let ctx = Arc::clone(&ctx_ref);
                let method = method.to_string();
                Box::pin(async move {
                    dispatch::dispatch(&method, params, storage.as_ref(), ctx.as_ref()).await
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
                    require_tls: false,
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
