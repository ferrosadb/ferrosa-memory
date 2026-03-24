//! # ferrosa-memory-batch
//!
//! Nightly batch job for routing guideline refinement (ADR-002).
//! Also provides data migration utilities.
//!
//! ## Usage
//!
//! ```sh
//! # Run guideline refinement (default)
//! ferrosa-memory-batch
//!
//! # Migrate all entities to the configured default session_id
//! ferrosa-memory-batch migrate-session
//!
//! # With config
//! FERROSA_MEMORY_CONFIG=./ferrosa-memory.toml ferrosa-memory-batch
//! ```

use ferrosa_core::batch;
use ferrosa_core::cql_storage::CqlStorage;
use ferrosa_core::storage::Storage;
use ferrosa_core::types::TenantContext;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = match ferrosa_core::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config not found ({e}), using defaults");
            ferrosa_core::config::parse_config(
                "[ferrosa]\ncontact_points = [\"localhost:9042\"]\n",
            )?
        }
    };

    let subcommand = std::env::args().nth(1).unwrap_or_default();

    match subcommand.as_str() {
        "migrate-session" => migrate_session(&config).await,
        _ => run_guidelines(&config).await,
    }
}

/// Migrate all entities to the configured default session_id.
///
/// Reads all entities for the tenant, re-inserts with the target session_id,
/// then deletes the old session partitions.
async fn migrate_session(config: &ferrosa_core::config::Config) -> anyhow::Result<()> {
    let target_sid = config
        .server
        .session_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("no session_id configured in [server]"))?;

    let tenant_id = config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("no tenant_id configured in [server]"))?;

    let ctx = TenantContext {
        tenant_id,
        session_origin: "batch-migrate".into(),
    };

    tracing::info!(
        tenant_id = %tenant_id,
        target_session_id = %target_sid,
        "starting session migration"
    );

    let storage = CqlStorage::connect(&config.ferrosa).await?;
    tracing::info!("connected to CQL cluster");

    // Read all entities for this tenant
    let entities = storage.entity_list_all(&ctx).await?;
    tracing::info!(count = entities.len(), "loaded entities");

    let mut migrated = 0;
    let mut skipped = 0;
    let mut old_sessions: std::collections::HashSet<Uuid> = std::collections::HashSet::new();

    for entity in &entities {
        if entity.session_id == target_sid {
            skipped += 1;
            continue;
        }

        old_sessions.insert(entity.session_id);

        // Re-insert with target session_id
        let mut migrated_entity = entity.clone();
        migrated_entity.session_id = target_sid;
        storage.entity_put(&ctx, &migrated_entity).await?;
        migrated += 1;
    }

    tracing::info!(
        migrated = migrated,
        skipped = skipped,
        old_sessions = old_sessions.len(),
        "entity migration complete"
    );

    // Delete old session partitions
    for old_sid in &old_sessions {
        match storage.delete_session(&ctx, *old_sid).await {
            Ok(n) => tracing::info!(session_id = %old_sid, deleted = n, "cleaned old session"),
            Err(e) => {
                tracing::warn!(session_id = %old_sid, error = %e, "failed to clean old session")
            }
        }
    }

    tracing::info!("migration complete");
    Ok(())
}

/// Run the nightly guideline refinement job.
async fn run_guidelines(config: &ferrosa_core::config::Config) -> anyhow::Result<()> {
    tracing::info!("ferrosa-memory-batch starting");

    tracing::info!(
        guideline_version = %config.routing.guideline_version,
        "current guideline version"
    );

    let storage = CqlStorage::connect(&config.ferrosa).await?;
    tracing::info!("connected to CQL cluster");

    let outcomes = storage.feedback_list_all().await?;

    if outcomes.is_empty() {
        tracing::info!("no feedback outcomes found — nothing to compute");
        tracing::info!("ferrosa-memory-batch complete");
        return Ok(());
    }

    tracing::info!(count = outcomes.len(), "loaded feedback outcomes");

    let stats = batch::compute_strategy_accuracy(&outcomes);

    for s in &stats {
        tracing::info!(
            program_type = %s.program_type,
            task_complexity = %s.task_complexity,
            accuracy = s.accuracy,
            total = s.total,
            succeeded = s.succeeded,
            avg_latency_ms = s.avg_latency_ms,
            "strategy stats"
        );
    }

    let next_version = next_guideline_version(&config.routing.guideline_version);
    let guidelines = batch::generate_guidelines(&stats, &next_version);

    tracing::info!(
        version = %next_version,
        strategies = stats.len(),
        "generated routing guidelines"
    );

    let ks = &config.ferrosa.keyspace;
    let query = format!(
        "INSERT INTO {ks}.routing_guidelines (version, rules, created_at) VALUES (?, ?, toTimestamp(now()))"
    );
    storage
        .session()
        .query_with_values(
            query.as_str(),
            cdrs_tokio::query_values!(next_version.clone(), guidelines.clone()),
        )
        .await?;

    tracing::info!(version = %next_version, "routing guidelines written to CQL");
    tracing::info!("ferrosa-memory-batch complete");
    Ok(())
}

/// Increment a version string like "v1" -> "v2", "v42" -> "v43".
fn next_guideline_version(current: &str) -> String {
    if let Some(num_str) = current.strip_prefix('v')
        && let Ok(n) = num_str.parse::<u64>()
    {
        return format!("v{}", n + 1);
    }
    "v1".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_increment() {
        assert_eq!(next_guideline_version("v1"), "v2");
        assert_eq!(next_guideline_version("v42"), "v43");
        assert_eq!(next_guideline_version("v0"), "v1");
    }

    #[test]
    fn version_fallback() {
        assert_eq!(next_guideline_version(""), "v1");
        assert_eq!(next_guideline_version("latest"), "v1");
        assert_eq!(next_guideline_version("abc"), "v1");
    }
}
