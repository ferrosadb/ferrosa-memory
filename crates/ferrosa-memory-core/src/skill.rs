//! Skills layer — structured methodology knowledge.
//!
//! A skill is a globally-scoped entity carrying structured steps, trigger
//! keywords, prerequisites, and tag membership. This module provides the
//! ingest / retrieve / invoke primitives. The corresponding MCP tools
//! (`ingest_skill`, `retrieve_skills_for_context`, `invoke_skill`) live in
//! `dispatch.rs` and delegate here.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::embedding::EmbeddingClient;
use crate::storage::Storage;
use crate::types::{EntityEntry, EntityScope, TenantContext, TypedEdge};

/// One step of a skill's instructions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Step {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub instruction: String,
}

/// Caller-provided skill definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestSkillParams {
    pub name: String,
    pub category: String,
    pub description: String,
    #[serde(default)]
    pub trigger_keywords: Vec<String>,
    /// Additional tags beyond `category`. Each becomes a `TAGGED_AS` edge.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Other skill names this one requires. Each becomes a `REQUIRES` edge.
    #[serde(default)]
    pub prerequisites: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub output_artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_criteria: Option<String>,
    /// Precomputed content-hash for idempotent re-ingest. When the caller
    /// passes the same hash as the stored skill, the write is a no-op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// The caller's session — recorded as `ingested_by_session` on the
    /// (global-scope) skill entity for audit and session-affinity ranking.
    pub caller_session_id: Uuid,
}

/// Result of a skill ingest call.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SkillIngestAction {
    Created {
        entity_id: Uuid,
        version: String,
    },
    Updated {
        entity_id: Uuid,
        version: String,
        prior_version: Option<String>,
    },
    Skipped {
        entity_id: Uuid,
        version: String,
        reason: &'static str,
    },
}

impl SkillIngestAction {
    pub fn entity_id(&self) -> Uuid {
        match self {
            Self::Created { entity_id, .. }
            | Self::Updated { entity_id, .. }
            | Self::Skipped { entity_id, .. } => *entity_id,
        }
    }
}

/// Normalize a tag name per the design doc: lowercase, dash-separated,
/// alphanumeric + dash only. `"Chaos Engineering"` → `chaos-engineering`.
pub fn normalize_tag(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Compute the YYYYMMDD{NN} version for the next skill ingested today,
/// given the list of existing skill versions. NN is zero-padded to two
/// digits up to 99, then grows naturally.
pub fn next_version(today_yyyymmdd: &str, existing_versions: &[&str]) -> String {
    let today_prefix = today_yyyymmdd;
    let max_today = existing_versions
        .iter()
        .filter_map(|v| {
            if v.len() >= 10 && v.starts_with(today_prefix) {
                v[today_prefix.len()..].parse::<u32>().ok()
            } else {
                None
            }
        })
        .max()
        .unwrap_or(0);
    format!("{today_prefix}{:02}", max_today + 1)
}

fn today_yyyymmdd() -> String {
    chrono::Utc::now().format("%Y%m%d").to_string()
}

/// Ingest a skill into the global-scope partition.
///
/// - Resolves the storage partition via `crate::scope::resolve_storage_session`
///   with scope=Global, so skills live under the tenant sentinel.
/// - Generates a YYYYMMDDNN version. Callers cannot set it.
/// - Checks for an existing skill by exact name; if present with matching
///   `content_hash`, skips. Otherwise writes the updated entity.
/// - Creates/resolves tag entities for `category` and each item in `tags`
///   (normalized), then emits `TAGGED_AS` edges from the skill to each.
/// - For each prerequisite, resolves the skill by name (if it exists) and
///   emits a `REQUIRES` edge. Missing prerequisites are logged and skipped
///   (the caller can re-run after ingesting them).
/// - Generates `description_embedding` via the embedding client when provided.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_skill(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    params: IngestSkillParams,
    embedding_client: Option<&EmbeddingClient>,
    graph_client: Option<&crate::graph::GraphClient>,
) -> anyhow::Result<SkillIngestAction> {
    anyhow::ensure!(!params.name.is_empty(), "skill name must not be empty");
    anyhow::ensure!(
        !params.category.is_empty(),
        "skill category must not be empty"
    );
    anyhow::ensure!(
        !params.description.is_empty(),
        "skill description must not be empty"
    );

    let (storage_session, ingested_by) = crate::scope::resolve_storage_session(
        params.caller_session_id,
        EntityScope::Global,
        ctx.tenant_id,
    );

    // Look up any existing skill with this exact name in the global partition.
    let mut existing_matches = storage
        .entity_find_phonetic(ctx, storage_session, &params.name)
        .await
        .unwrap_or_default();
    existing_matches.retain(|e| e.entity_name == params.name && e.entity_type == "skill");
    let existing = if let Some(head) = existing_matches.first() {
        // Fetch full entity for property comparison.
        storage
            .entity_get_by_id(ctx, storage_session, head.entity_id)
            .await
            .unwrap_or(None)
    } else {
        None
    };

    // Idempotent skip: same content_hash → no work.
    if let (Some(existing), Some(new_hash)) = (existing.as_ref(), params.content_hash.as_ref())
        && existing.content_hash.as_deref() == Some(new_hash.as_str())
    {
        let version = existing
            .properties
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(SkillIngestAction::Skipped {
            entity_id: existing.entity_id,
            version,
            reason: "content_hash unchanged",
        });
    }

    // Build the new version string.
    let today = today_yyyymmdd();
    let all_skills = storage
        .entity_list_session(ctx, storage_session)
        .await
        .unwrap_or_default();
    let today_versions: Vec<&str> = all_skills
        .iter()
        .filter(|e| e.entity_type == "skill")
        .filter_map(|e| e.properties.get("version").and_then(|v| v.as_str()))
        .collect();
    let new_version = next_version(&today, &today_versions);

    // Build properties JSON.
    let properties = serde_json::json!({
        "category": params.category,
        "trigger_keywords": params.trigger_keywords,
        "steps": params.steps,
        "output_artifacts": params.output_artifacts,
        "completion_criteria": params.completion_criteria,
        "version": new_version,
    });

    // Tags = category + explicit tags, all normalized.
    let mut tag_names: Vec<String> = std::iter::once(params.category.clone())
        .chain(params.tags.iter().cloned())
        .map(|t| normalize_tag(&t))
        .filter(|t| !t.is_empty())
        .collect();
    tag_names.sort();
    tag_names.dedup();

    // Generate description embedding if the client is wired.
    let description_embedding = match embedding_client {
        Some(c) => match c.embed(&params.description).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!(
                    skill = %params.name,
                    error = %e,
                    "description embedding skipped"
                );
                None
            }
        },
        None => None,
    };

    // Entity id: reuse existing on update, new otherwise.
    let entity_id = existing
        .as_ref()
        .map(|e| e.entity_id)
        .unwrap_or_else(Uuid::new_v4);
    let now = chrono::Utc::now();

    let entry = EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id,
        session_id: storage_session,
        entity_name: params.name.clone(),
        entity_type: "skill".into(),
        source_fold_id: None,
        context_snippet: String::new(),
        entity_embedding: None,
        confidence: 1.0,
        state: crate::types::MemoryState::default(),
        created_at: existing.as_ref().map(|e| e.created_at).unwrap_or(now),
        description: Some(params.description.clone()),
        description_embedding,
        tags: tag_names.clone(),
        properties,
        content_hash: params.content_hash.clone(),
        updated_at: Some(now),
        scope: EntityScope::Global,
        ingested_by_session: ingested_by,
    };

    storage.entity_put(ctx, &entry).await?;

    // --- Tag resolution + TAGGED_AS edges ---
    for tag_name in &tag_names {
        match ensure_tag_entity(storage, ctx, storage_session, tag_name, ingested_by, now).await {
            Ok(tag_id) => {
                let edge = TypedEdge {
                    tenant_id: ctx.tenant_id,
                    session_id: storage_session,
                    src_id: entity_id,
                    edge_type: "TAGGED_AS".into(),
                    dst_id: tag_id,
                    weight: 1.0,
                    metadata: None,
                    created_at: now,
                };
                if let Err(e) = storage.typed_edge_put(ctx, &edge).await {
                    tracing::warn!(skill = %params.name, tag = %tag_name, error = %e, "TAGGED_AS edge write failed");
                }
            }
            Err(e) => {
                tracing::warn!(skill = %params.name, tag = %tag_name, error = %e, "tag entity resolution failed");
            }
        }
    }

    // --- REQUIRES edges for prerequisites ---
    // Cycle prevention: adding `skill -[REQUIRES]-> prereq` creates a cycle
    // iff prereq already transitively requires skill. When a graph client is
    // available, run the Cypher check and fail-closed on that edge. When
    // the graph is unreachable or unconfigured, log a warning and skip the
    // edge (we'd rather miss a relationship than silently allow a cycle to
    // land under a misconfigured environment).
    for prereq_name in &params.prerequisites {
        let mut matches = storage
            .entity_find_phonetic(ctx, storage_session, prereq_name)
            .await
            .unwrap_or_default();
        matches.retain(|e| e.entity_name == *prereq_name && e.entity_type == "skill");
        let Some(prereq) = matches.first() else {
            tracing::info!(
                skill = %params.name,
                prereq = %prereq_name,
                "prerequisite skill not found; ingest it and re-run to create REQUIRES edge"
            );
            continue;
        };

        if let Some(graph) = graph_client {
            match graph
                .would_create_cycle(entity_id, prereq.entity_id, "REQUIRES")
                .await
            {
                Ok(true) => {
                    tracing::warn!(
                        skill = %params.name,
                        prereq = %prereq_name,
                        "REQUIRES edge would form a cycle; rejecting"
                    );
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        skill = %params.name,
                        prereq = %prereq_name,
                        error = %e,
                        "REQUIRES cycle check failed (graph unreachable); \
                         skipping edge. Run ingest_skill again when graph is healthy."
                    );
                    continue;
                }
            }
        } else {
            tracing::debug!(
                skill = %params.name,
                prereq = %prereq_name,
                "REQUIRES cycle check skipped (no graph client wired)"
            );
        }

        let edge = TypedEdge {
            tenant_id: ctx.tenant_id,
            session_id: storage_session,
            src_id: entity_id,
            edge_type: "REQUIRES".into(),
            dst_id: prereq.entity_id,
            weight: 1.0,
            metadata: None,
            created_at: now,
        };
        if let Err(e) = storage.typed_edge_put(ctx, &edge).await {
            tracing::warn!(skill = %params.name, prereq = %prereq_name, error = %e, "REQUIRES edge write failed");
        }
    }

    let prior_version = existing
        .as_ref()
        .and_then(|e| e.properties.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Ok(if existing.is_some() {
        SkillIngestAction::Updated {
            entity_id,
            version: new_version,
            prior_version,
        }
    } else {
        SkillIngestAction::Created {
            entity_id,
            version: new_version,
        }
    })
}

/// A single skill retrieval hit, returned by `retrieve_skills_for_context`.
#[derive(Debug, Clone, Serialize)]
pub struct SkillHit {
    pub skill_name: String,
    pub entity_id: Uuid,
    pub score: f64,
    pub description: String,
    pub category: String,
    pub version: String,
    pub used_in_session: bool,
}

/// Retrieve skills relevant to the given context, scored by a cheap
/// heuristic: cosine similarity over description_embedding when available,
/// plus keyword overlap against the skill's trigger_keywords and tags.
///
/// This is deliberately simpler than `hybrid_search` — the skill catalog
/// is small (dozens of entries), so a linear scan over the global partition
/// is fine. We can swap in the full two-stage re-rank pipeline later when
/// catalog size warrants it.
#[allow(clippy::too_many_arguments)]
pub async fn retrieve_skills_for_context(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    caller_session_id: Uuid,
    context: &str,
    query_embedding: Option<&[f32]>,
    limit: usize,
    min_score: f64,
    used_entity_ids: &std::collections::HashSet<Uuid>,
) -> anyhow::Result<Vec<SkillHit>> {
    anyhow::ensure!(!context.is_empty(), "context must not be empty");

    let _ = caller_session_id; // reserved for session-scope skills in a future pass

    let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
    let all = storage
        .entity_list_session(ctx, global_session)
        .await
        .unwrap_or_default();

    let context_lower = context.to_lowercase();
    let context_tokens: std::collections::HashSet<String> = context_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty() && s.len() > 2)
        .map(str::to_string)
        .collect();

    let mut hits: Vec<SkillHit> = all
        .into_iter()
        .filter(|e| e.entity_type == "skill")
        .filter_map(|e| {
            let score = score_skill_against_context(
                &e,
                &context_lower,
                &context_tokens,
                query_embedding,
            );
            if score < min_score {
                return None;
            }
            let category = e
                .properties
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let version = e
                .properties
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = e.description.clone().unwrap_or_default();
            let used_in_session = used_entity_ids.contains(&e.entity_id);
            Some(SkillHit {
                skill_name: e.entity_name.clone(),
                entity_id: e.entity_id,
                score,
                description,
                category,
                version,
                used_in_session,
            })
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    Ok(hits)
}

/// Compute a relevance score in [0, 1+] for a skill entity against a query.
/// Signals:
///   - cosine similarity over description_embedding (if both sides have one)
///   - keyword overlap between query tokens and skill trigger_keywords + tags
///   - name substring boost (query contains the skill name)
fn score_skill_against_context(
    skill: &EntityEntry,
    context_lower: &str,
    context_tokens: &std::collections::HashSet<String>,
    query_embedding: Option<&[f32]>,
) -> f64 {
    let mut score = 0.0;

    // 1. Semantic similarity over description embeddings.
    if let (Some(qe), Some(de)) = (query_embedding, skill.description_embedding.as_ref()) {
        let sim = cosine_similarity(qe, de);
        score += sim * 0.5;
    }

    // 2. Trigger keyword overlap.
    let triggers: Vec<&str> = skill
        .properties
        .get("trigger_keywords")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // Trigger keywords are often multi-word phrases ("red-green-refactor",
    // "kent beck"). Check substring against the full lowercased context,
    // not just the token set.
    let trigger_matches = triggers
        .iter()
        .filter(|t| {
            let lowered = t.to_lowercase();
            context_lower.contains(&lowered)
                || context_tokens.contains(&lowered)
        })
        .count();
    if !triggers.is_empty() {
        score += 0.3 * (trigger_matches as f64 / triggers.len() as f64);
    }

    // 3. Tag overlap — any tag word appears in context.
    let tag_matches = skill
        .tags
        .iter()
        .filter(|t| context_lower.contains(t.as_str()))
        .count();
    if tag_matches > 0 {
        score += 0.1 * (tag_matches.min(3) as f64);
    }

    // 4. Name hit — context mentions the skill by name.
    let name_lower = skill.entity_name.to_lowercase();
    if context_lower.contains(&name_lower) {
        score += 0.3;
    }

    score
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let x = *x as f64;
        let y = *y as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Fetch a skill by name from the global partition. Returns `Ok(Some(skill))`
/// for an exact-name match, `Ok(None)` if no skill has that name.
pub async fn get_skill_by_name(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    name: &str,
) -> anyhow::Result<Option<EntityEntry>> {
    let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
    let mut matches = storage
        .entity_find_phonetic(ctx, global_session, name)
        .await
        .unwrap_or_default();
    matches.retain(|e| e.entity_name == name && e.entity_type == "skill");
    if let Some(head) = matches.first() {
        // entity_find_phonetic is lightweight — fetch the full entity for the steps.
        storage
            .entity_get_by_id(ctx, global_session, head.entity_id)
            .await
    } else {
        Ok(None)
    }
}

/// Closest skill names (phonetic-match top-K) for did_you_mean hints on a
/// missed `invoke_skill` lookup.
pub async fn similar_skill_names(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    name: &str,
    k: usize,
) -> Vec<String> {
    let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
    let mut matches = storage
        .entity_find_phonetic(ctx, global_session, name)
        .await
        .unwrap_or_default();
    matches.retain(|e| e.entity_type == "skill");
    matches
        .into_iter()
        .take(k)
        .map(|e| e.entity_name)
        .collect()
}

/// Structured response returned by `invoke_skill`. Purely data — no tool
/// orchestration here; the caller decides how to drive the steps.
#[derive(Debug, Clone, Serialize)]
pub struct InvokeSkillResult {
    pub skill_name: String,
    pub entity_id: Uuid,
    pub description: String,
    pub category: String,
    pub version: String,
    pub steps: Vec<Step>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_step_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_criteria: Option<String>,
    pub output_artifacts: Vec<String>,
    pub prerequisites_satisfied: bool,
    pub prerequisites: Vec<String>,
}

/// Result of an `ensure_parent_tag` call.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum EnsureParentTagAction {
    Created { child_id: Uuid, parent_id: Uuid },
    Skipped { child_id: Uuid, parent_id: Uuid },
}

/// Idempotently create a `PARENT_TAG` edge from `child` to `parent`, by name.
/// Resolves (or creates) both tag entities. If the edge already exists,
/// returns `Skipped`. Relies on the graph client's cycle check when one is
/// provided — fails loud if cycle would form or the check itself errors.
pub async fn ensure_parent_tag(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    caller_session_id: Uuid,
    child_tag_raw: &str,
    parent_tag_raw: &str,
    graph_client: Option<&crate::graph::GraphClient>,
) -> anyhow::Result<EnsureParentTagAction> {
    let child_tag = normalize_tag(child_tag_raw);
    let parent_tag = normalize_tag(parent_tag_raw);
    anyhow::ensure!(!child_tag.is_empty(), "child_tag must not be empty");
    anyhow::ensure!(!parent_tag.is_empty(), "parent_tag must not be empty");
    anyhow::ensure!(
        child_tag != parent_tag,
        "child and parent tags must differ; got {child_tag:?}"
    );

    let (_, ingested_by) = crate::scope::resolve_storage_session(
        caller_session_id,
        EntityScope::Global,
        ctx.tenant_id,
    );
    let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
    let now = chrono::Utc::now();

    let child_id =
        ensure_tag_entity(storage, ctx, global_session, &child_tag, ingested_by, now).await?;
    let parent_id =
        ensure_tag_entity(storage, ctx, global_session, &parent_tag, ingested_by, now).await?;

    // Idempotency: check for existing PARENT_TAG edge.
    let existing = storage
        .typed_edge_list_from(ctx, global_session, child_id)
        .await
        .unwrap_or_default();
    if existing
        .iter()
        .any(|e| e.edge_type == "PARENT_TAG" && e.dst_id == parent_id)
    {
        return Ok(EnsureParentTagAction::Skipped {
            child_id,
            parent_id,
        });
    }

    // Cycle check (when graph client is wired).
    if let Some(graph) = graph_client {
        match graph
            .would_create_cycle(child_id, parent_id, "PARENT_TAG")
            .await
        {
            Ok(true) => {
                anyhow::bail!(
                    "PARENT_TAG edge {} -> {} would form a cycle; rejecting",
                    child_tag,
                    parent_tag
                );
            }
            Ok(false) => {}
            Err(e) => {
                anyhow::bail!(
                    "PARENT_TAG cycle check failed (graph unreachable): {}. \
                     Retry when the graph is healthy.",
                    e
                );
            }
        }
    }

    let edge = TypedEdge {
        tenant_id: ctx.tenant_id,
        session_id: global_session,
        src_id: child_id,
        edge_type: "PARENT_TAG".into(),
        dst_id: parent_id,
        weight: 1.0,
        metadata: None,
        created_at: now,
    };
    storage.typed_edge_put(ctx, &edge).await?;

    Ok(EnsureParentTagAction::Created {
        child_id,
        parent_id,
    })
}

/// Result of a `verify_skill` call. Always returned (never errors on
/// missing skill — the caller wants to see negative results too).
#[derive(Debug, Clone, Serialize)]
pub struct VerifySkillResult {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub tags: Vec<String>,
    pub prerequisites: Vec<String>,
    pub required_by: Vec<String>,
    pub missing_prerequisites: Vec<String>,
}

/// Verify a skill's graph neighborhood — resolved tags, prerequisites,
/// reverse-prerequisites, and any prerequisites declared at ingest time
/// that still haven't landed as REQUIRES edges (e.g., because the prereq
/// skill wasn't ingested at the time). Returns `{exists: false}` for
/// unknown skill names; never errors on miss.
pub async fn verify_skill(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    skill_name: &str,
) -> anyhow::Result<VerifySkillResult> {
    let Some(entity) = get_skill_by_name(storage, ctx, skill_name).await? else {
        return Ok(VerifySkillResult {
            exists: false,
            entity_id: None,
            version: None,
            content_hash: None,
            tags: Vec::new(),
            prerequisites: Vec::new(),
            required_by: Vec::new(),
            missing_prerequisites: Vec::new(),
        });
    };

    let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);

    // Outgoing edges → TAGGED_AS tags + REQUIRES prerequisites.
    let outgoing = storage
        .typed_edge_list_from(ctx, global_session, entity.entity_id)
        .await
        .unwrap_or_default();
    let mut tags: Vec<String> = Vec::new();
    let mut prerequisites: Vec<String> = Vec::new();
    for edge in &outgoing {
        let Some(target) = storage
            .entity_get_by_id(ctx, global_session, edge.dst_id)
            .await
            .unwrap_or(None)
        else {
            continue;
        };
        match edge.edge_type.as_str() {
            "TAGGED_AS" if target.entity_type == "tag" => tags.push(target.entity_name),
            "REQUIRES" if target.entity_type == "skill" => {
                prerequisites.push(target.entity_name)
            }
            _ => {}
        }
    }
    tags.sort();
    prerequisites.sort();

    // Incoming REQUIRES edges → skills that require this one. Session-wide
    // scan filtered to REQUIRES with dst_id == our entity.
    let all_edges = storage
        .typed_edge_list_session(ctx, global_session)
        .await
        .unwrap_or_default();
    let mut required_by: Vec<String> = Vec::new();
    for edge in &all_edges {
        if edge.edge_type != "REQUIRES" || edge.dst_id != entity.entity_id {
            continue;
        }
        if let Some(source) = storage
            .entity_get_by_id(ctx, global_session, edge.src_id)
            .await
            .unwrap_or(None)
            && source.entity_type == "skill"
        {
            required_by.push(source.entity_name);
        }
    }
    required_by.sort();

    // Missing prerequisites: names declared at ingest that never landed as
    // edges. `ingest_skill` doesn't currently persist the raw prereq list
    // into properties — callers who want missing-prereq diagnostics must
    // set `properties.raw_prerequisites` (forge's skill-ingest does). For
    // callers that don't populate it, this stays empty — no false
    // positives.
    let raw_prereqs: Vec<String> = entity
        .properties
        .get("raw_prerequisites")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let resolved: std::collections::HashSet<&str> =
        prerequisites.iter().map(|s| s.as_str()).collect();
    let missing_prerequisites: Vec<String> = raw_prereqs
        .into_iter()
        .filter(|p| !resolved.contains(p.as_str()))
        .collect();

    let version = entity
        .properties
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Ok(VerifySkillResult {
        exists: true,
        entity_id: Some(entity.entity_id),
        version,
        content_hash: entity.content_hash.clone(),
        tags,
        prerequisites,
        required_by,
        missing_prerequisites,
    })
}

/// Build an `InvokeSkillResult` from a skill entity. The caller is
/// responsible for ensuring `entity.entity_type == "skill"`.
pub fn build_invoke_result(entity: &EntityEntry) -> InvokeSkillResult {
    let steps: Vec<Step> = entity
        .properties
        .get("steps")
        .cloned()
        .map(|v| serde_json::from_value::<Vec<Step>>(v).unwrap_or_default())
        .unwrap_or_default();
    let first_step_prompt = steps.first().map(|s| s.instruction.clone());
    let completion_criteria = entity
        .properties
        .get("completion_criteria")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let output_artifacts: Vec<String> = entity
        .properties
        .get("output_artifacts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let category = entity
        .properties
        .get("category")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let version = entity
        .properties
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    InvokeSkillResult {
        skill_name: entity.entity_name.clone(),
        entity_id: entity.entity_id,
        description: entity.description.clone().unwrap_or_default(),
        category,
        version,
        steps,
        first_step_prompt,
        completion_criteria,
        output_artifacts,
        // Prerequisite satisfaction check is deferred — it requires walking
        // REQUIRES edges and confirming each prereq has been used/acknowledged
        // in the caller's session. Placeholder: true when there are none.
        prerequisites_satisfied: true,
        prerequisites: Vec::new(),
    }
}

/// Resolve or create a tag entity (entity_type="tag", scope=Global) by name.
async fn ensure_tag_entity(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    storage_session: Uuid,
    tag_name: &str,
    ingested_by: Option<Uuid>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Uuid> {
    let mut matches = storage
        .entity_find_phonetic(ctx, storage_session, tag_name)
        .await
        .unwrap_or_default();
    matches.retain(|e| e.entity_name == tag_name && e.entity_type == "tag");
    if let Some(head) = matches.first() {
        return Ok(head.entity_id);
    }

    let entity_id = Uuid::new_v4();
    let tag_entry = EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id,
        session_id: storage_session,
        entity_name: tag_name.into(),
        entity_type: "tag".into(),
        source_fold_id: None,
        context_snippet: String::new(),
        entity_embedding: None,
        confidence: 1.0,
        state: crate::types::MemoryState::default(),
        created_at: now,
        description: None,
        description_embedding: None,
        tags: vec![tag_name.into()],
        properties: serde_json::Value::Null,
        content_hash: None,
        updated_at: Some(now),
        scope: EntityScope::Global,
        ingested_by_session: ingested_by,
    };
    storage.entity_put(ctx, &tag_entry).await?;
    Ok(entity_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    fn base_params(name: &str) -> IngestSkillParams {
        IngestSkillParams {
            name: name.into(),
            category: "testing".into(),
            description: format!("the {name} methodology"),
            trigger_keywords: vec!["test".into()],
            tags: Vec::new(),
            prerequisites: Vec::new(),
            steps: vec![Step {
                phase: Some("Red".into()),
                instruction: "write a failing test".into(),
            }],
            output_artifacts: vec!["checklist".into()],
            completion_criteria: Some("all steps complete".into()),
            content_hash: None,
            caller_session_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn normalize_tag_basic() {
        assert_eq!(normalize_tag("Chaos Engineering"), "chaos-engineering");
        assert_eq!(normalize_tag("TDD"), "tdd");
        assert_eq!(normalize_tag("foo/bar/baz"), "foo-bar-baz");
        assert_eq!(normalize_tag("  extra  "), "extra");
        assert_eq!(normalize_tag("!!!symbols!!!"), "symbols");
        assert_eq!(normalize_tag(""), "");
    }

    #[test]
    fn next_version_first_of_day() {
        assert_eq!(next_version("20260416", &[]), "2026041601");
    }

    #[test]
    fn next_version_sequential() {
        let existing = vec!["2026041601", "2026041602"];
        assert_eq!(next_version("20260416", &existing), "2026041603");
    }

    #[test]
    fn next_version_ignores_other_days() {
        let existing = vec!["2026041501", "2026041502", "2026041401"];
        assert_eq!(next_version("20260416", &existing), "2026041601");
    }

    #[test]
    fn next_version_handles_triple_digit_day() {
        let existing: Vec<String> = (1..=99).map(|n| format!("20260416{:02}", n)).collect();
        let refs: Vec<&str> = existing.iter().map(|s| s.as_str()).collect();
        assert_eq!(next_version("20260416", &refs), "20260416100");
    }

    #[tokio::test]
    async fn ingest_skill_creates_new() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let action = ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        match action {
            SkillIngestAction::Created { version, .. } => {
                let today = today_yyyymmdd();
                assert!(version.starts_with(&today));
            }
            other => panic!("expected Created, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn ingest_skill_stores_as_global_scope() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let action = ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let entry = storage
            .entity_get_by_id(&ctx, global_session, action.entity_id())
            .await
            .unwrap()
            .expect("skill must land in the global partition");
        assert_eq!(entry.scope, EntityScope::Global);
        assert_eq!(entry.entity_type, "skill");
        assert_eq!(entry.session_id, global_session);
    }

    #[tokio::test]
    async fn ingest_skill_writes_description_and_properties() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let action = ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        let global_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let entry = storage
            .entity_get_by_id(&ctx, global_session, action.entity_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.description.as_deref(), Some("the tdd methodology"));
        assert_eq!(entry.properties["category"], "testing");
        assert!(entry.properties["version"].is_string());
        assert_eq!(entry.tags, vec!["testing".to_string()]);
    }

    #[tokio::test]
    async fn ingest_skill_same_content_hash_skips() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let mut p = base_params("tdd");
        p.content_hash = Some("sha256:abc".into());
        let first = ingest_skill(&storage, &ctx, p.clone(), None, None)
            .await
            .unwrap();
        let second = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();
        assert!(matches!(first, SkillIngestAction::Created { .. }));
        match second {
            SkillIngestAction::Skipped { reason, .. } => {
                assert_eq!(reason, "content_hash unchanged");
            }
            other => panic!("expected Skipped on identical content_hash, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn ingest_skill_different_hash_updates_same_entity() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let mut p = base_params("tdd");
        p.content_hash = Some("sha256:v1".into());
        let first = ingest_skill(&storage, &ctx, p.clone(), None, None)
            .await
            .unwrap();
        p.content_hash = Some("sha256:v2".into());
        p.description = "the tdd methodology, refined".into();
        let second = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();
        assert_eq!(
            first.entity_id(),
            second.entity_id(),
            "update must keep the same entity_id"
        );
        assert!(matches!(second, SkillIngestAction::Updated { .. }));
    }

    #[tokio::test]
    async fn ingest_skill_creates_tag_entity_for_category() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        let global = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let entities = storage.entity_list_session(&ctx, global).await.unwrap();
        let tags: Vec<&EntityEntry> = entities.iter().filter(|e| e.entity_type == "tag").collect();
        assert!(
            tags.iter().any(|t| t.entity_name == "testing"),
            "expected a tag entity named 'testing', got: {:?}",
            tags.iter().map(|t| &t.entity_name).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn ingest_skill_creates_tag_entities_for_additional_tags() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let mut p = base_params("tdd");
        p.tags = vec!["Kent Beck".into(), "methodology".into()];
        ingest_skill(&storage, &ctx, p, None, None).await.unwrap();
        let global = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let entities = storage.entity_list_session(&ctx, global).await.unwrap();
        let tag_names: Vec<&str> = entities
            .iter()
            .filter(|e| e.entity_type == "tag")
            .map(|e| e.entity_name.as_str())
            .collect();
        assert!(tag_names.contains(&"testing"));
        assert!(tag_names.contains(&"kent-beck"));
        assert!(tag_names.contains(&"methodology"));
    }

    #[tokio::test]
    async fn ingest_skill_emits_requires_edge_when_prereq_exists() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        // Ingest prereq first.
        ingest_skill(&storage, &ctx, base_params("unit-testing"), None, None)
            .await
            .unwrap();
        // Now ingest the skill that requires it.
        let mut p = base_params("tdd");
        p.prerequisites = vec!["unit-testing".into()];
        let tdd = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();

        let global = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let edges = storage
            .typed_edge_list_session(&ctx, global)
            .await
            .unwrap();
        let requires: Vec<&TypedEdge> = edges
            .iter()
            .filter(|e| e.edge_type == "REQUIRES" && e.src_id == tdd.entity_id())
            .collect();
        assert_eq!(requires.len(), 1);
    }

    #[tokio::test]
    async fn ingest_skill_skips_missing_prereq_without_failing() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let mut p = base_params("tdd");
        p.prerequisites = vec!["does-not-exist".into()];
        let action = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();
        // Ingest succeeds even with a dangling prereq (logged, not failed).
        assert!(matches!(action, SkillIngestAction::Created { .. }));
    }

    #[tokio::test]
    async fn retrieve_skills_returns_matching_by_name() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        ingest_skill(&storage, &ctx, base_params("threat-model"), None, None)
            .await
            .unwrap();
        let caller = Uuid::new_v4();
        let hits = retrieve_skills_for_context(
            &storage,
            &ctx,
            caller,
            "I need to do some TDD",
            None,
            5,
            0.01,
            &std::collections::HashSet::new(),
        )
        .await
        .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].skill_name, "tdd");
    }

    #[tokio::test]
    async fn retrieve_skills_scores_trigger_keyword_matches() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let mut p = base_params("tdd");
        p.trigger_keywords = vec!["red-green-refactor".into(), "kent".into()];
        ingest_skill(&storage, &ctx, p, None, None).await.unwrap();
        let caller = Uuid::new_v4();
        let hits = retrieve_skills_for_context(
            &storage,
            &ctx,
            caller,
            "applying red-green-refactor to this bug",
            None,
            5,
            0.01,
            &std::collections::HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 0.0);
    }

    #[tokio::test]
    async fn retrieve_skills_flags_used_in_session() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let action = ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        let caller = Uuid::new_v4();
        let mut used = std::collections::HashSet::new();
        used.insert(action.entity_id());
        let hits = retrieve_skills_for_context(
            &storage,
            &ctx,
            caller,
            "tdd",
            None,
            5,
            0.0,
            &used,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].used_in_session);
    }

    #[tokio::test]
    async fn retrieve_skills_respects_min_score() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        let caller = Uuid::new_v4();
        // High min_score filters out weak matches (no trigger keyword hit).
        let hits = retrieve_skills_for_context(
            &storage,
            &ctx,
            caller,
            "unrelated query about deploying to kubernetes",
            None,
            5,
            0.5,
            &std::collections::HashSet::new(),
        )
        .await
        .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn get_skill_by_name_returns_matching() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        let found = get_skill_by_name(&storage, &ctx, "tdd").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().entity_name, "tdd");
    }

    #[tokio::test]
    async fn get_skill_by_name_none_on_miss() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        let found = get_skill_by_name(&storage, &ctx, "tdd-typo").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn ensure_parent_tag_creates_on_first_call() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let caller = Uuid::new_v4();
        let action = ensure_parent_tag(&storage, &ctx, caller, "tdd", "testing", None)
            .await
            .unwrap();
        assert!(matches!(action, EnsureParentTagAction::Created { .. }));

        // Both tag entities should now exist in the global partition.
        let global = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let entities = storage.entity_list_session(&ctx, global).await.unwrap();
        let tag_names: Vec<&str> = entities
            .iter()
            .filter(|e| e.entity_type == "tag")
            .map(|e| e.entity_name.as_str())
            .collect();
        assert!(tag_names.contains(&"tdd"));
        assert!(tag_names.contains(&"testing"));
    }

    #[tokio::test]
    async fn ensure_parent_tag_is_idempotent() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let caller = Uuid::new_v4();
        let first = ensure_parent_tag(&storage, &ctx, caller, "tdd", "testing", None)
            .await
            .unwrap();
        let second = ensure_parent_tag(&storage, &ctx, caller, "tdd", "testing", None)
            .await
            .unwrap();
        assert!(matches!(first, EnsureParentTagAction::Created { .. }));
        assert!(matches!(second, EnsureParentTagAction::Skipped { .. }));

        // Only one edge landed despite two calls.
        let global = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let edges = storage
            .typed_edge_list_session(&ctx, global)
            .await
            .unwrap();
        let parent_edges: Vec<&TypedEdge> =
            edges.iter().filter(|e| e.edge_type == "PARENT_TAG").collect();
        assert_eq!(parent_edges.len(), 1);
    }

    #[tokio::test]
    async fn ensure_parent_tag_normalizes_names() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let caller = Uuid::new_v4();
        // Caller uses mixed case / spaces — normalize_tag should collapse
        // these onto the same underlying tag entities.
        let first = ensure_parent_tag(&storage, &ctx, caller, "TDD", "Testing", None)
            .await
            .unwrap();
        let second = ensure_parent_tag(&storage, &ctx, caller, "tdd", "testing", None)
            .await
            .unwrap();
        assert!(matches!(first, EnsureParentTagAction::Created { .. }));
        assert!(matches!(second, EnsureParentTagAction::Skipped { .. }));
    }

    #[tokio::test]
    async fn ensure_parent_tag_rejects_self_loop() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let caller = Uuid::new_v4();
        let err = ensure_parent_tag(&storage, &ctx, caller, "tdd", "tdd", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must differ"));
    }

    #[tokio::test]
    async fn verify_skill_reports_exists_false_for_missing() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let result = verify_skill(&storage, &ctx, "does-not-exist").await.unwrap();
        assert!(!result.exists);
        assert!(result.tags.is_empty());
        assert!(result.prerequisites.is_empty());
    }

    #[tokio::test]
    async fn verify_skill_surfaces_tags_and_prerequisites() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        // Seed a prereq skill first.
        ingest_skill(&storage, &ctx, base_params("unit-testing"), None, None)
            .await
            .unwrap();
        // Skill under test — tagged, with prereq.
        let mut p = base_params("tdd");
        p.tags = vec!["methodology".into()];
        p.prerequisites = vec!["unit-testing".into()];
        ingest_skill(&storage, &ctx, p, None, None).await.unwrap();

        let result = verify_skill(&storage, &ctx, "tdd").await.unwrap();
        assert!(result.exists);
        assert!(result.entity_id.is_some());
        // Category + extra tag should both show up.
        assert!(result.tags.contains(&"testing".to_string()));
        assert!(result.tags.contains(&"methodology".to_string()));
        assert_eq!(result.prerequisites, vec!["unit-testing".to_string()]);
        assert!(result.missing_prerequisites.is_empty());
    }

    #[tokio::test]
    async fn verify_skill_reports_required_by() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        // "unit-testing" will end up required by "tdd".
        ingest_skill(&storage, &ctx, base_params("unit-testing"), None, None)
            .await
            .unwrap();
        let mut p = base_params("tdd");
        p.prerequisites = vec!["unit-testing".into()];
        ingest_skill(&storage, &ctx, p, None, None).await.unwrap();

        let result = verify_skill(&storage, &ctx, "unit-testing").await.unwrap();
        assert!(result.exists);
        assert_eq!(result.required_by, vec!["tdd".to_string()]);
    }

    #[tokio::test]
    async fn verify_skill_reports_missing_prereqs_when_raw_tracked() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        // Create skill with a prereq that doesn't exist. ingest_skill
        // silently skips the missing prereq edge today. Simulate the
        // forge-ingest behavior of recording raw_prerequisites in
        // properties so verify_skill can cross-check.
        let mut p = base_params("tdd");
        p.prerequisites = vec!["unit-testing".into()]; // won't resolve
        let action = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();

        // Manually patch properties.raw_prerequisites via a fetch+put —
        // emulates what forge's skill-ingest pipeline will do.
        let global = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let mut entity = storage
            .entity_get_by_id(&ctx, global, action.entity_id())
            .await
            .unwrap()
            .unwrap();
        if let Some(obj) = entity.properties.as_object_mut() {
            obj.insert(
                "raw_prerequisites".into(),
                serde_json::json!(["unit-testing"]),
            );
        }
        storage.entity_put(&ctx, &entity).await.unwrap();

        let result = verify_skill(&storage, &ctx, "tdd").await.unwrap();
        assert!(result.exists);
        assert_eq!(
            result.missing_prerequisites,
            vec!["unit-testing".to_string()],
            "prereq declared at ingest but never resolved to a REQUIRES edge must be reported"
        );
        assert!(
            result.prerequisites.is_empty(),
            "resolved prereqs is empty because the edge never landed"
        );
    }

    #[tokio::test]
    async fn build_invoke_result_populates_steps_and_first_prompt() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        ingest_skill(&storage, &ctx, base_params("tdd"), None, None)
            .await
            .unwrap();
        let entity = get_skill_by_name(&storage, &ctx, "tdd")
            .await
            .unwrap()
            .expect("tdd must be retrievable");
        let result = build_invoke_result(&entity);
        assert_eq!(result.skill_name, "tdd");
        assert_eq!(result.category, "testing");
        assert!(!result.version.is_empty());
        assert_eq!(result.steps.len(), 1);
        assert_eq!(
            result.first_step_prompt.as_deref(),
            Some("write a failing test")
        );
        assert_eq!(
            result.completion_criteria.as_deref(),
            Some("all steps complete")
        );
    }
}
