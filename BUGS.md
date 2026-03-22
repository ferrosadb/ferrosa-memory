# Ferrosa Compatibility Bug Report

## Summary

During integration testing of ferrosa-memory-mcp against Ferrosa DB, we identified
and fixed 7 CQL protocol issues (all landed on `feature/udf-uda-query-time`) and
documented 4 remaining issues that block specific features.

**Reproduction tests:** `crates/ferrosa-core/tests/ferrosa_bugs.rs`
```sh
cargo test -p ferrosa-core --test ferrosa_bugs -- --ignored --nocapture
```

---

## Fixed Issues (landed on feature branch)

| # | Issue | Commit | Impact |
|---|-------|--------|--------|
| 1 | `toJson()` CQL function missing | `c6872d9` | cdrs-tokio session build hung (metadata refresh) |
| 2 | `pk_count` missing in PREPARE bind metadata | `531027a` | PREPARE response truncated, driver parse failure |
| 3 | Bind variable types not populated in PREPARE | `6002f9c` | All PREPARE statements failed |
| 4 | CQL protocol v5 response to v4 client | `2be6d2b` | Driver parsed wrong frame format, all ops failed |
| 5 | Positional bind values rejected in EXECUTE | `2be6d2b` | "bind markers not supported" error on all writes |
| 6 | SSTable BTI trie partition index sign bit | `6d43056` | Point queries on 2nd+ partition returned corrupt data |
| 7 | Prepared SELECT on compound PK panics in router | uncommitted | "PK lookup should have been handled above" panic |

## Open Issues

### BUG-1: vector\<float, N\> type not serializable via cdrs-tokio

**Severity:** CRITICAL (FMEA F31, RPN 180)

**Description:** Ferrosa implements `vector<float, N>` (Cassandra 5.0 spec, commit `a9a7e43`).
cdrs-tokio v9 has partial vector support in `cassandra-protocol` v4 but the INSERT/SELECT
round-trip fails because `Vec<f32>` doesn't serialize to the CQL VECTOR wire format.

**Impact:** All embedding columns stored as NULL. ANN queries (`ORDER BY embedding ANN OF ?`)
non-functional. `fold_search` falls back to LIMIT-based retrieval. `entity_search_ann` returns
empty. **Semantic search completely broken.**

**Blocked on:** Either:
1. Custom VECTOR serializer in ferrosa-memory-mcp (bypassing cdrs-tokio type system)
2. PR to cdrs-tokio adding VECTOR type support
3. Verify if Ferrosa's VECTOR wire encoding matches what `cassandra-protocol::types::vector` expects

**Reproduction:** `ferrosa_bugs::bug_vector_type_insert_roundtrip`

---

### BUG-2: SUBSCRIBE change stream not wired

**Severity:** MEDIUM

**Description:** Ferrosa supports `SUBSCRIBE SELECT ... DELTA` for real-time streaming of
table mutations. ferrosa-memory-mcp needs this for real-time anomaly alerting (spec Section 9.3).

**Impact:** Anomaly detection is batch-only. Memory poisoning attacks detected with delay
instead of real-time.

**Blocked on:** Testing SUBSCRIBE via cdrs-tokio. The protocol extension may require a
custom frame handler since SUBSCRIBE is not standard CQL.

**Reproduction:** `ferrosa_bugs::bug_subscribe_change_stream`

---

### BUG-3: COUNT(\*) column name mismatch

**Severity:** LOW (workaround in place)

**Description:** Ferrosa returns `COUNT(*)` result column as `system.count` instead of `count`.
cdrs-tokio's `r_by_name("count")` fails to find it.

**Workaround:** `entity_count` in `cql_storage.rs` uses `SELECT entity_id` + client-side
`rows.len()` instead of `COUNT(*)`.

**Ferrosa fix:** Commit `523483e` was supposed to fix this but the column is still
`system.count` in prepared statement results.

**Reproduction:** `ferrosa_bugs::bug_count_column_name`

---

### BUG-4: Phonetic index (Double Metaphone) untested

**Severity:** MEDIUM

**Description:** The spec calls for Ferrosa's phonetic index on `entity_name` for fuzzy
name matching. The DDL includes a phonetic index using Double Metaphone algorithm.

**Impact:** Entity deduplication only catches exact case-insensitive matches.
Phonetic variants like "Jon Smith" / "John Smyth" are treated as different entities.

**Blocked on:**
1. Verifying that Ferrosa supports `USING 'phonetic'` index type
2. Determining the query syntax for phonetic lookups (standard `WHERE =` or special syntax?)

**Reproduction:** `ferrosa_bugs::bug_phonetic_index_query`

---

## Previously Fixed (during this session)

These issues were found, diagnosed (often with raw TCP hex dumps), and fixed in real-time
during the integration testing session:

1. **toJson():** cdrs-tokio sends `SELECT keyspace_name, toJson(replication) FROM system_schema.keyspaces`
   during session startup. Ferrosa didn't implement `toJson()`.

2. **pk_count:** CQL v4 PREPARE response requires `pk_count` (i32) and `pk_indexes` (i16[])
   after `columns_count` in bind metadata. Ferrosa omitted these fields.

3. **Bind variable types:** PREPARE response had empty type encoding for bind variables.
   cdrs-tokio couldn't determine column types for EXECUTE serialization.

4. **Protocol version:** Ferrosa responded with CQL v5 header (0x85) to a v4 client.
   The v5 PREPARE response includes `result_metadata_id` which shifted all byte offsets.

5. **Positional binds:** Ferrosa's EXECUTE handler only accepted named placeholders.
   All CQL drivers use positional bind values with prepared statements.

6. **SSTable sign bit:** BTI trie partition index encoded negative offsets with corrupted
   sign bits, causing point queries on the 2nd+ partition to read past end of file.

7. **Router panic:** Prepared SELECT on `entity_store` with full compound partition key
   was incorrectly routed through ALLOW FILTERING path, hitting an unreachable code panic.

## Test Matrix

| Test | Status | Requires |
|------|--------|----------|
| `bug_vector_type_insert_roundtrip` | FAIL (expected) | cdrs-tokio VECTOR support |
| `bug_vector_ann_query` | FAIL (expected) | VECTOR support + HNSW index |
| `bug_subscribe_change_stream` | FAIL (expected) | SUBSCRIBE protocol support |
| `bug_count_column_name` | FAIL (expected) | Ferrosa COUNT(*) column fix |
| `bug_phonetic_index_query` | FAIL (expected) | Ferrosa phonetic index support |
