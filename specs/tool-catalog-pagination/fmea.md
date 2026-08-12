---
title: Bounded Tool Catalog FMEA
executive_summary:
  purpose: >-
    Scores catalog pagination failure modes and maps high-risk modes to
    required tests and delivery tasks.
  critical_items:
    - Measuring the inner page instead of the final response has the highest RPN at 378.
    - Runtime-incomplete catalog versions and missing telemetry each score RPN 336.
    - Every failure mode at RPN 200 or above has a dedicated verification requirement.
status: draft
last_updated: 2026-08-12
---

# Bounded Tool Catalog FMEA

Severity (S), occurrence (O), and detection difficulty (D) use 1–10 scales.
`RPN = S × O × D`; RPN 200 or higher is release-blocking until its mitigation
has direct automated evidence.

| ID | Failure mode | Effect | S | O | D | RPN | Required control and test |
|---|---|---|---:|---:|---:|---:|---|
| FM-01 | Limit measures inner page, not final surface envelope | Client still rejects oversized result | 9 | 7 | 6 | 378 | Exact 16,384-byte tests against every final result encoder |
| FM-02 | Catalog version omits runtime entity types | Cursor silently traverses incompatible schemas | 8 | 6 | 7 | 336 | Change runtime types and prove stale-cursor result before source access |
| FM-03 | Pagination has no byte/pull telemetry | Regression reaches production undetected | 7 | 6 | 8 | 336 | Metrics contract and alert tests for bytes, pulls, progress, failures |
| FM-04 | Server builds full catalog then slices | Server memory and CPU still scale with catalog size | 8 | 8 | 5 | 320 | Source spy and 10,000-entry performance fixture, pulls ≤ emitted + 1 |
| FM-05 | Cursor not bound to mode/filter/surface | Entries leak, duplicate, or disappear | 8 | 6 | 6 | 288 | Property tests for every fingerprint dimension and visibility boundary |
| FM-06 | Protocol adapters diverge semantically | Same cursor/page differs by surface | 7 | 4 | 8 | 224 | Cross-surface parity contract with surface-specific envelope assertions |
| FM-07 | First-party client consumes only page one | Tools disappear from eval, setup, or UI | 7 | 5 | 5 | 175 | Real-client traversal and named-lookup system tests |
| FM-08 | Cache serves a stale page under a new version | Caller sees invalid schema or skips entries | 6 | 5 | 6 | 180 | Versioned cache-key test and deployment restart test |
| FM-09 | One schema exceeds page budget | Infinite retry, truncation, or oversized result | 7 | 3 | 5 | 105 | Typed `ENTRY_TOO_LARGE`, no cursor progress, readiness diagnostic |
| FM-10 | Excessive exact names amplify lookup/encoding | CPU and response amplification | 5 | 5 | 4 | 100 | Count/byte limits before source access |
| FM-11 | Search ordering changes between requests | Duplicate or omitted traversal entries | 7 | 4 | 6 | 168 | Canonical ranking/key property test and restart/version rule |
| FM-12 | Empty page advertises continuation | Caller loops forever | 8 | 3 | 4 | 96 | Invariant: non-final page emits at least one item and advances key |
| FM-13 | Hint does not preserve normalized filters | Caller restarts a different search | 5 | 5 | 5 | 125 | Exact next-argument contract test on every page type |
| FM-14 | Operator server drains pages on behalf of browser | Memory protection is bypassed | 7 | 4 | 6 | 168 | HTTP/workbench integration test with incremental requests |
| FM-15 | Future DB source scans then filters in service | Database and memory load becomes unbounded | 8 | 4 | 7 | 224 | Repository trait contract requires cursor/filter/projection/limit pushdown |

## Critical verification scenarios

| Test | Covers | Pass condition |
|---|---|---|
| FMEA-T01 | FM-01 | Encoded response is 16,384 bytes or less; next entry would exceed it |
| FMEA-T02 | FM-02 | Runtime schema input change yields `STALE_CURSOR` and zero source pulls |
| FMEA-T03 | FM-03 | Metrics expose final bytes, emitted/pulled entries, surface, and error code |
| FMEA-T04 | FM-04 | A deep page of a 10,000-entry source retains O(page) state and pulls at most one look-ahead |
| FMEA-T05 | FM-05 | Reusing a cursor across any surface, visibility, detail, query, name, or category change fails |
| FMEA-T06 | FM-06 | Equivalent semantic selection yields identical entries/order/version across adapters |
| FMEA-T07 | FM-15 | A database-source test double receives keyset, predicates, projection, order, and bounded limit |

## Systemic findings

The largest risks come from checking the wrong boundary and from preserving the
current eager source behind a paginated facade. Output-token safety and server
memory safety are separate requirements; release evidence must prove both.

Cursor correctness is also a fleet property. A locally deterministic codec is
insufficient if replicas disagree about the effective schema inputs. Deployment
verification therefore belongs in the final rollout packet, not only unit tests.
