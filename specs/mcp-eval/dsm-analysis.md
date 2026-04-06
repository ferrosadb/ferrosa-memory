# MCP Eval Framework — DSM Analysis

## Tool Dependency Clusters (9 clusters, 50 tools)

### Cluster 1: Core Data Operations (CRUD)
- `upsert_entity`, `batch_ingest`, `start_fold`, `append_to_fold`, `complete_fold`, `write_temporal_fact`, `create_edge`, `batch_create_edges`
- Backend: CQL Storage only. No graph or embedding deps.

### Cluster 2: Embedding/Vector Search (Ollama + Storage)
- `hybrid_search` (5-signal RRF: phonetic + ANN + fold + warmth + PageRank)
- `recursive_explore` (decompose → multi-pass hybrid_search → Datalog)
- `retrieve_fold_context` (ANN on fold summaries)
- `retrieve_entities` (phonetic + ANN)
- Backend: CQL Storage + Ollama HTTP (optional) + HNSW vector indices

### Cluster 3: Graph Traversal & Knowledge Synthesis
- `spread_activation` (Collins & Loftus, iterative BFS)
- `find_memory_chain` (BFS shortest path)
- `explore_connections` (graph neighborhood)
- `query_derived` (Datalog inference with caching)
- `manage_rules` (Datalog rule CRUD)
- `promote_predicate` (materialize derived predicate)
- Backend: CQL Storage + Datalog engine (in-memory)

### Cluster 4: Memory Lifecycle & Smart Ingest
- `smart_ingest` (NER + prediction error gating: CREATE/UPDATE/SUPERSEDE/SKIP)
- `run_consolidation` (CO_OCCURS discovery + Datalog batch + PageRank + decay)
- Backend: CQL + Ollama NER + Datalog + PageRank + Warmth (highest coupling)

### Cluster 5: Memory State & Feedback
- `promote_memory`, `demote_memory`, `importance_score`, `record_outcome`
- Backend: CQL Storage only

### Cluster 6: Plan/Fold Tree Management
- `write_plan_node`, `get_plan_context`, `update_plan_node`
- Typical sequence: write_plan_node → start_fold → append_to_fold* → complete_fold → update_plan_node
- Backend: CQL Storage only

### Cluster 7: Memoization Cache
- `check_memo_cache`, `store_memo_result`
- Backend: CQL Storage (content_hash index)

### Cluster 8: Intentions (Prospective Memory)
- `set_intention`, `check_intentions`, `complete_intention`, `snooze_intention`, `list_intentions`
- Backend: CQL Storage (intention registry with trigger matching)

### Cluster 9: Audit & Telemetry
- `get_stats` (aggregates from entity/fold/edge/memo/temporal counts)
- Implicit audit in write operations
- Backend: CQL Storage

## Backend Coupling Matrix

| Backend | Tools | Failure Impact |
|---------|-------|----------------|
| **CQL Storage** | ALL 50 | System down — single point of failure behind Storage trait |
| **Ollama HTTP** | upsert_entity, smart_ingest, hybrid_search, recursive_explore | Graceful: fallback to phonetic search |
| **HNSW Vector** | hybrid_search, retrieve_entities, retrieve_fold_context, recursive_explore | ANN disabled, phonetic-only |
| **Datalog Engine** | hybrid_search, recursive_explore, query_derived, promote_predicate, run_consolidation | No derived facts, base facts only |
| **Graph HTTP** | (Currently unused — all graph via CQL edge tables) | N/A |
| **Warmth/PageRank** | hybrid_search, recursive_explore, run_consolidation | Ranking degraded, no personalization |

## DIKW Tool Mapping

### Data Layer (Raw Storage)
`upsert_entity`, `batch_ingest`, `write_temporal_fact`, `create_edge`, `batch_create_edges`, `start_fold`, `append_to_fold`, `complete_fold`
- Validation only (confidence gating, dedup). No inference.

### Information Layer (Contextualization)
`retrieve_entities`, `retrieve_fold_context`, `get_temporal_chain`, `explore_connections`, `find_memory_chain`, `get_stats`
- Retrieval without transformation. Phonetic/ANN indexing.

### Knowledge Layer (Synthesis & Inference)
`hybrid_search`, `recursive_explore`, `spread_activation`, `query_derived`, `run_consolidation`, `find_duplicates`
- Multi-source fusion, Datalog reasoning, graph clustering, PageRank.

### Wisdom Layer (Decision Support)
`smart_ingest`, `check_intentions`, `predict_needed`, `importance_score`, `promote_memory`, `demote_memory`, `promote_predicate`
- Apply domain knowledge (memory theory, workload patterns) to support decisions.

## High-Coupling Tools (eval priority)

1. **`smart_ingest`** — depends on NER + Storage + Importance + Ollama. Most complex tool.
2. **`run_consolidation`** — depends on Dream + Datalog + PageRank + Warmth + Promotion. Most coupled.
3. **`hybrid_search`** — orchestrates 5 signals. Any signal failure degrades ranking.
4. **`recursive_explore`** — calls hybrid_search internally + Datalog evaluation.

## Eval Framework Design Implications

1. **Backend isolation testing**: test CQL-only tools independently, then layer in optional backends
2. **Coupling-ordered testing**: low-coupling tools first (create_edge, write_plan_node), high-coupling last (smart_ingest, run_consolidation)
3. **Workflow sequence testing**: plan→fold→entity→consolidation→search lifecycle
4. **Graceful degradation testing**: Ollama down, Datalog timeout, warmth missing
5. **DIKW progression testing**: verify data tools feed information tools which feed knowledge tools which feed wisdom tools
