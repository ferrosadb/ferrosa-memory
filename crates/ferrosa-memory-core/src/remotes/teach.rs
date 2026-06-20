//! Module: Teacher-side remote memory query planning and stream events.
//! Correctness: Correct when teacher query events start before retrieval completion, compact items preserve provenance hashes, and policy gates prevent raw/detail/skill leakage by default.
//! Last revised: 2026-05-12
//! Last changed: Implemented Packet E teacher-side planning, stream events, and policy gates.

use anyhow::{anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::remote_identity::{ContentHash, InstanceId};
use crate::remotes::safety::classify_safety;
use crate::remotes::types::{
    ApplicabilityFrame, DetailRef, SafetyClassification, TeachingItem, TeachingKind, TeachingPacket,
};
use crate::storage::Storage;
use crate::types::{EntityEntry, MemoryState, TenantContext};

/// Teacher-side request for a streaming remote query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeachQueryRequest {
    pub remote_id: Uuid,
    pub learner_instance_id: InstanceId,
    pub query: String,
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default = "default_max_items")]
    pub max_items: i32,
    #[serde(default)]
    pub query_embedding: Option<Vec<f32>>,
    #[serde(default)]
    pub grants: Vec<String>,
    #[serde(default)]
    pub include_raw_context: bool,
    #[serde(default)]
    pub include_detail: bool,
    #[serde(default)]
    pub include_skill: bool,
}

fn default_max_items() -> i32 {
    8
}

/// Transport-neutral stream event. MCP stdio can return these as a JSON array;
/// future streaming transports can emit each variant as it is produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TeachQueryEvent {
    Started {
        request_id: Uuid,
        packet_id: Uuid,
        continuation_token: String,
    },
    Item {
        packet_id: Uuid,
        item: Box<TeachingItem>,
        continuation_token: String,
    },
    Error {
        packet_id: Uuid,
        partial_packet_id: Option<Uuid>,
        message: String,
        signed_negative: bool,
        continuation_token: Option<String>,
    },
    Completed {
        packet: TeachingPacket,
        continuation_token: Option<String>,
    },
}

impl TeachQueryEvent {
    pub fn continuation_token(&self) -> Option<&str> {
        match self {
            Self::Started {
                continuation_token, ..
            }
            | Self::Item {
                continuation_token, ..
            } => Some(continuation_token),
            Self::Error {
                continuation_token, ..
            } => continuation_token.as_deref(),
            Self::Completed {
                continuation_token, ..
            } => continuation_token.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeachStreamControl {
    Continue,
    Stop,
}

/// Normalized teacher retrieval hit before compaction into a wire item.
#[derive(Debug, Clone, PartialEq)]
pub struct TeachingSourceHit {
    pub kind: TeachingKind,
    pub title: String,
    pub summary: String,
    pub raw_body: String,
    pub source_ref: String,
    pub source_hash: String,
    pub score: f64,
    pub stale: bool,
    pub superseded_by: Option<Uuid>,
}

impl TeachingSourceHit {
    pub fn context_bm25(
        source_ref: impl Into<String>,
        title: impl Into<String>,
        summary: impl Into<String>,
        raw_body: impl Into<String>,
        source_hash: impl Into<String>,
        score: f64,
    ) -> Self {
        Self {
            kind: TeachingKind::Summary,
            title: title.into(),
            summary: summary.into(),
            raw_body: raw_body.into(),
            source_ref: source_ref.into(),
            source_hash: source_hash.into(),
            score,
            stale: false,
            superseded_by: None,
        }
    }

    pub fn entity_vector(
        entity_id: Uuid,
        title: impl Into<String>,
        summary: impl Into<String>,
        source_hash: impl Into<String>,
        score: f64,
        kind: TeachingKind,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            summary: summary.into(),
            raw_body: String::new(),
            source_ref: format!("entity:{entity_id}"),
            source_hash: source_hash.into(),
            score,
            stale: false,
            superseded_by: None,
        }
    }

    pub fn stale(mut self) -> Self {
        self.stale = true;
        self
    }

    pub fn superseded_by(mut self, entity_id: Uuid) -> Self {
        self.superseded_by = Some(entity_id);
        self
    }
}

/// Compact ranked hits into signed TeachingItems without leaking raw context by default.
pub fn plan_teaching_items(
    packet_id: Uuid,
    request: &TeachQueryRequest,
    mut hits: Vec<TeachingSourceHit>,
) -> anyhow::Result<Vec<TeachingItem>> {
    enforce_policy(request)?;
    if hits.is_empty() {
        return Ok(vec![negative_item(
            packet_id,
            request,
            "No remote memory hits",
            format!("No remote memory hits for query: {}", request.query),
            "negative:no_hits",
        )]);
    }

    hits.sort_by(|a, b| adjusted_score(b).total_cmp(&adjusted_score(a)));
    let now = Utc::now();
    let max_items = request.max_items.max(1) as usize;
    hits.into_iter()
        .take(max_items)
        .map(|hit| {
            let adjusted = adjusted_score(&hit);
            let mut kind = hit.kind;
            if hit.stale || hit.superseded_by.is_some() {
                kind = TeachingKind::Negative;
            }
            let body = if request.include_raw_context && has_grant(request, "raw_context") {
                Some(hit.raw_body.clone())
            } else {
                None
            };
            let hash = ContentHash::sha256_bytes(
                format!("{}:{}:{}", hit.source_ref, hit.source_hash, hit.summary).as_bytes(),
            );
            let detail_hash = ContentHash::sha256_bytes(hit.raw_body.as_bytes());
            let safety = if body.is_some() {
                classify_safety(&hit.raw_body)
            } else {
                classify_safety(&hit.summary)
            };
            let mut metadata = serde_json::json!({
                "provenance_refs": [hit.source_ref],
                "source_hashes": [hit.source_hash],
                "rank_score": adjusted,
                "signature_hash": hash.0,
            });
            if let Some(superseded_by) = hit.superseded_by {
                metadata["superseded_by"] = serde_json::json!(superseded_by.to_string());
            }
            if hit.stale {
                metadata["stale"] = serde_json::json!(true);
            }
            let item_id = Uuid::new_v4();
            let detail_expires_at = now + chrono::Duration::minutes(15);
            let detail_token = ContentHash::sha256_bytes(
                format!(
                    "{}:{}:{}:{}:{}",
                    request.remote_id,
                    packet_id,
                    item_id,
                    hit.source_ref,
                    detail_expires_at.timestamp_millis()
                )
                .as_bytes(),
            )
            .0;
            Ok(TeachingItem {
                item_id,
                packet_id,
                kind,
                title: hit.title,
                summary: hit.summary,
                body,
                content_hash: hash,
                applicability: applicability(request),
                safety,
                detail_ref: Some(DetailRef {
                    remote_id: request.remote_id,
                    packet_id,
                    item_id,
                    token: detail_token,
                    detail_hash,
                    more_available: true,
                    expires_at: detail_expires_at,
                }),
                metadata,
                created_at: now,
            })
        })
        .collect()
}

/// Execute a teacher query as transport-neutral stream events.
pub async fn teach_query_stream<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    request: TeachQueryRequest,
) -> anyhow::Result<Vec<TeachQueryEvent>> {
    let mut events = Vec::new();
    teach_query_stream_with_sink(storage, ctx, session_id, request, |event| {
        events.push(event);
        TeachStreamControl::Continue
    })
    .await?;
    Ok(events)
}

/// Execute a teacher query and emit each event as it becomes available.
pub async fn teach_query_stream_with_sink<S, F>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    request: TeachQueryRequest,
    mut emit: F,
) -> anyhow::Result<()>
where
    S: Storage,
    F: FnMut(TeachQueryEvent) -> TeachStreamControl,
{
    if request.query.trim().is_empty() {
        bail!("query is required");
    }
    let request_id = Uuid::new_v4();
    let packet_id = Uuid::new_v4();
    if matches!(
        emit(TeachQueryEvent::Started {
            request_id,
            packet_id,
            continuation_token: continuation(packet_id, 0),
        }),
        TeachStreamControl::Stop
    ) {
        return Ok(());
    }

    if let Err(err) = enforce_policy(&request) {
        emit(TeachQueryEvent::Error {
            packet_id,
            partial_packet_id: Some(packet_id),
            message: err.to_string(),
            signed_negative: true,
            continuation_token: Some(continuation(packet_id, 1)),
        });
        return Ok(());
    }

    let hits = match collect_hits(storage, ctx, session_id, &request).await {
        Ok(hits) => hits,
        Err(err) => {
            emit(TeachQueryEvent::Error {
                packet_id,
                partial_packet_id: Some(packet_id),
                message: err.to_string(),
                signed_negative: false,
                continuation_token: Some(continuation(packet_id, 1)),
            });
            return Ok(());
        }
    };
    let items = plan_teaching_items(packet_id, &request, hits)?;
    for (idx, item) in items.iter().cloned().enumerate() {
        if matches!(
            emit(TeachQueryEvent::Item {
                packet_id,
                item: Box::new(item),
                continuation_token: continuation(packet_id, idx + 1),
            }),
            TeachStreamControl::Stop
        ) {
            return Ok(());
        }
    }
    let packet = TeachingPacket {
        packet_id,
        teacher_instance_id: InstanceId(ctx.tenant_id),
        request_id: Some(request_id),
        source_namespace: request
            .namespaces
            .first()
            .cloned()
            .unwrap_or_else(|| "default".into()),
        query: request.query.clone(),
        items,
        expires_at: None,
        created_at: Utc::now(),
    };
    emit(TeachQueryEvent::Completed {
        packet,
        continuation_token: None,
    });
    Ok(())
}

async fn collect_hits<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    request: &TeachQueryRequest,
) -> anyhow::Result<Vec<TeachingSourceHit>> {
    let limit = request.max_items.max(1) as usize;
    let mut hits = Vec::new();
    for segment in storage
        .context_segment_search_bm25(ctx, session_id, &request.query, limit)
        .await?
    {
        hits.push(TeachingSourceHit::context_bm25(
            format!("context_segment:{}", segment.segment_id),
            segment
                .segment_summary
                .clone()
                .unwrap_or_else(|| segment.conversation_id.clone()),
            segment
                .segment_summary
                .unwrap_or_else(|| trim_summary(&segment.segment_text)),
            segment.segment_text,
            segment.content_hash,
            0.75,
        ));
    }
    let entities = if let Some(embedding) = &request.query_embedding {
        storage
            .entity_search_ann(ctx, session_id, embedding, limit)
            .await?
    } else {
        storage
            .entity_find_phonetic(ctx, session_id, &request.query)
            .await?
    };
    for entity in entities {
        hits.push(entity_hit(entity));
    }
    if let Some(embedding) = &request.query_embedding {
        for segment in storage
            .context_segment_search_ann(ctx, session_id, embedding, limit)
            .await?
        {
            hits.push(TeachingSourceHit::context_bm25(
                format!("context_segment:{}", segment.segment_id),
                segment
                    .segment_summary
                    .clone()
                    .unwrap_or_else(|| segment.conversation_id.clone()),
                segment
                    .segment_summary
                    .unwrap_or_else(|| trim_summary(&segment.segment_text)),
                segment.segment_text,
                segment.content_hash,
                0.70,
            ));
        }
    }
    Ok(hits)
}

fn entity_hit(entity: EntityEntry) -> TeachingSourceHit {
    let summary = entity
        .description
        .clone()
        .unwrap_or_else(|| entity.context_snippet.clone());
    let hash = entity
        .content_hash
        .clone()
        .unwrap_or_else(|| ContentHash::sha256_bytes(summary.as_bytes()).0);
    let mut hit = TeachingSourceHit::entity_vector(
        entity.entity_id,
        entity.entity_name,
        summary,
        hash,
        entity.confidence,
        kind_for_entity_type(&entity.entity_type),
    );
    if !matches!(entity.state, MemoryState::Active) {
        hit = hit.stale();
    }
    hit
}

fn kind_for_entity_type(entity_type: &str) -> TeachingKind {
    match entity_type {
        "decision" => TeachingKind::Decision,
        "pattern" => TeachingKind::Pattern,
        "bug" => TeachingKind::Bug,
        "skill" => TeachingKind::SkillStub,
        "procedure" => TeachingKind::ProcedureStub,
        _ => TeachingKind::Fact,
    }
}

fn adjusted_score(hit: &TeachingSourceHit) -> f64 {
    if hit.stale || hit.superseded_by.is_some() {
        hit.score * 0.25
    } else {
        hit.score
    }
}

fn enforce_policy(request: &TeachQueryRequest) -> anyhow::Result<()> {
    let mut denied = Vec::new();
    if request.include_raw_context && !has_grant(request, "raw_context") {
        denied.push("raw_context");
    }
    if request.include_detail && !has_grant(request, "detail") {
        denied.push("detail");
    }
    if request.include_skill && !has_grant(request, "skill") {
        denied.push("skill");
    }
    if denied.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "denied by remote policy: missing grants for {}",
            denied.join(",")
        ))
    }
}

fn has_grant(request: &TeachQueryRequest, grant: &str) -> bool {
    request.grants.iter().any(|g| g == grant)
}

fn negative_item(
    packet_id: Uuid,
    request: &TeachQueryRequest,
    title: impl Into<String>,
    summary: impl Into<String>,
    source_ref: impl Into<String>,
) -> TeachingItem {
    let summary = summary.into();
    let hash = ContentHash::sha256_bytes(format!("{}:{summary}", request.query).as_bytes());
    TeachingItem {
        item_id: Uuid::new_v4(),
        packet_id,
        kind: TeachingKind::Negative,
        title: title.into(),
        summary,
        body: None,
        content_hash: hash.clone(),
        applicability: applicability(request),
        safety: SafetyClassification {
            risk: crate::remotes::types::SafetyRisk::None,
            reasons: vec!["signed negative knowledge".into()],
            redacted: false,
            requires_human: false,
        },
        detail_ref: None,
        metadata: serde_json::json!({
            "provenance_refs": [source_ref.into()],
            "source_hashes": [hash.0],
            "signature_hash": hash.0,
            "rank_score": 0.0,
        }),
        created_at: Utc::now(),
    }
}

fn applicability(request: &TeachQueryRequest) -> ApplicabilityFrame {
    ApplicabilityFrame {
        namespaces: request.namespaces.clone(),
        host_os: None,
        container_runtime: None,
        hardware: Vec::new(),
        required_tags: request
            .namespaces
            .iter()
            .map(|ns| format!("namespace:{ns}"))
            .collect(),
        excluded_tags: Vec::new(),
        confidence: 0.8,
    }
}

fn continuation(packet_id: Uuid, offset: usize) -> String {
    format!("teach:{packet_id}:{offset}")
}

fn trim_summary(text: &str) -> String {
    const MAX: usize = 240;
    if text.len() <= MAX {
        text.to_string()
    } else {
        format!("{}…", &text[..MAX])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{self, SessionState};
    use crate::remote_identity::InstanceId;
    use crate::remotes::types::TeachingKind;
    use crate::storage::mock::MockStorage;
    use crate::types::TenantContext;
    use serde_json::json;
    use uuid::Uuid;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn remote_id() -> Uuid {
        id(7)
    }

    fn request() -> TeachQueryRequest {
        TeachQueryRequest {
            remote_id: remote_id(),
            learner_instance_id: InstanceId(id(8)),
            query: "ferrosa memory remotes".into(),
            namespaces: vec!["ferrosa-memory".into()],
            max_items: 10,
            query_embedding: Some(vec![0.1, 0.2, 0.3]),
            grants: vec![],
            include_raw_context: false,
            include_detail: false,
            include_skill: false,
        }
    }

    fn ctx() -> TenantContext {
        TenantContext {
            tenant_id: id(42),
            session_origin: "test".into(),
        }
    }

    #[test]
    fn bm25_and_vector_hits_become_compact_items_with_provenance_and_hashes() {
        let packet_id = id(100);
        let hits = vec![
            TeachingSourceHit::context_bm25(
                "seg-1",
                "Remote memory plan",
                "BM25 segment summary",
                "full raw context must stay behind detail grants",
                "hash-bm25",
                0.91,
            ),
            TeachingSourceHit::entity_vector(
                id(200),
                "RemoteMemoryPolicy",
                "Entity vector summary",
                "hash-vector",
                0.88,
                TeachingKind::Pattern,
            ),
        ];

        let items = plan_teaching_items(packet_id, &request(), hits).unwrap();

        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.body.is_none()));
        assert!(items.iter().all(|item| item.detail_ref.is_some()));
        assert!(
            items
                .iter()
                .all(|item| item.metadata["provenance_refs"].as_array().unwrap().len() == 1)
        );
        assert_eq!(items[0].metadata["source_hashes"][0], "hash-bm25");
        assert_eq!(items[1].metadata["source_hashes"][0], "hash-vector");
    }

    #[test]
    fn stale_and_superseded_hits_are_negative_or_lower_ranked() {
        let packet_id = id(101);
        let hits = vec![
            TeachingSourceHit::entity_vector(
                id(1),
                "current",
                "current fact",
                "hash-current",
                0.7,
                TeachingKind::Fact,
            ),
            TeachingSourceHit::entity_vector(
                id(2),
                "old",
                "old fact",
                "hash-old",
                0.99,
                TeachingKind::Fact,
            )
            .stale()
            .superseded_by(id(1)),
        ];

        let items = plan_teaching_items(packet_id, &request(), hits).unwrap();

        assert_eq!(items[0].title, "current");
        assert_eq!(items[1].kind, TeachingKind::Negative);
        assert_eq!(items[1].metadata["superseded_by"], id(1).to_string());
        assert!(
            items[1].metadata["rank_score"].as_f64().unwrap()
                < items[0].metadata["rank_score"].as_f64().unwrap()
        );
    }

    #[test]
    fn no_hits_emit_signed_negative_knowledge() {
        let packet_id = id(102);
        let items = plan_teaching_items(packet_id, &request(), vec![]).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, TeachingKind::Negative);
        assert!(items[0].title.contains("No remote memory hits"));
        assert!(items[0].metadata["signature_hash"].as_str().unwrap().len() >= 32);
    }

    #[tokio::test]
    async fn stream_starts_before_retrieval_and_completes_with_continuation_token() {
        let store = MockStorage::new();
        let events = teach_query_stream(&store, &ctx(), id(9), request())
            .await
            .unwrap();

        assert!(matches!(
            events.first(),
            Some(TeachQueryEvent::Started { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(TeachQueryEvent::Completed { .. })
        ));
        assert!(
            events
                .iter()
                .any(|event| event.continuation_token().is_some())
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TeachQueryEvent::Item { .. }))
        );
    }

    #[tokio::test]
    async fn stream_returns_error_event_with_partial_packet_metadata_when_retrieval_fails_after_start()
     {
        let store = MockStorage::new();
        *store.force_phonetic_error.lock().await = Some("boom".into());
        let mut req = request();
        req.query_embedding = None;

        let events = teach_query_stream(&store, &ctx(), id(9), req)
            .await
            .unwrap();

        assert!(matches!(
            events.first(),
            Some(TeachQueryEvent::Started { .. })
        ));
        let error = events
            .iter()
            .find_map(|event| match event {
                TeachQueryEvent::Error {
                    partial_packet_id,
                    message,
                    ..
                } => Some((partial_packet_id, message)),
                _ => None,
            })
            .expect("error event");
        assert!(error.0.is_some());
        assert!(error.1.contains("boom"));
    }

    #[tokio::test]
    async fn packet_l_stream_sink_can_stop_after_started_without_touching_retrieval() {
        let store = MockStorage::new();
        *store.force_phonetic_error.lock().await = Some("retrieval should not run".into());
        let mut req = request();
        req.query_embedding = None;
        let mut events = Vec::new();

        teach_query_stream_with_sink(&store, &ctx(), id(9), req, |event| {
            events.push(event);
            TeachStreamControl::Stop
        })
        .await
        .unwrap();

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TeachQueryEvent::Started { .. }));
    }

    #[tokio::test]
    async fn packet_l_stream_sink_receives_started_before_retrieval_error() {
        let store = MockStorage::new();
        *store.force_phonetic_error.lock().await = Some("boom".into());
        let mut req = request();
        req.query_embedding = None;
        let mut events = Vec::new();

        teach_query_stream_with_sink(&store, &ctx(), id(9), req, |event| {
            events.push(event);
            TeachStreamControl::Continue
        })
        .await
        .unwrap();

        assert!(matches!(
            events.first(),
            Some(TeachQueryEvent::Started { .. })
        ));
        assert!(matches!(
            events.get(1),
            Some(TeachQueryEvent::Error { message, .. }) if message.contains("boom")
        ));
    }

    #[tokio::test]
    async fn raw_context_detail_and_skill_are_denied_without_grants() {
        let store = MockStorage::new();
        let mut req = request();
        req.include_raw_context = true;
        req.include_detail = true;
        req.include_skill = true;

        let events = teach_query_stream(&store, &ctx(), id(9), req)
            .await
            .unwrap();

        assert!(matches!(
            events.first(),
            Some(TeachQueryEvent::Started { .. })
        ));
        let error = events
            .iter()
            .find_map(|event| match event {
                TeachQueryEvent::Error {
                    message,
                    signed_negative,
                    ..
                } => Some((message, signed_negative)),
                _ => None,
            })
            .expect("policy error");
        assert!(error.0.contains("raw_context"));
        assert!(*error.1);
    }

    #[tokio::test]
    async fn teach_query_stream_tool_is_listed_and_schema_rejects_missing_query_or_remote() {
        let tools = dispatch::tool_definitions(&["concept".into()]);
        let tool = tools
            .iter()
            .find(|tool| tool.name == "teach_query_stream")
            .expect("tool listed");
        assert_eq!(tool.input_schema["required"], json!(["remote_id", "query"]));

        let store = MockStorage::new();
        let session = SessionState::default();
        let err = dispatch::dispatch(
            "tools/call",
            json!({"name": "teach_query_stream", "arguments": {"remote_id": remote_id().to_string()}}),
            &store,
            &ctx(),
            &session,
        ).await.unwrap_err();
        assert_eq!(err.0, crate::transport::INVALID_PARAMS);
    }
}
