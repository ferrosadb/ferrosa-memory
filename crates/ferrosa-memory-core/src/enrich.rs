//! Entity enrichment and graph lint — post-ingest LLM enhancement.
//!
//! Adds semantic descriptions to structurally-extracted code entities
//! using a local LLM (OpenAI-compatible API). Three operations:
//!
//! 1. **Enrich** — Generate 2-3 sentence descriptions for entities
//! 2. **Annotate** — Add relationship explanations to typed edges
//! 3. **Lint** — Detect structural issues in the knowledge graph

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{EntityEntry, TenantContext, TypedEdge};

const ENRICHED_PREFIX: &str = "[enriched] ";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Runtime configuration for a single enrichment run.
pub struct EnrichRunConfig {
    pub llm_base_url: String,
    pub llm_model: String,
    pub operations: Vec<String>,
    pub entity_type_filter: Option<Vec<String>>,
    pub force: bool,
    pub dry_run: bool,
    pub batch_size: usize,
    /// Embedding provider URL + model, used to generate `description_embedding`
    /// alongside each LLM-generated description. When unset (empty url),
    /// description is written without an embedding.
    #[doc(hidden)]
    pub ollama_base_url: String,
    pub embed_model: String,
    pub embed_dimensions: u32,
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct EnrichResult {
    pub entities_enriched: usize,
    pub entities_skipped: usize,
    pub edges_annotated: usize,
    pub edges_skipped: usize,
    pub lint_report: Option<LintReport>,
    pub errors: Vec<String>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct LintReport {
    pub total_entities: usize,
    pub total_edges: usize,
    pub enriched_count: usize,
    pub unenriched_count: usize,
    pub annotated_edges: usize,
    pub unannotated_edges: usize,
    pub findings: Vec<LintFinding>,
}

#[derive(Debug, Serialize)]
pub struct LintFinding {
    pub severity: LintSeverity,
    pub check: String,
    pub entity_name: Option<String>,
    pub message: String,
}

#[derive(Debug, Serialize, Clone, Copy)]
pub enum LintSeverity {
    Error,
    Warning,
    Info,
}

// ---------------------------------------------------------------------------
// LLM client (OpenAI-compatible)
// ---------------------------------------------------------------------------

struct EnrichLlm {
    http: reqwest::Client,
    base_url: String,
    model: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

impl EnrichLlm {
    fn new(base_url: &str, model: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
        }
    }

    async fn chat(&self, system: &str, user: &str, max_tokens: u32) -> anyhow::Result<String> {
        let body = serde_json::json!({
            "model": &self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.3,
            "max_tokens": max_tokens,
        });

        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM API error {status}: {text}");
        }

        let chat: ChatResponse = resp.json().await?;
        chat.choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("LLM returned no choices"))
    }
}

// ---------------------------------------------------------------------------
// Idempotency helpers
// ---------------------------------------------------------------------------

fn is_enriched(entity: &EntityEntry) -> bool {
    // New canonical signal: dedicated `description` field populated.
    // Legacy format (pre-Sprint 1): ENRICHED_PREFIX smashed into
    // context_snippet. Recognize both so re-runs skip correctly during
    // the migration window.
    entity.description.is_some() || entity.context_snippet.starts_with(ENRICHED_PREFIX)
}

fn is_edge_annotated(edge: &TypedEdge) -> bool {
    edge.metadata.as_ref().is_some_and(|m| !m.is_empty())
}

fn strip_enrichment(context: &str) -> &str {
    if let Some(rest) = context.strip_prefix(ENRICHED_PREFIX) {
        if let Some(pos) = rest.find("\n---\n") {
            &rest[pos + 5..]
        } else {
            rest
        }
    } else {
        context
    }
}

// ---------------------------------------------------------------------------
// Batching
// ---------------------------------------------------------------------------

struct EntityBatch {
    module_name: String,
    crate_name: String,
    entities: Vec<EntityEntry>,
    /// Edges where src or dst is in this batch.
    related_edges: Vec<TypedEdge>,
}

fn build_containment_batches(
    entities: &[EntityEntry],
    edges: &[TypedEdge],
    batch_size: usize,
) -> Vec<EntityBatch> {
    // Build a map: child_id → parent entity (via "contains" edges).
    let entity_map: HashMap<Uuid, &EntityEntry> =
        entities.iter().map(|e| (e.entity_id, e)).collect();

    let mut parent_of: HashMap<Uuid, Uuid> = HashMap::new();
    for edge in edges {
        if edge.edge_type == "contains" {
            parent_of.insert(edge.dst_id, edge.src_id);
        }
    }

    // Group entities by their nearest "module" or "crate" parent.
    let mut groups: HashMap<Uuid, Vec<EntityEntry>> = HashMap::new();
    let mut orphans: Vec<EntityEntry> = Vec::new();

    for entity in entities {
        if let Some(&parent_id) = parent_of.get(&entity.entity_id) {
            groups.entry(parent_id).or_default().push(entity.clone());
        } else {
            orphans.push(entity.clone());
        }
    }

    let mut batches = Vec::new();

    // Module-grouped batches.
    for (parent_id, members) in &groups {
        let parent = entity_map.get(parent_id);
        let module_name = parent.map(|p| p.entity_name.as_str()).unwrap_or("unknown");
        let crate_name = module_name.split("::").next().unwrap_or(module_name);

        // Split large modules into sub-batches.
        for chunk in members.chunks(batch_size) {
            let member_ids: HashSet<Uuid> = chunk.iter().map(|e| e.entity_id).collect();
            let related: Vec<TypedEdge> = edges
                .iter()
                .filter(|e| member_ids.contains(&e.src_id) || member_ids.contains(&e.dst_id))
                .cloned()
                .collect();

            batches.push(EntityBatch {
                module_name: module_name.to_string(),
                crate_name: crate_name.to_string(),
                entities: chunk.to_vec(),
                related_edges: related,
            });
        }
    }

    // Orphan batches (grouped by type).
    let mut orphan_by_type: HashMap<String, Vec<EntityEntry>> = HashMap::new();
    for entity in orphans {
        orphan_by_type
            .entry(entity.entity_type.clone())
            .or_default()
            .push(entity);
    }
    for (etype, members) in &orphan_by_type {
        for chunk in members.chunks(batch_size) {
            let member_ids: HashSet<Uuid> = chunk.iter().map(|e| e.entity_id).collect();
            let related: Vec<TypedEdge> = edges
                .iter()
                .filter(|e| member_ids.contains(&e.src_id) || member_ids.contains(&e.dst_id))
                .cloned()
                .collect();

            batches.push(EntityBatch {
                module_name: format!("(ungrouped {etype})"),
                crate_name: String::new(),
                entities: chunk.to_vec(),
                related_edges: related,
            });
        }
    }

    batches
}

// ---------------------------------------------------------------------------
// LLM prompt construction
// ---------------------------------------------------------------------------

fn build_enrich_prompt(batch: &EntityBatch, entity_map: &HashMap<Uuid, &EntityEntry>) -> String {
    let mut prompt = format!(
        "Module: {}\nParent crate: {}\n\nEntities to describe:\n",
        batch.module_name, batch.crate_name
    );

    for (i, entity) in batch.entities.iter().enumerate() {
        let context = strip_enrichment(&entity.context_snippet);
        let truncated = if context.len() > 300 {
            &context[..300]
        } else {
            context
        };

        // Summarize edges for this entity.
        let edge_summary: Vec<String> = batch
            .related_edges
            .iter()
            .filter(|e| e.src_id == entity.entity_id)
            .take(5)
            .map(|e| {
                let dst_name = entity_map
                    .get(&e.dst_id)
                    .map(|d| d.entity_name.as_str())
                    .unwrap_or("?");
                format!("{}({})", e.edge_type, dst_name)
            })
            .collect();

        let edges_str = if edge_summary.is_empty() {
            String::new()
        } else {
            format!("\n   Edges: {}", edge_summary.join(", "))
        };

        prompt.push_str(&format!(
            "{}. [{}] {} — context: \"{}\"{}\n",
            i + 1,
            entity.entity_type,
            entity.entity_name,
            truncated.replace('"', "'"),
            edges_str,
        ));
    }

    prompt.push_str(
        "\nRespond with a JSON array. Each element: {\"entity\": \"<name>\", \"description\": \"<2-3 sentences>\"}",
    );
    prompt
}

fn build_annotate_prompt(edges: &[TypedEdge], entity_map: &HashMap<Uuid, &EntityEntry>) -> String {
    let mut prompt = String::from("Edges to annotate:\n");

    for (i, edge) in edges.iter().enumerate() {
        let src_name = entity_map
            .get(&edge.src_id)
            .map(|e| e.entity_name.as_str())
            .unwrap_or("?");
        let dst_name = entity_map
            .get(&edge.dst_id)
            .map(|e| e.entity_name.as_str())
            .unwrap_or("?");
        let src_ctx = entity_map
            .get(&edge.src_id)
            .map(|e| {
                let c = strip_enrichment(&e.context_snippet);
                if c.len() > 100 { &c[..100] } else { c }
            })
            .unwrap_or("");
        let dst_ctx = entity_map
            .get(&edge.dst_id)
            .map(|e| {
                let c = strip_enrichment(&e.context_snippet);
                if c.len() > 100 { &c[..100] } else { c }
            })
            .unwrap_or("");

        prompt.push_str(&format!(
            "{}. {} --{}-- > {}\n   Src: \"{}\"\n   Dst: \"{}\"\n",
            i + 1,
            src_name,
            edge.edge_type,
            dst_name,
            src_ctx.replace('"', "'"),
            dst_ctx.replace('"', "'"),
        ));
    }

    prompt.push_str(
        "\nRespond with a JSON array. Each element: {\"edge_index\": <N>, \"annotation\": \"<1 sentence>\"}",
    );
    prompt
}

// ---------------------------------------------------------------------------
// LLM response parsing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct EntityEnrichment {
    entity: String,
    description: String,
}

#[derive(Deserialize)]
struct EdgeAnnotation {
    edge_index: usize,
    annotation: String,
}

/// Extract a JSON array from potentially noisy LLM output.
fn extract_json_array(raw: &str) -> Option<&str> {
    let start = raw.find('[')?;
    let mut depth = 0;
    for (i, ch) in raw[start..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Lint
// ---------------------------------------------------------------------------

pub fn run_lint(entities: &[EntityEntry], edges: &[TypedEdge]) -> LintReport {
    let entity_ids: HashSet<Uuid> = entities.iter().map(|e| e.entity_id).collect();
    let entity_names: HashSet<&str> = entities.iter().map(|e| e.entity_name.as_str()).collect();

    // Build adjacency set.
    let mut connected: HashSet<Uuid> = HashSet::new();
    for edge in edges {
        connected.insert(edge.src_id);
        connected.insert(edge.dst_id);
    }

    // Track containment targets.
    let contained: HashSet<Uuid> = edges
        .iter()
        .filter(|e| e.edge_type == "contains")
        .map(|e| e.dst_id)
        .collect();

    let enriched_count = entities.iter().filter(|e| is_enriched(e)).count();
    let annotated_count = edges.iter().filter(|e| is_edge_annotated(e)).count();

    let mut findings = Vec::new();

    // Orphan entities (no edges at all).
    for entity in entities {
        if !connected.contains(&entity.entity_id) {
            findings.push(LintFinding {
                severity: LintSeverity::Warning,
                check: "orphan_entity".into(),
                entity_name: Some(entity.entity_name.clone()),
                message: format!(
                    "{} '{}' has no edges",
                    entity.entity_type, entity.entity_name
                ),
            });
        }
    }

    // Missing containment (functions/structs without a parent module).
    for entity in entities {
        if matches!(
            entity.entity_type.as_str(),
            "function" | "struct" | "enum" | "trait"
        ) && !contained.contains(&entity.entity_id)
        {
            findings.push(LintFinding {
                severity: LintSeverity::Info,
                check: "missing_containment".into(),
                entity_name: Some(entity.entity_name.clone()),
                message: format!(
                    "{} '{}' has no parent module (no incoming 'contains' edge)",
                    entity.entity_type, entity.entity_name
                ),
            });
        }
    }

    // Dangling edges.
    for edge in edges {
        if !entity_ids.contains(&edge.src_id) {
            findings.push(LintFinding {
                severity: LintSeverity::Error,
                check: "dangling_edge".into(),
                entity_name: None,
                message: format!(
                    "Edge {} --{}-- > {} has unknown source",
                    edge.src_id, edge.edge_type, edge.dst_id
                ),
            });
        }
        if !entity_ids.contains(&edge.dst_id) {
            findings.push(LintFinding {
                severity: LintSeverity::Error,
                check: "dangling_edge".into(),
                entity_name: None,
                message: format!(
                    "Edge {} --{}-- > {} has unknown destination",
                    edge.src_id, edge.edge_type, edge.dst_id
                ),
            });
        }
    }

    // Duplicate entity names.
    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for entity in entities {
        *name_counts.entry(entity.entity_name.as_str()).or_default() += 1;
    }
    for (name, count) in &name_counts {
        if *count > 1 {
            findings.push(LintFinding {
                severity: LintSeverity::Warning,
                check: "duplicate_name".into(),
                entity_name: Some(name.to_string()),
                message: format!("Entity name '{}' appears {} times", name, count),
            });
        }
    }

    // Missing cross-references (doc sections mentioning entity names).
    let ref_targets: HashSet<(Uuid, Uuid)> = edges
        .iter()
        .filter(|e| e.edge_type == "references")
        .map(|e| (e.src_id, e.dst_id))
        .collect();

    for entity in entities {
        if entity.entity_type != "section" {
            continue;
        }
        let ctx = &entity.context_snippet;
        for name in &entity_names {
            if name.len() < 4 || *name == entity.entity_name.as_str() {
                continue;
            }
            // Simple word-boundary check.
            if ctx.contains(name) {
                let target = entities.iter().find(|e| e.entity_name == *name);
                if let Some(target) = target
                    && !ref_targets.contains(&(entity.entity_id, target.entity_id))
                {
                    findings.push(LintFinding {
                        severity: LintSeverity::Info,
                        check: "missing_cross_reference".into(),
                        entity_name: Some(entity.entity_name.clone()),
                        message: format!(
                            "Section '{}' mentions '{}' but has no 'references' edge",
                            entity.entity_name, name
                        ),
                    });
                }
            }
        }
    }

    // Cap findings to avoid overwhelming output.
    findings.truncate(200);

    LintReport {
        total_entities: entities.len(),
        total_edges: edges.len(),
        enriched_count,
        unenriched_count: entities.len() - enriched_count,
        annotated_edges: annotated_count,
        unannotated_edges: edges.len() - annotated_count,
        findings,
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub async fn run_enrichment(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    config: &EnrichRunConfig,
) -> anyhow::Result<EnrichResult> {
    let start = std::time::Instant::now();
    let mut result = EnrichResult {
        entities_enriched: 0,
        entities_skipped: 0,
        edges_annotated: 0,
        edges_skipped: 0,
        lint_report: None,
        errors: Vec::new(),
        elapsed_ms: 0,
    };

    let entities = storage.entity_list_session(ctx, session_id).await?;
    let edges = storage.typed_edge_list_session(ctx, session_id).await?;

    let entity_map: HashMap<Uuid, &EntityEntry> =
        entities.iter().map(|e| (e.entity_id, e)).collect();

    let do_enrich = config.operations.iter().any(|o| o == "enrich");
    let do_annotate = config.operations.iter().any(|o| o == "annotate");
    let do_lint = config.operations.iter().any(|o| o == "lint");

    // --- Enrich entities ---
    if do_enrich && !config.dry_run {
        let llm = EnrichLlm::new(&config.llm_base_url, &config.llm_model);

        let system_prompt = "You are a code documentation expert. Given a group of code entities \
            from the same module, write a concise 2-3 sentence description for each entity \
            explaining WHAT it does, WHY it exists, and HOW it fits into the larger system. \
            Be specific and technical. Do not repeat the entity name or type in the description.";

        // Filter entities.
        let candidates: Vec<&EntityEntry> = entities
            .iter()
            .filter(|e| {
                if !config.force && is_enriched(e) {
                    return false;
                }
                if let Some(ref filter) = config.entity_type_filter {
                    return filter.iter().any(|f| f == &e.entity_type);
                }
                true
            })
            .collect();

        let skipped = entities.len() - candidates.len();
        result.entities_skipped = skipped;

        // Build filtered entity list for batching.
        let candidate_entries: Vec<EntityEntry> = candidates.iter().map(|e| (*e).clone()).collect();
        let batches = build_containment_batches(&candidate_entries, &edges, config.batch_size);

        tracing::info!(
            batches = batches.len(),
            candidates = candidate_entries.len(),
            "enrichment: starting entity enrichment"
        );

        // Optional embedding client for populating description_embedding
        // alongside each LLM-generated description.
        let embed_client = if !config.ollama_base_url.is_empty()
            && !config.embed_model.is_empty()
        {
            Some(crate::embedding::EmbeddingClient::new(
                &crate::config::EmbeddingConfig {
                    provider: "ollama".into(),
                    ollama_base_url: config.ollama_base_url.clone(),
                    model: config.embed_model.clone(),
                    dimensions: config.embed_dimensions,
                    ner_model: String::new(),
                },
            ))
        } else {
            None
        };

        for (batch_idx, batch) in batches.iter().enumerate() {
            let user_prompt = build_enrich_prompt(batch, &entity_map);

            match llm.chat(system_prompt, &user_prompt, 2048).await {
                Ok(response) => {
                    if let Some(json_str) = extract_json_array(&response) {
                        match serde_json::from_str::<Vec<EntityEnrichment>>(json_str) {
                            Ok(enrichments) => {
                                for enrichment in &enrichments {
                                    if let Some(entity) = batch
                                        .entities
                                        .iter()
                                        .find(|e| e.entity_name == enrichment.entity)
                                    {
                                        // Generate description_embedding if a provider is
                                        // configured. Falls back to None on error — we still
                                        // want the description written even if embedding fails.
                                        let desc_embedding = match &embed_client {
                                            Some(c) => match c.embed(&enrichment.description).await {
                                                Ok(v) => Some(v),
                                                Err(e) => {
                                                    tracing::debug!(
                                                        entity = %entity.entity_name,
                                                        error = %e,
                                                        "description embedding generation skipped"
                                                    );
                                                    None
                                                }
                                            },
                                            None => None,
                                        };

                                        let now = chrono::Utc::now();
                                        // Write to the dedicated description field (Sprint 1).
                                        // Leave context_snippet alone — it's the raw
                                        // extraction source, not a retrieval signal.
                                        let enriched = EntityEntry {
                                            description: Some(enrichment.description.clone()),
                                            description_embedding: desc_embedding,
                                            updated_at: Some(now),
                                            ..entity.clone()
                                        };
                                        if let Err(e) = storage.entity_put(ctx, &enriched).await {
                                            result.errors.push(format!(
                                                "entity_put {}: {e}",
                                                entity.entity_name
                                            ));
                                        } else {
                                            result.entities_enriched += 1;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                result
                                    .errors
                                    .push(format!("batch {batch_idx} JSON parse error: {e}"));
                            }
                        }
                    } else {
                        result
                            .errors
                            .push(format!("batch {batch_idx}: no JSON array in LLM response"));
                    }
                }
                Err(e) => {
                    result
                        .errors
                        .push(format!("batch {batch_idx} LLM error: {e}"));
                }
            }

            if (batch_idx + 1) % 10 == 0 {
                tracing::info!(
                    batch = batch_idx + 1,
                    total = batches.len(),
                    enriched = result.entities_enriched,
                    "enrichment: progress"
                );
            }
        }
    }

    // --- Annotate edges ---
    if do_annotate && !config.dry_run {
        let llm = EnrichLlm::new(&config.llm_base_url, &config.llm_model);

        let system_prompt = "You are a code architecture expert. For each relationship between \
            code entities, write a brief explanation (1 sentence) of WHY this relationship \
            exists — not just restating the edge type, but explaining the architectural reason.";

        let candidates: Vec<&TypedEdge> = edges
            .iter()
            .filter(|e| config.force || !is_edge_annotated(e))
            .collect();

        result.edges_skipped = edges.len() - candidates.len();

        tracing::info!(
            candidates = candidates.len(),
            "enrichment: starting edge annotation"
        );

        for chunk in candidates.chunks(config.batch_size) {
            let edge_refs: Vec<TypedEdge> = chunk.iter().map(|e| (*e).clone()).collect();
            let user_prompt = build_annotate_prompt(&edge_refs, &entity_map);

            match llm.chat(system_prompt, &user_prompt, 2048).await {
                Ok(response) => {
                    if let Some(json_str) = extract_json_array(&response) {
                        match serde_json::from_str::<Vec<EdgeAnnotation>>(json_str) {
                            Ok(annotations) => {
                                for ann in &annotations {
                                    if ann.edge_index >= 1 && ann.edge_index <= edge_refs.len() {
                                        let edge = &edge_refs[ann.edge_index - 1];
                                        let annotated = TypedEdge {
                                            metadata: Some(ann.annotation.clone()),
                                            created_at: chrono::Utc::now(),
                                            ..edge.clone()
                                        };
                                        if let Err(e) =
                                            storage.typed_edge_put(ctx, &annotated).await
                                        {
                                            result.errors.push(format!("typed_edge_put: {e}"));
                                        } else {
                                            result.edges_annotated += 1;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                result.errors.push(format!("edge JSON parse: {e}"));
                            }
                        }
                    }
                }
                Err(e) => {
                    result.errors.push(format!("edge LLM error: {e}"));
                }
            }
        }
    }

    // --- Lint ---
    if do_lint {
        // Re-fetch if enrichment changed data, otherwise use cached.
        let (lint_entities, lint_edges) = if do_enrich && !config.dry_run {
            let e = storage.entity_list_session(ctx, session_id).await?;
            let ed = storage.typed_edge_list_session(ctx, session_id).await?;
            (e, ed)
        } else {
            (entities, edges)
        };
        result.lint_report = Some(run_lint(&lint_entities, &lint_edges));
    }

    result.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enriched_prefix_roundtrip() {
        let original = "struct `Foo` @ src/lib.rs:42";
        let enriched = format!("{ENRICHED_PREFIX}Foo manages bar state.\n---\n{original}");
        assert!(is_enriched(&EntityEntry {
            tenant_id: Uuid::nil(),
            entity_id: Uuid::nil(),
            session_id: Uuid::nil(),
            entity_name: "Foo".into(),
            entity_type: "struct".into(),
            source_fold_id: None,
            context_snippet: enriched.clone(),
            entity_embedding: None,
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        }));
        assert_eq!(strip_enrichment(&enriched), original);
    }

    #[test]
    fn strip_unenriched_is_noop() {
        let ctx = "struct `Foo` @ src/lib.rs:42";
        assert_eq!(strip_enrichment(ctx), ctx);
    }

    #[test]
    fn is_enriched_recognizes_new_description_field() {
        // Sprint 1: the canonical enriched signal is a populated
        // description field. Legacy ENRICHED_PREFIX in context_snippet is
        // also accepted (transition window until backfill runs).
        let new_format = EntityEntry {
            tenant_id: Uuid::nil(),
            entity_id: Uuid::nil(),
            session_id: Uuid::nil(),
            entity_name: "Foo".into(),
            entity_type: "struct".into(),
            source_fold_id: None,
            context_snippet: "struct `Foo` @ src/lib.rs:42".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            description: Some("Foo manages bar state.".into()),
            ..Default::default()
        };
        assert!(is_enriched(&new_format));
    }

    #[test]
    fn is_enriched_false_on_plain_entity() {
        let plain = EntityEntry {
            tenant_id: Uuid::nil(),
            entity_id: Uuid::nil(),
            session_id: Uuid::nil(),
            entity_name: "Foo".into(),
            entity_type: "concept".into(),
            source_fold_id: None,
            context_snippet: "plain source text".into(),
            entity_embedding: None,
            confidence: 1.0,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        assert!(!is_enriched(&plain));
    }

    #[test]
    fn extract_json_array_from_noisy_output() {
        let raw = r#"Here is the result:
[{"entity": "Foo", "description": "bar"}]
Done."#;
        let arr = extract_json_array(raw).unwrap();
        assert!(arr.starts_with('['));
        assert!(arr.ends_with(']'));
        let parsed: Vec<EntityEnrichment> = serde_json::from_str(arr).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].entity, "Foo");
    }

    #[test]
    fn lint_detects_orphans() {
        let entities = vec![EntityEntry {
            tenant_id: Uuid::nil(),
            entity_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            entity_name: "orphan".into(),
            entity_type: "function".into(),
            source_fold_id: None,
            context_snippet: "test".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: crate::types::MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        }];
        let report = run_lint(&entities, &[]);
        assert!(report.findings.iter().any(|f| f.check == "orphan_entity"));
    }

    #[test]
    fn lint_detects_missing_containment() {
        let fn_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let entities = vec![
            EntityEntry {
                tenant_id: Uuid::nil(),
                entity_id: fn_id,
                session_id: Uuid::nil(),
                entity_name: "my_func".into(),
                entity_type: "function".into(),
                source_fold_id: None,
                context_snippet: "test".into(),
                entity_embedding: None,
                confidence: 0.9,
                state: crate::types::MemoryState::Active,
                created_at: chrono::Utc::now(),
                ..Default::default()
            },
            EntityEntry {
                tenant_id: Uuid::nil(),
                entity_id: other_id,
                session_id: Uuid::nil(),
                entity_name: "other".into(),
                entity_type: "concept".into(),
                source_fold_id: None,
                context_snippet: "test".into(),
                entity_embedding: None,
                confidence: 0.9,
                state: crate::types::MemoryState::Active,
                created_at: chrono::Utc::now(),
                ..Default::default()
            },
        ];
        // Edge connects them but not via "contains".
        let edges = vec![TypedEdge {
            tenant_id: Uuid::nil(),
            session_id: Uuid::nil(),
            src_id: other_id,
            edge_type: "references".into(),
            dst_id: fn_id,
            weight: 1.0,
            metadata: None,
            created_at: chrono::Utc::now(),
        }];
        let report = run_lint(&entities, &edges);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.check == "missing_containment")
        );
    }
}
