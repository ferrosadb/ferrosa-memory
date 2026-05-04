---
type: feature
priority: P1
status: draft
created: 2026-05-04
reported-by: Hermes Agent (post-debugging session)
---

# Explicit migration status endpoint / query

## Problem

After deploying a new MCP binary with migration 31 registered, there was no way to confirm whether the migration actually ran. `docker logs ferrosa-memory-mcp` showed no migration-related messages. The `first_seen` column errors kept appearing, but it was unclear if they were from pre-restart or post-restart logs.

This is a blind spot: the agent (and operator) cannot tell:
- What is the current schema version in the database?
- Which migrations have been applied?
- Which migrations are registered in the running binary but not yet applied?

## Why it matters

Silent migration failure = silent schema drift. The graph write path keeps failing, but the logs look like "old errors" because there's no explicit `migration_applied` event.

## Desired Behavior

An explicit migration status mechanism:
1. **Query endpoint:** A tool or CQL query that returns `(current_db_version, binary_registry_max_version, pending_migrations[])`.
2. **Explicit event log:** Every applied migration writes a row to a `migration_log` table: `(version, applied_at, binary_hash, duration_ms, success)`.
3. **Startup banner:** On MCP startup, log: `Schema version: 31 (db) / 31 (binary). 0 pending migrations.`
4. **Mismatch alarm:** If `db_version < binary_version`, log ERROR with the list of pending migrations.

## Proposed Implementation

### Short-term: CQL query + logging
- Add a `schema_version` table (or use an existing system table) to track the applied version.
- On startup, read `schema_version`, compare to `BOOTSTRAP_DDLS.len()`, log the delta.
- This is ~20 lines in `main.rs`.

### Medium-term: Migration status tool
- Add an MCP tool: `mcp_ferrosa_memory_migration_status()`.
- Returns JSON: `{ db_version: 31, binary_version: 31, pending: [], last_applied: "2026-05-04T09:00:00Z" }`.
- This lets the agent query status without reading logs.

### Long-term: Migration dashboard in workbench
- The operator workbench shows a "Schema" panel: current version, pending migrations, last applied timestamp.
- One-click "Apply pending migrations" button (with confirmation for non-additive changes).

## Acceptance Criteria

- [ ] After MCP restart, logs contain an explicit line: `Schema version: N (db) / M (binary). X pending.`
- [ ] If `db_version < binary_version`, an ERROR-level log appears with migration names.
- [ ] A CQL query `SELECT * FROM agent_memory.schema_version` returns the current version.
- [ ] `migration_log` table contains rows for every applied migration with timestamps.
- [ ] The agent can call a tool to check status without parsing logs.

## Related

- `ddl/031_co_occurs_first_seen.cql` — the migration that motivated this
- `crates/ferrosa-memory-core/src/migration.rs` — migration registry
- `crates/ferrosa-memory-mcp/src/main.rs` — startup gate and logging
- `ferrosa-memory-ops` skill — operational runbook
