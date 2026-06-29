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

#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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

// ─── Property tests (T-P-002, T-P-003) ────────────────────────────
//
// Replaces the former `tests/property/test_expert_system_properties.py`
// pseudo-property tests (which only grepped this file's source text) with real
// proptests that drive the actual `reviewer_from_ctx`, `record_approval`,
// `alias_scope_rank`, and `resolve_alias` functions.
#[cfg(test)]
mod property_tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use proptest::prelude::*;

    fn alias_entry(
        tenant_id: Uuid,
        kind: AliasScopeKind,
        scope_ref: &str,
        updated_minutes: i64,
    ) -> AliasEntry {
        let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let updated_at = base + chrono::Duration::minutes(updated_minutes);
        AliasEntry {
            tenant_id,
            alias_id: Uuid::new_v4(),
            alias_name: "deploy".to_string(),
            scope_kind: kind,
            scope_ref: scope_ref.to_string(),
            canonical_tool: "deploy_tool".to_string(),
            parameter_map: json!({}),
            fixed_arguments: json!({}),
            args_templates: json!({}),
            status: ClaimStatus::Approved,
            created_at: base,
            updated_at,
        }
    }

    /// T-P-003 rank ordering, asserted via the real `alias_scope_rank` function.
    #[test]
    fn alias_scope_rank_orders_session_over_workspace_over_global() {
        assert!(
            alias_scope_rank(AliasScopeKind::Session) > alias_scope_rank(AliasScopeKind::Workspace)
        );
        assert!(
            alias_scope_rank(AliasScopeKind::Workspace) > alias_scope_rank(AliasScopeKind::Global)
        );
        assert_eq!(alias_scope_rank(AliasScopeKind::Session), 3);
        assert_eq!(alias_scope_rank(AliasScopeKind::Workspace), 2);
        assert_eq!(alias_scope_rank(AliasScopeKind::Global), 1);
    }

    proptest! {
        /// T-P-002 "approval replay preserves auth-derived state": `reviewer_from_ctx`
        /// is a pure function of `ctx.session_origin` — independent of tenant id, of
        /// any caller-supplied reviewer (there is none), and of the sequence of
        /// approval decisions recorded against it.
        #[test]
        fn reviewer_is_auth_derived_and_replay_invariant(
            origin in "[a-z:]{0,15}",
            decisions in prop::collection::vec(0u8..3, 0..6),
        ) {
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: origin.clone(),
            };
            let expected = reviewer_from_ctx(&ctx);

            // Pure in ctx: same session_origin under a different tenant -> same reviewer.
            let ctx_other_tenant = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: origin.clone(),
            };
            prop_assert_eq!(reviewer_from_ctx(&ctx_other_tenant), expected.clone());

            // Replay invariance: recording any sequence of decisions never changes
            // the auth-derived reviewer stamped on each approval.
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let store = MockStorage::new();
                for (i, tag) in decisions.iter().enumerate() {
                    let decision = match tag % 3 {
                        0 => ApprovalDecision::Proposed,
                        1 => ApprovalDecision::Approved,
                        _ => ApprovalDecision::Rejected,
                    };
                    let entry = record_approval(
                        &store,
                        &ctx,
                        ArtifactKind::Rule,
                        &format!("rule-{i}"),
                        decision,
                        None,
                        "global".to_string(),
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                    assert_eq!(entry.reviewer, expected);
                    // reviewer_from_ctx remains stable across the replay sequence.
                    assert_eq!(reviewer_from_ctx(&ctx), expected);
                }
            });
        }

        /// T-P-003 "alias scope resolution is deterministic": `resolve_alias` picks
        /// the same winner regardless of the order aliases are stored in, and that
        /// winner has the maximum `alias_scope_rank` present (ties broken by newest
        /// `updated_at`). Exercises the real comparator inside `resolve_alias`.
        ///
        /// Every entry is given a unique (scope_kind, scope_ref) so the store's own
        /// per-scope dedup never fires — what is under test is `resolve_alias`'s
        /// comparator, not `alias_put` collision handling. Global scopes match
        /// unconditionally; a single Workspace ("ws") and a single Session
        /// (session_id) entry can also match, so rank precedence is exercised.
        #[test]
        fn resolve_alias_is_permutation_invariant_and_rank_respecting(
            global_minutes in prop::collection::vec(0i64..5000, 1..6),
            include_ws in any::<bool>(),
            ws_minutes in 0i64..5000,
            include_sess in any::<bool>(),
            sess_minutes in 0i64..5000,
        ) {
            // Distinct updated_at among Global entries -> unique tie-break winner.
            let mut seen = std::collections::HashSet::new();
            let global_minutes: Vec<i64> = global_minutes
                .into_iter()
                .filter(|m| seen.insert(*m))
                .collect();
            prop_assume!(!global_minutes.is_empty());

            let tenant_id = Uuid::new_v4();
            let session_id = Uuid::new_v4();
            let mut entries: Vec<AliasEntry> = global_minutes
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    alias_entry(tenant_id, AliasScopeKind::Global, &format!("g{i}"), *m)
                })
                .collect();
            if include_ws {
                entries.push(alias_entry(tenant_id, AliasScopeKind::Workspace, "ws", ws_minutes));
            }
            if include_sess {
                let sref = session_id.to_string();
                entries.push(alias_entry(tenant_id, AliasScopeKind::Session, &sref, sess_minutes));
            }

            // Expected winner derived via the real `alias_scope_rank`: highest rank
            // present, then newest updated_at.
            let max_rank = entries
                .iter()
                .map(|e| alias_scope_rank(e.scope_kind))
                .max()
                .unwrap();
            let expected = entries
                .iter()
                .filter(|e| alias_scope_rank(e.scope_kind) == max_rank)
                .max_by_key(|e| e.updated_at)
                .unwrap()
                .clone();

            let ctx = TenantContext {
                tenant_id,
                session_origin: "tester".into(),
            };
            let rt = tokio::runtime::Runtime::new().unwrap();
            let (forward, reversed) = rt.block_on(async {
                let store_a = MockStorage::new();
                for e in entries.iter() {
                    store_a.alias_put(&ctx, e).await.unwrap();
                }
                let forward = resolve_alias(&store_a, &ctx, "deploy", Some("ws"), Some(session_id))
                    .await
                    .unwrap();

                let store_b = MockStorage::new();
                for e in entries.iter().rev() {
                    store_b.alias_put(&ctx, e).await.unwrap();
                }
                let reversed = resolve_alias(&store_b, &ctx, "deploy", Some("ws"), Some(session_id))
                    .await
                    .unwrap();
                (forward, reversed)
            });

            let forward = forward.expect("a matching alias must resolve");
            let reversed = reversed.expect("a matching alias must resolve");
            // Permutation invariance: identical winner regardless of store order.
            prop_assert_eq!(forward.alias_id, reversed.alias_id);
            // Winner respects rank then recency.
            prop_assert_eq!(forward.alias_id, expected.alias_id);
            prop_assert_eq!(alias_scope_rank(forward.scope_kind), max_rank);
        }
    }
}
