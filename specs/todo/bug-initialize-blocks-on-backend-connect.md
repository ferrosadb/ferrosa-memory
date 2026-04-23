---
type: bug
priority: P1
status: draft
created: 2026-04-19
updated: 2026-04-20
reported-by: frg fmem-skill-ingest failing to complete MCP handshake (2026-04-19)
---

# `initialize` blocks ~20s on synchronous CQL + graph handshakes

## Observed

Running `frg fmem-skill-ingest` against a freshly built `ferrosa-memory-mcp`
binary (stdio transport) fails with:

```
Error: fmem initialize: fmem call timed out
```

frg's stdio client caps individual calls at 10 seconds
(`tools/forge/crates/fmem-client/src/transport/stdio.rs:35` —
`DEFAULT_TIMEOUT: Duration = Duration::from_secs(10)`).

Measuring the MCP `initialize` response time directly by piping
JSON-RPC into the binary:

```
--- elapsed: 20.035534858703613s ---
```

During those 20 seconds the server's stderr emits three sequential
blocking events:

```
 WARN ferrosa_memory_mcp: CQL connection failed (CQL session build timed
       out (10s) — is Ferrosa running?), starting in reconnecting mode —
       tools will return errors until connection is established
 WARN ferrosa_memory_mcp: graph connection failed (error sending request
       for url (http://localhost:17474/graph/health)), graph traversals
       disabled
 WARN ferrosa_memory_mcp: failed to load intentions from storage
       error=CQL connection not yet established, retrying in background...
 WARN ferrosa_memory_mcp: viz server error: Address already in use
       (os error 48)
```

**Environment at time of observation:**
- ferrosa cluster up and reachable: `nc -z localhost 19042` succeeds on
  19042–19044 (nodes 1-3).
- `FERROSA_MEMORY_CONFIG` pointed at
  `examples/ferrosa-memory.toml` with
  `contact_points = ["localhost:19042", "localhost:19043", "localhost:19044"]`.
- No "config not found" warning — config is being read.

## Why it matters

The MCP `initialize` response should complete in well under a second.
MCP clients (Claude Code, `frg`, MCP Inspector) enforce short per-call
timeouts to fail fast on broken servers — 5–15s is typical.
A 20-second blocking initialize means every cold client invocation
fails out of the gate against stdio transport.

This specifically breaks `frg fmem-skill-ingest` end-to-end: with no
successful `initialize`, the skill catalog cannot be re-ingested against
a freshly built fmem binary. The previous release reached initialize
within ~3s (observed earlier today pre-fix), so this is a regression.

## Hypothesis

Two distinct problems compound:

### Problem A — `initialize` is synchronous over backend connects

The code comment `starting in reconnecting mode — tools will return
errors until connection is established` shows the design intent:
**`initialize` should succeed even when CQL/graph are unreachable**,
and tool calls should surface the error instead. But the current
implementation still waits the full CQL session-build timeout (10s)
before entering reconnecting mode. Then it synchronously probes the
graph (+10s). Only then does `initialize` return.

The fix: move CQL + graph + viz startup to a background task. Return
from `initialize` as soon as MCP handshake is complete. Tool calls
already handle "CQL connection not yet established" — those paths are
the intended surface for unavailable-backend errors.

### Problem B — CQL session build times out even with a reachable cluster

The ferrosa cluster is reachable (`nc`/TCP-level connect succeeds to
19042-19044) yet the CQL driver reports `session build timed out (10s)`.
Possible causes:

- Driver version regression — the CQL driver used by fmem may have
  changed between the "pre-fix" and "post-fix" builds and now fails
  handshake against the ferrosa cluster.
- Protocol-version mismatch between driver and cluster after ferrosa
  was rebuilt.
- Config routing — the binary may be connecting to the correct host:port
  but the cluster rejecting the session-build step (authentication,
  keyspace, or schema introspection).

Investigation starter:

```bash
# Enable trace logging on the CQL session build
RUST_LOG=scylla=trace,ferrosa_memory_mcp=debug \
  FERROSA_MEMORY_CONFIG=.runtime/ferrosa-memory-http.toml \
  ./target/release/ferrosa-memory-mcp 2>&1 | head -200
```

Check whether the driver completes the CQL options/startup/register
frames or where it hangs.

## Reproduction

```bash
# With ferrosa cluster up (podman compose up) and fmem built fresh:
printf '{"jsonrpc":"2.0","id":1,"method":"initialize",\
"params":{"protocolVersion":"2024-11-05","capabilities":{},\
"clientInfo":{"name":"probe","version":"0"}}}\n' | \
  FERROSA_MEMORY_CONFIG=./examples/ferrosa-memory.toml \
  ./target/release/ferrosa-memory-mcp

# Observe: ~20s wait before the initialize result is written to stdout.
```

Or exercise via frg (fails at the 10s client timeout):

```bash
FERROSA_MEMORY_CONFIG=./examples/ferrosa-memory.toml \
  frg fmem-skill-ingest --root skills \
    --server './target/release/ferrosa-memory-mcp'
# Error: fmem initialize: fmem call timed out
```

## Related

- Previously filed:
  `bug-ingest-skill-bulk-nondeterminism.md` — blocks bulk-run
  verification. This bug (initialize blocked) now blocks *any* stdio
  invocation, so it is a hard prerequisite.
- Previously filed:
  `bug-content-hash-clobbered-by-partial-entity-updates.md` — also CQL-
  related; may share a common root cause if the recent ferrosa rebuild
  changed CQL defaults.

## Proposed fix (Problem A — primary)

Split `initialize` work into two phases:

1. **Synchronous (inside `initialize` handler):**
   - Parse + validate config
   - Register MCP tool/resource schemas
   - Return `initialize` result
2. **Background (`tokio::spawn`):**
   - CQL session build (with reconnect loop)
   - Graph health probe
   - Intentions warm-load
   - Viz server bind

Tool handlers already check "CQL connection not yet established" —
return that as a tool-level error until the background connect
succeeds. This matches the existing "reconnecting mode" design.

This alone would drop cold-start `initialize` from 20s → sub-second
and restore `frg fmem-skill-ingest` end-to-end.

## Proposed fix (Problem B — secondary)

Investigate the 10s CQL session-build timeout against a reachable
cluster. Log the driver's handshake stages and identify whether the
stall is at options / startup / register / keyspace-use. Depending on
the stage, the fix is a driver config change, a ferrosa-side protocol
fix, or a version pin.

## Acceptance

- `initialize` returns within 1s on a cold-start stdio binary even
  with CQL/graph offline.
- `frg fmem-skill-ingest --root skills` completes the full pipeline
  against the running ferrosa cluster (post fixing B, or with B still
  open and tool calls returning "backend unavailable" errors gracefully).
- No regression on existing HTTP-transport flows.
