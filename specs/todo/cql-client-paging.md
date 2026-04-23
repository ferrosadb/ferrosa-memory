---
type: todo
priority: P2
status: draft
created: 2026-04-06
updated: 2026-04-20
---

# CQL Client-Side Paging

## Problem

All CQL clients (ferrosa-memory-core CqlStorage, frg ingest, restore script) fetch full partitions without paging. With 15K+ entities in a single `(tenant_id, session_id)` partition (~209MB), this causes:

1. The CQL server assembles the entire 209MB response in memory
2. If the SSTable is partially written, the read fails and the entire partition is skipped
3. Even on success, 209MB in one TCP response is fragile

## Fix

### 1. CqlStorage (ferrosa-memory-core)

Set `default_fetch_size` on the cdrs-tokio session:
```rust
// In CqlStorage::connect()
session.set_page_size(5000); // or equivalent cdrs-tokio API
```

All `SELECT` queries that scan partitions should use paging. The cdrs-tokio driver handles paging transparently — results come in pages of `fetch_size` rows, the driver auto-fetches the next page.

### 2. Restore Script (scripts/restore-memory.sh)

The Python cassandra-driver supports paging natively:
```python
session.default_fetch_size = 5000
# All queries now page automatically
```

### 3. Skilltools Ingest

The Rust CQL driver used by forge should also page reads if it does any SELECT queries during ingest.

### 4. Backup Script

The backup script dumps entire tables — it should page:
```python
session.default_fetch_size = 5000
rows = session.execute('SELECT * FROM entity_store WHERE tenant_id = %s AND session_id = %s', ...)
# Driver pages automatically, iterate normally
```

## Impact

- Prevents 209MB single reads that trigger the truncation bug
- Reduces memory pressure on the CQL coordinator
- More resilient to partial SSTable failures (only lose one page, not entire partition)

## Partition Size

Long-term, consider splitting the single `(tenant_id, session_id)` partition. 15K entities in one partition is above the Cassandra recommended limit of 100MB. Options:
- Add a bucket key: `(tenant_id, session_id, bucket)` where bucket = `entity_id[0:2]` (first 2 hex chars, 256 buckets)
- Or use time-based bucketing for temporal data
