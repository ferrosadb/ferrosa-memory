//! Module: Serializable remote memory transfer types.
//! Correctness: Correct when teaching packets, import state, grants, stubs, provenance, conflicts, feedback, and import batches round-trip through serde with stable wire names.
//! Last revised: 2026-05-12
//! Last changed: Implemented Packet B remote memory data contracts.

use crate::remote_identity::{ContentHash, InstanceId, PublicKeyFingerprint};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Trust bucket assigned by the learner to a configured remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTrustClass {
    Personal,
    Team,
    Partner,
    Public,
    Archive,
}

/// Category of knowledge carried in a teaching item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeachingKind {
    Fact,
    Decision,
    Pattern,
    Bug,
    Summary,
    SkillStub,
    ProcedureStub,
    Negative,
}

/// Learner-side lifecycle state for imported remote memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportState {
    Active,
    ActiveStub,
    NeedsActivation,
    Conflicting,
    Quarantined,
    Superseded,
    Archived,
    Rejected,
}

impl std::fmt::Display for ImportState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        write!(f, "{}", s.trim_matches('"'))
    }
}

/// Safety risk classification assigned before learner-side import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyRisk {
    None,
    Low,
    Medium,
    High,
    Suspected,
    Redacted,
}

/// Feedback categories that can demote trust or quarantine future imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackType {
    Irrelevant,
    WrongScope,
    WrongFact,
    BadSourceNamespace,
    BadProcedure,
    StopSignal,
    PromptInjection,
}

/// Configured remote instance and learner trust metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRemote {
    pub remote_id: Uuid,
    pub instance_id: InstanceId,
    pub name: String,
    pub endpoint: String,
    pub trust_class: RemoteTrustClass,
    pub public_key_fingerprint: PublicKeyFingerprint,
    pub enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Namespaced policy grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteGrant {
    pub namespace: String,
    pub grant: String,
}

/// Namespaced policy denial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDeny {
    pub namespace: String,
    pub deny: String,
}

/// A policy fact attached to one remote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemotePolicyFact {
    pub fact_id: Uuid,
    pub remote_id: Uuid,
    pub kind: RemotePolicyKind,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Policy fact payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePolicyKind {
    Grant(RemoteGrant),
    Deny(RemoteDeny),
}

/// Teacher-side request captured for auditing and deterministic signatures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeachingRequest {
    pub request_id: Uuid,
    pub learner_instance_id: InstanceId,
    pub query: String,
    pub namespaces: Vec<String>,
    pub max_items: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Signed envelope payload returned by a teacher.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeachingPacket {
    pub packet_id: Uuid,
    pub teacher_instance_id: InstanceId,
    pub request_id: Option<Uuid>,
    pub source_namespace: String,
    pub query: String,
    pub items: Vec<TeachingItem>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// One transferrable memory item inside a packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeachingItem {
    pub item_id: Uuid,
    pub packet_id: Uuid,
    pub kind: TeachingKind,
    pub title: String,
    pub summary: String,
    pub body: Option<String>,
    pub content_hash: ContentHash,
    pub applicability: ApplicabilityFrame,
    pub safety: SafetyClassification,
    pub detail_ref: Option<DetailRef>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Applicability scope used to prevent Linux/GPU facts from silently activating on macOS, etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplicabilityFrame {
    pub namespaces: Vec<String>,
    pub host_os: Option<String>,
    pub container_runtime: Option<String>,
    pub hardware: Vec<String>,
    pub required_tags: Vec<String>,
    pub excluded_tags: Vec<String>,
    pub confidence: f64,
}

/// Classifier output attached to a teaching item before import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyClassification {
    pub risk: SafetyRisk,
    pub reasons: Vec<String>,
    pub redacted: bool,
    pub requires_human: bool,
}

/// Capability reference for fetching more detail without sending raw context by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailRef {
    pub remote_id: Uuid,
    pub item_id: Uuid,
    pub detail_hash: ContentHash,
    pub more_available: bool,
}

/// Learner-side stub for a remote item whose details are deferred or gated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteStub {
    pub stub_id: Uuid,
    pub remote_id: Uuid,
    pub packet_id: Uuid,
    pub item_id: Uuid,
    pub local_entity_id: Option<Uuid>,
    pub title: String,
    pub summary: String,
    pub state: ImportState,
    pub detail_ref: Option<DetailRef>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Provenance row tying local memory to a signed remote item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryProvenance {
    pub provenance_id: Uuid,
    pub local_entity_id: Uuid,
    pub remote_id: Uuid,
    pub packet_id: Uuid,
    pub item_id: Uuid,
    pub content_hash: ContentHash,
    pub signature_hash: ContentHash,
    pub imported_at: chrono::DateTime<chrono::Utc>,
}

/// Conflict detected between remote content and local memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryConflict {
    pub conflict_id: Uuid,
    pub local_entity_id: Uuid,
    pub remote_id: Uuid,
    pub item_id: Uuid,
    pub reason: String,
    pub resolved: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// User/agent feedback about a remote memory import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryFeedback {
    pub feedback_id: Uuid,
    pub remote_id: Uuid,
    pub target_id: Uuid,
    pub feedback_type: FeedbackType,
    pub note: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Learner-side import batch summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportBatch {
    pub batch_id: Uuid,
    pub remote_id: Uuid,
    pub packet_id: Uuid,
    pub state: ImportState,
    pub imported_count: i32,
    pub rejected_count: i32,
    pub conflict_count: i32,
    pub explanation: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_identity::{ContentHash, InstanceId};
    use serde_json::json;
    use uuid::Uuid;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn teaching_packet_round_trips_with_items() {
        let packet = TeachingPacket {
            packet_id: id(1),
            teacher_instance_id: InstanceId(id(2)),
            request_id: Some(id(3)),
            source_namespace: "gpu_builds".into(),
            query: "fix cuda build".into(),
            items: vec![TeachingItem {
                item_id: id(4),
                packet_id: id(1),
                kind: TeachingKind::Decision,
                title: "Use pinned CUDA image".into(),
                summary: "Pinned image avoids toolchain drift".into(),
                body: Some("nvidia/cuda:12.4.1-devel".into()),
                content_hash: ContentHash("abc123".into()),
                applicability: ApplicabilityFrame {
                    namespaces: vec!["gpu_builds".into()],
                    host_os: Some("linux".into()),
                    container_runtime: Some("docker".into()),
                    hardware: vec!["nvidia".into()],
                    required_tags: vec!["cuda".into()],
                    excluded_tags: vec!["macos".into()],
                    confidence: 0.91,
                },
                safety: SafetyClassification {
                    risk: SafetyRisk::Low,
                    reasons: vec!["build-only".into()],
                    redacted: false,
                    requires_human: false,
                },
                detail_ref: Some(DetailRef {
                    remote_id: id(5),
                    item_id: id(4),
                    detail_hash: ContentHash("def456".into()),
                    more_available: true,
                }),
                metadata: json!({"source": "test"}),
                created_at: chrono::Utc::now(),
            }],
            expires_at: None,
            created_at: chrono::Utc::now(),
        };

        let encoded = serde_json::to_string(&packet).unwrap();
        let decoded: TeachingPacket = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.items[0].safety.risk, SafetyRisk::Low);
        assert_eq!(
            decoded.items[0].applicability.namespaces,
            vec!["gpu_builds"]
        );
    }

    #[test]
    fn import_state_has_stable_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_string(&ImportState::ActiveStub).unwrap(),
            "\"active_stub\""
        );
        assert_eq!(
            serde_json::to_string(&ImportState::NeedsActivation).unwrap(),
            "\"needs_activation\""
        );
    }

    #[test]
    fn remote_policy_facts_serialize_namespace_and_kind() {
        let grant = RemotePolicyFact {
            fact_id: id(10),
            remote_id: id(11),
            kind: RemotePolicyKind::Grant(RemoteGrant {
                namespace: "gpu_builds".into(),
                grant: "autocommit".into(),
            }),
            created_at: chrono::Utc::now(),
            expires_at: None,
        };
        let value = serde_json::to_value(&grant).unwrap();
        assert_eq!(value["kind"]["grant"]["namespace"], "gpu_builds");
        assert_eq!(value["kind"]["grant"]["grant"], "autocommit");
    }
}
