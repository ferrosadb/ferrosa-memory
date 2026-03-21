//! # ferrosa-memory-mcp
//!
//! MCP server binary that exposes Ferrosa's memory tools via stdio or HTTP+SSE.
//!
//! ## Usage
//!
//! ```sh
//! # stdio mode (default, for Claude Code)
//! ferrosa-memory-mcp
//!
//! # HTTP mode
//! FERROSA_MEMORY_CONFIG=./ferrosa-memory.toml ferrosa-memory-mcp
//! ```

use std::sync::Arc;

use ferrosa_core::auth;
use ferrosa_core::dispatch;
use ferrosa_core::http;
use ferrosa_core::storage::mock::MockStorage;
use ferrosa_core::transport;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("ferrosa-memory-mcp starting");

    // TODO: load config, connect real CQL. For now, use mock storage.
    let config = match ferrosa_core::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config not found ({e}), using defaults");
            ferrosa_core::config::parse_config(
                "[ferrosa]\ncontact_points = [\"localhost:9042\"]\n",
            )?
        }
    };

    let storage = Arc::new(MockStorage::new());
    let tenant_id = uuid::Uuid::new_v4(); // TODO: from config
    let metrics = Arc::new(ferrosa_core::metrics::MemoryMetrics::new()?);
    tracing::info!("metrics registered");

    match config.server.transport.as_str() {
        "stdio" => {
            let ctx = Arc::new(auth::authenticate_stdio(tenant_id));
            tracing::info!("serving on stdio");

            let storage_ref: Arc<MockStorage> = Arc::clone(&storage);
            let ctx_ref: Arc<ferrosa_core::types::TenantContext> = Arc::clone(&ctx);

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
                Arc::new(move |_user: &str, _pass: &str| {
                    // TODO: validate against CQL credentials
                    Some(tenant_id)
                });

            http::serve_http(
                http::HttpConfig {
                    port: config.server.http_port,
                    require_tls: false, // TODO: TLS support
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
