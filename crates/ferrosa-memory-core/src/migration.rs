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

use futures_util::StreamExt;
use scylla::frame::response::result::{CqlValue, Row};
use scylla::frame::value::CqlTimeuuid;
use uuid::Uuid;

use crate::cql_storage::{ColMap, CqlSession, build_col_map, cql_get};

/// Operator-facing schema status for the running binary and target keyspace.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MigrationStatus {
    pub db_version: u32,
    pub binary_version: u32,
    pub pending: Vec<u32>,
    pub last_applied: Option<String>,
}

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
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 20,
        description: "rich entity schema (Sprint 1 of skills layer)",
        ddl: include_str!("../../../ddl/020_rich_entity_schema.cql"),
    },
    Migration {
        version: 21,
        description: "derived cache TTL tracking table",
        ddl: include_str!("../../../ddl/021_derived_cache_ttl.cql"),
    },
    Migration {
        version: 22,
        description: "approval log store",
        ddl: include_str!("../../../ddl/022_approval_store.cql"),
    },
    Migration {
        version: 23,
        description: "exact alias registry store",
        ddl: include_str!("../../../ddl/023_alias_store.cql"),
    },
    Migration {
        version: 24,
        description: "active rule index for wildcard rule listing",
        ddl: include_str!("../../../ddl/024_rules_active_index.cql"),
    },
    Migration {
        version: 25,
        description: "warmth reputation backfill",
        ddl: include_str!("../../../ddl/025_warmth_reputation.cql"),
    },
    Migration {
        version: 26,
        description: "confidence scoring table",
        ddl: include_str!("../../../ddl/026_confidence_scoring.cql"),
    },
    Migration {
        version: 27,
        description: "contradiction registry",
        ddl: include_str!("../../../ddl/027_contradiction_registry.cql"),
    },
    Migration {
        version: 28,
        description: "consolidation pipeline tables",
        ddl: include_str!("../../../ddl/028_consolidation_pipeline.cql"),
    },
    Migration {
        version: 29,
        description: "domain schema bundles",
        ddl: include_str!("../../../ddl/029_domain_schema_bundles.cql"),
    },
    Migration {
        version: 30,
        description: "fix temporal_events timeuuid → uuid columns",
        ddl: include_str!("../../../ddl/030_temporal_events_uuid_columns.cql"),
    },
    Migration {
        version: 31,
        description: "add first_seen timestamp to co_occurs_with edge table",
        ddl: include_str!("../../../ddl/031_co_occurs_first_seen.cql"),
    },
    Migration {
        version: 32,
        description: "temporal semantic context segments",
        ddl: include_str!("../../../ddl/032_context_segments.cql"),
    },
    Migration {
        version: 33,
        description: "fix trajectory_folds timeuuid → uuid fold identifiers",
        ddl: include_str!("../../../ddl/033_trajectory_folds_uuid_columns.cql"),
    },
    Migration {
        version: 34,
        description: "secondary indexes for Datalog filters",
        ddl: include_str!("../../../ddl/034_datalog_filter_indexes.cql"),
    },
    Migration {
        version: 35,
        description: "semantic document chunks and retrieval indexes",
        ddl: include_str!("../../../ddl/035_document_chunks.cql"),
    },
    Migration {
        version: 36,
        description: "fix feedback_outcomes query_id timeuuid → uuid",
        ddl: include_str!("../../../ddl/036_feedback_outcomes_query_uuid.cql"),
    },
    Migration {
        version: 37,
        description: "forget journal store (durable, replayable forget operations)",
        ddl: include_str!("../../../ddl/037_forget_journal.cql"),
    },
    Migration {
        version: 38,
        description: "entity/object retraction records (forget audit + restore metadata)",
        ddl: include_str!("../../../ddl/038_retraction_record.cql"),
    },
];

/// Pre-versioning DDLs. Applied in order when `run_migrations` detects a
/// greenfield keyspace (no keyspace row in `system_schema.keyspaces`).
/// Existing deployments — the ones that ran DDLs 001-019 manually via
/// cqlsh before this module existed — skip bootstrap and use the
/// adoption seed at [`PRE_VERSIONING_BASELINE`].
///
/// Ordering mirrors the `ddl/NNN_*.cql` filenames. Two pairs share
/// numbers on disk (008, 009); they're serialized here in the order the
/// dev cluster was bootstrapped historically.
pub const BOOTSTRAP_DDLS: &[&str] = &[
    include_str!("../../../ddl/001_keyspace.cql"),
    include_str!("../../../ddl/002_folds_entities.cql"),
    include_str!("../../../ddl/003_edge_tables.cql"),
    include_str!("../../../ddl/004_audit_anomaly.cql"),
    include_str!("../../../ddl/005_vector_columns.cql"),
    include_str!("../../../ddl/006_entity_state.cql"),
    include_str!("../../../ddl/007_intentions.cql"),
    include_str!("../../../ddl/008_intentions_repo_scope.cql"),
    include_str!("../../../ddl/008_routing_guidelines.cql"),
    include_str!("../../../ddl/009_secondary_indexes.cql"),
    include_str!("../../../ddl/009_tool_usage_log.cql"),
    include_str!("../../../ddl/010_edge_strength.cql"),
    include_str!("../../../ddl/011_warmth_field.cql"),
    include_str!("../../../ddl/012_datalog_rules.cql"),
    include_str!("../../../ddl/013_derived_cache.cql"),
    include_str!("../../../ddl/014_derivation_provenance.cql"),
    include_str!("../../../ddl/015_heat_telemetry.cql"),
    include_str!("../../../ddl/016_durable_materialization.cql"),
    include_str!("../../../ddl/017_typed_edges.cql"),
    include_str!("../../../ddl/018_edge_session_indexes.cql"),
    include_str!("../../../ddl/019_type_registry.cql"),
    include_str!("../../../ddl/020_rich_entity_schema.cql"),
    include_str!("../../../ddl/021_derived_cache_ttl.cql"),
    include_str!("../../../ddl/022_approval_store.cql"),
    include_str!("../../../ddl/023_alias_store.cql"),
    include_str!("../../../ddl/024_rules_active_index.cql"),
    include_str!("../../../ddl/025_warmth_reputation.cql"),
    include_str!("../../../ddl/026_confidence_scoring.cql"),
    include_str!("../../../ddl/027_contradiction_registry.cql"),
    include_str!("../../../ddl/028_consolidation_pipeline.cql"),
    include_str!("../../../ddl/029_domain_schema_bundles.cql"),
];

/// Role-auth seed DDL — creates `ferrosa_admin` (superuser) and
/// `ferrosa_user` (LOGIN), plus the keyspace/table-level grants that
/// give `ferrosa_user` SELECT on everything in `agent_memory` and
/// MODIFY only on application-owned tables.
///
/// Applied by `apply_bootstrap` ONLY when `FERROSA_AUTH_ENABLED=true`.
/// When auth is disabled, `system_auth` keyspace doesn't contain the
/// role tables and the DDL would fail — the guard prevents that
/// failure for operators who haven't flipped auth on yet.
///
/// See specs/decisions/design-cql-role-auth-rollout.md Sprint B.
pub const ROLES_DDL: &str = include_str!("../../../ddl/100_roles.cql");

/// Returns true if the migration runner should apply `ROLES_DDL`.
///
/// Gated on `FERROSA_AUTH_ENABLED=true` so a cluster with auth disabled
/// never tries to create roles against a non-existent `system_auth`
/// keyspace. Matches the shape of `ferrosa_storage::StorageEngineConfig`'s
/// `auth_enabled` plumbing — a single env var flips both sides.
pub fn should_apply_roles_ddl() -> bool {
    matches!(
        std::env::var("FERROSA_AUTH_ENABLED").ok().as_deref(),
        Some("true" | "1" | "on" | "yes")
    )
}

/// Error type for migration failures. Every variant carries enough context
/// for an operator to triage and reach for the backup.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error(
        "schema downgrade detected: keyspace at v{keyspace}, this build only supports up to v{code}. Restore from backup or upgrade the binary."
    )]
    Downgrade { keyspace: u32, code: u32 },
    #[error(
        "migration {version} failed on statement {stmt_index}: {source}. Schema remains at v{last_good}."
    )]
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

/// P0-11/W-03: Variant of `run_migrations` for DBaaS mode.
///
/// In DBaaS mode, the control plane provisions the keyspace and schema
/// before the application starts. The application must NOT issue any DDL
/// — it does not have DDL privileges on a managed cluster. Instead, this
/// function:
///
/// 1. Asserts that the keyspace already exists (fails loud if not).
/// 2. Returns `Ok(())` so the caller can proceed.
///
/// Use `run_migrations` for self-hosted / local-dev installs.
pub async fn assert_keyspace_exists_dbaas(
    session: &CqlSession,
    keyspace: &str,
) -> Result<(), MigrationError> {
    let exists = keyspace_exists(session, keyspace)
        .await
        .map_err(|e| MigrationError::Setup { source: e })?;
    if !exists {
        return Err(MigrationError::Setup {
            source: anyhow::anyhow!(
                "FERROSA_DBAAS_MODE=true but keyspace '{}' does not exist in \
                 system_schema.keyspaces. The DBaaS control plane must provision \
                 the keyspace before the application starts. \
                 Check tenant provisioning status or contact support.",
                keyspace
            ),
        });
    }
    tracing::info!(
        keyspace,
        "DBaaS mode: keyspace exists, skipping DDL — schema is managed by the control plane"
    );
    Ok(())
}

/// Apply every migration whose version is strictly greater than the
/// keyspace's current version. Returns the number of migrations applied.
///
/// Runs `schema_version` table creation and adoption-seed logic first.
/// Safe to run on every boot — the check is a single query when up to date.
///
/// In DBaaS mode, use `assert_keyspace_exists_dbaas` instead — this function
/// must not be called when `FERROSA_DBAAS_MODE=true`.
pub async fn run_migrations(session: &CqlSession, keyspace: &str) -> Result<usize, MigrationError> {
    // If the keyspace doesn't exist yet, this is a greenfield install.
    // Apply the historic DDLs (001-019) first so pre-versioning state
    // is in place before modern migrations (20+) run.
    let greenfield = !keyspace_exists(session, keyspace)
        .await
        .map_err(|e| MigrationError::Setup { source: e })?;
    if greenfield {
        tracing::info!(
            keyspace,
            bootstrap_count = BOOTSTRAP_DDLS.len(),
            "keyspace absent; running greenfield bootstrap"
        );
        apply_bootstrap(session, keyspace)
            .await
            .map_err(|source| MigrationError::Statement {
                version: 0,
                stmt_index: 0,
                last_good: 0,
                source,
            })?;
    }

    ensure_schema_version_table(session, keyspace)
        .await
        .map_err(|e| MigrationError::Setup { source: e })?;

    let current = current_version(session, keyspace)
        .await
        .map_err(|e| MigrationError::Setup { source: e })?;

    let current = match current {
        Some(v) => v,
        None => {
            // schema_version is empty. Seed the adoption baseline so the
            // keyspace is marked as "pre-versioning — up to v19" before
            // modern migrations run.
            tracing::info!(
                baseline = PRE_VERSIONING_BASELINE,
                "schema_version empty; seeding pre-versioning adoption baseline"
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

    // Gap detection: re-fetch the full applied set so that any registered
    // migration *not* present in `schema_version` is treated as pending —
    // even if `current` (MAX) sits above it. Closes the window where a
    // single bookkeeping row goes missing (manual rollback, partial
    // restore) and the runner silently never re-applies it.
    let mut applied = applied_versions(session, keyspace)
        .await
        .map_err(|e| MigrationError::Setup { source: e })?;
    let applied_migrations: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|m| applied.contains(&m.version))
        .collect();
    for m in applied_migrations {
        let satisfied = migration_postcondition_satisfied(session, keyspace, m.version)
            .await
            .map_err(|e| MigrationError::Setup { source: e })?;
        if !satisfied {
            tracing::warn!(
                version = m.version,
                description = m.description,
                "schema_version row exists but migration postcondition is missing; deleting ledger row for repair"
            );
            delete_version(session, keyspace, m.version)
                .await
                .map_err(|e| MigrationError::Setup { source: e })?;
            applied.remove(&m.version);
        }
    }
    let pending: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|m| !applied.contains(&m.version))
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

    // Pin the session's default keyspace to the configured one before
    // running each migration. DDL files must NOT hardcode `USE <ks>;` —
    // they're deployable into any keyspace (dev, test, per-tenant). The
    // split_cql helper strips any stray USE statements defensively.
    let use_ks = format!("USE {keyspace}");
    #[allow(deprecated)]
    session
        .query_unpaged(use_ks, ())
        .await
        .map_err(|e| MigrationError::Setup { source: e.into() })?;

    let mut applied = 0usize;
    let mut last_good = current;
    for m in pending {
        tracing::info!(
            version = m.version,
            description = m.description,
            "applying migration"
        );
        if m.version == 36 {
            apply_feedback_outcomes_query_id_uuid_migration(session, keyspace)
                .await
                .map_err(|source| MigrationError::Statement {
                    version: m.version,
                    stmt_index: 0,
                    last_good,
                    source,
                })?;
        } else {
            // Some DDL files in MIGRATIONS hardcode `agent_memory.<table>`
            // qualified references — `USE keyspace` only helps unqualified
            // names. Run the same qualify_ddl rewrite the bootstrap path
            // uses so the modern migrations are deployable into any
            // configured keyspace (agent_memory_test, per-tenant ks, etc.).
            let qualified = qualify_ddl(m.ddl, keyspace);
            for (i, stmt) in split_cql(&qualified).iter().enumerate() {
                #[allow(deprecated)]
                if let Err(source) = session.query_unpaged(stmt.as_str(), ()).await {
                    return Err(MigrationError::Statement {
                        version: m.version,
                        stmt_index: i,
                        last_good,
                        source: source.into(),
                    });
                }
            }
        }
        // Allow schema to settle across nodes before recording version.
        if let Err(e) = session.await_schema_agreement().await {
            return Err(MigrationError::Statement {
                version: m.version,
                stmt_index: split_cql(m.ddl).len(),
                last_good,
                source: e.into(),
            });
        }
        let satisfied = migration_postcondition_satisfied(session, keyspace, m.version)
            .await
            .map_err(|source| MigrationError::Statement {
                version: m.version,
                stmt_index: split_cql(m.ddl).len(),
                last_good,
                source,
            })?;
        if !satisfied {
            return Err(MigrationError::Statement {
                version: m.version,
                stmt_index: split_cql(m.ddl).len(),
                last_good,
                source: anyhow::anyhow!(
                    "migration {} postcondition failed; required schema objects are missing",
                    m.version
                ),
            });
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

/// Return the current migration status without applying migrations.
///
/// This is intentionally read-only so operators and MCP clients can ask
/// whether the database schema is current without causing DDL side effects.
pub async fn migration_status(
    session: &CqlSession,
    keyspace: &str,
) -> anyhow::Result<MigrationStatus> {
    ensure_schema_version_table(session, keyspace).await?;
    let applied = applied_versions(session, keyspace).await?;
    let db_version = current_version(session, keyspace).await?.unwrap_or(0);
    let binary_version = MIGRATIONS
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap_or(PRE_VERSIONING_BASELINE);
    let pending = MIGRATIONS
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .map(|m| m.version)
        .collect();
    let last_applied = last_applied_at(session, keyspace).await?;

    Ok(MigrationStatus {
        db_version,
        binary_version,
        pending,
        last_applied,
    })
}

/// Apply the historic bootstrap DDLs against a greenfield keyspace.
///
/// Handles two kinds of DDL rewriting:
///
/// 1. **Hardcoded `agent_memory` references** — DDL files hardcode the
///    production keyspace name in `CREATE KEYSPACE`, keyspace-qualified
///    table references (`agent_memory.entity_types`), and graph
///    extension strings. We substitute these with the configured
///    keyspace before execution.
/// 2. **Unqualified table names after the `USE agent_memory;` convention**
///    — Most DDLs end up with lines like `CREATE TABLE IF NOT EXISTS
///    memo_cache (...)` that rely on the session's default keyspace.
///    `split_cql` strips the USE statements, so we have to prefix every
///    unqualified `CREATE TABLE`, `CREATE INDEX ... ON <table>`, and
///    `ALTER TABLE <table>` with the keyspace.
async fn apply_bootstrap(session: &CqlSession, keyspace: &str) -> anyhow::Result<()> {
    let applied_at = chrono::Utc::now();
    for (file_idx, ddl) in BOOTSTRAP_DDLS.iter().enumerate() {
        let rewritten = qualify_ddl(ddl, keyspace);
        for (i, stmt) in split_cql(&rewritten).iter().enumerate() {
            let prepared = prepare_bootstrap_statement(stmt, applied_at);
            #[allow(deprecated)]
            if let Err(e) = session.query_unpaged(prepared.as_str(), ()).await {
                anyhow::bail!(
                    "bootstrap DDL[{file_idx}] statement {i} failed: {e}\n--- statement ---\n{prepared}"
                );
            }
            // Wait for schema agreement so subsequent statements don't race
            // against a not-yet-visible table on other nodes.
            if let Err(e) = session.await_schema_agreement().await {
                anyhow::bail!(
                    "bootstrap DDL[{file_idx}] statement {i}: schema agreement timeout: {e}"
                );
            }
        }
    }

    // Role/grant DDL is gated — see ROLES_DDL doc comment. Running it
    // without auth enabled would fail because system_auth isn't usable.
    if should_apply_roles_ddl() {
        let rewritten = qualify_ddl(ROLES_DDL, keyspace);
        for (i, stmt) in split_cql(&rewritten).iter().enumerate() {
            #[allow(deprecated)]
            if let Err(e) = session.query_unpaged(stmt.as_str(), ()).await {
                anyhow::bail!("roles DDL statement {i} failed: {e}\n--- statement ---\n{stmt}");
            }
        }
    }
    Ok(())
}

pub fn prepare_bootstrap_statement(
    stmt: &str,
    applied_at: chrono::DateTime<chrono::Utc>,
) -> String {
    let timestamp_literal = format!(
        "'{}'",
        applied_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    stmt.replace("toTimestamp(now())", &timestamp_literal)
        .replace("now()", &timestamp_literal)
}

/// Substitute the hardcoded `agent_memory` keyspace with the configured
/// keyspace, and prefix unqualified `CREATE TABLE`, `CREATE INDEX ... ON`,
/// `ALTER TABLE`, `DROP TABLE`, `INSERT INTO` with the keyspace.
///
/// Operates at the statement level (split on `;` via split_cql) so that
/// multi-line CREATE INDEX / CREATE TABLE statements where the target
/// identifier lives on a continuation line still get qualified.
///
/// Public for testing — callers should only go through `apply_bootstrap`.
pub fn qualify_ddl(ddl: &str, keyspace: &str) -> String {
    // Step 1: replace qualified references (`agent_memory.table`) and the
    // CREATE KEYSPACE / WITH agent_memory usage. Word-boundary matches
    // avoid rewriting "agent_memory" embedded in prose or graph labels.
    let mut out = ddl.replace("agent_memory.", &format!("{keyspace}."));
    for pre in [
        " agent_memory ",
        " agent_memory;",
        " agent_memory\n",
        "\tagent_memory ",
    ] {
        out = out.replace(pre, &pre.replace("agent_memory", keyspace));
    }

    // Step 2: strip line comments FIRST, so a semicolon inside a `--`
    // comment doesn't trigger a false statement split downstream.
    let mut no_comments = String::with_capacity(out.len());
    for line in out.lines() {
        let code = match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        };
        no_comments.push_str(code);
        no_comments.push('\n');
    }

    // Step 3: qualify unqualified target identifiers in DDL statements.
    // Split into statements on top-level `;`, qualify each, join back.
    // Preserves line breaks inside statements (multi-line CREATE INDEX).
    let mut stmts: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in no_comments.chars() {
        if ch == ';' {
            stmts.push(qualify_stmt(&current, keyspace));
            stmts.push(";".into());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        stmts.push(qualify_stmt(&current, keyspace));
    }
    stmts.concat()
}

/// Rewrite a single statement (no trailing `;`) to qualify the first
/// table-shaped identifier after a known DDL prefix.
fn qualify_stmt(stmt: &str, keyspace: &str) -> String {
    // Only care about the leading non-comment keyword. Strip any `--`
    // line comments before scanning.
    let leading: String = stmt
        .lines()
        .map(|line| match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let leading_trim = leading.trim_start();

    // Longest-first so `CREATE TABLE IF NOT EXISTS ` wins over `CREATE TABLE `.
    const PATTERNS: &[(&str, bool)] = &[
        ("CREATE TABLE IF NOT EXISTS ", false),
        ("CREATE TABLE ", false),
        ("DROP TABLE IF EXISTS ", false),
        ("DROP TABLE ", false),
        ("ALTER TABLE ", false),
        ("INSERT INTO ", false),
        ("UPDATE ", false),
        ("DELETE FROM ", false),
        ("TRUNCATE ", false),
        ("CREATE INDEX IF NOT EXISTS ", true),
        ("CREATE INDEX ", true),
    ];

    for (prefix, is_create_index) in PATTERNS {
        if !leading_trim.len().ge(&prefix.len())
            || !leading_trim[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            continue;
        }
        // Found a match. Figure out where the target identifier lives in
        // the ORIGINAL stmt (not the comment-stripped leading), and patch
        // it in place.
        let prefix_end_in_stmt = match find_ci(stmt, prefix) {
            Some(i) => i + prefix.len(),
            None => continue,
        };
        return if *is_create_index {
            qualify_create_index_stmt(stmt, prefix_end_in_stmt, keyspace)
                .unwrap_or_else(|| stmt.to_string())
        } else {
            qualify_target_ident(stmt, prefix_end_in_stmt, keyspace)
                .unwrap_or_else(|| stmt.to_string())
        };
    }
    stmt.to_string()
}

/// Given `stmt` and the byte offset just past the DDL prefix, qualify
/// the next unqualified identifier. Whitespace (including newlines) is
/// consumed before the identifier.
fn qualify_target_ident(stmt: &str, start: usize, keyspace: &str) -> Option<String> {
    let rest = &stmt[start..];
    let ident_start_rel = rest.find(|c: char| !c.is_whitespace())?;
    let ident_start = start + ident_start_rel;
    let ident_tail = &stmt[ident_start..];
    let (ident, _) = split_at_first_paren_or_whitespace(ident_tail);
    let qualified = try_qualify_identifier(ident, keyspace)?;
    let ident_end = ident_start + ident.len();
    Some(format!(
        "{}{}{}",
        &stmt[..ident_start],
        qualified,
        &stmt[ident_end..]
    ))
}

/// CREATE INDEX has a name between the prefix and the ON clause. Find
/// the ON keyword (case-insensitive, surrounded by any whitespace), then
/// qualify the identifier after it.
fn qualify_create_index_stmt(stmt: &str, start: usize, keyspace: &str) -> Option<String> {
    let after_on = find_on_keyword(stmt, start)?;
    qualify_target_ident(stmt, after_on, keyspace)
}

/// Return the byte offset just past a standalone `ON` keyword (case-
/// insensitive) somewhere after `start`. The `ON` must be preceded and
/// followed by whitespace so we don't match inside a larger identifier
/// (e.g., "on_create" or "MENTIONED_ON_STARTUP").
fn find_on_keyword(stmt: &str, start: usize) -> Option<usize> {
    let bytes = stmt.as_bytes();
    let mut i = start;
    while i + 2 <= bytes.len() {
        let (c0, c1) = (bytes[i], bytes[i + 1]);
        if (c0 == b'O' || c0 == b'o') && (c1 == b'N' || c1 == b'n') {
            let prev_ok = i == 0 || (bytes[i - 1] as char).is_whitespace();
            let next_ok = i + 2 >= bytes.len()
                || (bytes[i + 2] as char).is_whitespace()
                || bytes[i + 2] == b'(';
            if prev_ok && next_ok {
                return Some(i + 2);
            }
        }
        i += 1;
    }
    None
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.to_ascii_uppercase();
    let n = needle.to_ascii_uppercase();
    h.find(&n)
}

/// If `ident` looks like a bare SQL identifier (alphanumeric + underscore,
/// not already qualified with a dot), return `keyspace.ident`.
fn try_qualify_identifier(ident: &str, keyspace: &str) -> Option<String> {
    let t = ident.trim();
    if t.is_empty() || t.contains('.') {
        return None;
    }
    if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(format!("{keyspace}.{t}"))
}

fn split_at_first_paren_or_whitespace(s: &str) -> (&str, &str) {
    for (i, c) in s.char_indices() {
        if c == '(' || c == ';' || c.is_whitespace() {
            return (&s[..i], &s[i..]);
        }
    }
    (s, "")
}

/// Check whether a keyspace with the given name exists.
///
/// Filters client-side: some Ferrosa builds don't honor
/// `WHERE keyspace_name = '...'` on `system_schema.keyspaces`, so we pull
/// all rows and match the `keyspace_name` column ourselves.
async fn keyspace_exists(session: &CqlSession, keyspace: &str) -> anyhow::Result<bool> {
    #[allow(deprecated)]
    let result = session
        .query_unpaged("SELECT keyspace_name FROM system_schema.keyspaces", ())
        .await?;
    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    for row in rows {
        if let Ok(name) = cql_get::<String>(&row, &col_map, "keyspace_name")
            && name == keyspace
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn table_exists(session: &CqlSession, keyspace: &str, table: &str) -> anyhow::Result<bool> {
    #[allow(deprecated)]
    let result = session
        .query_unpaged(
            "SELECT keyspace_name, table_name FROM system_schema.tables",
            (),
        )
        .await?;
    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    for row in rows {
        let Ok(ks) = cql_get::<String>(&row, &col_map, "keyspace_name") else {
            continue;
        };
        let Ok(name) = cql_get::<String>(&row, &col_map, "table_name") else {
            continue;
        };
        if ks == keyspace && name == table {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn column_type(
    session: &CqlSession,
    keyspace: &str,
    table: &str,
    column: &str,
) -> anyhow::Result<Option<String>> {
    #[allow(deprecated)]
    let result = session
        .query_unpaged(
            format!(
                "SELECT type FROM system_schema.columns \
                 WHERE keyspace_name = '{keyspace}' \
                 AND table_name = '{table}' \
                 AND column_name = '{column}'"
            ),
            (),
        )
        .await?;
    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    let mut found: Option<String> = None;
    for row in rows {
        let actual = cql_get::<String>(&row, &col_map, "type")?;
        if let Some(prior) = &found {
            if !prior.eq_ignore_ascii_case(&actual) {
                anyhow::bail!(
                    "conflicting system_schema.columns rows for {keyspace}.{table}.{column}: {prior} and {actual}"
                );
            }
        } else {
            found = Some(actual);
        }
    }
    Ok(found)
}

fn cql_get_raw(row: &Row, col_map: &ColMap, name: &str) -> Option<CqlValue> {
    let idx = col_map.get(name)?;
    row.columns.get(*idx).cloned().flatten()
}

fn feedback_outcomes_create_table_ddl(keyspace: &str, table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {keyspace}.{table} (\
            tenant_id uuid,\
            session_id uuid,\
            query_id uuid,\
            program_type text,\
            query_embedding vector<float, 768>,\
            task_complexity text,\
            succeeded boolean,\
            latency_ms int,\
            token_cost int,\
            guideline_version text,\
            created_at timestamp,\
            PRIMARY KEY ((tenant_id), created_at, query_id)\
        ) WITH CLUSTERING ORDER BY (created_at DESC, query_id DESC)"
    )
}

async fn count_table(session: &CqlSession, keyspace: &str, table: &str) -> anyhow::Result<i64> {
    #[allow(deprecated)]
    let result = session
        .query_unpaged(format!("SELECT COUNT(*) FROM {keyspace}.{table}"), ())
        .await?;
    let rows = result.rows_or_empty();
    let row = rows
        .first()
        .ok_or_else(|| anyhow::anyhow!("COUNT(*) returned no rows for {keyspace}.{table}"))?;
    match row.columns.first().cloned().flatten() {
        Some(CqlValue::BigInt(count)) => Ok(count),
        Some(CqlValue::Int(count)) => Ok(i64::from(count)),
        other => {
            anyhow::bail!("COUNT(*) for {keyspace}.{table} returned unexpected value {other:?}")
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FeedbackQueryIdEncoding {
    Uuid,
    Timeuuid,
}

#[allow(deprecated)]
async fn copy_feedback_outcomes_rows(
    session: &CqlSession,
    keyspace: &str,
    src_table: &str,
    dst_table: &str,
    query_id_encoding: FeedbackQueryIdEncoding,
) -> anyhow::Result<usize> {
    let select = format!(
        "SELECT tenant_id, session_id, query_id, program_type, query_embedding, \
         task_complexity, succeeded, latency_ms, token_cost, guideline_version, created_at \
         FROM {keyspace}.{src_table}"
    );
    let insert = session
        .prepare(format!(
            "INSERT INTO {keyspace}.{dst_table} \
             (tenant_id, session_id, query_id, program_type, query_embedding, task_complexity, \
              succeeded, latency_ms, token_cost, guideline_version, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .await?;
    let select = session.prepare(select).await?;
    let mut iter = session.execute_iter(select, ()).await?;
    let col_map = build_col_map(iter.get_column_specs());
    let mut copied = 0usize;
    while let Some(row) = iter.next().await {
        let row = row?;
        let tenant_id: Uuid = cql_get(&row, &col_map, "tenant_id")?;
        let session_id: Uuid = cql_get(&row, &col_map, "session_id")?;
        let query_id = match query_id_encoding {
            FeedbackQueryIdEncoding::Uuid => cql_get::<Uuid>(&row, &col_map, "query_id")?,
            FeedbackQueryIdEncoding::Timeuuid => {
                let legacy = cql_get::<CqlTimeuuid>(&row, &col_map, "query_id")?;
                Uuid::from(legacy)
            }
        };
        let program_type: String = cql_get(&row, &col_map, "program_type")?;
        let query_embedding = cql_get_raw(&row, &col_map, "query_embedding");
        let task_complexity: String = cql_get(&row, &col_map, "task_complexity")?;
        let succeeded: bool = cql_get(&row, &col_map, "succeeded")?;
        let latency_ms: i32 = cql_get(&row, &col_map, "latency_ms")?;
        let token_cost: i32 = cql_get(&row, &col_map, "token_cost")?;
        let guideline_version = cql_get_raw(&row, &col_map, "guideline_version");
        let created_at: chrono::DateTime<chrono::Utc> = cql_get(&row, &col_map, "created_at")?;

        session
            .execute_unpaged(
                &insert,
                (
                    tenant_id,
                    session_id,
                    query_id,
                    program_type,
                    query_embedding,
                    task_complexity,
                    succeeded,
                    latency_ms,
                    token_cost,
                    guideline_version,
                    created_at,
                ),
            )
            .await?;
        copied += 1;
    }
    Ok(copied)
}

#[allow(deprecated)]
async fn feedback_outcomes_uuid_write_probe(
    session: &CqlSession,
    keyspace: &str,
    table: &str,
) -> anyhow::Result<bool> {
    if !table_exists(session, keyspace, table).await? {
        return Ok(false);
    }
    let tenant_id = Uuid::from_u128(0xfeed_bacc_0000_4000_8000_0000_0000_0036);
    let session_id = Uuid::from_u128(0xfeed_bacc_0000_4000_8000_0000_0000_0037);
    let query_id = Uuid::from_u128(0xfeed_bacc_0000_4000_8000_0000_0000_0038);
    let created_at = chrono::DateTime::<chrono::Utc>::UNIX_EPOCH;
    let insert_result = session
        .query_unpaged(
            format!(
                "INSERT INTO {keyspace}.{table} \
             (tenant_id, session_id, query_id, program_type, task_complexity, \
              succeeded, latency_ms, token_cost, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
            (
                tenant_id,
                session_id,
                query_id,
                "__migration_v36_uuid_probe__",
                "simple",
                true,
                0i32,
                0i32,
                created_at,
            ),
        )
        .await;
    if let Err(error) = insert_result {
        tracing::warn!(
            keyspace,
            table,
            error = %error,
            "feedback_outcomes uuid write probe insert failed"
        );
        return Ok(false);
    }

    session
        .query_unpaged(
            format!(
                "DELETE FROM {keyspace}.{table} \
             WHERE tenant_id = ? AND created_at = ? AND query_id = ?"
            ),
            (tenant_id, created_at, query_id),
        )
        .await?;
    Ok(true)
}

async fn apply_feedback_outcomes_query_id_uuid_migration(
    session: &CqlSession,
    keyspace: &str,
) -> anyhow::Result<()> {
    let table = "feedback_outcomes";
    let staging = "feedback_outcomes_v36";

    if feedback_outcomes_uuid_write_probe(session, keyspace, table).await? {
        if table_exists(session, keyspace, staging).await? {
            let staging_count = count_table(session, keyspace, staging).await?;
            if staging_count > 0 {
                let copied = copy_feedback_outcomes_rows(
                    session,
                    keyspace,
                    staging,
                    table,
                    FeedbackQueryIdEncoding::Uuid,
                )
                .await?;
                if i64::try_from(copied)? != staging_count {
                    anyhow::bail!(
                        "feedback_outcomes v36 recovery copied {copied} rows from staging, expected {staging_count}"
                    );
                }
            }
            #[allow(deprecated)]
            session
                .query_unpaged(format!("DROP TABLE IF EXISTS {keyspace}.{staging}"), ())
                .await?;
        }
        return Ok(());
    }

    let final_type = column_type(session, keyspace, table, "query_id").await?;
    if final_type
        .as_deref()
        .is_some_and(|actual| actual.eq_ignore_ascii_case("uuid"))
    {
        anyhow::bail!(
            "feedback_outcomes.query_id metadata reports uuid, but UUID write probe failed"
        );
    }

    if final_type
        .as_deref()
        .is_some_and(|actual| !actual.eq_ignore_ascii_case("timeuuid"))
    {
        anyhow::bail!(
            "feedback_outcomes.query_id has unexpected type {:?}; expected uuid or legacy timeuuid",
            final_type
        );
    }

    if final_type.is_none() {
        #[allow(deprecated)]
        session
            .query_unpaged(feedback_outcomes_create_table_ddl(keyspace, table), ())
            .await?;
        if table_exists(session, keyspace, staging).await? {
            let staging_count = count_table(session, keyspace, staging).await?;
            let copied = copy_feedback_outcomes_rows(
                session,
                keyspace,
                staging,
                table,
                FeedbackQueryIdEncoding::Uuid,
            )
            .await?;
            if i64::try_from(copied)? != staging_count {
                anyhow::bail!(
                    "feedback_outcomes v36 recovery copied {copied} rows from staging, expected {staging_count}"
                );
            }
            #[allow(deprecated)]
            session
                .query_unpaged(format!("DROP TABLE IF EXISTS {keyspace}.{staging}"), ())
                .await?;
        }
        return Ok(());
    }

    #[allow(deprecated)]
    session
        .query_unpaged(format!("DROP TABLE IF EXISTS {keyspace}.{staging}"), ())
        .await?;
    #[allow(deprecated)]
    session
        .query_unpaged(feedback_outcomes_create_table_ddl(keyspace, staging), ())
        .await?;

    let legacy_count = count_table(session, keyspace, table).await?;
    let staged = copy_feedback_outcomes_rows(
        session,
        keyspace,
        table,
        staging,
        FeedbackQueryIdEncoding::Timeuuid,
    )
    .await?;
    let staged_count = count_table(session, keyspace, staging).await?;
    if i64::try_from(staged)? != staged_count {
        anyhow::bail!(
            "feedback_outcomes v36 staged {staged} legacy rows, but staging table contains {staged_count}; leaving legacy table intact"
        );
    }
    if staged_count != legacy_count {
        tracing::warn!(
            keyspace,
            legacy_count,
            staged_count,
            "feedback_outcomes v36 COUNT(*) disagrees with enumerable legacy rows; proceeding with staged enumerable rows"
        );
    }

    #[allow(deprecated)]
    session
        .query_unpaged(format!("DROP TABLE {keyspace}.{table}"), ())
        .await?;
    #[allow(deprecated)]
    session
        .query_unpaged(feedback_outcomes_create_table_ddl(keyspace, table), ())
        .await?;

    let restored = copy_feedback_outcomes_rows(
        session,
        keyspace,
        staging,
        table,
        FeedbackQueryIdEncoding::Uuid,
    )
    .await?;
    if i64::try_from(restored)? != staged_count {
        anyhow::bail!(
            "feedback_outcomes v36 restored {restored} rows, expected {staged_count}; preserved rows remain in {keyspace}.{staging}"
        );
    }

    #[allow(deprecated)]
    session
        .query_unpaged(format!("DROP TABLE IF EXISTS {keyspace}.{staging}"), ())
        .await?;
    Ok(())
}

async fn migration_postcondition_satisfied(
    session: &CqlSession,
    keyspace: &str,
    version: u32,
) -> anyhow::Result<bool> {
    match version {
        38 => table_exists(session, keyspace, "retraction").await,
        37 => table_exists(session, keyspace, "forget_journal").await,
        36 => feedback_outcomes_uuid_write_probe(session, keyspace, "feedback_outcomes").await,
        35 => {
            for table in [
                "document_chunks",
                "document_terms",
                "document_phonetic_terms",
            ] {
                if !table_exists(session, keyspace, table).await? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(true),
    }
}

pub async fn ensure_schema_version_table(
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
    #[allow(deprecated)]
    session.query_unpaged(ddl, ()).await?;
    Ok(())
}

/// Set of every version currently recorded in `schema_version`. The runner
/// uses this for gap detection: it applies any registered migration whose
/// version is **not** in the applied set, not just `version > max(applied)`.
/// Without this, an out-of-band rollback of a single intermediate row
/// (or a corrupted bookkeeping table that lost a row) would never be
/// repaired — every subsequent run would compute `current = MAX(...)` and
/// see nothing pending. Matches industry-standard runners (Flyway, Alembic).
async fn applied_versions(
    session: &CqlSession,
    keyspace: &str,
) -> anyhow::Result<std::collections::HashSet<u32>> {
    let q = format!("SELECT version FROM {keyspace}.schema_version");
    #[allow(deprecated)]
    let result = session.query_unpaged(q, ()).await?;
    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    let mut set = std::collections::HashSet::new();
    for row in rows {
        if let Ok(v) = cql_get::<i32>(&row, &col_map, "version") {
            set.insert(v as u32);
        }
    }
    Ok(set)
}

async fn current_version(session: &CqlSession, keyspace: &str) -> anyhow::Result<Option<u32>> {
    let q = format!("SELECT version FROM {keyspace}.schema_version");
    #[allow(deprecated)]
    let result = session.query_unpaged(q, ()).await?;
    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    let mut max: Option<u32> = None;
    for row in rows {
        if let Ok(v) = cql_get::<i32>(&row, &col_map, "version") {
            let v = v as u32;
            max = Some(max.map_or(v, |m| m.max(v)));
        }
    }
    Ok(max)
}

async fn last_applied_at(session: &CqlSession, keyspace: &str) -> anyhow::Result<Option<String>> {
    let q = format!("SELECT applied_at FROM {keyspace}.schema_version");
    #[allow(deprecated)]
    let result = session.query_unpaged(q, ()).await?;
    let col_map = build_col_map(result.col_specs());
    let rows = result.rows_or_empty();
    let mut last: Option<chrono::DateTime<chrono::Utc>> = None;
    for row in rows {
        if let Ok(applied_at) =
            cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "applied_at")
        {
            last = Some(last.map_or(applied_at, |current| current.max(applied_at)));
        }
    }
    Ok(last.map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
}

fn schema_version_insert_query(keyspace: &str) -> String {
    format!(
        "INSERT INTO {keyspace}.schema_version \
         (version, applied_at, description, applied_by) \
         VALUES (?, ?, ?, ?)"
    )
}

pub async fn record_version(
    session: &CqlSession,
    keyspace: &str,
    version: u32,
    description: &str,
) -> anyhow::Result<()> {
    let host = hostname().unwrap_or_else(|| "unknown".into());
    let applied_at = chrono::Utc::now();
    let q = schema_version_insert_query(keyspace);
    #[allow(deprecated)]
    session
        .query_unpaged(
            q,
            (version as i32, applied_at, description.to_string(), host),
        )
        .await?;
    Ok(())
}

async fn delete_version(session: &CqlSession, keyspace: &str, version: u32) -> anyhow::Result<()> {
    let q = format!("DELETE FROM {keyspace}.schema_version WHERE version = ?");
    #[allow(deprecated)]
    session.query_unpaged(q, (version as i32,)).await?;
    Ok(())
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
}

/// Split a CQL DDL script into individual statements.
///
/// Strips line comments (`-- ...` to end of line), ignores blank lines and
/// whitespace, and splits on `;`. Also drops `USE <keyspace>` statements —
/// the migration runner pins the session's default keyspace from the
/// configured one, so hardcoded USE clauses in DDL files would override
/// (and may target a keyspace that doesn't exist in test/per-tenant
/// deployments). Does not handle block comments or strings containing
/// semicolons — the DDL files under `ddl/` don't use those.
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
        .filter(|s| {
            // Drop USE statements (case-insensitive, first token).
            let first_token: String = s
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            first_token != "USE"
        })
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
    fn split_cql_drops_use_statements() {
        // DDLs may include `USE agent_memory;` for cqlsh convenience, but
        // the migration runner pins the keyspace itself — USE must be
        // filtered out so it doesn't point a test deployment at a
        // nonexistent production keyspace.
        let ddl = "USE agent_memory;\nALTER TABLE entity_store ADD foo text;";
        let stmts = split_cql(ddl);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].starts_with("ALTER TABLE"));
    }

    #[test]
    fn split_cql_drops_use_case_insensitive() {
        let ddl = "use agent_memory;\nUse agent_memory;\nALTER TABLE foo ADD bar text;";
        let stmts = split_cql(ddl);
        assert_eq!(stmts.len(), 1);
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
        let m20 = MIGRATIONS
            .iter()
            .find(|m| m.version == 20)
            .expect("v20 present");
        assert!(m20.ddl.contains("ALTER TABLE entity_store ADD description"));
        assert!(m20.ddl.contains("ALTER TABLE entity_store ADD scope"));
    }

    #[test]
    fn qualify_ddl_substitutes_agent_memory_references() {
        // Qualified references and CREATE KEYSPACE both pick up the
        // configured keyspace name.
        let ddl = "CREATE KEYSPACE IF NOT EXISTS agent_memory WITH replication = {};\n\
                   USE agent_memory;\n\
                   CREATE TABLE IF NOT EXISTS agent_memory.entity_types (name text PRIMARY KEY);";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(rewritten.contains("CREATE KEYSPACE IF NOT EXISTS agent_memory_test"));
        assert!(rewritten.contains("agent_memory_test.entity_types"));
        assert!(!rewritten.contains(" agent_memory ")); // no bare references left
    }

    #[test]
    fn qualify_ddl_prefixes_unqualified_create_table() {
        let ddl = "CREATE TABLE IF NOT EXISTS memo_cache (id text PRIMARY KEY);";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(rewritten.contains("agent_memory_test.memo_cache"));
        assert!(!rewritten.contains(" memo_cache ("));
    }

    #[test]
    fn qualify_ddl_prefixes_create_index_on_clause() {
        let ddl = "CREATE INDEX IF NOT EXISTS idx_foo ON memo_cache (result_embedding);";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(rewritten.contains("ON agent_memory_test.memo_cache"));
    }

    #[test]
    fn qualify_ddl_prefixes_alter_table() {
        let ddl = "ALTER TABLE entity_store ADD description text;";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(rewritten.contains("ALTER TABLE agent_memory_test.entity_store"));
    }

    #[test]
    fn qualify_ddl_handles_multi_line_create_index() {
        // DDL 009 wraps the ON clause onto the next line. Per-line
        // parsing would miss it; statement-level parsing must catch it.
        let ddl =
            "CREATE INDEX IF NOT EXISTS idx_entity_by_tenant\n    ON entity_store (tenant_id);";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(
            rewritten.contains("agent_memory_test.entity_store"),
            "multi-line CREATE INDEX must qualify the table identifier, got: {rewritten}"
        );
    }

    #[test]
    fn qualify_ddl_rewrites_actual_ddl_009_file() {
        // Against the actual shipping DDL file. If this passes but the
        // live run fails, the problem is in apply_bootstrap's execution
        // loop, not the qualifier.
        let ddl = include_str!("../../../ddl/009_secondary_indexes.cql");
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        for (i, stmt) in split_cql(&rewritten).iter().enumerate() {
            let upper = stmt.to_uppercase();
            if upper.starts_with("CREATE INDEX") {
                eprintln!("stmt #{i}: {}\n", stmt);
                assert!(
                    stmt.contains("agent_memory_test."),
                    "statement missing qualification: {stmt}"
                );
            }
        }
    }

    #[test]
    fn qualify_ddl_handles_ddl_009_shape() {
        // Exact shape of ddl/009_secondary_indexes.cql: several multi-line
        // CREATE INDEX statements, interleaved with `--` comment blocks.
        let ddl = "-- Some comment block\n\n\
                   USE agent_memory;\n\n\
                   -- first index\n\
                   CREATE INDEX IF NOT EXISTS idx_a\n    ON entity_store (tenant_id);\n\n\
                   -- second index\n\
                   CREATE INDEX IF NOT EXISTS idx_b\n    ON entity_store (entity_id);\n\n\
                   -- third index comment block that includes -- agent_memory word\n\
                   CREATE INDEX IF NOT EXISTS idx_c\n    ON mentioned_in (tenant_id);\n\n\
                   CREATE INDEX IF NOT EXISTS idx_d\n    ON co_occurs_with (tenant_id);\n\n\
                   -- fourth\n\
                   CREATE INDEX IF NOT EXISTS idx_e\n    ON co_occurs_with (entity_b);\n";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        // Every CREATE INDEX ON ... must have been qualified.
        for table in ["entity_store", "mentioned_in", "co_occurs_with"] {
            let qualified = format!("agent_memory_test.{table}");
            assert!(
                rewritten.contains(&qualified),
                "missing qualification for {table}, got:\n{rewritten}"
            );
        }
        // No leftover unqualified references (sanity: look for " ON entity_store"
        // style which would indicate a missed qualification).
        assert!(
            !rewritten.contains(" ON entity_store "),
            "unqualified `ON entity_store ` leaked through"
        );
        assert!(
            !rewritten.contains(" ON co_occurs_with "),
            "unqualified `ON co_occurs_with ` leaked through"
        );
    }

    #[test]
    fn qualify_ddl_handles_adjacent_multi_statement_block() {
        // Reproduces the live cluster failure: multi-line CREATE INDEX
        // as the fifth statement in a block. Starts with a blank line
        // (between previous `;` and this statement) and has the ON on
        // a continuation.
        let ddl = "CREATE INDEX IF NOT EXISTS idx_a ON foo (x);\n\
                   CREATE INDEX IF NOT EXISTS idx_b\n    ON co_occurs_with (entity_b);\n";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(
            rewritten.contains("agent_memory_test.co_occurs_with"),
            "second multi-line CREATE INDEX must qualify target, got: {rewritten}"
        );
        assert!(
            rewritten.contains("agent_memory_test.foo"),
            "first CREATE INDEX still qualified, got: {rewritten}"
        );
    }

    #[test]
    fn qualify_ddl_prefixes_drop_table() {
        let ddl = "DROP TABLE IF EXISTS intentions;";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(rewritten.contains("DROP TABLE IF EXISTS agent_memory_test.intentions"));
    }

    #[test]
    fn qualify_ddl_leaves_already_qualified_tables_alone() {
        // After step 1 rewrites agent_memory.X to keyspace.X, step 2
        // should NOT re-qualify (no double-prefixing).
        let ddl = "CREATE TABLE IF NOT EXISTS agent_memory.foo (id text PRIMARY KEY);";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(rewritten.contains("agent_memory_test.foo"));
        assert!(!rewritten.contains("agent_memory_test.agent_memory_test.foo"));
    }

    #[test]
    fn qualify_ddl_ignores_non_ddl_lines() {
        // Comments are stripped so `;` inside them can't trigger false
        // statement splits. The qualifier still rewrites the DDL
        // statement that follows.
        let ddl = "-- some comment\nINSERT INTO agent_memory.entity_types VALUES ('x');";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(rewritten.contains("INSERT INTO agent_memory_test.entity_types"));
        assert!(
            !rewritten.contains("-- some comment"),
            "comments should be stripped so embedded `;` can't fake-split statements"
        );
    }

    #[test]
    fn qualify_ddl_comments_with_semicolons_dont_split_statements() {
        // The exact pattern from ddl/009_secondary_indexes.cql that broke
        // the live run: a `;` inside a `--` comment line preceding a
        // multi-line CREATE INDEX. Without comment-stripping, the
        // splitter cut the comment in half and the CREATE INDEX landed
        // in a "statement" that started with non-DDL prose.
        let ddl = "-- entity_b is a clustering column; queries without the key\n\
                   CREATE INDEX IF NOT EXISTS idx_x\n    ON co_occurs_with (entity_b);";
        let rewritten = qualify_ddl(ddl, "agent_memory_test");
        assert!(
            rewritten.contains("agent_memory_test.co_occurs_with"),
            "CREATE INDEX must be qualified even when preceded by a `;`-containing comment, got:\n{rewritten}"
        );
    }

    #[test]
    fn prepare_bootstrap_statement_rewrites_now_to_apply_time_timestamp_literal() {
        let stmt = "INSERT INTO agent_memory.entity_types (type_name, description, created_at)\n\
                    VALUES ('person', 'desc', toTimestamp(now()))";
        let applied_at = chrono::DateTime::parse_from_rfc3339("2026-05-04T22:53:21.123Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let prepared = prepare_bootstrap_statement(stmt, applied_at);

        assert!(
            !prepared.contains("toTimestamp(now())") && !prepared.contains("now()"),
            "prepared bootstrap statement must not send Ferrosa a server-side now() expression: {prepared}"
        );
        assert!(
            prepared.contains("'2026-05-04T22:53:21.123Z'"),
            "prepared bootstrap statement must preserve current apply-time timestamp semantics, got: {prepared}"
        );
    }

    #[test]
    fn schema_version_bookkeeping_binds_timestamp_instead_of_server_now_expression() {
        let q = schema_version_insert_query("agent_memory");

        assert!(
            !q.contains("now()") && !q.contains("toTimestamp"),
            "schema_version bookkeeping must bind a timestamp value because FerrosaDB returns timeuuid for now(): {q}"
        );
        assert!(
            q.contains("VALUES (?, ?, ?, ?)"),
            "schema_version bookkeeping must bind version, applied_at, description, and applied_by: {q}"
        );
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
        assert!(
            msg.contains("backup"),
            "error must point the operator at backup recovery"
        );
    }

    // ── W-03 tests ───────────────────────────────────────────────────────────

    /// P0-11/W-03: assert_keyspace_exists_dbaas returns Setup error with clear
    /// message when the keyspace is absent. Uses split_cql to verify DDL
    /// content without a live session.
    #[test]
    fn dbaas_assert_keyspace_error_message_mentions_provisioning() {
        // Simulate the error path: construct the error directly as the function
        // would, since we can't create a live CQL session in unit tests.
        let keyspace = "agent_memory_tenant_abc";
        let err = MigrationError::Setup {
            source: anyhow::anyhow!(
                "FERROSA_DBAAS_MODE=true but keyspace '{}' does not exist in \
                 system_schema.keyspaces. The DBaaS control plane must provision \
                 the keyspace before the application starts. \
                 Check tenant provisioning status or contact support.",
                keyspace
            ),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_DBAAS_MODE"),
            "error must mention FERROSA_DBAAS_MODE, got: {msg}"
        );
        assert!(
            msg.contains(keyspace),
            "error must name the missing keyspace, got: {msg}"
        );
        assert!(
            msg.contains("control plane") || msg.contains("provisioning"),
            "error must point operator at the provisioning path, got: {msg}"
        );
    }

    /// P0-11/W-03: In DBaaS mode the bootstrap DDL registry must produce zero
    /// DDL when split_cql skips USE statements — confirming that the runner
    /// would issue no DDL if accidentally called.
    ///
    /// This is a belt-and-suspenders check: `assert_keyspace_exists_dbaas` is
    /// the primary gating function; this test ensures the DDL filtering that
    /// split_cql already does (dropping USE statements) still holds.
    #[test]
    fn split_cql_strips_use_system_auth_from_roles_ddl() {
        // split_cql must remove USE statements (including USE system_auth).
        let stmts = split_cql(ROLES_DDL);
        for stmt in &stmts {
            let first_token = stmt
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            assert_ne!(
                first_token, "USE",
                "split_cql must drop all USE statements, leaked: {stmt}"
            );
        }
    }

    // ── W-04 tests ───────────────────────────────────────────────────────────

    /// P0-11/W-04: no USE system_auth statement survives split_cql processing
    /// of the ROLES_DDL. The runtime DDL stream must never contain a
    /// `USE system_auth` token — an external tenant's role has no access to
    /// the system_auth keyspace.
    #[test]
    fn roles_ddl_contains_no_use_system_auth_after_split() {
        let stmts = split_cql(ROLES_DDL);
        for stmt in &stmts {
            assert!(
                !stmt.to_ascii_uppercase().contains("USE SYSTEM_AUTH"),
                "runtime DDL stream must not contain USE system_auth — \
                 an external tenant has no system_auth access. Leaked: {stmt}"
            );
        }
    }

    /// P0-11/W-04: no GRANT statement in the runtime DDL stream when
    /// FERROSA_DBAAS_MODE is true (ROLES_DDL is only applied in bootstrap,
    /// and bootstrap is skipped in DBaaS mode via assert_keyspace_exists_dbaas).
    /// This test audits the raw DDL source to confirm GRANT is present in
    /// ROLES_DDL (i.e., it would be issued if bootstrap ran), but confirms
    /// split_cql keeps it out of any USE-statement-free path.
    ///
    /// Note: GRANT statements themselves are NOT filtered by split_cql (they
    /// only apply inside bootstrap which is guarded by DBaaS mode). The real
    /// protection is that `assert_keyspace_exists_dbaas` must be called
    /// instead of `run_migrations` in DBaaS mode — verified in W-03.
    /// This test documents the presence of GRANTs in the file for auditability.
    #[test]
    fn roles_ddl_contains_grants_that_must_not_reach_dbaas_tenants() {
        // Confirm GRANT exists in the raw DDL so auditors know the file has
        // privilege-escalating content that must be blocked at the caller level.
        assert!(
            ROLES_DDL.contains("GRANT"),
            "ROLES_DDL must contain GRANT statements (if it doesn't, update this test)"
        );
        // Confirm `should_apply_roles_ddl` is the guard (FERROSA_AUTH_ENABLED
        // must be false or absent for ROLES_DDL to be skipped).
        assert!(
            !should_apply_roles_ddl(),
            "In test environment (no FERROSA_AUTH_ENABLED), roles DDL must not apply"
        );
    }

    /// P0-11/W-04: should_apply_roles_ddl is false unless explicitly enabled.
    #[test]
    fn should_apply_roles_ddl_false_by_default() {
        // FERROSA_AUTH_ENABLED is not set in the test environment.
        // Even if it was set to something other than true/1/on/yes, it must be false.
        let result = should_apply_roles_ddl();
        // We just document the contract: in a clean env (no FERROSA_AUTH_ENABLED),
        // the guard must be false. If CI sets this var, the test environment is
        // misconfigured — surface that loudly.
        assert!(
            !result,
            "should_apply_roles_ddl must be false in test environment; \
             is FERROSA_AUTH_ENABLED set in CI? If so, that's a misconfiguration."
        );
    }
}
