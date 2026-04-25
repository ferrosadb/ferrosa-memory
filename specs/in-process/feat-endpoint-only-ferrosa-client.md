---
type: feat
priority: P0
status: in-process
created: 2026-04-20
updated: 2026-04-20
reported-by: dsm-analysis refactor request (2026-04-20)
---

# Refactor `ferrosa-memory` to respect the public graph boundary

## Goal

Make `ferrosa-memory` a client to Ferrosa at the correct abstraction level: direct CQL for app-owned tables is acceptable, but graph-owned backing tables are not a public API and operator query surfaces should be passthrough/fail-loud.

## Required Outcomes

1. Replace local workbench "CQL" interpretation with authenticated passthrough to Ferrosa public CQL
2. Keep operator Datalog semantics local to ferrosa-memory and remove any public-Datalog drift from UI/docs/tests
3. Route graph reads and writes through public graph/Cypher interfaces; no serving-path writes may name graph-owned backing tables directly
4. Add SPARQL client/query support through Ferrosa public interfaces instead of local translation layers
5. Keep direct CQL usage compatible with the planned `app_reader` role rollout
6. Fail loudly when Ferrosa public APIs disagree with expected semantics; track those defects in Ferrosa

## Why now

The current runtime has become tightly coupled to Ferrosa internals:

- `CqlStorage` construction in `ferrosa-memory-mcp`
- local read-only query parsing for the workbench CQL surface
- graph reads use the public Cypher endpoint while graph writes still bypass it and target graph-owned backing tables directly
- mixed ownership of graph/query semantics across local code and Ferrosa

This undermines the intended product position: `ferrosa-memory` should orchestrate memory workflows and UI composition, not re-implement Ferrosa storage behavior.

## Acceptance

- No browser query surface emulates CQL/SPARQL/Cypher public semantics locally, and Datalog is explicitly documented as repo-owned
- Runtime graph mutations use Ferrosa public graph interfaces
- Direct CQL usage stays within app-table ownership and `app_reader`-compatible permissions
- No serving-path graph mutation names Ferrosa graph-owned backing tables directly
- Failures from public interfaces are surfaced clearly to the user
- Docs, tests, and architecture specs consistently describe `ferrosa-memory` as a client to Ferrosa that respects the graph boundary

## References

- [overview.md](../overview.md)
- [components.md](../components.md)
- [data-flow.md](../data-flow.md)
- [dsm-analysis.md](../dsm-analysis.md)
- [decisions/adr-005-endpoint-only-ferrosa-client.md](../decisions/adr-005-endpoint-only-ferrosa-client.md)
- [bug-ferrosa-memory-bypasses-graph-api-for-writes.md](./bug-ferrosa-memory-bypasses-graph-api-for-writes.md)
- [todo-extend-ferrosa-memory-graph-client-with-cypher-writes.md](./todo-extend-ferrosa-memory-graph-client-with-cypher-writes.md)
