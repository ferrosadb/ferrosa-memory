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

    // TODO: connect to CQL with batch job credentials (separate from MCP server)
    // TODO: SELECT * FROM feedback_outcomes WHERE succeeded = false
    // TODO: Compute strategy accuracy per (program_type, task_complexity)
    // TODO: Generate updated routing guidelines in NL format
    // TODO: INSERT INTO routing_guidelines (version, rules, created_at)

    tracing::info!("ferrosa-memory-batch complete");
    Ok(())
}
