# Project Plan: MCP 2026-07-28 Release Support

> Last updated: 2026-07-17
> Status: implementation in progress
> Source: MCP draft spec, local PDF `~/Desktop/Model Context Protocol Release.pdf`, /project-management

## Executive Summary

Ferrosa Memory is operationally close to the 2026-07-28 MCP direction because its shared HTTP service already exposes a single authenticated `POST /mcp` endpoint and does not depend on an MCP session store. The base modern HTTP path is now implemented: `server/discover`, per-request version/header validation, required `_meta` validation, Base64-safe mirrored header decoding, Origin validation, modern result envelopes, cache metadata, and conservative tool `outputSchema` contracts. Legacy clients remain supported through the legacy dispatch path.

The near-term goal is a dual-era HTTP server by the July 28, 2026 final spec date: legacy clients keep working, modern clients can call statelessly, and we have credible conformance evidence plus a co-marketing demo around "stateless durable memory for MCP agents." Ferrosa Memory does not need a modern stdio path because it is operated as a long-running service. Tasks and MCP Apps are valuable follow-on showcases, but they should not block the base protocol compatibility milestone.

## Source Evidence

| Source | Notes |
|---|---|
| https://modelcontextprotocol.io/specification/draft | Primary authoritative draft specification entry point. |
| https://modelcontextprotocol.io/llms.txt | Machine-readable documentation index used to locate draft pages, extensions, and SEPs. |
| https://modelcontextprotocol.io/specification/draft/server/tools | Tool list/call result shapes, tool definition schema, `x-mcp-header`, `outputSchema`, and tool-name constraints. |
| `~/Desktop/Model Context Protocol Release.pdf` | 14-page deck created 2026-07-17. Summarizes the 2026-07-28 release candidate: stateless core, extensions, auth hardening, deprecation policy. |
| https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/ | Official release-candidate blog post. |
| https://modelcontextprotocol.io/specification/draft/changelog | Official draft changelog against `2025-11-25`. |
| https://modelcontextprotocol.io/specification/draft/basic/versioning | Per-request version negotiation and dual-era compatibility. |
| https://modelcontextprotocol.io/specification/draft/basic/transports/streamable-http | Modern Streamable HTTP header, validation, SSE, and compatibility rules. |
| https://modelcontextprotocol.io/specification/draft/server/discover | `server/discover` response shape and caching expectations. |

## Current Ferrosa Memory Posture

| Area | Current state | 2026-07-28 target |
|---|---|---|
| HTTP endpoint | `POST /mcp` exists; Basic auth maps principal to tenant. | Keep endpoint; add modern header and body metadata validation. |
| Protocol version | `dispatch::server_info()` advertises `2024-11-05`. | Advertise `2026-07-28` through `server/discover`; support legacy only in dual-era path. |
| Handshake | `initialize` and `notifications/initialized` are accepted. `initialize` reads `roots`. | Modern path has no handshake. Legacy path remains behind dual-era compatibility. |
| Request metadata | Modern HTTP handler validates version, method, name, required `_meta`, JSON-RPC id shape, and Base64 sentinel decoding for mirrored headers. | Keep this covered by conformance tests and update if the draft changes. |
| Discovery | No `server/discover`. | Add mandatory `server/discover` with supported versions, capabilities, server info, instructions, `ttlMs`, and `cacheScope`. |
| Result bodies | `tools/list` returns `{ "tools": [...] }`; tool calls return legacy MCP content. | Modern results include `resultType: "complete"` and cache fields on list/read responses. Tool calls include `resultType` and may later support `input_required`. Legacy results remain unchanged. |
| Tool definitions | Tool definitions expose `name`, `description`, `inputSchema`, and a conservative common `outputSchema` matching the current `structuredContent` envelope. | Audit names and schemas against the draft Tools page: JSON Schema 2020-12 default, no sensitive `x-mcp-header`, optional precise per-tool `outputSchema` only where stable. |
| Notifications | Historical HTTP notification behavior exists for `notifications/initialized`; task mutations emit process-local events for the visualizer. | Streamable HTTP has no core client notifications. `subscriptions/listen` is implemented for task-list resource updates; broader CDC streams should be added as explicit resources before advertising them. |
| Auth | Shared HTTP is TLS + Basic auth for private/internal deployments. OAuth authorization is not advertised as an implemented MCP capability. | For public co-marketing, add OAuth protected-resource metadata and a bearer-token path before claiming public OAuth support. |
| Extensions | Ferrosa Memory has durable session task tools and operator HTML surfaces, but not MCP Tasks or MCP Apps extensions. | Treat Tasks and Apps as phase-two extension demos after core compatibility. |

## Assumptions

| # | Assumption | Status | Impact if Wrong |
|---|---|---|---|
| A1 | Final spec ships on July 28, 2026 with no breaking change to the RC items listed here. | unvalidated | Sprint 1 may need a final-spec adjustment pass. |
| A2 | We should preserve legacy clients while adding modern 2026 support over the long-running HTTP service. Modern stdio is not required for Ferrosa Memory. | validated | If false, implementation can be simpler but may break current users. |
| A3 | Co-marketing requires demonstrable modern MCP support, not necessarily full Tasks and Apps support on day one. | unvalidated | If full extension support is required, the timeline expands by 1-2 sprints. |
| A4 | Ferrosa Memory is primarily an MCP server/resource server, not an OAuth client. | validated | Auth tasks focus on protected-resource metadata and server-side token validation, not client registration UX. |
| A5 | No schema migration is required for base protocol compatibility. | unvalidated | Tasks extension persistence may require migrations later. |

## Dependencies

| # | Dependency | Owner | Due Date | Status |
|---|---|---|---|---|
| D1 | Final MCP 2026-07-28 schema/types or conformance cases. | MCP WG | 2026-07-28 | pending |
| D2 | Decide public auth posture: Basic-auth internal only vs OAuth bearer for public demo. | Ferrosa tech lead | 2026-07-19 | pending |
| D3 | Pick co-marketing demo host/client: Codex, Claude Desktop/Code, or a lightweight reference client. | Product/engineering | 2026-07-19 | pending |
| D4 | Confirm whether Tasks/App extension support is launch-blocking or follow-on. | Product/engineering | 2026-07-19 | pending |

## Risk Register

| # | Risk | Probability | Impact | Mitigation | Owner |
|---|---|---|---|---|---|
| R1 | Header/body validation is implemented too loosely and creates proxy/security ambiguity. | M | H | Centralize modern request parsing and add negative tests for missing/mismatched headers, Base64 encoded names, and method/name disagreement. | Tech lead |
| R2 | Dual-era support becomes tangled with existing dispatch and breaks current clients. | M | H | Add an explicit protocol-era adapter before `dispatch`; keep legacy responses byte-compatible where tests assert them. | Tech lead |
| R3 | Result-shape changes break current tool consumers. | H | M | Gate modern `resultType`/cache fields by detected protocol era. Add legacy and modern snapshot tests. | Tech lead |
| R4 | Auth work expands into a full OAuth server/client project. | M | H | Scope Sprint 2 to protected-resource metadata and bearer-token verification only; keep full OAuth flows as backlog unless co-marketing requires them. | Tech lead |
| R5 | Tasks and Apps demos distract from base conformance. | M | M | Make extension work Sprint 3, after core modern protocol tests pass. | Product/engineering |
| R6 | Official conformance suite changes after RC. | M | M | Treat conformance as a CI lane and keep one "final spec adjustment" task open for July 28. | Tech lead |

## Sprint Plan

### Sprint 1: Modern Stateless Core (Priority: Critical)

**Goal:** Ferrosa Memory accepts 2026-07-28 requests and still serves existing legacy clients.
**Duration:** 1 week
**Definition of Done:** Modern protocol smoke tests pass over HTTP; existing legacy tests still pass; unsupported/malformed modern requests fail with spec-shaped errors. The stdio binary remains on the legacy path unless a future operator need emerges.

| # | Task | Description | Success Criteria | Tests | Estimate | Status |
|---|---|---|---|---|---|---|
| 1.1 | Protocol era adapter | Introduce a small adapter that classifies each request as modern, legacy, or invalid before calling dispatch. Modern is selected by `_meta.io.modelcontextprotocol/protocolVersion` and, for HTTP, `MCP-Protocol-Version`. Legacy is selected by `initialize`. | Adapter returns a typed request context with protocol version, client info, client capabilities, and era. | Unit tests for modern, legacy, missing version, unsupported version. | M | done |
| 1.2 | `server/discover` | Add `server/discover` to dispatch. Return `resultType: "complete"`, `supportedVersions`, capabilities, server info under `_meta`, instructions, `ttlMs`, and `cacheScope`. | Modern clients can discover Ferrosa Memory without `initialize`. | Dispatch unit test and HTTP integration test. | S | done |
| 1.3 | HTTP metadata headers | Validate `MCP-Protocol-Version`, `Mcp-Method`, and required `Mcp-Name` on modern Streamable HTTP requests before dispatch. Decode Base64 sentinel header values and return HTTP 400 with JSON-RPC `HeaderMismatch` `-32020` on mismatch. | Missing/mismatched headers are rejected; matching encoded/decoded headers reach dispatch. | HTTP tests for tools/list, tools/call, mismatch, missing header, Base64 `Mcp-Name`. | M | done |
| 1.4 | Unsupported protocol errors | Add `UnsupportedProtocolVersionError` `-32022` with supported/requested versions. Ensure modern clients can distinguish this from legacy failures. | Unknown versions return the spec-shaped error and supported versions list. | Unit and HTTP tests. | S | done |
| 1.5 | Modern result envelope | For modern requests, include `resultType: "complete"` on ordinary results and `_meta.io.modelcontextprotocol/serverInfo` where appropriate. Keep legacy response bodies unchanged. | `tools/list`, `tools/call`, and `server/discover` modern and legacy snapshots differ only by intentional modern fields. | Snapshot tests for `tools/list`, representative `tools/call`, and `server/discover`. | M | done |
| 1.6 | Tool schema audit | Validate all advertised tool names and `inputSchema` objects against the draft Tools page. Prefer `{ "type": "object", "additionalProperties": false }` for no-parameter tools where practical. Do not add `x-mcp-header` to sensitive fields. | Modern `tools/list` contains only spec-valid tool definitions; no custom `x-mcp-header` annotations are advertised. | Snapshot/schema validation test over the full tool catalog. | M | done: common `outputSchema` snapshot added; no custom `x-mcp-header` annotations |
| 1.7 | Client/eval updates | Add a modern mode to `crates/ferrosa-memory-eval/src/mcp_client.rs` that sends per-request `_meta` plus required HTTP headers. Keep legacy tests. | Eval client can test both eras. | Existing eval tests plus new modern HTTP smoke. | M | todo |

**Quality Gates:**
- [x] `cargo test -p ferrosa-memory-core modern_`
- [x] `cargo test -p ferrosa-memory-core modern_mcp_`
- [x] `cargo test -p ferrosa-memory-core get_or_delete_mcp_returns_method_not_allowed`
- [x] `cargo test -p ferrosa-memory-core tool_definitions_catalog_snapshot`
- [ ] `cargo test -p ferrosa-memory-eval mcp_client`
- [x] Legacy HTTP `tools/list` still works without modern headers

### Sprint 2: Cacheability, Change Streams, and Auth Posture (Priority: High)

**Goal:** Make Ferrosa Memory operationally credible for modern MCP hosts and gateways.
**Duration:** 1 week
**Definition of Done:** List/discovery responses are cache-aware, origin protection is explicit, and the public auth story is documented and testable.

| # | Task | Description | Success Criteria | Tests | Estimate | Status |
|---|---|---|---|---|---|---|
| 2.1 | Cacheable list results | Add `ttlMs` and `cacheScope` to modern `tools/list` and `server/discover`. Return tools in deterministic order. | Clients can cache tool discovery safely; no tenant-private data is marked public. | Snapshot tests and deterministic ordering test. | S | done |
| 2.2 | Capability truthfulness | Advertise only implemented capabilities. Do not advertise resources/prompts/subscriptions unless real methods exist. | `server/discover` capabilities match dispatch methods. | Capability snapshot plus unknown-method tests. | S | done |
| 2.3 | `subscriptions/listen` task stream | Implement Streamable HTTP `subscriptions/listen` for task-list resources. Acknowledge first, filter to requested resource URIs, tag notifications with `io.modelcontextprotocol/subscriptionId`, and keep the stream open until cancellation. | Task monitor clients can subscribe to `ferrosa-memory://tasks/{session_id}/current` or `/list` and receive `notifications/resources/updated` after task mutations. | Dispatch resource tests and HTTP SSE subscription test. | M | done |
| 2.4 | Origin validation | Enforce Streamable HTTP `Origin` validation with config for allowed origins. Keep local/dev defaults safe. | Invalid browser origins receive 403 before auth/dispatch. | HTTP tests for absent, loopback/same-host allowed, and denied Origin. | M | done |
| 2.5 | Protected resource metadata | Add OAuth protected-resource metadata discovery for HTTP deployments and include `WWW-Authenticate` `resource_metadata` on 401 when configured. | OAuth-capable clients can discover authorization server metadata; Basic-only internal deployments remain supported. | HTTP tests for 401 challenge and well-known endpoint. | M | backlog: required only before advertising public OAuth support |
| 2.6 | Tool output schemas | Add `outputSchema` only for stable, structured high-value tools. Preserve text content for compatibility and put JSON under `structuredContent` where useful. | Modern clients get better structured data without breaking existing text consumers. | Snapshot/schema tests for selected tools. | M | done: common envelope schema; precise per-tool schemas remain backlog B4 |
| 2.7 | Auth positioning doc | Update shared HTTP docs to state which mode is internal Basic auth and which mode is public/OAuth-ready. | Co-marketing claims do not overstate auth readiness. | Docs review. | S | done in this plan: internal Basic auth only; public OAuth not advertised |

**Quality Gates:**
- [x] Security negative tests for origin and header validation
- [x] `cargo test -p ferrosa-memory-core --lib modern_resources`
- [x] `cargo test -p ferrosa-memory-core --lib modern_mcp_subscriptions`
- [x] Docs updated with public vs internal auth posture
- [x] No capability is advertised without a working method

### Sprint 3: Extensions Showcase (Priority: Medium)

**Goal:** Turn Ferrosa Memory's existing product shape into credible MCP extension stories.
**Duration:** 2 weeks
**Definition of Done:** At least one extension path is demoable without weakening base conformance.

| # | Task | Description | Success Criteria | Tests | Estimate | Status |
|---|---|---|---|---|---|---|
| 3.1 | Tasks extension mapping | Map long-running operations such as consolidation, remote pulls, bulk ingest, and eval runs onto `io.modelcontextprotocol/tasks` handles. | A tool can return a task handle; client can call `tasks/get`, `tasks/update`, and `tasks/cancel`. | Unit tests for lifecycle and cancellation. | L | todo |
| 3.2 | Task persistence model | Decide whether MCP Tasks use existing `session_task` tables or a new protocol-task table. If new table, add an ordered migration. | Task handles are tenant-scoped, restart-safe, and do not leak across clients. | Migration test and tenant isolation test. | M | todo |
| 3.3 | MCP Apps preview | Package a read-only Memory Workbench or graph explorer view as an MCP App template if the extension spec is stable enough. | A host can prefetch/cache the template and UI actions route through audited JSON-RPC calls. | Static asset integrity test plus manual host demo. | L | todo |
| 3.4 | Extension negotiation | Add `extensions` to modern capabilities only for extensions that are implemented and tested. | Clients can opt in; non-supporting clients fall back to core protocol behavior. | Capability tests for with/without extension support. | M | todo |

**Quality Gates:**
- [ ] Extension features are opt-in and do not change core request behavior
- [ ] Tenant isolation tests cover task/app action paths
- [ ] Extension support can be disabled by config if host compatibility is uncertain

### Sprint 4: Conformance, Release, and Co-Marketing (Priority: High)

**Goal:** Ship credible public evidence that Ferrosa Memory supports the new MCP release.
**Duration:** 1 week, can overlap after Sprint 1 stabilizes
**Definition of Done:** A demo, compatibility matrix, and release notes are ready for July 28, 2026.

| # | Task | Description | Success Criteria | Tests | Estimate | Status |
|---|---|---|---|---|---|---|
| 4.1 | Conformance harness | Add MCP 2026-07-28 scenario tests for discovery, version errors, header validation, modern tools/list, and a representative tools/call. | CI has a named modern-MCP lane. | `cargo test` lane plus any official conformance suite once available. | M | todo |
| 4.2 | Compatibility matrix | Document supported clients and transports: legacy stdio, legacy HTTP, modern Streamable HTTP, auth modes, extensions. | Sales/support can answer "does it work with X?" without guessing. | Docs review. | S | todo |
| 4.3 | Demo script | Build a short demo: stateless request to `hybrid_search`, any-instance routing story, cacheable `tools/list`, and durable memory recall. | Demo can be run locally and recorded. | Manual runbook with expected outputs. | M | todo |
| 4.4 | Co-marketing messaging | Prepare copy: "Ferrosa Memory is a durable, stateless MCP-native memory layer for agents." Include honest caveats for extension and auth status. | Messaging is accurate against the compatibility matrix. | Product/engineering review. | S | todo |
| 4.5 | Release notes | Publish internal/external release notes tied to the July 28 final spec. | Notes link spec sources, version support, migration notes, and upgrade steps. | Docs review. | S | todo |

**Quality Gates:**
- [ ] Modern MCP smoke runs green
- [ ] Legacy clients still have a tested path
- [ ] Release note claims match conformance evidence
- [ ] Co-marketing demo has a deterministic runbook

## Co-Marketing Angle

Lead with the architectural fit:

- Ferrosa Memory turns the MCP stateless shift into a product advantage: durable state lives in Ferrosa, while MCP requests stay self-contained and horizontally routable.
- Explicit model-visible handles match Ferrosa Memory's existing design: sessions, task IDs, fold IDs, entity IDs, remote IDs, and forget tokens are ordinary arguments rather than hidden transport state.
- Cacheable discovery is especially strong for a large tool catalog: deterministic `tools/list` plus `ttlMs/cacheScope` lets clients reduce prompt churn while Ferrosa Memory keeps rich capabilities discoverable.
- Trace/context metadata aligns with the existing metrics and workbench story; add OpenTelemetry trace propagation as a follow-on if needed for enterprise demos.

Do not claim full public OAuth or Tasks/App extension support until those lanes are actually implemented. A defensible launch claim is:

> Ferrosa Memory supports the MCP 2026-07-28 stateless server model with durable, tenant-scoped agent memory over a horizontally deployable HTTP service.

## Backlog

| # | Item | Description | Priority | Source |
|---|---|---|---|---|
| B1 | OpenTelemetry trace propagation | Preserve `traceparent`, `tracestate`, and `baggage` from request `_meta` into logs/spans. | M | MCP SEP-414 |
| B2 | Full OAuth bearer verifier | Replace or augment Basic auth with JWT/JWKS bearer-token validation for public shared service. | H | MCP authorization hardening |
| B3 | Full resource/prompt surfaces | Add resources/prompts only if there is a real product need; otherwise keep capabilities tools-only. | L | MCP capabilities |
| B4 | Full structured output coverage | After the selected Sprint 2 tools prove stable, expand `outputSchema` and `structuredContent` coverage across the rest of the catalog where it improves clients. | M | JSON Schema 2020-12 |
| B5 | Official conformance suite integration | Wire upstream conformance scenarios once the final suite is published. | H | MCP release process |
| B6 | MCP Apps design review | Security-review sandboxed app templates before enabling remote hosts. | M | MCP Apps extension |

## Quality Standards

- New protocol code must have modern and legacy tests when behavior differs.
- Header validation must be centralized, covered by negative tests, and use case-insensitive header-name comparisons.
- Capabilities must be truthful: no advertised method or extension without a working implementation.
- Auth claims must be verified by tests and docs; internal Basic auth and public OAuth-ready mode must not be conflated.
- Any schema/table change for Tasks persistence must follow `AGENTS.md`: versioned, automatic, ordered, data-preserving migration.

## Estimation Methodology

- Unit: T-shirt sizes.
- Basis: decomposition against the current Rust dispatch/HTTP code and MCP draft requirements.
- Buffer: Sprint 1 includes compatibility-buffer tasks because final spec lands 2026-07-28; extension work is separated to avoid blocking base conformance.
- Velocity: TBD; Sprint 1 is scoped to a single focused engineer-week plus review.
