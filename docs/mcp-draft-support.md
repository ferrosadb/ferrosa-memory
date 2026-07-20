# MCP Draft Protocol Support

Ferrosa Memory supports the MCP draft per-request HTTP model for shared HTTP deployments. Modern MCP clients can discover capabilities without a legacy `initialize` session, then call tools, prompts, resources, and task subscriptions with explicit protocol metadata on each request.

This support is intentionally scoped to the draft features Ferrosa Memory exposes today. It is not a full conformance suite for every draft feature.

## Supported draft surface

| Area | Support |
|------|---------|
| Protocol version | `2026-07-28` |
| Discovery | `server/discover` |
| Tools | `tools/list`, `tools/call` |
| Prompts | `prompts/list`, `prompts/get` |
| Resources | `resources/list`, `resources/read` |
| Subscriptions | `subscriptions/listen` for task resources over Server-Sent Events |
| Result envelope | `resultType`, `_meta.io.modelcontextprotocol/serverInfo`, `ttlMs`, `cacheScope` |

Legacy MCP clients can continue to use `initialize`, `tools/list`, and `tools/call` without draft headers.

## HTTP request contract

Draft requests are sent to the shared HTTP endpoint:

```text
POST /mcp
```

Every draft request must include:

- `MCP-Protocol-Version: 2026-07-28`
- `Mcp-Method: <json-rpc method>`
- `params._meta.io.modelcontextprotocol/protocolVersion: "2026-07-28"`
- `params._meta.io.modelcontextprotocol/clientCapabilities: {}` or a client capability object

Requests whose method names a specific item also require `Mcp-Name`:

| Method | `Mcp-Name` value |
|--------|------------------|
| `tools/call` | Tool name, for example `hybrid_search` |
| `prompts/get` | Prompt name, for example `resume` |
| `resources/read` | Resource URI |

When a `Mcp-Name` value contains characters that are unsafe for HTTP header values, encode it as:

```text
=?base64?<URL_SAFE_NO_PAD_BASE64_VALUE>?=
```

For example, `ferrosa-memory://tasks/<session>/current` should usually be sent as an encoded `Mcp-Name` header even though the JSON body still contains the plain URI.

Shared HTTP mode also applies the normal HTTP deployment rules: TLS, authentication, origin validation, request-size limits, and per-IP rate limiting.

## Common shell setup

The examples below assume the server is reachable on loopback. For production, use HTTPS and a real principal from your `http-auth.toml`.

```bash
export FMEM_URL="http://127.0.0.1:8765/mcp"
export FMEM_AUTH="ferrosa_user:ferrosa_user"
export MCP_VERSION="2026-07-28"

mcp_post() {
  method="$1"
  body="$2"
  name="${3:-}"

  args=(
    -sS
    -u "$FMEM_AUTH"
    -H "Content-Type: application/json"
    -H "Accept: application/json"
    -H "MCP-Protocol-Version: $MCP_VERSION"
    -H "Mcp-Method: $method"
  )

  if [ -n "$name" ]; then
    args+=( -H "Mcp-Name: $name" )
  fi

  curl "${args[@]}" --data "$body" "$FMEM_URL"
}
```

## Discover capabilities

Use `server/discover` before assuming draft features are available.

```bash
mcp_post server/discover '{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "server/discover",
  "params": {
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {},
      "io.modelcontextprotocol/clientInfo": {
        "name": "example-client",
        "version": "0.1.0"
      }
    }
  }
}' | jq .
```

Expected highlights:

```json
{
  "result": {
    "supportedVersions": ["2026-07-28"],
    "capabilities": {
      "tools": {},
      "prompts": { "listChanged": false },
      "resources": { "subscribe": true }
    },
    "resultType": "complete",
    "cacheScope": "private"
  }
}
```

## List and call tools

```bash
mcp_post tools/list '{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/list",
  "params": {
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}' | jq '.result.tools[].name'
```

Call a tool with `Mcp-Name` set to the tool name:

```bash
mcp_post tools/call '{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "hybrid_search",
    "arguments": {
      "query": "current task context",
      "k": 5
    },
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}' hybrid_search | jq .
```

## Use prompts as compact workflows

Ferrosa Memory exposes prompts for common memory workflows such as `recall`, `resume`, `forget`, and `remind`.

```bash
mcp_post prompts/list '{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "prompts/list",
  "params": {
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}' | jq '.result.prompts[] | {name, description}'
```

Resolve the `resume` prompt:

```bash
mcp_post prompts/get '{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "prompts/get",
  "params": {
    "name": "resume",
    "arguments": {},
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}' resume | jq .
```

## Read task resources

Task resources let clients inspect active work without first selecting a tool. `resources/list` returns session resources and, when a workspace is supplied, a workspace-wide active-task resource.

```bash
WORKSPACE="$(pwd)"

mcp_post resources/list "$(jq -n --arg workspace "$WORKSPACE" '{
  jsonrpc: "2.0",
  id: 6,
  method: "resources/list",
  params: {
    workspace: $workspace,
    _meta: {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}')" | jq '.result.resources[] | {uri, title}'
```

Read the current-task resource for a known session:

```bash
SESSION_ID="00000000-0000-0000-0000-000000000000"
RESOURCE_URI="ferrosa-memory://tasks/$SESSION_ID/current"
HEADER_NAME="=?base64?$(printf '%s' "$RESOURCE_URI" | base64 | tr '+/' '-_' | tr -d '=\n')?="

mcp_post resources/read "$(jq -n --arg uri "$RESOURCE_URI" '{
  jsonrpc: "2.0",
  id: 7,
  method: "resources/read",
  params: {
    uri: $uri,
    _meta: {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}')" "$HEADER_NAME" | jq .
```

## Subscribe to workspace task updates

`subscriptions/listen` opens an SSE stream and sends a `notifications/resources/updated` event whenever a subscribed task resource changes.

A common agent integration is:

1. Encode the current working directory as a workspace task resource URI.
2. Start `subscriptions/listen` with `Accept: text/event-stream`.
3. When a `notifications/resources/updated` event arrives, call `resources/read` for that URI.
4. Reconcile the new task snapshot into the agent's local state.

Example:

```bash
WORKSPACE="$(pwd)"
WORKSPACE_B64="$(printf '%s' "$WORKSPACE" | base64 | tr '+/' '-_' | tr -d '=\n')"
RESOURCE_URI="ferrosa-memory://tasks/workspaces/$WORKSPACE_B64/active"

curl -sS -N \
  -u "$FMEM_AUTH" \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -H "MCP-Protocol-Version: $MCP_VERSION" \
  -H "Mcp-Method: subscriptions/listen" \
  --data "$(jq -n --arg uri "$RESOURCE_URI" '{
    jsonrpc: "2.0",
    id: 8,
    method: "subscriptions/listen",
    params: {
      notifications: {
        resourceSubscriptions: [$uri]
      },
      _meta: {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {
          name: "example-client",
          version: "0.1.0"
        }
      }
    }
  }')" \
  "$FMEM_URL"
```

The stream first acknowledges the subscription:

```text
event: message
data: {"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged",...}
```

When any active task for that workspace changes, the stream emits:

```text
event: message
data: {"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"ferrosa-memory://tasks/workspaces/.../active",...}}
```

SSE keepalive comments are emitted periodically:

```text
:
```

Clients should ignore keepalive comments and parse both LF and CRLF-delimited SSE frames.

## Example: keep an agent's task state fresh

A modern Goose/Ferrosa integration can keep memory task state current without polling every turn:

```text
agent starts in /repo
  -> server/discover
  -> subscriptions/listen ferrosa-memory://tasks/workspaces/<base64(/repo)>/active

another session creates or completes a task for /repo
  -> ferrosa-memory emits notifications/resources/updated

agent receives update
  -> resources/read ferrosa-memory://tasks/workspaces/<base64(/repo)>/active
  -> updates its local task board / resume hints
```

This is useful for:

- multi-agent coordination across the same repository;
- resuming work after compaction or restart;
- waking an agent UI when a dependency task becomes unblocked;
- detecting that another session created follow-up work for the current workspace.

## Errors to expect

Draft validation fails loud so clients do not accidentally mix protocol modes.

| Problem | Typical error |
|---------|---------------|
| Missing `MCP-Protocol-Version` | `missing MCP-Protocol-Version header` |
| Missing `Mcp-Method` | `missing Mcp-Method header` |
| Header method does not match JSON-RPC method | `Mcp-Method header (...) does not match JSON-RPC method (...)` |
| Missing body protocol metadata | `missing params._meta.io.modelcontextprotocol/protocolVersion` |
| Missing client capabilities metadata | `missing params._meta.io.modelcontextprotocol/clientCapabilities` |
| Unsupported draft version | `Unsupported MCP protocol version` |
| Missing `Mcp-Name` for named requests | `missing Mcp-Name header` |
| Subscription request without SSE accept header | `subscriptions/listen requires Accept: text/event-stream` |

Use legacy MCP requests when integrating with older clients; use the draft headers and `_meta` fields consistently when integrating with draft clients.
