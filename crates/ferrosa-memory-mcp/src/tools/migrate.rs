//! `migrate` — one-shot binary that applies the keyspace + DDL bundle
//! against a Ferrosa contact point.
//!
//! Used by CI to seed the schema before integration tests run, and by
//! operators bootstrapping a fresh cluster outside the dev mcp binary.
//!
//! Usage:
//!   FERROSA_CQL_CONTACT_POINTS="$HOST:$PORT" cargo run --bin migrate
//!   cargo run --bin migrate -- --contact-points "$HOST:$PORT" --keyspace agent_memory
//!
//! `$HOST:$PORT` is operator-supplied (per p0-11 W-01: no hardcoded
//! loopback addresses in production code). CI sets the env var explicitly.
//!
//! Exit codes: 0 = success, 1 = any failure (logged via tracing).

use anyhow::Context;
use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::{CqlSession, connect_admin_session};
use ferrosa_memory_core::migration::run_migrations;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = parse_args();
    tracing::info!(
        contact_points = ?cli.contact_points,
        keyspace = %cli.keyspace,
        "starting fmem migrate"
    );

    let config = FerrosaCqlConfig {
        contact_points: cli.contact_points.clone(),
        keyspace: cli.keyspace.clone(),
        replication_factor: 1,
        consistency: "QUORUM".to_string(),
        username: cli.username.clone().unwrap_or_default(),
        password: cli.password.clone().unwrap_or_default(),
        admin_username: cli.username.clone(),
        admin_password: cli.password.clone(),
    };

    // Wait for the cluster's schema agreement to settle before applying
    // DDL — the workflow already waits for ports, but a freshly-formed
    // cluster may still be electing leaders.
    let session = connect_with_retry(&config).await?;

    let applied = run_migrations(&session, &cli.keyspace)
        .await
        .with_context(|| format!("run_migrations(keyspace={})", cli.keyspace))?;

    tracing::info!(applied, "migrations completed");
    Ok(())
}

async fn connect_with_retry(config: &FerrosaCqlConfig) -> anyhow::Result<Arc<CqlSession>> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=30u32 {
        match connect_admin_session(config).await {
            Ok(s) => {
                tracing::info!(attempt, "connected to ferrosa cluster");
                return Ok(s);
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "connect failed; retrying in 2s");
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("connect_admin_session failed without error")))
}

#[derive(Debug)]
struct Args {
    contact_points: Vec<String>,
    keyspace: String,
    username: Option<String>,
    password: Option<String>,
}

fn parse_args() -> Args {
    let mut iter = std::env::args().skip(1);
    let mut contact_points: Vec<String> = vec![];
    let mut keyspace: Option<String> = None;
    let mut username: Option<String> = None;
    let mut password: Option<String> = None;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--contact-points" => {
                contact_points = iter
                    .next()
                    .expect("--contact-points needs a comma-separated value")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--keyspace" => keyspace = iter.next(),
            "--username" => username = iter.next(),
            "--password" => password = iter.next(),
            other => panic!("unknown argument: {other}"),
        }
    }
    if contact_points.is_empty()
        && let Ok(v) = std::env::var("FERROSA_CQL_CONTACT_POINTS")
    {
        contact_points = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if contact_points.is_empty() {
        eprintln!(
            "ERROR: contact points unset. Pass --contact-points <host:port>[,…] or set \
             FERROSA_CQL_CONTACT_POINTS env var. (No fallback default per p0-11 W-01.)"
        );
        std::process::exit(2);
    }
    Args {
        contact_points,
        keyspace: keyspace
            .or_else(|| std::env::var("FERROSA_KEYSPACE").ok())
            .unwrap_or_else(|| "agent_memory".to_string()),
        username,
        password,
    }
}
