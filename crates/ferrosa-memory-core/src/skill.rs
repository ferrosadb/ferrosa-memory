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
pub async fn ingest_skill(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    params: IngestSkillParams,
    embedding_client: Option<&EmbeddingClient>,
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
    for prereq_name in &params.prerequisites {
        let mut matches = storage
            .entity_find_phonetic(ctx, storage_session, prereq_name)
            .await
            .unwrap_or_default();
        matches.retain(|e| e.entity_name == *prereq_name && e.entity_type == "skill");
        if let Some(prereq) = matches.first() {
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
        } else {
            tracing::info!(
                skill = %params.name,
                prereq = %prereq_name,
                "prerequisite skill not found; ingest it and re-run to create REQUIRES edge"
            );
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
        let action = ingest_skill(&storage, &ctx, base_params("tdd"), None)
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
        let action = ingest_skill(&storage, &ctx, base_params("tdd"), None)
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
        let action = ingest_skill(&storage, &ctx, base_params("tdd"), None)
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
        let first = ingest_skill(&storage, &ctx, p.clone(), None)
            .await
            .unwrap();
        let second = ingest_skill(&storage, &ctx, p, None).await.unwrap();
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
        let first = ingest_skill(&storage, &ctx, p.clone(), None)
            .await
            .unwrap();
        p.content_hash = Some("sha256:v2".into());
        p.description = "the tdd methodology, refined".into();
        let second = ingest_skill(&storage, &ctx, p, None).await.unwrap();
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
        ingest_skill(&storage, &ctx, base_params("tdd"), None)
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
        ingest_skill(&storage, &ctx, p, None).await.unwrap();
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
        ingest_skill(&storage, &ctx, base_params("unit-testing"), None)
            .await
            .unwrap();
        // Now ingest the skill that requires it.
        let mut p = base_params("tdd");
        p.prerequisites = vec!["unit-testing".into()];
        let tdd = ingest_skill(&storage, &ctx, p, None).await.unwrap();

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
        let action = ingest_skill(&storage, &ctx, p, None).await.unwrap();
        // Ingest succeeds even with a dangling prereq (logged, not failed).
        assert!(matches!(action, SkillIngestAction::Created { .. }));
    }
}
