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
    MIGRATIONS, Migration, PRE_VERSIONING_BASELINE, run_migrations,
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
// T-05: Old-schema keyspace (v30, missing first_seen) auto-upgrades through
// the current migration registry.
// ---------------------------------------------------------------------------

/// T-05: Simulate a keyspace at version 30 (pre-first_seen fix), run
/// migrations, and assert the column exists and the graph MERGE path
/// no longer errors on "unknown property 'first_seen'".
#[tokio::test]
#[ignore = "requires live test cluster; run with --ignored"]
async fn t03_old_schema_auto_upgrades_to_v31() {
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

    // Step 4: Delete every post-v30 row so max(schema_version) really
    // rewinds to 30. The migration runner intentionally treats the max
    // recorded version as current, so deleting only v31 is insufficient once
    // later migrations exist.
    let pending_after_rewind = MIGRATIONS.iter().filter(|m| m.version > 30).count();
    for migration in MIGRATIONS.iter().filter(|m| m.version > 30) {
        let delete_version = format!(
            "DELETE FROM {}.schema_version WHERE version = {}",
            cfg.keyspace, migration.version
        );
        session
            .query_unpaged(delete_version, ())
            .await
            .unwrap_or_else(|_| panic!("delete v{} record", migration.version));
    }

    // Step 5: Run migrations again — should detect v31+ pending and apply
    // the current tail of the append-only registry.
    let applied = run_migrations(&session, &cfg.keyspace)
        .await
        .expect("migration re-run must succeed after rewind");
    assert_eq!(
        applied, pending_after_rewind,
        "all post-v30 migrations should apply after rewind"
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
