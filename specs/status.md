# ferrosa-memory Development Status

> Last updated: 2026-04-23
> Status: Living document

## Overview

`ferrosa-memory` is a Rust MCP server and operator workbench for durable agent memory on top of Ferrosa. It now has:

- shared HTTP auth/startup guardrails
- converged expert-system governance backends
- an operator workbench rooted at `/`
- explicit Sprint 9 work to finish the role-scoped boundary:
  app-table CQL stays allowed, graph-table writes do not

## Sprint Status

| Sprint | Status | Notes |
|--------|--------|-------|
| Sprint 1 | Complete | Foundation, memoization, plans, auth, CQL/storage baseline |
| Sprint 2 | Complete | Fold lifecycle, compression, graph hierarchy basics |
| Sprint 3 | Complete | Entity graph, temporal facts, feedback loop |
| Sprint 4 | Complete | Routing layer, HTTP transport, security hardening |
| Sprint 4.9 | Complete | Anomaly subscription and richer stats |
| Sprint 5 | Complete | Datalog, recursive exploration, warmth, pagerank |
| Sprint 5b | Complete | Durable materialization / promotion |
| Sprint 6 | Complete | Production hardening and type registry |
| Sprint 7 | Complete | Shared HTTP deployment hardening |
| Sprint 8 | Complete | Operator workbench, CQL/SPARQL passthrough, local Datalog ownership, rules/approvals/aliases, and live summary fixes are landed. |
| Sprint 9 | In Progress | Code-side graph-write cutover is landed, but final completion is blocked by a Ferrosa bug: public `TYPED_EDGE` MERGE succeeds without materializing a row. |
| Sprint 10 | In Progress | `ingest_entities` is landed on the MCP surface, batch entity/edge CRUD tools now use the real storage/graph delete paths, and live embedding generation works on `18765`; remaining work is progress notifications plus closing a Ferrosa-side existing-row backfill visibility / ANN issue. |

## Current Focus

### Post-Sprint 10 Backlog (filed 2026-05-04)

Operational gaps discovered during live debugging sessions:

| Item | Type | Priority | File |
|------|------|----------|------|
| Auto-session entity extraction | Feature | P2 | `todo/feat-auto-session-entity-extraction.md` |
| Consolidation timeout resilience | Bug | P1 | `todo/bug-run-consolidation-timeout-under-prepare-failures.md` |
| `memory` tool silent rejection | Bug | P1 | `todo/bug-memory-tool-silent-rejection.md` |
| Duplicate entities across sessions | Bug | P2 | `todo/bug-duplicate-entities-across-sessions.md` |
| Consolidation cron job | Feature | P2 | `todo/feat-consolidation-cron-job.md` |
| Migration status endpoint | Feature | P1 | `todo/feat-migration-status-endpoint.md` |

These were discovered while diagnosing: (1) arXiv cron silence (`search_arxiv.py` double-encoding), (2) `trajectory_folds` ANN PREPARE failure (FRSA-BUG-025), (3) migration 31 application status unknown, (4) `memory` tool at 93% blocking operational notes.

### Sprint 7 — Complete

Shared HTTP is now treated as a real authenticated service boundary:

- auth backend required
- TLS/startup guardrails enforced
- liveness/readiness split
- workbench and viz rollout posture covered by focused tests

Verification currently green for the shipped Sprint 7 surface:

- `cargo test --workspace --test shared_http_deployment_spec --test expert_system_rules_spec --test expert_system_governance_spec`
- `pytest tests/system/test_shared_http_workbench.py`
- `pytest tests/integration/test_query_surfaces.py tests/integration/test_expert_system_runtime.py`

### Sprint 8 — Complete

The operator console surface is now landed:

- effective rule loader
- claims / approvals / aliases
- explanation API
- integrated workbench + viz shell
- public CQL and SPARQL passthrough routes
- explicit local Datalog ownership and provenance UI
- live summary path fixed to use fast aggregate counts instead of graph-table row scans

### Sprint 9 — In Progress

The remaining architectural correction is narrowed to one external blocker:

- direct CQL is allowed for app-owned tables under the serving role
- graph-owned backing tables are not a public API
- serving-path graph writes now route through the public graph client seam
- workbench CQL/SPARQL are landed as passthrough/fail-loud
- dead local workbench CQL emulation is removed
- startup/readiness now succeed on the rebuilt local stack
- final completion is blocked by Ferrosa not materializing canonical public `TYPED_EDGE` MERGE writes

### Sprint 10 — In Progress

The server-owned bulk ingest workstream is underway:

- one `ingest_entities` MCP call for semantic entities + typed edges
- server-owned schema mapping and `schema_version` advertisement
- structured row-level failures instead of subprocess exit-code diagnostics
- optional server-side embedding for missing vectors
- strict edge validation and dry-run support, with MCP progress notifications still open
- tenant enforcement and graph-boundary compliance on the ingest path
- batch update/delete tools for entities and typed edges now use real hard-delete/update-capable backends instead of soft-delete / unsupported placeholders
- live `ingest_entities` + `retrieve_entities` verification is green for fresh v2-embedded rows on `http://127.0.0.1:18765/mcp`
- tenant-wide v2 backfill verification is green: phase 0 reran cleanly for `7555` rows with zero failures, representative old rows now read back with fresh `updated_at` values, and managed-server `hybrid_search` returns `entity_ann` hits from the old partition

## Open Workstreams

1. Close the Ferrosa bug where canonical public `TYPED_EDGE` MERGE does not materialize a row.
2. Re-run live graph-write verification once that Ferrosa fix lands.
3. Finish Sprint 10 progress notifications and workflow/system coverage for `ingest_entities`.
4. Keep readiness/auth hardening aligned to least-privilege role assumptions.
5. Keep Datalog clearly documented and tested as ferrosa-memory-owned.

## Verification Notes

Current local verification on the rebuilt `28765/28766` stack:

- `https://127.0.0.1:28765/healthz/ready` -> `ready`
- `http://127.0.0.1:28766/workbench/api/summary` -> `ready`
- summary counts:
  - `node_count: 7463`
  - `edge_count: 16016`
  - `derived_fact_count: 29886`
  - `rule_count: 10`

Focused verification is green:

- `cargo test -p ferrosa-memory-core http:: --lib`
- `cargo build -p ferrosa-memory-mcp`
- `/tmp/ferrosa-pytest-env/bin/python -m pytest tests/integration/test_expert_system_runtime.py tests/integration/test_query_surfaces.py tests/system/test_shared_http_workbench.py -q`

The remaining gap is specifically the public typed-edge mutation path:

- `POST /workbench/api/cql/query` is wired as a public CQL passthrough surface
- `POST /workbench/api/sparql/query` is wired as a public SPARQL passthrough surface
- Datalog is local by design; docs/tests now avoid presenting it as a Ferrosa public passthrough
- application-side graph writes are funneled through a shared `graph_write` seam and the serving path no longer names graph tables directly for writes
- `create_edge` now routes through the public graph client, but the canonical `TYPED_EDGE` public MERGE currently returns success without materializing a row in `agent_memory.typed_edges`

That final issue is tracked in Ferrosa at:

- [bug-public-cypher-typed-edge-merge-does-not-materialize.md](/Users/bkearns/src/ferrosa/specs/in-process/bug-public-cypher-typed-edge-merge-does-not-materialize.md)
