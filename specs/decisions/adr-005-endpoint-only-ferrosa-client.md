## Status

Accepted

## Context

`ferrosa-memory` has drifted from its intended architectural position.

The current implementation embeds Ferrosa storage behavior directly:

- `ferrosa-memory-mcp` creates `CqlStorage` and connects to Ferrosa with `cdrs-tokio`
- `ferrosa-memory-core/src/cql_storage.rs` prepares and executes table-level CQL statements directly
- workbench "CQL" and Datalog surfaces interpret queries locally instead of passing them through to Ferrosa public query interfaces
- `graph.rs` already behaves more like the intended boundary by using Ferrosa's public HTTP Cypher endpoint for graph traversals

The correction is narrower than "no direct CQL":

1. CQL is a public protocol, and using the Rust CQL driver for app-owned tables is acceptable.
2. Graph-owned tables such as `typed_edges`, `folded_into`, `mentioned_in`, `co_occurs_with`, `supersedes`, `derived_edges_by_pred`, and `derived_edges_by_src` are internal storage schema owned by `ferrosa-graph`, not a public API.

Analogy: talking to PostgreSQL over the wire protocol is fine; writing directly to `pg_index` instead of issuing `CREATE INDEX` is not. Same protocol, wrong abstraction level.

This creates two problems:

1. `ferrosa-memory` is tightly coupled to Ferrosa internals and schema details instead of acting as a client
2. UI query surfaces can paper over bugs by emulating or constraining semantics locally rather than failing loudly when Ferrosa's public APIs do not behave correctly

The required posture is different: `ferrosa-memory` should be a client to Ferrosa, not an alternate storage implementation.

## Decision

Adopt a public-protocol / graph-boundary integration rule for runtime behavior.

1. **Direct CQL is allowed where `ferrosa-memory` is acting as an application client.**
   Using `cdrs-tokio` and prepared CQL statements for app-owned tables is compatible with the intended architecture and with the planned `app_reader` role rollout.

2. **Graph-owned backing tables are not a public API.**
   Serving-path graph reads and especially graph writes must not rely on direct mutation of graph-owned backing tables. Graph mutations must go through Ferrosa's graph interfaces.

3. **Frontend query surfaces are passthrough consoles, not emulators.**
   The CQL, SPARQL, Cypher, and Datalog/operator query surfaces must forward requests to Ferrosa public APIs with minimal shaping for auth, transport, and presentation. They must not implement local substitute semantics.

4. **Failures are surfaced, not papered over.**
   If a public Ferrosa interface fails, disagrees with expected semantics, or lacks parity with advertised behavior, `ferrosa-memory` must return a clear error and the issue is tracked as a Ferrosa bug.

5. **`ferrosa-memory` owns client orchestration, auth mapping, and UI composition.**
   The project continues to own MCP protocol handling, tenant/auth boundaries, workbench composition, and client-side aggregation where needed, but it should not reach through Ferrosa graph interfaces into graph-engine storage layout.

6. **The serving path must be compatible with the CQL role-auth rollout.**
   `ferrosa-memory` should be able to run as `app_reader`: `SELECT` on graph tables, `MODIFY` on app tables, and no direct requirement to `MODIFY` graph tables.

## Consequences

- The current runtime architecture is explicitly non-compliant with the graph boundary and requires refactoring
- Query surfaces in the workbench must be simplified into authenticated passthroughs
- Several backend modules will either disappear from the serving path or be reduced to thinner client adapters
- Bugs previously "fixed" in local emulation layers should instead be treated as contract bugs in Ferrosa when the public APIs are the source of truth
- DSM guidance shifts from "replace all direct CQL" to "separate acceptable app-table CQL access from unacceptable graph-table ownership and local query emulation"

## Required Refactor Outcomes

1. Move graph mutations onto Ferrosa graph interfaces
2. Move CQL/SPARQL/Cypher operator query execution to passthrough adapters
3. Remove local query interpreters that masquerade as public query engines while keeping ferrosa-memory-owned Datalog explicit
4. Keep direct CQL usage aligned with app-table ownership and the `app_reader` role boundary
5. Update tests to validate client behavior against public Ferrosa contracts and auth-role expectations
