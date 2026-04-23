use std::str::FromStr;

use serde_json::json;
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{
    AliasEntry, AliasScopeKind, ApprovalDecision, ApprovalEntry, ArtifactKind, ClaimStatus,
    EntityEntry, EntityScope, TenantContext,
};

pub const CLAIM_ENTITY_TYPE: &str = "claim";
pub const APPROVAL_MIRROR_ENTITY_TYPE: &str = "approval_decision";
pub const ALIAS_MIRROR_ENTITY_TYPE: &str = "tool_alias";

pub fn reviewer_from_ctx(ctx: &TenantContext) -> String {
    ctx.session_origin
        .split_once(':')
        .map(|(_, reviewer)| reviewer.to_string())
        .unwrap_or_else(|| ctx.session_origin.clone())
}

pub fn approval_mirror_entity_id(kind: ArtifactKind, artifact_ref: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("approval-mirror:{kind}:{artifact_ref}").as_bytes(),
    )
}

pub fn claim_entity_id(claim_id: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("claim:{claim_id}").as_bytes())
}

pub fn alias_mirror_entity_id(
    alias_name: &str,
    scope_kind: AliasScopeKind,
    scope_ref: &str,
) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("alias:{alias_name}:{scope_kind}:{scope_ref}").as_bytes(),
    )
}

pub fn parse_claim_status(value: &str) -> anyhow::Result<ClaimStatus> {
    match value {
        "proposed" => Ok(ClaimStatus::Proposed),
        "approved" => Ok(ClaimStatus::Approved),
        "rejected" => Ok(ClaimStatus::Rejected),
        other => anyhow::bail!("unknown claim status: {other}"),
    }
}

pub fn parse_approval_decision(value: &str) -> anyhow::Result<ApprovalDecision> {
    match value {
        "proposed" => Ok(ApprovalDecision::Proposed),
        "approved" => Ok(ApprovalDecision::Approved),
        "rejected" => Ok(ApprovalDecision::Rejected),
        other => anyhow::bail!("unknown approval decision: {other}"),
    }
}

pub fn parse_artifact_kind(value: &str) -> anyhow::Result<ArtifactKind> {
    match value {
        "rule" => Ok(ArtifactKind::Rule),
        "claim" => Ok(ArtifactKind::Claim),
        "alias" => Ok(ArtifactKind::Alias),
        "skill" => Ok(ArtifactKind::Skill),
        other => anyhow::bail!("unknown artifact kind: {other}"),
    }
}

pub fn parse_alias_scope_kind(value: &str) -> anyhow::Result<AliasScopeKind> {
    match value {
        "global" => Ok(AliasScopeKind::Global),
        "workspace" => Ok(AliasScopeKind::Workspace),
        "session" => Ok(AliasScopeKind::Session),
        other => anyhow::bail!("unknown alias scope kind: {other}"),
    }
}

pub fn approval_entity(entry: &ApprovalEntry) -> EntityEntry {
    EntityEntry {
        tenant_id: entry.tenant_id,
        entity_id: entry.mirror_entity_id,
        session_id: entry.session_scope.unwrap_or(Uuid::nil()),
        entity_name: format!("approval:{}:{}", entry.artifact_kind, entry.artifact_ref),
        entity_type: APPROVAL_MIRROR_ENTITY_TYPE.to_string(),
        source_fold_id: None,
        context_snippet: entry
            .review_note
            .clone()
            .unwrap_or_else(|| format!("{} {}", entry.reviewer, entry.decision)),
        entity_embedding: None,
        confidence: 1.0,
        state: crate::types::MemoryState::Active,
        created_at: entry.created_at,
        description: Some(format!(
            "{} decision on {}:{} by {}",
            entry.decision, entry.artifact_kind, entry.artifact_ref, entry.reviewer
        )),
        description_embedding: None,
        tags: vec![
            "approval".into(),
            entry.artifact_kind.to_string(),
            entry.decision.to_string(),
        ],
        properties: json!({
            "artifact_kind": entry.artifact_kind,
            "artifact_ref": entry.artifact_ref,
            "decision": entry.decision,
            "review_note": entry.review_note,
            "reviewer": entry.reviewer,
            "scope": entry.scope,
            "workspace_scope": entry.workspace_scope,
            "session_scope": entry.session_scope,
            "approval_id": entry.approval_id,
        }),
        content_hash: None,
        updated_at: Some(entry.created_at),
        scope: EntityScope::Global,
        ingested_by_session: entry.session_scope,
    }
}

pub fn claim_entity(
    ctx: &TenantContext,
    claim_id: &str,
    session_id: Uuid,
    claim_text: &str,
    domain: &str,
    status: ClaimStatus,
    confidence: f64,
    source_ref: Option<&str>,
    support_count: i32,
    workspace_scope: Option<&str>,
) -> EntityEntry {
    let now = chrono::Utc::now();
    EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id: claim_entity_id(claim_id),
        session_id,
        entity_name: claim_id.to_string(),
        entity_type: CLAIM_ENTITY_TYPE.to_string(),
        source_fold_id: None,
        context_snippet: claim_text.to_string(),
        entity_embedding: None,
        confidence,
        state: crate::types::MemoryState::Active,
        created_at: now,
        description: Some(claim_text.to_string()),
        description_embedding: None,
        tags: vec!["claim".into(), status.to_string(), domain.to_string()],
        properties: json!({
            "claim_id": claim_id,
            "claim_text": claim_text,
            "domain": domain,
            "status": status,
            "confidence": confidence,
            "source_ref": source_ref,
            "support_count": support_count,
            "workspace_scope": workspace_scope,
            "session_scope": session_id,
        }),
        content_hash: None,
        updated_at: Some(now),
        scope: EntityScope::Session,
        ingested_by_session: Some(session_id),
    }
}

pub fn claim_status_from_entity(entry: &EntityEntry) -> ClaimStatus {
    entry
        .properties
        .get("status")
        .and_then(|value| value.as_str())
        .and_then(|value| parse_claim_status(value).ok())
        .unwrap_or(ClaimStatus::Proposed)
}

pub fn alias_scope_rank(scope_kind: AliasScopeKind) -> u8 {
    match scope_kind {
        AliasScopeKind::Session => 3,
        AliasScopeKind::Workspace => 2,
        AliasScopeKind::Global => 1,
    }
}

pub async fn latest_approval(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    kind: ArtifactKind,
    artifact_ref: &str,
) -> anyhow::Result<Option<ApprovalEntry>> {
    storage
        .approval_latest(ctx, &kind.to_string(), artifact_ref)
        .await
}

pub async fn approval_state(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    kind: ArtifactKind,
    artifact_ref: &str,
) -> anyhow::Result<Option<ApprovalDecision>> {
    Ok(latest_approval(storage, ctx, kind, artifact_ref)
        .await?
        .map(|entry| entry.decision))
}

pub async fn is_artifact_approved(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    kind: ArtifactKind,
    artifact_ref: &str,
) -> anyhow::Result<bool> {
    Ok(matches!(
        approval_state(storage, ctx, kind, artifact_ref).await?,
        Some(ApprovalDecision::Approved)
    ))
}

pub async fn record_approval(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    artifact_kind: ArtifactKind,
    artifact_ref: &str,
    decision: ApprovalDecision,
    review_note: Option<String>,
    scope: String,
    workspace_scope: Option<String>,
    session_scope: Option<Uuid>,
) -> anyhow::Result<ApprovalEntry> {
    let entry = ApprovalEntry {
        tenant_id: ctx.tenant_id,
        approval_id: Uuid::now_v7(),
        artifact_kind,
        artifact_ref: artifact_ref.to_string(),
        decision,
        review_note,
        reviewer: reviewer_from_ctx(ctx),
        scope,
        workspace_scope,
        session_scope,
        mirror_entity_id: approval_mirror_entity_id(artifact_kind, artifact_ref),
        created_at: chrono::Utc::now(),
    };

    storage.approval_append(ctx, &entry).await?;
    storage.entity_put(ctx, &approval_entity(&entry)).await?;
    Ok(entry)
}

pub async fn resolve_alias(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    alias_name: &str,
    workspace_scope: Option<&str>,
    session_scope: Option<Uuid>,
) -> anyhow::Result<Option<AliasEntry>> {
    let mut aliases = storage.alias_list(ctx, alias_name).await?;
    aliases.retain(|entry| matches!(entry.status, ClaimStatus::Approved));
    aliases.sort_by(|left, right| {
        alias_scope_rank(right.scope_kind)
            .cmp(&alias_scope_rank(left.scope_kind))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });

    Ok(aliases.into_iter().find(|entry| match entry.scope_kind {
        AliasScopeKind::Session => session_scope
            .map(|session_id| entry.scope_ref == session_id.to_string())
            .unwrap_or(false),
        AliasScopeKind::Workspace => workspace_scope
            .map(|workspace| entry.scope_ref == workspace)
            .unwrap_or(false),
        AliasScopeKind::Global => true,
    }))
}

pub fn alias_mirror_entity(entry: &AliasEntry, session_scope: Option<Uuid>) -> EntityEntry {
    EntityEntry {
        tenant_id: entry.tenant_id,
        entity_id: alias_mirror_entity_id(&entry.alias_name, entry.scope_kind, &entry.scope_ref),
        session_id: session_scope.unwrap_or(Uuid::nil()),
        entity_name: entry.alias_name.clone(),
        entity_type: ALIAS_MIRROR_ENTITY_TYPE.to_string(),
        source_fold_id: None,
        context_snippet: format!("{} -> {}", entry.alias_name, entry.canonical_tool),
        entity_embedding: None,
        confidence: 1.0,
        state: crate::types::MemoryState::Active,
        created_at: entry.created_at,
        description: Some(format!(
            "{} alias in {} scope {}",
            entry.alias_name, entry.scope_kind, entry.scope_ref
        )),
        description_embedding: None,
        tags: vec![
            "alias".into(),
            entry.scope_kind.to_string(),
            entry.status.to_string(),
        ],
        properties: json!({
            "alias_id": entry.alias_id,
            "alias_name": entry.alias_name,
            "scope_kind": entry.scope_kind,
            "scope_ref": entry.scope_ref,
            "canonical_tool": entry.canonical_tool,
            "parameter_map": entry.parameter_map,
            "fixed_arguments": entry.fixed_arguments,
            "args_templates": entry.args_templates,
            "status": entry.status,
        }),
        content_hash: None,
        updated_at: Some(entry.updated_at),
        scope: EntityScope::Global,
        ingested_by_session: session_scope,
    }
}

impl FromStr for ArtifactKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_artifact_kind(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
}
