# fmem Evaluation Blueprint — 6 Gaps

> Token-efficient plan. Each section = objective, files, test, commit.

---

## 1. L4 Task-Level Evaluation Scenarios

**Objective:** Measure if fmem *helps* agents complete multi-session interdependent tasks (MemoryArena insight). Build on existing `ferrosa-memory-eval` crate.

**Files:**
- `crates/ferrosa-memory-eval/src/task_agent.rs` — LLM agent loop using fmem via MCP
- `crates/ferrosa-memory-eval/src/task_grader.rs` — task success scoring + ablation
- `crates/ferrosa-memory-eval/scenarios/level4/*.toml` — 5 scenario definitions

**Task 1.1: Task agent loop**
```rust
// task_agent.rs — minimal agent that uses fmem tools via stdio MCP
pub struct TaskAgent {
    llm_client: LlmClient,       // OpenAI-compatible
    mcp_transport: McpTransport, // spawns ferrosa-memory-mcp binary
    session_id: Uuid,
}

impl TaskAgent {
    pub async fn run_session(&mut self, prompt: &str, tools_allowed: &[&str]) -> AgentOutput {
        // 1. Pre-load: query fmem for relevant context (hybrid_search, predict_needed)
        // 2. LLM call with system prompt + fmem context + user prompt
        // 3. Parse tool calls from LLM response
        // 4. Execute via MCP, store results back in fmem (smart_ingest, write_temporal_fact)
        // 5. Repeat until task complete or max_steps
    }
}
```
- Test: `cargo test --package ferrosa-memory-eval task_agent::tests::single_session` — runs 3-step dummy task

**Task 1.2: Task grader**
```rust
// task_grader.rs
pub struct TaskGrader;

impl TaskGrader {
    pub fn score_success(&self, expected: &[&str], actual: &AgentOutput) -> f64 {
        // Exact match on root-cause identification list
        let found: HashSet<_> = actual.findings.iter().collect();
        let needed: HashSet<_> = expected.iter().collect();
        found.intersection(&needed).count() as f64 / needed.len() as f64
    }

    pub fn ablation_baseline(&self, scenario: &Path, config: &EvalConfig) -> AblationResult {
        // Run 4 times: full fmem → no warmth → no confidence → no datalog → no consolidation
        // Return (mean_sr, std_sr) per condition
    }
}
```
- Test: `task_grader::tests::ablation_mechanics` — verify 4 conditions produce different scores on a trivial task

**Task 1.3: 5 L4 scenarios (TOML)**
```toml
# scenarios/level4/debugging_cross_session.toml
[scenario]
id = "l4-debug-cross-session"
name = "Multi-session bug investigation"
level = 4
sessions = 3
tags = ["task-level", "episodic"]

[[sessions]]
name = "s1"
prompt = "User reports login failures. Investigate and store findings."
expected_findings = ["OAuth token expiry", "redis pool exhaustion"]

[[sessions]]
name = "s2"
depends_on = ["s1"]
prompt = "Login still failing. Check memory for prior findings and continue."
expected_findings = ["OAuth token expiry", "redis pool exhaustion", "session store timeout"]

[[sessions]]
name = "s3"]
depends_on = ["s1", "s2"]
prompt = "User says 'this started after the deploy on Tuesday.' Cross-reference with your timeline."
expected_findings = ["OAuth token expiry", "redis pool exhaustion", "session store timeout", "deploy correlation"]

[grading]
type = "task_success"
subtasks = ["Identify OAuth token expiry", "Identify redis pool exhaustion", "Identify session store timeout", "Correlate with deploy timeline"]
```
Other 4: `requirements_evolution.toml`, `cross_project_transfer.toml`, `long_horizon_planning.toml`, `contradiction_resolution.toml`

- Test: `runner::tests::run_l4_debugging` — end-to-end with mock LLM that emits deterministic tool calls

**Task 1.4: Mock LLM for deterministic testing**
```rust
pub struct MockLlm {
    scripted_responses: Vec<String>, // pre-canned tool call JSON
}
// Returns tool calls in sequence; no API cost, no flakiness
```

---

## 2. Graph Quality Metrics

**Objective:** Validate that the KG is semantically correct, not just densely connected. Ground-truth benchmark approach.

**Files:**
- `crates/ferrosa-memory-eval/src/metrics/kg_metrics.rs`
- `crates/ferrosa-memory-eval/src/metrics/graph_benchmark.rs`
- `crates/ferrosa-memory-eval/benches/kg_quality.rs` (criterion)

**Task 2.1: Edge precision / recall against ground truth**
```rust
// kg_metrics.rs
pub struct KgGroundTruth {
    pub entities: Vec<(String, String)>,        // (name, type)
    pub edges: Vec<(String, String, String)>,   // (src, dst, type)
}

pub fn edge_precision_recall(
    storage: &dyn Storage,
    ctx: &TenantContext,
    gt: &KgGroundTruth,
) -> (f64, f64) {
    let found: HashSet<_> = load_all_edges(storage, ctx).await.into_iter().collect();
    let expected: HashSet<_> = gt.edges.iter().collect();
    let tp = found.intersection(&expected).count() as f64;
    let precision = tp / found.len() as f64;
    let recall = tp / expected.len() as f64;
    (precision, recall)
}
```
- Test: `kg_metrics::tests::perfect_graph` — inject exact GT, expect precision=1.0 recall=1.0

**Task 2.2: Microstructure fidelity (ERGM-lite)**
```rust
pub fn microstructure_fidelity(
    storage: &dyn Storage,
    ctx: &TenantContext,
    gt: &KgGroundTruth,
) -> MicrostructureScore {
    // Count triangles, 2-paths, stars in both graphs
    let gt_tri = count_triangles(gt);
    let found_tri = count_triangles_from_storage(storage, ctx).await;
    let triangle_ratio = (found_tri as f64) / (gt_tri as f64).max(1.0);
    // Same for 2-paths, stars
    MicrostructureScore { triangle_ratio, two_path_ratio, star_ratio }
}
```
- Test: `tests::triangle_bias` — GT has 3 triangles, storage has 5 (2 false) → ratio = 1.0 but precision drops

**Task 2.3: Deduplication benchmark**
```rust
pub async fn dedup_benchmark(
    storage: &dyn Storage,
    ctx: &TenantContext,
) -> DedupScore {
    // 1. Ingest 50 entities, 10 are intentional duplicates
    // 2. Run find_duplicates
    // 3. Measure: true_positives, false_positives, false_negatives
    let tp = found_dup_pairs.iter().filter(|p| intentional_dups.contains(p)).count();
    let fp = found_dup_pairs.iter().filter(|p| !intentional_dups.contains(p)).count();
    let fn_ = intentional_dups.iter().filter(|p| !found_dup_pairs.contains(p)).count();
    DedupScore { precision: tp/(tp+fp), recall: tp/(tp+fn_) }
}
```
- Test: `tests::dedup_half_duplicates` — 10 dup pairs in 50 entities → expect recall >= 0.7

**Task 2.4: Contradiction detection benchmark**
```rust
pub async fn contradiction_benchmark(storage: &dyn Storage) -> ContradictionScore {
    // Inject 20 fact pairs: 10 contradictory, 10 compatible
    // Call contradiction tool on each pair
    // Score: accuracy = (TP + TN) / 20
}
```
- Note: Current `contradiction.rs` is keyword-based. This benchmark will expose its ceiling. Upgrade to semantic later.

---

## 3. Cross-Domain Personalization

**Objective:** Test whether fmem transfers knowledge across projects/domains and infers latent preferences (MemoryCD insight).

**Files:**
- `crates/ferrosa-memory-eval/scenarios/level4/cross_domain_transfer.toml`
- `crates/ferrosa-memory-eval/src/metrics/personalization.rs`

**Task 3.1: Cross-domain scenario**
```toml
# scenarios/level4/cross_domain_transfer.toml
[scenario]
id = "l4-cross-domain"
name = "Cross-project pattern transfer"
sessions = 4

# Domain A: Auth service (3 sessions of debugging)
[[sessions]]
name = "auth-s1"
domain = "auth"
prompt = "Debug OAuth token refresh race condition."
expected_findings = ["token expiry", "redis lock contention"]

[[sessions]]
name = "auth-s2"
domain = "auth"
depends_on = ["auth-s1"]
prompt = "User reports 401s after 5 minutes. What did we find last time?"
expected_findings = ["token expiry", "redis lock contention"]

# Domain B: Payment service (new domain, same underlying pattern)
[[sessions]]
name = "payment-s1"
domain = "payment"
depends_on = ["auth-s1", "auth-s2"]  # must recall prior domain
prompt = "Payment webhook timing out after 5 minutes. Any patterns from past projects that might apply?"
expected_findings = ["timeout pattern", "token expiry analogy", "redis lock contention analogy"]

[grading]
type = "transfer_score"
// Score: did agent retrieve auth-domain facts and map them to payment-domain problem?
```

**Task 3.2: Latent preference inference metric**
```rust
// personalization.rs
pub async fn latent_preference_score(
    storage: &dyn Storage,
    ctx: &TenantContext,
) -> f64 {
    // 1. Ingest 20 "user preference" facts across varied phrasing
    //    (e.g., "I like concise output", "prefer short answers", "hate verbosity")
    // 2. Query for "user preference on output style"
    // 3. Check if retrieval returns the canonical preference
    // 4. Score: Jaccard of retrieved vs. annotated preference cluster
}
```
- Test: `tests::preference_cluster` — 3 phrasings of same preference → all should retrieve under canonical query

**Task 3.3: Style drift tracking**
```rust
pub async fn style_drift_score(
    storage: &dyn Storage,
    ctx: &TenantContext,
    old_session: Uuid,
    new_session: Uuid,
) -> f64 {
    // Compare entity types and edge patterns between two project domains
    // Score: fraction of edge types shared across domains (higher = better transfer)
}
```

---

## 4. Episodic Memory / Record Outcome Loop

**Objective:** Close the feedback loop — make `record_outcome` actually affect future retrieval behavior.

**Files:**
- `crates/ferrosa-memory-core/src/warmth.rs` — add outcome-based warmth modulation
- `crates/ferrosa-memory-core/src/retrieval.rs` — outcome-aware ranking
- `crates/ferrosa-memory-mcp/src/main.rs` — wire `record_outcome` to warmth

**Task 4.1: Outcome → warmth modulation**
```rust
// warmth.rs — add outcome-based boost
pub async fn apply_outcome_boost(
    storage: &dyn Storage,
    ctx: &TenantContext,
    entity_id: Uuid,
    outcome: &RetrievalOutcome,
) -> anyhow::Result<()> {
    match outcome {
        RetrievalOutcome { succeeded: true, latency_ms, .. } => {
            // Success + fast = warmth boost
            let boost = if *latency_ms < 50 { 0.3 } else { 0.15 };
            warmth_boost(storage, ctx, entity_id, boost).await
        }
        RetrievalOutcome { succeeded: false, .. } => {
            // Failed retrieval = warmth penalty (it wasn't useful)
            warmth_penalty(storage, ctx, entity_id, 0.2).await
        }
    }
}
```
- Test: `warmth::tests::outcome_boost_increases_warmth` — record success → query warmth → assert higher

**Task 4.2: Outcome-aware retrieval ranking**
```rust
// retrieval.rs — in hybrid_search and retrieve_entities
pub fn score_entity(entity: &Entity, outcome_history: &[(bool, f64)]) -> f64 {
    let base_score = entity.embedding_similarity * 0.4 + entity.phonetic_score * 0.3;
    let outcome_bonus = outcome_history.iter()
        .filter(|(s, _)| *s)
        .count() as f64 * 0.05;  // +0.05 per past success
    let outcome_penalty = outcome_history.iter()
        .filter(|(s, _)| !*s)
        .count() as f64 * 0.03;  // -0.03 per past failure
    base_score + outcome_bonus - outcome_penalty
}
```
- Test: `retrieval::tests::successful_entity_ranked_higher` — 2 identical entities, one with success outcomes → it ranks first

**Task 4.3: Per-tool learning**
```rust
// In MCP dispatch, after every tool call:
if tool_name == "hybrid_search" {
    // Auto-record outcome based on whether downstream tool used results
    let outcome = RetrievalOutcome {
        query_id: generate_query_id(),
        succeeded: !response.is_empty(),
        latency_ms,
        token_cost: 0,
    };
    storage.record_outcome(ctx, outcome).await?;
}
```
- Test: `dispatch::tests::auto_record_on_search` — call hybrid_search → check `record_outcome` table has entry

**Task 4.4: Session replay scaffold**
```rust
// New tool: `replay_session` (or workbench endpoint)
pub async fn replay_session(
    storage: &dyn Storage,
    ctx: &TenantContext,
    session_id: Uuid,
) -> Vec<ReplayEvent> {
    // Return chronological list of:
    // - tool calls made
    // - entities created/updated
    // - temporal facts written
    // - outcomes recorded
    // This is the "event replay" primitive for episodic memory
}
```
- Test: `replay::tests::roundtrip` — run 3 tools in session → replay → assert 3 events in order

---

## 5. Progressive Disclosure Implementation

**Objective:** Reduce token burn by exposing ~15 primary tools, suggesting ~20 secondary, hiding ~15 internal.

**Files:**
- `crates/ferrosa-memory-core/src/progressive_disclosure.rs` — tier logic
- `crates/ferrosa-memory-mcp/src/main.rs` — filter `tools/list` response

**Task 5.1: Tier definitions**
```rust
// progressive_disclosure.rs
pub const TIER_1_ALWAYS: &[&str] = &[
    "smart_ingest", "hybrid_search", "create_edge", "batch_create_edges",
    "explore_connections", "check_intentions", "set_intention", "complete_intention",
    "get_stats", "write_temporal_fact", "get_temporal_chain",
    "start_fold", "append_to_fold", "complete_fold", "write_plan_node",
];

pub const TIER_2_SUGGESTED: &[&str] = &[
    "recursive_explore", "spread_activation", "find_memory_chain", "query_derived",
    "run_consolidation", "manage_rules", "find_duplicates", "importance_score",
    "predict_needed", "batch_ingest", "retrieve_entities", "retrieve_fold_context",
    // ... (20 total)
];

pub const TIER_3_INTERNAL: &[&str] = &[
    "upsert_entity", "check_memo_cache", "store_memo_result", "promote_predicate",
    "run_consolidation", // promoted to T2 via hint
];
```

**Task 5.2: Hint injection in responses**
```rust
// In each Tier-1 tool handler, append hints when conditions met:
if response.results.len() < 3 {
    response.hints.push("Try `recursive_explore` for deeper graph traversal.");
}
if response.edges.is_empty() && response.entities.len() == 1 {
    response.hints.push("Try `spread_activation` to find related concepts.");
}
```
- Test: `tests::hint_on_empty_results` — hybrid_search with 1 result → response contains `recursive_explore` hint

**Task 5.3: `tools/list` filtering**
```rust
pub fn filter_tools_for_context(
    all_tools: Vec<ToolDefinition>,
    session_context: &SessionContext,
) -> Vec<ToolDefinition> {
    let mut visible = TIER_1_ALWAYS.iter()
        .filter_map(|name| all_tools.iter().find(|t| t.name == *name))
        .cloned()
        .collect::<Vec<_>>();
    
    // Add Tier-2 if hints were triggered in this session
    for hinted in &session_context.triggered_hints {
        if let Some(tool) = all_tools.iter().find(|t| t.name == *hinted) {
            visible.push(tool.clone());
        }
    }
    visible
}
```
- Test: `tests::tier_2_appears_after_hint` — initial tools/list has 15, after hint trigger has 16+

---

## 6. Warmth/Decay Validation

**Objective:** Empirically validate the Ebbinghaus curve and threshold-based forgetting against actual relevance patterns.

**Files:**
- `crates/ferrosa-memory-eval/src/metrics/temporal_metrics.rs`
- `crates/ferrosa-memory-core/src/warmth.rs` — add decay parameter configuration

**Task 6.1: Decay curve accuracy**
```rust
// temporal_metrics.rs
pub async fn decay_curve_accuracy(
    storage: &dyn Storage,
    ctx: &TenantContext,
    entity_id: Uuid,
) -> DecayAccuracy {
    let expected = vec![
        (Duration::from_secs(0),     1.0),   // t=0
        (Duration::from_secs(3600),  0.55),  // 1h — Ebbinghaus
        (Duration::from_secs(86400), 0.21),  // 24h
        (Duration::from_secs(604800), 0.05), // 7d
    ];
    let mut measured = vec![];
    for (delay, expected_warmth) in &expected {
        tokio::time::sleep(*delay).await; // in eval, use fake clock
        let actual = storage.warmth_get(ctx, entity_id).await.unwrap();
        measured.push((delay, actual, expected_warmth));
    }
    let mse = measured.iter().map(|(_, a, e)| (a - e).powi(2)).sum::<f64>() / measured.len() as f64;
    DecayAccuracy { mse }
}
```
- Test: `tests::ebbinghaus_match` — mock clock, advance 1h/24h/7d, assert warmth matches curve within 0.05

**Task 6.2: Content-type-specific decay rates**
```rust
// warmth.rs — extend decay parameters
pub struct DecayProfile {
    pub half_life_hours: f64,
    pub category: &'static str,
}

pub const DECAY_PROFILES: &[DecayProfile] = &[
    DecayProfile { half_life_hours: 24.0,  category: "bug" },      // bugs decay fast
    DecayProfile { half_life_hours: 168.0, category: "architecture" }, // arch decays slow
    DecayProfile { half_life_hours: 72.0,  category: "decision" },  // decisions medium
    DecayProfile { half_life_hours: 12.0,  category: "transient" }, // transient very fast
];
```
- Test: `tests::bug_faster_than_arch` — same initial warmth, 24h later bug lower than arch

**Task 6.3: Threshold-based forgetting validation**
```rust
// temporal_metrics.rs
pub async fn forgetting_validation(
    storage: &dyn Storage,
    ctx: &TenantContext,
    threshold: f64,
) -> ForgettingScore {
    // 1. Seed 100 entities with varied ages (0h to 30d)
    // 2. Run prune_forgotten(threshold)
    // 3. Check: entities below threshold are gone, entities above are retained
    let pre = count_entities(storage, ctx).await;
    let pruned = storage.prune_forgotten(ctx, threshold).await.unwrap();
    let post = count_entities(storage, ctx).await;
    let expected_pruned = pre - (entities_above_threshold as usize);
    ForgettingScore {
        precision: if pruned == expected_pruned { 1.0 } else { 0.0 },
        retention_rate: post as f64 / pre as f64,
    }
}
```
- Test: `tests::perfect_forget_at_threshold` — 50 entities at 0.9, 50 at 0.1, threshold=0.5 → exactly 50 remain

**Task 6.4: A/B decay vs. no-decay on task success**
```rust
// In task_grader.rs ablation:
pub async fn ablation_decay(&self, scenario: &Path) -> (f64, f64) {
    let with_decay = run_with_config(scenario, Config { decay_enabled: true }).await;
    let no_decay = run_with_config(scenario, Config { decay_enabled: false }).await;
    (with_decay.sr, no_decay.sr)
}
// If with_decay > no_decay, decay is useful. If with_decay < no_decay, it's harmful.
```
- Test: `tests::decay_ablation_on_l4_debugging` — run L4 scenario both ways, assert improvement or document regression

---

## Commit Order

```bash
# 1. L4 scenarios + task agent
git add crates/ferrosa-memory-eval/src/task_agent.rs crates/ferrosa-memory-eval/src/task_grader.rs crates/ferrosa-memory-eval/scenarios/level4/
git commit -m "feat(eval): L4 task-level scenarios with mock agent and ablation harness"

# 2. Graph quality metrics
git add crates/ferrosa-memory-eval/src/metrics/kg_metrics.rs crates/ferrosa-memory-eval/src/metrics/graph_benchmark.rs
git commit -m "feat(eval): graph quality metrics — precision, recall, microstructure, dedup"

# 3. Cross-domain personalization
git add crates/ferrosa-memory-eval/src/metrics/personalization.rs crates/ferrosa-memory-eval/scenarios/level4/cross_domain_transfer.toml
git commit -m "feat(eval): cross-domain transfer scenarios + latent preference inference"

# 4. Episodic memory loop
git add crates/ferrosa-memory-core/src/warmth.rs crates/ferrosa-memory-core/src/retrieval.rs crates/ferrosa-memory-mcp/src/main.rs
git commit -m "feat(core): outcome-based warmth modulation + per-tool learning loop"

# 5. Progressive disclosure
git add crates/ferrosa-memory-core/src/progressive_disclosure.rs crates/ferrosa-memory-mcp/src/main.rs
git commit -m "feat(mcp): progressive disclosure — tiered tool visibility with hint injection"

# 6. Decay validation
git add crates/ferrosa-memory-eval/src/metrics/temporal_metrics.rs crates/ferrosa-memory-core/src/warmth.rs
git commit -m "feat(eval): temporal metrics — decay curve validation + content-type profiles"
```

---

## Dependency Graph

```
1 (L4 scenarios) ──requires──┬── 2 (kg metrics)        [graph state needed for tasks]
                             ├── 6 (decay validation)  [warmth affects retrieval in tasks]
                             └── 4 (episodic loop)     [outcomes affect agent behavior]

3 (cross-domain) ──requires── 1 (L4 agent)            [needs task agent to run cross-domain]

5 (progressive)  ──standalone── [no deps, no blockers]

4 (episodic)     ──requires── 6 (decay)               [outcome-modulated decay]
```

**Execution order:** 5 (standalone) → 6 (decay infra) → 4 (outcome loop on decay) → 1 (L4 harness) → 2 + 3 (metrics on harness)

---

## Token-Efficiency Notes

- **Mock LLM** avoids API cost in CI. Pre-canned responses are deterministic.
- **Ground-truth benchmarks** are synthetic (controlled entities/edges), not scraped.
- **Fake clock** for decay tests avoids real sleeps.
- **Single `task_agent.rs`** handles all L4 scenarios — parameterized by TOML, not per-scenario code.
- **Ablation framework** reuses same scenario with config flags — no duplicate scenario files.
