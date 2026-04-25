---
type: feat
priority: P1
status: draft
created: 2026-04-22
updated: 2026-04-22
---

# feat: add bulk `ingest_entities` MCP surface

**Status:** todo
**Consumer:** forge and future non-CQL ingestors
**Created:** 2026-04-22
**Driving need:** bulk project/session ingest should be one server-owned MCP call, not a client-owned CQL subprocess contract.

## Goal

Add a first-class `ingest_entities` MCP tool that bulk-ingests entities and typed edges for one `(tenant_id, session_id)` scope in a single call.

The server owns:

- current CQL schema mapping
- direct app-table CQL writes
- embedding computation for missing vectors
- idempotency and conflict behavior
- structured per-row failure reporting

Clients send semantic payloads and stop doing direct CQL or Python-side loader orchestration.

## Request

```json
{
  "tenant_id": "UUID",
  "session_id": "UUID",
  "entities": [
    {
      "id": "UUID",
      "name": "string",
      "entity_type": "document|section|bug|code|...",
      "context": "string",
      "confidence": 0.0,
      "state": "active|resolved|...",
      "embedding": [0.123],
      "attrs": {}
    }
  ],
  "edges": [
    {
      "src_id": "UUID",
      "dst_id": "UUID",
      "edge_type": "depends_on|contains|...",
      "weight": 1.0,
      "metadata": {}
    }
  ],
  "options": {
    "embed_missing": true,
    "embedding_model": "nomic-embed-text",
    "on_conflict": "update|skip|error",
    "strict_edges": true,
    "dry_run": false
  }
}
```

## Response

```json
{
  "entities": {
    "inserted": 12,
    "updated": 3,
    "skipped": 0,
    "failed": [
      { "id": "UUID", "reason": "schema_mismatch: unknown attr 'foo'" }
    ]
  },
  "edges": {
    "inserted": 40,
    "skipped_duplicate": 0,
    "failed": [
      {
        "src_id": "UUID",
        "dst_id": "UUID",
        "edge_type": "depends_on",
        "reason": "endpoint_not_found: dst_id not resident and not in batch"
      }
    ]
  },
  "embeddings": {
    "computed": 10,
    "received": 2,
    "failed": []
  },
  "schema_version": "2026-03-01",
  "duration_ms": 1234
}
```

## Invariants

1. The server owns the storage schema. Clients send semantic fields; `ferrosa-memory` maps them to current app-table columns and current edge-write behavior.
2. There is no silent per-row drop. Every entity or edge failure must appear in `failed[]` with a structured reason.
3. `on_conflict = "update"` is idempotent for the same `(tenant_id, session_id, entity_id)` payload. `skip` preserves resident state. `error` surfaces the conflict.
4. The tool is a batch, not a transaction. Partial row failures are surfaced in-band. The call must not lie about full success.
5. If `embedding` is supplied, the server stores it verbatim. If it is absent and `embed_missing = true`, the server computes it and owns retries/rate limits against Ollama.
6. With `strict_edges = true`, every edge endpoint must resolve either to an entity in this batch or to an already-resident entity in the same `(tenant_id, session_id)`.
7. Tenant isolation is enforced server-side. `tenant_id` in the request is validated against authenticated caller context and cannot widen access.
8. Large batches emit MCP `$/progress` notifications at bounded intervals.
9. `dry_run = true` performs schema validation, conflict detection, endpoint resolution, and embedding-plan reporting without writes.
10. The response always includes `schema_version` so clients can warn on drift without owning the schema.

## Why This Shape

- One call matches forge's current ingestion shape: entities and edges are already materialized in memory before ingest begins.
- Structured row failures are strictly better than subprocess exit codes and stderr scraping.
- Optional client-supplied embeddings supports migration from client-owned Ollama usage to server-owned embeddings.
- The payload mirrors today's loader semantics closely enough that migration is mostly transport replacement, not data-model replacement.

## Acceptance Criteria

- [ ] `ingest_entities` exists in `tools/list` with a schema matching the bulk-ingest contract.
- [ ] Unknown/invalid entity attrs are surfaced in `entities.failed[]`; they are not silently ignored.
- [ ] `on_conflict = "update"` upserts existing entities idempotently.
- [ ] `on_conflict = "skip"` leaves resident entities untouched and increments `skipped`.
- [ ] `on_conflict = "error"` reports conflicts without mutating the conflicting row.
- [ ] `strict_edges = true` rejects orphan edges with structured reasons.
- [ ] `strict_edges = false` still reports duplicate/orphan outcomes honestly; it does not invent success.
- [ ] `dry_run = true` performs validation/resolution and returns counters without writing rows.
- [ ] server-side embedding computation works for missing embeddings and reports per-row failures in `embeddings.failed[]`.
- [ ] the response includes `schema_version` and `duration_ms`.
- [ ] large batches emit bounded progress notifications.

## Dependencies

- Existing app-table CQL storage path remains in scope and is allowed.
- Existing public graph write path remains the edge-write boundary for graph-owned state.
- Existing embedding client / Ollama integration can be reused behind server-owned policy.

## Out of Scope

- Schema migration API
- Entity deletion or edge deletion
- Cross-session or cross-tenant ingest in one call
- Streaming ingest protocol for corpora that exceed MCP message-size limits

## Estimated Effort

- Contract + payload validation + tool wiring: 1 day
- Entity write path + conflict semantics + schema-version reporting: 1-2 days
- Edge validation + graph/app write integration: 1-2 days
- Embedding ownership + progress notifications + dry-run: 1-2 days
- Coverage and consumer smoke harness: 1 day
- **Total:** ~1 sprint slice / ~1 week
