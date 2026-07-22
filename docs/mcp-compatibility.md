# MCP compatibility matrix

Ferrosa Memory supports both existing MCP clients and the stateless HTTP model proposed for the `2026-07-28` draft. The draft is not final until the MCP project publishes the final schema and conformance suite; this matrix describes the implementation and evidence in this repository.

| Surface | Status | Evidence and limits |
|---|---|---|
| Legacy stdio | Supported | `initialize`, `notifications/initialized`, tools, and current client workflows remain unchanged. |
| Legacy HTTP | Supported | Stable protocol headers do not trigger draft-only validation. Regression coverage protects Claude Code, Goose, and other stable clients. |
| Draft Streamable HTTP | Supported profile | Stateless `POST /mcp`, `server/discover`, per-request version/capability metadata, mirrored headers, modern result envelopes, and cache metadata are tested. |
| Tools | Supported | Deterministic `tools/list`, `tools/call`, input schemas, and a conservative structured-output envelope. Precise per-tool output schemas remain follow-on work. |
| Prompts | Supported subset | Static `forget`, `resume`, `recall`, and `remind` workflows through `prompts/list` and `prompts/get`. |
| Resources | Supported subset | Session task resources and workspace-wide active-task resources through `resources/list` and `resources/read`. |
| Subscriptions | Supported subset | `subscriptions/listen` streams updates for the task resources above. Other resource and catalog notifications are not advertised. |
| Progress and cancellation | Test fixture only | An opt-in, authenticated fixture validates request-scoped SSE progress and cancellation behavior. It is absent from production builds. |
| Authentication | Internal/shared-service mode | TLS plus HTTP Basic authentication maps principals to tenants. Public OAuth protected-resource metadata and bearer verification are not implemented or advertised. |
| MCP Tasks extension | Not implemented | Ferrosa Memory session tasks are a product workflow, not the `io.modelcontextprotocol/tasks` extension. Durable extension handles and `tasks/get`, `tasks/update`, and `tasks/cancel` remain follow-on work. |
| MCP Apps | Not implemented | The workbench and visualizer are not advertised as MCP Apps. |
| Modern stdio | Not targeted | Ferrosa Memory's stateless draft profile targets its long-running shared HTTP service. |
| Official final-suite certification | Pending upstream | Run an adjustment and official-suite pass after the final specification and suite are published on July 28, 2026. |

## Machine-readable discovery

Call `server/discover` using the draft request metadata described in [MCP draft protocol support](mcp-draft-support.md). Its supported-profile metadata is authoritative for methods currently implemented by a running server. Clients must not infer support for every draft feature from the advertised protocol version.

## Release claim

A defensible pre-final claim is:

> Ferrosa Memory supports the MCP `2026-07-28` draft stateless server profile over authenticated Streamable HTTP while preserving legacy MCP clients.

Do not describe this as final-spec certification, public OAuth support, or support for the MCP Tasks or Apps extensions until those claims have corresponding implementation and conformance evidence.
