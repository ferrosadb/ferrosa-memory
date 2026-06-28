---
title: Project Plan — Cross-Replica Consolidation Coordination
status: draft
date: 2026-06-28
executive_summary: >
  Four-sprint plan to move ferrosa-memory from per-process consolidation to a
  database-backed lease queue. Sprint 1 ships the short-term doc fix. Sprint 2
  lands the schema and storage trait contract. Sprints 3 and 4 implement the
  worker, dispatch changes, tests, and rollout.
---

# Project Plan — Cross-Replica Consolidation Coordination

## Goal

Make `ferrosa-memory` safe to run with `replicaCount > 1` per tenant by
coordinating consolidation through the Ferrosa database.

## Sprints

### Sprint 1 — Document the constraint (1-2 days)

**Shippable outcome:** operators know `replicaCount > 1` is unsafe until the
lease queue is implemented.

1. Add a `replicaCount = 1 per tenant` note to `specs/shared-http-deployment.md`.
1. Add the same warning to the Helm chart `values.yaml` comments.
1. Add an FMEA note to `specs/threat-model.md` under availability/operational
   risks.
1. Open issue 130 follow-up comment pointing to the ADR and this plan.

### Sprint 2 — Schema and trait contract (2-3 days)

**Shippable outcome:** migration and `Storage` trait additions are code-reviewed
and mergeable behind a feature flag.

1. Create `ddl/049_consolidation_lease_queue.cql` with `consolidation_requests`
   and `consolidation_runs` tables.
1. Register migration 049 in `crates/ferrosa-memory-core/src/migration.rs`.
1. Add `ConsolidationRequest`, `ConsolidationRun`, and trait methods to
   `crates/ferrosa-memory-core/src/storage.rs`.
1. Implement `MockStorage` stubs that track in-memory lease state for unit tests.
1. Add unit tests for claim/renew/complete semantics.
1. Add a schema-validation test asserting PK and LWT compatibility.

### Sprint 3 — Worker and dispatch rewrite (3-4 days)

**Shippable outcome:** single-replica behavior is preserved and all existing
consolidation tests pass with the new DB-backed worker.

1. Implement `CqlStorage` queue/lease operations using LWT.
1. Add `consolidation_worker_loop` in `crates/ferrosa-memory-mcp/src/main.rs`.
1. Replace in-memory `consolidation_queue` / `dirty` / `last_consolidation_status`
   usage with DB upserts and run-log reads.
1. Add `[consolidation]` config section and deprecate `server.idle_consolidation_*`.
1. Update `SessionState` to remove superseded fields.
1. Run `make test-unit` and `make test-contracts` green.

### Sprint 4 — Multi-replica tests and rollout (2-3 days)

**Shippable outcome:** CI proves no duplicate consolidation across replicas, and
`replicaCount > 1` becomes documented as supported.

1. Add a Python integration test that starts two MCP HTTP replicas, writes
   entities, and asserts only one consolidation run appears in the log.
1. Add a crash/recovery test that kills the lease-holding replica and verifies
   the surviving replica takes over after TTL.
1. Add metrics: `consolidation_claims_total`,
   `consolidation_duplicate_prevented_total`,
   `consolidation_stuck_session_seconds`.
1. Update Helm chart default and remove the temporary `replicaCount = 1`
   warning once tests pass.
1. Update CHANGELOG and operator runbook.

## Workstream Map

| Packet | Owner | Depends on | Main files | Parallelizable? |
|--------|-------|------------|------------|-----------------|
| A | Docs + FMEA | none | `specs/shared-http-deployment.md`, `specs/threat-model.md`, Helm `values.yaml` | Yes |
| B | Schema + Storage | none | `ddl/049_consolidation_lease_queue.cql`, `migration.rs`, `storage.rs`, `cql_storage.rs`, `types.rs` | No, single agent |
| C | Mock + unit tests | B | `storage.rs` tests, new test module | Yes after B skeleton |
| D | Worker + dispatch | B | `main.rs`, `dispatch.rs` | No with B, can start after trait is stable |
| E | Config + deprecation | D | config, workbench, docs | Yes after D |
| F | Integration tests | D | `tests/integration`, `tests/system` | Yes after D |
| G | Metrics + Helm cleanup | F | `metrics.rs`, Helm chart | Yes after F green |

## Risks

1. **LWT behavior on Ferrosa may differ from Scylla/Cassandra.** Mitigation:
   integration test the claim path against a real cluster early in Sprint 2.
1. **Clock skew between replicas could cause lease overlap.** Mitigation:
   lease checks compare wall-clock with generous jitter and use LWT conditions
   on the `lease_owner` column.
1. **Migration 049 on large existing keyspaces could create table-contention.
   Mitigation:** `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS`
   are idempotent; schedule a low-traffic window.
1. **Removing `dirty` flag could regress power-efficiency on idle services.**
   Mitigation: worker only polls when at least one pending row exists; sleep
   when the queue is empty.

## Definition of Done

- [ ] `replicaCount = 1` documentation shipped in Sprint 1.
- [ ] Migration 049 applied and reversible by backup restore.
- [ ] `Storage` trait methods implemented in `CqlStorage` and `MockStorage`.
- [ ] Unit tests cover claim, renew, complete, and stuck-lease takeover.
- [ ] Integration test proves single consolidation per session across two replicas.
- [ ] Integration test proves takeover after lease-holder crash.
- [ ] All CI checks green including `make test-unit`, `make test-contracts`,
      `make test-integration`.
- [ ] Helm chart and operator docs updated to support `replicaCount > 1`.
