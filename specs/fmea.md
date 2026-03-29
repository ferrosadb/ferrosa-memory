# Failure Mode and Effects Analysis — ferrosa-memory-mcp

> Last updated: 2026-03-29
> Status: Updated — Sprint 5 failure modes added (F42–F52) for Datalog inference, warmth field, PageRank, and recursive explore components.

## Scoring Criteria

| Score | Severity (S) | Occurrence (O) | Detection (D) |
|-------|-------------|----------------|----------------|
| 1 | No effect | < 1 in 10,000 | Always detected before release |
| 2-3 | Minor degradation | 1 in 1,000 | High chance of detection in testing |
| 4-6 | Significant degradation | 1 in 100 | Moderate detection in testing |
| 7-8 | Major failure, data loss | 1 in 10 | Low detection, requires specific test |
| 9-10 | Critical, security breach | > 1 in 10 | Cannot detect without targeted audit |

**RPN = S x O x D** (max 1000). Action required for RPN >= 50.

## FMEA Table

### Component: cql_client (M10) — Fan-in: 8, Propagation: 86%

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F01 | CQL connection pool exhausted | All tool calls fail; complete service outage | 9 | 4 | 3 | **108** | Connection pool monitoring metric. Configurable pool size. Health check on `/metrics`. Graceful error to MCP client. |
| F02 | Prepared statement cache miss after schema change | Queries fail with schema mismatch error | 7 | 3 | 4 | **84** | Lazy statement re-preparation on schema error. Integration test with schema migration. |
| F03 | CQL query timeout on large partition | Slow or failed retrieval for single tool call | 5 | 5 | 3 | **75** | Per-query timeout config. Partition size monitoring. Warn when partition exceeds threshold. |
| F04 | Ferrosa node failure (single node in RF=3) | No effect if quorum maintained | 2 | 4 | 2 | 16 | Standard — RF=3 handles this. |
| F05 | CQL injection via unsanitized parameter | Data corruption or cross-tenant leak | 10 | 2 | 2 | **40** | Prepared statements only. Static analysis / clippy lint for string interpolation in CQL. |

### Component: tool_router (M4) — Strategy Selection

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F06 | Router selects wrong strategy | Suboptimal retrieval quality; slower response | 4 | 6 | 5 | **120** | Feedback loop records every routing decision. Nightly batch detects strategy accuracy regression. A/B test routing changes. |
| F07 | Router guidelines table empty/corrupt | Falls through to default (HNSW ANN) for all queries | 3 | 3 | 4 | 36 | Default strategy is safe fallback. Log warning when guidelines missing. |
| F08 | Router latency adds overhead to every call | All tool calls slower by routing overhead | 3 | 3 | 3 | 27 | Router is in-memory decision tree, not a DB call. Benchmark to confirm < 1ms. |

### Component: memo_tools (M5) — Memoization Cache

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F09 | Cache returns stale result (model version mismatch) | LLM agent uses outdated sub-call result | 7 | 3 | 5 | **105** | `model_version` in partition key ensures version-specific cache. Integration test: different model versions produce different cache keys. |
| F10 | SHA-256 collision (two different prompts, same hash) | Wrong cached result returned | 9 | 1 | 8 | **72** | Astronomically unlikely (2^-128). Document as accepted risk. Optional: store truncated prompt alongside hash for verification. |
| F11 | Thundering herd: concurrent misses write duplicate entries | Wasted computation, extra storage | 3 | 5 | 4 | **60** | Last-write-wins is acceptable (results deterministic for same hash). Track duplicate write rate in metrics. LWT support when Ferrosa adds it. |
| F12 | TTL sweep job fails; expired entries accumulate | Storage growth, possibly stale results served | 5 | 3 | 4 | **60** | Sweep job health metric. Alert on `memo_cache` row count growth rate. Manual sweep CQL available. |

### Component: fold_tools (M7) — Highest Fan-out (5 deps)

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F13 | `complete_fold` fails after DB write but before graph edge | Orphaned fold vertex; fold hierarchy broken | 7 | 3 | 6 | **126** | Two-phase: write CQL row first, then graph edge. Reconciliation job detects orphaned folds (fold with `parent_fold_id` but no `FOLDED_INTO` edge). |
| F14 | Compression fails on large trajectory | Raw trajectory stored uncompressed; storage cost increase | 4 | 3 | 3 | 36 | Fallback: store uncompressed, flag for retry. Compression is best-effort, not blocking. |
| F15 | `append_to_fold` on already-folded fold | Data appended to sealed fold, corrupting summary | 8 | 2 | 3 | **48** | Status check before append: reject if `status != 'active'`. Return clear error to caller. |
| F16 | S3 Glacier retrieval latency (hours) for `include_raw=true` | Caller blocks waiting for archived trajectory | 6 | 4 | 4 | **96** | Return error if fold is archived and `include_raw=true`. Offer async retrieval option. Document latency expectations per tier. |
| F17 | HNSW index returns irrelevant fold summaries | Agent gets wrong context, makes bad decisions | 6 | 4 | 6 | **144** | Return relevance scores alongside results. Let caller set minimum similarity threshold. Log retrieval quality in `feedback_outcomes`. |

### Component: entity_tools (M8) — Entity Store

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F18 | Phonetic match creates false merge (two distinct entities) | Entity data corrupted; wrong facts attributed | 7 | 4 | 6 | **168** | Phonetic match + embedding similarity threshold for merge decision. If embedding distance > threshold, create new entity even with phonetic match. |
| F19 | Memory poisoning: attacker crafts high-confidence entities | Poisoned entities dominate retrieval results | 9 | 3 | 7 | **189** | Confidence gating (reject < 0.7). Anomaly detection (>3σ retrieval frequency). Audit log. Rate limit entity upserts per session. |
| F20 | Entity graph grows unbounded per session | Cypher traversals slow down; DoS vector | 5 | 4 | 5 | **100** | Per-session entity count limit (configurable, default 1000). Alert on graph size metrics. |

### Component: temporal_events — Temporal Chaining

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F21 | Supersession chain broken (new fact doesn't mark old as invalid) | Agent retrieves contradictory facts | 7 | 3 | 5 | **105** | Atomic: read-old + invalidate-old + write-new in single CQL batch. Integration test for supersession integrity. |
| F22 | `valid_until` not set on superseded fact | Two "current" facts for same entity | 6 | 3 | 4 | **72** | Validation query: check for duplicate `valid_until IS NULL` per entity. Reconciliation in batch job. |

### Component: auth (M3) — Tenant Isolation

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F23 | Auth bypass: tool call reaches handler without `TenantContext` | Cross-tenant data access | 10 | 1 | 2 | 20 | Type system enforcement: tool handlers take `TenantContext` as required parameter (not `Option`). Won't compile without it. |
| F24 | HTTP Basic credentials sent over plaintext | Credential theft | 9 | 2 | 3 | **54** | Require TLS in HTTP mode. Reject non-TLS connections. Config validation on startup. |

### Component: embedding_client (M13)

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F25 | Ollama endpoint down | All embedding-dependent tools fail (memo store, fold retrieval, entity retrieval) | 7 | 4 | 2 | **56** | Health check on startup. Timeout (10s). Graceful degradation: tools that can function without embedding (plan_tools, feedback_tools) still work. Clear error message for tools that need embedding. |
| F26 | Embedding model changed; old embeddings incompatible | Cosine similarity meaningless across model versions | 8 | 2 | 6 | **96** | Store `embedding_model` alongside vectors. On model change, log warning. Provide migration tool to re-embed existing records. |

### Component: compression (M12)

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F27 | Compression produces unreadable output | Lost trajectory data | 8 | 2 | 3 | **48** | Verify round-trip: `decompress(compress(x)) == x` in unit tests. Checksum stored alongside compressed data. |
| F28 | Compression ratio worse than 1:1 (expansion) | Wasted storage, no benefit | 2 | 3 | 2 | 12 | Skip compression if ratio > 0.95. Return uncompressed. |

### Component: transport (M1)

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F29 | Malformed JSON-RPC crashes server | Full outage until restart | 8 | 3 | 3 | **72** | Catch deserialization errors at transport layer. Return JSON-RPC error response, don't panic. Fuzz test JSON-RPC parsing. |
| F30 | SSE connection leak (HTTP mode) | Memory/fd exhaustion over time | 6 | 3 | 5 | **90** | Connection timeout. Idle connection cleanup. Track open connection count in metrics. |

## RPN Summary (sorted descending)

| RPN | ID | Failure Mode | Component |
|-----|----|-------------|-----------|
| **189** | F19 | Memory poisoning via crafted entities | entity_tools |
| **168** | F18 | Phonetic false merge corrupts entities | entity_tools |
| **150** | F51 | Query decomposition produces irrelevant sub-queries | recursive_explore |
| **144** | F17 | HNSW returns irrelevant fold summaries | fold_tools |
| **140** | F45 | Derived cache returns stale facts after rule change | datalog |
| **126** | F13 | Orphaned fold vertex (partial write) | fold_tools |
| **120** | F06 | Router selects wrong strategy | tool_router |
| **120** | F46 | Warmth runaway feedback loop | warmth |
| **108** | F01 | CQL connection pool exhaustion | cql_client |
| **105** | F09 | Stale memo cache (model version) | memo_tools |
| **105** | F21 | Broken supersession chain | temporal_events |
| **100** | F20 | Unbounded entity graph growth | entity_tools |
| **96** | F16 | Glacier retrieval blocks caller | fold_tools |
| **96** | F26 | Embedding model change breaks similarity | embedding_client |
| **90** | F30 | SSE connection leak | transport |
| **90** | F35 | Smart ingest false positive dedup | cognitive tools |
| **84** | F02 | Prepared statement cache miss | cql_client |
| **80** | F33 | WebSocket visualizer connection leak | visualization |
| **80** | F36 | Dream consolidation stale co-occurrence | cognitive tools |
| **80** | F43 | Rule parsing accepts invalid syntax | datalog |
| **75** | F03 | CQL query timeout on large partition | cql_client |
| **75** | F48 | Decay pass deletes still-relevant entities | warmth |
| **72** | F10 | SHA-256 collision (accepted risk) | memo_tools |
| **72** | F22 | Duplicate current facts for entity | temporal_events |
| **72** | F29 | Malformed JSON-RPC crashes server | transport |
| **72** | F42 | Semi-naive evaluation diverges (no fixpoint) | datalog |
| **60** | F11 | Thundering herd duplicate writes | memo_tools |
| **60** | F12 | TTL sweep job failure | memo_tools |
| **60** | F44 | Confidence propagation produces values > 1.0 | datalog |
| **56** | F25 | Ollama endpoint down | embedding_client |
| **54** | F24 | HTTP credentials over plaintext | auth |
| **48** | F37 | Quota enforcement race condition | quota enforcement |
| **42** | F34 | Spreading activation infinite loop | cognitive tools |
| **36** | F47 | Warmth boost blocks retrieval response | warmth |
| **36** | F52 | Convergence detection fails (never converges) | recursive_explore |
| **27** | F49 | PPR computation fails during consolidation | pagerank |
| **16** | F50 | PPR scores all zero (disconnected graph) | pagerank |

## Test Cases for RPN >= 50

| Test ID | For FMEA | Test Description | Type |
|---------|----------|------------------|------|
| TC01 | F19 | Attempt entity upsert with confidence < 0.7; verify rejection | Unit |
| TC02 | F19 | Upsert 100+ entities in rapid succession; verify rate limit triggers | Integration |
| TC03 | F19 | Insert entity, retrieve it 50 times, verify anomaly flag at >3σ | Integration |
| TC04 | F18 | Upsert "John Smith" then "Jon Smyth"; verify phonetic match + embedding distance check prevents false merge | Integration |
| TC05 | F18 | Upsert "Apple (company)" then "Apple (fruit)"; verify they remain separate despite phonetic match | Integration |
| TC06 | F17 | Store 10 folds with known embeddings; query with a specific embedding; verify top-k relevance scores exceed minimum threshold | Integration |
| TC07 | F13 | Kill process after CQL write, before graph edge; restart; verify reconciliation detects orphan | Integration |
| TC08 | F06 | Execute 100 queries with known-optimal strategies; verify router selects correctly for > 80% | Integration |
| TC09 | F01 | Open max_connections CQL sessions; attempt one more; verify graceful error, not crash | Integration |
| TC10 | F09 | Store memo with model_v1; query with model_v2; verify cache miss | Unit |
| TC11 | F21 | Write fact A for entity; write fact B superseding A; verify A.valid_until is set and B.supersedes_id = A | Integration |
| TC12 | F20 | Attempt to upsert entity #1001 when limit is 1000; verify rejection | Unit |
| TC13 | F16 | Query `retrieve_fold_context(include_raw=true)` on archived fold; verify error with tier info | Unit |
| TC14 | F26 | Store embedding with model "v1"; change config to model "v2"; verify warning on retrieval | Integration |
| TC15 | F30 | Open 100 SSE connections, let them idle; verify cleanup after timeout | Integration |
| TC16 | F02 | Alter table schema (add column); execute query; verify re-preparation succeeds | Integration |
| TC17 | F03 | Insert 100K rows in one partition; query with timeout; verify timeout error (not hang) | Integration |
| TC18 | F29 | Send malformed JSON-RPC; verify error response, server stays alive | Unit + Fuzz |
| TC19 | F11 | Send 10 concurrent `store_memo_result` for same hash; verify exactly 1 logical entry | Integration |
| TC20 | F12 | Set TTL to 1 second; insert memo; wait; run sweep; verify deletion | Integration |
| TC21 | F25 | Stop Ollama; call `store_memo_result`; verify timeout error within 10s, other tools still work | Integration |
| TC22 | F24 | Start HTTP server; attempt connection without TLS; verify rejection | Integration |
| TC23 | F22 | Write two facts for same entity without supersession; run validation; verify detection | Integration |
| TC24 | F27 | Compress then decompress 100 sample trajectories; verify exact round-trip equality | Unit (property-based) |

### Component: cql_storage — Vector Column Support (RESOLVED, 2026-03-23)

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F31 | cdrs-tokio v9 lacks `vector<float,768>` type support | All embeddings stored as NULL in CQL. ANN queries (`ORDER BY embedding ANN OF ?`) non-functional. `fold_search` falls back to LIMIT-based retrieval. `entity_search_ann` returns empty. Semantic search completely broken. | 9 | 10 | 2 | ~~180~~ **18** | **RESOLVED** (commit e5c9a27). Vector reads work via cdrs-tokio fork. Detection lowered to 2 (always caught in tests). S=9, O=1, D=2. |
| F32 | Graph edge creation not implemented in write paths | `FOLDED_INTO`, `SUPERSEDES`, `MENTIONED_IN`, `CO_OCCURS_WITH` edges never created. Graph traversals return empty. Fold hierarchy, entity relationships, and temporal chains not queryable via Cypher. | 7 | 10 | 2 | ~~140~~ **14** | **RESOLVED** (commit ca207aa). Graph edges write via CQL. Detection lowered to 2 (always caught in tests). S=7, O=1, D=2. |

### Updated RPN entries (append to summary)

| RPN | ID | Failure Mode | Component | Status |
|-----|----|-------------|-----------|--------|
| ~~180~~ **18** | F31 | Vector column type not supported by cdrs-tokio | cql_storage | Resolved (e5c9a27) |
| ~~140~~ **14** | F32 | Graph edge creation not implemented | fold/entity/temporal tools | Resolved (ca207aa) |

### Component: Visualization — WebSocket Dashboard

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F33 | WebSocket visualizer connection leak | If viz WebSocket connections aren't cleaned up on disconnect, memory grows unboundedly. Server OOM over time. | 5 | 4 | 4 | **80** | Idle timeout and connection limit (already implemented in http.rs). Monitor open connection count in metrics. |

### Component: Cognitive Tools — Spreading Activation, Smart Ingest, Dream Consolidation

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F34 | Spreading activation infinite loop | If graph has cycles, BFS without visited tracking could loop forever. Tool call hangs, blocking agent. | 7 | 2 | 3 | **42** | Visited set in spreading.rs prevents revisiting. Bounded iteration with max depth. |
| F35 | Smart ingest false positive dedup | Prediction error gate could incorrectly match unrelated content, skipping important ingestion. Agent loses data silently. | 6 | 3 | 5 | **90** | Configurable similarity threshold. "Skipped" action returned to caller so agent can override. |
| F36 | Dream consolidation stale co-occurrence | Co-occurrence patterns from old sessions may produce irrelevant consolidation suggestions. Agent acts on outdated patterns. | 4 | 4 | 5 | **80** | Session-scoped tracking. Consolidation is advisory only — agent decides whether to act. |

### Component: Quota Enforcement

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F37 | Quota enforcement race condition | Concurrent writes could exceed quota if count check and write aren't atomic. Tenant exceeds allocated storage. | 4 | 3 | 4 | **48** | Slight over-quota acceptable. Counts are eventually consistent. Hard limit enforced at next read. |

### Component: datalog (M35) — Inference Engine

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F42 | Semi-naive evaluation diverges (no fixpoint) | Infinite loop, CPU exhaustion. Tool call hangs indefinitely, blocking agent. | 8 | 3 | 3 | **72** | `max_iterations=100` hard cap in evaluator. Log iteration count on every evaluation. Alert if >50 iterations on any single evaluation. Integration test with cyclic rules verifies cap triggers. |
| F43 | Rule parsing accepts invalid syntax | Garbage rules stored in registry, causing evaluation errors or panics on next inference. | 5 | 4 | 4 | **80** | `parse_rule()` validates before storage — reject rules that don't produce valid AST. Test all built-in rules parse correctly. Fuzz test parser with random input. |
| F44 | Confidence propagation produces values > 1.0 | Derived facts with inflated confidence bias retrieval results, undermining trust calibration. | 4 | 3 | 5 | **60** | Clamp derived confidence to `[0.0, 1.0]` after every computation. Use `min(parent_confidences) * rule_weight` per spec Section 11. Unit test with chain of high-confidence parents verifies clamping. |
| F45 | Derived cache returns stale facts after rule change | Incorrect inference results served from cache, contradicting current rule set. | 7 | 4 | 5 | **140** | Invalidate cache for affected predicate families on any rule change via `derived_cache_clear()`. Include `rule_version` in cache key. Integration test: change rule, verify cache miss on next query. |

### Component: warmth (M36) — Persistent Spreading Activation

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F46 | Warmth runaway feedback loop | Popular entities dominate all searches via self-reinforcing warmth. Cold start problem for new entities. Retrieval diversity collapses. | 6 | 4 | 5 | **120** | Max warmth cap 10.0 in `warmth_boost()`. Ebbinghaus decay in consolidation normalizes distribution. Monitor warmth distribution in `get_stats` — flag if >10% entities at cap. |
| F47 | Warmth boost blocks retrieval response | Added latency on every retrieval call from synchronous warmth updates. | 4 | 3 | 3 | **36** | Fire-and-forget via `tokio::spawn`. Boost failure does not affect retrieval result. Log boost errors separately. |
| F48 | Decay pass deletes still-relevant entities | Aggressive Ebbinghaus decay removes important memories that haven't been accessed recently but remain valuable. | 5 | 3 | 5 | **75** | Identity zone has 0.1x decay multiplier (nearly permanent). Access resets decay timer. Conservative prune threshold (0.01). Integration test: verify Identity-zone entity survives aggressive decay pass. |

### Component: pagerank (M37) — Personalized PageRank

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F49 | PPR computation fails during consolidation | Stale PageRank scores remain in warmth table. 5-signal fusion uses outdated graph centrality signal. | 3 | 3 | 3 | **27** | Non-fatal — fusion degrades gracefully to 4 signals. Log warning on PPR failure. Consolidation continues with remaining phases. |
| F50 | PPR scores all zero (disconnected graph) | PageRank signal adds no value to 5-signal fusion. All entities tied on this signal. | 2 | 4 | 2 | **16** | Expected behavior for sparse or disconnected graphs. Document as normal. Fusion weights auto-compensate via RRF (zero-contribution signal has no effect on ranking). |

### Component: recursive_explore (M38) — Recursive Query

| ID | Failure Mode | Effect | S | O | D | RPN | Recommended Action |
|----|-------------|--------|---|---|---|-----|-------------------|
| F51 | Query decomposition produces irrelevant sub-queries | Noise results mixed with signal. Retrieval quality degrades compared to single-pass `hybrid_search`. | 5 | 5 | 6 | **150** | Original query always included as first sub-query (guarantees baseline quality). Cap at 5 sub-queries. Score synthesis favors multi-pass consensus — results appearing in multiple passes ranked higher. |
| F52 | Convergence detection fails (never converges) | Max passes exhausted on every exploration. Consistently high latency for `recursive_explore` calls. | 4 | 3 | 3 | **36** | Hard cap at `max_passes` (default 3). Total timeout for exploration. Convergence threshold configurable. Log pass count and convergence status for monitoring. |

### New Test Cases

| Test ID | For FMEA | Test Description | Type |
|---------|----------|------------------|------|
| TC25 | F31 | Store entity with embedding via CQL; read back; verify embedding is not NULL | Integration |
| TC26 | F31 | Execute `ORDER BY entity_embedding ANN OF ?` query; verify results ordered by similarity | Integration |
| TC27 | F32 | Complete a fold with parent_fold_id; verify `FOLDED_INTO` edge exists in graph | Integration |
| TC28 | F32 | Upsert two co-occurring entities; verify `CO_OCCURS_WITH` edge exists | Integration |
| TC29 | F33 | Open 50 WebSocket connections, disconnect half abruptly; verify server reclaims connections after idle timeout | Integration |
| TC30 | F34 | Create cyclic graph (A->B->C->A); run `spread_activation` from A; verify termination and no duplicate visits | Integration |
| TC31 | F35 | Ingest two semantically distinct items with similar surface text; verify both are stored (no false dedup) | Integration |
| TC32 | F35 | Ingest exact duplicate; verify "Skipped" action returned to caller | Unit |
| TC33 | F36 | Run `run_consolidation` with co-occurrences from sessions > 30 days old; verify staleness flag or empty result | Integration |
| TC34 | F37 | Send 10 concurrent writes when quota is at limit-1; verify at most slight over-quota, not unbounded growth | Integration |
| TC35 | F42 | Create cyclic Datalog rules (A->B->C->A transitive closure); evaluate; verify fixpoint reached within `max_iterations` and evaluation terminates | Unit |
| TC36 | F42 | Set `max_iterations=5`; provide rules that need >5 iterations; verify early bail with warning | Unit |
| TC37 | F43 | Attempt to store malformed rule body via `manage_rules`; verify rejection with parse error | Unit |
| TC38 | F43 | Parse all built-in rule families (taxonomy, part-of, co-occurrence, reachability); verify all produce valid AST | Unit |
| TC39 | F44 | Chain 5 rules with confidence 0.95 each, weight 1.0; verify derived confidence clamped to <= 1.0 | Unit |
| TC40 | F45 | Store derived facts in cache; change rule via `manage_rules`; query same predicate; verify cache miss and re-derivation | Integration |
| TC41 | F46 | Boost same entity 100 times; verify warmth capped at 10.0 | Unit |
| TC42 | F46 | Run decay pass on session with 50 entities; verify warmth distribution normalized (no entity >10.0) | Unit |
| TC43 | F48 | Create entity in Identity decay zone; run aggressive decay; verify entity survives with warmth above prune threshold | Unit |
| TC44 | F48 | Create entity in Operational zone with warmth 0.02; run decay; verify pruned below 0.01 threshold | Unit |
| TC45 | F51 | Run `recursive_explore` with a single-entity query; verify original query appears in sub-queries and results contain the entity | Integration |
| TC46 | F51 | Run `recursive_explore` with nonsensical query; verify graceful degradation (returns some results via original query, not empty) | Integration |
| TC47 | F52 | Set `max_passes=2`; run exploration on large graph that won't converge; verify exactly 2 passes and `converged=false` | Unit |
