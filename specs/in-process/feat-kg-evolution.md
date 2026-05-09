# fmem Knowledge Graph Evolution — 5-Gap Implementation Plan

> **For Hermes:** Use `subagent-driven-development` skill to implement task-by-task.
> **Branch:** `feat/kg-evolution` off `feat/datalog-filter-grammar`

**Goal:** Wire confidence scoring, automated decay, consolidation pipeline, contradiction detection, and schema co-evolution into ferrosa-memory.

**Status:** Tasks 0-2 complete and committed. Task 3 (consolidation) and Task 4 (contradiction wiring) + Task 5 (schema bundles) remain. Subagents timed out on Tasks 3-4 due to Storage trait complexity.

**Completed commits:**
- `88f02d6` feat(kg-evolution): DDLs and types for confidence, contradiction, consolidation, schema bundles
- `62e815d` feat(confidence): compute and store confidence scores per temporal fact  
- `e551d94` feat(decay): automated decay pass + threshold-based forgetting in batch job
- `cd8e9f8` feat(contradiction): core logic stub with tests (Task 4 partial)

**Architecture:** Layer 4 new DDL tables + Rust modules onto existing `warmth.rs`, `temporal.rs`, `skill.rs` infrastructure. Batch job in `ferrosa-memory-batch` drives automation. No breaking changes to existing API.

**Tech Stack:** Rust (Tokio), CQL (Ferrosa), scylla-rust-driver fork

---

## Task 0: Infrastructure — Branch + DDLs

**Objective:** Create migration-safe DDLs for 4 new tables without breaking existing schema.

**Files:**
- Create: `ddl/026_confidence_scoring.cql`
- Create: `ddl/027_contradiction_registry.cql`
- Create: `ddl/028_consolidation_pipeline.cql`
- Create: `ddl/029_domain_schema_bundles.cql`
- Modify: `crates/ferrosa-memory-core/src/migration.rs` (append to `BOOTSTRAP_DDLS` + `MIGRATIONS`)
- Modify: `crates/ferrosa-memory-core/src/types.rs` (new struct fields)

**Step 1: Write DDL 026 — Confidence Scoring**

```sql
-- Confidence scoring for temporal facts and entities
-- Sprint: KG Evolution Gap 1

CREATE TABLE IF NOT EXISTS agent_memory.confidence_scores (
    entity_id uuid,
    fact_hash text,           -- SHA256 of fact_text for dedup
    confidence double,          -- 0.0 to 1.0
    source_count int,           -- how many sources support this
    last_confirmed_at timestamp,
    contradiction_count int,    -- how many facts contradict this
    PRIMARY KEY ((entity_id), fact_hash)
);

CREATE INDEX IF NOT EXISTS idx_confidence_high
    ON agent_memory.confidence_scores (confidence)
    WHERE confidence >= 0.8;
```

**Step 2: Write DDL 027 — Contradiction Registry**

```sql
-- Tracks fact-to-fact contradictions for human review
-- Sprint: KG Evolution Gap 4

CREATE TABLE IF NOT EXISTS agent_memory.contradictions (
    tenant_id uuid,
    entity_id uuid,
    old_fact_hash text,
    new_fact_hash text,
    old_fact_text text,
    new_fact_text text,
    detected_at timestamp,
    resolved_at timestamp,
    resolution text,            -- 'superseded', 'merged', 'false_positive', 'pending'
    resolver text,            -- 'agent', 'human', 'batch'
    PRIMARY KEY ((tenant_id, entity_id), detected_at, old_fact_hash, new_fact_hash)
) WITH CLUSTERING ORDER BY (detected_at DESC);
```

**Step 3: Write DDL 028 — Consolidation Pipeline**

```sql
-- Tracks promotion of folds → entities → skills
-- Sprint: KG Evolution Gap 3

CREATE TABLE IF NOT EXISTS agent_memory.consolidation_queue (
    tenant_id uuid,
    stage text,               -- 'fold_raw', 'fold_compressed', 'entity_extracted', 'skill_candidate'
    artifact_id uuid,         -- fold_id or entity_id or skill_name
    artifact_kind text,       -- 'fold', 'entity', 'skill'
    source_session_id uuid,
    promotion_score double,   -- computed warmth + confidence composite
    promoted_at timestamp,
    PRIMARY KEY ((tenant_id, stage), promotion_score, artifact_id)
) WITH CLUSTERING ORDER BY (promotion_score DESC);

CREATE TABLE IF NOT EXISTS agent_memory.consolidation_history (
    tenant_id uuid,
    artifact_id uuid,
    from_stage text,
    to_stage text,
    promoted_at timestamp,
    promotion_reason text,
    PRIMARY KEY ((tenant_id, artifact_id), promoted_at)
) WITH CLUSTERING ORDER BY (promoted_at DESC);
```

**Step 4: Write DDL 029 — Domain Schema Bundles**

```sql
-- Versioned domain schemas (bundles of skills)
-- Sprint: KG Evolution Gap 5

CREATE TABLE IF NOT EXISTS agent_memory.domain_schemas (
    schema_id uuid,
    schema_name text,
    version int,
    description text,
    skill_names list<text>,     -- ordered list of skill names in this schema
    routing_guidelines text,    -- YAML/JSON blob
    created_at timestamp,
    updated_at timestamp,
    PRIMARY KEY ((schema_id), version)
) WITH CLUSTERING ORDER BY (version DESC);

CREATE INDEX IF NOT EXISTS idx_domain_schema_name
    ON agent_memory.domain_schemas (schema_name);
```

**Step 5: Register DDLs in migration.rs**

Add 4 `include_str!` entries to `BOOTSTRAP_DDLS` (for greenfield) and 4 `Migration` entries to `MIGRATIONS` (version 26-29) for existing deployments.

**Step 6: Add types**

In `types.rs`, add:
- `ConfidenceScore` struct
- `ContradictionEntry` struct
- `ConsolidationStage` enum (`FoldRaw`, `FoldCompressed`, `EntityExtracted`, `SkillCandidate`)
- `DomainSchema` struct

**Step 7: Verify DDLs**

Run: `cargo test -p ferrosa-memory-core --test launch_gates_g3_g4`
Expected: PASS (no schema regressions)

**Step 8: Commit**

```bash
git add ddl/026_*.cql ddl/027_*.cql ddl/028_*.cql ddl/029_*.cql
git add crates/ferrosa-memory-core/src/migration.rs crates/ferrosa-memory-core/src/types.rs
git commit -m "feat(kg-evolution): DDLs for confidence, contradiction, consolidation, schema bundles"
```

---

## Task 1: Confidence Scoring Module

**Objective:** Compute and store confidence scores for every temporal fact.

**Files:**
- Create: `crates/ferrosa-memory-core/src/confidence.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs` (add `pub mod confidence;`)
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs` (add `confidence_put`, `confidence_get`)
- Test: `crates/ferrosa-memory-core/src/confidence.rs` (inline `#[cfg(test)]`)

**Step 1: Write confidence.rs**

```rust
//! Confidence scoring for temporal facts.
//!
//! Confidence = source_support * recency_bonus * (1 - contradiction_penalty)
//!
//! - source_support: min(source_count / 5, 1.0) — cap at 5 sources = full weight
//! - recency_bonus: exp(-age_in_days / 30) — 30-day half-life
//! - contradiction_penalty: 0.2 * contradiction_count (capped at 0.5)

use crate::config::RmhConfig;
use crate::storage::Storage;
use crate::types::{ConfidenceScore, TenantContext};
use uuid::Uuid;

/// Compute confidence score for a fact.
pub fn compute_confidence(
    source_count: usize,
    last_confirmed_at: chrono::DateTime<chrono::Utc>,
    contradiction_count: usize,
) -> f64 {
    let source_support = (source_count as f64 / 5.0).min(1.0);
    let age_days = chrono::Utc::now()
        .signed_duration_since(last_confirmed_at)
        .num_days() as f64;
    let recency_bonus = (-age_days / 30.0).exp();
    let contradiction_penalty = (0.2 * contradiction_count as f64).min(0.5);
    (source_support * recency_bonus * (1.0 - contradiction_penalty)).clamp(0.0, 1.0)
}

/// Record or update confidence for a fact.
pub async fn record_fact_confidence(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
    fact_text: &str,
    source_count: usize,
    contradiction_count: usize,
) -> anyhow::Result<f64> {
    let fact_hash = format!("{:x}", sha2::Sha256::digest(fact_text.as_bytes()));
    let now = chrono::Utc::now();
    let confidence = compute_confidence(source_count, now, contradiction_count);

    let score = ConfidenceScore {
        entity_id,
        fact_hash,
        confidence,
        source_count: source_count as i32,
        last_confirmed_at: now,
        contradiction_count: contradiction_count as i32,
    };

    storage.confidence_put(ctx, &score).await?;
    Ok(confidence)
}
```

**Step 2: Add storage methods to cql_storage.rs**

Add prepared statements:
```rust
confidence_put: session.prepare("INSERT INTO {ks}.confidence_scores (entity_id, fact_hash, confidence, source_count, last_confirmed_at, contradiction_count) VALUES (?, ?, ?, ?, ?, ?)").await?,
confidence_get: session.prepare("SELECT confidence, source_count, last_confirmed_at, contradiction_count FROM {ks}.confidence_scores WHERE entity_id = ? AND fact_hash = ?").await?,
```

Implement `Storage::confidence_put` and `confidence_get`.

**Step 3: Wire into `write_temporal_fact`**

In `temporal.rs`, after writing a new fact, call `confidence::record_fact_confidence(storage, ctx, entity_id, fact_text, 1, 0).await?`.

**Step 4: Test**

```rust
#[tokio::test]
async fn test_confidence_computation() {
    let score = compute_confidence(3, chrono::Utc::now(), 0);
    assert!((0.5..=0.7).contains(&score), "3 sources, fresh, no contradictions → ~0.6");

    let old = chrono::Utc::now() - chrono::Duration::days(60);
    let stale = compute_confidence(5, old, 0);
    assert!(stale < 0.5, "60 days old → decayed");

    let contradicted = compute_confidence(5, chrono::Utc::now(), 3);
    assert!(contradicted < 0.5, "3 contradictions → penalty");
}
```

Run: `cargo test -p ferrosa-memory-core confidence`
Expected: 3 passed

**Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/confidence.rs crates/ferrosa-memory-core/src/lib.rs
git add crates/ferrosa-memory-core/src/cql_storage.rs crates/ferrosa-memory-core/src/temporal.rs
git commit -m "feat(confidence): compute and store confidence scores per temporal fact"
```

---

## Task 2: Automated Decay + Forgetting

**Objective:** Wire `run_decay_pass` into the batch job and add threshold-based pruning.

**Files:**
- Modify: `crates/ferrosa-memory-core/src/warmth.rs` (add `prune_forgotten`)
- Modify: `crates/ferrosa-memory-batch/src/main.rs` (add `run_decay_and_forget` subcommand)
- Modify: `crates/ferrosa-memory-core/src/config.rs` (add `forget_threshold`, `decay_interval_hours`)

**Step 1: Add prune_forgotten to warmth.rs**

```rust
/// Remove warmth entries below threshold (soft-delete → move to cold storage).
/// Returns count of pruned entries.
pub async fn prune_forgotten(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    threshold: f64,
) -> anyhow::Result<usize> {
    let entries = storage.warmth_list_session(ctx, session_id).await?;
    let mut pruned = 0;
    for entry in entries {
        let score = compute_warmth_score(storage, ctx, entry.entity_id, &RmhConfig::default()).await?;
        if score < threshold {
            storage.warmth_delete(ctx, entry.entity_id).await?;
            pruned += 1;
        }
    }
    Ok(pruned)
}
```

**Step 2: Add config fields**

In `config.rs` `RmhConfig`:
```rust
pub forget_threshold: f64,      // default 0.05 — below this = forgotten
pub decay_interval_hours: u32,  // default 24 — how often batch job runs
```

**Step 3: Add batch job subcommand**

In `ferrosa-memory-batch/src/main.rs`:
```rust
"decay-forget" => run_decay_and_forget(&config).await,
```

```rust
async fn run_decay_and_forget(config: &Config) -> anyhow::Result<()> {
    let storage = CqlStorage::connect(&batch_cql_config(config)).await?;
    let ctx = TenantContext::default();
    let rmh = &config.rmh;

    let decayed = run_decay_pass(&storage, &ctx, config.server.session_id.parse()?, rmh).await?;
    tracing::info!(decayed, "warmth decay applied");

    let pruned = prune_forgotten(&storage, &ctx, config.server.session_id.parse()?, rmh.forget_threshold).await?;
    tracing::info!(pruned, "entities forgotten");

    Ok(())
}
```

**Step 4: Test**

Run: `FERROSA_TEST_CQL_PORT=19542 cargo test -p ferrosa-memory-core --test scylla_driver_migration t06_memo_touch_increments_hit_count -- --nocapture`
Expected: PASS (warmth still works)

**Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/warmth.rs crates/ferrosa-memory-core/src/config.rs
git add crates/ferrosa-memory-batch/src/main.rs
git commit -m "feat(decay): automated decay pass + threshold-based forgetting in batch job"
```

---

## Task 3: Consolidation Pipeline

**Objective:** Automate promotion: raw fold → compressed fold → extracted entity → skill candidate.

**Files:**
- Create: `crates/ferrosa-memory-core/src/consolidation.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs` (add `pub mod consolidation;`)
- Modify: `crates/ferrosa-memory-batch/src/main.rs` (add `run_consolidation` subcommand)
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs` (add consolidation prepared statements)

**Step 1: Write consolidation.rs**

```rust
//! Automated consolidation pipeline.
//!
//! Stages:
//! 1. fold_raw → fold_compressed (token count drops below threshold)
//! 2. fold_compressed → entity_extracted (reusable fact extracted)
//! 3. entity_extracted → skill_candidate (pattern repeated across 3+ sessions)

use crate::storage::Storage;
use crate::types::{ConsolidationStage, TenantContext};
use uuid::Uuid;

/// Run one consolidation cycle for a tenant.
pub async fn run_consolidation_cycle(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    config: &ConsolidationConfig,
) -> anyhow::Result<ConsolidationReport> {
    let mut report = ConsolidationReport::default();

    // Stage 1: Compress folds with high token counts
    let raw_folds = storage.fold_list_raw(ctx).await?;
    for fold in raw_folds {
        if fold.token_count > config.compression_threshold {
            storage.consolidation_queue_put(ctx, &ConsolidationStage::FoldCompressed, &fold).await?;
            report.compressed += 1;
        }
    }

    // Stage 2: Extract entities from compressed folds
    let compressed = storage.consolidation_queue_list(ctx, &ConsolidationStage::FoldCompressed).await?;
    for fold in compressed {
        if let Some(entity) = extract_entity_from_fold(&fold).await? {
            storage.consolidation_queue_put(ctx, &ConsolidationStage::EntityExtracted, &entity).await?;
            report.entities_extracted += 1;
        }
    }

    // Stage 3: Promote to skill candidates (3+ sessions, high confidence)
    let extracted = storage.consolidation_queue_list(ctx, &ConsolidationStage::EntityExtracted).await?;
    for entity in extracted {
        let sessions = storage.entity_sessions(ctx, entity.entity_id).await?;
        if sessions.len() >= 3 {
            storage.consolidation_queue_put(ctx, &ConsolidationStage::SkillCandidate, &entity).await?;
            report.skill_candidates += 1;
        }
    }

    Ok(report)
}
```

**Step 2: Add batch job subcommand**

```rust
"consolidate" => run_consolidation(&config).await,
```

**Step 3: Test**

Write mock storage test verifying stage transitions.

Run: `cargo test -p ferrosa-memory-core consolidation`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/ferrosa-memory-core/src/consolidation.rs crates/ferrosa-memory-core/src/lib.rs
git add crates/ferrosa-memory-batch/src/main.rs crates/ferrosa-memory-core/src/cql_storage.rs
git commit -m "feat(consolidation): automated fold→entity→skill promotion pipeline"
```

---

## Task 4: Contradiction Detection

**Objective:** Before writing a new temporal fact, check for conflicts with existing facts on the same entity.

**Files:**
- Create: `crates/ferrosa-memory-core/src/contradiction.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs` (add `pub mod contradiction;`)
- Modify: `crates/ferrosa-memory-core/src/temporal.rs` (call `contradiction::check_before_write`)
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs` (add contradiction prepared statements)

**Step 1: Write contradiction.rs**

```rust
//! Contradiction detection for temporal facts.
//!
//! Two facts contradict if:
//! - Same entity
//! - Similar semantic content (embedding cosine similarity > 0.85)
//! - Opposite polarity (detected via negation keywords: "not", "no longer", "deprecated")

use crate::storage::Storage;
use crate::types::{ContradictionEntry, TenantContext};
use uuid::Uuid;

/// Check if `new_fact` contradicts any existing fact for `entity_id`.
/// Returns `Ok(None)` if no contradiction, `Ok(Some(entry))` if found.
pub async fn check_before_write(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
    new_fact: &str,
) -> anyhow::Result<Option<ContradictionEntry>> {
    let chain = storage.get_temporal_chain(ctx, entity_id).await?;
    for old_fact in chain.facts {
        if is_contradiction(&old_fact.text, new_fact) {
            let entry = ContradictionEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                old_fact_hash: hash_fact(&old_fact.text),
                new_fact_hash: hash_fact(new_fact),
                old_fact_text: old_fact.text.clone(),
                new_fact_text: new_fact.to_string(),
                detected_at: chrono::Utc::now(),
                resolved_at: None,
                resolution: "pending".to_string(),
                resolver: "agent".to_string(),
            };
            storage.contradiction_put(ctx, &entry).await?;
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn is_contradiction(old: &str, new: &str) -> bool {
    // Simple heuristic: negation flip + high token overlap
    let old_negated = has_negation(old);
    let new_negated = has_negation(new);
    if old_negated == new_negated {
        return false; // same polarity
    }
    token_overlap(old, new) > 0.6
}

fn has_negation(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["not", "no longer", "deprecated", "removed", "false"]
        .iter()
        .any(|w| lower.contains(w))
}

fn token_overlap(a: &str, b: &str) -> f64 {
    let a_tokens: std::collections::HashSet<_> = a.split_whitespace().collect();
    let b_tokens: std::collections::HashSet<_> = b.split_whitespace().collect();
    let intersection = a_tokens.intersection(&b_tokens).count();
    let union = a_tokens.union(&b_tokens).count();
    intersection as f64 / union as f64
}

fn hash_fact(text: &str) -> String {
    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
}
```

**Step 2: Wire into `write_temporal_fact`**

In `temporal.rs`, before writing:
```rust
if let Some(contradiction) = contradiction::check_before_write(storage, ctx, entity_id, fact_text).await? {
    tracing::warn!(entity_id = %entity_id, "contradiction detected — fact queued for review");
    // Still write the fact (supersession will handle it), but flag it
}
```

**Step 3: Test**

```rust
#[tokio::test]
async fn test_contradiction_detection() {
    let storage = mock_storage();
    let ctx = TenantContext::default();
    let eid = Uuid::new_v4();

    // Write initial fact
    storage.write_temporal_fact(&ctx, eid, "Server uses port 8080").await?;

    // Contradictory fact should be detected
    let result = contradiction::check_before_write(&storage, &ctx, eid, "Server does not use port 8080").await?;
    assert!(result.is_some(), "negation flip + overlap → contradiction");

    // Non-contradictory fact should pass
    let result2 = contradiction::check_before_write(&storage, &ctx, eid, "Server uses TLS").await?;
    assert!(result2.is_none(), "different topic → no contradiction");
}
```

Run: `cargo test -p ferrosa-memory-core contradiction`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/ferrosa-memory-core/src/contradiction.rs crates/ferrosa-memory-core/src/lib.rs
git add crates/ferrosa-memory-core/src/temporal.rs crates/ferrosa-memory-core/src/cql_storage.rs
git commit -m "feat(contradiction): detect fact conflicts before write, queue for review"
```

---

## Task 5: Domain Schema Bundles

**Objective:** Bundle skills into versioned domain schemas that can be exported/shared.

**Files:**
- Modify: `crates/ferrosa-memory-core/src/skill.rs` (add `bundle_as_schema`)
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs` (add `mcp__fmem__export_schema` tool)
- Modify: `crates/ferrosa-memory-core/src/cql_storage.rs` (add schema prepared statements)

**Step 1: Add bundle_as_schema to skill.rs**

```rust
/// Bundle a set of skills into a versioned domain schema.
pub async fn bundle_as_schema(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    schema_name: &str,
    skill_names: &[String],
    description: &str,
) -> anyhow::Result<DomainSchema> {
    let schema_id = Uuid::new_v4();
    let version = 1; // TODO: bump if schema_name already exists

    let schema = DomainSchema {
        schema_id,
        schema_name: schema_name.to_string(),
        version,
        description: description.to_string(),
        skill_names: skill_names.to_vec(),
        routing_guidelines: generate_routing_guidelines(skill_names).await?,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    storage.domain_schema_put(ctx, &schema).await?;
    Ok(schema)
}
```

**Step 2: Add MCP tool**

In `dispatch.rs`, add:
```rust
/// Export a domain schema bundle.
async fn export_schema(
    args: &Value,
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
) -> Result<Value, (i32, String)> {
    let schema_name = require_string(args, "schema_name")?;
    let skill_names = require_string_array(args, "skill_names")?;
    let description = optional_string(args, "description").unwrap_or_default();

    let schema = skill::bundle_as_schema(storage, ctx, &schema_name, &skill_names, &description).await
        .map_err(|e| (-32603, format!("schema bundle failed: {e}")))?;

    Ok(json!({
        "schema_id": schema.schema_id,
        "version": schema.version,
        "skill_count": schema.skill_names.len(),
    }))
}
```

**Step 3: Test**

Run: `cargo test -p ferrosa-memory-core skill`
Expected: PASS (existing skill tests still pass)

**Step 4: Commit**

```bash
git add crates/ferrosa-memory-core/src/skill.rs crates/ferrosa-memory-core/src/dispatch.rs
git add crates/ferrosa-memory-core/src/cql_storage.rs
git commit -m "feat(schema-bundles): versioned domain schemas exportable as shareable packages"
```

---

## Task 6: Batch Job Integration

**Objective:** Wire all 5 gaps into a single nightly batch job.

**Files:**
- Modify: `crates/ferrosa-memory-batch/src/main.rs`

**Step 1: Update default run path**

Change the default subcommand to run all 5 operations in sequence:

```rust
_ => {
    // Run full KG evolution pipeline
    run_decay_and_forget(&config).await?;
    run_consolidation(&config).await?;
    run_guidelines(&config).await?; // existing
    Ok(())
}
```

**Step 2: Add cron entry example**

Document in README:
```bash
# Run nightly at 2 AM
0 2 * * * FERROSA_MEMORY_CONFIG=/etc/ferrosa-memory.toml /usr/local/bin/ferrosa-memory-batch
```

**Step 3: Commit**

```bash
git add crates/ferrosa-memory-batch/src/main.rs
 git commit -m "feat(batch): unified nightly pipeline — decay, consolidation, guideline refinement"
```

---

## Task 7: Integration Tests

**Objective:** End-to-end test of the full pipeline.

**Files:**
- Create: `crates/ferrosa-memory-core/tests/kg_evolution_e2e.rs`

**Step 1: Write E2E test**

```rust
//! E2E: full KG evolution pipeline
//!
//! 1. Create entity, write fact
//! 2. Verify confidence score computed
//! 3. Write contradictory fact, verify contradiction detected
//! 4. Let warmth decay, verify decay pass works
//! 5. Run consolidation, verify fold promoted to entity
//! 6. Bundle skills, verify schema exportable

#[tokio::test]
async fn kg_evolution_full_pipeline() {
    let cfg = test_config();
    let storage = CqlStorage::connect(&cfg).await.expect("connect");
    let ctx = TenantContext::default();

    // 1. Create entity + fact
    let eid = Uuid::new_v4();
    storage.smart_ingest(&ctx, "TestEntity", "entity", "test content").await.unwrap();

    // 2. Write temporal fact → confidence computed
    storage.write_temporal_fact(&ctx, eid, "Uses port 8080").await.unwrap();
    let confidence = storage.confidence_get(&ctx, eid, &hash_fact("Uses port 8080")).await.unwrap();
    assert!(confidence.confidence > 0.5);

    // 3. Contradictory fact → contradiction detected
    let result = contradiction::check_before_write(&storage, &ctx, eid, "Does not use port 8080").await.unwrap();
    assert!(result.is_some());

    // 4. Decay pass
    let pruned = warmth::prune_forgotten(&storage, &ctx, ctx.session_id, 0.01).await.unwrap();
    // (may be 0 if test is fast, but shouldn't error)

    // 5. Consolidation (requires fold setup)
    let report = consolidation::run_consolidation_cycle(&storage, &ctx, &ConsolidationConfig::default()).await.unwrap();
    // report values may be 0 in empty test, but shouldn't error

    // 6. Schema bundle
    let schema = skill::bundle_as_schema(&storage, &ctx, "test-domain", &["tdd".to_string()], "test").await.unwrap();
    assert_eq!(schema.version, 1);
}
```

**Step 2: Run E2E**

```bash
FERROSA_TEST_CQL_PORT=19542 cargo test -p ferrosa-memory-core --test kg_evolution_e2e -- --nocapture
```
Expected: PASS

**Step 3: Commit**

```bash
git add crates/ferrosa-memory-core/tests/kg_evolution_e2e.rs
git commit -m "test(kg-evolution): end-to-end pipeline integration test"
```

---

## Task 8: Documentation + PR

**Objective:** Document the new system and open a PR.

**Files:**
- Create: `specs/implemented/feat-kg-evolution.md`
- Modify: `README.md` (add "Knowledge Graph Evolution" section)

**Step 1: Write architecture spec**

Follow the existing spec format in `specs/implemented/` — include:
- Motivation (link to LLM Wiki V2 video)
- Architecture diagram (Mermaid)
- Data flow (raw → confidence → contradiction → decay → consolidation → schema)
- Configuration reference
- Testing notes

**Step 2: Open PR**

```bash
git push origin feat/kg-evolution
gh pr create --base feat/datalog-filter-grammar --title "feat(kg-evolution): confidence, decay, consolidation, contradiction, schema bundles" --body-file specs/implemented/feat-kg-evolution.md
```

---

## Summary: What Gets Built

| Component | New Files | Modified Files |
|-----------|-----------|----------------|
| DDLs | 4 `.cql` files | `migration.rs`, `types.rs` |
| Confidence | `confidence.rs` | `cql_storage.rs`, `temporal.rs` |
| Decay/Forget | — | `warmth.rs`, `config.rs`, `batch/main.rs` |
| Consolidation | `consolidation.rs` | `cql_storage.rs`, `batch/main.rs` |
| Contradiction | `contradiction.rs` | `cql_storage.rs`, `temporal.rs` |
| Schema Bundles | — | `skill.rs`, `dispatch.rs`, `cql_storage.rs` |
| Integration | `kg_evolution_e2e.rs` | `batch/main.rs` |
| Docs | `feat-kg-evolution.md` | `README.md` |

**Total:** ~8 new files, ~10 modified files, ~1,500 lines of Rust, 4 DDLs, 1 E2E test.

**Ready to execute?** Say "go" and I'll dispatch subagents task-by-task.
