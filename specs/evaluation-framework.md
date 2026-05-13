# fmem Evaluation Framework v1.0

> **Goal:** Quantify whether ferrosa-memory (fmem) is *helping* agents work better, not just whether it stores and retrieves facts correctly. Inspired by [MemoryArena (arXiv:2602.16313)](https://arxiv.org/abs/2602.16313) and synthesized from 7 related papers.

---

## 1. Core Insight from MemoryArena

Existing memory benchmarks (LoCoMo, LongMemEval) measure **memorization** ("do you remember X?") in isolation. MemoryArena introduces the **Memory-Agent-Environment loop**: later tasks depend on earlier actions and feedback. Memory is only "working" if it improves **task-level decision-making** in interdependent multi-session loops.

**For fmem, this means:**
- We must measure whether fmem-retrieved context improves an agent's ability to complete multi-step, cross-session tasks.
- Single-tool correctness ("did `smart_ingest` return OK?") is necessary but not sufficient.
- The eval must exercise the full loop: **ingest → decay/consolidate → retrieve → use in downstream reasoning → complete task**.

---

## 2. Evaluation Dimensions (6 Axes)

| Dimension | What It Measures | Source Papers | fmem-Specific |
|-----------|-----------------|---------------|---------------|
| **A. Retrieval Quality** | Can fmem find the right memory when needed? | LongMemEval, Survey | Hybrid search (ANN + phonetic), cross-session stability |
| **B. KG Structure Quality** | Is the knowledge graph well-formed and useful? | Graph Recall (NeurIPS'24) | Edge precision/recall, microstructure fidelity, graph density |
| **C. Temporal / Episodic** | Does time-aware memory work correctly? | LoCoMo, LongMemEval | Warmth decay, confidence scoring, temporal fact chains, freshness |
| **D. Inference & Derivation** | Are derived facts logically sound and useful? | MemoryArena (reasoning) | Datalog `query_derived`, `recursive_explore`, `spread_activation` |
| **E. Task-Level Effectiveness** | Does fmem *help* agents complete real tasks? | **MemoryArena** (primary) | With/without fmem A/B on multi-session agent tasks |
| **F. Efficiency & Cost** | What's the overhead of using fmem? | Survey (8th metric family) | Latency, throughput, storage per entity, embedding cost |

---

## 3. Metrics & Measurement Methods

### A. Retrieval Quality Metrics

| Metric | Definition | Measurement |
|--------|-----------|-------------|
| **Recall@k** | Fraction of relevant memories in top-k | Ground-truth relevance labels on scenario queries |
| **nDCG@k** | Ranking quality with graded relevance | Compare retrieval order against human-annotated relevance |
| **Hit@k** | Is correct answer in top-k? | Binary per query |
| **Cross-session consistency** | Same query → same results across sessions | Re-run queries after time decay, measure Jaccard similarity of top-5 |
| **Search strategy accuracy** | Does phonetic catch variants? Does ANN catch semantics? | Controlled entity set with deliberate name/semantic variants |

**fmem-specific retrieval features to exercise:**
- `hybrid_search` (ANN + phonetic fusion)
- `retrieve_entities` (phonetic name matching)
- `explore_connections` (graph neighborhood)
- `find_memory_chain` (path search)
- `predict_needed` (proactive context loading)

### B. KG Structure Quality Metrics

| Metric | Definition | Measurement |
|--------|-----------|-------------|
| **Edge precision** | Fraction of edges that are semantically correct | Human annotation or LLM judge on edge meaningfulness |
| **Edge recall** | Fraction of ground-truth relationships captured | Compare against annotated "should exist" edges |
| **Graph density** | `|E| / (|V| * (|V| - 1))` excluding self-edges | Count entities/edges post-scenario |
| **Microstructure fidelity** | Do local patterns (triangles, 2-paths) match ground truth? | ERGM-style analysis (adapted from Graph Recall paper) |
| **Ontological consistency** | Same concept → same type across sessions | Type stability score over repeated ingests |
| **Connected component ratio** | `|CC| / |V|` — lower is better | Graph analytics query |

**fmem-specific KG features to exercise:**
- `create_edge` / `batch_create_edges` (explicit edges)
- `run_consolidation` (CO_OCCURS auto-discovery)
- `smart_ingest` (auto-linking, supersession)
- `find_duplicates` (deduplication quality)

### C. Temporal / Episodic Metrics

| Metric | Definition | Measurement |
|--------|-----------|-------------|
| **Warmth decay accuracy** | Does warmth follow Ebbinghaus curve? | Ingest entity → query at t=0, 1h, 24h → compare warmth values to expected |
| **Confidence calibration** | High-confidence facts are more accurate | Score confidence vs. ground-truth correctness |
| **Temporal chain accuracy** | `get_temporal_chain` returns facts in order | Verify fact sequence matches ingest order |
| **Freshness hit rate** | Recent facts are retrieved for time-sensitive queries | Time-biased queries should prefer recent facts |
| **Contradiction detection rate** | Conflicting facts are properly flagged | Inject contradictory facts, measure `contradiction` tool detection |
| **Decay pass completeness** | `run_decay_and_forget` correctly prunes low-warmth entities | Pre/post decay entity count + warmth threshold check |

**fmem-specific temporal features:**
- `write_temporal_fact` + `get_temporal_chain`
- `warmth_decay_all` (Ebbinghaus decay)
- `confidence_put` / `confidence_get`
- `prune_forgotten` (threshold-based forgetting)
- `contradiction` detection

### D. Inference & Derivation Metrics

| Metric | Definition | Measurement |
|--------|-----------|-------------|
| **Inference correctness** | Derived facts are logically valid | Ground-truth Datalog rules → query derived facts → verify |
| **Derivation coverage** | What fraction of derivable facts are found? | Exhaustive rule evaluation on small controlled graphs |
| **Provenance accuracy** | `query_derived` returns correct support chain | Walk provenance, verify each parent fact exists |
| **Recursive explore convergence** | Does `recursive_explore` terminate with correct results? | Bounded-depth graph, verify results at each depth |
| **Spread activation relevance** | `spread_activation` returns semantically related entities | Human relevance judgments on activated neighborhood |

**fmem-specific inference features:**
- `query_derived` (Datalog fact derivation)
- `recursive_explore` (bounded graph traversal)
- `spread_activation` (PPR-based relevance)
- `manage_rules` (rule registry)
- `evaluate_rule_with_aggregates` (count aggregates)

### E. Task-Level Effectiveness Metrics *(The MemoryArena Contribution)*

This is the **most important dimension**. We measure whether an agent *with fmem* outperforms an agent *without fmem* on interdependent multi-session tasks.

| Metric | Definition | Measurement |
|--------|-----------|-------------|
| **Task Success Rate (SR)** | % of tasks fully completed correctly | A/B: agent+fmem vs. agent+no-memory vs. agent+long-context-only |
| **Task Progress Score (PS)** | Fraction of subtasks correctly completed | `PS = (1/N) Σ (passed_subtasks / total_subtasks)` |
| **soft Process Score (sPS)** | Partial credit for hard tasks | Per-constraint satisfaction scoring |
| **SR@k** | Success rate at subtask depth k | Measure decay as interdependency chain lengthens |
| **Memory benefit ratio** | `(SR_with_fmem - SR_without) / SR_without` | Quantifies net improvement |
| **Ablation benefit** | Per-feature ablation (no warmth, no confidence, no Datalog) | Remove one feature at a time, measure SR drop |

**Task scenarios to implement:**
1. **Multi-session debugging** — Agent investigates a bug across 3+ sessions, must recall prior findings
2. ** evolving requirements** — User changes requirements; agent must track versions and apply latest
3. **Cross-project knowledge transfer** — Agent works on Project A, then B; must recall relevant patterns from A
4. **Long-horizon planning** — 10-step plan where step 5 depends on step 2's outcome stored in fmem
5. **Contradiction resolution** — User states conflicting facts; agent must detect and resolve using confidence scores

### F. Efficiency & Cost Metrics

| Metric | Definition | Measurement |
|--------|-----------|-------------|
| **Latency (p50/p95/p99)** | End-to-end tool call latency | Instrumented in scenario runner |
| **Throughput** | Entities/edges ingested per second | Bulk ingest scenarios |
| **Storage per entity** | Bytes per entity including embeddings | `get_stats` + storage inspection |
| **Embedding cost** | Tokens/time spent on embedding generation | Ollama client instrumentation |
| **Query cost** | CQL queries per user request | MCP server metrics (Prometheus) |
| **Cost@target** | Cost to achieve 80% SR | Derived from above |

---

## 4. Benchmark Scenarios

Building on the existing `ferrosa-memory-eval` TOML scenario format (L1/L2/L3), we add **Task-Level (L4)** scenarios:

### Level 4: Task-Loop Scenarios (New)

These are agent-facing scenarios where an LLM agent uses fmem to complete a task. The eval framework runs the agent loop and scores outcomes.

```toml
[scenario]
id = "task-debugging-cross-session"
name = "Multi-session bug investigation with memory"
level = 4
task_type = "debugging"
tags = ["task-level", "cross-session", "episodic"]

# Session 1: Initial investigation
[[sessions]]
name = "session-1"
[[sessions.steps]]
agent_prompt = "The user reports login failures. Investigate and store findings in memory."
tools_allowed = ["smart_ingest", "hybrid_search", "write_temporal_fact"]
expected_findings = ["OAuth token expiry", "redis connection pool exhausted"]

# Session 2: Follow-up (days later) — must recall Session 1
[[sessions]]
name = "session-2"
depends_on = ["session-1"]
[[sessions.steps]]
agent_prompt = "User reports login still failing. Check your memory for prior findings and continue investigation."
tools_allowed = ["hybrid_search", "get_temporal_chain", "smart_ingest"]
expected_findings = ["OAuth token expiry", "redis connection pool exhausted", "new: session store timeout"]

[grading]
methods = ["task_success", "claim_rubric", "ablation"]

[grading.task_success]
# Did the agent correctly identify all root causes?
subtasks = [
  "Identify OAuth token expiry",
  "Identify redis connection pool exhaustion",
  "Identify session store timeout"
]

[grading.ablation]
# Compare against no-memory baseline
baseline = "no_memory"
features = ["warmth", "confidence", "datalog", "consolidation"]
```

---

## 5. Implementation Plan

### Phase 1: Instrumentation (Week 1)
- [ ] Add latency/token/cost instrumentation to `ferrosa-memory-mcp` tool handlers
- [ ] Export Prometheus metrics for all 6 dimensions
- [ ] Add `eval_mode` flag to MCP server (isolated tenant, deterministic embedding)

### Phase 2: Metric Collectors (Week 2)
- [ ] Implement `retrieval_metrics.rs` — Recall@k, nDCG, Hit@k
- [ ] Implement `kg_metrics.rs` — edge precision/recall, density, microstructure
- [ ] Implement `temporal_metrics.rs` — decay accuracy, confidence calibration, freshness
- [ ] Implement `inference_metrics.rs` — derivation correctness, provenance accuracy

### Phase 3: Task-Level Harness (Weeks 3-4)
- [ ] Implement `task_agent.rs` — LLM agent loop that uses fmem via MCP
- [ ] Implement `task_grader.rs` — task success scoring, ablation framework
- [ ] Write 5 L4 task scenarios (debugging, requirements, cross-project, planning, contradiction)
- [ ] Run A/B: with/without fmem, with/without individual features

### Phase 4: Report & Dashboard (Week 5)
- [ ] Aggregate all 6 dimensions into composite fmem Health Score
- [ ] Time-series tracking (SR@k over sessions, decay curves)
- [ ] CI integration: run eval nightly, report regression

---

## 6. Baselines & Comparison

| Baseline | What It Tests |
|----------|--------------|
| **No memory** | Raw LLM with only in-context window |
| **Long-context only** | LLM with 128K+ context, no external memory |
| **Plain RAG** | BM25 + embedding retrieval, no graph/temporal/warmth |
| **Mem0** | External memory baseline (if API available) |
| **Letta** | Agent memory baseline (if API available) |

**A/B protocol:**
1. Same agent model (e.g., GPT-4.1-mini) across all conditions
2. Same task scenarios, randomized order
3. 3 runs per condition, report mean ± std
4. Feature ablation: full fmem → no warmth → no confidence → no Datalog → no consolidation

---

## 7. Deliverables

| Deliverable | Location | Status |
|-------------|----------|--------|
| This framework document | `specs/evaluation-framework.md` | **v1.0** |
| Paper corpus + synthesis | `~/corpus/memory_evaluation_papers_summary.md` | Complete |
| Scenario definitions | `crates/ferrosa-memory-eval/scenarios/level4/*.toml` | To be written |
| Metric collectors | `crates/ferrosa-memory-eval/src/metrics/*.rs` | To be written |
| Task agent harness | `crates/ferrosa-memory-eval/src/task_agent.rs` | To be written |
| CI integration | `.github/workflows/eval.yml` | To be written |
| Living report | `eval-results/latest.json` + dashboard | To be written |

---

## 8. References

| Paper | ArXiv | Key Contribution |
|-------|-------|-----------------|
| **MemoryArena** | 2602.16313 | Task-level multi-session evaluation loop |
| LoCoMo | 2402.17753 | Long-term conversational memory QA |
| LongMemEval | 2410.10813 | 5 core memory abilities, 115K-1.5M tokens |
| Memory in LLMs: Survey | 2509.18868 | 8 metric families, 3-setting protocol |
| MemoryCD | 2603.25973 | Real-user cross-domain personalization |
| Graph Recall by LLMs | 2402.11821 | ERGM microstructure fidelity for KGs |
| Episodic Memory Missing | 2502.06975 | Position paper on episodic memory |
| MAFBench | 2602.03128 | Multi-agent framework benchmark |

---

## 9. Open Questions

1. **LLM-as-Agent cost:** Running 3+ LLM calls per task scenario is expensive. Should we use a smaller model (e.g., Qwen-7B local) for the task agent, or cache agent trajectories?
2. **Ground truth generation:** Task scenarios need human-annotated "correct" answers. Should we bootstrap with GPT-4o-generated ground truth and review?
3. **Real vs. synthetic tasks:** MemoryArena uses synthetic but realistic tasks. Should fmem eval use synthetic tasks or integrate with real user session logs (anonymized)?
4. **Cross-model generalization:** Does fmem benefit scale with model capability? Should we test across model sizes?
5. **PDF parser upgrade:** The paper ingestion pipeline currently uses `pdftotext`. [OpenDataLoader PDF](https://github.com/opendataloader-project/opendataloader-pdf) provides structured extraction (tables, headings, JSON metadata). Should we migrate the corpus pipeline?

---

*Framework version: 1.0 | Based on MemoryArena (2602.16313) + 7 related papers | For ferrosa-memory eval crate*

---

## Appendix A: PDF Parser Comparison for Paper Ingestion

When building this framework we evaluated [OpenDataLoader PDF](https://github.com/opendataloader-project/opendataloader-pdf) against our current `pdftotext` pipeline.

| Capability | OpenDataLoader PDF | pdftotext (current) |
|---|---|---|
| Tables | **Excellent** — Markdown tables with row/column structure | Lost / jumbled |
| Section structure | **Excellent** — heading hierarchy as `##`/`###` | None |
| Multi-column layouts | Correct reading order | Mostly correct |
| JSON metadata | Element types, bounding boxes, page numbers | None |
| Speed | ~1s for 15 pages | ~0.1s |
| Equations | Plain text (local mode); LaTeX (hybrid mode) | Plain text only |

**Recommendation:** Migrate the paper ingestion pipeline to OpenDataLoader PDF. Use default fast (local) mode with `format="markdown,json"` for clean text + structured metadata. Keep `pdftotext` as fallback.

---

## Appendix B: Paper Corpus

All papers downloaded to `~/corpus/` for permanent preservation:

```
~/corpus/
├── 2602.16313.pdf          # MemoryArena (primary)
├── 2602.16313_layout.txt   # Extracted text
├── locomo.pdf              # LoCoMo (2402.17753)
├── longmemeval.pdf         # LongMemEval (2410.10813)
├── memory_llm_survey.pdf   # Survey (2509.18868)
├── memorycd.pdf            # MemoryCD (2603.25973)
├── graph_recall_llm.pdf    # Graph Recall (2402.11821)
├── episodic_memory_missing.pdf  # Episodic Memory (2502.06975)
├── mafbench.pdf            # MAFBench (2602.03128)
└── memory_evaluation_papers_summary.md  # This synthesis
```
