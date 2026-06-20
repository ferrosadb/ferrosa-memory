//! Module: Archive remote lifecycle for cold remote memory detail.
//! Correctness: Correct when cold large detail can be signed to an archive remote while local access is preserved through active stubs and archived detail cannot override newer local facts.
//! Last revised: 2026-05-12
//! Last changed: Added Packet J archive lifecycle regression tests.

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::future::Future;
use uuid::Uuid;

use crate::remote_identity::{
    ContentHash, InstancePublicIdentity, InstanceSigningIdentity, SignedEnvelope,
};
use crate::remotes::detail::{
    DetailFetchRequest, DetailKind, DetailResponsePayload, RemoteDetailClient, fetch_detail,
};
use crate::remotes::policy::{PolicyAction, PolicyFact, RemotePolicy};
use crate::remotes::types::{
    ApplicabilityFrame, DetailRef, ImportState, RemoteStub, SafetyClassification, TeachingKind,
};

/// Thresholds for selecting local detail that is safe to move out to archive storage.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveSelectionConfig {
    pub max_warmth: f64,
    pub min_detail_bytes: usize,
    pub min_idle_days: i64,
}

impl Default for ArchiveSelectionConfig {
    fn default() -> Self {
        Self {
            max_warmth: 0.2,
            min_detail_bytes: 16 * 1024,
            min_idle_days: 30,
        }
    }
}

/// Local memory detail considered by Packet J archive pruning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveSourceRecord {
    pub local_entity_id: Uuid,
    pub remote_id: Uuid,
    pub packet_id: Uuid,
    pub item_id: Uuid,
    pub title: String,
    pub summary: String,
    pub detail: String,
    pub kind: TeachingKind,
    pub namespace: String,
    pub warmth: f64,
    pub last_used_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub current_ops: bool,
    pub safety: SafetyClassification,
    pub applicability: ApplicabilityFrame,
}

/// Candidate returned when a source record crosses cold/large archive thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveCandidate {
    pub local_entity_id: Uuid,
    pub remote_id: Uuid,
    pub packet_id: Uuid,
    pub item_id: Uuid,
    pub namespace: String,
    pub detail_bytes: usize,
    pub reason: String,
}

/// Signed archive packet payload persisted to an archive remote before local pruning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivePacketPayload {
    pub archive_remote_id: Uuid,
    pub original_remote_id: Uuid,
    pub local_entity_id: Uuid,
    pub packet_id: Uuid,
    pub item_id: Uuid,
    pub title: String,
    pub summary: String,
    pub namespace: String,
    pub content_hash: ContentHash,
    pub archived_at: DateTime<Utc>,
}

/// Result of committing one archive candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveCommitResult {
    pub signed_packet: SignedEnvelope<ArchivePacketPayload>,
    pub signed_detail: SignedEnvelope<DetailResponsePayload>,
    pub local_stub: RemoteStub,
    pub archived_state: ImportState,
}

/// Request for fetching detail through an archive stub.
#[derive(Debug, Clone)]
pub struct ArchiveDetailFetchRequest {
    pub remote_name: String,
    pub stub: RemoteStub,
    pub policy: RemotePolicy,
    pub public_identity: InstancePublicIdentity,
    pub learner_grants: Vec<String>,
}

/// Minimal local version metadata used to prevent archived detail from silently overwriting newer facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFactVersion {
    pub local_entity_id: Uuid,
    pub content_hash: ContentHash,
    pub updated_at: DateTime<Utc>,
    pub state: ImportState,
}

/// Archived version metadata returned alongside archive detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedDetailVersion {
    pub local_entity_id: Uuid,
    pub content_hash: ContentHash,
    pub archived_at: DateTime<Utc>,
}

/// Merge action for archived detail relative to the current local fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveMergeAction {
    UseArchivedDetail,
    ConflictReview,
}

/// Merge decision for archived detail fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveMergeDecision {
    pub action: ArchiveMergeAction,
    pub explanation: String,
}

/// Archive remote abstraction for Packet J commit/fetch tests and future HTTP transport.
pub trait ArchiveRemoteClient: RemoteDetailClient {
    fn push_archive_packet(
        &mut self,
        signed_packet: SignedEnvelope<ArchivePacketPayload>,
        signed_detail: SignedEnvelope<DetailResponsePayload>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Test/dry-run archive client that records pushed archive packets and returns configured detail.
#[derive(Debug, Clone, Default)]
pub struct MockArchiveRemoteClient {
    pub received_packets: Vec<SignedEnvelope<ArchivePacketPayload>>,
    pub received_details: Vec<SignedEnvelope<DetailResponsePayload>>,
    fetch_detail: Option<SignedEnvelope<DetailResponsePayload>>,
}

impl MockArchiveRemoteClient {
    pub fn with_detail(signed_detail: SignedEnvelope<DetailResponsePayload>) -> Self {
        Self {
            received_packets: Vec::new(),
            received_details: Vec::new(),
            fetch_detail: Some(signed_detail),
        }
    }
}

impl RemoteDetailClient for MockArchiveRemoteClient {
    async fn fetch_detail(
        &self,
        _request: &DetailFetchRequest,
    ) -> anyhow::Result<SignedEnvelope<DetailResponsePayload>> {
        self.fetch_detail
            .clone()
            .or_else(|| self.received_details.last().cloned())
            .ok_or_else(|| anyhow!("archive remote has no detail for request"))
    }
}

impl ArchiveRemoteClient for MockArchiveRemoteClient {
    async fn push_archive_packet(
        &mut self,
        signed_packet: SignedEnvelope<ArchivePacketPayload>,
        signed_detail: SignedEnvelope<DetailResponsePayload>,
    ) -> anyhow::Result<()> {
        self.fetch_detail = Some(signed_detail.clone());
        self.received_packets.push(signed_packet);
        self.received_details.push(signed_detail);
        Ok(())
    }
}

/// Select a source record for archival only when it is cold, large, idle, and not active current_ops.
pub fn select_archive_candidate(
    record: &ArchiveSourceRecord,
    config: &ArchiveSelectionConfig,
    now: DateTime<Utc>,
) -> Option<ArchiveCandidate> {
    let detail_bytes = record.detail.len();
    let idle_days = now.signed_duration_since(record.last_used_at).num_days();
    if record.current_ops {
        return None;
    }
    if record.warmth > config.max_warmth {
        return None;
    }
    if detail_bytes < config.min_detail_bytes {
        return None;
    }
    if idle_days < config.min_idle_days {
        return None;
    }

    Some(ArchiveCandidate {
        local_entity_id: record.local_entity_id,
        remote_id: record.remote_id,
        packet_id: record.packet_id,
        item_id: record.item_id,
        namespace: record.namespace.clone(),
        detail_bytes,
        reason: format!(
            "cold low-warmth large detail: warmth {:.3}, {} bytes, idle {idle_days} days",
            record.warmth, detail_bytes
        ),
    })
}

/// Commit one candidate by pushing signed packet/detail to archive and returning the retained local stub.
pub async fn archive_candidate<C: ArchiveRemoteClient>(
    archive: &mut C,
    signer: &InstanceSigningIdentity,
    archive_remote_id: Uuid,
    record: &ArchiveSourceRecord,
    candidate: ArchiveCandidate,
) -> anyhow::Result<ArchiveCommitResult> {
    if candidate.local_entity_id != record.local_entity_id || candidate.item_id != record.item_id {
        bail!("archive candidate does not match source record");
    }

    let archived_at = Utc::now();
    let detail_hash = ContentHash::sha256_bytes(record.detail.as_bytes());
    let detail_ref = DetailRef {
        remote_id: archive_remote_id,
        packet_id: record.packet_id,
        item_id: record.item_id,
        token: ContentHash::sha256_bytes(
            format!(
                "archive:{}:{}:{}:{}",
                archive_remote_id, record.packet_id, record.item_id, detail_hash
            )
            .as_bytes(),
        )
        .0,
        detail_hash: detail_hash.clone(),
        more_available: true,
        expires_at: archived_at + chrono::Duration::days(3650),
    };

    let packet = ArchivePacketPayload {
        archive_remote_id,
        original_remote_id: record.remote_id,
        local_entity_id: record.local_entity_id,
        packet_id: record.packet_id,
        item_id: record.item_id,
        title: record.title.clone(),
        summary: record.summary.clone(),
        namespace: record.namespace.clone(),
        content_hash: detail_hash,
        archived_at,
    };
    let detail = DetailResponsePayload {
        remote_id: archive_remote_id,
        packet_id: record.packet_id,
        item_id: record.item_id,
        detail_ref: detail_ref.clone(),
        detail: record.detail.clone(),
        kind: DetailKind::Knowledge,
        safety: record.safety.clone(),
        applicability: record.applicability.clone(),
        created_at: archived_at,
    };
    let signed_packet = signer.sign(packet)?;
    let signed_detail = signer.sign(detail)?;
    archive
        .push_archive_packet(signed_packet.clone(), signed_detail.clone())
        .await?;

    let local_stub = RemoteStub {
        stub_id: Uuid::new_v4(),
        remote_id: archive_remote_id,
        packet_id: record.packet_id,
        item_id: record.item_id,
        local_entity_id: Some(record.local_entity_id),
        title: record.title.clone(),
        summary: record.summary.clone(),
        state: ImportState::ActiveStub,
        detail_ref: Some(detail_ref),
        created_at: archived_at,
        updated_at: archived_at,
    };

    Ok(ArchiveCommitResult {
        signed_packet,
        signed_detail,
        local_stub,
        archived_state: ImportState::Archived,
    })
}

/// Default policy for archive remotes: detail fetch for historical detail, no current_ops autocommit.
pub fn default_archive_remote_policy(remote: &str) -> RemotePolicy {
    RemotePolicy::from_facts([
        PolicyFact::remote(remote),
        PolicyFact::trusted_for(remote, "historical_detail"),
        PolicyFact::grant(remote, PolicyAction::Read, "historical_detail"),
        PolicyFact::grant(remote, PolicyAction::DetailFetch, "knowledge"),
    ])
}

/// Fetch archive detail through the retained local stub and existing signed-detail verifier.
pub async fn fetch_archived_detail<C: RemoteDetailClient>(
    archive: &C,
    request: ArchiveDetailFetchRequest,
) -> anyhow::Result<SignedEnvelope<DetailResponsePayload>> {
    let detail_ref = request
        .stub
        .detail_ref
        .clone()
        .ok_or_else(|| anyhow!("archive stub has no detail ref"))?;
    if request.stub.state != ImportState::ActiveStub && request.stub.state != ImportState::Archived
    {
        bail!("archive detail can only be fetched from archived or active_stub local records");
    }
    fetch_detail(
        archive,
        DetailFetchRequest {
            remote_name: request.remote_name,
            detail_ref,
            policy: request.policy,
            public_identity: request.public_identity,
            learner_grants: request.learner_grants,
            allow_raw_context: false,
        },
    )
    .await
}

/// Decide whether archived detail may be used directly or must go through conflict review.
pub fn decide_archived_detail_merge(
    local: &LocalFactVersion,
    archived: &ArchivedDetailVersion,
) -> ArchiveMergeDecision {
    if local.local_entity_id == archived.local_entity_id
        && local.state == ImportState::Active
        && local.updated_at > archived.archived_at
        && local.content_hash != archived.content_hash
    {
        return ArchiveMergeDecision {
            action: ArchiveMergeAction::ConflictReview,
            explanation: "archive detail is older than a newer active local fact; require conflict review before override".into(),
        };
    }

    ArchiveMergeDecision {
        action: ArchiveMergeAction::UseArchivedDetail,
        explanation: "archived detail matches local version or is newer than local metadata".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_identity::{ContentHash, InstanceId, InstanceSigningIdentity};
    use crate::remotes::detail::{DetailKind, DetailResponsePayload};
    use crate::remotes::types::{
        ApplicabilityFrame, DetailRef, ImportState, RemoteStub, SafetyClassification, SafetyRisk,
        TeachingKind,
    };
    use uuid::Uuid;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn safety() -> SafetyClassification {
        SafetyClassification {
            risk: SafetyRisk::Low,
            reasons: vec!["historical detail".into()],
            redacted: false,
            requires_human: false,
        }
    }

    fn applicability(namespace: &str) -> ApplicabilityFrame {
        ApplicabilityFrame {
            namespaces: vec![namespace.into()],
            host_os: None,
            container_runtime: None,
            hardware: vec![],
            required_tags: vec![],
            excluded_tags: vec![],
            confidence: 0.9,
        }
    }

    fn detail_ref(remote_id: Uuid, packet_id: Uuid, item_id: Uuid, detail: &str) -> DetailRef {
        DetailRef {
            remote_id,
            packet_id,
            item_id,
            token: format!("token-{item_id}"),
            detail_hash: ContentHash::sha256_bytes(detail.as_bytes()),
            more_available: true,
            expires_at: chrono::Utc::now() + chrono::Duration::days(7),
        }
    }

    fn source_record(
        namespace: &str,
        warmth: f64,
        detail_bytes: usize,
        last_used_days_ago: i64,
        current_ops: bool,
    ) -> ArchiveSourceRecord {
        let now = chrono::Utc::now();
        ArchiveSourceRecord {
            local_entity_id: id(10),
            remote_id: id(20),
            packet_id: id(30),
            item_id: id(40),
            title: "Old deployment notes".into(),
            summary: format!("{namespace} archived deployment detail"),
            detail: "x".repeat(detail_bytes),
            kind: TeachingKind::Decision,
            namespace: namespace.into(),
            warmth,
            last_used_at: now - chrono::Duration::days(last_used_days_ago),
            created_at: now - chrono::Duration::days(90),
            current_ops,
            safety: safety(),
            applicability: applicability(namespace),
        }
    }

    #[test]
    fn warm_recently_used_items_are_not_archive_candidates() {
        let config = ArchiveSelectionConfig::default();
        let warm = source_record("historical_detail", 0.9, 64_000, 120, false);
        let recent = source_record("historical_detail", 0.1, 64_000, 1, false);

        assert!(select_archive_candidate(&warm, &config, chrono::Utc::now()).is_none());
        assert!(select_archive_candidate(&recent, &config, chrono::Utc::now()).is_none());
    }

    #[test]
    fn cold_low_warmth_large_items_are_archive_candidates() {
        let config = ArchiveSelectionConfig::default();
        let record = source_record("historical_detail", 0.05, 64_000, 120, false);

        let candidate = select_archive_candidate(&record, &config, chrono::Utc::now())
            .expect("cold large detail should be archiveable");

        assert_eq!(candidate.local_entity_id, record.local_entity_id);
        assert!(candidate.reason.contains("cold"));
        assert!(candidate.detail_bytes >= config.min_detail_bytes);
    }

    #[test]
    fn active_operational_facts_are_not_archived_when_current_or_frequently_used() {
        let config = ArchiveSelectionConfig::default();
        let record = source_record("current_ops", 0.05, 64_000, 120, true);

        assert!(select_archive_candidate(&record, &config, chrono::Utc::now()).is_none());
    }

    #[tokio::test]
    async fn archive_commit_sends_signed_detail_and_keeps_local_active_stub() {
        let signer = InstanceSigningIdentity::generate(InstanceId(id(1)));
        let archive_remote_id = id(99);
        let record = source_record("historical_detail", 0.05, 64_000, 120, false);
        let candidate = select_archive_candidate(
            &record,
            &ArchiveSelectionConfig::default(),
            chrono::Utc::now(),
        )
        .unwrap();
        let mut archive = MockArchiveRemoteClient::default();

        let commit =
            archive_candidate(&mut archive, &signer, archive_remote_id, &record, candidate)
                .await
                .unwrap();

        assert_eq!(archive.received_details.len(), 1);
        archive.received_details[0]
            .verify(&signer.public_identity())
            .unwrap();
        assert_eq!(commit.local_stub.state, ImportState::ActiveStub);
        assert_eq!(commit.archived_state, ImportState::Archived);
        assert_eq!(
            commit.local_stub.detail_ref.as_ref().unwrap().remote_id,
            archive_remote_id
        );
    }

    #[test]
    fn archive_remote_defaults_to_historical_detail_not_current_ops() {
        let policy = default_archive_remote_policy("cold-store");
        let historical = crate::remotes::policy::PolicyItem::new("a", "historical_detail");
        let current = crate::remotes::policy::PolicyItem::new("b", "current_ops");

        let historical_decision = policy.can_fetch_detail("cold-store", &historical);
        assert!(
            historical_decision.allowed,
            "{}",
            historical_decision.explanation
        );
        assert!(!policy.can_autocommit("cold-store", &current).allowed);
        assert!(historical_decision.explanation.contains("detail_fetch"));
    }

    #[tokio::test]
    async fn archive_stub_routes_to_archive_detail_fetch() {
        let signer = InstanceSigningIdentity::generate(InstanceId(id(2)));
        let archive_remote_id = id(77);
        let detail = "full archived detail";
        let packet_id = id(30);
        let item_id = id(40);
        let detail_ref = detail_ref(archive_remote_id, packet_id, item_id, detail);
        let payload = DetailResponsePayload {
            remote_id: archive_remote_id,
            packet_id,
            item_id,
            detail_ref: detail_ref.clone(),
            detail: detail.into(),
            kind: DetailKind::Knowledge,
            safety: safety(),
            applicability: applicability("historical_detail"),
            created_at: chrono::Utc::now(),
        };
        let signed = signer.sign(payload).unwrap();
        let archive = MockArchiveRemoteClient::with_detail(signed);
        let stub = RemoteStub {
            stub_id: id(50),
            remote_id: archive_remote_id,
            packet_id,
            item_id,
            local_entity_id: Some(id(10)),
            title: "Archived".into(),
            summary: "historical_detail archived".into(),
            state: ImportState::ActiveStub,
            detail_ref: Some(detail_ref),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let request = ArchiveDetailFetchRequest {
            remote_name: "cold-store".into(),
            stub,
            policy: default_archive_remote_policy("cold-store"),
            public_identity: signer.public_identity(),
            learner_grants: vec!["detail_fetch".into()],
        };

        let fetched = fetch_archived_detail(&archive, request).await.unwrap();

        assert_eq!(fetched.payload.detail, detail);
    }

    #[test]
    fn archive_detail_does_not_override_newer_active_local_fact_without_conflict_review() {
        let local = LocalFactVersion {
            local_entity_id: id(10),
            content_hash: ContentHash("newer".into()),
            updated_at: chrono::Utc::now(),
            state: ImportState::Active,
        };
        let archived = ArchivedDetailVersion {
            local_entity_id: id(10),
            content_hash: ContentHash("older".into()),
            archived_at: chrono::Utc::now() - chrono::Duration::days(30),
        };

        let decision = decide_archived_detail_merge(&local, &archived);

        assert_eq!(decision.action, ArchiveMergeAction::ConflictReview);
        assert!(decision.explanation.contains("newer active local fact"));
    }
}
