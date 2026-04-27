//! `migrate` — one-shot binary that applies the keyspace + DDL bundle
//! against a Ferrosa contact point.
//!
//! Used by CI to seed the schema before integration tests run, and by
//! operators bootstrapping a fresh cluster outside the dev mcp binary.
//!
//! Usage:
//!   FERROSA_CQL_CONTACT_POINTS="$HOST:$PORT" cargo run --bin migrate
//!   cargo run --bin migrate -- --contact-points "$HOST:$PORT" --keyspace agent_memory --ddl-dir ddl
//!
//! `$HOST:$PORT` is operator-supplied (per p0-11 W-01: no hardcoded
//! loopback addresses in production code). CI sets the env var explicitly.
//!
//! Exit codes: 0 = success, 1 = any failure (logged via tracing).
//!
//! # Why not `run_migrations()`?
//!
//! `ferrosa_memory_core::migration::run_migrations()` triggers the scylla
//! driver's automatic schema-agreement metadata fetch, which currently
//! errors against ferrosa because `system_schema.types.field_names` is
//! declared as `Text` instead of `Set<Text>` (a server-side bug in
//! ferrosa's compatibility shim). This binary bypasses that path by
//! issuing raw DDL queries directly.

#![allow(deprecated)]
use anyhow::Context;
use scylla::{LegacySession, SessionBuilder};
use std::path::PathBuf;
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
        ddl_dir = %cli.ddl_dir.display(),
        "starting fmem migrate"
    );

    let session = connect_with_retry(&cli.contact_points).await?;

    // Read DDL files in lexicographic order (the prefix `001_keyspace.cql`
    // sequence already encodes the apply order).
    let mut entries: Vec<_> = std::fs::read_dir(&cli.ddl_dir)
        .with_context(|| format!("listing ddl dir {}", cli.ddl_dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("cql"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let stmts = split_cql_statements(&body);
        tracing::info!(file = %path.display(), stmts = stmts.len(), "applying DDL");
        for stmt in stmts {
            apply_with_retry(&session, &stmt)
                .await
                .with_context(|| format!("apply {}: {}", path.display(), preview(&stmt)))?;
        }
    }

    tracing::info!("migrations completed");
    Ok(())
}

#[allow(deprecated)]
async fn connect_with_retry(contact_points: &[String]) -> anyhow::Result<Arc<LegacySession>> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=30u32 {
        let mut builder = SessionBuilder::new();
        for cp in contact_points {
            builder = builder.known_node(cp.as_str());
        }
        match builder.build_legacy().await {
            Ok(s) => {
                tracing::info!(attempt, "connected to ferrosa cluster");
                return Ok(Arc::new(s));
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "connect failed; retrying in 2s");
                last_err = Some(anyhow::anyhow!("{e}"));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("session builder failed without error")))
}

#[allow(deprecated)]
async fn apply_with_retry(session: &LegacySession, stmt: &str) -> anyhow::Result<()> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=10u32 {
        match session.query_unpaged(stmt, ()).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                let retryable = msg.contains("schema may still be propagating")
                    || msg.contains("not found")
                    || msg.contains("Server is overloaded");
                if !retryable || attempt == 10 {
                    return Err(anyhow::anyhow!("{e}"));
                }
                tracing::warn!(attempt, error = %e, "DDL retry in 2s");
                last_err = Some(anyhow::anyhow!("{e}"));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("DDL retry exhausted")))
}

/// Split a multi-statement `.cql` file into individual statements. CQL
/// uses `;` as a terminator. Lines starting with `--` are comments and
/// stripped. Blank statements are dropped.
fn split_cql_statements(body: &str) -> Vec<String> {
    let mut cleaned = String::with_capacity(body.len());
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") {
            continue;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    cleaned
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn preview(stmt: &str) -> String {
    let s = stmt
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join(" ");
    if s.len() > 80 {
        format!("{}…", &s[..80])
    } else {
        s
    }
}

#[derive(Debug)]
struct Args {
    contact_points: Vec<String>,
    keyspace: String,
    ddl_dir: PathBuf,
}

fn parse_args() -> Args {
    let mut iter = std::env::args().skip(1);
    let mut contact_points: Vec<String> = vec![];
    let mut keyspace: Option<String> = None;
    let mut ddl_dir: Option<PathBuf> = None;
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
            "--ddl-dir" => ddl_dir = iter.next().map(PathBuf::from),
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
        ddl_dir: ddl_dir
            .or_else(|| std::env::var("FERROSA_DDL_DIR").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("ddl")),
    }
}
