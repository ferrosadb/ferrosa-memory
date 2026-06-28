---
title: FMEA — Cross-Replica Consolidation Coordination
status: draft
date: 2026-06-28
executive_summary: >
  Failure-mode analysis for the proposed database-backed lease queue. Highest
  risks are lease TTL too long (stuck session after replica crash) and
  duplicate claims due to clock skew or LWT misuse. All RPN >= 50 items
  map to explicit tests.
---

# FMEA — Cross-Replica Consolidation Coordination

| # | Function | Failure Mode | Effect | S | Cause | O | Detection | D | RPN | Test |
|---|----------|-------------|--------|---|-------|---|-----------|---|-----|------|
| 1 | Claim lease | Two replicas win the same lease | Duplicate consolidation run on same session | 6 | LWT condition omitted or clock skew makes TTL overlap | 4 | Run log shows two `started_at` within TTL | 5 | 120 | Two-replica race test with run-log assertion |
| 2 | Hold lease | Winner crashes mid-run | Session stuck until TTL expires; no data loss | 7 | Replica OOM/node failure | 3 | Lease not renewed and no complete row after TTL | 4 | 84 | Kill replica mid-consolidation; verify second replica claims after TTL |
| 3 | Hold lease | Lease TTL too long | Recovery time unacceptable after crash | 5 | Config error or default too high | 4 | Operator metric `stuck_session_seconds` | 5 | 100 | Set TTL to 5 s; crash winner; assert takeover within 2x poll |
| 4 | Queue insert | Upsert rejected / DB partition | Consolidation silently skipped | 6 | DB unavailable during write tool | 5 | Write tool returns success but request row missing | 4 | 120 | Drop DB connectivity on write; assert queue row present on recovery |
| 5 | Complete run | Winner fails to mark completed | Session re-consolidated immediately | 5 | Network blip between dream finish and CQL update | 4 | `attempt_count` increments and next run starts quickly | 4 | 80 | Blackhole CQL briefly after dream completes |
| 6 | Retention | Completed rows accumulate | Table bloat / degraded queries | 4 | No TTL or janitor | 5 | Storage growth alert | 5 | 100 | Insert old completed rows; assert cleanup after retention window |
| 7 | Tenant context | Worker uses process default tenant | Wrong tenant consolidates session | 8 | Worker spawned without tenant context | 2 | Authentication/authorization error or wrong data | 3 | 48 | Multi-tenant test: worker must authenticate per queued tenant |
| 8 | LWT semantics | Non-serialized claim on table without correct PK | Split-brain claims | 9 | Schema missing `(tenant_id, session_id)` PK | 2 | Unit test or code review | 2 | 36 | Schema test asserts PK and LWT compatibility |

## Notes

- Severity 6 for duplicate run: `dream` is edge-upsert idempotent, so the
  primary impact is wasted compute and confusing run history, not corruption.
- Severity 7 for stuck session: user-visible delay, but no data loss.
- Severity 8 for wrong tenant: data isolation breach; prevented by keeping
  tenant context with each request row and using existing auth paths.
- Detection ratings assume operator metrics plus unit/integration tests.
- All RPN >= 50 items require tests before the feature can be declared done.
