# ADR-003: Shared HTTP Auth Boundary and Viz Exposure

## Status

Accepted

## Context

`ferrosa-memory-mcp` started as a local-first stdio MCP server. The repo now also contains an HTTP transport and a visualization server, but the current shared-service posture is not acceptable:

- HTTP auth is wired through a permissive validator in `crates/ferrosa-memory-mcp/src/main.rs`.
- Viz runs as a separate listener using a stdio-style tenant context rather than request auth.
- The service exposes only a generic `/health` endpoint, not a readiness contract.

The project needs a clear production boundary before shipping a shared endpoint.

## Decision

For shared HTTP deployments:

1. Require real authenticated principals mapped to tenants.
2. Require TLS.
3. Treat viz as a separate operational surface, disabled by default on shared deployments.
4. Allow fixed/default tenant behavior only for stdio/local development.

## Consequences

- The public HTTP surface becomes smaller and easier to secure.
- Local workflows remain simple: stdio continues to work without extra auth plumbing.
- Viz remains available for development and operator use, but no longer inherits the trust model of the public MCP endpoint.
- Future work can split viz into a dedicated crate if the UI grows, but deployment safety does not depend on that refactor.
