---
type: todo
priority: P2
status: draft
created: 2026-04-06
updated: 2026-04-20
---

# Refactor: Code Smell Cleanup

**Status**: Blocked on P0 data loss fix verification
**Priority**: P2 (tech debt)
**Date**: 2026-04-06

## Summary

Smell scan and clippy audit identified structural issues across ferrosa-memory-core.
All tests pass (478/479 — 1 unrelated failure in `snapshot_filters_co_occurs_without_strength`).

## Critical: High-Complexity Functions

### storage.rs — Trait impl with massive functions (CC 30-41)

The `MemoryStorage` trait impl has 20+ methods, many 300-500+ lines with cyclomatic complexity 30-41. These are the worst offenders in the codebase.

| Function | Lines | CC |
|----------|------:|---:|
| `memo_put` | 518 | 41 |
| `plan_put` | 515 | 41 |
| `fold_put` | 491 | 40 |
| `entity_put` | 448 | 40 |
| `entity_count` | 419 | 39 |
| `fold_count` | 416 | 39 |
| `memo_count` | 413 | 38 |
| `entity_list_all` | 403 | 34 |
| `fold_list_all` | 400 | 33 |
| `temporal_list_all` | 397 | 32 |
| `temporal_put` | 384 | 32 |
| `delete_session` | 348 | 29 |
| `edge_decay_weights` | 306 | 29 |

**Suggested refactoring**: These likely share common patterns (CQL query building, error handling, session routing). Extract shared logic into helper functions. Consider a macro or generic approach if the pattern is truly mechanical.

### dispatch.rs:tool_definitions — 657 lines, CC=52, nesting=8

Single function defining all MCP tool schemas. This is a data declaration masquerading as code.

**Suggested refactoring**: Extract tool definitions into a declarative structure (array of structs, or a macro). Each tool's definition should be self-contained and independently readable.

### cql_storage.rs:connect — 409 lines

Schema bootstrap function that creates all tables/indexes.

**Suggested refactoring**: Extract DDL statements into a list, iterate. Or split into per-table initialization functions.

### compression.rs:compress — 107 lines

**Suggested refactoring**: Decompose into phases: tokenize, score, filter, reassemble.

## Medium: Clippy Warnings (4)

All in `http.rs`:
- Line 550: unused variable `view_mode` (assigned but never read)
- Line 636: value assigned to `view_mode` is never read
- Line 554: collapsible `if` statement
- Line 675: collapsible `if` statement

**Fix**: Straightforward, can be done in a single pass.

## Medium: Deep Nesting

| File | Function | Depth |
|------|----------|------:|
| `datalog.rs:251` | `evaluate_rule` | 6 |
| `recursive_explore.rs:24` | `decompose_query` | 7 |
| `transport.rs:92` | `serve_stdio` | 5 |
| `smart_ingest.rs:305` | `extract_entity_candidates` | 5 |

**Suggested refactoring**: Extract inner match arms into named functions. Use guard clauses to flatten.

## Low: Long Test Functions

Several test functions exceed 60 lines. Not production code, lower priority:
- `security_tests.rs:271` — `delete_session_cascades_all_tables` (130 lines)
- `datalog.rs:1093` — `test_load_session_facts` (78 lines)
- `dispatch.rs:4579` — `anomaly_emits_event_on_bus` (74 lines)

## Informational: Hardcoded Tunables

Many hardcoded constants in production code (thresholds, weights, ports). Most are already backed by `config.rs` defaults — the smell detector flags the default values themselves. Low priority unless we need runtime tunability.

## Proposed Approach

1. Fix clippy warnings first (smallest, zero risk)
2. Tackle `storage.rs` — extract common patterns from the trait impl
3. Refactor `dispatch.rs:tool_definitions` into declarative form
4. Extract `cql_storage.rs:connect` DDL into iterable structure
5. Flatten deep nesting in datalog/recursive_explore
6. Decompose `compression.rs:compress`

Each step: one commit, tests must pass before and after.
