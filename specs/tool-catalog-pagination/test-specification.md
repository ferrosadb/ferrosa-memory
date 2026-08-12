---
title: Bounded Tool Catalog Test Specification
executive_summary:
  purpose: >-
    Defines seven test layers for catalog cursor correctness, exact response
    limits, bounded source work, client compatibility, and sustained operation.
  critical_items:
    - Exact byte-boundary and source-pull tests are independent release gates.
    - Property tests cover arbitrary entry sizes and cursor fingerprint dimensions.
    - Performance and duration tests use a 10,000-entry synthetic source without a live database.
status: draft
last_updated: 2026-08-12
---

# Bounded Tool Catalog Test Specification

## Test ID allocation

The existing catalog maxima are U19, C7, I16, S15, P4, PF5, and D4. This
blueprint reserves the next IDs without renumbering historical tests.

## Layer 1 — Unit tests

| ID | Behavior | Required assertions |
|---|---|---|
| T-U-020 | Cursor round trip and rejection | Codec/version/query/surface/visibility bind; stale, malformed, oversized, and mismatched cursors fail before source reads |
| T-U-021 | Projection compatibility | Compact fields are bounded; schema mode matches existing `ToolDef`; schema digest changes iff canonical schema changes |
| T-U-022 | Semantic byte packing | Complete entries only; final encoded result ≤16,384; boundary equality allowed; oversized single entry typed; progress guaranteed |
| T-U-023 | Hint construction | Every page/error has exact continuation, completion, or restart arguments with no stale cursor |
| T-U-024 | Lazy source work | Direct seek/name lookup; source pulls ≤ emitted + 1; nonmatching entries do not build schemas |

## Layer 2 — Contract tests

| ID | Boundary | Required assertions |
|---|---|---|
| T-C-008 | `all_tools` input/output/error contract | Compact default; query/categories/names/detail/cursor; duplicated result envelope counted; stable public name |
| T-C-009 | Cross-surface catalog semantics | Same eligible tools, order, version, digests, filters, and cursor meaning; only protocol field names/envelopes differ |

The contract harness must serialize the exact result value exposed to the
caller. For `all_tools`, this is the complete `CallToolResult`, including text
fallback and `structuredContent`. For `tools/list`, it is the complete legacy or
modern JSON-RPC `result`, including `_meta` and cache fields. For operator HTTP,
it is the response JSON body. JSON-RPC request IDs and transport headers are not
stable catalog content and are excluded.

## Layer 3 — Integration tests

| ID | Integration | Required assertions |
|---|---|---|
| T-I-017 | `all_tools` traversal | Compact, schema, named jump, lexical/category filtering, final hints, stale restart; every match once |
| T-I-018 | Legacy and modern `tools/list` | Real dispatch paths, `nextCursor`, tier/full visibility, cache/version metadata, structured errors |
| T-I-019 | Operator and workbench | Incremental compact page fetch, selected named schema fetch, cursor query routing, no server/browser reassembly |

## Layer 4 — System tests

| ID | Workflow | Required assertions |
|---|---|---|
| T-S-016 | Real stdio and HTTP clients | Eval/setup traverse to late-catalog tools; stale deployment restart works; existing tool calls still execute after discovery |

The system test exercises the built MCP binary and HTTP route. It must include
at least one expected tool that cannot appear on page one.

## Layer 5 — Property tests

| ID | Generated domain | Invariants |
|---|---|---|
| T-P-005 | Arbitrary ordered catalogs, UTF-8 entry sizes, budgets, filters, versions, and cursor mutations | No oversize response, no split, no duplicate/omission, progress, deterministic restart, fingerprint mismatch rejection |

Include multibyte UTF-8, empty result sets, one-entry pages, exactly-16,384-byte
results, and one-byte-over cases. Shrinking must retain the final surface
encoder and not replace it with an inner-page approximation.

## Layer 6 — Performance tests

| ID | Profile | Gate |
|---|---|---|
| T-PF-006 | 10,000 synthetic descriptors; first, middle, and deep pages; compact/search/schema/named modes | Retained entries O(page + 1); pulls ≤ emitted + 1 after seek; no full schema build; p95 page assembly target recorded and regression-bounded |

Performance thresholds should be baselined on CI hardware in AT-108. The
non-negotiable gates are bounded construction and retained state, not an
unmeasured absolute latency guessed during design.

## Layer 7 — Duration tests

| ID | Profile | Gate |
|---|---|---|
| T-D-005 | Repeated mixed catalog traffic with cursor traversal, named jumps, invalid/stale inputs, and version swaps | No response >16 KiB, no stuck cursor, no unbounded RSS slope, all metrics remain internally consistent |

The duration runner must generate traffic and sample RSS over multiple
intervals. A single before/after snapshot does not satisfy this test.

## Traceability matrix

| Requirement/risk | Tests |
|---|---|
| 16 KiB final response | T-U-022, T-C-008, T-C-009, T-I-018, T-P-005, T-D-005 |
| Source-level bounded work | T-U-024, T-I-019, T-PF-006, T-D-005 |
| Versioned stale restart | T-U-020, T-U-023, T-I-017, T-S-016, T-P-005 |
| Search in `all_tools` | T-C-008, T-I-017, T-P-005 |
| Cross-surface consistency | T-C-009, T-I-018, T-I-019 |
| First-party compatibility | T-I-019, T-S-016 |
| FM-01 | T-U-022, T-C-008, T-C-009 |
| FM-02 | T-U-020, T-S-016, T-D-005 |
| FM-03 | T-D-005 and telemetry assertions in T-I-017/T-I-018 |
| FM-04 | T-U-024, T-PF-006 |
| FM-05 | T-U-020, T-P-005 |
| FM-15 | T-U-024 source-contract test double |

## Harness integration

- Register the new Rust contract binary under `make test-contracts`.
- Preserve `make test-unit`, `make test-integration`, `make test-system`, and
  `make test-all` behavior.
- Add explicit performance and duration targets if the current Makefile cannot
  select T-PF-006/T-D-005 independently.
- Keep full-catalog collection in tests only, for compatibility comparison and
  exhaustive traversal assertions.
