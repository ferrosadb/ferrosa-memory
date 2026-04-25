---
type: feat
priority: P1
status: in-process
created: 2026-04-19
updated: 2026-04-20
reported-by: blueprint update for operator UI shell (2026-04-19)
---

# Operator console above viz with CQL, Datalog, and rules interfaces

## Goal

Promote the current viz page into one destination inside a broader operator console.

The console should expose:

1. a home page at `/`
2. Viz for graph exploration
3. a public-CQL passthrough explorer
4. a public-SPARQL passthrough explorer
5. a ferrosa-memory-owned Datalog explorer with provenance
6. a rules-management interface

## Why now

Viz is useful, but it is too narrow to serve as the top-level operator experience. Debugging and reviewing symbolic state requires both raw-data inspection and inference-level inspection.

## Constraints

- CQL explorer is read-only by default and should pass requests through to Ferrosa's public CQL interface rather than emulate semantics locally
- SPARQL explorer should pass requests through to Ferrosa's public SPARQL interface rather than inventing local RDF/query semantics
- Datalog explorer must surface provenance, not just tuples, and should clearly state that ferrosa-memory owns the evaluator over Ferrosa-backed data
- Rules manager must show builtin, registry, and effective rule views
- The console must sit behind the same operator auth boundary as reviewer-facing flows

## Acceptance

- `/` acts as a landing page and routes to all destinations
- Operators can run representative public-CQL queries and inspect rows
- Operators can run representative public-SPARQL queries and inspect graph-oriented results
- Operators can run local Datalog queries and inspect explanation chains
- Operators can compare builtin, registry, and effective rules from the UI

## References

- [expert-system-knowledge-plane.md](../expert-system-knowledge-plane.md)
- [project-plan.md](../project-plan.md)
- [shared-http-deployment.md](../shared-http-deployment.md)
