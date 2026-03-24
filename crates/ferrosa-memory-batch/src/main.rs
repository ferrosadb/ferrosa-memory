//! # ferrosa-memory-batch
//!
//! Nightly batch job for routing guideline refinement (ADR-002).
//!
//! Reads failure pairs from `feedback_outcomes`, computes strategy accuracy
//! per task complexity, and writes updated routing guidelines to the
//! `routing_guidelines` config table.
//!
//! ## Usage
//!
//! ```sh
//! # Run once (triggered by cron or systemd timer)
//! ferrosa-memory-batch
//!
//! # With config
//! FERROSA_MEMORY_CONFIG=./ferrosa-memory.toml ferrosa-memory-batch
//! ```
//!
//! ## Output
//!
//! Logs strategy accuracy statistics and writes a new guideline version
//! to CQL. The MCP server reads the latest version on each request.

use ferrosa_core::batch;
use ferrosa_core::cql_storage::CqlStorage;
use ferrosa_core::storage::Storage;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("ferrosa-memory-batch starting");

    let config = match ferrosa_core::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config not found ({e}), using defaults");
            ferrosa_core::config::parse_config(
                "[ferrosa]\ncontact_points = [\"localhost:9042\"]\n",
            )?
        }
    };

    tracing::info!(
        guideline_version = %config.routing.guideline_version,
        "current guideline version"
    );

    // 1. Connect to CQL
    let storage = CqlStorage::connect(&config.ferrosa).await?;
    tracing::info!("connected to CQL cluster");

    // 2. Query all feedback outcomes
    let outcomes = storage.feedback_list_all().await?;

    if outcomes.is_empty() {
        tracing::info!("no feedback outcomes found — nothing to compute");
        tracing::info!("ferrosa-memory-batch complete");
        return Ok(());
    }

    tracing::info!(count = outcomes.len(), "loaded feedback outcomes");

    // 3. Compute strategy accuracy per (program_type, task_complexity)
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

    // 4. Generate updated routing guidelines
    let next_version = next_guideline_version(&config.routing.guideline_version);
    let guidelines = batch::generate_guidelines(&stats, &next_version);

    tracing::info!(
        version = %next_version,
        strategies = stats.len(),
        "generated routing guidelines"
    );

    // 5. Write routing guidelines to CQL
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
/// Falls back to "v1" if the format is unrecognized.
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
