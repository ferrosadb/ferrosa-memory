---
type: feat
priority: P1
status: implemented
created: 2026-04-16
updated: 2026-04-20
reported-by: user
---

# Application-owned schema versioning for fmem

## Motivation

DDLs 001-019 are applied manually via `cqlsh < ddl/NNN_foo.cql` as a deployment ritual. Each new DDL (including 020 from the skills work) extends this. The user wants fmem itself to own the migration: know which schema version the keyspace is at, know which version the code requires, and apply pending DDLs at startup.

Goals:

- **Versioned:** `schema_version` table records which migrations have been applied.
- **Forward-only:** migrations never undo. Downgrades abort server startup.
- **Fail loud:** any migration error aborts startup with a clear message; no silent partial migration.
- **Backup-aware:** the operator's upgrade workflow is "take backup → start new fmem build → fmem auto-migrates → verify." Server must log loudly before applying so operators confirm the backup landed.

## Design

### Storage

```cql
CREATE TABLE IF NOT EXISTS schema_version (
    version      int PRIMARY KEY,
    applied_at   timestamp,
    description  text,
    applied_by   text  -- host/process info for audit
);
```

### Code structure

A new `ferrosa-memory-core::migration` module:

```rust
pub struct Migration {
    pub version: u32,
    pub description: &'static str,
    pub ddl: &'static str,  // embedded via include_str!
}

pub const MIGRATIONS: &[Migration] = &[
    Migration { version: 20, description: "rich entity schema", ddl: include_str!("../../../ddl/020_rich_entity_schema.cql") },
    // future migrations append here, monotonically increasing version
];

pub async fn run_migrations(session: &CdrsSession, keyspace: &str) -> Result<()> { ... }
```

Server startup (`main.rs`):

```rust
match migration::run_migrations(&session, &config.ferrosa.keyspace).await {
    Ok(count) if count > 0 => tracing::info!("applied {count} schema migrations"),
    Ok(_) => tracing::debug!("schema up to date"),
    Err(e) => anyhow::bail!("schema migration failed, aborting startup: {e}"),
}
```

### Adoption for existing deployments

DDLs 001-019 were applied manually. When the versioning feature first ships, the operator must seed the `schema_version` table to reflect current state:

```cql
INSERT INTO schema_version (version, applied_at, description, applied_by)
VALUES (19, toTimestamp(now()), 'pre-versioning baseline', 'manual-seed');
```

Document this step clearly. First-run detection: if `schema_version` table doesn't exist, server logs a warning and refuses to start in production mode (requires explicit `--allow-unversioned-schema` flag to bypass for dev).

### Execution semantics

- Each migration applied in its own CQL connection scope.
- On success: `INSERT INTO schema_version (version, applied_at, description, applied_by)`.
- On failure: log the CQL error, log the version it failed on, abort startup. Leaves `schema_version` at the last successful version.
- DDLs may contain multiple statements separated by `;` — split carefully (cqlsh-style: no `;` inside quoted strings).
- Migrations are idempotent where possible (`CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`). `ALTER TABLE ADD` is not idempotent in CQL — wrap in a pre-check that queries the schema and skips if the column exists.

### Downgrade protection

If `MAX(version) > max(MIGRATIONS)`, the keyspace was touched by a newer server build. Abort startup with a clear error: "keyspace at v21, this build only supports up to v20. Upgrade the binary or restore from backup."

## Acceptance Criteria

- [ ] Fresh keyspace: starting fmem applies all migrations from v1, ends at current target.
- [ ] Pre-existing keyspace (DDLs 001-019 applied manually): after seed INSERT, starting fmem applies only v20 forward.
- [ ] Mid-migration failure (simulate via bad DDL): server aborts, `schema_version` stays at last success, error logged with version number.
- [ ] Downgrade attempted: older binary against newer keyspace — server refuses to start, clear error.
- [ ] Production startup without `schema_version` table: server refuses unless `--allow-unversioned-schema` flag supplied.
- [ ] Log line at startup names current version and target version.

## Dependencies

- Precedes: any future fmem DDL. New DDL = new migration entry in `MIGRATIONS`.
- Migration 020 (rich entity schema) is the first migration to use this framework post-seed.

## Out of Scope

- Rollback (schema "down" migrations). Forward-only; rollback is via backup restore.
- Per-tenant schema isolation (global keyspace schema).
- Cross-keyspace migrations (everything is in `agent_memory`).

## Implementation Notes

_To be filled in by implementer._
