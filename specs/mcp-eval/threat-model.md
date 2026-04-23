# MCP Eval Framework — STRIDE Threat Model

**Date:** 2026-04-05
**Scope:** Eval framework, MCP server under evaluation, interaction between them

## Trust Boundaries

| ID | Boundary | Description |
|----|----------|-------------|
| TB-E0 | Scenario files → Eval Runner | Local filesystem, developer-authored |
| TB-E1 | Eval Runner → MCP Server | JSON-RPC over stdio or HTTP |
| TB-E2 | Eval Runner → Ferrosa public query interfaces | Eval-harness-only observation path for graph/storage inspection outside the MCP transport boundary |
| TB-E3 | Eval Runner → Claude API | LLM-as-Judge calls with tool responses |
| TB-E4 | Eval Results → Consumer | JSON reports for CI/dashboards/developers |

## Threat Table

| ID | STRIDE | Component | Threat | L | I | Risk | Mitigation |
|----|--------|-----------|--------|:-:|:-:|:----:|------------|
| ET-T2 | Tampering | LLM Judge (TB-E3) | **Prompt injection via MCP tool responses.** Tool output embedded in judge prompt could contain instructions to always return PASS. | 4 | 5 | **20** | Sanitize responses before judge prompt. Use structured JSON output parsing. Cross-validate: if programmatic FAIL but judge PASS, flag anomalous. |
| ET-S3 | Spoofing | Public query observer (TB-E2) | **Eval-harness query path bypasses MCP tenant isolation.** DIKW/Semantic analyzers query Ferrosa through public interfaces outside the MCP auth layer. | 3 | 5 | **15** | Dedicated read-only API credentials. All queries must include tenant_id + session_id. Thin wrapper rejects unfiltered queries. |
| ET-T3 | Tampering | Semantic Analyzer | **Ontological poisoning.** Corrupted type registry from prior runs masks type system decay. | 3 | 4 | 12 | Snapshot type registry before eval. Diff after run. Compare against canonical type list. |
| ET-T4 | Tampering | DIKW Analyzer | **Inference chain corruption.** Malicious Datalog rules derive false facts, inflating knowledge synthesis scores. | 3 | 4 | 12 | Verify derived fact *content* against ground truth, not just counts. Clean up rules in teardown. |
| ET-I1 | Info Disclosure | LLM Judge (TB-E3) | **Sensitive memory content sent to external API.** Tool responses containing user memory sent to Claude for grading. | 3 | 4 | 12 | Synthetic data only in scenarios. Naming convention enforcement. Validate no real-data patterns before API call. |
| ET-D1 | DoS | Scenario Runner | **Resource exhaustion.** High-coupling tools (run_consolidation, recursive_explore) exhaust cluster CPU/memory. | 3 | 4 | 12 | Per-scenario timeout. Circuit breaker (3 consecutive timeouts = abort). Resource budget limits. Isolated cluster. |
| ET-D3 | DoS | Session Cleanup | **Graph pollution from incomplete cleanup.** Failed delete_session leaves residual data, inflating future metrics. | 3 | 4 | 12 | Two-phase cleanup: MCP delete + CQL verification. Cleanup ledger. Sweep job for stale eval sessions. Dedicated eval tenant. |
| ET-E1 | Elevation | Public query observer (TB-E2) | **Eval-harness observer path has write permissions.** Bug in analyzer code could corrupt system state during observation. | 2 | 5 | 10 | Read-only public API credentials for observation. Separate eval_scratch keyspace or sandbox for aggregation. |
| ET-E2 | Elevation | DIKW Emergence | **Manufacturing emergent relationships.** Scenarios explicitly create edges then claim them as "discovered." | 4 | 3 | 12 | Tag edges by creation method. Only count synthesis-tool edges (consolidation, Datalog) not CRUD edges for emergence. |
| ET-E3 | Elevation | Claim Rubric | **Trivially satisfiable claims.** Loose substring matching (e.g., "entity_id") passes any response. | 3 | 4 | 12 | Require value assertions. "Claim discrimination" meta-test against wrong responses. Mandatory negative test cases. |
| ET-S1 | Spoofing | Scenario Loader (TB-E0) | **Scenario file substitution.** Crafted TOMLs test weaker subset, hiding regressions. | 2 | 4 | 8 | Version control + code review. SHA-256 manifest logged in report. CI pins checksums. |
| ET-S2 | Spoofing | MCP Client (TB-E1) | **MCP server impersonation.** Wrong binary/endpoint graded instead of target server. | 2 | 5 | 10 | Record binary hash + initialize response in report. CI validates server identity. |
| ET-T1 | Tampering | Ground Truth (TB-E0) | **Ground truth poisoning.** Modified expected results make regressions pass. | 2 | 5 | 10 | Versioned with code review. File hashes in report. Invariant assertions. |
| ET-R1 | Repudiation | Report Generator (TB-E4) | **Result manipulation after generation.** Edited scores hide regressions. | 2 | 4 | 8 | HMAC-signed reports. Merkle root of scenario results. Append-only storage. |
| ET-R2 | Repudiation | Scenario Runner | **Non-deterministic results without provenance.** Cherry-picked favorable runs. | 3 | 3 | 9 | Full provenance: git SHA, cluster version, model IDs, checksums. Run N times, report mean + variance. |
| ET-I2 | Info Disclosure | Public query observer (TB-E2) | **Cross-session measurement contamination.** Imperfect session filtering inflates graph density. | 3 | 3 | 9 | All observation queries filter tenant_id + session_id. Clean room check before scenario. |
| ET-I3 | Info Disclosure | Reports (TB-E4) | **Architecture details leaked via reports.** Tool names, CQL structures, Datalog rules exposed. | 2 | 3 | 6 | Classify as internal-only. Redact CQL/Datalog details in external reports. |
| ET-D2 | DoS | LLM Judge (TB-E3) | **Claude API rate limiting stalls pipeline.** Many judge calls hit rate limits. | 2 | 3 | 6 | Rate limiting + caching. Judge optional per scenario. Fallback to programmatic + claim grading. |

## Risk Summary

- **Critical (>=15):** ET-T2 (prompt injection into judge), ET-S3 (public-query bypass)
- **High (10-14):** 10 threats covering ontological poisoning, inference corruption, resource exhaustion, cleanup failures, emergence gaming, claim inflation
- **Medium (6-9):** 5 threats covering provenance, contamination, leakage, rate limiting

## Top 4 Architectural Recommendations

1. **LLM judge is the weakest link (ET-T2).** Implement structured output parsing, injection stripping, and cross-validation with programmatic graders as Sprint 1 deliverable.
2. **The eval observer path (ET-S3, ET-E1) creates a second trust boundary.** Use read-only public-API credentials plus a tenant-scoped query wrapper.
3. **Emergence scoring (ET-E2) needs edge provenance metadata** to distinguish system-discovered vs. script-created relationships.
4. **Session cleanup (ET-D3) is operationally critical.** Dedicated eval tenant + ledger-based sweep prevents cumulative graph pollution.
