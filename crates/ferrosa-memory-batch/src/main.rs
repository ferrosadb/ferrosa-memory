//! # ferrosa-memory-batch
//!
//! Nightly batch job for routing guideline refinement.
//! Reads failure pairs from `feedback_outcomes`, computes strategy accuracy,
//! and writes updated routing guidelines.
//!
//! ## Usage
//!
//! ```sh
//! # Run once (triggered by cron or systemd timer)
//! ferrosa-memory-batch
//! ```

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("ferrosa-memory-batch starting");

    let _config = ferrosa_core::config::load_config()?;

    // TODO: connect to CQL, read feedback_outcomes, compute guidelines
    tracing::info!("ferrosa-memory-batch complete");

    Ok(())
}
