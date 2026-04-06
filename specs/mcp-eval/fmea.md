# MCP Eval Framework — FMEA

## Scoring: S(everity) x O(ccurrence) x D(etection) = RPN. Action for RPN >= 50.

## Top 10 Failure Modes by RPN

| RPN | ID | Component | Failure Mode | Category |
|-----|----|-----------|-------------|----------|
| **336** | EF01 | Claim Rubric | Substring match too lenient — "entity created" matches "no entity created" | False Pass |
| **320** | EF16 | Semantic/Multi-hop | Multi-hop test passes via search fallback, not graph reasoning | False Pass |
| **245** | EF02 | DIKW/Emergence | Edge correctness not verified — garbage CO_OCCURS edges inflate density | False Pass |
| **224** | EF03 | Semantic/Inference | Derived facts checked by predicate name only, not argument bindings | False Pass |
| **210** | EF06 | LLM Judge | Vague rubrics cause judge to PASS anything plausible | False Pass |
| **210** | EF07 | Scenario Runner | Cross-scenario state leakage via tenant-level entities | False Fail |
| **196** | EF12 | DIKW/Wisdom | Intentions trigger on wrong context scored as success | DIKW Error |
| **196** | EF19 | LLM Judge | Non-deterministic verdicts across runs (temperature, prompt sensitivity) | Non-Determinism |
| **180** | EF05 | Semantic/Ontology | Type coverage hides misclassification (NER "concept" fallback) | False Pass |
| **180** | EF11 | DIKW/Knowledge | Symmetric edges double-counted in knowledge gain (2x inflation) | DIKW Error |

## Full FMEA Table (25 failure modes)

### False Passes (system appears correct but isn't)

| ID | Component | Failure Mode | S | O | D | RPN | Action |
|----|-----------|-------------|---|---|---|-----|--------|
| EF01 | Claim Rubric | Substring match too lenient | 8 | 7 | 6 | 336 | Word-boundary regex. Claim polarity. Adversarial ground-truth test cases. |
| EF02 | DIKW Emergence | Edge correctness unverified | 7 | 5 | 7 | 245 | Sample N edges post-consolidation, validate via LLM or ground truth. Flag if >30% meaningless. |
| EF03 | Semantic Inference | Predicate-only match, not bindings | 8 | 4 | 7 | 224 | Ground truth specifies full tuples `(pred, arg0, arg1)`. Exact-match on entity IDs. |
| EF04 | Programmatic | Correct action on wrong entity | 7 | 4 | 6 | 168 | Add `expect_entity_name`. Cross-reference entity_id with retrieval. |
| EF05 | Semantic Ontology | Type coverage hides misclassification | 6 | 5 | 6 | 180 | Score = correct_types / expected_types. Penalize "concept" fallback. |
| EF06 | LLM Judge | Vague rubrics → false PASS | 6 | 5 | 7 | 210 | Include PASS/FAIL examples in rubrics. Structured JSON output. Calibrate with known-bad runs. |

### False Fails (system correct but eval rejects)

| ID | Component | Failure Mode | S | O | D | RPN | Action |
|----|-----------|-------------|---|---|---|-----|--------|
| EF07 | Scenario Runner | Cross-scenario state leakage | 7 | 6 | 5 | 210 | delete_session before AND after. Verify entity_count=0 pre-scenario. |
| EF08 | Programmatic | UUID/float format mismatch | 5 | 6 | 4 | 120 | Normalize UUIDs. Float-epsilon comparison. Allow regex patterns. |
| EF09 | Tool Usage | Cold-start latency fails first scenario | 5 | 5 | 3 | 75 | Warm-up phase. Percentile thresholds. Exclude first call. |
| EF10 | DIKW/Data-Info | Eventual consistency race on temporal chain | 6 | 4 | 5 | 120 | Settle delay (50-200ms). Retry with backoff for state assertions. |

### DIKW Scoring Errors

| ID | Component | Failure Mode | S | O | D | RPN | Action |
|----|-----------|-------------|---|---|---|-----|--------|
| EF11 | Info→Knowledge | Symmetric edges double-counted | 6 | 6 | 5 | 180 | Deduplicate by sorting (min_id, max_id). |
| EF12 | Knowledge→Wisdom | Intention triggers on wrong context | 7 | 4 | 7 | 196 | Ground truth specifies expected trigger context. Negative test with unrelated context. |
| EF13 | Emergence | Base facts counted as derived | 5 | 5 | 6 | 150 | Exclude facts whose pred+args match base facts. |
| EF14 | All sub-modules | Before-snapshot taken after first step | 6 | 3 | 4 | 72 | Snapshot before any tool calls. Assert entity_count=0. |

### Semantic Repository Scoring Errors

| ID | Component | Failure Mode | S | O | D | RPN | Action |
|----|-----------|-------------|---|---|---|-----|--------|
| EF15 | Graph Quality | Self-edges inflate density | 5 | 5 | 5 | 125 | Exclude self-edges and metadata edges from density calculation. |
| EF16 | Multi-hop | Test passes via search fallback | 8 | 5 | 8 | 320 | Verify path through graph, not just result. Require intermediate entities. Check tool call sequence. |
| EF17 | Dedup | Offline-only dedup testing | 6 | 5 | 6 | 180 | Test both ingest-time (smart_ingest) and offline (find_duplicates). Weighted composite. |
| EF18 | Inference | Shallow/deep derivations weighted equally | 5 | 4 | 6 | 120 | Weight by provenance chain length. Scenarios requiring 3+ hop derivation. |

### Non-Determinism

| ID | Component | Failure Mode | S | O | D | RPN | Action |
|----|-----------|-------------|---|---|---|-----|--------|
| EF19 | LLM Judge | Different verdicts across runs | 7 | 7 | 4 | 196 | Temperature=0. Cache verdicts by (scenario_id, response_hash). Majority vote (3x). |
| EF20 | Runner | Embedding non-determinism | 5 | 6 | 6 | 180 | Cosine-similarity thresholds. Pre-computed embeddings. Order-independent assertions. |
| EF21 | Runner | CQL consistency under load | 6 | 4 | 5 | 120 | ALL consistency for eval reads. Settle delay. FLAKY marker in report. |
| EF22 | Claim Rubric | Claim order dependency | 5 | 3 | 5 | 75 | Evaluate in declared order. Allow depends_on chaining. |

### Infrastructure

| ID | Component | Failure Mode | S | O | D | RPN | Action |
|----|-----------|-------------|---|---|---|-----|--------|
| EF23 | MCP Client | Server crash cascades | 7 | 3 | 3 | 63 | Detect crash, restart, mark step as ERROR not FAIL. |
| EF24 | Analyzers | Raw CQL vs MCP data divergence | 6 | 3 | 6 | 108 | Query through MCP tools where possible. Integration test: compare counts. |
| EF25 | Report | Mixed score scales in composite | 4 | 3 | 3 | 36 | Normalize to 0-1 before aggregation. Separate pass/fail per level. |

## Systemic Findings

### #1: False Pass Dominance
6 of top 10 RPNs are false-pass modes. Anti-pattern: **surface-level matching without semantic verification**.
**Fix:** "Red team" scenario suite — engineered to trigger false-pass conditions. If any passes, the grader has a bug.

### #2: Non-Determinism Cluster (combined RPN 571)
4 modes: LLM judge, embeddings, CQL consistency, claim ordering.
**Fix:** "Stability canary" — run 3 identical scenarios, assert identical scores. Any divergence halts the run.

### #3: CQL Single Point of Failure
All 50 tools + all analyzers depend on CQL. Flaky cluster = correlated failures.
**Fix:** Pre-flight health check (all nodes UP, SELECT responds < 100ms). Abort if unhealthy.

## Test Cases (32 tests for RPN >= 50)

See full test case table in the FMEA report. Key tests:
- ET01-ET03: Adversarial claim matching (EF01, RPN 336)
- ET04-ET05: Multi-hop path verification vs search fallback (EF16, RPN 320)
- ET06-ET07: Edge quality sampling post-consolidation (EF02, RPN 245)
- ET10-ET11: LLM judge calibration with known-bad scenarios (EF06, RPN 210)
- ET15-ET16: Judge stability (3x/10x runs, temperature=0) (EF19, RPN 196)
