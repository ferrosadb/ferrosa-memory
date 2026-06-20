//! Module: Progressive remote detail references and stub-driven fetch.
//! Correctness: Correct when detail refs are opaque, expire, bind to packet/item/grants, and learner policy explains whether stub queries fetch detail.
//! Last revised: 2026-05-12
//! Last changed: Implemented Packet G detail refs, signed detail fetch, and stub fetch decisions.

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use uuid::Uuid;

use crate::remote_identity::{ContentHash, InstancePublicIdentity, SignedEnvelope};
use crate::remotes::policy::{PolicyItem, RemotePolicy};
use crate::remotes::types::{
    ApplicabilityFrame, DetailRef, RemoteStub, SafetyClassification, TeachingKind,
};

/// Teacher-side grant record backing an opaque detail reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailGrantRecord {
    pub remote_id: Uuid,
    pub packet_id: Uuid,
    pub item_id: Uuid,
    pub source_ref: String,
    pub detail_hash: ContentHash,
    pub grants: Vec<String>,
    pub expires_at: DateTime<Utc>,
}

/// Expanded detail kind. Raw context is a separate capability from ordinary knowledge detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailKind {
    Knowledge,
    RawContext,
}

/// Signed payload returned by the teacher for an accepted detail fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailResponsePayload {
    pub remote_id: Uuid,
    pub packet_id: Uuid,
    pub item_id: Uuid,
    pub detail_ref: DetailRef,
    pub detail: String,
    pub kind: DetailKind,
    pub safety: SafetyClassification,
    pub applicability: ApplicabilityFrame,
    pub created_at: DateTime<Utc>,
}

/// Learner-side request for a detail fetch.
#[derive(Debug, Clone)]
pub struct DetailFetchRequest {
    pub remote_name: String,
    pub detail_ref: DetailRef,
    pub policy: RemotePolicy,
    pub public_identity: InstancePublicIdentity,
    pub learner_grants: Vec<String>,
    pub allow_raw_context: bool,
}

/// Policy decision for whether a local stub should trigger detail fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubDetailDecision {
    pub should_fetch: bool,
    pub explanation: String,
}

/// Client abstraction for fetching signed detail from a teacher remote.
pub trait RemoteDetailClient: Send + Sync {
    fn fetch_detail(
        &self,
        request: &DetailFetchRequest,
    ) -> impl Future<Output = anyhow::Result<SignedEnvelope<DetailResponsePayload>>> + Send;
}

/// Test/dry-run remote detail client that returns a pre-signed payload.
#[derive(Debug, Clone)]
pub struct MockRemoteDetailClient {
    signed_detail: SignedEnvelope<DetailResponsePayload>,
}

impl MockRemoteDetailClient {
    pub fn signed(signed_detail: SignedEnvelope<DetailResponsePayload>) -> Self {
        Self { signed_detail }
    }
}

impl RemoteDetailClient for MockRemoteDetailClient {
    async fn fetch_detail(
        &self,
        _request: &DetailFetchRequest,
    ) -> anyhow::Result<SignedEnvelope<DetailResponsePayload>> {
        Ok(self.signed_detail.clone())
    }
}

/// Teacher-side in-memory detail grant store.
#[derive(Debug, Clone, Default)]
pub struct TeacherDetailStore {
    records: HashMap<String, DetailGrantRecord>,
}

impl TeacherDetailStore {
    pub fn from_records(records: impl IntoIterator<Item = DetailGrantRecord>) -> Self {
        let mut store = Self::default();
        for record in records {
            let detail_ref = issue_detail_ref(record.clone());
            store.records.insert(detail_ref.token, record);
        }
        store
    }

    pub fn resolve(
        &self,
        detail_ref: &DetailRef,
        packet_id: Uuid,
        item_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<&DetailGrantRecord> {
        if detail_ref.packet_id != packet_id {
            bail!("packet mismatch for detail ref");
        }
        if detail_ref.item_id != item_id {
            bail!("item mismatch for detail ref");
        }
        if detail_ref.expires_at <= now {
            bail!("detail ref expired");
        }
        let record = self
            .records
            .get(&detail_ref.token)
            .ok_or_else(|| anyhow!("unknown detail ref"))?;
        if record.remote_id != detail_ref.remote_id
            || record.packet_id != detail_ref.packet_id
            || record.item_id != detail_ref.item_id
            || record.detail_hash != detail_ref.detail_hash
        {
            bail!("detail ref grant binding mismatch");
        }
        if record.expires_at <= now {
            bail!("detail grant expired");
        }
        Ok(record)
    }
}

/// Issue an opaque learner-facing ref from a teacher-side grant record.
pub fn issue_detail_ref(record: DetailGrantRecord) -> DetailRef {
    let token_material = format!(
        "{}:{}:{}:{}:{}:{}",
        record.remote_id,
        record.packet_id,
        record.item_id,
        record.source_ref,
        record.grants.join(","),
        record.expires_at.timestamp_millis()
    );
    DetailRef {
        remote_id: record.remote_id,
        packet_id: record.packet_id,
        item_id: record.item_id,
        token: ContentHash::sha256_bytes(token_material.as_bytes()).0,
        detail_hash: record.detail_hash,
        more_available: true,
        expires_at: record.expires_at,
    }
}

/// Fetch signed detail, verifying teacher signature, ref binding, hash, and learner policy.
pub async fn fetch_detail<C: RemoteDetailClient>(
    client: &C,
    request: DetailFetchRequest,
) -> anyhow::Result<SignedEnvelope<DetailResponsePayload>> {
    let item = PolicyItem::new(request.detail_ref.item_id.to_string(), "knowledge");
    let decision = request.policy.can_fetch_detail(&request.remote_name, &item);
    if !decision.allowed || !has_grant(&request.learner_grants, "detail_fetch") {
        bail!("detail fetch denied: {}", decision.explanation);
    }

    let signed = client.fetch_detail(&request).await?;
    signed.verify(&request.public_identity)?;
    let payload = &signed.payload;
    if payload.remote_id != request.detail_ref.remote_id
        || payload.packet_id != request.detail_ref.packet_id
        || payload.item_id != request.detail_ref.item_id
        || payload.detail_ref != request.detail_ref
    {
        bail!("signed detail response does not match requested detail ref");
    }
    if ContentHash::sha256_bytes(payload.detail.as_bytes()) != request.detail_ref.detail_hash {
        bail!("signed detail response hash does not match detail ref");
    }
    if payload.kind == DetailKind::RawContext
        && (!request.allow_raw_context || !has_grant(&request.learner_grants, "raw_context"))
    {
        bail!("raw context detail denied without explicit teacher and learner grants");
    }
    Ok(signed)
}

/// Decide whether a local stub has enough summary context or should fetch detail.
pub fn decide_stub_detail_fetch(
    stub: &RemoteStub,
    query: &str,
    policy: &RemotePolicy,
    remote_name: &str,
) -> StubDetailDecision {
    let Some(detail_ref) = &stub.detail_ref else {
        return StubDetailDecision {
            should_fetch: false,
            explanation: "stub has no detail ref".into(),
        };
    };
    if summary_satisfies_query(&stub.summary, query) {
        return StubDetailDecision {
            should_fetch: false,
            explanation: "stub summary is sufficient for this simple query".into(),
        };
    }

    let namespace = stub
        .summary
        .split_whitespace()
        .next()
        .unwrap_or("knowledge")
        .to_lowercase();
    let item = PolicyItem::new(detail_ref.item_id.to_string(), namespace);
    let decision = policy.can_fetch_detail(remote_name, &item);
    StubDetailDecision {
        should_fetch: decision.allowed,
        explanation: if decision.allowed {
            format!(
                "detail fetch grant permits retrieving stub detail: {}",
                decision.explanation
            )
        } else {
            format!(
                "no detail fetch grant or policy denied detail: {}",
                decision.explanation
            )
        },
    }
}

fn summary_satisfies_query(summary: &str, query: &str) -> bool {
    let query_lc = query.to_lowercase();
    let summary_lc = summary.to_lowercase();
    let asks_for_detail = ["exact", "why", "how", "flags", "full", "source", "raw"]
        .iter()
        .any(|marker| query_lc.contains(marker));
    if asks_for_detail {
        return false;
    }
    query_lc
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| term.len() >= 4)
        .any(|term| summary_lc.contains(term))
}

fn has_grant(grants: &[String], grant: &str) -> bool {
    grants.iter().any(|g| g == grant)
}

/// Map a detail payload kind to its teaching item bucket.
pub fn teaching_kind_for_detail(kind: DetailKind) -> TeachingKind {
    match kind {
        DetailKind::Knowledge => TeachingKind::Summary,
        DetailKind::RawContext => TeachingKind::Summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_identity::{ContentHash, InstanceId, InstanceSigningIdentity};
    use crate::remotes::policy::{PolicyAction, PolicyFact, RemotePolicy};
    use crate::remotes::types::{
        ApplicabilityFrame, DetailRef, ImportState, RemoteStub, SafetyClassification, SafetyRisk,
    };
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn policy_with_detail(remote: &str) -> RemotePolicy {
        RemotePolicy::from_facts([
            PolicyFact::remote(remote),
            PolicyFact::grant(remote, PolicyAction::DetailFetch, "knowledge"),
        ])
    }

    fn applicability(namespace: &str) -> ApplicabilityFrame {
        ApplicabilityFrame {
            namespaces: vec![namespace.into()],
            host_os: None,
            container_runtime: None,
            hardware: vec![],
            required_tags: vec![],
            excluded_tags: vec![],
            confidence: 1.0,
        }
    }

    fn safe() -> SafetyClassification {
        SafetyClassification {
            risk: SafetyRisk::Low,
            reasons: vec!["test".into()],
            redacted: false,
            requires_human: false,
        }
    }

    fn stub(detail_ref: DetailRef, summary: &str) -> RemoteStub {
        let now = Utc::now();
        RemoteStub {
            stub_id: id(100),
            remote_id: detail_ref.remote_id,
            packet_id: detail_ref.packet_id,
            item_id: detail_ref.item_id,
            local_entity_id: None,
            title: "remote gpu fact".into(),
            summary: summary.into(),
            state: ImportState::ActiveStub,
            detail_ref: Some(detail_ref),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn detail_ref_is_opaque_and_does_not_encode_raw_source_id() {
        let detail_ref = issue_detail_ref(DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "context_segment:raw-secret-source-id".into(),
            detail_hash: ContentHash("hash".into()),
            grants: vec!["detail_fetch".into()],
            expires_at: Utc::now() + Duration::minutes(5),
        });

        assert!(!detail_ref.token.contains("raw-secret-source-id"));
        assert!(
            !serde_json::to_string(&detail_ref)
                .unwrap()
                .contains("raw-secret-source-id")
        );
    }

    #[test]
    fn expired_detail_ref_is_rejected() {
        let record = DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "entity:abc".into(),
            detail_hash: ContentHash("hash".into()),
            grants: vec!["detail_fetch".into()],
            expires_at: Utc::now() - Duration::seconds(1),
        };
        let detail_ref = issue_detail_ref(record.clone());
        let store = TeacherDetailStore::from_records([record]);

        let err = store
            .resolve(
                &detail_ref,
                detail_ref.packet_id,
                detail_ref.item_id,
                Utc::now(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn detail_ref_for_item_a_cannot_fetch_item_b() {
        let record = DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "entity:a".into(),
            detail_hash: ContentHash("hash".into()),
            grants: vec!["detail_fetch".into()],
            expires_at: Utc::now() + Duration::minutes(5),
        };
        let detail_ref = issue_detail_ref(record.clone());
        let store = TeacherDetailStore::from_records([record]);

        let err = store
            .resolve(&detail_ref, detail_ref.packet_id, id(4), Utc::now())
            .unwrap_err();
        assert!(err.to_string().contains("item mismatch"));
    }

    #[tokio::test]
    async fn trusted_personal_remote_detail_fetch_succeeds() {
        let teacher = InstanceSigningIdentity::generate(InstanceId(id(500)));
        let detail_ref = issue_detail_ref(DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "entity:a".into(),
            detail_hash: ContentHash::sha256_bytes(b"expanded detail"),
            grants: vec!["detail_fetch".into()],
            expires_at: Utc::now() + Duration::minutes(5),
        });
        let client = MockRemoteDetailClient::signed(
            teacher
                .sign(DetailResponsePayload {
                    remote_id: id(1),
                    packet_id: id(2),
                    item_id: id(3),
                    detail_ref: detail_ref.clone(),
                    detail: "expanded detail".into(),
                    kind: DetailKind::Knowledge,
                    safety: safe(),
                    applicability: applicability("gpu_builds"),
                    created_at: Utc::now(),
                })
                .unwrap(),
        );

        let fetched = fetch_detail(
            &client,
            DetailFetchRequest {
                remote_name: "gpu".into(),
                detail_ref,
                policy: policy_with_detail("gpu"),
                public_identity: teacher.public_identity(),
                learner_grants: vec!["detail_fetch".into()],
                allow_raw_context: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(fetched.payload.detail, "expanded detail");
    }

    #[tokio::test]
    async fn team_detail_fetch_without_grant_fails() {
        let teacher = InstanceSigningIdentity::generate(InstanceId(id(501)));
        let detail_ref = issue_detail_ref(DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "entity:a".into(),
            detail_hash: ContentHash::sha256_bytes(b"expanded detail"),
            grants: vec!["detail_fetch".into()],
            expires_at: Utc::now() + Duration::minutes(5),
        });
        let client = MockRemoteDetailClient::signed(
            teacher
                .sign(DetailResponsePayload {
                    remote_id: id(1),
                    packet_id: id(2),
                    item_id: id(3),
                    detail_ref: detail_ref.clone(),
                    detail: "expanded detail".into(),
                    kind: DetailKind::Knowledge,
                    safety: safe(),
                    applicability: applicability("team"),
                    created_at: Utc::now(),
                })
                .unwrap(),
        );

        let err = fetch_detail(
            &client,
            DetailFetchRequest {
                remote_name: "team".into(),
                detail_ref,
                policy: RemotePolicy::from_facts([PolicyFact::remote("team")]),
                public_identity: teacher.public_identity(),
                learner_grants: vec![],
                allow_raw_context: false,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("detail fetch denied"));
    }

    #[tokio::test]
    async fn raw_context_detail_requires_teacher_and_learner_grants() {
        let teacher = InstanceSigningIdentity::generate(InstanceId(id(502)));
        let detail_ref = issue_detail_ref(DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "context_segment:abc".into(),
            detail_hash: ContentHash::sha256_bytes(b"raw context detail"),
            grants: vec!["detail_fetch".into(), "raw_context".into()],
            expires_at: Utc::now() + Duration::minutes(5),
        });
        let response = teacher
            .sign(DetailResponsePayload {
                remote_id: id(1),
                packet_id: id(2),
                item_id: id(3),
                detail_ref: detail_ref.clone(),
                detail: "raw context detail".into(),
                kind: DetailKind::RawContext,
                safety: safe(),
                applicability: applicability("gpu_builds"),
                created_at: Utc::now(),
            })
            .unwrap();
        let client = MockRemoteDetailClient::signed(response.clone());

        let denied = fetch_detail(
            &client,
            DetailFetchRequest {
                remote_name: "gpu".into(),
                detail_ref: detail_ref.clone(),
                policy: policy_with_detail("gpu"),
                public_identity: teacher.public_identity(),
                learner_grants: vec!["detail_fetch".into()],
                allow_raw_context: true,
            },
        )
        .await
        .unwrap_err();
        assert!(denied.to_string().contains("raw context"));

        let allowed = fetch_detail(
            &MockRemoteDetailClient::signed(response),
            DetailFetchRequest {
                remote_name: "gpu".into(),
                detail_ref,
                policy: policy_with_detail("gpu"),
                public_identity: teacher.public_identity(),
                learner_grants: vec!["detail_fetch".into(), "raw_context".into()],
                allow_raw_context: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(allowed.payload.kind, DetailKind::RawContext);
    }

    #[test]
    fn stub_summary_is_enough_for_simple_query() {
        let detail_ref = issue_detail_ref(DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "entity:a".into(),
            detail_hash: ContentHash("hash".into()),
            grants: vec!["detail_fetch".into()],
            expires_at: Utc::now() + Duration::minutes(5),
        });

        let decision = decide_stub_detail_fetch(
            &stub(detail_ref, "Use CUDA 12.4.1 image for GPU builds"),
            "which CUDA image?",
            &policy_with_detail("gpu"),
            "gpu",
        );

        assert!(!decision.should_fetch);
        assert!(decision.explanation.contains("stub summary is sufficient"));
    }

    #[test]
    fn complex_query_over_stub_triggers_detail_fetch_when_policy_allows() {
        let detail_ref = issue_detail_ref(DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "entity:a".into(),
            detail_hash: ContentHash("hash".into()),
            grants: vec!["detail_fetch".into()],
            expires_at: Utc::now() + Duration::minutes(5),
        });

        let decision = decide_stub_detail_fetch(
            &stub(detail_ref, "CUDA build image"),
            "give me exact Dockerfile flags and why linking fails",
            &policy_with_detail("gpu"),
            "gpu",
        );

        assert!(decision.should_fetch);
        assert!(decision.explanation.contains("detail fetch grant permits"));
    }

    #[test]
    fn stub_explanation_says_why_detail_was_not_fetched() {
        let detail_ref = issue_detail_ref(DetailGrantRecord {
            remote_id: id(1),
            packet_id: id(2),
            item_id: id(3),
            source_ref: "entity:a".into(),
            detail_hash: ContentHash("hash".into()),
            grants: vec!["detail_fetch".into()],
            expires_at: Utc::now() + Duration::minutes(5),
        });

        let decision = decide_stub_detail_fetch(
            &stub(detail_ref, "CUDA build image"),
            "give me exact Dockerfile flags and why linking fails",
            &RemotePolicy::from_facts([PolicyFact::remote("gpu")]),
            "gpu",
        );

        assert!(!decision.should_fetch);
        assert!(decision.explanation.contains("no detail fetch grant"));
    }
}
