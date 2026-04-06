---
type: bug
priority: P0
reported-by: human
implemented-by: ""
verified-by: ""
created: 2026-04-05
updated: 2026-04-05
source: manual
source-location: "tools/skilltools/specs/bug-ingest-dangling-edge-references.md"
---

# Entity store loses entities from prior ingests

## Description

When multiple codebases are ingested sequentially via `skilltools ingest --cql`, entities from earlier ingests are lost. Only the last codebase's entities survive. All ingests use the same `(tenant_id, session_id)` partition, with unique `entity_id` clustering keys, so Cassandra upsert semantics should not cause overwrites.

## Evidence

3 codebases ingested sequentially into the same cluster with session `00000000-...`:
- ferrosa-memory: 2,865 entities reported inserted
- ferrosa: 11,029 entities reported inserted
- ferrosa-dbaas: 2,015 entities reported inserted

**Post-ingest:** Only 2,482 entities survive. Breakdown:
- 14 crates — **all from ferrosa-dbaas** (the last ingest)
- 76 modules, 1,300 functions — match ferrosa-dbaas counts
- 0 crates from ferrosa (expected ~30) or ferrosa-memory (expected ~10)
- 74 person entities from paper ingestion (survived)
- 19,138 edges — most pointing to entities that no longer exist

**13,427 entities were lost.** The Python loader reported successful INSERT for all 15,909 but only the last batch persists.

## Root Cause Hypotheses

1. **Ferrosa storage engine compaction/GC**: The LSM-tree storage may be dropping older SSTables during compaction, especially under heavy write load from sequential bulk ingests. If compaction runs between ingests, it could discard tombstoned data incorrectly.

2. **S3 tiering race condition**: If entity_store data is being tiered to S3 between ingests, a subsequent compaction might not see the S3-resident data and treat the partition as empty.

3. **Write-ahead log truncation**: If the commit log is truncated before the first ingest's SSTables are flushed, those writes are lost on restart.

4. **Python cassandra-driver consistency**: The loader uses default consistency level (likely ONE). If the local replica acknowledges the write but fails to replicate before the next ingest overwrites, data is lost.

5. **Partition size limit**: 15,909 entities in one partition `(tenant_id, session_id)` may hit a ferrosa partition size limit, causing silent truncation.

## Expected Behavior

All entities from all ingests should persist. The `entity_store` partition should hold the union of all ingested entities since the entity_ids are unique UUIDv5 values.

## Reproduction

```bash
skilltools ingest --cql localhost:19042 /path/to/codebase-a
# Verify: entity count matches reported count
skilltools ingest --cql localhost:19042 /path/to/codebase-b
# Verify: entity count = sum of both ingests (no loss)
```

## Impact

- Knowledge graph is structurally incomplete — only the last-ingested codebase is queryable
- 76.8% of edges are dangling
- Re-ingesting doesn't fix it — it just overwrites again
