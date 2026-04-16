//! Application-owned schema versioning.
//!
//! Each DDL file is declared as a [`Migration`] with a monotonically
//! increasing `version` number. At server startup [`run_migrations`] queries
//! the `schema_version` table, applies pending migrations in order, and
//! records each success. On failure it fails loud — startup aborts and the
//! operator's backup is the rollback path.
//!
//! ## Adoption for pre-versioning installs
//!
//! DDLs 001-019 were applied manually before this module existed. When
//! `run_migrations` runs for the first time against an existing keyspace,
//! it auto-seeds `schema_version` to the pre-versioning baseline (version
//! 19) so only migration 20 and later execute. Fresh keyspaces start at
//! version 0 and apply every migration in the registry (though the
//! pre-versioning DDLs are expected to have been applied as bootstrap).
//!
//! ## Rollback
//!
//! Forward-only. If a newer build has registered a migration past the
//! current code's registry — i.e. someone ran a newer binary — startup
//! aborts with a clear "downgrade detected" error. Restore from backup to
//! recover.

use cdrs_tokio::query::QueryValues;
use cdrs_tokio::query_values;
use cdrs_tokio::types::ByName;

use crate::cql_storage::CqlSession;

/// A single schema change wired into the server binary.
#[derive(Debug)]
pub struct Migration {
    /// Monotonically increasing version number. Corresponds to the `NNN` in
    /// `ddl/NNN_*.cql`.
    pub version: u32,
    /// Short human-readable summary of the change.
    pub description: &'static str,
    /// The CQL DDL text. May contain multiple statements separated by `;`
    /// and `--` comments (both stripped by the splitter).
    pub ddl: &'static str,
}

/// Version at which the codebase switched from manual DDL apply to this
/// module. Any keyspace that existed before this boundary is auto-seeded
/// to this version on first run of the migration runner.
pub const PRE_VERSIONING_BASELINE: u32 = 19;

/// Ordered registry of migrations. Append only. Never edit an existing
/// entry's `ddl` — that would produce divergent schemas across
/// deployments. Bump the version and add a new migration instead.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 20,
    description: "rich entity schema (Sprint 1 of skills layer)",
    ddl: include_str!("../../../ddl/020_rich_entity_schema.cql"),
}];

/// Error type for migration failures. Every variant carries enough context
/// for an operator to triage and reach for the backup.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("schema downgrade detected: keyspace at v{keyspace}, this build only supports up to v{code}. Restore from backup or upgrade the binary.")]
    Downgrade { keyspace: u32, code: u32 },
    #[error("migration {version} failed on statement {stmt_index}: {source}. Schema remains at v{last_good}.")]
    Statement {
        version: u32,
        stmt_index: usize,
        last_good: u32,
        #[source]
        source: anyhow::Error,
    },
    #[error("schema_version bookkeeping write failed after migration {version} applied: {source}")]
    BookkeepingWrite {
        version: u32,
        #[source]
        source: anyhow::Error,
    },
    #[error("schema_version table setup failed: {source}")]
    Setup {
        #[source]
        source: anyhow::Error,
    },
}

/// Apply every migration whose version is strictly greater than the
/// keyspace's current version. Returns the number of migrations applied.
///
/// Runs `schema_version` table creation and adoption-seed logic first.
/// Safe to run on every boot — the check is a single query when up to date.
pub async fn run_migrations(
    session: &CqlSession,
    keyspace: &str,
) -> Result<usize, MigrationError> {
    ensure_schema_version_table(session, keyspace)
        .await
        .map_err(|e| MigrationError::Setup { source: e })?;

    let current = current_version(session, keyspace)
        .await
        .map_err(|e| MigrationError::Setup { source: e })?;

    // Detect adoption: if schema_version is empty but the keyspace has
    // tables from the pre-versioning era, seed the baseline.
    let current = match current {
        Some(v) => v,
        None => {
            let has_legacy = keyspace_has_legacy_tables(session, keyspace)
                .await
                .unwrap_or(false);
            if has_legacy {
                tracing::info!(
                    baseline = PRE_VERSIONING_BASELINE,
                    "schema_version empty but legacy tables present; seeding adoption baseline"
                );
                record_version(
                    session,
                    keyspace,
                    PRE_VERSIONING_BASELINE,
                    "pre-versioning baseline (adoption seed)",
                )
                .await
                .map_err(|e| MigrationError::BookkeepingWrite {
                    version: PRE_VERSIONING_BASELINE,
                    source: e,
                })?;
                PRE_VERSIONING_BASELINE
            } else {
                0
            }
        }
    };

    // Downgrade protection: the registry's top version must be >= keyspace's.
    let code_max = MIGRATIONS
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap_or(PRE_VERSIONING_BASELINE);
    if current > code_max {
        return Err(MigrationError::Downgrade {
            keyspace: current,
            code: code_max,
        });
    }

    let pending: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|m| m.version > current)
        .collect();

    if pending.is_empty() {
        tracing::debug!(current, "schema up to date");
        return Ok(0);
    }

    tracing::info!(
        current,
        pending_count = pending.len(),
        target = code_max,
        "applying schema migrations"
    );

    let mut applied = 0usize;
    let mut last_good = current;
    for m in pending {
        tracing::info!(version = m.version, description = m.description, "applying migration");
        for (i, stmt) in split_cql(m.ddl).iter().enumerate() {
            if let Err(source) = session.query(stmt.as_str()).await {
                return Err(MigrationError::Statement {
                    version: m.version,
                    stmt_index: i,
                    last_good,
                    source: source.into(),
                });
            }
        }
        record_version(session, keyspace, m.version, m.description)
            .await
            .map_err(|source| MigrationError::BookkeepingWrite {
                version: m.version,
                source,
            })?;
        last_good = m.version;
        applied += 1;
    }

    tracing::info!(
        applied,
        current_version = last_good,
        "schema migrations complete"
    );
    Ok(applied)
}

async fn ensure_schema_version_table(
    session: &CqlSession,
    keyspace: &str,
) -> anyhow::Result<()> {
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS {keyspace}.schema_version (\
            version int PRIMARY KEY,\
            applied_at timestamp,\
            description text,\
            applied_by text)"
    );
    session.query(ddl).await?;
    Ok(())
}

async fn current_version(
    session: &CqlSession,
    keyspace: &str,
) -> anyhow::Result<Option<u32>> {
    let q = format!("SELECT version FROM {keyspace}.schema_version");
    let envelope = session.query(q).await?;
    let rows = envelope.response_body()?.into_rows().unwrap_or_default();
    let mut max: Option<u32> = None;
    for row in rows {
        if let Ok(v) = row.r_by_name::<i32>("version") {
            let v = v as u32;
            max = Some(max.map_or(v, |m| m.max(v)));
        }
    }
    Ok(max)
}

async fn record_version(
    session: &CqlSession,
    keyspace: &str,
    version: u32,
    description: &str,
) -> anyhow::Result<()> {
    let host = hostname().unwrap_or_else(|| "unknown".into());
    let q = format!(
        "INSERT INTO {keyspace}.schema_version \
         (version, applied_at, description, applied_by) \
         VALUES (?, toTimestamp(now()), ?, ?)"
    );
    session
        .query_with_values(
            q,
            query_values!(
                version as i32,
                description.to_string(),
                host
            ),
        )
        .await?;
    Ok(())
}

async fn keyspace_has_legacy_tables(
    session: &CqlSession,
    keyspace: &str,
) -> anyhow::Result<bool> {
    // If `entity_store` exists, this keyspace was bootstrapped with the
    // pre-versioning DDLs.
    let q = format!(
        "SELECT table_name FROM system_schema.tables \
         WHERE keyspace_name = '{keyspace}' AND table_name = 'entity_store'"
    );
    let envelope = session.query(q).await?;
    let rows = envelope.response_body()?.into_rows().unwrap_or_default();
    Ok(!rows.is_empty())
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
}

// Suppress the unused warning while only query_values is used via macro.
#[allow(dead_code)]
fn _assert_query_values_type_used() {
    let _: QueryValues = query_values!("dummy".to_string());
}

/// Split a CQL DDL script into individual statements.
///
/// Strips line comments (`-- ...` to end of line), ignores blank lines and
/// whitespace, and splits on `;`. Does not handle block comments or strings
/// containing semicolons — the DDL files under `ddl/` don't use those.
pub fn split_cql(ddl: &str) -> Vec<String> {
    let mut stripped = String::with_capacity(ddl.len());
    for line in ddl.lines() {
        let code_only = match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        };
        stripped.push_str(code_only);
        stripped.push('\n');
    }
    stripped
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_cql_strips_line_comments() {
        let ddl = "\
            -- this is a comment\n\
            CREATE TABLE foo (id int PRIMARY KEY);\n\
            -- another comment\n\
            ALTER TABLE foo ADD bar text;\n\
        ";
        let stmts = split_cql(ddl);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("CREATE TABLE foo"));
        assert!(stmts[1].starts_with("ALTER TABLE foo"));
    }

    #[test]
    fn split_cql_handles_inline_comments() {
        let ddl = "CREATE TABLE foo (id int PRIMARY KEY); -- the main table\nALTER TABLE foo ADD bar text;\n";
        let stmts = split_cql(ddl);
        assert_eq!(stmts.len(), 2);
        assert!(!stmts[0].contains("main table"));
    }

    #[test]
    fn split_cql_preserves_multiline_statements() {
        let ddl = "CREATE TABLE foo (\n  id int PRIMARY KEY,\n  name text\n);\nALTER TABLE foo ADD bar text;\n";
        let stmts = split_cql(ddl);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("PRIMARY KEY"));
        assert!(stmts[0].contains("name text"));
    }

    #[test]
    fn split_cql_ignores_empty_and_whitespace() {
        let ddl = ";;\n\n   ;  \n\n";
        assert!(split_cql(ddl).is_empty());
    }

    #[test]
    fn registry_versions_are_monotonic_and_unique() {
        let mut prev = PRE_VERSIONING_BASELINE;
        for m in MIGRATIONS {
            assert!(
                m.version > prev,
                "migration versions must be strictly increasing; got {} after {}",
                m.version,
                prev
            );
            prev = m.version;
        }
    }

    #[test]
    fn migration_020_embeds_the_rich_entity_ddl() {
        // Sanity: ensure include_str! picked up the expected DDL content.
        let m20 = MIGRATIONS.iter().find(|m| m.version == 20).expect("v20 present");
        assert!(m20.ddl.contains("ALTER TABLE entity_store ADD description"));
        assert!(m20.ddl.contains("ALTER TABLE entity_store ADD scope"));
    }

    #[test]
    fn downgrade_error_formats_versions() {
        let err = MigrationError::Downgrade {
            keyspace: 25,
            code: 20,
        };
        let msg = err.to_string();
        assert!(msg.contains("v25"));
        assert!(msg.contains("v20"));
        assert!(msg.contains("backup"), "error must point the operator at backup recovery");
    }
}
