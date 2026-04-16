---
type: feat
priority: P1
reported-by: user
implemented-by: ""
verified-by: ""
created: 2026-04-16
updated: 2026-04-16
source: skills-layer Sprint 1 planning
source-location: "specs/skills-layer-design.md"
---

# Test cluster harness on port offset +500

## Motivation

Live-CQL tests (`crates/ferrosa-memory-core/tests/cql_storage_live.rs`, `tests/vector_live.rs`, etc.) currently target the dev cluster on default ports. Running them mutates dev state and can't safely run in CI or alongside live work.

The user wants a separate test cluster on offset ports (+500: CQL 9042→9542, HTTP graph port +500, etc.) so tests can exercise real CQL+graph without touching the dev cluster.

## Design

### Startup script

New `scripts/start-test-cluster.sh`:

- Starts a Ferrosa cluster (same topology as `start-cluster.sh`) with an explicit `--port-offset 500` and a dedicated data dir (`~/.ferrosa-test-data/`).
- Creates the `agent_memory_test` keyspace (distinct from `agent_memory`).
- Runs all DDLs in order (or, once schema-versioning is in, the new fmem build migrates it on first connect).
- Idempotent: running twice is a no-op; adds a `stop-test-cluster.sh` symmetric command.

### Test config

Live-test files read cluster endpoint from env:

```bash
FERROSA_TEST_CQL_HOST=localhost
FERROSA_TEST_CQL_PORT=9542
FERROSA_TEST_GRAPH_URL=http://localhost:7688
FERROSA_TEST_KEYSPACE=agent_memory_test
```

Tests skip (not fail) if env is unset — keeps `cargo test` green without the harness. A `test-all.sh` wrapper sets the env and runs the suite against the test cluster.

### Isolation

- Dedicated keyspace (`agent_memory_test`) means the dev keyspace is never touched.
- Per-test cleanup: each test starts with a unique tenant/session UUID; tests tear down via targeted deletes. No `TRUNCATE` of shared tables.
- Cluster lifecycle: the test cluster can run long-lived. Tests tolerate leftover state by scoping queries with fresh UUIDs.

### CI integration

- Lightweight: CI spins up the test cluster once per workflow, runs tests, tears down.
- Healthcheck before tests: `cqlsh -h localhost -p 9542 -e 'DESCRIBE KEYSPACES'` must succeed before any test binary runs.

## Integration with schema versioning (feat-schema-versioning)

Once schema-versioning (`specs/todo/feat-schema-versioning.md`) lands, first boot of fmem against the test cluster auto-runs all migrations → the test keyspace always matches the current code's expected schema. Until then, `start-test-cluster.sh` runs DDLs 001-020 in sequence manually.

## Acceptance Criteria

- [ ] `scripts/start-test-cluster.sh` boots a cluster at offset +500, creates `agent_memory_test`, applies DDLs, and prints the connection info.
- [ ] `scripts/stop-test-cluster.sh` shuts down cleanly without touching the dev cluster.
- [ ] With test cluster running, `FERROSA_TEST_CQL_PORT=9542 cargo test --features live-cql` runs live tests and passes.
- [ ] Without test cluster, `cargo test` still passes (live tests skip with a clear message, not fail).
- [ ] `crates/ferrosa-memory-core/tests/cql_storage_live.rs` round-trips all `EntityEntry` fields including the new rich-schema columns (description, description_embedding, tags, properties, content_hash, updated_at, scope, ingested_by_session).
- [ ] Dev cluster unaffected by running the test suite (validate by checking dev keyspace row counts before/after).

## Dependencies

- Skills Sprint 1 code lands first (needs fields to test against).
- Ideally: schema-versioning (feat-schema-versioning.md) lands before this so the test cluster auto-migrates.

## Out of Scope

- Docker/podman-based test cluster (current dev setup is bare-metal; match that).
- Load/stress testing (separate work).
- Multi-node test cluster (single-node is fine for correctness).

## Implementation Notes

_To be filled in by implementer._
