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
    BOOTSTRAP_DDLS, MIGRATIONS, Migration, PRE_VERSIONING_BASELINE, ROLES_DDL, migration_status,
    run_migrations,
};
use ferrosa_memory_core::test_cluster::TestClusterConfig;
use uuid::Uuid;

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
        tls_ca_path: None,
        tls_skip_hostname_verify: false,
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

/// Ferrosa accepts CQL keyspace names up to 48 characters. Keep the per-run
/// keyspace unique without generating a name that the cluster rejects before
/// migration 0 can start.
fn fresh_migration_keyspace() -> String {
    format!("migration_{}", Uuid::new_v4().simple())
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

#[test]
fn fresh_migration_keyspace_is_within_cql_identifier_limit() {
    let keyspace = fresh_migration_keyspace();
    assert!(
        keyspace.len() <= 48,
        "generated keyspace exceeds Ferrosa's 48-character limit: {keyspace}"
    );
    assert!(
        keyspace
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_'),
        "generated keyspace contains an invalid CQL identifier character: {keyspace}"
    );
}

/// Every test binary that runs migrations against the SHARED test keyspace
/// must be serialized by the same nextest test-group.
///
/// The in-process `live_migration_test_lock` cannot serialize across
/// processes, and nextest runs one process PER TEST — so the only thing
/// keeping these apart under nextest is the `migration-drift` test-group in
/// `.config/nextest.toml`. That group was scoped to `binary(migration_drift)`
/// alone, while `launch_gates_g3_g4` and `skill_e2e_live` also call
/// `run_migrations` against `test.keyspace` (the shared `agent_memory_test`).
/// Nothing serialized them against migration_drift, so they ran concurrently.
///
/// That is a real race, not a flaky test. t03 deletes the v31 `schema_version`
/// row and asserts its own `run_migrations` re-applies exactly one migration.
/// A concurrent `run_migrations` from another binary re-applies v31 first, so
/// t03's call finds nothing pending and returns 0:
///
///     migration_drift.rs:317
///     assertion `left == right` failed:
///       exactly one migration (v31) should apply after rewind
///       left: 0   right: 1
///
/// Observed on CI run 32606574972 (PR #227), which was a one-line version
/// bump and could not itself have caused it.
///
/// This test fails the moment a new test binary starts running migrations
/// without being added to the group, which is how the gap appeared.
#[test]
fn every_migration_running_binary_is_serialized_by_the_nextest_group() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut migration_runners: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).expect("tests dir must be readable") {
        let path = entry.expect("readable dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("test source must be readable");
        // A call site, not a mention in prose.
        if body.contains("run_migrations(") {
            migration_runners.push(
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .expect("test file stem")
                    .to_string(),
            );
        }
    }
    migration_runners.sort();
    assert!(
        !migration_runners.is_empty(),
        "scan found no migration-running test binaries — the detector is broken, \
         not the config"
    );

    let config_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.config/nextest.toml");
    let config = std::fs::read_to_string(&config_path).unwrap_or_else(|e| {
        panic!(
            "nextest config must be readable at {}: {e}",
            config_path.display()
        )
    });

    let missing: Vec<&String> = migration_runners
        .iter()
        .filter(|binary| !config.contains(&format!("binary({binary})")))
        .collect();

    assert!(
        missing.is_empty(),
        "these test binaries call run_migrations against the shared keyspace but \
         are NOT in the migration-drift nextest test-group, so nextest runs them \
         concurrently with migration_drift and they corrupt each other's \
         schema_version state: {missing:?}\n\
         Add them to the test-group filter in .config/nextest.toml."
    );
}

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
    // the `first_seen` column. The bootstrap above guarantees it exists;
    // fail loudly if the test fixture cannot establish the required state.
    // Ferrosa's ALTER TABLE grammar accepts `DROP <column>`, not Cassandra's
    // optional `IF EXISTS` suffix.
    let rollback_stmt = format!(
        "ALTER TABLE {}.co_occurs_with DROP first_seen",
        cfg.keyspace
    );
    session
        .query_unpaged(rollback_stmt, ())
        .await
        .expect("rollback must remove first_seen before replaying migration 31");

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
// T-10: Fresh-keyspace release gate — execute and verify the full registry
// ---------------------------------------------------------------------------

/// A fully migrated keyspace must STAY migrated across a database restart.
///
/// Reproduces the clean-install failure seen on the Tahoe QA VM: after the
/// installer hands the daemons to launchd — which restarts ferrosa —
/// ferrosa-memory finds the schema ledger back at v34 and fails migration 35's
/// postcondition forever, while every object migration 35 creates is present in
/// the node's own persisted schema.json (document_chunks, document_terms,
/// document_phonetic_terms and all three indexes). The objects exist on disk,
/// and `system_schema`, queried over CQL, disagrees.
///
/// This is why the fresh-keyspace gate below passes while the install still
/// breaks: that gate never restarts the database. Everything before the restart
/// is identical.
///
/// TWO-PHASE, run by hand around a restart:
///
///   export FERROSA_TEST_CQL_PORT=... FERROSA_TEST_KEYSPACE=restart_repro
///   cargo test -p ferrosa-memory-core --test migration_drift \
///     migrations_survive_a_database_restart -- --ignored --nocapture
///   # restart the ferrosa node, wait for /readyz 200
///   # run the same command again — the second run is the assertion
///
/// Phase one migrates and must apply the full registry. Phase two must be a
/// no-op with the ledger still at the newest version. A postcondition failure
/// or a regressed `db_version` on the second run IS the bug.
#[tokio::test]
#[ignore = "requires live cluster AND a manual restart between runs; see doc comment"]
async fn migrations_survive_a_database_restart() {
    let test = TestClusterConfig::from_env()
        .expect("FERROSA_TEST_CQL_PORT must be set; start a cluster first");
    let cfg = test_cfg(&test);
    let admin = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect to cluster");
    let keyspace = test.keyspace.clone();

    let applied = run_migrations(&admin, &keyspace)
        .await
        .unwrap_or_else(|error| panic!("migrations failed against {keyspace}: {error}"));

    let status = migration_status(&admin, &keyspace)
        .await
        .expect("schema status query");
    let newest = MIGRATIONS
        .last()
        .expect("migration registry must not be empty")
        .version;

    eprintln!(
        "keyspace={keyspace} applied={applied} db_version={} newest={newest}",
        status.db_version
    );

    assert_eq!(
        status.db_version, newest,
        "the ledger must be at the newest migration after a run. Seeing an OLDER \
         version here on the second (post-restart) run is the bug: the schema \
         regressed across a database restart"
    );
}

/// Creates a unique empty keyspace and runs the same application-owned runner
/// used at server startup. This is the merge-blocking release gate: a checked-in
/// migration is not enough; every registered migration must execute, record
/// its ledger row, and leave its derived postconditions present on a real CQL
/// cluster. The unique keyspace keeps this proof independent of the shared
/// `agent_memory_test` fixture and makes cleanup safe after failure.
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t10_fresh_keyspace_applies_every_registered_migration() {
    let test = TestClusterConfig::from_env()
        .expect("FERROSA_TEST_CQL_PORT must be set; start a test cluster first");
    let bootstrap_cfg = test_cfg(&test);
    let admin = connect_session(
        &bootstrap_cfg,
        &bootstrap_cfg.username,
        &bootstrap_cfg.password,
    )
    .await
    .expect("connect to test cluster for fresh-keyspace migration gate");
    let keyspace = fresh_migration_keyspace();

    let test_result: anyhow::Result<()> = async {
        let applied = run_migrations(&admin, &keyspace)
            .await
            .map_err(|error| anyhow::anyhow!("fresh migration registry failed: {error}"))?;
        if applied != MIGRATIONS.len() {
            anyhow::bail!(
                "fresh-keyspace migration run applied {applied} migrations; expected {}",
                MIGRATIONS.len()
            );
        }

        let status = migration_status(&admin, &keyspace)
            .await
            .map_err(|error| anyhow::anyhow!("fresh schema status query failed: {error}"))?;
        let expected = MIGRATIONS
            .last()
            .expect("migration registry must not be empty")
            .version;
        if status.db_version != expected {
            anyhow::bail!(
                "schema ledger reached v{}, expected release registry v{expected}",
                status.db_version
            );
        }
        if !status.pending.is_empty() {
            anyhow::bail!(
                "fresh keyspace has recorded versions but missing schema postconditions: {:?}",
                status.pending
            );
        }

        for migration in MIGRATIONS {
            if !live_version_recorded(&admin, &keyspace, migration.version).await {
                anyhow::bail!(
                    "migration {} ({}) has no schema_version ledger row",
                    migration.version,
                    migration.description
                );
            }
        }

        // Re-running must be an idempotent no-op after all postconditions are
        // confirmed, preventing a false pass that only works once.
        let reapplied = run_migrations(&admin, &keyspace)
            .await
            .map_err(|error| anyhow::anyhow!("idempotent migration rerun failed: {error}"))?;
        if reapplied != 0 {
            anyhow::bail!("an already-complete fresh keyspace reapplied {reapplied} migrations");
        }
        Ok(())
    }
    .await;

    let cleanup = admin
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), ())
        .await;
    if let Err(error) = cleanup {
        panic!("fresh-keyspace migration gate cleanup failed for {keyspace}: {error}");
    }
    test_result.unwrap_or_else(|error| panic!("fresh-keyspace migration gate failed: {error}"));
}

// ---------------------------------------------------------------------------
// T-11/T-12: Live partial-migration recovery (entity_warmth + typed_edges)
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

// ---------------------------------------------------------------------------
// T-14/T-15/T-16 (t_34ef406d): interrupted first-run bootstrap
// ---------------------------------------------------------------------------

/// Index of the first bootstrap DDL file that creates `table`.
fn bootstrap_file_creating(table: &str) -> usize {
    BOOTSTRAP_DDLS
        .iter()
        .position(|ddl| created_table_names(ddl).iter().any(|t| t == table))
        .unwrap_or_else(|| panic!("no bootstrap DDL file creates {table}"))
}

/// Replay the first `files` bootstrap DDL files into `keyspace`, exactly as
/// `apply_bootstrap` would. Simulates a first run that was killed partway
/// through: the keyspace exists, some tables exist, and `schema_version` was
/// never created.
async fn replay_bootstrap_prefix(
    session: &ferrosa_memory_core::cql_storage::CqlSession,
    keyspace: &str,
    files: usize,
) {
    let applied_at = chrono::Utc::now();
    for ddl in BOOTSTRAP_DDLS.iter().take(files) {
        let rewritten = ferrosa_memory_core::migration::qualify_ddl(ddl, keyspace);
        for stmt in ferrosa_memory_core::migration::split_cql(&rewritten) {
            let prepared =
                ferrosa_memory_core::migration::prepare_bootstrap_statement(&stmt, applied_at);
            session
                .query_unpaged(prepared.as_str(), ())
                .await
                .unwrap_or_else(|e| panic!("partial bootstrap statement failed: {e}\n{prepared}"));
            session
                .await_schema_agreement()
                .await
                .expect("schema agreement during partial bootstrap");
        }
    }
}

/// Every table the pre-versioning DDL files (001-019) create. Computed here
/// from the DDL text rather than from the production helper so this test does
/// not inherit a bug in the code under test.
fn expected_pre_versioning_tables() -> Vec<String> {
    let cut = BOOTSTRAP_DDLS
        .iter()
        .position(|ddl| *ddl == include_str!("../../../ddl/020_rich_entity_schema.cql"))
        .expect("ddl/020 must be present in BOOTSTRAP_DDLS");
    let mut tables: Vec<String> = BOOTSTRAP_DDLS
        .iter()
        .take(cut)
        .flat_map(|ddl| created_table_names(ddl))
        .collect();
    tables.sort();
    tables.dedup();
    tables
}

async fn missing_tables(
    session: &ferrosa_memory_core::cql_storage::CqlSession,
    keyspace: &str,
    expected: &[String],
) -> Vec<String> {
    let mut missing = Vec::new();
    for table in expected {
        if !live_table_exists(session, keyspace, table).await {
            missing.push(table.clone());
        }
    }
    missing
}

/// T-14 (t_34ef406d): a first run killed partway through the bootstrap must be
/// **resumed**, not mistaken for a pre-versioning install.
///
/// The reported failure: `keyspace_exists` is the greenfield signal, so once
/// `ddl/001_keyspace.cql` has run the next start skips the whole bootstrap,
/// finds `schema_version` empty, and seeds the adoption baseline at 19 —
/// asserting that DDLs 1-19 ran when they did not. Every table from the
/// unreached files (here `rules_by_id`, from `ddl/012`) is then never created
/// by anything, the CQL prepare loop fails forever, and `/healthz/ready`
/// never returns 200.
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t14_interrupted_bootstrap_is_resumed_not_adopted_at_baseline() {
    // Own keyspace, but still serialized: concurrent CREATE/DROP KEYSPACE on
    // the shared cluster contends for schema agreement with its siblings.
    let _serial = live_migration_test_lock().lock().await;
    let test = TestClusterConfig::from_env()
        .expect("FERROSA_TEST_CQL_PORT must be set; start a test cluster first");
    let cfg = test_cfg(&test);
    let session = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect_session must succeed against test cluster");
    let keyspace = fresh_migration_keyspace();

    let outcome: anyhow::Result<()> = async {
        // Kill the first run just before ddl/012 creates rules_by_id — the
        // table named in the incident's PREPARE failure.
        let cut = bootstrap_file_creating("rules_by_id");
        replay_bootstrap_prefix(&session, &keyspace, cut).await;

        // Preconditions: half-built keyspace, no version ledger.
        if !live_table_exists(&session, &keyspace, "entity_store").await {
            anyhow::bail!("partial bootstrap did not create entity_store");
        }
        if live_table_exists(&session, &keyspace, "rules_by_id").await {
            anyhow::bail!("partial bootstrap should have stopped before rules_by_id");
        }
        if live_table_exists(&session, &keyspace, "schema_version").await {
            anyhow::bail!("partial bootstrap must leave schema_version absent");
        }

        // The next daemon start.
        let run = run_migrations(&session, &keyspace).await;

        // Whatever the runner returns, the install must not be left with a
        // pre-19 table that nothing will ever create.
        let expected = expected_pre_versioning_tables();
        let missing = missing_tables(&session, &keyspace, &expected).await;
        if !missing.is_empty() {
            anyhow::bail!(
                "resumed bootstrap left {} pre-versioning table(s) missing: {missing:?} \
                 (run_migrations returned {:?})",
                missing.len(),
                run.as_ref().map_err(|e| e.to_string())
            );
        }
        run.map_err(|e| anyhow::anyhow!("run_migrations must recover the interrupted run: {e}"))?;

        let status = migration_status(&session, &keyspace)
            .await
            .map_err(|e| anyhow::anyhow!("migration_status: {e}"))?;
        if !status.pending.is_empty() {
            anyhow::bail!(
                "resumed install still has pending migrations: {:?}",
                status.pending
            );
        }
        let expected_version = MIGRATIONS.last().expect("registry not empty").version;
        if status.db_version != expected_version {
            anyhow::bail!(
                "resumed install reached v{}, expected v{expected_version}",
                status.db_version
            );
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), ())
        .await;
    if let Err(error) = cleanup {
        panic!("t14 cleanup failed for {keyspace}: {error}");
    }
    outcome.unwrap_or_else(|error| panic!("interrupted-bootstrap recovery failed: {error}"));
}

/// T-15 (t_34ef406d regression guard): a **genuine** pre-versioning install —
/// one that really did run DDLs 001-019 by hand and holds live data — must
/// still be adopted at the baseline, and its data must not be touched.
///
/// This is the risk the fix introduces: making an incomplete keyspace re-run
/// the bootstrap must not make a complete one re-run it, because
/// `ddl/008_intentions_repo_scope.cql` opens with `DROP TABLE IF EXISTS
/// intentions`.
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t15_genuine_pre_versioning_install_adopts_baseline_and_keeps_its_data() {
    let _serial = live_migration_test_lock().lock().await;
    let test = TestClusterConfig::from_env()
        .expect("FERROSA_TEST_CQL_PORT must be set; start a test cluster first");
    let cfg = test_cfg(&test);
    let session = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect_session must succeed against test cluster");
    let keyspace = fresh_migration_keyspace();

    let outcome: anyhow::Result<()> = async {
        // Build the full pre-versioning schema the way a hand-run install had it.
        let cut = BOOTSTRAP_DDLS
            .iter()
            .position(|ddl| *ddl == include_str!("../../../ddl/020_rich_entity_schema.cql"))
            .expect("ddl/020 present");
        replay_bootstrap_prefix(&session, &keyspace, cut).await;

        // Legacy rows the adoption path must preserve.
        let tenant = Uuid::new_v4();
        let intention = Uuid::new_v4();
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.intentions \
                     (tenant_id, repo, intention_id, description, status) \
                     VALUES ({tenant}, 'legacy-repo', {intention}, 'legacy intention', 'pending')"
                ),
                (),
            )
            .await
            .map_err(|e| anyhow::anyhow!("seed legacy intention: {e}"))?;
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.entity_types (type_name, description) \
                     VALUES ('operator_custom_type', 'hand-edited by the operator')"
                ),
                (),
            )
            .await
            .map_err(|e| anyhow::anyhow!("seed operator entity type: {e}"))?;

        run_migrations(&session, &keyspace)
            .await
            .map_err(|e| anyhow::anyhow!("adoption run must succeed: {e}"))?;

        if !live_version_recorded(&session, &keyspace, PRE_VERSIONING_BASELINE).await {
            anyhow::bail!(
                "a genuine pre-versioning install must still be seeded at baseline v{PRE_VERSIONING_BASELINE}"
            );
        }

        let kept = session
            .query_unpaged(
                format!(
                    "SELECT description FROM {keyspace}.intentions \
                     WHERE tenant_id = {tenant} AND repo = 'legacy-repo' \
                     AND intention_id = {intention}"
                ),
                (),
            )
            .await
            .map_err(|e| anyhow::anyhow!("read back legacy intention: {e}"))?;
        if kept.rows_or_empty().is_empty() {
            anyhow::bail!(
                "adoption destroyed the legacy intentions row — the bootstrap's \
                 `DROP TABLE IF EXISTS intentions` must never run against a complete keyspace"
            );
        }

        let custom = session
            .query_unpaged(
                format!(
                    "SELECT type_name FROM {keyspace}.entity_types \
                     WHERE type_name = 'operator_custom_type'"
                ),
                (),
            )
            .await
            .map_err(|e| anyhow::anyhow!("read back operator entity type: {e}"))?;
        if custom.rows_or_empty().is_empty() {
            anyhow::bail!("adoption dropped the operator's custom entity_types row");
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), ())
        .await;
    if let Err(error) = cleanup {
        panic!("t15 cleanup failed for {keyspace}: {error}");
    }
    outcome.unwrap_or_else(|error| panic!("pre-versioning adoption regressed: {error}"));
}

/// T-16 (t_34ef406d): the awkward middle case — an install adopted so early
/// that its ≤19 baseline never created some pre-19 tables (the gap that
/// migrations 25 and 42 were added to paper over), but which holds real data.
/// The resumed bootstrap must create the missing table and must NOT execute
/// the historic `DROP TABLE IF EXISTS intentions` against populated data.
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t16_incomplete_legacy_install_is_repaired_without_dropping_populated_tables() {
    let _serial = live_migration_test_lock().lock().await;
    let test = TestClusterConfig::from_env()
        .expect("FERROSA_TEST_CQL_PORT must be set; start a test cluster first");
    let cfg = test_cfg(&test);
    let session = connect_session(&cfg, &cfg.username, &cfg.password)
        .await
        .expect("connect_session must succeed against test cluster");
    let keyspace = fresh_migration_keyspace();

    let outcome: anyhow::Result<()> = async {
        let cut = BOOTSTRAP_DDLS
            .iter()
            .position(|ddl| *ddl == include_str!("../../../ddl/020_rich_entity_schema.cql"))
            .expect("ddl/020 present");
        replay_bootstrap_prefix(&session, &keyspace, cut).await;

        let tenant = Uuid::new_v4();
        let intention = Uuid::new_v4();
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.intentions \
                     (tenant_id, repo, intention_id, description, status) \
                     VALUES ({tenant}, 'legacy-repo', {intention}, 'legacy intention', 'pending')"
                ),
                (),
            )
            .await
            .map_err(|e| anyhow::anyhow!("seed legacy intention: {e}"))?;

        // The early-adoption gap: this install never got ddl/011's table.
        session
            .query_unpaged(format!("DROP TABLE IF EXISTS {keyspace}.entity_warmth"), ())
            .await
            .map_err(|e| anyhow::anyhow!("simulate missing entity_warmth: {e}"))?;

        run_migrations(&session, &keyspace)
            .await
            .map_err(|e| anyhow::anyhow!("repair run must succeed: {e}"))?;

        if !live_table_exists(&session, &keyspace, "entity_warmth").await {
            anyhow::bail!("the repair must create the missing pre-19 table entity_warmth");
        }
        let kept = session
            .query_unpaged(
                format!(
                    "SELECT description FROM {keyspace}.intentions \
                     WHERE tenant_id = {tenant} AND repo = 'legacy-repo' \
                     AND intention_id = {intention}"
                ),
                (),
            )
            .await
            .map_err(|e| anyhow::anyhow!("read back legacy intention: {e}"))?;
        if kept.rows_or_empty().is_empty() {
            anyhow::bail!(
                "repairing a missing pre-19 table destroyed the populated intentions table"
            );
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {keyspace}"), ())
        .await;
    if let Err(error) = cleanup {
        panic!("t16 cleanup failed for {keyspace}: {error}");
    }
    outcome.unwrap_or_else(|error| panic!("incomplete legacy repair failed: {error}"));
}
