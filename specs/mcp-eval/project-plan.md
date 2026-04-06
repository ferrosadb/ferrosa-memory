# MCP Eval Framework — Project Plan

## Sprint Overview

| Sprint | Focus | Duration | Deliverable |
|--------|-------|----------|-------------|
| 1 | Foundation + Anti-False-Pass | 2 weeks | Basic eval runner with programmatic + claim grading, red-team suite |
| 2 | DIKW + Semantic + RDF* Annotations | 2 weeks | Full 3-level grading, emergence scoring with edge provenance |
| 3 | LLM Judge + CI + Polish | 2 weeks | Production-ready eval with judge, parallel execution, stability canary |
| 4 | SPARQL Endpoint + Full RDF* | 2 weeks | SPARQL query endpoint, serialization formats, eval integration |

---

## Sprint 1: Foundation + Anti-False-Pass

**Priority: Address EF01 (RPN 336), EF16 (RPN 320), EF03 (RPN 224) — the top false-pass modes.**

| Task | Description | Size | Source | Tests |
|------|-------------|------|--------|-------|
| S1-T1 | Create `crates/ferrosa-memory-eval/` workspace member with Cargo.toml, depend on ferrosa-memory-core | S | architect | Compiles, workspace resolves |
| S1-T2 | Implement `scenario.rs` — TOML parser for EvalScenario, EvalStep, GroundTruth. Validate against `tool_definitions()` | M | architect | Parse 5 sample TOMLs, reject malformed |
| S1-T3 | Implement `mcp_client.rs` — JSON-RPC stdio client, spawn MCP server as child process | M | architect | Connect, call `initialize`, call `get_stats` |
| S1-T4 | Implement `runner.rs` — execute steps, collect ToolCallTrace, session isolation (fresh session_id per scenario), pre/post cleanup | M | architect, EF07 | Run 3-step scenario, verify traces recorded |
| S1-T5 | Implement `grading/programmatic.rs` — schema validation, tool sequence matching, field assertions with **entity identity verification** (EF04 fix) | M | architect, EF04 | Correct/incorrect sequence detection, wrong-entity detection |
| S1-T6 | Implement `grading/claim_rubric.rs` — **word-boundary regex matching** (EF01 fix), claim polarity (positive/negative), partial credit | M | EF01 (RPN 336) | ET01-ET03: adversarial substring tests, 0% false positive rate |
| S1-T7 | Implement `report.rs` — CLI text output, JSON serialization, score normalization (0-1 scale, EF25 fix) | S | architect | Formatted output for 5 scenarios |
| S1-T8 | Write 5 Level 1 scenarios: memo_cache, entity_crud, fold_lifecycle, plan_hierarchy, temporal_facts | M | dsm | All pass against live cluster |
| S1-T9 | Write 3 "red team" scenarios — engineered to trigger false-pass conditions (lenient claims, wrong entity, search fallback) | M | FMEA systemic | All must FAIL. If any passes, grader has a bug. |
| S1-T10 | Pre-flight health check: verify Ferrosa nodes UP + CQL < 100ms before eval starts | S | FMEA EF-CQL | Eval aborts with clear message if cluster unhealthy |
| S1-T11 | Warm-up phase: run throwaway scenario before scoring (EF09 fix) | S | EF09 (RPN 75) | First scored scenario not penalized by cold start |

**Sprint 1 exit criteria:** `ferrosa-memory-eval` binary runs 8 L1 scenarios + 3 red-team scenarios against live cluster. Zero false passes on red-team suite. Results in JSON + CLI text.

---

## Sprint 2: DIKW + Semantic + RDF*

**Priority: Address EF02 (RPN 245), EF12 (RPN 196), EF11 (RPN 180), EF16 (RPN 320).**

| Task | Description | Size | Source | Tests |
|------|-------------|------|--------|-------|
| S2-T1 | Implement `dikw/data_info.rs` — entity type checking, temporal scoping, session isolation verification. Settle delay for eventual consistency (EF10 fix). | M | architect, EF10 | Type correctness vs ground truth, temporal chain present |
| S2-T2 | Implement `dikw/info_knowledge.rs` — consolidation edge counting with **deduplication for symmetric edges** (EF11 fix), search recall@k measurement | M | EF11 (RPN 180) | ET18: CO_OCCURS counted correctly (not 2x) |
| S2-T3 | Implement `dikw/knowledge_wisdom.rs` — intention trigger verification with **context correctness check** (EF12 fix), smart_ingest decision scoring, predict_needed accuracy | M | EF12 (RPN 196) | ET14: wrong-context trigger scored as incorrect |
| S2-T4 | Implement `dikw/emergence.rs` — before/after graph snapshots, edge provenance filtering (**only count synthesis-tool edges**, ET-E2 threat fix), derived fact filtering (EF13 fix: exclude base facts) | L | EF02, EF13, threat ET-E2 | ET06-ET07: garbage edges penalized, not rewarded |
| S2-T5 | Implement `semantic/inference.rs` — derived fact verification with **full tuple matching** (EF03 fix), provenance chain depth weighting (EF18 fix) | M | EF03 (RPN 224), EF18 | ET08-ET09: swapped args detected, depth-weighted scoring |
| S2-T6 | Implement `semantic/ontology.rs` — type accuracy vs ground truth (not just coverage, EF05 fix), canonical type list comparison | M | EF05 (RPN 180) | ET17: "concept" fallback penalized |
| S2-T7 | Implement `semantic/graph_quality.rs` — density excluding self-edges (EF15 fix), connectivity components, avg path length | M | EF15 | ET23: self-edges excluded from density |
| S2-T8 | Implement `semantic/multi_hop.rs` — **path verification** (EF16 fix): require intermediate entities in result, check tool call sequence includes graph traversal | L | EF16 (RPN 320) | ET04-ET05: search-fallback detected and scored as fail |
| S2-T9 | Implement `semantic/dedup.rs` — **dual-layer testing**: ingest-time (smart_ingest) + offline (find_duplicates) (EF17 fix) | M | EF17 (RPN 180) | ET19: ingest-time dedup tested separately |
| S2-T10 | **RDF* edge provenance**: extend TypedEdge schema with `created_by` (explicit/consolidation/datalog/spread), `confidence`, `derived_at` fields. This enables emergence scoring (S2-T4) and inference auditing (S2-T5). | L | RDF*, threat ET-E2 | Edges carry provenance. Eval can filter by creation method. |
| S2-T11 | **RDF* statement annotations**: extend CQL schema to support metadata on edges (statement-about-statement). Map to existing `metadata` TEXT field as structured JSON initially, with migration path to native RDF* triples. | M | RDF*, semantic repo | Annotations queryable. Provenance chain traversable. |
| S2-T12 | Write 5 Level 2 DIKW scenarios: contextualization, consolidation_discovery, recursive_exploration, smart_ingest_decisions, emergent_relationships | L | architect | All produce DIKW scores |
| S2-T13 | Write 5 Level 3 Semantic scenarios: inference_correctness, ontological_consistency, graph_completeness, multi_hop_reasoning, semantic_dedup | L | architect | All produce Semantic scores |

**Sprint 2 exit criteria:** Full 3-level grading operational. Edge provenance via RDF* annotations. 18 scenarios total (8 L1 + 5 L2 + 5 L3). Emergence scoring correctly distinguishes explicit vs. system-discovered edges.

---

## Sprint 3: LLM Judge + CI + Polish

**Priority: Address EF06 (RPN 210), EF19 (RPN 196), EF20 (RPN 180) — judge reliability and non-determinism.**

| Task | Description | Size | Source | Tests |
|------|-------------|------|--------|-------|
| S3-T1 | Implement `grading/llm_judge.rs` — Claude API integration, **structured JSON output** (EF06 fix), temperature=0, response sanitization (ET-T2 threat: prompt injection stripping) | L | EF06, EF19, threat ET-T2 | ET10-ET11: calibration with known-bad, 9/10 stability |
| S3-T2 | Cross-validation: if programmatic FAIL but judge PASS, flag anomalous (threat ET-T2 mitigation) | M | threat ET-T2 | Known-bad scenario triggers anomaly flag |
| S3-T3 | Implement `grading/tool_usage.rs` — latency tracking, unnecessary call detection, token cost estimation | M | architect | Detect extra tool calls beyond expected sequence |
| S3-T4 | McpQualityScores computation — map grading results to 1-5 Accuracy/Completeness/Relevance/Clarity/Reasoning | M | architect | Scores reflect tool quality, not just pass/fail |
| S3-T5 | **Stability canary**: run 3 identical scenarios, assert identical scores. Any divergence halts the run. (EF19, EF20, EF21 fix) | M | FMEA systemic | Canary catches non-determinism before it reaches reports |
| S3-T6 | Judge verdict caching: keyed on (scenario_id, response_content_hash). Prevents re-evaluation flakiness. | S | EF19 | Cached verdict reused for identical inputs |
| S3-T7 | HTTP transport mode for `mcp_client.rs` (connect to running server, not just stdio spawn) | M | architect | Connect to HTTP endpoint, run scenarios |
| S3-T8 | `--parallel` support with per-scenario session isolation. Unique tenant_id per eval run. | M | architect, EF07 | 3 scenarios run concurrently, no cross-contamination |
| S3-T9 | Scenario manifest: SHA-256 checksums of all TOML + ground_truth files, logged in report (threat ET-S1 mitigation) | S | threat ET-S1 | Manifest matches expected checksums |
| S3-T10 | Server identity verification: record binary hash + `initialize` response in report (threat ET-S2) | S | threat ET-S2 | CI validates server identity |
| S3-T11 | Cleanup ledger: track all session_ids created, sweep stale sessions >1hr old from eval tenant (EF-D3 threat) | M | threat ET-D3 | No residual data after eval run |
| S3-T12 | Write 3 regression scenarios (known bugs: co_occurs session mismatch, edge dedup, ghost rows) | M | regression | Verify known bugs stay fixed |
| S3-T13 | CI integration: eval job in GitHub Actions, gated (not per-commit), requires live cluster | M | architect | CI runs eval, reports results |
| S3-T14 | Documentation: README for scenarios/, how to write new scenarios, how to interpret reports | S | architect | New developer can write a scenario from docs |

**Sprint 3 exit criteria:** Production-ready eval with LLM judge, parallel execution, stability canary, CI integration. 21+ scenarios. Full report with L1/L2/L3 scores.

---

## Sprint 4: SPARQL Endpoint + Full RDF*

**Priority: Complete semantic repository capabilities. Enable SPARQL-based eval verification.**

| Task | Description | Size | Source | Tests |
|------|-------------|------|--------|-------|
| S4-T1 | Create `crates/ferrosa-memory-sparql/` workspace member. Depend on `spargebra` crate for SPARQL parsing. | S | RDF* spec | Compiles, parses basic SELECT |
| S4-T2 | Implement `parser.rs` — SPARQL text → algebra tree via spargebra. Support SELECT, WHERE, FILTER, ORDER BY, LIMIT. | M | RDF* spec | Parse 10 representative queries |
| S4-T3 | Implement `planner.rs` — translate algebra tree to CQL queries + Datalog evaluation plans. Basic graph patterns → typed_edge queries. FILTER → CQL WHERE + Rust predicates. | L | RDF* spec | Plan generation for triple patterns, filters, joins |
| S4-T4 | Implement `executor.rs` — run plans against Storage trait. Join result sets. Handle OPTIONAL (left-join semantics). | L | RDF* spec | Execute 5 queries against live cluster, verify results |
| S4-T5 | Implement `rdf_star.rs` — RDF* annotation queries: `<< ?s ?p ?o >> ?prop ?val` translates to edge_annotations table joins. | M | RDF* spec | Query edge confidence, created_by, derived_at via SPARQL* |
| S4-T6 | Implement `results.rs` — SPARQL JSON Results format (`application/sparql-results+json`). | S | RDF* spec | Valid JSON results per W3C spec |
| S4-T7 | Implement Turtle serialization (`text/turtle`) for CONSTRUCT and entity export. | M | RDF* spec | Valid Turtle output, round-trip parse test |
| S4-T8 | Implement `endpoint.rs` — HTTP handler on web console port. GET/POST `/sparql`. Content negotiation for result formats. | M | RDF* spec | curl query returns results in requested format |
| S4-T9 | Implement `namespace.rs` — standard prefix management (foaf, dc, prov, rdf, rdfs, owl, ex). Auto-expand prefixed URIs. | S | RDF* spec | `foaf:Person` expands to full IRI |
| S4-T10 | Add optional `uri: Option<String>` to EntityEntry and TypedEdge structs. Migration for existing data. | S | RDF* spec | Entities with URIs queryable via SPARQL |
| S4-T11 | Property path support: `?s foaf:knows+ ?o` maps to `spread_activation` or BFS traversal. | L | RDF* spec | Transitive closure queries work via SPARQL paths |
| S4-T12 | Integrate SPARQL into eval framework: add `sparql_verify` step type in scenarios. Semantic Analyzer uses SPARQL for graph state inspection instead of raw CQL. | M | eval architect | Eval scenarios can include SPARQL verification queries |
| S4-T13 | Write 3 SPARQL-based eval scenarios: RDF* annotation queries, multi-hop property paths, inference verification via SPARQL | M | eval architect | Scenarios pass against live cluster |
| S4-T14 | N-Triples export (`application/n-triples`) for bulk graph dump. | S | RDF* spec | Valid N-Triples output |
| S4-T15 | Implement `update.rs` — SPARQL UPDATE parser (INSERT DATA, DELETE DATA, DELETE/INSERT MODIFY). Parse via spargebra's update support. | M | RDF* spec | Parse 5 representative UPDATE queries |
| S4-T16 | Implement `write_plan.rs` — translate UPDATE algebra to Storage trait calls. INSERT DATA → `typed_edge_put` + `entity_put` + `annotation_put`. DELETE DATA → scoped removal. MODIFY → atomic delete+insert. | L | RDF* spec | INSERT creates entities/edges visible via SELECT. DELETE removes them. |
| S4-T17 | RDF* annotated inserts: `<< ?s ?p ?o >> ?prop ?val` in INSERT DATA writes to `edge_annotations` table. All writes carry `created_by: "sparql"` provenance. | M | RDF* spec | Annotated triples queryable after insert |
| S4-T18 | Tenant/session scoping for writes: extract tenant + session from HTTP auth context. Reject cross-tenant writes. Audit log every UPDATE with full query text. | M | threat model | No cross-tenant writes. Audit trail complete. |
| S4-T19 | Pattern-matched bulk operations: `INSERT { ... } WHERE { ... }` and `DELETE { ... } WHERE { ... }` — query first, then apply mutations to matching results. | L | RDF* spec | Bulk insert/delete by pattern. Verify with follow-up SELECT. |
| S4-T20 | LOAD support: `LOAD <file:///path/to/data.ttl>` imports Turtle/N-Triples into a session. Uses batch ingest for performance. | M | RDF* spec | Import 1000-triple file, verify via SELECT count. |

**Sprint 4 exit criteria:** `/sparql` endpoint serving reads AND writes. INSERT DATA, DELETE DATA, and MODIFY work with RDF* annotations. Tenant-scoped with audit logging. Eval framework uses SPARQL for both verification and scenario setup. Property paths resolve via graph traversal.

---

## Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| LLM judge prompt injection (ET-T2) | High | Critical | Sanitization + cross-validation (S3-T1, S3-T2) |
| False passes dominate eval (FMEA systemic) | High | Critical | Red-team suite in Sprint 1 (S1-T9) |
| Non-determinism erodes trust (EF19-22) | Medium | High | Stability canary (S3-T5), temperature=0, caching |
| RDF* schema migration complexity | Medium | Medium | Start with JSON in metadata field, migrate to native later (S2-T11) |
| Ferrosa cluster instability during eval | Medium | High | Pre-flight health check (S1-T10), isolated cluster |
| Claude API cost for LLM judge | Low | Medium | Optional per scenario, caching, --no-llm-judge flag |

## Dependencies

```
Sprint 1: [no external deps — pure framework + programmatic grading]
Sprint 2: [depends on Sprint 1 runner + graders]
  S2-T10 (RDF* provenance) → S2-T4 (emergence scoring)
  S2-T11 (RDF* annotations) → S2-T5 (inference auditing)
Sprint 3: [depends on Sprint 2 DIKW/Semantic modules]
  S3-T1 (LLM judge) → S3-T2 (cross-validation)
  S3-T5 (stability canary) → S3-T13 (CI integration)
```

## Success Metrics

| Metric | Target | Measured By |
|--------|--------|-------------|
| Red-team false-pass rate | 0% | All red-team scenarios must FAIL |
| Stability canary pass rate | 100% (3/3 identical) | S3-T5 |
| L1 MCP quality baseline | >3.5/5.0 on current codebase | Sprint 1 exit |
| L2 DIKW composite baseline | >0.60 on current codebase | Sprint 2 exit |
| L3 Semantic composite baseline | >0.50 on current codebase | Sprint 2 exit |
| Eval suite runtime | <120s for 21 scenarios | Sprint 3 exit |
| Judge stability | >=9/10 identical verdicts | S3-T5, ET15 |
