# TDD TODO: fix startup seed timestamp failures and NULL timestamp rows at source

Status: DONE

## Problem

Fresh `ferrosa-memory-mcp` startup/log verification shows two warning classes:

1. `seed_sprint1_types` writes fail because Ferrosa reports `now()` returns `timeuuid` while `entity_types.created_at` / `edge_types.created_at` expect `timestamp`.
2. Normal reads encounter rows with NULL `created_at` / `updated_at`, producing `row has null/corrupt timestamp; defaulting to epoch` warnings.

The goal is not to hide these warnings. The goal is to remove their source.

## Root cause hypotheses

- Seed writes embed a CQL time expression (`now()` or `toTimestamp(now())`) instead of binding a Rust timestamp value. Ferrosa's CQL function handling still rejects that expression for timestamp columns.
- NULL timestamp rows were created by an older writer, migration, or seed path that did not populate NOT-NULL logical metadata. We must identify the affected table/rows and fix the writer/migration; if live data already contains bad rows, add a data repair step.

## RED plan

1. Seed timestamp RED:
   - Add a focused test that generated Sprint-1 seed INSERT statements bind `created_at` as a timestamp parameter (`?`) and do not embed `now()`/`toTimestamp(now())`.
   - Expected initial failure: current query contains CQL time expression.

2. NULL timestamp source RED:
   - Query live data to identify exact table(s), primary keys, and writer path(s) for NULL `created_at` / `updated_at`.
   - Add a focused test at that writer/migration seam proving new rows always populate timestamps.
   - If the corruption is historical live data only, add an idempotent repair migration test proving NULL rows are backfilled.

## GREEN plan

1. Change seed insert statements to bind `chrono::Utc::now()` as a normal timestamp parameter.
2. Fix the writer/migration that can create NULL timestamps.
3. Add an idempotent data repair migration if current live rows are already corrupt.
4. Keep warnings in place so future corruption remains visible.

## Verification commands

```bash
cargo test -p ferrosa-memory-core cql_storage -- --nocapture
cargo test -p ferrosa-memory-core migration -- --nocapture
cargo test -p ferrosa-memory-core --lib
cargo build --release --target x86_64-unknown-linux-gnu
```

After deploy:

```bash
curl -fsS http://127.0.0.1:18765/healthz/ready
# Exercise manage_rules/query_derived/spread_activation and grep fresh logs.
```

## Acceptance criteria

- Startup no longer logs `seed_sprint1_types: ... type mismatch: now() returns timeuuid`.
- Live tables no longer contain NULL timestamp rows in the affected source table(s).
- Warnings remain available for future actual corrupt rows.
- Focused and full core tests pass.
- Release build has no warnings.
