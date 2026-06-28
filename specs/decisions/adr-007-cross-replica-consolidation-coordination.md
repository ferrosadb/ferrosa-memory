---
title: ADR-007 Cross-Replica Consolidation Coordination
status: proposed
date: 2026-06-28
executive_summary: >
  Consolidation for a single (tenant, session) must be coordinated across
  ferrosa-memory MCP replicas so that only one replica runs the dream
  consolidation at a time. We will use the shared Ferrosa database as the
  coordination substrate — a durable work queue plus LWT-based lease — rather
  than relying on session affinity or a separate cron job. This keeps the
  serving layer horizontally scalable and lets the database’s HA properties
  protect the coordination plane.
---

# ADR-007: Cross-Replica Consolidation Coordination

## Status

Proposed.

## Context

`ferrosa-memory` can run multiple MCP-server replicas behind a single load
balancer for the same tenant. Today the consolidation trigger is purely
in-process:

- `SessionState.consolidation_queue` is an in-memory `VecDeque<(tenant_id, session_id)>`.
- Deduplication is `queue.contains(...)` scoped to one process.
- The idle consolidation loop is a per-process Tokio timer (`idle_consolidation_seconds`).
- `runtime_session_id` and `last_consolidation_status` are per-process.

With `replicaCount > 1` the same session is queued and consolidated
independently by every replica, producing duplicate or competing
`run_consolidation` / `dream` work. The issue
[ferrosadb/ferrosa-memory#130](https://github.com/ferrosadb/ferrosa-memory/issues/130)
documents this and asks for a solution.

Ferrosa DB is already the durable shared substrate. The user preference is to
coordinate through it because it is supposed to be HA.

## Decision

Adopt a **database-backed lease queue** for cross-replica consolidation
coordination.

1. Each replica inserts or updates a durable request row keyed by `(tenant_id, session_id)`.
2. A periodic worker on every replica polls the queue and attempts to take an
   LWT-based lease on a pending row.
3. Exactly one replica wins the lease for a given `(tenant, session)` at a time.
4. The winner runs `dream::run_consolidation`, then marks the row completed
   (or failed with retry/backoff metadata).
5. Non-winners skip until the next poll cycle.
6. Leases expire automatically after a TTL so a crashed winner cannot block the
   session forever.

This decision has three consequences for alternatives:

- **Session affinity is not the primary fix.** It can be a useful deployment
  optimization later, but it does not solve duplicate idle timers, it adds
  LB complexity, and it cannot coordinate across replicas during rolling
  restarts or rebalancing.
- **External cron job is not the primary fix.** The existing
  `feat-consolidation-cron-job.md` idea remains valid as a complementary
  batch/operator path, but the serving layer must also coordinate so that a
  cron job and live replicas do not race on the same session.
- **The database is the source of coordination truth.** The same Ferrosa
  keyspace that already owns the entities, edges, and session state now also
  owns the consolidation queue and lease state.

## Consequences

- The MCP server becomes safe to scale horizontally per tenant for reads and
  for background consolidation.
- Replicas remain stateless w.r.t. consolidation; any replica can run any queued
  session.
- The database becomes a single coordination plane, so coordination
  availability matches database availability.
- A new CQL migration adds the queue/lease tables.
- The `Storage` trait gains queue/lease operations, implemented by
  `CqlStorage` and `MockStorage`.
- The per-process `consolidation_queue`, idle timer, and status map are
  superseded by the DB-backed worker, then removed.
- The short-term documentation fix (`replicaCount = 1 per tenant`) is kept
  until the lease queue is shipped and verified.

## Required Controls

- LWT compare-and-set on lease columns with explicit TTL.
- Lease TTL must be shorter than the expected consolidation duration plus a
  safety margin, with renewal while work is running.
- Completed/failed rows are retained for operator visibility and must not
  accumulate unbounded; a retention window is configured.
- The queue worker must be tenant-aware and authenticate under the queued
  tenant, not the process default tenant.
- Backpressure: if a replica cannot acquire a lease because the queue is full,
  the write path must still return success to the caller; consolidation is
  best-effort and retried.
- Idempotency: `dream` consolidation is already edge-upsert idempotent, so a
  lease timeout followed by a second winner is safe.

## Open Questions

1. Should the lease table use a single-row-per-session model or a
   time-ordered log of consolidation attempts? Proposed: single active lease
   row plus a separate audit log table.
2. What is the initial lease TTL and poll interval? Proposed: poll every 5 s,
   lease TTL 30 s, renewal at 15 s.
