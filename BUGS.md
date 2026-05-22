# Ferrosa / Ferrosa-Memory Bug Tracker

Open issues found during integration testing of ferrosa-memory-mcp against Ferrosa DB.
Bugs are filed against whichever component owns the fix.

Reproduction tests for ferrosa-protocol issues: `crates/ferrosa-memory-core/tests/ferrosa_bugs.rs`

```sh
cargo test -p ferrosa-memory-core --test ferrosa_bugs -- --ignored --nocapture
```

---

## Open — ferrosa-memory-mcp

### BUG-M-001 · P1 · HTTP 504 on all tool calls during ANN index warmup

**Component:** ferrosa-memory-mcp (root cause: ferrosa — see BUG-F-001)  
**Version:** ≤ 0.9.0

**Description:** After any ferrosa restart, the ANN vector index takes
approximately 300 seconds to rebuild in memory against an entity store with
3,600+ entities. During this window, every CQL query blocks until it times out,
causing ferrosa-memory-mcp to return HTTP 504 Gateway Timeout for all MCP tool
calls — including non-vector tools like `retrieve_entities` and
`check_intentions`.

**Impact:** ferrosa-memory-mcp is fully unavailable for several minutes after
any restart. MCP clients see tool errors with no actionable message. The
first batch of any ingest script is silently dropped.

**Workaround:**
- Wait ~5 minutes after restart before issuing any MCP calls.
- In ingest scripts: retry with 30s/60s backoff, cap batch size at 2 on the
  first pass after restart.

**Blocked on:** ferrosa BUG-F-001 (ANN index cold-load blocking all queries).

---

### BUG-M-002 · P2 · `ferrosa_user` permission loss not detected — writes fail silently

**Component:** ferrosa-memory-mcp  
**Version:** ≤ 0.9.0

**Description:** After a ferrosa restart (which clears all GRANTs — see
BUG-F-002), `smart_ingest` and `ingest_entities` return success responses
(`inserted: 1, skipped: 0`) but insert nothing. No error is surfaced in the MCP
response or tool output.

**Reproduce:**
1. Restart ferrosa without re-granting permissions.
2. Call `smart_ingest` with a new entity.
3. Observe: tool returns `{"inserted": 1}`.
4. Call `retrieve_entities` with the same entity ID.
5. Observe: entity not found.

**Impact:** Ingest appears to succeed. Data is silently dropped. The only signal
is a later retrieval miss.

**Workaround:** Always re-grant permissions after a ferrosa restart before
starting ferrosa-memory-mcp:

```python
from cassandra.cluster import Cluster
from cassandra.auth import PlainTextAuthProvider
s = Cluster(['127.0.0.1'], port=9042,
            auth_provider=PlainTextAuthProvider('ferrosa_admin', 'ferrosa_admin'),
            protocol_version=4).connect('agent_memory')
s.execute("GRANT ALL PERMISSIONS ON KEYSPACE agent_memory TO ferrosa_user")
```

---

### BUG-M-003 · P2 · `null` text columns deserialize as `""` not `null` in tool responses

**Component:** ferrosa-memory-mcp  
**Version:** 0.9.0

**Description:** When an optional text field was written as CQL `null`,
`task_get`, `task_list`, `task_board`, and entity retrieval tools return the
field as an empty string `""` rather than JSON `null`. Affects all optional
string fields: `body`, `assignee`, `reviewer`, `block_reason`, `result`,
`summary`.

**Root cause:** scylla 0.15 legacy result API (`into_legacy_result()`)
deserializes null text columns as empty strings in untyped column iteration.

**Impact:** Consumers cannot distinguish "field not set" from "field set to
empty string." Affects display logic and conditional checks.

**Workaround:** Treat `""` as absent when reading optional string fields.

---

### BUG-M-004 · P3 · HTTP server binds IPv4 only — `localhost` fails on macOS when SSH tunnel is active

**Component:** ferrosa-memory-mcp  
**Version:** ≤ 0.9.0

**Description:** ferrosa-memory-mcp binds `127.0.0.1:18765` (IPv4 only). On
macOS, `localhost` resolves to `::1` (IPv6) before `127.0.0.1`. If any SSH
tunnel is bound to `::1:18765`, connections to `http://localhost:18765/mcp`
route to the tunnel rather than the MCP server.

**Reproduce:**
```bash
ssh remote -L 18766:localhost:18766 &   # binds ::1:18766 on macOS
curl http://localhost:18765/mcp         # may silently hit the wrong listener
```

**Workaround:** Always use `http://127.0.0.1:18765` (explicit IPv4) in all
configuration, never `http://localhost:18765`.

---

## Open — ferrosa

### BUG-F-001 · P1 · ANN index cold-load blocks all CQL queries for ~300s after restart

**Component:** ferrosa  
**Version:** ≤ 0.11.0

**Description:** On restart with a large entity store (3,600+ entities), ferrosa
rebuilds its HNSW vector index in memory before serving any queries. All CQL
queries block and time out during this window, causing cascading 504s in any
MCP client.

**Impact:** Any downstream service that depends on CQL — including
ferrosa-memory-mcp — is unavailable for minutes after every restart. The window
grows with entity count.

**Workaround:** Wait for node stabilization (confirmed by a successful CQL
health probe) before starting dependent services.

---

### BUG-F-002 · P0 · Role permissions not persisted across restarts

**Component:** ferrosa  
**Version:** ≤ 0.11.0

**Description:** `GRANT ALL PERMISSIONS ON KEYSPACE agent_memory TO ferrosa_user`
is not persisted. Every ferrosa process restart drops all grants. Writes from
sessions authenticated as `ferrosa_user` silently succeed with no rows inserted.

**Impact:** Silent data loss after any restart. The symptom is
indistinguishable from a successful write until a later retrieval miss reveals
the missing rows.

**Workaround:** Re-run the GRANT statement after every restart, before starting
ferrosa-memory-mcp.

---

### BUG-F-003 · P2 · PREPARE returns malformed metadata — breaks cassandra-driver

**Component:** ferrosa  
**Version:** ≤ 0.11.0

**Description:** The PREPARE response returns malformed column metadata.
`cassandra-driver` (Python) fails to parse the PREPARED response and the session
enters a broken state requiring reconnection.

**Impact:** Prepared statements cannot be used. Libraries that transparently
prepare queries will fail.

**Workaround:** Use literal string-interpolated CQL only. Escape single quotes
by doubling them (`'` → `''`). Never call `session.prepare()`.

---

### BUG-F-004 · P2 · Scientific notation in `list<float>` literals causes parse error

**Component:** ferrosa  
**Version:** ≤ 0.11.0

**Description:** CQL `list<float>` literals containing values in scientific
notation (e.g. `1.23e-05`) are rejected by the CQL parser with a syntax error,
despite being valid CQL per the Cassandra specification.

**Workaround:** Format all float values with fixed-point notation (`f"{v:.8f}"`
in Python).

---

### BUG-F-005 · P2 · `COMPACT` DDL not supported

**Component:** ferrosa  
**Version:** ≤ 0.11.0

**Description:** `COMPACT <keyspace>.<table>` and the `COMPACT STORAGE` clause
produce `unexpected token Keyword(Compact)`, preventing manual SSTable
compaction and limiting storage management.

**Workaround:** Use the ferrosa admin REST API to inspect SSTable counts:
`curl http://127.0.0.1:9090/api/storage`.

---

### BUG-F-006 · P3 · Bolt graph vertex label DDL syntax undocumented and non-functional

**Component:** ferrosa  
**Version:** ≤ 0.11.0

**Description:** `create_edge` and `explore_connections` require ferrosa's
embedded Bolt server to have vertex label tables registered via Cypher DDL.
None of the standard DDL forms work: `CREATE (n:Entity)`, `MERGE (n:Entity
{id:$id})`, `CALL schema.createVertexLabel('Entity')` all return errors. No
working DDL syntax is documented.

**Impact:** The graph layer (`create_edge`, `explore_connections`) is
non-functional for new installs. Edge inserts must bypass the MCP tool and write
directly to the `typed_edges` CQL table.

**Workaround:**
```cql
INSERT INTO agent_memory.typed_edges
  (tenant_id, session_id, src_id, edge_type, dst_id, weight, created_at)
VALUES (<tenant_uuid>, <session_uuid>, <src_uuid>, 'contains', <dst_uuid>, 1.0, <ts_ms>)
```

---

## Fixed in 0.9.0 (ferrosa-memory-mcp)

| # | Issue | PR |
|---|-------|----|
| `create_edge` MERGE omitted `tenant_id`/`session_id` from relationship key — edges silently dropped or unretriavable | [#27](https://github.com/ferrosadb/ferrosa-memory/pull/27) |
| `hybrid_search` did not cross session boundary — entities stored under nil-session UUID were invisible to live sessions | [#27](https://github.com/ferrosadb/ferrosa-memory/pull/27) |
| No `list_entities` tool for structured equality filtering (status, entity_type, assignee, properties) | [#27](https://github.com/ferrosadb/ferrosa-memory/pull/27) |
| First-use setup: circular bootstrap — MCP PREPARE statements issued before keyspace migrations run, causing infinite reconnect loop | [#28](https://github.com/ferrosadb/ferrosa-memory/pull/28) |

## Fixed in 0.11.0 (ferrosa)

| # | Issue | PR |
|---|-------|----|
| Default internode port 7000 collides with macOS ControlCenter | [#55](https://github.com/ferrosadb/ferrosa/pull/55) |
| `ferrosa.toml` config ignored when env vars absent | [#55](https://github.com/ferrosadb/ferrosa/pull/55) |
| Corrupt/empty `host_id` crashes startup or causes silent split-brain | [#55](https://github.com/ferrosadb/ferrosa/pull/55) |

## Historical (fixed prior to 0.9.0)

These issues were found during early integration and fixed on `feature/udf-uda-query-time`:

| # | Issue | Commit |
|---|-------|--------|
| `toJson()` CQL function missing — session startup hung | `c6872d9` |
| `pk_count` missing in PREPARE bind metadata — driver parse failure | `531027a` |
| Bind variable types not populated in PREPARE response | `6002f9c` |
| CQL protocol v5 response to v4 client — frame offset mismatch | `2be6d2b` |
| Positional bind values rejected in EXECUTE | `2be6d2b` |
| SSTable BTI trie partition index sign bit — point queries on 2nd+ partition returned corrupt data | `6d43056` |
| Prepared SELECT on compound PK panics in router | (uncommitted at time of report) |
