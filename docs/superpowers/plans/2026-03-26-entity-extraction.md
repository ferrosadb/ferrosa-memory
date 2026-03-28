# Three-Tier Entity Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the 8-word content truncation in `smart_ingest` with a three-tier entity name extraction chain: explicit name → LLM extraction → heuristic fallback.

**Architecture:** Add an optional `entity_name` param to the `smart_ingest` tool and function. When absent, call a new `ner::extract_entity_from_content()` function that tries LLM extraction via Ollama, falls back to heuristic NER, and finally to the current truncation. The LLM also infers entity type when the caller passed "concept". A batch `rename-entities` command fixes existing data.

**Tech Stack:** Rust, Tokio, reqwest (HTTP client for Ollama), serde_json (JSON parsing), cdrs-tokio (CQL storage)

---

### File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/ferrosa-memory-core/src/config.rs` | Modify | Add `ner_model` field to `EmbeddingConfig` |
| `crates/ferrosa-memory-core/src/ner.rs` | Modify | Add `extract_entity_from_content()` — the three-tier chain |
| `crates/ferrosa-memory-core/src/smart_ingest.rs` | Modify | Add `entity_name` param, wire up extraction |
| `crates/ferrosa-memory-core/src/dispatch.rs` | Modify | Parse `entity_name` from args, pass NER config to handler |
| `crates/ferrosa-memory-batch/src/main.rs` | Modify | Add `rename-entities` subcommand |

---

### Task 1: Add `ner_model` to Config

**Files:**
- Modify: `crates/ferrosa-memory-core/src/config.rs:178-198`

- [ ] **Step 1: Write the failing test**

In `config.rs`, inside the existing `#[cfg(test)] mod tests` block, add:

```rust
#[test]
fn embedding_config_has_ner_model_default() {
    let config = EmbeddingConfig::default();
    assert_eq!(config.ner_model, "qwen3.5:27b");
}

#[test]
fn parse_toml_with_ner_model() {
    let toml_str = r#"
[server]
transport = "stdio"

[ferrosa]
contact_points = ["localhost:9042"]

[embeddings]
ner_model = "llama3:8b"
"#;
    let config = parse_config(toml_str).unwrap();
    assert_eq!(config.embeddings.ner_model, "llama3:8b");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib -p ferrosa-memory-core -- config::tests::embedding_config_has_ner_model`
Expected: FAIL — `EmbeddingConfig` has no field `ner_model`

- [ ] **Step 3: Add `ner_model` field to `EmbeddingConfig`**

In `config.rs`, add the field and default:

```rust
// Add to EmbeddingConfig struct (after dimensions field):
#[serde(default = "default_ner_model")]
pub ner_model: String,

// Add to Default impl:
ner_model: default_ner_model(),

// Add the default function near other defaults:
fn default_ner_model() -> String {
    "qwen3.5:27b".to_string()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib -p ferrosa-memory-core -- config::tests`
Expected: All config tests PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/config.rs
git commit -m "feat: add ner_model config field (default qwen3.5:27b)"
```

---

### Task 2: Add `extract_entity_from_content` to `ner.rs`

**Files:**
- Modify: `crates/ferrosa-memory-core/src/ner.rs`

- [ ] **Step 1: Write failing tests**

Add to the existing `#[cfg(test)] mod tests` block in `ner.rs`:

```rust
#[test]
fn parse_llm_json_response_extracts_name_and_type() {
    let json = r#"{"name": "Ben Kearns", "type": "person"}"#;
    let (name, etype) = parse_extraction_response(json);
    assert_eq!(name, "Ben Kearns");
    assert_eq!(etype, "person");
}

#[test]
fn parse_llm_json_response_garbage_returns_none() {
    let (name, etype) = parse_extraction_response("not json at all");
    assert!(name.is_empty());
    assert_eq!(etype, "concept");
}

#[test]
fn parse_llm_json_response_missing_name_returns_empty() {
    let json = r#"{"type": "person"}"#;
    let (name, etype) = parse_extraction_response(json);
    assert!(name.is_empty());
    assert_eq!(etype, "person");
}

#[test]
fn heuristic_extraction_finds_capitalized_entity() {
    let (name, etype) = heuristic_extract_entity("Ben Kearns built Ferrosa from scratch");
    assert_eq!(name, "Ben Kearns");
    assert_eq!(etype, "person");
}

#[test]
fn heuristic_extraction_falls_back_to_truncation() {
    let (name, etype) = heuristic_extract_entity("everything is lowercase no entities here at all today");
    // Falls back to first 8 words
    assert_eq!(name, "everything is lowercase no entities here at all");
    assert_eq!(etype, "concept");
}

#[test]
fn type_override_only_for_concept() {
    assert_eq!(apply_type_override("concept", "person"), "person");
    assert_eq!(apply_type_override("person", "tool"), "person");
    assert_eq!(apply_type_override("org", "concept"), "org");
}

#[tokio::test]
async fn extract_entity_from_content_uses_heuristic_when_llm_down() {
    let http = reqwest::Client::new();
    let (name, etype) = extract_entity_from_content(
        &http,
        "http://invalid:99999",
        "fake-model",
        "The project called Docker is widely used",
        "concept",
    )
    .await;
    assert_eq!(name, "Docker");
    assert_eq!(etype, "tool");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib -p ferrosa-memory-core -- ner::tests::parse_llm_json`
Expected: FAIL — `parse_extraction_response` not found

- [ ] **Step 3: Implement the extraction functions**

Add to `ner.rs` (above the existing `classify_entity` function):

```rust
/// Parse the LLM's JSON response into (name, type).
/// Returns empty name and "concept" type on parse failure.
pub fn parse_extraction_response(raw: &str) -> (String, String) {
    #[derive(Deserialize)]
    struct ExtractionResponse {
        name: Option<String>,
        #[serde(rename = "type")]
        entity_type: Option<String>,
    }

    let trimmed = raw.trim();
    // Try to find JSON in the response (LLM may add surrounding text)
    let json_start = trimmed.find('{');
    let json_end = trimmed.rfind('}');

    let json_str = match (json_start, json_end) {
        (Some(s), Some(e)) if e > s => &trimmed[s..=e],
        _ => return (String::new(), "concept".to_string()),
    };

    match serde_json::from_str::<ExtractionResponse>(json_str) {
        Ok(resp) => {
            let name = resp.name.unwrap_or_default().trim().to_string();
            let etype = resp
                .entity_type
                .map(|t| t.trim().to_lowercase())
                .filter(|t| VALID_TYPES.contains(&t.as_str()))
                .unwrap_or_else(|| "concept".to_string());
            (name, etype)
        }
        Err(_) => (String::new(), "concept".to_string()),
    }
}

/// Apply the type override rule: only override when caller said "concept".
pub fn apply_type_override(caller_type: &str, extracted_type: &str) -> String {
    if caller_type == "concept" {
        extracted_type.to_string()
    } else {
        caller_type.to_string()
    }
}

/// Heuristic entity extraction from content text.
/// Uses `extract_entity_candidates` for capitalized phrases, falls back
/// to first-8-words truncation.
pub fn heuristic_extract_entity(content: &str) -> (String, String) {
    let candidates = crate::smart_ingest::extract_entity_candidates(content);
    if let Some((name, etype)) = candidates.into_iter().next() {
        (name, etype)
    } else {
        let truncated: String = content
            .split_whitespace()
            .take(8)
            .collect::<Vec<_>>()
            .join(" ");
        let etype = crate::smart_ingest::infer_entity_type(&truncated);
        (truncated, etype.to_string())
    }
}

/// LLM-based entity extraction from content text.
async fn llm_extract_entity(
    http: &reqwest::Client,
    ollama_base_url: &str,
    model: &str,
    content: &str,
) -> anyhow::Result<(String, String)> {
    let prompt = format!(
        "/no_think\nExtract the primary named entity from this text.\n\
         Return JSON: {{\"name\": \"...\", \"type\": \"person|org|tool|project|place|event|concept|decision|pattern|preference\"}}\n\
         Text: {content}\n\
         Reply with ONLY the JSON, nothing else."
    );

    let body = serde_json::json!({
        "model": model,
        "prompt": prompt,
        "stream": false,
        "options": {
            "temperature": 0.0,
            "num_predict": 60
        }
    });

    let resp = http
        .post(format!("{ollama_base_url}/api/generate"))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Ollama returned {}", resp.status());
    }

    let parsed: OllamaGenerateResponse = resp.json().await?;
    let (name, etype) = parse_extraction_response(&parsed.response);

    if name.is_empty() {
        anyhow::bail!("LLM returned empty entity name");
    }

    Ok((name, etype))
}

/// Three-tier entity extraction from content.
///
/// 1. LLM extraction via Ollama (returns name + type)
/// 2. Heuristic extraction (capitalized phrases + `infer_entity_type`)
/// 3. First 8 words truncation (last resort)
///
/// The `caller_type` is used for the override rule: extracted type only
/// replaces "concept", explicit types are preserved.
pub async fn extract_entity_from_content(
    http: &reqwest::Client,
    ollama_base_url: &str,
    model: &str,
    content: &str,
    caller_type: &str,
) -> (String, String) {
    // Tier 2: LLM extraction
    match llm_extract_entity(http, ollama_base_url, model, content).await {
        Ok((name, extracted_type)) => {
            let final_type = apply_type_override(caller_type, &extracted_type);
            tracing::info!(name = %name, extracted_type, final_type, "LLM entity extraction succeeded");
            return (name, final_type);
        }
        Err(e) => {
            tracing::debug!(error = %e, "LLM entity extraction failed, using heuristic");
        }
    }

    // Tier 3: Heuristic extraction
    let (name, extracted_type) = heuristic_extract_entity(content);
    let final_type = apply_type_override(caller_type, &extracted_type);
    (name, final_type)
}
```

Also update the module doc at the top of `ner.rs`:
```rust
//! Named entity recognition — extraction and classification via Ollama.
//!
//! Three-tier entity extraction from content:
//! 1. Explicit name (caller provides `entity_name`)
//! 2. LLM extraction via Ollama (returns name + type)
//! 3. Heuristic fallback (capitalized phrases + `infer_entity_type`)
//!
//! Also provides standalone classification for existing entity names.
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib -p ferrosa-memory-core -- ner::tests`
Expected: All ner tests PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/ner.rs
git commit -m "feat: add three-tier entity extraction to ner module"
```

---

### Task 3: Wire `entity_name` into `smart_ingest`

**Files:**
- Modify: `crates/ferrosa-memory-core/src/smart_ingest.rs:64-73` (function signature)
- Modify: `crates/ferrosa-memory-core/src/smart_ingest.rs:93-118` (Created path)
- Modify: `crates/ferrosa-memory-core/src/smart_ingest.rs:152-172` (Superseded path)
- Modify: `crates/ferrosa-memory-core/src/fold.rs:129-143` (call site)

- [ ] **Step 1: Write failing test**

Add to the existing `#[cfg(test)] mod tests` block in `smart_ingest.rs`:

```rust
#[tokio::test]
async fn smart_ingest_uses_explicit_entity_name() {
    use crate::storage::mock::MockStorage;

    let store = MockStorage::new();
    let ctx = TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "test".into(),
    };

    let result = smart_ingest(
        &store,
        &ctx,
        Uuid::new_v4(),
        "Ben Kearns is the developer of ferrosa-memory-mcp and has ops background",
        "person",
        None,
        None,
        &IngestConfig::default(),
        Some("Ben Kearns"),
        None, // no NER config — should use explicit name
    )
    .await
    .unwrap();

    assert!(matches!(result, IngestDecision::Created { .. }));

    // Verify the stored entity has the clean name
    let entities = store.entities.lock().await;
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].entity_name, "Ben Kearns");
}

#[tokio::test]
async fn smart_ingest_without_name_falls_back_to_heuristic() {
    use crate::storage::mock::MockStorage;

    let store = MockStorage::new();
    let ctx = TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "test".into(),
    };

    // No entity_name, no NER config (LLM unavailable) — should use heuristic
    let result = smart_ingest(
        &store,
        &ctx,
        Uuid::new_v4(),
        "The project called Docker is widely used in production",
        "concept",
        None,
        None,
        &IngestConfig::default(),
        None,  // no explicit name
        None,  // no NER config — heuristic fallback
    )
    .await
    .unwrap();

    assert!(matches!(result, IngestDecision::Created { .. }));

    let entities = store.entities.lock().await;
    assert_eq!(entities.len(), 1);
    // Heuristic should extract "Docker" from the content
    assert_eq!(entities[0].entity_name, "Docker");
    // Type should be overridden from "concept" to "tool"
    assert_eq!(entities[0].entity_type, "tool");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib -p ferrosa-memory-core -- smart_ingest::tests::smart_ingest_uses_explicit`
Expected: FAIL — wrong number of arguments to `smart_ingest`

- [ ] **Step 3: Update `smart_ingest` function signature and body**

Update the function signature in `smart_ingest.rs`:

```rust
/// NER configuration for LLM-based entity extraction.
pub struct NerConfig {
    pub http: reqwest::Client,
    pub ollama_base_url: String,
    pub model: String,
}

pub async fn smart_ingest(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    content: &str,
    entity_type: &str,
    embedding: Option<&[f32]>,
    source_fold_id: Option<Uuid>,
    config: &IngestConfig,
    entity_name: Option<&str>,       // NEW: explicit name (tier 1)
    ner_config: Option<&NerConfig>,   // NEW: LLM config for tier 2
) -> anyhow::Result<IngestDecision> {
```

Replace the entity name derivation in the Created path (line ~100):

```rust
    // Resolve entity name via three-tier chain
    let (resolved_name, resolved_type) = resolve_entity_name(
        entity_name,
        content,
        entity_type,
        ner_config,
    )
    .await;
```

Add the `resolve_entity_name` helper function:

```rust
/// Three-tier entity name resolution.
/// Tier 1: explicit name → Tier 2: LLM extraction → Tier 3: heuristic
async fn resolve_entity_name(
    explicit_name: Option<&str>,
    content: &str,
    caller_type: &str,
    ner_config: Option<&NerConfig>,
) -> (String, String) {
    // Tier 1: explicit name provided
    if let Some(name) = explicit_name {
        if !name.trim().is_empty() {
            return (name.trim().to_string(), caller_type.to_string());
        }
    }

    // Tier 2+3: LLM extraction with heuristic fallback
    if let Some(ner) = ner_config {
        return crate::ner::extract_entity_from_content(
            &ner.http,
            &ner.ollama_base_url,
            &ner.model,
            content,
            caller_type,
        )
        .await;
    }

    // No NER config — heuristic only
    crate::ner::heuristic_extract_entity(content)
}
```

Use `resolved_name` and `resolved_type` in both the Created and Superseded `EntityEntry` blocks, replacing the old `content.split_whitespace().take(8)` pattern.

- [ ] **Step 4: Fix the `fold.rs` call site**

In `fold.rs:132`, the call to `smart_ingest` needs the two new params. Since fold auto-extraction already provides a clean entity name via `extract_entity_candidates`, pass it explicitly:

```rust
let _ = crate::smart_ingest::smart_ingest(
    storage,
    ctx,
    session_id,
    &name,
    &entity_type,
    None,
    Some(fold_id),
    &crate::smart_ingest::IngestConfig::default(),
    Some(&name),  // explicit name from extract_entity_candidates
    None,         // no NER needed — name is already clean
)
.await;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib -p ferrosa-memory-core -- smart_ingest::tests`
Expected: All smart_ingest tests PASS (existing tests may need the two new `None` params appended)

- [ ] **Step 6: Fix existing tests**

All existing `smart_ingest()` call sites in tests need `None, None` appended for the two new params. Search for `smart_ingest(` in test code and update each call.

- [ ] **Step 7: Run full test suite**

Run: `cargo test --workspace --lib`
Expected: All tests PASS

- [ ] **Step 8: Commit**

```bash
git add crates/ferrosa-memory-core/src/smart_ingest.rs crates/ferrosa-memory-core/src/fold.rs
git commit -m "feat: add entity_name param to smart_ingest with three-tier resolution"
```

---

### Task 4: Update `dispatch.rs` — Tool Schema and Handler

**Files:**
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs:342-356` (tool schema)
- Modify: `crates/ferrosa-memory-core/src/dispatch.rs:1357-1381` (handler)

- [ ] **Step 1: Update the tool schema**

In the `smart_ingest` `ToolDef` properties (line ~347), add `entity_name`:

```rust
"entity_name": {
    "type": "string",
    "maxLength": 256,
    "description": "Clean entity name (e.g. 'Ben Kearns', 'Ferrosa'). If omitted, extracted automatically from content via LLM or heuristic."
},
```

- [ ] **Step 2: Update `handle_smart_ingest` to parse and pass the new field**

In `handle_smart_ingest` (line ~1363), add:

```rust
let entity_name = args
    .get("entity_name")
    .and_then(|v| v.as_str())
    .map(|s| s.to_string());
```

Build the NER config from the session's embedding config. Add `embedding_config: Option<Arc<EmbeddingConfig>>` to `SessionState`, or pass it through the dispatch function. The simplest approach: construct the `NerConfig` inline in the handler using config values from the dispatch context.

Update the `smart_ingest` call to pass the new params:

```rust
let ner_config = crate::smart_ingest::NerConfig {
    http: reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default(),
    ollama_base_url: session.ollama_base_url.clone(),
    model: session.ner_model.clone(),
};

let decision = crate::smart_ingest::smart_ingest(
    storage,
    ctx,
    session_id,
    content,
    entity_type,
    embedding.as_deref(),
    source_fold_id,
    &config,
    entity_name.as_deref(),
    Some(&ner_config),
)
.await
.map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
```

Also update the viz event to use the resolved entity name (from the decision result) instead of truncated content for the label.

- [ ] **Step 3: Add `ollama_base_url` and `ner_model` to `SessionState`**

In `dispatch.rs` `SessionState` struct, add:

```rust
pub ollama_base_url: String,
pub ner_model: String,
```

Update the `Default` impl:

```rust
ollama_base_url: "http://localhost:11434".to_string(),
ner_model: "qwen3.5:27b".to_string(),
```

Update the `SessionState` construction in `main.rs` to pass these from config.

- [ ] **Step 4: Run full test suite**

Run: `cargo test --workspace --lib`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/dispatch.rs crates/ferrosa-memory-mcp/src/main.rs
git commit -m "feat: wire entity_name into smart_ingest tool schema and dispatch"
```

---

### Task 5: Add `rename-entities` Batch Command

**Files:**
- Modify: `crates/ferrosa-memory-batch/src/main.rs`

- [ ] **Step 1: Add the subcommand routing**

In `main.rs`, add to the match:

```rust
"rename-entities" => rename_entities(&config).await,
```

- [ ] **Step 2: Implement `rename_entities`**

```rust
/// Re-extract entity names using three-tier NER for entities with
/// sentence-fragment names (>5 words).
async fn rename_entities(config: &ferrosa_memory_core::config::Config) -> anyhow::Result<()> {
    let tenant_id = config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("no tenant_id configured in [server]"))?;

    let ctx = TenantContext {
        tenant_id,
        session_origin: "batch-rename".into(),
    };

    let storage = CqlStorage::connect(&config.ferrosa).await?;
    tracing::info!("connected to CQL cluster");

    let entities = storage.entity_list_all(&ctx).await?;
    let fragment_count = entities
        .iter()
        .filter(|e| e.entity_name.split_whitespace().count() > 5)
        .count();
    tracing::info!(
        total = entities.len(),
        fragments = fragment_count,
        "loaded entities"
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let ollama_url = &config.embeddings.ollama_base_url;
    let ner_model = &config.embeddings.ner_model;

    let mut renamed = 0;
    let mut skipped = 0;

    for entity in &entities {
        let word_count = entity.entity_name.split_whitespace().count();
        if word_count <= 5 {
            skipped += 1;
            continue;
        }

        let (new_name, new_type) = ferrosa_memory_core::ner::extract_entity_from_content(
            &http,
            ollama_url,
            ner_model,
            &entity.context_snippet,
            &entity.entity_type,
        )
        .await;

        if new_name == entity.entity_name && new_type == entity.entity_type {
            skipped += 1;
            continue;
        }

        let mut updated = entity.clone();
        updated.entity_name = new_name.clone();
        updated.entity_type = new_type.clone();
        storage.entity_put(&ctx, &updated).await?;

        tracing::info!(
            old_name = %entity.entity_name.chars().take(50).collect::<String>(),
            new_name = %new_name,
            old_type = %entity.entity_type,
            new_type = %new_type,
            "renamed entity"
        );
        renamed += 1;
    }

    tracing::info!(renamed, skipped, "rename complete");
    Ok(())
}
```

- [ ] **Step 3: Run build**

Run: `cargo build --workspace`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/ferrosa-memory-batch/src/main.rs
git commit -m "feat: add rename-entities batch command for fixing sentence-fragment names"
```

---

### Task 6: Full CI Validation

**Files:** None (validation only)

- [ ] **Step 1: Format check**

Run: `cargo fmt --check`
Expected: PASS

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, 0 warnings

- [ ] **Step 3: Tests**

Run: `cargo test --workspace --lib`
Expected: All tests PASS

- [ ] **Step 4: Coverage**

Run:
```bash
EXCL='(tests/|test_|_test\.rs|mock|cql_storage\.rs|main\.rs|http\.rs|graph\.rs|embedding\.rs)'
cargo llvm-cov --workspace --ignore-filename-regex "$EXCL" --json 2>/dev/null | \
  python3 -c "import json,sys; d=json.load(sys.stdin); print(f\"Coverage: {d['data'][0]['totals']['lines']['percent']:.1f}%\")"
```
Expected: ≥80%

- [ ] **Step 5: Docs**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items`
Expected: PASS

- [ ] **Step 6: Push and create PR**

```bash
git push -u origin feature/entity-extraction
gh pr create --title "feat: three-tier entity name extraction" --body "..."
```
