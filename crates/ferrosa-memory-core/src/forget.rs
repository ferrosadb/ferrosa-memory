//! Candidate-confirmed forgetting (the `forget` tool's engine).
//!
//! Implements the **propose → confirm → forget** workflow described in
//! `specs/todo/feat-forget-memory.md`. This module is the storage-facing core;
//! the MCP dispatch wiring (ToolDefs + arms) is added in a later stage.
//!
//! ## Workflow
//!
//! - [`propose`] is **read-only**: it runs a cross-session [`hybrid_search`],
//!   computes each candidate's blast radius (edges / temporal chains / derived
//!   refs / inbound references) and content hash, and returns a stateless,
//!   keyed-hash-signed [`ForgetToken`]. It mutates nothing.
//! - [`confirm`] decodes + verifies the token (TTL + keyed hash), re-fetches
//!   each selected entity, and **skips** any whose content hash changed since
//!   propose (TOCTOU guard, URS-FORGET-006). For the survivors it writes a
//!   [`ForgetJournalEntry`] *before* mutating, then either retracts (default,
//!   reversible) or hard-deletes the entity together with its edges and derived
//!   rows, and finally marks the journal `completed` and writes an audit row.
//! - [`restore`] reverses a retraction via [`crate::entity::restore_entity`].
//!
//! ## Token scheme
//!
//! The token is `hex(json(payload)) + "." + hex(sha256(key || json))`. There is
//! no `base64`/`hmac` crate in this crate, so we use hex for transport and a
//! keyed SHA-256 (`hash(key || payload)`) for integrity. The token carries the
//! candidate set (id, type, session, content hash), a scope hash, and a
//! `created_at` epoch second for TTL enforcement. It is fully stateless — no
//! server-side session storage.
//!
//! ## v1 limitations (documented, not hidden)
//!
//! - **Edges are removed, never soft-marked.** There is no per-edge "retracted"
//!   state in the storage model, so on a *retract*-mode forget the candidate's
//!   edges are *deleted* (same as hard mode) even though the entity itself is
//!   only soft-retracted. On [`restore`] the entity returns to its prior state
//!   but its edges are **NOT** recreated. This is a known v1 gap; see the spec
//!   note on referential integrity.
//! - **Typed edges only.** Edge teardown here uses the `typed_edge_*` Storage
//!   primitives. Legacy bidirectional edges (`CO_OCCURS`/`MENTIONED_IN`/
//!   `SUPERSEDES`) are *counted* in the blast radius via `edge_list_for_entity`
//!   but their per-edge deletion routes through the graph API, which is wired
//!   in a later stage. The blast radius still reports them so the user sees the
//!   true reference count.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::hybrid_search::{FusionConfig, SearchFilter, SearchScope, hybrid_search};
use crate::storage::Storage;
use crate::types::{EntityEntry, ForgetJournalEntry, TenantContext};

/// A single candidate the user may choose to forget, as captured in the token.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CandidateRef {
    pub object_id: Uuid,
    pub object_type: String,
    pub session_id: Uuid,
    /// Content hash at propose time — re-checked at confirm (TOCTOU guard).
    pub content_hash: String,
}

/// The stateless, signed forget token. Encodes everything `confirm` needs to
/// validate a selection without server-side storage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForgetToken {
    pub candidates: Vec<CandidateRef>,
    /// Hash of the propose-time scope, so a token can't be replayed against a
    /// different scope.
    pub scope_hash: String,
    /// Epoch seconds at propose time; TTL is enforced against this.
    pub created_at: i64,
}

/// Disposition mode for a confirmed forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetMode {
    /// Reversible: entity → `Unavailable` + retraction record. Default.
    Retract,
    /// Permanent: entity + edges + derived rows deleted. Irreversible.
    Hard,
}

/// The blast radius of a candidate — how much references it, in each dimension.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlastRadius {
    /// Total distinct edges referencing the entity (inbound + outbound, typed +
    /// legacy, de-duplicated).
    pub edges: usize,
    /// Temporal supersession/fact chains for the entity.
    pub temporal_chains: usize,
    /// Best-effort count of derived-cache rows referencing the entity.
    pub derived_refs: usize,
    /// Inbound references (edges whose `dst` is the entity) — the ones that
    /// would dangle if missed.
    pub referenced_by: usize,
}

/// One proposed candidate, enriched with blast radius for disclosure.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProposedCandidate {
    pub object_id: Uuid,
    pub object_type: String,
    pub session_id: Uuid,
    pub name: String,
    pub snippet: String,
    pub match_score: f64,
    pub state: String,
    pub last_accessed: DateTime<Utc>,
    pub content_hash: String,
    pub blast_radius: BlastRadius,
    pub high_impact: bool,
}

/// Summary counts for a propose response.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProposeSummary {
    pub candidate_count: usize,
    pub high_impact_count: usize,
}

/// Result of [`propose`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProposeResult {
    /// The signed, TTL-bounded token to hand back on confirm.
    pub forget_token: String,
    pub candidates: Vec<ProposedCandidate>,
    pub summary: ProposeSummary,
    pub warnings: Vec<String>,
}

/// Per-item outcome from [`confirm`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForgottenItem {
    pub id: Uuid,
    pub outcome: String,
    pub new_state: String,
}

/// A skipped selection and why.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkippedItem {
    pub id: Uuid,
    pub reason: String,
}

/// Result of [`confirm`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfirmResult {
    pub forgotten: Vec<ForgottenItem>,
    pub skipped: Vec<SkippedItem>,
    pub restorable_until: Option<DateTime<Utc>>,
    pub forget_id: Uuid,
}

// ─── Content hashing ──────────────────────────────────────────────

/// Stable content hash of an entity, used to detect material change between
/// propose and confirm (URS-FORGET-006). Hashes a stable subset — name,
/// description, context snippet, properties, and *sorted* tags — so benign
/// metadata touches (warmth, `last_accessed`, state) don't spuriously skip a
/// valid forget. Tags are sorted so ordering changes don't matter.
pub fn entity_content_hash(entity: &EntityEntry) -> String {
    let mut tags = entity.tags.clone();
    tags.sort();
    let properties = serde_json::to_string(&entity.properties).unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(entity.entity_name.as_bytes());
    hasher.update([0u8]);
    hasher.update(entity.description.as_deref().unwrap_or("").as_bytes());
    hasher.update([0u8]);
    hasher.update(entity.context_snippet.as_bytes());
    hasher.update([0u8]);
    hasher.update(properties.as_bytes());
    hasher.update([0u8]);
    hasher.update(tags.join("\u{1f}").as_bytes());
    hex::encode(hasher.finalize())
}

// ─── Token encode/decode ──────────────────────────────────────────

/// Keyed SHA-256 over `key || payload`. No HMAC crate is available; this keyed
/// construction is sufficient for a short-lived, server-issued integrity tag.
fn keyed_hash(key: &[u8], payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.update(payload);
    hex::encode(hasher.finalize())
}

/// Encode a token as `hex(json) + "." + hex(sha256(key || json))`.
pub fn encode(token: &ForgetToken, key: &[u8]) -> anyhow::Result<String> {
    let json = serde_json::to_vec(token)?;
    let tag = keyed_hash(key, &json);
    Ok(format!("{}.{}", hex::encode(&json), tag))
}

/// Decode + verify a token: checks the keyed hash and the TTL.
///
/// Returns an error (never a silent default) when the token is malformed, the
/// signature does not verify, or it has expired — so `confirm` rejects loudly
/// and the caller must re-propose.
pub fn decode(
    s: &str,
    key: &[u8],
    ttl_seconds: u64,
    now: DateTime<Utc>,
) -> anyhow::Result<ForgetToken> {
    let (body_hex, tag) = s
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("token expired/invalid: malformed"))?;
    let json = hex::decode(body_hex)
        .map_err(|_| anyhow::anyhow!("token expired/invalid: bad encoding"))?;

    let expected = keyed_hash(key, &json);
    // Constant-time-ish compare on equal-length hex tags.
    if expected.len() != tag.len()
        || expected
            .bytes()
            .zip(tag.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
    {
        anyhow::bail!("token expired/invalid: signature mismatch");
    }

    let token: ForgetToken = serde_json::from_slice(&json)
        .map_err(|_| anyhow::anyhow!("token expired/invalid: bad payload"))?;

    let age = now.timestamp() - token.created_at;
    if age < 0 || age as u64 > ttl_seconds {
        anyhow::bail!("token expired/invalid: ttl exceeded");
    }

    Ok(token)
}

/// Stable hash of a scope filter description, bound into the token.
fn scope_hash(query: &str, scope_filters: &[String]) -> String {
    let mut sorted = scope_filters.to_vec();
    sorted.sort();
    let mut hasher = Sha256::new();
    hasher.update(query.as_bytes());
    hasher.update([0u8]);
    hasher.update(sorted.join("\u{1f}").as_bytes());
    hex::encode(hasher.finalize())
}

// ─── Blast radius ─────────────────────────────────────────────────

/// Compute a candidate's blast radius: distinct referencing edges (inbound +
/// outbound, typed + legacy), temporal chains, derived refs, and the inbound
/// count. Read-only.
async fn blast_radius(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    entity_id: Uuid,
) -> anyhow::Result<BlastRadius> {
    let out = storage
        .typed_edge_list_from(ctx, session_id, entity_id)
        .await?;
    let inbound = storage
        .typed_edge_list_to(ctx, session_id, entity_id)
        .await?;

    // De-dup typed edges across the two directions by full key, and fold in the
    // legacy bidirectional neighbors so the count reflects every reference.
    let mut keys: std::collections::HashSet<(Uuid, String, Uuid)> =
        std::collections::HashSet::new();
    for e in out.iter().chain(inbound.iter()) {
        keys.insert((e.src_id, e.edge_type.clone(), e.dst_id));
    }
    let referenced_by = inbound.len();

    let legacy = storage.edge_list_for_entity(ctx, entity_id).await?;
    for (neighbor, edge_type) in &legacy {
        // Legacy edges are undirected pairs; key them with the entity as one
        // endpoint so they don't collide with typed edges.
        keys.insert((entity_id, format!("legacy:{edge_type}"), *neighbor));
    }

    // Temporal chains: read-only presence check. The storage layer exposes the
    // *current* fact (`temporal_get_current`); a full chain count would require
    // a scan, so v1 reports 1 when a temporal fact exists, 0 otherwise.
    let temporal_chains = match storage.temporal_get_current(ctx, entity_id).await {
        Ok(Some(_)) => 1,
        Ok(None) => 0,
        // Best-effort: a backend without temporal storage must not block a
        // forget. Log and treat as zero (observable, not silent corruption).
        Err(e) => {
            tracing::debug!(%entity_id, error = %e, "temporal blast-radius probe failed");
            0
        }
    };
    // Derived refs: there is no read-only by-entity count primitive (the delete
    // path scans), so v1 reports 0 here (best-effort). The confirm path still
    // invalidates derived rows regardless of this count.
    let derived_refs = 0;

    Ok(BlastRadius {
        edges: keys.len(),
        temporal_chains,
        derived_refs,
        referenced_by,
    })
}

// ─── Propose ──────────────────────────────────────────────────────

/// Propose candidates for forgetting (read-only). Runs a cross-session hybrid
/// search, enriches the top entity candidates with blast radius + content hash,
/// and returns a signed token plus the candidate list. **Mutates nothing.**
#[allow(clippy::too_many_arguments)]
pub async fn propose(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    query: &str,
    scope_filters: &[String],
    limit: usize,
    candidate_max: usize,
    high_impact_edge_threshold: usize,
    key: &[u8],
    now: DateTime<Utc>,
) -> anyhow::Result<ProposeResult> {
    anyhow::ensure!(!query.is_empty(), "forget propose: query must not be empty");

    let effective_limit = limit.min(candidate_max).clamp(1, 50);
    let filter = SearchFilter {
        scope: SearchScope::Both,
        ..SearchFilter::default()
    };
    let results = hybrid_search(
        storage,
        ctx,
        session_id,
        query,
        None,
        effective_limit,
        None,
        None,
        None,
        &FusionConfig::default(),
        Some(&filter),
    )
    .await?;

    let mut candidates = Vec::new();
    let mut warnings = Vec::new();
    let mut high_impact_count = 0usize;

    for r in results.iter().filter(|r| r.result_type == "entity") {
        if candidates.len() >= effective_limit {
            break;
        }
        // Re-fetch the entity to compute a stable content hash and read its
        // current state. We don't know the entity's session a priori, so use
        // the caller's session; cross-session entities are retrieved by id.
        let Some(entity) = storage.entity_get_by_id(ctx, session_id, r.id).await? else {
            // The search hit may be a cross-session/global entity not visible
            // via this session's by-id read. Skip with a warning rather than
            // fabricate a candidate we can't safely hash.
            warnings.push(format!("candidate {} not resolvable in session", r.id));
            continue;
        };

        let radius = blast_radius(storage, ctx, entity.session_id, entity.entity_id).await?;
        let high_impact = radius.edges >= high_impact_edge_threshold;
        if high_impact {
            high_impact_count += 1;
        }

        candidates.push(ProposedCandidate {
            object_id: entity.entity_id,
            object_type: "entity".to_string(),
            session_id: entity.session_id,
            name: entity.entity_name.clone(),
            snippet: entity.context_snippet.clone(),
            match_score: r.score,
            state: entity.state.to_string(),
            last_accessed: entity.created_at,
            content_hash: entity_content_hash(&entity),
            blast_radius: radius,
            high_impact,
        });
    }

    if results.len() > candidates.len() {
        warnings.push("candidate set truncated to limit".to_string());
    }

    let token = ForgetToken {
        candidates: candidates
            .iter()
            .map(|c| CandidateRef {
                object_id: c.object_id,
                object_type: c.object_type.clone(),
                session_id: c.session_id,
                content_hash: c.content_hash.clone(),
            })
            .collect(),
        scope_hash: scope_hash(query, scope_filters),
        created_at: now.timestamp(),
    };
    let forget_token = encode(&token, key)?;

    let summary = ProposeSummary {
        candidate_count: candidates.len(),
        high_impact_count,
    };

    Ok(ProposeResult {
        forget_token,
        candidates,
        summary,
        warnings,
    })
}

// ─── Confirm ──────────────────────────────────────────────────────

/// Confirm a forget: decode + verify the token, apply the TOCTOU guard, and
/// disposition each surviving selection as one journaled unit.
#[allow(clippy::too_many_arguments)]
pub async fn confirm(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    token_str: &str,
    selected_ids: &[Uuid],
    mode: ForgetMode,
    ack_high_impact: bool,
    reason: &str,
    actor: &str,
    key: &[u8],
    ttl_seconds: u64,
    purge_days: u32,
    high_impact_edge_threshold: usize,
    now: DateTime<Utc>,
) -> anyhow::Result<ConfirmResult> {
    let token = decode(token_str, key, ttl_seconds, now)?;
    let forget_id = Uuid::new_v4();

    // Selected must be a subset of the token's candidates.
    let by_id: std::collections::HashMap<Uuid, &CandidateRef> =
        token.candidates.iter().map(|c| (c.object_id, c)).collect();
    for id in selected_ids {
        anyhow::ensure!(
            by_id.contains_key(id),
            "selected id {id} is not in the token's candidate set"
        );
    }

    let restorable_until = now + chrono::Duration::days(purge_days as i64);
    let mut forgotten = Vec::new();
    let mut skipped = Vec::new();

    for id in selected_ids {
        let candidate = by_id[id];

        // Re-fetch and re-hash: skip on material change (TOCTOU guard).
        let Some(entity) = storage
            .entity_get_by_id(ctx, candidate.session_id, *id)
            .await?
        else {
            skipped.push(SkippedItem {
                id: *id,
                reason: "changed since proposed".to_string(),
            });
            continue;
        };
        if entity_content_hash(&entity) != candidate.content_hash {
            skipped.push(SkippedItem {
                id: *id,
                reason: "changed since proposed".to_string(),
            });
            continue;
        }

        // High-impact gate.
        let radius = blast_radius(storage, ctx, entity.session_id, *id).await?;
        if radius.edges >= high_impact_edge_threshold && !ack_high_impact {
            skipped.push(SkippedItem {
                id: *id,
                reason: "high-impact requires acknowledgement".to_string(),
            });
            continue;
        }

        // Journal the unit BEFORE mutating (atomicity backstop).
        let target_ids = serde_json::to_string(&serde_json::json!([{
            "object_type": "entity",
            "object_id": id.to_string(),
            "session_id": entity.session_id.to_string(),
        }]))
        .unwrap_or_else(|_| "[]".to_string());
        let mode_str = match mode {
            ForgetMode::Retract => "retract",
            ForgetMode::Hard => "hard",
        };
        let journal = ForgetJournalEntry {
            tenant_id: ctx.tenant_id,
            forget_id,
            target_ids,
            mode: mode_str.to_string(),
            step_states: r#"{"entity":"pending","edges":"pending","derived":"pending"}"#
                .to_string(),
            status: "in_progress".to_string(),
            reason: reason.to_string(),
            actor: actor.to_string(),
            created_at: now,
            updated_at: now,
        };
        storage.forget_journal_put(ctx, &journal).await?;

        // Tear down typed edges (both directions). NOTE: there is no soft-edge
        // state, so edges are *deleted* even in retract mode and are NOT
        // recreated on restore (documented v1 limitation).
        teardown_typed_edges(storage, ctx, entity.session_id, *id).await?;

        let (outcome, new_state) = match mode {
            ForgetMode::Retract => {
                crate::entity::retract_entity(
                    storage,
                    ctx,
                    entity.session_id,
                    *id,
                    reason,
                    actor,
                    forget_id,
                    restorable_until,
                    now,
                )
                .await?;
                // Invalidate derived data so retracted facts aren't re-surfaced.
                invalidate_derived(storage, ctx, *id).await?;
                ("retracted".to_string(), "unavailable".to_string())
            }
            ForgetMode::Hard => {
                invalidate_derived(storage, ctx, *id).await?;
                storage.entity_delete(ctx, entity.session_id, *id).await?;
                ("deleted".to_string(), "deleted".to_string())
            }
        };

        storage
            .forget_journal_update_status(
                ctx,
                forget_id,
                "completed",
                r#"{"entity":"done","edges":"done","derived":"done"}"#,
                now,
            )
            .await?;

        forgotten.push(ForgottenItem {
            id: *id,
            outcome,
            new_state,
        });
    }

    // Audit the forget (URS-FORGET-005).
    let audit = crate::types::AuditEntry {
        tenant_id: ctx.tenant_id,
        audit_id: Uuid::new_v4(),
        operation: format!(
            "forget:{}",
            match mode {
                ForgetMode::Retract => "retract",
                ForgetMode::Hard => "hard",
            }
        ),
        target_table: "entity".to_string(),
        target_id: forgotten
            .iter()
            .map(|f| f.id.to_string())
            .collect::<Vec<_>>()
            .join(","),
        session_id: Uuid::nil(),
        created_at: now,
    };
    storage.audit_put(ctx, &audit).await?;

    let restorable_until = if matches!(mode, ForgetMode::Retract) && !forgotten.is_empty() {
        Some(restorable_until)
    } else {
        None
    };

    Ok(ConfirmResult {
        forgotten,
        skipped,
        restorable_until,
        forget_id,
    })
}

/// Delete every typed edge touching `entity_id`, in both directions, so no
/// surviving edge references a forgotten fact (URS-FORGET-009, typed subset).
async fn teardown_typed_edges(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    entity_id: Uuid,
) -> anyhow::Result<()> {
    let out = storage
        .typed_edge_list_from(ctx, session_id, entity_id)
        .await?;
    for e in out {
        storage
            .typed_edge_delete(ctx, session_id, e.src_id, &e.edge_type, e.dst_id)
            .await?;
    }
    let inbound = storage
        .typed_edge_list_to(ctx, session_id, entity_id)
        .await?;
    for e in inbound {
        storage
            .typed_edge_delete(ctx, session_id, e.src_id, &e.edge_type, e.dst_id)
            .await?;
    }
    Ok(())
}

/// Invalidate derived/materialized data referencing the entity, so a forgotten
/// fact can't be re-surfaced through a stale derivation.
async fn invalidate_derived(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
) -> anyhow::Result<()> {
    storage.confidence_delete_by_entity(ctx, entity_id).await?;
    storage.temporal_delete_by_entity(ctx, entity_id).await?;
    storage
        .derived_cache_delete_by_entity(ctx, entity_id)
        .await?;
    Ok(())
}

// ─── Restore ──────────────────────────────────────────────────────

/// Restore a retracted entity to its prior state. Delegates to
/// [`crate::entity::restore_entity`]. Returns whether a retraction record was
/// found (and therefore whether anything was restored).
///
/// **v1 limitation:** the entity's edges were hard-removed at forget time (no
/// soft-edge state), so they are NOT recreated here — only the entity's
/// memory state is restored.
pub async fn restore(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
) -> anyhow::Result<bool> {
    crate::entity::restore_entity(storage, ctx, entity_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use crate::types::{MemoryState, TypedEdge};

    const KEY: &[u8] = b"forget-test-key-0000000000000000";

    fn ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    async fn put_entity(
        store: &MockStorage,
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
        snippet: &str,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let entry = EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: id,
            session_id,
            entity_name: name.to_string(),
            entity_type: "concept".to_string(),
            source_fold_id: None,
            context_snippet: snippet.to_string(),
            entity_embedding: None,
            confidence: 1.0,
            state: MemoryState::Active,
            created_at: Utc::now(),
            description: None,
            description_embedding: None,
            tags: Vec::new(),
            properties: serde_json::Value::Null,
            content_hash: None,
            updated_at: None,
            scope: Default::default(),
            ingested_by_session: None,
        };
        store.entity_put(ctx, &entry).await.unwrap();
        id
    }

    async fn add_typed_edge(
        store: &MockStorage,
        ctx: &TenantContext,
        session_id: Uuid,
        src: Uuid,
        edge_type: &str,
        dst: Uuid,
    ) {
        store.typed_edges.lock().await.push(TypedEdge {
            tenant_id: ctx.tenant_id,
            session_id,
            src_id: src,
            edge_type: edge_type.to_string(),
            dst_id: dst,
            weight: 1.0,
            metadata: None,
            created_at: Utc::now(),
        });
    }

    #[tokio::test]
    async fn token_roundtrip_and_ttl() {
        let token = ForgetToken {
            candidates: vec![CandidateRef {
                object_id: Uuid::new_v4(),
                object_type: "entity".into(),
                session_id: Uuid::new_v4(),
                content_hash: "abc".into(),
            }],
            scope_hash: "scope".into(),
            created_at: Utc::now().timestamp(),
        };
        let encoded = encode(&token, KEY).unwrap();
        let decoded = decode(&encoded, KEY, 600, Utc::now()).unwrap();
        assert_eq!(decoded, token);

        // Wrong key fails.
        assert!(
            decode(
                &encoded,
                b"different-key-aaaaaaaaaaaaaaaaaaa",
                600,
                Utc::now()
            )
            .is_err()
        );

        // Expired fails.
        let later = Utc::now() + chrono::Duration::seconds(601);
        assert!(decode(&encoded, KEY, 600, later).is_err());
    }

    #[tokio::test]
    async fn propose_returns_ranked_candidates_and_mutates_nothing() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();
        put_entity(
            &store,
            &ctx,
            sid,
            "outbound port exhaustion",
            "sockets held",
        )
        .await;
        put_entity(&store, &ctx, sid, "unrelated topic", "nothing here").await;

        let before_entities = store.entities.lock().await.len();
        let before_retractions = store.retractions.lock().await.len();

        let res = propose(
            &store,
            &ctx,
            sid,
            "outbound port",
            &[],
            10,
            50,
            25,
            KEY,
            Utc::now(),
        )
        .await
        .unwrap();

        assert!(!res.candidates.is_empty(), "should find candidates");
        assert!(res.candidates.iter().any(|c| c.name.contains("outbound")));
        // Token decodes.
        assert!(decode(&res.forget_token, KEY, 600, Utc::now()).is_ok());
        // Nothing mutated.
        assert_eq!(store.entities.lock().await.len(), before_entities);
        assert_eq!(store.retractions.lock().await.len(), before_retractions);
        assert!(store.forget_journal.lock().await.is_empty());
    }

    #[tokio::test]
    async fn confirm_retract_hides_entity_writes_record_and_journal_and_audit() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();
        let id = put_entity(&store, &ctx, sid, "Secret Fact", "to forget").await;

        let res = propose(
            &store,
            &ctx,
            sid,
            "Secret Fact",
            &[],
            10,
            50,
            25,
            KEY,
            Utc::now(),
        )
        .await
        .unwrap();
        let now = Utc::now();
        let confirmed = confirm(
            &store,
            &ctx,
            &res.forget_token,
            &[id],
            ForgetMode::Retract,
            false,
            "user said forget",
            "tester",
            KEY,
            600,
            7,
            25,
            now,
        )
        .await
        .unwrap();

        assert_eq!(confirmed.forgotten.len(), 1);
        assert_eq!(confirmed.forgotten[0].new_state, "unavailable");
        assert!(confirmed.skipped.is_empty());
        assert!(confirmed.restorable_until.is_some());

        // Entity moved to Unavailable + excluded from phonetic recall.
        let entity = store
            .entity_get_by_id(&ctx, sid, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.state, MemoryState::Unavailable);
        let recall = store
            .entity_find_phonetic(&ctx, sid, "secret")
            .await
            .unwrap();
        assert!(recall.iter().all(|e| e.entity_id != id));

        // Retraction record written.
        assert!(
            store
                .retraction_get_latest(&ctx, id)
                .await
                .unwrap()
                .is_some()
        );

        // Journal completed.
        let journal = store
            .forget_journal_get(&ctx, confirmed.forget_id)
            .await
            .unwrap()
            .expect("journal entry present");
        assert_eq!(journal.status, "completed");
        assert_eq!(journal.mode, "retract");

        // Audit row written.
        let audits = store.audit_entries.lock().await;
        assert!(audits.iter().any(|a| a.operation == "forget:retract"));
    }

    #[tokio::test]
    async fn confirm_hard_deletes_entity_and_edges() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();
        let id = put_entity(&store, &ctx, sid, "Throwaway", "delete me").await;
        let other = Uuid::new_v4();
        add_typed_edge(&store, &ctx, sid, id, "RELATES_TO", other).await;
        add_typed_edge(&store, &ctx, sid, other, "RELATES_TO", id).await;

        let res = propose(
            &store,
            &ctx,
            sid,
            "Throwaway",
            &[],
            10,
            50,
            25,
            KEY,
            Utc::now(),
        )
        .await
        .unwrap();
        let confirmed = confirm(
            &store,
            &ctx,
            &res.forget_token,
            &[id],
            ForgetMode::Hard,
            false,
            "purge",
            "tester",
            KEY,
            600,
            7,
            25,
            Utc::now(),
        )
        .await
        .unwrap();

        assert_eq!(confirmed.forgotten.len(), 1);
        assert_eq!(confirmed.forgotten[0].new_state, "deleted");
        assert!(confirmed.restorable_until.is_none());

        // Entity gone.
        assert!(
            store
                .entity_get_by_id(&ctx, sid, id)
                .await
                .unwrap()
                .is_none()
        );
        // No surviving typed edge references it.
        assert!(
            store
                .typed_edge_list_from(&ctx, sid, id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .typed_edge_list_to(&ctx, sid, id)
                .await
                .unwrap()
                .is_empty()
        );
        let remaining = store.typed_edges.lock().await;
        assert!(
            remaining.iter().all(|e| e.src_id != id && e.dst_id != id),
            "no edge should reference the deleted entity"
        );
    }

    #[tokio::test]
    async fn toctou_changed_candidate_is_skipped() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();
        let id = put_entity(&store, &ctx, sid, "Mutable", "original snippet").await;

        let res = propose(
            &store,
            &ctx,
            sid,
            "Mutable",
            &[],
            10,
            50,
            25,
            KEY,
            Utc::now(),
        )
        .await
        .unwrap();

        // Mutate content between propose and confirm (changes the hash).
        {
            let mut entities = store.entities.lock().await;
            let e = entities.iter_mut().find(|e| e.entity_id == id).unwrap();
            e.context_snippet = "DIFFERENT now".to_string();
        }

        let confirmed = confirm(
            &store,
            &ctx,
            &res.forget_token,
            &[id],
            ForgetMode::Retract,
            false,
            "r",
            "tester",
            KEY,
            600,
            7,
            25,
            Utc::now(),
        )
        .await
        .unwrap();

        assert!(confirmed.forgotten.is_empty());
        assert_eq!(confirmed.skipped.len(), 1);
        assert_eq!(confirmed.skipped[0].reason, "changed since proposed");
        // Entity untouched.
        let entity = store
            .entity_get_by_id(&ctx, sid, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.state, MemoryState::Active);
    }

    #[tokio::test]
    async fn expired_token_confirm_errors() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();
        let id = put_entity(&store, &ctx, sid, "Old", "stale").await;

        let propose_time = Utc::now();
        let res = propose(&store, &ctx, sid, "Old", &[], 10, 50, 25, KEY, propose_time)
            .await
            .unwrap();

        let later = propose_time + chrono::Duration::seconds(601);
        let err = confirm(
            &store,
            &ctx,
            &res.forget_token,
            &[id],
            ForgetMode::Retract,
            false,
            "r",
            "tester",
            KEY,
            600,
            7,
            25,
            later,
        )
        .await;
        assert!(err.is_err());
        assert!(
            err.unwrap_err()
                .to_string()
                .contains("token expired/invalid")
        );
    }

    #[tokio::test]
    async fn high_impact_without_ack_is_skipped() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();
        let id = put_entity(&store, &ctx, sid, "Hub", "central node").await;
        // Give it 3 edges and use a threshold of 2 -> high impact.
        for _ in 0..3 {
            add_typed_edge(&store, &ctx, sid, id, "RELATES_TO", Uuid::new_v4()).await;
        }

        let res = propose(&store, &ctx, sid, "Hub", &[], 10, 50, 2, KEY, Utc::now())
            .await
            .unwrap();
        assert!(res.candidates.iter().any(|c| c.high_impact));

        let confirmed = confirm(
            &store,
            &ctx,
            &res.forget_token,
            &[id],
            ForgetMode::Retract,
            false, // no ack
            "r",
            "tester",
            KEY,
            600,
            7,
            2,
            Utc::now(),
        )
        .await
        .unwrap();

        assert!(confirmed.forgotten.is_empty());
        assert_eq!(confirmed.skipped.len(), 1);
        assert_eq!(
            confirmed.skipped[0].reason,
            "high-impact requires acknowledgement"
        );
    }

    #[tokio::test]
    async fn restore_brings_retracted_entity_back() {
        let store = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();
        let id = put_entity(&store, &ctx, sid, "Comeback", "restore me").await;
        // Put it in Dormant so restore must read the prior state.
        store
            .entity_update_state(&ctx, id, MemoryState::Dormant)
            .await
            .unwrap();

        let res = propose(
            &store,
            &ctx,
            sid,
            "Comeback",
            &[],
            10,
            50,
            25,
            KEY,
            Utc::now(),
        )
        .await
        .unwrap();
        confirm(
            &store,
            &ctx,
            &res.forget_token,
            &[id],
            ForgetMode::Retract,
            false,
            "r",
            "tester",
            KEY,
            600,
            7,
            25,
            Utc::now(),
        )
        .await
        .unwrap();

        // Retracted.
        assert_eq!(
            store
                .entity_get_by_id(&ctx, sid, id)
                .await
                .unwrap()
                .unwrap()
                .state,
            MemoryState::Unavailable
        );

        let restored = restore(&store, &ctx, id).await.unwrap();
        assert!(restored);
        assert_eq!(
            store
                .entity_get_by_id(&ctx, sid, id)
                .await
                .unwrap()
                .unwrap()
                .state,
            MemoryState::Dormant
        );

        // Restoring again finds no record.
        assert!(!restore(&store, &ctx, id).await.unwrap());
    }
}
