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
use ferrosa_memory_core::config::{parse_config, resolve_config_path};
use ferrosa_memory_core::migration::{
    MIGRATIONS, PRE_VERSIONING_BASELINE, ensure_schema_version_table, prepare_bootstrap_statement,
    qualify_ddl, record_version,
};
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

    let session = connect_with_retry(&cli.contact_points, cli.credentials.as_ref()).await?;

    if cli.probe_only {
        probe_keyspace(&session, &cli.keyspace).await?;
        return Ok(());
    }

    // Read DDL files in lexicographic order (the prefix `001_keyspace.cql`
    // sequence already encodes the apply order).
    let mut entries: Vec<_> = std::fs::read_dir(&cli.ddl_dir)
        .with_context(|| format!("listing ddl dir {}", cli.ddl_dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("cql"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let applied_at = chrono::Utc::now();
    for entry in entries {
        let path = entry.path();
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let stmts = prepare_statements_for_keyspace(
            &body,
            &cli.keyspace,
            applied_at,
            cli.replication_factor,
        );
        tracing::info!(file = %path.display(), stmts = stmts.len(), "applying DDL");
        for prepared in stmts {
            apply_with_retry(&session, &prepared)
                .await
                .with_context(|| format!("apply {}: {}", path.display(), preview(&prepared)))?;
        }
    }

    // Populate schema_version so subsequent run_migrations() calls (in
    // tests or runtime startup) see this keyspace as up to date and don't
    // re-apply the same DDLs. Without this the library's migration runner
    // restarts from the baseline and trips on already-existing tables
    // (e.g. migration 30's `CREATE TABLE temporal_events`).
    //
    // We mirror the MIGRATIONS registry rather than parsing filenames so a
    // non-versioned DDL like `100_roles.cql` (auth setup, not a migration)
    // doesn't bump schema_version above what the code knows about and
    // trip the downgrade guard.
    ensure_schema_version_table(&session, &cli.keyspace)
        .await
        .context("ensure schema_version table after DDL apply")?;
    record_version(
        &session,
        &cli.keyspace,
        PRE_VERSIONING_BASELINE,
        "pre-versioning baseline (migrate binary adoption seed)",
    )
    .await
    .context("record pre-versioning baseline in schema_version")?;
    for m in MIGRATIONS {
        record_version(&session, &cli.keyspace, m.version, m.description)
            .await
            .with_context(|| format!("record schema_version v{}", m.version))?;
    }

    tracing::info!("migrations completed");
    Ok(())
}

#[allow(deprecated)]
async fn connect_with_retry(
    contact_points: &[String],
    credentials: Option<&(String, String)>,
) -> anyhow::Result<Arc<LegacySession>> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=30u32 {
        let mut builder = SessionBuilder::new()
            // Keep auto schema-agreement (good — guarantees the cluster has
            // converged before the next DDL fires), but skip the post-agreement
            // metadata refresh: that refresh reads `system_schema.views`, which
            // since ferrosa fce7a13 ("system_schema boolean columns") returns
            // the Cassandra-5.0 10-column shape and trips the scylla driver
            // fork's type-checker (it still expects the 3-column shape). We
            // don't need the refresh — migrate is a one-shot DDL applier and
            // never queries local metadata.
            .refresh_metadata_on_auto_schema_agreement(false);
        for cp in contact_points {
            builder = builder.known_node(cp.as_str());
        }
        if let Some((user, pass)) = credentials {
            builder = builder.user(user.as_str(), pass.as_str());
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

/// SELECT-only probe: confirm `keyspace` is visible in `system_schema.keyspaces`.
/// Avoids any DDL so the scylla driver's auto schema-agreement metadata fetch
/// is never triggered — that path currently fails against Ferrosa's
/// `system_schema.views` column shape (10 cols vs. driver's expected 3).
///
/// Reads the full keyspace list (a handful of rows) and matches client-side
/// rather than using a `?` bind marker; Ferrosa's CQL PREPARE for
/// `WHERE keyspace_name = ?` currently fails with "expected 1 bind-marker
/// column spec(s) but resolved only 0".
#[allow(deprecated)]
async fn probe_keyspace(session: &LegacySession, keyspace: &str) -> anyhow::Result<()> {
    let result = session
        .query_unpaged("SELECT keyspace_name FROM system_schema.keyspaces", ())
        .await
        .with_context(|| format!("probing system_schema.keyspaces for `{keyspace}`"))?;
    let rows = result.rows_or_empty();
    let visible = rows.iter().any(|row| {
        row.columns
            .first()
            .and_then(|col| col.as_ref())
            .and_then(|val| val.as_text())
            .is_some_and(|name| name == keyspace)
    });
    if !visible {
        anyhow::bail!("keyspace `{keyspace}` not visible in system_schema.keyspaces");
    }
    tracing::info!(keyspace, "keyspace visible");
    Ok(())
}

#[allow(deprecated)]
async fn apply_with_retry(session: &LegacySession, stmt: &str) -> anyhow::Result<()> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=10u32 {
        match session.query_unpaged(stmt, ()).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                // "Already exists" outcomes are benign for additive DDL: they
                // mean the previous run (or a sibling node via schema gossip)
                // already applied this statement. We log and treat as success
                // so re-running migrate against a partially-migrated cluster
                // does not refuse to make progress. This keeps every DDL file
                // safely re-runnable.
                if is_idempotent_already_exists(&msg) {
                    tracing::info!(error = %e, "DDL no-op (already applied), continuing");
                    return Ok(());
                }
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

/// Recognises ferrosa / scylla / cassandra error strings that mean
/// "this DDL has already been applied; the post-condition is satisfied."
/// Conservative: only matches additive-DDL outcomes (column / index / table
/// already exists). Does NOT match drift in column type or other shape changes.
fn is_idempotent_already_exists(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("already exists")
        || m.contains("conflicts with an existing column")
        || m.contains("duplicate column")
}

fn prepare_statements_for_keyspace(
    body: &str,
    keyspace: &str,
    applied_at: chrono::DateTime<chrono::Utc>,
    replication_factor: Option<u32>,
) -> Vec<String> {
    let qualified = qualify_ddl(body, keyspace);
    let qualified = match replication_factor {
        Some(rf) => override_replication_factor(&qualified, rf),
        None => qualified,
    };
    split_cql_statements(&qualified)
        .into_iter()
        .map(|stmt| prepare_bootstrap_statement(&stmt, applied_at))
        .collect()
}

/// Rewrite NetworkTopologyStrategy `'datacenter1': N` (the form the
/// bootstrap DDL hardcodes) to the operator-supplied `N`. Single-node
/// test clusters need RF=1 because LOCAL_QUORUM against RF=3 with one
/// node receives 0 acks and times out.
fn override_replication_factor(qualified: &str, rf: u32) -> String {
    let mut out = String::with_capacity(qualified.len());
    let pattern = "'datacenter1':";
    let mut i = 0;
    let bytes = qualified.as_bytes();
    while i < bytes.len() {
        if qualified[i..].starts_with(pattern) {
            out.push_str(pattern);
            i += pattern.len();
            // Skip whitespace.
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                out.push(bytes[i] as char);
                i += 1;
            }
            // Skip the existing integer digits.
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            out.push_str(&rf.to_string());
        } else {
            out.push(qualified[i..].chars().next().unwrap());
            i += qualified[i..].chars().next().unwrap().len_utf8();
        }
    }
    out
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
    credentials: Option<(String, String)>,
    /// SELECT-only probe mode: connect, check that the keyspace is visible in
    /// `system_schema.keyspaces`, exit. Does not issue any DDL. Used by the CI
    /// cluster-propagation barrier to avoid triggering the scylla driver's
    /// auto schema-agreement metadata fetch (which currently fails against
    /// Ferrosa's `system_schema.views` column shape; tracked upstream).
    probe_only: bool,
    /// Override the replication factor used in the keyspace `CREATE`
    /// statement. The bootstrap DDL hardcodes RF=3 because production
    /// runs against a 3-node cluster; CI's isolated test cluster
    /// (`docker-compose.test.yml`) is single-node and any quorum write
    /// blocks forever at "received=0 required=2" without this override.
    replication_factor: Option<u32>,
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env: Vec<(String, String)> = std::env::vars().collect();
    let config_toml = resolve_config_path().and_then(|path| std::fs::read_to_string(path).ok());
    parse_args_from(
        args.iter().map(String::as_str),
        env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        config_toml.as_deref(),
    )
}

fn parse_args_from<'a, A, E>(args: A, env: E, config_toml: Option<&str>) -> Args
where
    A: IntoIterator<Item = &'a str>,
    E: IntoIterator<Item = (&'a str, &'a str)>,
{
    let env: std::collections::HashMap<&str, &str> = env.into_iter().collect();
    let config = config_toml.map(|toml| {
        parse_config(toml).unwrap_or_else(|e| panic!("failed to parse migrate config: {e}"))
    });

    let mut iter = args.into_iter();
    let mut contact_points: Vec<String> = vec![];
    let mut keyspace: Option<String> = None;
    let mut ddl_dir: Option<PathBuf> = None;
    let mut user: Option<String> = None;
    let mut password: Option<String> = None;
    let mut config_path: Option<PathBuf> = None;
    let mut probe_only = false;
    let mut replication_factor: Option<u32> = None;
    while let Some(flag) = iter.next() {
        match flag {
            "--contact-points" => {
                contact_points = iter
                    .next()
                    .expect("--contact-points needs a comma-separated value")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--keyspace" => keyspace = iter.next().map(str::to_string),
            "--ddl-dir" => ddl_dir = iter.next().map(PathBuf::from),
            "--user" => user = iter.next().map(str::to_string),
            "--password" => password = iter.next().map(str::to_string),
            "--config" => config_path = iter.next().map(PathBuf::from),
            "--probe-only" => probe_only = true,
            "--replication-factor" => {
                replication_factor = iter
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .filter(|&n| n > 0);
                if replication_factor.is_none() {
                    panic!("--replication-factor needs a positive integer");
                }
            }
            other => panic!("unknown argument: {other}"),
        }
    }

    let config = if let Some(path) = config_path {
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read migrate config at {}: {e}", path.display()));
        Some(parse_config(&content).unwrap_or_else(|e| {
            panic!("failed to parse migrate config at {}: {e}", path.display())
        }))
    } else {
        config
    };

    let env_value = |name: &str| env.get(name).map(|v| (*v).to_string());

    if contact_points.is_empty()
        && let Some(v) = env_value("FERROSA_CQL_CONTACT_POINTS")
    {
        contact_points = parse_contact_points(&v);
    }
    if contact_points.is_empty()
        && let Some(cfg) = config.as_ref()
    {
        contact_points = cfg.ferrosa.contact_points.clone();
    }
    if contact_points.is_empty() {
        eprintln!(
            "ERROR: contact points unset. Pass --contact-points <host:port>[,…], set \
             FERROSA_CQL_CONTACT_POINTS, or provide [ferrosa].contact_points in ferrosa-memory.toml."
        );
        std::process::exit(2);
    }

    let env_user = env_value("FERROSA_CQL_USER");
    let env_password = env_value("FERROSA_CQL_PASSWORD");
    let credentials =
        credentials_from_sources(user, password, env_user, env_password, config.as_ref())
            .or_else(|| local_loopback_migration_credentials(&contact_points));

    Args {
        contact_points,
        keyspace: keyspace
            .or_else(|| env_value("FERROSA_KEYSPACE"))
            .or_else(|| config.as_ref().map(|cfg| cfg.ferrosa.keyspace.clone()))
            .unwrap_or_else(|| "agent_memory".to_string()),
        ddl_dir: ddl_dir
            .or_else(|| env_value("FERROSA_DDL_DIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("ddl")),
        credentials,
        probe_only,
        replication_factor: replication_factor.or_else(|| {
            env_value("FERROSA_MIGRATE_REPLICATION_FACTOR")
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|&n| n > 0)
        }),
    }
}

fn parse_contact_points(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn credentials_from_sources(
    cli_user: Option<String>,
    cli_password: Option<String>,
    env_user: Option<String>,
    env_password: Option<String>,
    config: Option<&ferrosa_memory_core::config::Config>,
) -> Option<(String, String)> {
    match (cli_user, cli_password) {
        (Some(u), Some(p)) => return Some((u, p)),
        (None, None) => {}
        _ => exit_incomplete_credentials("--user", "--password"),
    }

    match (env_user, env_password) {
        (Some(u), Some(p)) => return Some((u, p)),
        (None, None) => {}
        _ => exit_incomplete_credentials("FERROSA_CQL_USER", "FERROSA_CQL_PASSWORD"),
    }

    config.map(
        |cfg| match (&cfg.ferrosa.admin_username, &cfg.ferrosa.admin_password) {
            (Some(u), Some(p)) => (u.clone(), p.clone()),
            (None, None) => (cfg.ferrosa.username.clone(), cfg.ferrosa.password.clone()),
            _ => exit_incomplete_credentials("admin_username", "admin_password"),
        },
    )
}

fn exit_incomplete_credentials(left: &str, right: &str) -> ! {
    eprintln!("ERROR: {left} and {right} must be supplied together, or both omitted.");
    std::process::exit(2);
}

fn local_loopback_migration_credentials(contact_points: &[String]) -> Option<(String, String)> {
    if contact_points
        .iter()
        .all(|cp| is_loopback_contact_point(cp))
    {
        Some(("ferrosa_admin".to_string(), "ferrosa_admin".to_string()))
    } else {
        None
    }
}

fn is_loopback_contact_point(contact_point: &str) -> bool {
    let host = contact_point
        .rsplit_once('@')
        .map_or(contact_point, |(_, host_port)| host_port)
        .rsplit_once(':')
        .map_or(contact_point, |(host, _)| host)
        .trim_matches(['[', ']']);

    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_then_prepare_rewrites_timestamp_functions_before_apply() {
        let body = "INSERT INTO agent_memory.entity_types (type_name, created_at) VALUES ('person', toTimestamp(now()));";
        let applied_at = chrono::DateTime::parse_from_rfc3339("2026-05-04T22:53:21.123Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let prepared = prepare_statements_for_keyspace(body, "agent_memory", applied_at, None)
            .pop()
            .expect("statement");
        assert!(!prepared.contains("toTimestamp(now())"));
        assert!(prepared.contains("'2026-05-04T22:53:21.123Z'"));
    }

    #[test]
    fn split_then_prepare_honors_requested_keyspace_before_apply() {
        let body = "CREATE KEYSPACE IF NOT EXISTS agent_memory WITH replication = {};\n\
                    USE agent_memory;\n\
                    CREATE TABLE IF NOT EXISTS agent_memory.entity_types (type_name text PRIMARY KEY);\n\
                    INSERT INTO agent_memory.entity_types (type_name, created_at) VALUES ('person', toTimestamp(now()));";
        let applied_at = chrono::DateTime::parse_from_rfc3339("2026-05-04T22:53:21.123Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let prepared =
            prepare_statements_for_keyspace(body, "agent_memory_pr12_keyspace", applied_at, None);

        assert!(
            prepared
                .iter()
                .any(|stmt| stmt.contains("agent_memory_pr12_keyspace")),
            "migrate --keyspace must rewrite raw DDL to the requested keyspace before apply: {prepared:#?}"
        );
        assert!(
            prepared.iter().all(|stmt| !stmt.contains("agent_memory.")),
            "migrate --keyspace must not apply raw production-keyspace table references: {prepared:#?}"
        );
    }

    #[test]
    fn parse_args_loads_local_config_credentials_when_cli_omits_auth() {
        let config_toml = r#"
[ferrosa]
contact_points = ["localhost:19042", "localhost:19043"]
keyspace = "agent_memory"
username = "ferrosa_admin"
password = "ferrosa_admin"
"#;

        let args = parse_args_from(
            ["--contact-points", "127.0.0.1:19042"],
            [],
            Some(config_toml),
        );

        assert_eq!(args.contact_points, vec!["127.0.0.1:19042"]);
        assert_eq!(args.keyspace, "agent_memory");
        assert_eq!(
            args.credentials,
            Some(("ferrosa_admin".to_string(), "ferrosa_admin".to_string()))
        );
    }

    #[test]
    fn parse_args_prefers_admin_config_credentials_for_migrations() {
        let config_toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
username = "ferrosa_user"
password = "ferrosa_user"
admin_username = "ferrosa_admin"
admin_password = "ferrosa_admin"
"#;

        let args = parse_args_from(std::iter::empty::<&str>(), [], Some(config_toml));

        assert_eq!(args.contact_points, vec!["localhost:19042"]);
        assert_eq!(
            args.credentials,
            Some(("ferrosa_admin".to_string(), "ferrosa_admin".to_string()))
        );
    }

    #[test]
    fn parse_args_cli_credentials_override_local_config() {
        let config_toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
username = "ferrosa_admin"
password = "ferrosa_admin"
"#;

        let args = parse_args_from(
            ["--user", "cli_user", "--password", "cli_pass"],
            [],
            Some(config_toml),
        );

        assert_eq!(
            args.credentials,
            Some(("cli_user".to_string(), "cli_pass".to_string()))
        );
    }

    #[test]
    fn parse_args_uses_local_admin_credentials_for_loopback_without_config() {
        let args = parse_args_from(["--contact-points", "127.0.0.1:19042"], [], None);

        assert_eq!(
            args.credentials,
            Some(("ferrosa_admin".to_string(), "ferrosa_admin".to_string()))
        );
    }

    #[test]
    fn parse_args_does_not_use_local_defaults_for_remote_contact_points() {
        let args = parse_args_from(["--contact-points", "ferrosa.example.com:9042"], [], None);

        assert_eq!(args.credentials, None);
    }

    #[test]
    fn parse_args_probe_only_defaults_off() {
        let args = parse_args_from(["--contact-points", "127.0.0.1:19042"], [], None);
        assert!(!args.probe_only);
    }

    #[test]
    fn parse_args_probe_only_flag_sets_it() {
        let args = parse_args_from(
            ["--contact-points", "127.0.0.1:19043", "--probe-only"],
            [],
            None,
        );
        assert!(args.probe_only);
    }
}
