//! Migration regression tests for schema drift fixes.
//!
//! Validates that additive migrations apply cleanly against a schema that
//! is missing the new column, and that the migration registry stays
//! append-only.
//!
//! Run:
//!   scripts/start-test-cluster.sh
//!   export $(scripts/start-test-cluster.sh --env)
//!   cargo test -p ferrosa-memory-core --test migration_drift -- --ignored --nocapture

#![allow(deprecated)]

use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::connect_session;
use ferrosa_memory_core::migration::{
    BOOTSTRAP_DDLS, MIGRATIONS, Migration, PRE_VERSIONING_BASELINE, ROLES_DDL, run_migrations,
};
use ferrosa_memory_core::test_cluster::TestClusterConfig;

fn trajectory_folds_ddl_columns_are_uuid_compatible(ddl: &str) -> bool {
    let ddl_lower = ddl.to_lowercase();
    ddl_lower.contains("trajectory_folds")
        && ddl_lower.contains("fold_id          uuid")
        && ddl_lower.contains("parent_fold_id   uuid")
        && !ddl_lower.contains("fold_id          timeuuid")
        && !ddl_lower.contains("parent_fold_id   timeuuid")
}

fn feedback_outcomes_ddl_query_id_is_uuid(ddl: &str) -> bool {
    let ddl_lower = ddl.to_lowercase();
    ddl_lower.contains("feedback_outcomes")
        && ddl_lower.contains("query_id")
        && ddl_lower.contains("query_id         uuid")
        && !ddl_lower.contains("query_id         timeuuid")
}

fn test_cfg(test: &TestClusterConfig) -> FerrosaCqlConfig {
    FerrosaCqlConfig {
        contact_points: vec![test.contact_point()],
        keyspace: test.keyspace.clone(),
        replication_factor: 1,
        consistency: "ONE".into(),
        username: "ferrosa_user".into(),
        password: "ferrosa_user".into(),
        admin_username: None,
        admin_password: None,
    }
}

/// Serializes the live migration tests. They share one `agent_memory_test`
/// keyspace and DROP/CREATE tables + rewrite `schema_version` to simulate
/// partial-migration recovery, so running concurrently lets one test's schema
/// changes break another's migration-count assertions. Under libtest
/// (`make test-live`, thread-parallel) this in-process mutex enforces serial
/// execution; under nextest (process-per-test) the `migration-drift` test-group
/// in `.config/nextest.toml` does. Both are needed — the mutex can't serialize
/// across nextest's separate processes, and the test-group doesn't apply to
/// libtest.
fn live_migration_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

// ---------------------------------------------------------------------------
// T-01: Registry invariants — monotonic versions, no gaps, append-only
// ---------------------------------------------------------------------------

/// T-01: MIGRATIONS array is monotonically increasing and every version
/// is strictly greater than the pre-versioning baseline.
#[test]
fn t01_registry_monotonic_and_above_baseline() {
    let mut last = PRE_VERSIONING_BASELINE;
    for m in MIGRATIONS {
        assert!(
            m.version > last,
            "migration version {} must be strictly greater than previous {}",
            m.version,
            last
        );
        last = m.version;
    }
    // The top version must be >= 31 (first_seen fix).
    assert!(
        last >= 31,
        "expected at least migration 31 (co_occurs_with.first_seen fix), got {}",
        last
    );
}

/// Every versioned DDL checked into a release must be present in the runtime
/// registry. Otherwise CI can build and ship a file that startup never applies.
#[test]
fn t01b_every_versioned_ddl_is_registered() {
    let ddl_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ddl");
    let registered: std::collections::BTreeSet<u32> = MIGRATIONS
        .iter()
        .map(|migration| migration.version)
        .collect();

    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&ddl_dir).expect("ddl directory must be readable in CI") {
        let path = entry.expect("ddl directory entry").path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((prefix, _)) = file_name.split_once('_') else {
            continue;
        };
        let Ok(version) = prefix.parse::<u32>() else {
            continue;
        };
        if (PRE_VERSIONING_BASELINE < version && version < 100) && !registered.contains(&version) {
            missing.push(file_name.to_string());
        }
    }

    assert!(
        missing.is_empty(),
        "versioned DDL files are not registered and therefore cannot be applied at startup: {missing:?}"
    );
}

// ---------------------------------------------------------------------------
// T-02: Migration 31 DDL is additive (ALTER TABLE ADD) and non-destructive
// ---------------------------------------------------------------------------

/// T-02: Migration 31 contains an ALTER TABLE ADD for first_seen.
#[test]
fn t02_migration_31_is_additive() {
    let m31: &Migration = MIGRATIONS
        .iter()
        .find(|m| m.version == 31)
        .expect("migration 31 must exist in the registry");

    let ddl_lower = m31.ddl.to_lowercase();
    assert!(
        ddl_lower.contains("alter table")
            && ddl_lower.contains("add")
            && ddl_lower.contains("first_seen"),
        "migration 31 DDL must be an ALTER TABLE ADD for first_seen. Got:\n{}",
        m31.ddl
    );
    assert!(
        !ddl_lower.contains("drop"),
        "migration 31 must not contain DROP (destructive). Got:\n{}",
        m31.ddl
    );
}

// ---------------------------------------------------------------------------
// T-03: trajectory_folds UUID compatibility for Rust uuid::Uuid bindings
// ---------------------------------------------------------------------------

/// T-03: Greenfield bootstrap DDL must create trajectory_folds with UUID fold
/// identifiers because storage binds Rust `uuid::Uuid`, not a CQL timeuuid.
#[test]
fn t03_bootstrap_trajectory_folds_uses_uuid_fold_ids() {
    let bootstrap_ddl = include_str!("../../../ddl/002_folds_entities.cql");
    assert!(
        trajectory_folds_ddl_columns_are_uuid_compatible(bootstrap_ddl),
        "trajectory_folds bootstrap DDL must use uuid for fold_id and parent_fold_id, not timeuuid. Got:\n{}",
        bootstrap_ddl
    );
}

/// T-04: Existing keyspaces need an explicit migration that recreates the empty
/// trajectory_folds table with UUID-compatible fold identifiers.
#[test]
fn t04_migration_33_recreates_trajectory_folds_with_uuid_fold_ids() {
    let m33: &Migration = MIGRATIONS
        .iter()
        .find(|m| m.version == 33)
        .expect("migration 33 must exist for trajectory_folds uuid fold ids");

    assert!(
        trajectory_folds_ddl_columns_are_uuid_compatible(m33.ddl),
        "migration 33 DDL must recreate trajectory_folds with uuid fold_id and parent_fold_id. Got:\n{}",
        m33.ddl
    );
    let ddl_lower = m33.ddl.to_lowercase();
    assert!(
        ddl_lower.contains("drop table if exists trajectory_folds")
            && ddl_lower.contains("create table trajectory_folds"),
        "migration 33 must drop/recreate trajectory_folds because CQL cannot alter primary-key types. Got:\n{}",
        m33.ddl
    );
}

// ---------------------------------------------------------------------------
// T-05: feedback_outcomes UUID compatibility for record_outcome query IDs
// ---------------------------------------------------------------------------

/// T-05: Greenfield bootstrap DDL must create feedback_outcomes.query_id as
/// UUID because record_outcome accepts arbitrary UUID query identifiers, not
/// CQL timeuuid values.
#[test]
fn t05_bootstrap_feedback_outcomes_query_id_uses_uuid() {
    let bootstrap_ddl = include_str!("../../../ddl/002_folds_entities.cql");
    assert!(
        feedback_outcomes_ddl_query_id_is_uuid(bootstrap_ddl),
        "feedback_outcomes bootstrap DDL must use uuid for query_id, not timeuuid. Got:\n{}",
        bootstrap_ddl
    );
}

/// T-06: Existing keyspaces need an explicit data-preserving migration that
/// converts legacy timeuuid query identifiers into UUID-compatible rows.
#[test]
fn t06_migration_36_is_data_preserving_feedback_outcomes_query_id_repair() {
    let m36: &Migration = MIGRATIONS
        .iter()
        .find(|m| m.version == 36)
        .expect("migration 36 must exist for feedback_outcomes uuid query_id");

    let ddl_lower = m36.ddl.to_lowercase();
    assert!(
        ddl_lower.contains("custom rust migration")
            && ddl_lower.contains("staging table")
            && ddl_lower.contains("copies legacy rows")
            && ddl_lower.contains("verifies counts"),
        "migration 36 must document the custom staging/copy/swap data-preserving path. Got:\n{}",
        m36.ddl
    );
    assert!(
        !ddl_lower.contains("drop table if exists feedback_outcomes;"),
        "migration 36 must not be a destructive DROP-only DDL; legacy telemetry must be copied. Got:\n{}",
        m36.ddl
    );
}

// ---------------------------------------------------------------------------
// T-07: Old-schema keyspace (v30, missing first_seen) auto-upgrades to v31
// ---------------------------------------------------------------------------

/// T-07: Simulate a keyspace at version 30 (pre-first_seen fix), run
/// migrations, and assert the column exists and the graph MERGE path
/// no longer errors on "unknown property 'first_seen'".
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t03_old_schema_auto_upgrades_to_v31() {
    let _serial = live_migration_test_lock().lock().await;
    let test = TestClusterConfig::from_env().expect(
        "FERROSA_TEST_CQL_PORT must be set. Start a test cluster with:\n\
         \t  scripts/start-test-cluster.sh\n\
         then re-run with --ignored --nocapture",
    );

    let cfg = test_cfg(&test);
    let session = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect_session must succeed against test cluster");

    // Step 1: Ensure the keyspace exists and is at baseline.
    run_migrations(&session, &cfg.keyspace)
        .await
        .expect("initial bootstrap must succeed");

    // Step 2: Roll back `co_occurs_with` to pre-v31 state by removing
    // the `first_seen` column if it exists. Scylla supports DROP COLUMN.
    let rollback_stmt = format!(
        "ALTER TABLE {}.co_occurs_with DROP first_seen IF EXISTS",
        cfg.keyspace
    );
    let _ = session.query_unpaged(rollback_stmt, ()).await; // may fail if column absent; that's fine

    // Step 3: Rewind schema_version to 30 to simulate an old deploy.
    let rewind_stmt = format!(
        "UPDATE {}.schema_version SET applied_at = toTimestamp(now()), description = 'rewind to v30' WHERE version = 30",
        cfg.keyspace
    );
    session
        .query_unpaged(rewind_stmt, ())
        .await
        .expect("rewind schema_version to 30");

    // Step 4: Delete the v31 row so the runner sees v31 as pending.
    let delete_v31 = format!(
        "DELETE FROM {}.schema_version WHERE version = 31",
        cfg.keyspace
    );
    session
        .query_unpaged(delete_v31, ())
        .await
        .expect("delete v31 record");

    // Step 5: Run migrations again — should detect v31 pending and apply it.
    let applied = run_migrations(&session, &cfg.keyspace)
        .await
        .expect("migration re-run must succeed after rewind");
    assert_eq!(
        applied, 1,
        "exactly one migration (v31) should apply after rewind"
    );

    // Step 6: Verify the column now exists by describing the table.
    let describe = format!(
        "SELECT column_name FROM system_schema.columns WHERE keyspace_name = '{}' AND table_name = 'co_occurs_with' AND column_name = 'first_seen'",
        cfg.keyspace
    );
    let result = session
        .query_unpaged(describe, ())
        .await
        .expect("describe query must succeed");

    let col_map = ferrosa_memory_core::cql_storage::build_col_map(result.col_specs());
    let rows = result.rows_or_empty();

    let mut found = false;
    for row in rows.iter() {
        let name =
            ferrosa_memory_core::cql_storage::cql_get::<String>(row, &col_map, "column_name")
                .expect("column_name");
        if name == "first_seen" {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "first_seen column must exist after migration 31 applies"
    );

    // Step 7: Verify the graph MERGE path no longer errors. We can't
    // run a full MERGE here (needs entities), but we can verify the
    // CQL table structure is compatible by checking the schema agrees.
    session
        .await_schema_agreement()
        .await
        .expect("schema must agree across nodes after migration");
}

// ---------------------------------------------------------------------------
// T-08: Migration 42 (typed_edges backfill) is additive and non-destructive
// ---------------------------------------------------------------------------

/// T-08: Migration 42 re-creates typed_edges idempotently for installs whose
/// ≤19 baseline predated ddl/017. Must be CREATE TABLE IF NOT EXISTS, never DROP.
#[test]
fn t08_migration_42_typed_edges_backfill_is_additive() {
    let m42: &Migration = MIGRATIONS
        .iter()
        .find(|m| m.version == 42)
        .expect("migration 42 must exist in the registry");

    let ddl_lower = m42.ddl.to_lowercase();
    assert!(
        ddl_lower.contains("create table if not exists") && ddl_lower.contains("typed_edges"),
        "migration 42 must CREATE TABLE IF NOT EXISTS typed_edges. Got:\n{}",
        m42.ddl
    );
    assert!(
        !ddl_lower.contains("drop"),
        "migration 42 must not contain DROP (destructive). Got:\n{}",
        m42.ddl
    );
}

// ---------------------------------------------------------------------------
// T-09: ferrosa_user grant coverage for the core write-path tables
// ---------------------------------------------------------------------------

/// T-09: Regression guard for the entity_store grant loss (writes silently fail
/// when ferrosa_user lacks MODIFY). Asserts ROLES_DDL grants MODIFY to
/// ferrosa_user on entity_store and the documented application-writable set, so
/// a refactor can't silently drop a grant the ingest path depends on.
#[test]
fn t09_roles_ddl_grants_modify_on_core_write_path_tables() {
    let ddl = ROLES_DDL.to_lowercase();
    // entity_store is the table from the reported incident; the rest are the
    // always-on write targets exercised by ingest / forget / warmth.
    let required = [
        "entity_store",
        "tool_usage_log",
        "temporal_events",
        "feedback_outcomes",
        "intentions",
        "memo_cache",
        "plan_state",
        "trajectory_folds",
        "audit_log",
        "entity_warmth",
        "retraction",
        "forget_journal",
    ];
    for table in required {
        let needle = format!("grant modify on agent_memory.{table}");
        let granted = ddl
            .lines()
            .any(|l| l.contains(&needle) && l.contains("to ferrosa_user"));
        assert!(
            granted,
            "ROLES_DDL must `GRANT MODIFY ON agent_memory.{table} TO ferrosa_user` \
             — a missing grant makes writes silently fail (see ddl/100_roles.cql)"
        );
    }
}

// ---------------------------------------------------------------------------
// T-10/T-11: Live partial-migration recovery (entity_warmth + typed_edges)
// ---------------------------------------------------------------------------

/// True if `keyspace.table.column` exists in system_schema.
async fn live_column_exists(
    session: &ferrosa_memory_core::cql_storage::CqlSession,
    keyspace: &str,
    table: &str,
    column: &str,
) -> bool {
    let q = format!(
        "SELECT column_name FROM system_schema.columns WHERE keyspace_name = '{keyspace}' \
         AND table_name = '{table}' AND column_name = '{column}'"
    );
    #[allow(deprecated)]
    let result = session
        .query_unpaged(q, ())
        .await
        .expect("system_schema.columns query");
    !result.rows_or_empty().is_empty()
}

/// True if `keyspace.table` exists in system_schema.
async fn live_table_exists(
    session: &ferrosa_memory_core::cql_storage::CqlSession,
    keyspace: &str,
    table: &str,
) -> bool {
    let q = format!(
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = '{keyspace}' \
         AND table_name = '{table}'"
    );
    #[allow(deprecated)]
    let result = session
        .query_unpaged(q, ())
        .await
        .expect("system_schema.tables query");
    !result.rows_or_empty().is_empty()
}

/// True if the schema_version ledger has a row for `version`.
async fn live_version_recorded(
    session: &ferrosa_memory_core::cql_storage::CqlSession,
    keyspace: &str,
    version: u32,
) -> bool {
    let q = format!("SELECT version FROM {keyspace}.schema_version WHERE version = {version}");
    #[allow(deprecated)]
    let result = session
        .query_unpaged(q, ())
        .await
        .expect("schema_version query");
    !result.rows_or_empty().is_empty()
}

/// T-10: Reproduce the reported partial-run failure (stuck at v24 because
/// migration 25's `ALTER entity_warmth ADD reputation` hit "table not found").
/// Drop entity_warmth entirely and delete the v25 ledger row, then re-run: the
/// runner's version-25 special case must re-create the table, (re)apply
/// reputation, and record v25.
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t10_partial_migration_recovers_missing_entity_warmth() {
    let _serial = live_migration_test_lock().lock().await;
    let test = TestClusterConfig::from_env()
        .expect("FERROSA_TEST_CQL_PORT must be set; start a test cluster first");
    let cfg = test_cfg(&test);
    let session = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect_session must succeed against test cluster");

    run_migrations(&session, &cfg.keyspace)
        .await
        .expect("initial migration must succeed");

    // Simulate the broken adopted install: the ≤19 baseline never created
    // entity_warmth. Drop it AND its v25 ledger row so v25 is pending again
    // with the prerequisite table absent — exactly the stuck-at-v24 shape.
    session
        .query_unpaged(
            format!("DROP TABLE IF EXISTS {}.entity_warmth", cfg.keyspace),
            (),
        )
        .await
        .expect("drop entity_warmth");
    session
        .query_unpaged(
            format!(
                "DELETE FROM {}.schema_version WHERE version = 25",
                cfg.keyspace
            ),
            (),
        )
        .await
        .expect("delete v25 ledger row");

    run_migrations(&session, &cfg.keyspace)
        .await
        .expect("migration re-run must recover the missing entity_warmth table");

    assert!(
        live_column_exists(&session, &cfg.keyspace, "entity_warmth", "reputation").await,
        "entity_warmth.reputation must exist after recovery"
    );
    assert!(
        live_version_recorded(&session, &cfg.keyspace, 25).await,
        "schema_version must record v25 after recovery"
    );
}

/// T-11: Reproduce the typed_edges baseline gap (an install adopted at baseline
/// 19 before ddl/017 existed never created typed_edges, so create_edge fails).
/// Drop typed_edges and delete the v42 ledger row, then re-run: migration 42
/// must re-create it idempotently and record v42.
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t11_baseline_gap_backfills_missing_typed_edges() {
    let _serial = live_migration_test_lock().lock().await;
    let test = TestClusterConfig::from_env()
        .expect("FERROSA_TEST_CQL_PORT must be set; start a test cluster first");
    let cfg = test_cfg(&test);
    let session = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect_session must succeed against test cluster");

    run_migrations(&session, &cfg.keyspace)
        .await
        .expect("initial migration must succeed");

    session
        .query_unpaged(
            format!("DROP TABLE IF EXISTS {}.typed_edges", cfg.keyspace),
            (),
        )
        .await
        .expect("drop typed_edges");
    session
        .query_unpaged(
            format!(
                "DELETE FROM {}.schema_version WHERE version = 42",
                cfg.keyspace
            ),
            (),
        )
        .await
        .expect("delete v42 ledger row");

    let applied = run_migrations(&session, &cfg.keyspace)
        .await
        .expect("migration re-run must backfill typed_edges");
    assert!(applied >= 1, "at least migration 42 should re-apply");

    assert!(
        live_table_exists(&session, &cfg.keyspace, "typed_edges").await,
        "typed_edges must exist after backfill"
    );
    assert!(
        live_version_recorded(&session, &cfg.keyspace, 42).await,
        "schema_version must record v42 after backfill"
    );
}

// ---------------------------------------------------------------------------
// T-12: Ledger/schema postcondition recovery
// ---------------------------------------------------------------------------

/// The incident shape: a previous deployment recorded v39 even though one of
/// its tables never reached the cluster.  A ledger-only runner declares this
/// healthy forever.  The runner must invalidate that row, replay the
/// idempotent migration, and restore the missing table without an operator
/// editing `schema_version` by hand.
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t12_recorded_migration_repairs_missing_schema_postcondition() {
    let _serial = live_migration_test_lock().lock().await;
    let test = TestClusterConfig::from_env()
        .expect("FERROSA_TEST_CQL_PORT must be set; start a test cluster first");
    let cfg = test_cfg(&test);
    let session = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect_session must succeed against test cluster");

    run_migrations(&session, &cfg.keyspace)
        .await
        .expect("initial migration must succeed");
    assert!(
        live_version_recorded(&session, &cfg.keyspace, 39).await,
        "initial migration must record v39"
    );

    // Intentionally retain the ledger row: this reproduces the false-success
    // condition from the deploy failure rather than the easy missing-row path.
    session
        .query_unpaged(
            format!(
                "DROP TABLE IF EXISTS {}.session_task_focus_stack",
                cfg.keyspace
            ),
            (),
        )
        .await
        .expect("drop one v39 table while retaining its schema_version row");

    let applied = run_migrations(&session, &cfg.keyspace)
        .await
        .expect("recorded migration with missing table must repair itself");
    assert!(
        applied >= 1,
        "v39 must be reapplied after postcondition drift"
    );
    assert!(
        live_table_exists(&session, &cfg.keyspace, "session_task_focus_stack").await,
        "v39 postcondition must be restored"
    );
    assert!(
        live_version_recorded(&session, &cfg.keyspace, 39).await,
        "the repaired migration must be recorded again"
    );
}

// ---------------------------------------------------------------------------
// T-13: Exhaustive grant coverage — every created app table is granted or exempt
// ---------------------------------------------------------------------------

/// Extract `CREATE TABLE [IF NOT EXISTS] [keyspace.]<name>` table names from a
/// DDL blob, lowercased and unqualified.
fn created_table_names(ddl: &str) -> Vec<String> {
    let lower = ddl.to_lowercase();
    let mut names = Vec::new();
    for (i, _) in lower.match_indices("create table") {
        let rest = lower[i + "create table".len()..].trim_start();
        let rest = rest
            .strip_prefix("if not exists")
            .unwrap_or(rest)
            .trim_start();
        let tok: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '(')
            .collect();
        let name = tok.rsplit('.').next().unwrap_or(&tok).trim().to_string();
        if !name.is_empty() {
            names.push(name);
        }
    }
    names
}

/// T-12: every table CREATEd by the schema must EITHER be `GRANT MODIFY … TO
/// ferrosa_user` in ROLES_DDL OR be in an explicit exempt list. This turns a
/// forgotten grant on a newly-added app table into a CI failure — the root
/// cause behind the entity_store grant incident (writes silently fail under
/// auth when the runtime role lacks MODIFY).
#[test]
fn t12_every_created_table_is_granted_or_explicitly_exempt() {
    // Graph-owned: writes go through GraphClient as ferrosa_admin, never via
    // direct ferrosa_user CQL — intentionally ungranted.
    const GRAPH_OWNED: &[&str] = &[
        "typed_edges",
        "folded_into",
        "mentioned_in",
        "co_occurs_with",
        "supersedes",
        "derived_edges_by_pred",
        "derived_edges_by_src",
    ];
    // No serving-path ferrosa_user write today: migration bookkeeping (admin
    // session), genuinely unused tables, or write paths stubbed for the "B10"
    // sprint. If a runtime write path is added, add a GRANT and drop from here.
    const NO_RUNTIME_WRITE: &[&str] = &[
        "schema_version",
        "contradictions",
        "consolidation_history",
        "consolidation_queue",
        "domain_schemas",
        "entity_retrieval_counts",
        "promoted_predicates",
        "routing_guidelines",
        "query_heat_by_predicate_day",
        "compute_cost_by_predicate_day",
    ];

    let roles = ROLES_DDL.to_lowercase();
    let granted = |t: &str| {
        let needle = format!("grant modify on agent_memory.{t}");
        roles
            .lines()
            .any(|l| l.contains(&needle) && l.contains("to ferrosa_user"))
    };

    let mut tables: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for ddl in BOOTSTRAP_DDLS {
        tables.extend(created_table_names(ddl));
    }
    for m in MIGRATIONS {
        tables.extend(created_table_names(m.ddl));
    }

    let unexpected: Vec<&String> = tables
        .iter()
        .filter(|t| {
            !granted(t)
                && !GRAPH_OWNED.contains(&t.as_str())
                && !NO_RUNTIME_WRITE.contains(&t.as_str())
        })
        .collect();

    assert!(
        unexpected.is_empty(),
        "these CREATEd tables are neither GRANTed MODIFY to ferrosa_user nor explicitly \
         exempt (graph-owned / no-runtime-write). Add a GRANT to ddl/100_roles.cql or an \
         entry to the exempt lists: {unexpected:?}"
    );
}
