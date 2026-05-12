//! Migration: inspect and fix session_id values on edge tables.
//!
//! co_occurs_with edges have session_id values that don't match the expected
//! session UUID, causing edge_list_session() to return empty while
//! edge_list_all() (tenant-only filter) finds them. This breaks Datalog
//! fact loading and query_derived.
//!
//! Usage:
//!   cargo run --bin fix_edge_sessions -- --dry-run    # inspect only
//!   cargo run --bin fix_edge_sessions                 # fix data

use ferrosa_memory_core::config::load_config;
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::cql_storage::{build_col_map, cql_get};
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let dry_run = std::env::args().any(|a| a == "--dry-run");

    let config = load_config()?;
    let tenant_id: uuid::Uuid = config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .expect("tenant_id must be set in config");

    let target_session: uuid::Uuid = config
        .server
        .session_id
        .as_ref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .expect("session_id must be set in config");

    tracing::info!(%tenant_id, %target_session, dry_run, "connecting to Ferrosa");

    let storage = CqlStorage::connect(&config.ferrosa).await?;
    let session = storage.session();

    // --- Scan non-partition-key session_id tables (can UPDATE in place) ---
    let updatable_tables: &[(&str, &str, &str)] = &[
        ("co_occurs_with", "entity_a", "entity_b"),
        ("mentioned_in", "entity_id", "fold_id"),
        ("folded_into", "source_fold_id", "target_fold_id"),
    ];

    for (table, src_col, dst_col) in updatable_tables {
        tracing::info!("--- Scanning {table} ---");

        let query =
            format!("SELECT {src_col}, {dst_col}, session_id, tenant_id FROM agent_memory.{table}");
        #[allow(deprecated)]
        let mut iter = session.query_iter(query, ()).await?;
        let col_map = build_col_map(iter.get_column_specs());

        let mut distribution: std::collections::HashMap<(uuid::Uuid, uuid::Uuid), usize> =
            std::collections::HashMap::new();
        let mut edges_to_fix: Vec<(uuid::Uuid, uuid::Uuid)> = Vec::new();
        let mut total_rows = 0usize;

        while let Some(row_result) = iter.next().await {
            let row = row_result?;
            total_rows += 1;
            let src: uuid::Uuid = cql_get(&row, &col_map, src_col).unwrap_or_default();
            let dst: uuid::Uuid = cql_get(&row, &col_map, dst_col).unwrap_or_default();
            let tid: uuid::Uuid =
                cql_get::<uuid::Uuid>(&row, &col_map, "tenant_id").unwrap_or_default();
            let sid: uuid::Uuid =
                cql_get::<uuid::Uuid>(&row, &col_map, "session_id").unwrap_or_default();

            *distribution.entry((tid, sid)).or_default() += 1;

            if tid == tenant_id && sid != target_session {
                edges_to_fix.push((src, dst));
            }
        }

        tracing::info!("{table}: {total_rows} total rows");
        for ((tid, sid), count) in &distribution {
            let marker = if *tid == tenant_id && *sid == target_session {
                " OK"
            } else if *tid == tenant_id {
                " <- needs fix"
            } else {
                ""
            };
            tracing::info!("  tenant={tid} session={sid} count={count}{marker}");
        }

        if edges_to_fix.is_empty() {
            tracing::info!("{table}: all edges OK");
            continue;
        }

        tracing::info!(
            "{table}: {} edges need session_id -> {target_session}",
            edges_to_fix.len()
        );

        if dry_run {
            continue;
        }

        // session_id is NOT in the primary key for these tables, so UPDATE works
        let update_query = format!(
            "UPDATE agent_memory.{table} SET session_id = ? \
             WHERE {src_col} = ? AND {dst_col} = ?"
        );

        let mut fixed = 0;
        for (src, dst) in &edges_to_fix {
            #[allow(deprecated)]
            match session
                .query_unpaged(update_query.clone(), (target_session, *src, *dst))
                .await
            {
                Ok(_) => fixed += 1,
                Err(e) => tracing::warn!(%src, %dst, error = %e, "update failed"),
            }
        }
        tracing::info!("{table}: fixed {fixed}/{}", edges_to_fix.len());
    }

    // --- typed_edges: partition key includes (tenant_id, session_id), need DELETE+INSERT ---
    tracing::info!("--- Scanning typed_edges ---");
    let query = "SELECT src_id, edge_type, dst_id, session_id, tenant_id, weight, metadata, created_at \
                 FROM agent_memory.typed_edges";
    #[allow(deprecated)]
    let mut iter = session.query_iter(query, ()).await?;
    let col_map = build_col_map(iter.get_column_specs());

    let mut te_distribution: std::collections::HashMap<(uuid::Uuid, uuid::Uuid), usize> =
        std::collections::HashMap::new();

    struct TypedEdgeRow {
        src: uuid::Uuid,
        etype: String,
        dst: uuid::Uuid,
        tid: uuid::Uuid,
        sid: uuid::Uuid,
        weight: f64,
        metadata: String,
        created_at: chrono::DateTime<chrono::Utc>,
    }

    let mut te_rows: Vec<TypedEdgeRow> = Vec::new();
    let mut te_total_rows = 0usize;
    while let Some(row_result) = iter.next().await {
        let row = row_result?;
        te_total_rows += 1;
        let tid: uuid::Uuid =
            cql_get::<uuid::Uuid>(&row, &col_map, "tenant_id").unwrap_or_default();
        let sid: uuid::Uuid =
            cql_get::<uuid::Uuid>(&row, &col_map, "session_id").unwrap_or_default();

        *te_distribution.entry((tid, sid)).or_default() += 1;

        if tid == tenant_id && sid != target_session {
            te_rows.push(TypedEdgeRow {
                src: cql_get::<uuid::Uuid>(&row, &col_map, "src_id").unwrap_or_default(),
                etype: cql_get::<String>(&row, &col_map, "edge_type").unwrap_or_default(),
                dst: cql_get::<uuid::Uuid>(&row, &col_map, "dst_id").unwrap_or_default(),
                tid,
                sid,
                weight: cql_get::<f64>(&row, &col_map, "weight").unwrap_or(1.0),
                metadata: cql_get::<String>(&row, &col_map, "metadata").unwrap_or_default(),
                created_at: cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                    .unwrap_or_default(),
            });
        }
    }

    tracing::info!("typed_edges: {te_total_rows} total rows");
    for ((tid, sid), count) in &te_distribution {
        let marker = if *tid == tenant_id && *sid == target_session {
            " OK"
        } else if *tid == tenant_id {
            " <- needs fix"
        } else {
            ""
        };
        tracing::info!("  tenant={tid} session={sid} count={count}{marker}");
    }

    if te_rows.is_empty() {
        tracing::info!("typed_edges: all edges OK");
    } else if dry_run {
        tracing::info!(
            "typed_edges: {} edges need fix (skipped, dry-run)",
            te_rows.len()
        );
    } else {
        tracing::info!(
            "typed_edges: migrating {} edges (DELETE+INSERT)",
            te_rows.len()
        );
        let mut fixed = 0;
        for edge in &te_rows {
            // Delete old row
            let del = "DELETE FROM agent_memory.typed_edges \
                       WHERE tenant_id = ? AND session_id = ? AND src_id = ? AND edge_type = ? AND dst_id = ?";
            #[allow(deprecated)]
            let _ = session
                .query_unpaged(
                    del,
                    (edge.tid, edge.sid, edge.src, edge.etype.clone(), edge.dst),
                )
                .await;

            // Insert with correct session_id
            let ins = "INSERT INTO agent_memory.typed_edges \
                       (tenant_id, session_id, src_id, edge_type, dst_id, weight, metadata, created_at) \
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
            #[allow(deprecated)]
            match session
                .query_unpaged(
                    ins,
                    (
                        edge.tid,
                        target_session,
                        edge.src,
                        edge.etype.clone(),
                        edge.dst,
                        edge.weight,
                        edge.metadata.clone(),
                        edge.created_at,
                    ),
                )
                .await
            {
                Ok(_) => fixed += 1,
                Err(e) => tracing::warn!(src=%edge.src, dst=%edge.dst, error=%e, "insert failed"),
            }
        }
        tracing::info!("typed_edges: fixed {fixed}/{}", te_rows.len());
    }

    // --- Create missing session_id indexes ---
    tracing::info!("--- Creating session_id indexes ---");
    let index_stmts = [
        "CREATE INDEX IF NOT EXISTS idx_co_occurs_by_session ON agent_memory.co_occurs_with (session_id)",
        "CREATE INDEX IF NOT EXISTS idx_mentioned_in_by_session ON agent_memory.mentioned_in (session_id)",
        "CREATE INDEX IF NOT EXISTS idx_folded_into_by_session ON agent_memory.folded_into (session_id)",
    ];
    for stmt in &index_stmts {
        #[allow(deprecated)]
        match session.query_unpaged(stmt.to_string(), ()).await {
            Ok(_) => tracing::info!("  OK: {stmt}"),
            Err(e) => tracing::warn!("  FAILED: {stmt} — {e}"),
        }
    }

    // --- Verify edge_list_session works ---
    tracing::info!("--- Verifying edge_list_session ---");
    let ctx = ferrosa_memory_core::types::TenantContext {
        tenant_id,
        session_origin: "migration".into(),
    };
    use ferrosa_memory_core::storage::Storage;
    let session_edges = storage.edge_list_session(&ctx, target_session).await?;
    tracing::info!(
        "edge_list_session(tenant={tenant_id}, session={target_session}): {} edges",
        session_edges.len()
    );
    if !session_edges.is_empty() {
        let sample = &session_edges[0];
        tracing::info!("  sample: {} -> {} ({})", sample.0, sample.1, sample.2);
    }

    let all_edges = storage.edge_list_all(&ctx).await?;
    tracing::info!(
        "edge_list_all(tenant={tenant_id}): {} edges",
        all_edges.len()
    );

    // Also test with swapped ctx like viz does
    let swapped_ctx = ferrosa_memory_core::types::TenantContext {
        tenant_id: target_session,
        session_origin: "migration".into(),
    };
    let swapped_edges = storage.edge_list_all(&swapped_ctx).await?;
    tracing::info!(
        "edge_list_all(tenant={target_session} [swapped]): {} edges",
        swapped_edges.len()
    );

    tracing::info!("done");
    Ok(())
}
