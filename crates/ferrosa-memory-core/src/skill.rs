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
use crate::types::{EntityEntry, EntityScope, TenantContext};

/// One step of a skill's instructions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub instruction: String,
}

/// Caller-provided skill definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
        /// Prerequisite skill names the caller declared that couldn't be
        /// resolved to REQUIRES edges (because the target skill hadn't
        /// been ingested yet). Empty when every prereq resolved. The
        /// skill itself still landed — these are deferred, not fatal.
        /// Callers can either ingest the missing prereqs and re-run
        /// this skill, or accept the partial graph.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        missing_prerequisites: Vec<String>,
    },
    Updated {
        entity_id: Uuid,
        version: String,
        prior_version: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        missing_prerequisites: Vec<String>,
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

    /// Prerequisite names declared at ingest that didn't resolve to a
    /// REQUIRES edge. Empty for Skipped (the skill was unchanged).
    pub fn missing_prerequisites(&self) -> &[String] {
        match self {
            Self::Created {
                missing_prerequisites,
                ..
            }
            | Self::Updated {
                missing_prerequisites,
                ..
            } => missing_prerequisites,
            Self::Skipped { .. } => &[],
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

async fn find_skill_by_exact_name_with_read_after_write_retry(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    storage_session: Uuid,
    name: &str,
) -> anyhow::Result<Option<EntityEntry>> {
    const BACKOFF_MS: [u64; 4] = [25, 50, 100, 200];

    for backoff_ms in BACKOFF_MS {
        let found = storage
            .entity_find_by_exact_name(ctx, storage_session, name, "skill")
            .await?;
        if found.is_some() {
            return Ok(found);
        }
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
    }
    storage
        .entity_find_by_exact_name(ctx, storage_session, name, "skill")
        .await
}

#[allow(clippy::too_many_arguments)]
async fn put_skill_typed_edge(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    src_id: Uuid,
    edge_type: &str,
    dst_id: Uuid,
    weight: f64,
    metadata: Option<&str>,
    graph_client: Option<&crate::graph::GraphClient>,
) -> anyhow::Result<()> {
    if let Some(graph) = graph_client {
        graph
            .put_typed_edge(
                ctx.tenant_id,
                session_id,
                src_id,
                edge_type,
                dst_id,
                weight,
                metadata,
            )
            .await
    } else {
        crate::graph_write::create_typed_edge(
            storage,
            ctx,
            session_id,
            src_id,
            edge_type,
            dst_id,
            weight,
            metadata.map(str::to_string),
        )
        .await
        .map(|_| ())
    }
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

    // Idempotency key = (tenant, session, entity_name, entity_type=skill).
    // An exact lookup is used here instead of the fuzzy phonetic scan:
    // the phonetic path was reproducibly stale under bulk writes
    // (bug-ingest-skill-bulk-nondeterminism), which caused ingest_skill
    // to allocate duplicate entity_ids on re-runs and left later
    // verify_skill reads without visible TAGGED_AS edges.
    // Propagate storage errors — swallowing them here caused
    // bug-ingest-skill-swallows-lookup-errors (a transient CQL error was
    // treated as "skill doesn't exist", which created duplicates on retry
    // and falsely populated `missing_prerequisites`).
    let existing = storage
        .entity_find_by_exact_name(ctx, storage_session, &params.name, "skill")
        .await
        .map_err(|e| anyhow::anyhow!("skill name lookup failed for {:?}: {e}", params.name))?;

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

    // Build the new version string. If we can't list existing skills we
    // can't compute the next NN suffix correctly — better to fail than to
    // collide with an existing version.
    let today = today_yyyymmdd();
    let all_skills = storage
        .entity_list_session(ctx, storage_session)
        .await
        .map_err(|e| anyhow::anyhow!("skill version scan failed: {e}"))?;
    let today_versions: Vec<&str> = all_skills
        .iter()
        .filter(|e| e.entity_type == "skill")
        .filter_map(|e| e.properties.get("version").and_then(|v| v.as_str()))
        .collect();
    let new_version = next_version(&today, &today_versions);

    // Build properties JSON. `raw_prerequisites` is the caller-declared
    // list before REQUIRES-edge resolution, stored so `verify_skill` can
    // report prereqs that were deferred (target skill not yet ingested).
    let properties = serde_json::json!({
        "category": params.category,
        "trigger_keywords": params.trigger_keywords,
        "steps": params.steps,
        "output_artifacts": params.output_artifacts,
        "completion_criteria": params.completion_criteria,
        "version": new_version,
        "raw_prerequisites": params.prerequisites.clone(),
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
    //
    // The tag's entity_id is a deterministic UUIDv5 of
    // `(tenant_id, normalized_tag_name)` — see
    // `scope::tenant_tag_entity_uuid`. That decouples the edge write
    // from the tag entity upsert:
    //
    //   - Compute tag_id locally, no lookup, no race.
    //   - Best-effort upsert the tag entity (idempotent). If this
    //     fails (e.g., CQL lane reconnect under bulk contention), a
    //     concurrent sibling ingest or a later retry will have
    //     written the row — the edge still references the right id.
    //   - Always write the TAGGED_AS edge.
    //
    // Pre-fix behavior skipped the edge when the tag upsert errored,
    // which caused bug-ingest-skill-cluster-tag-dropped: 5 concurrent
    // ingests racing on the same "analysis" tag upsert would lose
    // the edge on 4 of 5 skills.
    for tag_name in &tag_names {
        let tag_id = crate::scope::tenant_tag_entity_uuid(ctx.tenant_id, tag_name);
        if let Err(e) =
            ensure_tag_entity(storage, ctx, storage_session, tag_name, ingested_by, now).await
        {
            tracing::warn!(
                skill = %params.name,
                tag = %tag_name,
                error = %e,
                "tag entity upsert failed; writing TAGGED_AS edge against \
                 deterministic tag id anyway — sibling or later ingest is \
                 expected to materialize the tag row"
            );
        }
        if let Err(e) = put_skill_typed_edge(
            storage,
            ctx,
            storage_session,
            entity_id,
            "TAGGED_AS",
            tag_id,
            1.0,
            None,
            graph_client,
        )
        .await
        {
            tracing::warn!(
                skill = %params.name,
                tag = %tag_name,
                error = %e,
                "TAGGED_AS edge write failed"
            );
        }
    }

    // --- REQUIRES edges for prerequisites ---
    // Cycle prevention: adding `skill -[REQUIRES]-> prereq` creates a cycle
    // iff prereq already transitively requires skill. When a graph client is
    // available, run the Cypher check and fail-closed on that edge. When
    // the graph is unreachable or unconfigured, log a warning and skip the
    // edge (we'd rather miss a relationship than silently allow a cycle to
    // land under a misconfigured environment).
    //
    // Deferred prereqs (target skill not ingested yet) are collected into
    // `missing_prereqs` and surfaced on the result so callers don't need
    // a separate verify_skill round-trip to notice.
    let mut missing_prereqs: Vec<String> = Vec::new();
    for prereq_name in &params.prerequisites {
        // Fail loud on storage errors. A transient CQL failure used to be
        // silently converted to "prereq doesn't exist" and surfaced as
        // `missing_prerequisites` — the caller couldn't tell whether the
        // prereq was truly missing or the lookup just failed, and retried
        // ingests produced duplicate entities.
        let Some(prereq) = find_skill_by_exact_name_with_read_after_write_retry(
            storage,
            ctx,
            storage_session,
            prereq_name,
        )
        .await
        .map_err(|e| anyhow::anyhow!("prereq lookup failed for {prereq_name:?}: {e}"))?
        else {
            tracing::info!(
                skill = %params.name,
                prereq = %prereq_name,
                "prerequisite skill not found; ingest it and re-run to create REQUIRES edge"
            );
            missing_prereqs.push(prereq_name.clone());
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
                    missing_prereqs.push(prereq_name.clone());
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
                    missing_prereqs.push(prereq_name.clone());
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

        if let Err(e) = put_skill_typed_edge(
            storage,
            ctx,
            storage_session,
            entity_id,
            "REQUIRES",
            prereq.entity_id,
            1.0,
            None,
            graph_client,
        )
        .await
        {
            tracing::warn!(skill = %params.name, prereq = %prereq_name, error = %e, "REQUIRES edge write failed");
            missing_prereqs.push(prereq_name.clone());
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
            missing_prerequisites: missing_prereqs,
        }
    } else {
        SkillIngestAction::Created {
            entity_id,
            version: new_version,
            missing_prerequisites: missing_prereqs,
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
    // Caller needs a deterministic empty-vs-error signal. If CQL fails,
    // returning Ok([]) would look like "no skills" — worse than Err.
    let all = storage
        .entity_list_session(ctx, global_session)
        .await
        .map_err(|e| anyhow::anyhow!("skill catalog list failed: {e}"))?;

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
            let score =
                score_skill_against_context(&e, &context_lower, &context_tokens, query_embedding);
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
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    // Trigger keywords are often multi-word phrases ("red-green-refactor",
    // "kent beck"). Check substring against the full lowercased context,
    // not just the token set.
    let trigger_matches = triggers
        .iter()
        .filter(|t| {
            let lowered = t.to_lowercase();
            context_lower.contains(&lowered) || context_tokens.contains(&lowered)
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
    // Exact lookup, same key ingest_skill uses — keeps verify_skill and
    // invoke_skill from disagreeing with the write path on which row is
    // "the" skill with this name.
    storage
        .entity_find_by_exact_name(ctx, global_session, name, "skill")
        .await
        .map_err(|e| anyhow::anyhow!("skill lookup by name {name:?} failed: {e}"))
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
    // This feeds the "did_you_mean" hint on a missed invoke_skill. It is
    // decorative, not load-bearing — a hint failure must not cascade into
    // the caller's error. But the failure still needs to be SEEN: logging
    // (not silent drop) so an operator can spot a partial outage.
    let mut matches = match storage
        .entity_find_phonetic(ctx, global_session, name)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                skill = %name,
                error = %e,
                "similar_skill_names lookup failed; returning empty hints"
            );
            return Vec::new();
        }
    };
    matches.retain(|e| e.entity_type == "skill");
    matches.into_iter().take(k).map(|e| e.entity_name).collect()
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
    // A failed read here used to be silently treated as "no existing edge",
    // which would then try to create a duplicate — masking quorum /
    // consistency issues and producing duplicate edges on retry.
    let existing = storage
        .typed_edge_list_from(ctx, global_session, child_id)
        .await
        .map_err(|e| {
            anyhow::anyhow!("PARENT_TAG idempotency check failed for child {child_id}: {e}")
        })?;
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

    put_skill_typed_edge(
        storage,
        ctx,
        global_session,
        child_id,
        "PARENT_TAG",
        parent_id,
        1.0,
        None,
        graph_client,
    )
    .await?;

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
    // Fail loud on storage errors — verify_skill is used by audit
    // pipelines; a silent partial view of the graph would be worse than
    // no view at all.
    let outgoing = storage
        .typed_edge_list_from(ctx, global_session, entity.entity_id)
        .await
        .map_err(|e| {
            anyhow::anyhow!("verify_skill outgoing-edge scan failed for {skill_name:?}: {e}")
        })?;
    let mut tags: Vec<String> = Vec::new();
    let mut prerequisites: Vec<String> = Vec::new();
    for edge in &outgoing {
        // A missing target here is a genuine dangling edge (safe to skip);
        // an error fetching the target is a real problem worth surfacing.
        let target = match storage
            .entity_get_by_id(ctx, global_session, edge.dst_id)
            .await
        {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "verify_skill edge target fetch failed for {}: {e}",
                    edge.dst_id
                ));
            }
        };
        match edge.edge_type.as_str() {
            "TAGGED_AS" if target.entity_type == "tag" => tags.push(target.entity_name),
            "REQUIRES" if target.entity_type == "skill" => prerequisites.push(target.entity_name),
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
        .map_err(|e| {
            anyhow::anyhow!("verify_skill incoming-edge scan failed for {skill_name:?}: {e}")
        })?;
    let mut required_by: Vec<String> = Vec::new();
    for edge in &all_edges {
        if edge.edge_type != "REQUIRES" || edge.dst_id != entity.entity_id {
            continue;
        }
        let source = match storage
            .entity_get_by_id(ctx, global_session, edge.src_id)
            .await
        {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "verify_skill required_by source fetch failed for {}: {e}",
                    edge.src_id
                ));
            }
        };
        if source.entity_type == "skill" {
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
///
/// The tag's `entity_id` is deterministic: `UUIDv5((tenant_id, tag_name))`.
/// This replaces the previous read-then-maybe-create flow, which had a
/// lookup race under concurrent bulk ingest (see
/// bug-ingest-skill-tag-crosstalk): two ingests racing on the phonetic
/// scan could each create their own tag entity with a fresh random id,
/// and later `TAGGED_AS` writes could end up referencing the wrong one.
///
/// With a name-derived id, every caller computes the same id for the same
/// name. `entity_put` is an upsert on the primary key `(tenant_id,
/// session_id, entity_id)`, so re-writing the same row is safe; if the
/// tag already exists the write is a no-op content-wise.
async fn ensure_tag_entity(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    storage_session: Uuid,
    tag_name: &str,
    ingested_by: Option<Uuid>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Uuid> {
    let entity_id = crate::scope::tenant_tag_entity_uuid(ctx.tenant_id, tag_name);
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

    #[tokio::test]
    async fn ingest_skill_propagates_prereq_lookup_error() {
        // Regression for bug-ingest-skill-swallows-lookup-errors: if the
        // storage layer errors during a prereq lookup, ingest_skill must
        // surface the error, NOT silently mark the prereq as missing.
        let storage = MockStorage::new();
        let ctx = test_ctx();
        // Arm the forced-error hook BEFORE calling ingest. Any
        // entity_find_by_exact_name call is now guaranteed to error,
        // which is what ingest_skill now uses for both the self lookup
        // and the prereq lookup.
        *storage.force_exact_name_error.lock().await =
            Some("simulated CQL transport failure".into());
        let result = ingest_skill(
            &storage,
            &ctx,
            IngestSkillParams {
                name: "child-skill".into(),
                category: "testing".into(),
                description: "a child skill with a prereq".into(),
                trigger_keywords: vec![],
                tags: vec![],
                prerequisites: vec!["parent-skill".into()],
                steps: vec![],
                output_artifacts: vec![],
                completion_criteria: None,
                content_hash: None,
                caller_session_id: Uuid::nil(),
            },
            None,
            None,
        )
        .await;
        let err = result.expect_err("ingest_skill must surface storage errors");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("simulated CQL transport failure"),
            "error must name the underlying storage failure, got: {msg}"
        );
    }

    #[tokio::test]
    async fn ingest_skill_retries_prereq_exact_lookup_after_read_after_write_miss() {
        // Regression for the live skill E2E flake: the prerequisite skill
        // write can succeed, but an immediate exact-name read at consistency
        // ONE can hit a replica that has not observed it yet. Prerequisite
        // resolution should retry boundedly before reporting it missing.
        let storage = MockStorage::new();
        let ctx = test_ctx();

        ingest_skill(&storage, &ctx, base_params("unit-testing"), None, None)
            .await
            .unwrap();
        storage
            .force_exact_name_misses
            .store(2, std::sync::atomic::Ordering::Relaxed);

        let mut p = base_params("tdd");
        p.prerequisites = vec!["unit-testing".into()];
        let action = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();

        assert!(
            action.missing_prerequisites().is_empty(),
            "transient read-after-write misses must not surface as missing prerequisites"
        );
    }

    #[tokio::test]
    async fn ingest_skill_is_idempotent_without_phonetic_lookup() {
        // Regression for bug-ingest-skill-bulk-nondeterminism. Under bulk
        // load the phonetic scan (full-partition ALLOW FILTERING) returned
        // stale / empty results, causing ingest_skill to allocate a fresh
        // entity_id for a skill that already existed. The idempotency
        // check must instead go through an exact-name lookup that doesn't
        // depend on the fuzzy scan. Simulate the stale phonetic view by
        // forcing `entity_find_phonetic` to error — if ingest still
        // short-circuits to Skipped on matching content_hash, the
        // idempotency path is independent of the phonetic scan.
        let storage = MockStorage::new();
        let ctx = test_ctx();

        // First ingest lays down the skill with a known content_hash.
        let mut p = base_params("tdd");
        p.content_hash = Some("sha256:abc".into());
        let first = ingest_skill(&storage, &ctx, p.clone(), None, None)
            .await
            .unwrap();
        let first_id = match first {
            SkillIngestAction::Created { entity_id, .. } => entity_id,
            other => panic!("expected Created, got {other:?}"),
        };

        // Now break phonetic. The exact-name idempotency check must not
        // rely on it.
        *storage.force_phonetic_error.lock().await = Some("simulated CQL phonetic failure".into());

        let second = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();
        match second {
            SkillIngestAction::Skipped { entity_id, .. } => {
                assert_eq!(
                    entity_id, first_id,
                    "idempotent skip must reuse the original entity_id"
                );
            }
            other => panic!("expected Skipped on unchanged content_hash, got {other:?}"),
        }
    }

    #[test]
    fn step_deserialization_rejects_unknown_fields() {
        let json = r#"{"title": "Step one", "body": "Verify the thing"}"#;
        let err = serde_json::from_str::<Step>(json).unwrap_err().to_string();
        assert!(
            err.contains("unknown field") && err.contains("title"),
            "expected deny_unknown_fields error naming `title`, got: {err}"
        );
    }

    #[test]
    fn step_deserialization_accepts_known_fields() {
        let json = r#"{"phase": "Red", "instruction": "write a failing test"}"#;
        let s: Step = serde_json::from_str(json).unwrap();
        assert_eq!(s.phase.as_deref(), Some("Red"));
        assert_eq!(s.instruction, "write a failing test");
    }

    #[test]
    fn ingest_skill_params_rejects_unknown_top_level_field() {
        let json = serde_json::json!({
            "name": "foo",
            "category": "testing",
            "description": "d",
            "caller_session_id": "00000000-0000-0000-0000-000000000000",
            "spurious_field": 1
        });
        let err = serde_json::from_value::<IngestSkillParams>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown field") && err.contains("spurious_field"),
            "expected deny_unknown_fields error naming `spurious_field`, got: {err}"
        );
    }

    #[test]
    fn ingest_skill_params_accepts_minimum_valid_payload() {
        let json = serde_json::json!({
            "name": "foo",
            "category": "testing",
            "description": "d",
            "caller_session_id": "00000000-0000-0000-0000-000000000000"
        });
        let p: IngestSkillParams = serde_json::from_value(json).unwrap();
        assert_eq!(p.name, "foo");
        assert!(p.steps.is_empty());
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
            other => panic!(
                "expected Skipped on identical content_hash, got {:?}",
                other
            ),
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
    async fn ensure_tag_entity_is_deterministic_across_stores() {
        // Regression for bug-ingest-skill-tag-crosstalk. Under bulk ingest,
        // concurrent `ensure_tag_entity` calls produced non-deterministic
        // tag entity_ids and TAGGED_AS edges sometimes pointed at tags
        // belonging to other skills. Deriving the id deterministically
        // from (tenant, normalized_name) removes the lookup race entirely:
        // every caller computes the same id for the same tag, no matter
        // the storage state or concurrent activity.
        let ctx = test_ctx();
        let storage_a = MockStorage::new();
        let storage_b = MockStorage::new();
        let session_id = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let now = chrono::Utc::now();

        let id_a = ensure_tag_entity(&storage_a, &ctx, session_id, "architecture", None, now)
            .await
            .unwrap();
        let id_b = ensure_tag_entity(&storage_b, &ctx, session_id, "architecture", None, now)
            .await
            .unwrap();

        assert_eq!(
            id_a, id_b,
            "ensure_tag_entity must derive its UUID from (tenant_id, tag_name) \
             so two callers always agree — no lookup race can produce a wrong id"
        );

        // Different tenant → different id (no cross-tenant leakage).
        let ctx_other = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let other_session = crate::scope::tenant_global_session_uuid(ctx_other.tenant_id);
        let id_other = ensure_tag_entity(
            &storage_a,
            &ctx_other,
            other_session,
            "architecture",
            None,
            now,
        )
        .await
        .unwrap();
        assert_ne!(
            id_a, id_other,
            "ensure_tag_entity must scope by tenant_id; otherwise two tenants' \
             tags with the same name would collide on the same partition"
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
    async fn concurrent_ingest_of_distinct_skills_does_not_crosslink_tags() {
        // Acceptance invariant for bug-ingest-skill-tag-crosstalk.
        //
        // Every TAGGED_AS edge out of a skill must point at a tag entity
        // whose name is in that skill's declared `category` + `tags`.
        // Bulk ingest previously violated this — two skills ingested
        // back-to-back could end up cross-linked: compile-project's
        // "architecture" edge became "analysis" (complexity-audit's
        // tag) and vice versa.
        //
        // This test exercises the happy path under concurrent
        // ingest_skill calls via `tokio::join!`. MockStorage serializes
        // writes under its Mutex, so the race window is narrower than
        // against live CQL — but the same code path runs, and any
        // shared mutable state or non-deterministic id allocation in
        // ensure_tag_entity would fail this invariant.
        use std::sync::Arc;

        let storage = Arc::new(MockStorage::new());
        let ctx = Arc::new(test_ctx());

        let make_params = |name: &str, category: &str, extra: &str| {
            let mut p = base_params(name);
            p.category = category.into();
            p.tags = vec![extra.into()];
            p
        };

        let s1 = Arc::clone(&storage);
        let c1 = Arc::clone(&ctx);
        let p1 = make_params("compile-project", "architecture", "task-level");
        let s2 = Arc::clone(&storage);
        let c2 = Arc::clone(&ctx);
        let p2 = make_params("complexity-audit", "analysis", "task-level");
        let s3 = Arc::clone(&storage);
        let c3 = Arc::clone(&ctx);
        let p3 = make_params("cloud-architect", "cloud", "task-level");

        let (r1, r2, r3) = tokio::join!(
            async move { ingest_skill(s1.as_ref(), c1.as_ref(), p1, None, None).await },
            async move { ingest_skill(s2.as_ref(), c2.as_ref(), p2, None, None).await },
            async move { ingest_skill(s3.as_ref(), c3.as_ref(), p3, None, None).await },
        );
        r1.unwrap();
        r2.unwrap();
        r3.unwrap();

        let gs = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let cases = [
            ("compile-project", vec!["architecture", "task-level"]),
            ("complexity-audit", vec!["analysis", "task-level"]),
            ("cloud-architect", vec!["cloud", "task-level"]),
        ];
        for (skill_name, expected_tags) in &cases {
            let skill = get_skill_by_name(storage.as_ref(), ctx.as_ref(), skill_name)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("skill {skill_name} must exist"));
            let edges = storage
                .typed_edge_list_from(ctx.as_ref(), gs, skill.entity_id)
                .await
                .unwrap();
            let tag_edges: Vec<_> = edges
                .iter()
                .filter(|e| e.edge_type == "TAGGED_AS")
                .collect();
            assert_eq!(
                tag_edges.len(),
                expected_tags.len(),
                "{skill_name}: expected {} TAGGED_AS edges, got {}",
                expected_tags.len(),
                tag_edges.len()
            );
            for edge in &tag_edges {
                let tag = storage
                    .entity_get_by_id(ctx.as_ref(), gs, edge.dst_id)
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("TAGGED_AS dst {} must resolve", edge.dst_id));
                assert!(
                    expected_tags.contains(&tag.entity_name.as_str()),
                    "{skill_name} TAGGED_AS edge points at tag {:?} which is not in declared tags {:?}",
                    tag.entity_name,
                    expected_tags
                );
            }
        }
    }

    #[tokio::test]
    async fn concurrent_ingest_sharing_a_cluster_tag_every_skill_gets_the_edge() {
        // Regression for bug-ingest-skill-cluster-tag-dropped. Under
        // concurrent bulk ingest, 5 skills all declaring the same
        // cluster tag ("analysis" in the field report) lost that tag
        // on 4 of 5 skills — the category tag survived, the shared
        // frontmatter cluster tag didn't. Acceptance #3 in the spec:
        // "ingest 5 skills that all declare the same cluster tag;
        // verify all 5 have that tag after concurrent ingest."
        use std::sync::Arc;

        let storage = Arc::new(MockStorage::new());
        let ctx = Arc::new(test_ctx());

        let names = [
            "code-audit",
            "dsm-analysis",
            "fmea",
            "database-consistency-audit",
            "complexity-audit",
        ];
        let futs: Vec<_> = names
            .iter()
            .map(|n| {
                let s = Arc::clone(&storage);
                let c = Arc::clone(&ctx);
                let mut p = base_params(n);
                // category "testing" (from base_params) becomes one tag;
                // the frontmatter cluster tag is "analysis", the one
                // the bug dropped.
                p.tags = vec!["analysis".into()];
                async move { ingest_skill(s.as_ref(), c.as_ref(), p, None, None).await }
            })
            .collect();
        let results = futures_util::future::join_all(futs).await;
        for (name, r) in names.iter().zip(results.iter()) {
            r.as_ref()
                .unwrap_or_else(|e| panic!("{name} ingest failed: {e}"));
        }

        let gs = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let analysis_tag_id = crate::scope::tenant_tag_entity_uuid(ctx.tenant_id, "analysis");

        for name in &names {
            let skill = get_skill_by_name(storage.as_ref(), ctx.as_ref(), name)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("skill {name} must exist"));
            let edges = storage
                .typed_edge_list_from(ctx.as_ref(), gs, skill.entity_id)
                .await
                .unwrap();
            let to_analysis: Vec<_> = edges
                .iter()
                .filter(|e| e.edge_type == "TAGGED_AS" && e.dst_id == analysis_tag_id)
                .collect();
            assert_eq!(
                to_analysis.len(),
                1,
                "{name} must have exactly one TAGGED_AS edge to the shared \
                 'analysis' cluster tag, got {} edges",
                to_analysis.len()
            );
        }
    }

    #[tokio::test]
    async fn tagged_as_edge_persists_when_tag_entity_upsert_fails() {
        // Regression for bug-ingest-skill-cluster-tag-dropped. In the
        // field, the tag-entity upsert for a shared cluster tag would
        // fail under CQL contention (lane reconnect / write timeout)
        // and the old code would also skip the TAGGED_AS edge, so the
        // skill silently lost its link to the tag. With deterministic
        // tag ids this is recoverable: the edge points at a stable
        // UUID, and a concurrent or later ingest will have written
        // (or will write) the tag row. `ingest_skill` must not gate
        // the edge write on the tag upsert's success.
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let gs = crate::scope::tenant_global_session_uuid(ctx.tenant_id);

        // Fail every tag-entity upsert this ingest attempts.
        *storage.force_entity_put_error.lock().await = Some((
            "tag".into(),
            "simulated CQL write timeout on tag row".into(),
        ));

        let mut p = base_params("code-audit");
        p.tags = vec!["analysis".into()];
        ingest_skill(&storage, &ctx, p, None, None).await.unwrap();

        let skill = get_skill_by_name(&storage, &ctx, "code-audit")
            .await
            .unwrap()
            .expect("skill row must exist (skill entity_put was not armed to fail)");
        let edges = storage
            .typed_edge_list_from(&ctx, gs, skill.entity_id)
            .await
            .unwrap();
        let expected_tag_id = crate::scope::tenant_tag_entity_uuid(ctx.tenant_id, "analysis");
        let to_analysis: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == "TAGGED_AS" && e.dst_id == expected_tag_id)
            .collect();
        assert_eq!(
            to_analysis.len(),
            1,
            "TAGGED_AS edge to the deterministic 'analysis' tag id must be \
             written even when the tag-entity upsert transiently failed; \
             a concurrent sibling ingest (or a later retry) will have \
             written the tag row. Got {} edges.",
            to_analysis.len()
        );
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
        let edges = storage.typed_edge_list_session(&ctx, global).await.unwrap();
        let requires: Vec<&crate::types::TypedEdge> = edges
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
        let hits = retrieve_skills_for_context(&storage, &ctx, caller, "tdd", None, 5, 0.0, &used)
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
        let edges = storage.typed_edge_list_session(&ctx, global).await.unwrap();
        let parent_edges: Vec<&crate::types::TypedEdge> = edges
            .iter()
            .filter(|e| e.edge_type == "PARENT_TAG")
            .collect();
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
        let result = verify_skill(&storage, &ctx, "does-not-exist")
            .await
            .unwrap();
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
        // Ingest a skill declaring a prereq that doesn't exist yet. The
        // server now records raw_prerequisites on every ingest (so
        // verify_skill cross-checks work without the caller patching
        // properties).
        let mut p = base_params("tdd");
        p.prerequisites = vec!["unit-testing".into()]; // won't resolve
        let action = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();
        // The ingest action itself surfaces the missing prereq — callers
        // no longer need to follow up with verify_skill to see this.
        assert_eq!(
            action.missing_prerequisites(),
            &["unit-testing".to_string()],
            "ingest_skill must surface deferred prereqs in the action response"
        );

        let result = verify_skill(&storage, &ctx, "tdd").await.unwrap();
        assert!(result.exists);
        assert_eq!(
            result.missing_prerequisites,
            vec!["unit-testing".to_string()],
            "verify_skill must also see the missing prereq"
        );
        assert!(
            result.prerequisites.is_empty(),
            "resolved prereqs is empty because the edge never landed"
        );
    }

    #[tokio::test]
    async fn ingest_skill_missing_prereqs_empty_when_all_resolve() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        ingest_skill(&storage, &ctx, base_params("unit-testing"), None, None)
            .await
            .unwrap();
        let mut p = base_params("tdd");
        p.prerequisites = vec!["unit-testing".into()];
        let action = ingest_skill(&storage, &ctx, p, None, None).await.unwrap();
        assert!(
            action.missing_prerequisites().is_empty(),
            "all prereqs resolved; missing list must be empty"
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
