//! Maintenance: remove dangling typed edges whose source or destination entity
//! no longer exists in `entity_store`.
//!
//! Orphaned edges are the server-side root cause of the viz crash
//! ("Cannot create property 'vx' on string"): the snapshot streams an edge to
//! an id that has no node. The viz now defends against them
//! (assets/graph-sanitize.mjs); this binary removes them at the source.
//!
//! Usage:
//!   cargo run --bin reconcile-dangling-edges -- --dry-run   # report only
//!   cargo run --bin reconcile-dangling-edges                # delete dangling
//!
//! IMPORTANT — deletion limitation: deletes go through the graph engine, whose
//! Cypher must anchor on an endpoint `:Entity` node. An edge with BOTH endpoints
//! already deleted cannot be matched (the engine rejects relationship-only
//! patterns), so this binary cannot remove fully-orphaned historical rows — it
//! reports them. The durable fix is upstream: entity deletion now cleans an
//! entity's edges first (see delete_typed_edges_referencing_entity_tenant_wide),
//! preventing new orphans. Dry-run is the reliable use of this tool.
//!
//! Scope: the `typed_edges` table, which holds every edge_type streamed to the
//! viz (including CO_OCCURS_WITH and `references` rows). A separate
//! `co_occurs_with` backing table, if maintained, is not scanned here — but the
//! viz layer defends against any dangling row regardless of its source table.

use std::collections::HashSet;

use ferrosa_memory_core::config::load_config;
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use ferrosa_memory_core::reconcile::edge_is_dangling;
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::types::TenantContext;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();

    let dry_run = std::env::args().any(|a| a == "--dry-run");

    let config = load_config()?;
    // HTTP-transport deployments keep tenant_id out of [server] (that path is
    // rejected), so accept it from FERROSA_TENANT_ID and fall back to config.
    let tenant_id: uuid::Uuid = std::env::var("FERROSA_TENANT_ID")
        .ok()
        .or_else(|| config.server.tenant_id.clone())
        .and_then(|s| uuid::Uuid::parse_str(s.trim()).ok())
        .expect("set FERROSA_TENANT_ID (or server.tenant_id) to a valid UUID");
    let ctx = TenantContext {
        tenant_id,
        session_origin: "reconcile-dangling-edges".to_string(),
    };

    tracing::info!(%tenant_id, dry_run, "connecting to Ferrosa");
    let storage = CqlStorage::connect(&config.ferrosa).await?;
    // Deletes MUST route through the graph engine — direct CQL writes to
    // graph-owned tables are rejected by the storage layer. Build the same
    // GraphClient the server uses (only needed for the mutating path).
    let graph = if dry_run {
        None
    } else {
        Some(
            GraphClient::connect(&GraphConfig {
                http_url: config.graph.http_url.clone(),
                username: config.graph.username.clone(),
                password: config.graph.password.clone(),
                keyspace: config.ferrosa.keyspace.clone(),
            })
            .await?,
        )
    };

    // 1. Every existing entity id for the tenant.
    let entities = storage.entity_list_all(&ctx).await?;
    let existing: HashSet<uuid::Uuid> = entities.iter().map(|e| e.entity_id).collect();
    tracing::info!(entities = existing.len(), "loaded entity ids");

    // 2. Every typed edge; partition with the shared, tested predicate.
    let edges = storage.typed_edge_list_all(&ctx).await?;
    let dangling: Vec<_> = edges
        .iter()
        .filter(|e| edge_is_dangling(e.src_id, e.dst_id, &existing))
        .collect();
    tracing::info!(
        total = edges.len(),
        dangling = dangling.len(),
        "scanned typed edges"
    );

    for e in &dangling {
        tracing::info!(
            src_id = %e.src_id,
            dst_id = %e.dst_id,
            edge_type = %e.edge_type,
            missing_src = !existing.contains(&e.src_id),
            missing_dst = !existing.contains(&e.dst_id),
            "dangling typed edge"
        );
    }

    if dry_run {
        tracing::info!(would_delete = dangling.len(), "dry-run: no deletions");
        println!("DRY_RUN dangling_typed_edges={}", dangling.len());
        return Ok(());
    }

    let graph = graph.expect("graph client is constructed for non-dry-run");
    let mut deleted = 0usize;
    let mut failed = 0usize;
    for e in &dangling {
        match graph
            .delete_typed_edge(tenant_id, e.session_id, e.src_id, &e.edge_type, e.dst_id)
            .await
        {
            Ok(_) => deleted += 1,
            Err(err) => {
                failed += 1;
                tracing::warn!(src_id = %e.src_id, dst_id = %e.dst_id, error = %err, "failed to delete dangling edge");
            }
        }
    }
    tracing::info!(deleted, failed, "reconcile complete");
    println!("DELETED dangling_typed_edges={deleted} failed={failed}");
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
