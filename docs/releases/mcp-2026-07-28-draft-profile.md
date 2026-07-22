# MCP `2026-07-28` draft-profile support

**Status:** release candidate evidence as of July 22, 2026

**Scope:** Ferrosa Memory shared HTTP service

Ferrosa Memory now supports the stateless server model proposed by the MCP `2026-07-28` draft while preserving legacy stdio and HTTP clients.

## Included

- Stateless `POST /mcp` requests with protocol version, client capabilities, and client identity carried per request.
- `server/discover` with supported versions, truthful capabilities, server identity, cache metadata, and a machine-readable supported profile.
- Required Streamable HTTP header validation, including encoded `Mcp-Name` values and spec-shaped mismatch/version errors.
- Modern `resultType` envelopes and cacheable deterministic list responses without changing legacy response shapes.
- Tools plus scoped prompts and task resources.
- `subscriptions/listen` for session and workspace task-resource updates.
- Explicit Origin validation and stable-client regression coverage.
- A modern mode in the eval HTTP client and a named CI smoke covering discovery, list/call, subscriptions, and deterministic progress/cancellation fixtures.

## Compatibility and security

Legacy clients continue through the legacy protocol path. Draft validation is selected only when the request negotiates the draft or invokes a draft-only method.

Shared HTTP deployments currently use TLS plus HTTP Basic authentication to map principals to tenants. Ferrosa Memory does not advertise public OAuth, the MCP Tasks extension, or MCP Apps. See the [compatibility matrix](../mcp-compatibility.md) for the complete claim boundary.

## Verification

The `MCP Draft Conformance` CI job runs focused modern/legacy regression tests, eval-client request construction tests, and the deterministic end-to-end draft-profile smoke. The fixture endpoints are feature-gated, authenticated, and absent from default production builds.

For a reproducible walkthrough, use the [demo runbook](../mcp-draft-demo.md). Protocol details and request examples are in [MCP draft protocol support](../mcp-draft-support.md).

## Upgrade notes

- No database migration is introduced by this release evidence work.
- Existing clients do not need to adopt the draft immediately.
- New draft clients should call `server/discover` and honor its supported profile rather than assuming every draft feature is available.
- Operators should not enable fixture features in production artifacts.

## Before a final-standard claim

After the MCP project publishes the final specification and official conformance suite, Ferrosa Memory must compare the final schema against this implementation, apply any compatibility adjustments, run the upstream suite, and update the compatibility matrix. Until then, describe this release as support for the `2026-07-28` **draft profile**, not final-spec certification.
