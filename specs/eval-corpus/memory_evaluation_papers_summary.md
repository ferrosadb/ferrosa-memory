# Memory Evaluation Papers & Benchmarks — Research Summary

## Papers Downloaded to ~/corpus/

| Paper | File | ArXiv ID | Focus |
|---|---|---|---|
| LoCoMo | locomo.pdf | 2402.17753 | Long-term conversational memory QA benchmark |
| LongMemEval | longmemeval.pdf | 2410.10813 | Chat assistant long-term interactive memory (5 core abilities) |
| Memory in LLMs: Survey | memory_llm_survey.pdf | 2509.18868 | Comprehensive taxonomy + layered evaluation framework |
| MemoryCD | memorycd.pdf | 2603.25973 | Cross-domain lifelong personalization benchmark (real user data) |
| Graph Recall by LLMs | graph_recall_llm.pdf | 2402.11821 | Accuracy of graph structure recall from text (knowledge graph evaluation) |
| Episodic Memory Missing | episodic_memory_missing.pdf | 2502.06975 | Position paper on episodic memory for long-term LLM agents |
| MAFBench | mafbench.pdf | 2602.03128 | Multi-agent LLM framework benchmark (memory behavior) |

---

## 1. LoCoMo: Evaluating Very Long-Term Conversational Memory (arXiv:2402.17753)
**Authors:** Maharana, Lee, Tulyakov, Bansal, Barbieri, Fang (Snap/UNC/USC)

### What it measures
- Long-term memory in models across **300 turns / 9K tokens avg** over **up to 35 sessions**
- Three evaluation tasks:
  1. **Question Answering** — 5 reasoning types:
     - Single-hop (one session)
     - Multi-hop (multiple sessions)
     - Temporal reasoning (time/causality)
     - Open-domain knowledge (commonsense + world facts)
     - Adversarial (unanswerable → should abstain)
  2. **Event Summarization** — Extract causal/temporal event graph from conversation
  3. **Multi-modal Dialogue Generation** — Consistency with persona + events over time

### Key metrics
- **QA:** F1 score (exact + partial match), retrieval accuracy for RAG models
- **Event Summarization:** FactScore adapted for precision/recall of atomic facts vs ground-truth event graph
- **Dialogue Generation:** MMRelevance + standard NLG metrics

### Key findings
- Long-context LLMs improve 22-66% over base but still lag humans by 56% (73% on temporal)
- Long-context LLMs struggle with adversarial questions (-83% vs base)
- RAG with assertion database performs best balanced approach

---

## 2. LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory (arXiv:2410.10813)
**Authors:** Wu, Wang, Yu, Zhang, Chang, Yu (UCLA/Tencent AI Lab) — **ICLR 2025**

### What it measures
Five core long-term memory abilities via 500 curated questions:
1. **Information Extraction (IE)** — recall specifics from user or assistant turns
2. **Multi-Session Reasoning (MR)** — synthesize across sessions (aggregation/comparison)
3. **Temporal Reasoning (TR)** — timestamp metadata + explicit time references
4. **Knowledge Updates (KU)** — recognize and track changes in user info over time
5. **Abstention (ABS)** — correctly say "I don't know" for false-premise questions

### Scale
- **LongMemEval-S:** ~115K tokens per problem
- **LongMemEval-M:** 500 sessions (~1.5M tokens)
- Inspired by "needle-in-a-haystack" — freely extensible

### Key metrics
- Accuracy per ability type
- Memory recall@k (retrieval quality)
- Downstream QA accuracy

### Key findings
- Commercial chat assistants and long-context LLMs show **30% accuracy drop** on memorizing info across sustained interactions
- Memory design optimized across 3 stages: indexing, retrieval, reading
- Proposed optimizations: session→round granularity, fact-augmented keys, time-aware query expansion

---

## 3. Memory in LLMs: Mechanisms, Evaluation and Evolution (arXiv:2509.18868)
**Authors:** Zhang et al. (Digital China AI Research Institute)

### What it measures
A **unified taxonomy** of LLM memory with 4 types:
1. **Parametric** — facts encoded in weights (closed-book recall)
2. **Contextual** — working memory / visible context (position curves, mid-sequence drop)
3. **External** — RAG/retrieval (correctness × attribution/faithfulness)
4. **Procedural/Episodic** — cross-session consistency, timeline replay (E-MARS+)

### Evaluation protocol
- **Three-setting parallel protocol:** parametric-only (PO) / offline retrieval / online retrieval
- This **decouples capability from information availability**

### Key metrics (8 metric families)
| Family | Representative Metrics |
|---|---|
| Accuracy | EM, F1, ROUGE, Keyword Recall |
| Groundedness/Attribution | Citation Coverage, Unsupported Claim Rate (UCR), NLI consistency |
| Retrieval | Recall@k, nDCG, MRR, Hit@k |
| Sensitivity & Robustness | Length/position curves, noise/conflict stability |
| Timeliness | Freshness-Hit, Out-of-Date, selective accuracy, refusal rate |
| Maintainability | ESR (edit success rate), Locality, Drawdown |
| Privacy | Verbatim reproduction rate, membership inference AUC |
| Efficiency | Latency, throughput, cost@target |

### Key contribution
- **Minimum Reproducibility Disclosure (MRD)** — YAML schema for auditability
- Standardized reporting: temporal governance, leakage auditing, statistical testing

---

## 4. MemoryCD: Benchmarking Long-Context User Memory for Lifelong Cross-Domain Personalization (arXiv:2603.25973)
**Authors:** Zhang, Wei, Huang, Hui, Wang, Gong, Yu (Roblox/UIUC/Cambridge) — **ICLR 2026 Workshop**

### What it measures
**First cross-domain, real-user-data memory benchmark** based on Amazon Review dataset:
- 12 domains (Books, Movies, Health, Sports, etc.)
- Tracks **real users across years** — not synthetic LLM-generated personas
- 4 personalization tasks:
  1. **Personalized Rating Prediction** — MAE, RMSE
  2. **Review Generation** — style/tone alignment with user's historical reviews
  3. **Next-Item Recommendation** — Hit@k, NDCG
  4. **Cross-Domain Transfer** — preference transfer across domains

### Settings
- Single-domain memory vs cross-domain memory
- Tests whether agents can infer latent preferences from long-term behavioral traces

### Key findings
- Existing memory methods are **far from user satisfaction** across domains
- Cross-domain memory provides promising pathway but utilization strategies unexplored
- Evaluated 14 frontier LLMs + 6 memory system baselines

---

## 5. Microstructures and Accuracy of Graph Recall by LLMs (arXiv:2402.11821)
**Authors:** Wang, Cui, Kleinberg (Cornell/Stanford) — **NeurIPS 2024**

### What it measures
- **Graph recall** — can LLMs accurately recall graph structures described in text?
- Uses **Exponential Random Graph Model (ERGM)** to measure biased microstructures
- Compares LLM behavior to human cognitive studies on social network recall

### Key metrics
- Edge prediction accuracy (recall precision)
- Microstructure analysis: triangles, 2-paths, stars (via ERGM θ parameters)
- Downstream task impact: link prediction, graph summary, classification

### Key findings
- LLMs **underperform** in graph recall — even "simple" extraction is error-prone
- LLMs favor **triangles and alternating 2-paths** (similar compression heuristics to humans)
- More advanced LLMs show **domain-dependent performance** — best when narrative style matches original domain
- Graph recall errors propagate to downstream reasoning tasks

### Relevance to knowledge graph evaluation
- Provides methodology for measuring **precision/recall of entity/link extraction** from text
- ERGM framework can be adapted to evaluate KG construction quality

---

## 6. Episodic Memory is the Missing Piece for Long-Term LLM Agents (arXiv:2502.06975)
**Authors:** (position paper)

### Key argument
- Current agent memory systems lack **episodic memory** — the ability to store and replay specific experiences/events
- Proposes episodic memory as essential for:
  - Cross-session consistency
  - Learning from experience
  - Personalization over long horizons

---

## 7. MAFBench: Understanding Multi-Agent LLM Frameworks (arXiv:2602.03128)
**Authors:** (framework benchmark)

### What it measures
- Evaluates multi-agent LLM frameworks across:
  - Orchestration overhead
  - **Memory behavior**
  - Planning accuracy
  - Coordination success
- Framework-level design choices can increase latency 100x and reduce planning accuracy 30%

---

## Cross-Paper Comparison Matrix

| Benchmark | Type | Context Depth | # Sessions | Real User | Cross-Domain | Key Metric |
|---|---|---|---|---|---|---|
| LoCoMo | Synthetic dialog QA | ~9K tokens | 35 | ❌ | ❌ | F1 (QA), FactScore (summ) |
| LongMemEval | Task-oriented QA | 115K-1.5M tokens | 500 | ❌ | ❌ | Accuracy per 5 abilities |
| MemoryCD | Real-user personalization | ~400K tokens | 1000+ | ✅ | ✅ | MAE, RMSE, Hit@k |
| Graph Recall | Graph structure recall | Variable | N/A | N/A | N/A | Edge accuracy, ERGM |
| Survey | Taxonomy + framework | N/A | N/A | N/A | N/A | 8 metric families |

---

## Key Metrics for Evaluating Memory Systems (Synthesized)

### Retrieval Quality
- **Recall@k** — fraction of relevant memories retrieved in top-k
- **nDCG / MRR** — ranking quality
- **Hit@k** — whether correct memory is in top-k

### Answer Correctness
- **Exact Match (EM) / F1** — for QA-style evaluation
- **Accuracy** — per-ability-type (IE, MR, TR, KU, ABS)

### Faithfulness / Attribution
- **FactScore** — atomic fact precision/recall
- **Unsupported Claim Rate (UCR)** — hallucination detection
- **Citation Coverage** — evidence attribution

### Temporal / Episodic
- **Cross-session consistency** — behavior stability over time
- **Timeline replay accuracy** — event sequence correctness
- **Freshness hits / Out-of-date rate** — temporal governance

### Knowledge Graph Specific
- **Edge precision/recall** — entity/link extraction accuracy
- **Microstructure fidelity** — ERGM-based pattern preservation
- **Atomic fact support rate** — KG fact verification

### Efficiency
- **Latency / throughput** — memory system overhead
- **Cost@target** — cost to achieve accuracy threshold
- **Memory footprint** — storage requirements
