---
type: bug
priority: P3
status: draft
created: 2026-04-16
updated: 2026-04-20
reported-by: backfill-rich-entities repeat-run observation (2026-04-16)
---

# `content_hash` clobbered by concurrent partial-entity updates

## Observed

Running `ferrosa-memory-batch backfill-rich-entities` repeatedly always
reports `p4_hashed=1` — one entity's content_hash is re-computed and
re-written on every run. A direct CQL scan
(`SELECT entity_id, description, content_hash FROM entity_store`)
reports **zero** entities with `description.is_some() && content_hash.is_none()`
at rest. So the write lands durably, but a later
`entity_list_all` surfaces at least one entity with `content_hash=None`.

Hypothesis: some other write path (probably the live MCP's ingest path,
running during backfill) does `entity_put` with `content_hash: None` on
updates that aren't full round-trips, clobbering the backfill's hash.

## Why it matters

`content_hash` is the idempotency key for `ingest_skill` and (once adopted
elsewhere) other content-hash-gated writes. If it keeps oscillating, any
consumer that trusts it as a fingerprint will mis-cache.

## Proposed fix directions

- `cql_storage.rs::entity_put` should not overwrite `content_hash` with
  `None`. Treat `None` as "leave unchanged"; require explicit
  `content_hash: Some("")` to clear. (This is essentially the CQL
  `UPDATE … WHERE` pattern — don't issue the write for fields not
  provided.)
- Alternatively: split `entity_put` into `entity_create` vs
  `entity_update_fields` so callers only touch the fields they intend to
  change. Bigger refactor.

Prefer option 1 for now — narrow, doesn't ripple.

## Acceptance criteria

- [ ] Running `backfill-rich-entities` twice with the live MCP also
      running results in `p4_hashed=0` on the second run.
- [ ] Unit test: `entity_put` with `content_hash: None` on an existing
      row that had `content_hash: Some("sha256:…")` does NOT clear the
      hash.
