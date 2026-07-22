# MCP draft-profile demo runbook

This runbook demonstrates Ferrosa Memory's implemented `2026-07-28` draft profile without claiming final-spec certification. It uses only the authenticated shared HTTP service and deterministic test fixtures.

## 1. Verify the release gate

From the repository root:

```bash
cargo test -p ferrosa-memory-eval --lib http_client
cargo test -p ferrosa-memory-core \
  --features subscription-fixture,long-running-operation-fixture \
  --lib draft_profile_end_to_end_smoke
```

The first command proves that the eval client preserves legacy behavior and constructs valid stateless draft requests. The second exercises discovery, tools, a representative tool call, subscription acknowledgement/update, and completed/cancelled progress streams.

## 2. Start a fixture-enabled server

Use a development config with TLS/auth settings appropriate for your environment. The fixture endpoints remain authenticated and are compiled out of default production builds.

```bash
cargo run -p ferrosa-memory-mcp \
  --features subscription-fixture,long-running-operation-fixture -- \
  --config "$HOME/.config/ferrosa-memory.toml"
```

In another shell, set the endpoint and credentials:

```bash
export MCP_URL="${MCP_URL:-http://127.0.0.1:18765/mcp}"
export MCP_ORIGIN="${MCP_URL%/mcp}"
export MCP_USER="${MCP_USER:?set the configured HTTP username}"
export MCP_PASSWORD="${MCP_PASSWORD:?set the configured HTTP password}"
export MCP_VERSION=2026-07-28
```

## 3. Discover the supported profile

```bash
curl --fail-with-body --silent --show-error \
  --user "$MCP_USER:$MCP_PASSWORD" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -H "MCP-Protocol-Version: $MCP_VERSION" \
  -H 'Mcp-Method: server/discover' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"$MCP_VERSION\",\"io.modelcontextprotocol/clientCapabilities\":{},\"io.modelcontextprotocol/clientInfo\":{\"name\":\"ferrosa-demo\",\"version\":\"1\"}}}}" \
  "$MCP_URL" | jq .
```

Confirm `resultType` is `complete`, `supportedVersions` contains `2026-07-28`, and the supported-profile metadata lists only implemented methods.

## 4. Show cacheable tools and durable memory

Use the same `_meta` object for every request. First call `tools/list` with `Mcp-Method: tools/list`; confirm deterministic tools plus `ttlMs` and `cacheScope`. Then call `hybrid_search` with both `Mcp-Method: tools/call` and `Mcp-Name: hybrid_search`.

The full request templates are in [MCP draft protocol support](mcp-draft-support.md). For a live presentation, ingest a small named fact before the demo and search for it from a second stateless request. The requests carry no transport session; durable tenant-scoped state remains in Ferrosa.

## 5. Demonstrate progress and cancellation

```bash
curl --no-buffer --fail-with-body --silent --show-error \
  --user "$MCP_USER:$MCP_PASSWORD" \
  -H 'Content-Type: application/json' \
  -H 'Accept: text/event-stream' \
  --data '{"operationId":"demo-operation","totalSteps":3,"delayMs":100,"cancelAtStep":2}' \
  "$MCP_ORIGIN/_test/mcp/long-running-operation"
```

Expected sequence: two `notifications/progress` events followed by a terminal `resultType: "complete"` result with `status: "cancelled"` and `completedSteps: 2`. Omit `cancelAtStep` to demonstrate normal completion.

## 6. Demonstrate a resource subscription

Follow the authenticated `subscriptions/listen` example in [MCP draft protocol support](mcp-draft-support.md), then trigger the deterministic update endpoint from another shell:

```bash
curl --fail-with-body --silent --show-error \
  --user "$MCP_USER:$MCP_PASSWORD" \
  -H 'Content-Type: application/json' \
  --data "{\"uri\":\"$RESOURCE_URI\"}" \
  "$MCP_ORIGIN/_test/mcp/task-resource-update" | jq .
```

The subscription stream must acknowledge first and then emit `notifications/resources/updated` with the same resource URI and subscription ID.

## Presenter checklist

- Say **draft profile**, not final certification.
- Show `server/discover` before feature calls; it is the truthful capability boundary.
- Explain that Basic auth is for private/shared deployments, not public OAuth.
- Explain that the deterministic endpoints are test-only and absent from production builds.
- Keep MCP Tasks and MCP Apps out of the claim; see the [compatibility matrix](mcp-compatibility.md).
