# Remaining Gaps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close all remaining production gaps in ferrosa-memory-mcp — vector column reads, audit log, anomaly detection, batch job, quotas, TLS, viz snapshot, and vestige feature parity — ordered from highest risk to lowest.

**Architecture:** Each task is self-contained. Vector column fix (F31) unblocks ANN search across the system. Audit log and anomaly detection complete the security story. Batch job closes the routing feedback loop. Vestige features (spreading activation, importance scoring, memory chains, speculative retrieval) extend the cognitive memory model.

**Tech Stack:** Rust, Tokio, cdrs-tokio (forked with vector support), D3.js, tokio-tungstenite

---

## File Structure

| File | Responsibility | Status |
|------|---------------|--------|
| `crates/ferrosa-memory-core/src/cql_storage.rs` | CQL prepared statements + read/write impl | Modify: vector reads, audit writes, edge queries |
| `crates/ferrosa-memory-core/src/storage.rs` | Storage trait + mock | Modify: add audit, edge list methods |
| `crates/ferrosa-memory-core/src/audit.rs` | Audit log + anomaly detection | Modify: wire CQL persistence |
| `crates/ferrosa-memory-core/src/http.rs` | HTTP/viz/WebSocket | Modify: viz snapshot, TLS, rate limiting |
| `crates/ferrosa-memory-core/src/entity.rs` | Entity upsert/retrieve | Modify: quota enforcement, anomaly check |
| `crates/ferrosa-memory-core/src/memo.rs` | Memo cache | Modify: quota enforcement |
| `crates/ferrosa-memory-core/src/quota.rs` | Quota checking | Already complete, needs wiring |
| `crates/ferrosa-memory-core/src/dispatch.rs` | Tool dispatch + handlers | Modify: new tools, router integration |
| `crates/ferrosa-memory-core/src/spreading.rs` | **New:** Spreading activation search | Create |
| `crates/ferrosa-memory-core/src/importance.rs` | **New:** Multi-channel importance scoring | Create |
| `crates/ferrosa-memory-core/src/chains.rs` | **New:** Memory chain path discovery | Create |
| `crates/ferrosa-memory-core/src/speculative.rs` | **New:** Speculative retrieval | Create |
| `crates/ferrosa-memory-core/src/dedup.rs` | **New:** Duplicate detection/merge | Create |
| `crates/ferrosa-memory-batch/src/main.rs` | Batch job binary | Modify: wire CQL + batch logic |
| `assets/viz.html` | D3 dashboard | Modify: handle Snapshot events |
| `crates/ferrosa-memory-core/src/security_tests.rs` | Security regression suite | Modify: add tests for new features |

---

## Task 1: Fix Vector Column Reads (F31) — CRITICAL BLOCKER

**Files:**
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs:353,574`

The vector encode/decode module works. Writes encode to Blob. Reads have two TODO stubs returning `None`. This is 2 lines of real code but unblocks ANN search system-wide.

- [ ] **Step 1: Write failing test**

In `cql_storage.rs` tests (or a new integration test), test that after storing a memo with an embedding, reading it back returns the embedding:
```rust
#[tokio::test]
async fn memo_embedding_round_trip() {
    // Store memo with embedding via mock
    // Read back, assert embedding is Some and matches
}
```

Note: The mock storage already stores embeddings correctly. The real fix is in CqlStorage. Write a unit test that verifies `decode_vector` is called on the blob column.

- [ ] **Step 2: Fix memo_get vector read (line ~353)**

In `cql_storage.rs`, in the `memo_get` implementation, replace:
```rust
result_embedding: None, // TODO: vector column read
```
with:
```rust
result_embedding: row.get("result_embedding")
    .ok()
    .flatten()
    .and_then(|blob: Vec<u8>| {
        if blob.is_empty() { None } else { Some(crate::vector::decode_vector(&blob)) }
    }),
```

- [ ] **Step 3: Fix fold_get vector read (line ~574)**

Same pattern for `fold_embedding` in the fold_get implementation:
```rust
fold_embedding: row.get("fold_embedding")
    .ok()
    .flatten()
    .and_then(|blob: Vec<u8>| {
        if blob.is_empty() { None } else { Some(crate::vector::decode_vector(&blob)) }
    }),
```

- [ ] **Step 4: Fix entity vector read**

Check if entity_search_ann also has a TODO for embedding deserialization and fix it.

- [ ] **Step 5: Run tests, commit**

```bash
cargo test --lib -p ferrosa-memory-core
git commit -m "fix: read vector columns from CQL (closes F31)"
```

---

## Task 2: Viz Snapshot on WebSocket Connect

**Files:**
- Modify: `crates/ferrosa-memory-core/src/http.rs` (handle_viz_ws function)
- Modify: `assets/viz.html`

New WebSocket clients see a blank graph. On connect, query CQL for all entities + edges and send a Snapshot event.

- [ ] **Step 1: Update handle_viz_ws to send snapshot**

The function needs access to storage and ctx. Update the signature and callers. Before the event subscription loop, build and send a snapshot:

```rust
// Build initial snapshot from storage
let entities = storage.entity_list_session(ctx, session_id).await.unwrap_or_default();
let mut nodes: Vec<VizNode> = entities.iter().map(|e| crate::viz::entity_to_viz_node(e)).collect();
let edges: Vec<VizEdge> = Vec::new(); // Edge listing requires new Storage method

let snapshot = VizEvent::Snapshot { nodes, edges };
let json = serde_json::to_string(&snapshot).unwrap_or_default();
// Send via WebSocket frame
```

- [ ] **Step 2: Add edge_list_session to Storage trait**

```rust
async fn edge_list_session(
    &self,
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<Vec<(Uuid, Uuid, String)>>; // (source, target, edge_type)
```

Implement in mock (return empty vec) and CQL storage (query all 4 edge tables).

- [ ] **Step 3: Update viz.html to handle Snapshot**

The frontend already handles Snapshot in `handleEvent()`. Verify it works — the `case 'Snapshot'` sets `nodes` and `edges` and calls `render()`. Should work as-is.

- [ ] **Step 4: Test manually**

Start the server, open `http://localhost:8766/viz`, create some entities via MCP, verify they appear.

- [ ] **Step 5: Commit**

```bash
git commit -m "feat: send graph snapshot to viz clients on WebSocket connect"
```

---

## Task 3: Audit Log CQL Persistence

**Files:**
- Modify: `crates/ferrosa-memory-core/src/storage.rs` (add trait method)
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs` (add prepared statement + impl)
- Modify: `crates/ferrosa-memory-core/src/audit.rs` (update to use storage)
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs` (call audit after writes)

audit.rs creates AuditEntry objects in memory. We need to persist them to CQL and call from write handlers.

- [ ] **Step 1: Add audit_put to Storage trait**

```rust
async fn audit_put(&self, ctx: &TenantContext, entry: &AuditEntry) -> anyhow::Result<()>;
```

- [ ] **Step 2: Implement in mock and CQL**

CQL: `INSERT INTO audit_log (tenant_id, audit_id, operation, target_table, target_id, session_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)`

- [ ] **Step 3: Update audit::log_write to accept storage**

Change signature to take `storage` and persist:
```rust
pub async fn log_write(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    operation: &str,
    target_table: &str,
    target_id: &str,
    session_id: Uuid,
) -> anyhow::Result<()> {
    let entry = AuditEntry { ... };
    storage.audit_put(ctx, &entry).await
}
```

- [ ] **Step 4: Wire into dispatch handlers**

After successful mutations in `handle_smart_ingest`, `handle_upsert_entity`, `handle_complete_fold`, `handle_write_temporal_fact`, call `audit::log_write()`. Best-effort (ignore errors).

- [ ] **Step 5: Add security test**

Verify audit rows are created after entity writes.

- [ ] **Step 6: Commit**

```bash
git commit -m "feat: persist audit log entries to CQL (STRIDE R1)"
```

---

## Task 4: Anomaly Detection Integration

**Files:**
- Modify: `crates/ferrosa-memory-core/src/entity.rs` (add anomaly check to retrieve path)
- Modify: `crates/ferrosa-memory-core/src/audit.rs` (session baseline tracking)
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs` (pass config to retrieve handler)

The `check_anomaly()` function exists but is never called. Wire it into entity retrieval.

- [ ] **Step 1: Track retrieval counts per entity per session**

Add a simple counter to SessionState (or a new struct):
```rust
pub struct RetrievalTracker {
    counts: std::collections::HashMap<Uuid, usize>,
}
```

- [ ] **Step 2: In retrieve_entities handler, increment count and check**

After retrieval, for each returned entity:
```rust
tracker.increment(entity_id);
if audit::check_anomaly(tracker.count(entity_id), tracker.mean(), tracker.stddev(), config.anomaly_sigma_threshold) {
    tracing::warn!(entity_id = %entity_id, "anomalous retrieval frequency");
    // Emit metric
}
```

- [ ] **Step 3: Add test**

Create 100 retrievals for one entity, verify anomaly flag fires.

- [ ] **Step 4: Commit**

```bash
git commit -m "feat: anomaly detection on entity retrieval frequency (STRIDE T1)"
```

---

## Task 5: Quota Enforcement

**Files:**
- Modify: `crates/ferrosa-memory-core/src/entity.rs` (check before put)
- Modify: `crates/ferrosa-memory-core/src/memo.rs` (check before put)
- Modify: `crates/ferrosa-memory-core/src/config.rs` (add max_entities config)

quota.rs has `check_quota()` and `check_memo_quota()` fully implemented. Just needs wiring.

- [ ] **Step 1: Add max_entities to MemoryConfig**

```rust
pub max_entities: usize, // default 10_000
```

- [ ] **Step 2: Call check_quota before entity_put**

In entity.rs `upsert_entity()`, before creating a new entity:
```rust
let count = storage.entity_count(ctx, session_id).await?;
crate::quota::check_quota(count, config.max_entities)?;
```

- [ ] **Step 3: Call check_memo_quota before memo_put**

In memo.rs `store_memo_result()`:
```rust
let count = storage.memo_count(ctx).await?;
crate::quota::check_memo_quota(count, config.max_memo_results)?;
```

- [ ] **Step 4: Add tests, commit**

```bash
git commit -m "feat: enforce per-tenant entity and memo quotas (FMEA D1)"
```

---

## Task 6: Wire Batch Job

**Files:**
- Modify: `crates/ferrosa-memory-batch/src/main.rs`
- Modify: `crates/ferrosa-memory-batch/Cargo.toml` (add ferrosa-memory-core dep)

batch.rs already has `compute_strategy_accuracy()` and `generate_guidelines()`. The main.rs binary has 5 TODOs. Wire them together.

- [ ] **Step 1: Add CQL connection**

Use same CqlStorage from ferrosa-memory-core:
```rust
let storage = CqlStorage::connect(&config.ferrosa).await?;
```

- [ ] **Step 2: Query feedback_outcomes**

```rust
let outcomes = storage.feedback_list_failures(ctx).await?;
```
Add `feedback_list_failures` to Storage trait if not present.

- [ ] **Step 3: Compute accuracy and generate guidelines**

```rust
let accuracy = ferrosa_memory_core::batch::compute_strategy_accuracy(&outcomes);
let guidelines = ferrosa_memory_core::batch::generate_guidelines(&accuracy);
```

- [ ] **Step 4: Write routing guidelines to CQL**

Add a routing_guidelines table or use entity_store with a special type.

- [ ] **Step 5: Test, commit**

```bash
git commit -m "feat: wire batch job for nightly guideline refinement (ADR-002)"
```

---

## Task 7: TLS Support

**Files:**
- Modify: `crates/ferrosa-memory-core/src/http.rs`
- Modify: `crates/ferrosa-memory-core/Cargo.toml` (add tokio-rustls)
- Modify: `crates/ferrosa-memory-core/src/config.rs` (add cert_path, key_path)

- [ ] **Step 1: Add TLS config fields**

```rust
pub cert_path: Option<String>,
pub key_path: Option<String>,
```

- [ ] **Step 2: Add tokio-rustls dependency**

- [ ] **Step 3: Wrap TCP streams in TLS when configured**

In `serve_http()`, if `require_tls` and cert+key are set, create a TLS acceptor and wrap the stream.

- [ ] **Step 4: Add connection rate limiting**

Simple per-IP counter with a HashMap<IpAddr, (count, Instant)>. Reject connections exceeding limit.

- [ ] **Step 5: Test, commit**

```bash
git commit -m "feat: TLS support and connection rate limiting (FMEA F30)"
```

---

## Task 8: Spreading Activation Search

**Files:**
- Create: `crates/ferrosa-memory-core/src/spreading.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`

Vestige's core retrieval mechanism: activation spreads from a seed node through graph edges. Connected nodes with stronger edges get higher activation.

- [ ] **Step 1: Create spreading.rs**

```rust
//! Spreading activation — Collins & Loftus semantic network retrieval.

use std::collections::HashMap;
use uuid::Uuid;
use serde::Serialize;

use crate::storage::Storage;
use crate::types::TenantContext;

#[derive(Debug, Clone, Serialize)]
pub struct ActivatedNode {
    pub entity_id: Uuid,
    pub activation: f64,
    pub hops: usize,
}

/// Spread activation from seed entities through the graph.
pub async fn spread(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    seeds: Vec<Uuid>,
    max_hops: usize,
    decay: f64,  // 0.0-1.0, how much activation decays per hop
    limit: usize,
) -> anyhow::Result<Vec<ActivatedNode>> {
    let mut activation: HashMap<Uuid, f64> = HashMap::new();
    let mut frontier: Vec<(Uuid, f64, usize)> = seeds.iter().map(|id| (*id, 1.0, 0)).collect();

    // Initialize seeds
    for seed in &seeds {
        activation.insert(*seed, 1.0);
    }

    // BFS with decaying activation
    while let Some((node_id, current_activation, hop)) = frontier.pop() {
        if hop >= max_hops { continue; }

        // Get neighbors via edge_list_for_entity
        let neighbors = storage.edge_list_for_entity(ctx, node_id).await?;
        let spread_activation = current_activation * decay;

        for (neighbor_id, _edge_type) in neighbors {
            let existing = activation.entry(neighbor_id).or_insert(0.0);
            *existing += spread_activation;
            if hop + 1 < max_hops {
                frontier.push((neighbor_id, spread_activation, hop + 1));
            }
        }
    }

    // Sort by activation, return top-k
    let mut results: Vec<ActivatedNode> = activation
        .into_iter()
        .filter(|(id, _)| !seeds.contains(id)) // exclude seeds
        .map(|(entity_id, act)| ActivatedNode { entity_id, activation: act, hops: 0 })
        .collect();
    results.sort_by(|a, b| b.activation.partial_cmp(&a.activation).unwrap());
    results.truncate(limit);
    Ok(results)
}
```

- [ ] **Step 2: Add edge_list_for_entity to Storage trait**

```rust
async fn edge_list_for_entity(
    &self, ctx: &TenantContext, entity_id: Uuid,
) -> anyhow::Result<Vec<(Uuid, String)>>; // (neighbor_id, edge_type)
```

- [ ] **Step 3: Add ToolDef and handler**

`spread_activation` tool: seeds (array of uuid), max_hops (int 1-5), decay (float 0-1), limit (int).

- [ ] **Step 4: Tests, commit**

```bash
git commit -m "feat: spreading activation search (Collins & Loftus)"
```

---

## Task 9: Importance Scoring

**Files:**
- Create: `crates/ferrosa-memory-core/src/importance.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`

4-channel scoring inspired by vestige's neuroscience model: novelty, arousal, reward, attention.

- [ ] **Step 1: Create importance.rs**

```rust
//! Multi-channel importance scoring.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ImportanceScore {
    pub novelty: f64,     // How surprising/new (from smart_ingest similarity)
    pub arousal: f64,     // Emotional intensity (keyword heuristic)
    pub reward: f64,      // Past retrieval success (from feedback_outcomes)
    pub attention: f64,   // Recency and access frequency
    pub composite: f64,   // Weighted average
}

pub fn compute_importance(
    similarity_to_existing: f64,   // from smart_ingest
    retrieval_count: usize,        // how often retrieved
    last_accessed_seconds_ago: i64,
    feedback_success_rate: f64,    // from feedback_outcomes
) -> ImportanceScore {
    let novelty = 1.0 - similarity_to_existing;
    let arousal = 0.5; // placeholder — could use keyword detection
    let reward = feedback_success_rate;
    let attention = 1.0 / (1.0 + (last_accessed_seconds_ago as f64 / 3600.0));

    let composite = 0.3 * novelty + 0.2 * arousal + 0.3 * reward + 0.2 * attention;

    ImportanceScore { novelty, arousal, reward, attention, composite }
}
```

- [ ] **Step 2: Add `importance_score` ToolDef**

Takes entity_id, returns the score breakdown.

- [ ] **Step 3: Tests, commit**

```bash
git commit -m "feat: multi-channel importance scoring (novelty/arousal/reward/attention)"
```

---

## Task 10: Memory Chains (Path Discovery)

**Files:**
- Create: `crates/ferrosa-memory-core/src/chains.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`

BFS path discovery between two concepts through the knowledge graph.

- [ ] **Step 1: Create chains.rs**

```rust
//! Memory chains — path discovery between concepts.

use uuid::Uuid;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};

use crate::storage::Storage;
use crate::types::TenantContext;

#[derive(Debug, Clone, Serialize)]
pub struct ChainStep {
    pub entity_id: Uuid,
    pub entity_name: String,
    pub edge_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryChain {
    pub source: Uuid,
    pub destination: Uuid,
    pub steps: Vec<ChainStep>,
    pub hop_count: usize,
    pub confidence: f64,
}

/// Find shortest path between two entities via BFS.
pub async fn find_chain(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    source: Uuid,
    destination: Uuid,
    max_hops: usize,
) -> anyhow::Result<Option<MemoryChain>> {
    let mut visited: HashMap<Uuid, (Uuid, String)> = HashMap::new(); // node → (parent, edge_type)
    let mut queue: VecDeque<(Uuid, usize)> = VecDeque::new();
    queue.push_back((source, 0));

    while let Some((current, depth)) = queue.pop_front() {
        if current == destination {
            // Reconstruct path
            let mut steps = Vec::new();
            let mut node = destination;
            while node != source {
                if let Some((parent, edge_type)) = visited.get(&node) {
                    steps.push(ChainStep {
                        entity_id: node,
                        entity_name: String::new(), // fill from storage
                        edge_type: edge_type.clone(),
                    });
                    node = *parent;
                } else { break; }
            }
            steps.reverse();
            let hop_count = steps.len();
            let confidence = 1.0 / (1.0 + hop_count as f64);
            return Ok(Some(MemoryChain { source, destination, steps, hop_count, confidence }));
        }

        if depth >= max_hops { continue; }

        let neighbors = storage.edge_list_for_entity(ctx, current).await?;
        for (neighbor_id, edge_type) in neighbors {
            if !visited.contains_key(&neighbor_id) && neighbor_id != source {
                visited.insert(neighbor_id, (current, edge_type));
                queue.push_back((neighbor_id, depth + 1));
            }
        }
    }

    Ok(None)
}
```

- [ ] **Step 2: Add `find_memory_chain` ToolDef**

Takes source (uuid), destination (uuid), max_hops (int 1-10).

- [ ] **Step 3: Tests, commit**

```bash
git commit -m "feat: memory chain path discovery between concepts"
```

---

## Task 11: Speculative Retrieval

**Files:**
- Create: `crates/ferrosa-memory-core/src/speculative.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`

Proactively predict which memories will be needed based on access patterns.

- [ ] **Step 1: Create speculative.rs**

Track co-access patterns: when entities A and B are frequently retrieved in the same session, retrieving A should suggest B.

```rust
//! Speculative retrieval — predict needed memories from access patterns.

use std::collections::HashMap;
use uuid::Uuid;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Prediction {
    pub entity_id: Uuid,
    pub confidence: f64,
    pub reason: String,
}

/// Given recently accessed entities, predict what else will be needed.
pub fn predict(
    recent_accesses: &[Uuid],
    co_access_counts: &HashMap<(Uuid, Uuid), usize>,
    threshold: f64,
) -> Vec<Prediction> {
    let mut scores: HashMap<Uuid, f64> = HashMap::new();

    for &accessed in recent_accesses {
        for (&(a, b), &count) in co_access_counts {
            if a == accessed && !recent_accesses.contains(&b) {
                *scores.entry(b).or_insert(0.0) += count as f64;
            }
            if b == accessed && !recent_accesses.contains(&a) {
                *scores.entry(a).or_insert(0.0) += count as f64;
            }
        }
    }

    let max_score = scores.values().cloned().fold(0.0_f64, f64::max);
    if max_score == 0.0 { return Vec::new(); }

    let mut predictions: Vec<Prediction> = scores
        .into_iter()
        .map(|(id, score)| {
            let confidence = score / max_score;
            Prediction { entity_id: id, confidence, reason: "co-access pattern".into() }
        })
        .filter(|p| p.confidence >= threshold)
        .collect();

    predictions.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
    predictions.truncate(10);
    predictions
}
```

- [ ] **Step 2: Track co-access in SessionState**

Add access tracking to SessionState. Each retrieve call records entity IDs.

- [ ] **Step 3: Add `predict_needed` ToolDef**

Returns predicted entities with confidence scores.

- [ ] **Step 4: Tests, commit**

```bash
git commit -m "feat: speculative retrieval from co-access patterns"
```

---

## Task 12: Duplicate Detection

**Files:**
- Create: `crates/ferrosa-memory-core/src/dedup.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`

Find and merge semantically duplicate entities using text similarity.

- [ ] **Step 1: Create dedup.rs**

```rust
//! Duplicate detection and merge suggestions.

use uuid::Uuid;
use serde::Serialize;

use crate::storage::Storage;
use crate::types::{EntityEntry, TenantContext};

#[derive(Debug, Clone, Serialize)]
pub struct DuplicatePair {
    pub entity_a: Uuid,
    pub entity_b: Uuid,
    pub name_a: String,
    pub name_b: String,
    pub similarity: f64,
}

/// Find potential duplicates among session entities.
pub async fn find_duplicates(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    threshold: f64,
) -> anyhow::Result<Vec<DuplicatePair>> {
    let entities = storage.entity_list_session(ctx, session_id).await?;
    let mut pairs = Vec::new();

    // O(n^2) comparison — fine for <1000 entities per session
    for i in 0..entities.len() {
        for j in (i + 1)..entities.len() {
            let sim = crate::smart_ingest::compute_text_similarity(
                &entities[i].context_snippet,
                &entities[j].context_snippet,
            );
            if sim >= threshold {
                pairs.push(DuplicatePair {
                    entity_a: entities[i].entity_id,
                    entity_b: entities[j].entity_id,
                    name_a: entities[i].entity_name.clone(),
                    name_b: entities[j].entity_name.clone(),
                    similarity: sim,
                });
            }
        }
    }

    pairs.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
    Ok(pairs)
}
```

Note: `compute_text_similarity` in smart_ingest.rs is currently private. Make it `pub`.

- [ ] **Step 2: Add `find_duplicates` ToolDef**

Takes session_id, threshold (float 0-1, default 0.7).

- [ ] **Step 3: Tests, commit**

```bash
git commit -m "feat: duplicate detection for entity deduplication"
```

---

## Task 13: Router Integration

**Files:**
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs`

The router (`router.rs`) is fully implemented but not called from dispatch. Wire it so retrieval tools use the router for strategy selection.

- [ ] **Step 1: In retrieve_fold_context and retrieve_entities, call router**

Before executing the search, run the query through the router to determine optimal strategy:
```rust
let decision = crate::router::route(query, has_embedding, complexity);
// Use decision.k and decision.include_raw
```

- [ ] **Step 2: Test routing integration**

Verify that queries with entity names route to phonetic, queries with embeddings route to HNSW.

- [ ] **Step 3: Commit**

```bash
git commit -m "feat: wire routing decision tree into retrieval handlers"
```

---

## Task 14: Security Hardening Sweep

**Files:**
- Modify: `crates/ferrosa-memory-core/src/security_tests.rs`

Add tests for all newly implemented security features.

- [ ] **Step 1: Add audit log test**

Verify entity write creates audit row.

- [ ] **Step 2: Add quota enforcement test**

Verify writes rejected when quota exceeded.

- [ ] **Step 3: Add anomaly detection test**

Verify anomalous retrieval pattern is flagged.

- [ ] **Step 4: Add right-to-deletion cascade test**

Verify `delete_session` removes from all tables.

- [ ] **Step 5: Commit**

```bash
git commit -m "feat: security hardening sweep — audit, quota, anomaly, deletion tests"
```

---

## Task 15: S3 Lifecycle Configuration

**Files:**
- Create: `docs/operations/s3-lifecycle.md`

Document the `ferrosa-ctl` commands needed to configure Glacier tiering for archived folds.

- [ ] **Step 1: Write operational docs**

```markdown
# S3 Lifecycle Configuration for Archived Folds

Folds with status='archived' should tier to S3 Glacier after 30 days.

## RustFS/MinIO Configuration
mc ilm add local/ferrosa-memory --transition-days 30 --storage-class GLACIER --prefix "archived/"

## Ferrosa-ctl (when available)
ferrosa-ctl s3 lifecycle set --bucket ferrosa-memory --rule archive-30d --transition-days 30
```

- [ ] **Step 2: Commit**

```bash
git commit -m "docs: S3 lifecycle configuration for archived fold tiering"
```

---

## Summary

| Task | Risk | Area | New Tools |
|------|------|------|-----------|
| 1. Vector column reads (F31) | CRITICAL | Core | 0 |
| 2. Viz snapshot | HIGH | UX | 0 |
| 3. Audit log persistence | HIGH | Security | 0 |
| 4. Anomaly detection | HIGH | Security | 0 |
| 5. Quota enforcement | MEDIUM | Security | 0 |
| 6. Batch job wiring | MEDIUM | Routing | 0 |
| 7. TLS support | MEDIUM | Security | 0 |
| 8. Spreading activation | MEDIUM | Vestige | 1 (`spread_activation`) |
| 9. Importance scoring | LOW | Vestige | 1 (`importance_score`) |
| 10. Memory chains | LOW | Vestige | 1 (`find_memory_chain`) |
| 11. Speculative retrieval | LOW | Vestige | 1 (`predict_needed`) |
| 12. Duplicate detection | LOW | Vestige | 1 (`find_duplicates`) |
| 13. Router integration | LOW | Core | 0 |
| 14. Security sweep | LOW | Security | 0 |
| 15. S3 lifecycle docs | LOW | Ops | 0 |

**Total new tools: 5** (27 → 32)
