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
//! # With config file
//! FERROSA_MEMORY_CONFIG=./ferrosa-memory.toml ferrosa-memory-mcp
//! ```

use std::sync::Arc;

use ferrosa_core::auth;
use ferrosa_core::dispatch;
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
    let storage = Arc::new(MockStorage::new());
    let tenant_id = uuid::Uuid::new_v4(); // TODO: from config
    let ctx = Arc::new(auth::authenticate_stdio(tenant_id));

    let _metrics = ferrosa_core::metrics::MemoryMetrics::new()?;
    tracing::info!("metrics registered");

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

    Ok(())
}
