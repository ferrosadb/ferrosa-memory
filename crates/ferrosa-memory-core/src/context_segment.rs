//! Temporal semantic context segments.
//!
//! Context segments are raw, ordered conversation pages persisted before
//! compaction. They are searched with lexical + vector signals, then expanded
//! through temporal previous/next links.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextMessage {
    pub role: String,
    pub content: String,
    pub turn_index: i32,
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SegmentationConfig {
    pub strategy: String,
    pub target_tokens: i32,
    pub max_tokens: i32,
    pub time_gap_seconds: i64,
    pub semantic_drift_threshold: f32,
}

impl Default for SegmentationConfig {
    fn default() -> Self {
        Self {
            strategy: "deterministic_v1".into(),
            target_tokens: 1000,
            max_tokens: 1800,
            time_gap_seconds: 15 * 60,
            semantic_drift_threshold: 0.72,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextSegment {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub segment_id: Uuid,
    pub source_session: Uuid,
    pub source_fold_id: Option<Uuid>,
    pub conversation_id: String,
    pub segment_index: i32,
    pub start_turn: i32,
    pub end_turn: i32,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub segment_text: String,
    pub segment_summary: Option<String>,
    pub bm25_text: String,
    pub segment_embedding: Option<Vec<f32>>,
    pub token_count: i32,
    pub content_hash: String,
    pub prev_segment_id: Option<Uuid>,
    pub next_segment_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TemporalEdge {
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub src_id: Uuid,
    pub edge_type: String,
    pub dst_id: Uuid,
    pub relation_time: DateTime<Utc>,
    pub ordinal: i32,
    pub metadata: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestContextSegmentsParams {
    pub session_id: Uuid,
    pub conversation_id: String,
    pub messages: Vec<ContextMessage>,
    #[serde(default)]
    pub segmentation: SegmentationConfig,
    #[serde(default)]
    pub embed_missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentIngestRow {
    pub segment_id: Uuid,
    pub segment_index: i32,
    pub start_turn: i32,
    pub end_turn: i32,
    pub prev_segment_id: Option<Uuid>,
    pub next_segment_id: Option<Uuid>,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentIngestResult {
    pub segments_created: usize,
    pub segments_skipped: usize,
    pub segments: Vec<SegmentIngestRow>,
    pub edges_created: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSegmentSearchParams {
    pub session_id: Uuid,
    pub query: String,
    pub query_embedding: Option<Vec<f32>>,
    pub limit: usize,
    pub expand_prev: usize,
    pub expand_next: usize,
    pub max_expanded_tokens: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSegmentSearchHit {
    pub segment: ContextSegment,
    pub score: f64,
    pub sources: Vec<String>,
    pub expanded_context: Vec<ContextWindowSegment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSegmentSearchResult {
    pub results: Vec<ContextSegmentSearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextWindowParams {
    pub session_id: Uuid,
    pub segment_id: Uuid,
    pub prev: usize,
    pub next: usize,
    pub max_tokens: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextWindowSegment {
    pub direction: String,
    pub segment: ContextSegment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextWindowResult {
    pub segments: Vec<ContextWindowSegment>,
    pub token_count: i32,
}

pub fn segment_messages(
    session_id: Uuid,
    conversation_id: &str,
    messages: &[ContextMessage],
    config: &SegmentationConfig,
) -> anyhow::Result<Vec<ContextSegment>> {
    if messages.is_empty() {
        bail!("messages must not be empty");
    }
    let max_tokens = config.max_tokens.max(1);
    let target_tokens = config.target_tokens.max(1).min(max_tokens);
    let mut segments = Vec::new();
    let mut current: Vec<ContextMessage> = Vec::new();
    let mut current_tokens = 0;
    let mut last_time: Option<DateTime<Utc>> = None;

    for message in messages {
        let message_tokens = estimate_tokens(&message.content).max(1);
        let time_gap = match (last_time, message.created_at) {
            (Some(prev), Some(now)) => (now - prev).num_seconds() > config.time_gap_seconds,
            _ => false,
        };
        let would_exceed_target =
            !current.is_empty() && current_tokens + message_tokens > target_tokens;
        let would_exceed_max = !current.is_empty() && current_tokens + message_tokens > max_tokens;
        if time_gap || would_exceed_target || would_exceed_max {
            push_segment(session_id, conversation_id, &mut segments, &current)?;
            current.clear();
            current_tokens = 0;
        }
        current.push(message.clone());
        current_tokens += message_tokens;
        last_time = message.created_at;
    }
    if !current.is_empty() {
        push_segment(session_id, conversation_id, &mut segments, &current)?;
    }

    for i in 0..segments.len() {
        if i > 0 {
            segments[i].prev_segment_id = Some(segments[i - 1].segment_id);
        }
        if i + 1 < segments.len() {
            segments[i].next_segment_id = Some(segments[i + 1].segment_id);
        }
    }
    Ok(segments)
}

fn push_segment(
    session_id: Uuid,
    conversation_id: &str,
    segments: &mut Vec<ContextSegment>,
    messages: &[ContextMessage],
) -> anyhow::Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let segment_index = segments.len() as i32;
    let start_turn = messages.first().unwrap().turn_index;
    let end_turn = messages.last().unwrap().turn_index;
    let segment_text = messages
        .iter()
        .map(|m| format!("{}[{}]: {}", m.role, m.turn_index, m.content.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    let token_count: i32 = messages
        .iter()
        .map(|m| estimate_tokens(&m.content).max(1))
        .sum();
    let content_hash = content_hash(conversation_id, &segment_text, start_turn, end_turn);
    let segment_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, content_hash.as_bytes());
    segments.push(ContextSegment {
        tenant_id: Uuid::nil(),
        session_id,
        segment_id,
        source_session: session_id,
        source_fold_id: None,
        conversation_id: conversation_id.into(),
        segment_index,
        start_turn,
        end_turn,
        start_time: messages.first().and_then(|m| m.created_at),
        end_time: messages.last().and_then(|m| m.created_at),
        segment_text: segment_text.clone(),
        segment_summary: None,
        bm25_text: segment_text.to_lowercase(),
        segment_embedding: None,
        token_count,
        content_hash,
        prev_segment_id: None,
        next_segment_id: None,
        created_at: Utc::now(),
    });
    Ok(())
}

fn estimate_tokens(text: &str) -> i32 {
    let words = text.split_whitespace().count() as i32;
    words.max(((text.chars().count() as f32) / 4.0).ceil() as i32)
}

fn content_hash(
    conversation_id: &str,
    segment_text: &str,
    start_turn: i32,
    end_turn: i32,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(conversation_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(start_turn.to_be_bytes());
    hasher.update(end_turn.to_be_bytes());
    hasher.update(b"\0");
    hasher.update(segment_text.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

pub async fn ingest_context_segments<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    params: IngestContextSegmentsParams,
    embeddings: Option<Vec<Vec<f32>>>,
) -> anyhow::Result<SegmentIngestResult> {
    let mut segments = segment_messages(
        params.session_id,
        &params.conversation_id,
        &params.messages,
        &params.segmentation,
    )?;
    let mut rows = Vec::new();
    let mut created = 0;
    let mut skipped = 0;
    let mut edges_created = 0;
    let mut warnings = Vec::new();

    if let Some(embeddings) = embeddings.as_ref() {
        if embeddings.len() != segments.len() {
            bail!(
                "embedding count {} did not match segment count {}",
                embeddings.len(),
                segments.len()
            );
        }
    } else if params.embed_missing {
        warnings.push(
            "embed_missing requested but no embedding client/vector batch was supplied".into(),
        );
    }

    for (idx, segment) in segments.iter_mut().enumerate() {
        segment.tenant_id = ctx.tenant_id;
        if let Some(embeddings) = embeddings.as_ref() {
            segment.segment_embedding = Some(embeddings[idx].clone());
        }
        if storage
            .context_segment_get_by_hash(ctx, params.session_id, &segment.content_hash)
            .await?
            .is_some()
        {
            skipped += 1;
        } else {
            storage.context_segment_put(ctx, segment).await?;
            created += 1;
        }
        rows.push(SegmentIngestRow {
            segment_id: segment.segment_id,
            segment_index: segment.segment_index,
            start_turn: segment.start_turn,
            end_turn: segment.end_turn,
            prev_segment_id: segment.prev_segment_id,
            next_segment_id: segment.next_segment_id,
            content_hash: segment.content_hash.clone(),
        });
    }

    for pair in segments.windows(2) {
        let left = &pair[0];
        let right = &pair[1];
        let next = TemporalEdge {
            tenant_id: ctx.tenant_id,
            session_id: params.session_id,
            src_id: left.segment_id,
            edge_type: "next_context_segment".into(),
            dst_id: right.segment_id,
            relation_time: right.start_time.unwrap_or_else(Utc::now),
            ordinal: right.segment_index,
            metadata: format!("conversation_id={}", params.conversation_id),
            created_at: Utc::now(),
        };
        let prev = TemporalEdge {
            tenant_id: ctx.tenant_id,
            session_id: params.session_id,
            src_id: right.segment_id,
            edge_type: "previous_context_segment".into(),
            dst_id: left.segment_id,
            relation_time: left.end_time.unwrap_or_else(Utc::now),
            ordinal: left.segment_index,
            metadata: format!("conversation_id={}", params.conversation_id),
            created_at: Utc::now(),
        };
        storage.temporal_edge_put(ctx, &next).await?;
        storage.temporal_edge_put(ctx, &prev).await?;
        edges_created += 2;
    }

    Ok(SegmentIngestResult {
        segments_created: created,
        segments_skipped: skipped,
        segments: rows,
        edges_created,
        warnings,
    })
}

pub async fn search_context_segments<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    params: ContextSegmentSearchParams,
) -> anyhow::Result<ContextSegmentSearchResult> {
    if params.query.trim().is_empty() {
        bail!("query must not be empty");
    }
    let limit = params.limit.clamp(1, 50);
    let bm25 = storage
        .context_segment_search_bm25(ctx, params.session_id, &params.query, limit)
        .await?;
    let ann = match params.query_embedding.as_ref() {
        Some(embedding) => {
            storage
                .context_segment_search_ann(ctx, params.session_id, embedding, limit)
                .await?
        }
        None => Vec::new(),
    };
    let mut scores: HashMap<Uuid, (f64, ContextSegment, HashSet<String>)> = HashMap::new();
    add_ranked(&mut scores, bm25, "bm25", 1.0);
    add_ranked(&mut scores, ann, "ann", 1.0);

    let mut hits: Vec<_> = scores
        .into_values()
        .map(|(score, segment, sources)| (score, segment, sources.into_iter().collect::<Vec<_>>()))
        .collect();
    hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut results = Vec::new();
    for (score, segment, mut sources) in hits.into_iter().take(limit) {
        sources.sort();
        let expanded_context = if params.expand_prev > 0 || params.expand_next > 0 {
            get_context_window(
                storage,
                ctx,
                ContextWindowParams {
                    session_id: params.session_id,
                    segment_id: segment.segment_id,
                    prev: params.expand_prev,
                    next: params.expand_next,
                    max_tokens: params.max_expanded_tokens,
                },
            )
            .await?
            .segments
        } else {
            Vec::new()
        };
        results.push(ContextSegmentSearchHit {
            segment,
            score,
            sources,
            expanded_context,
        });
    }
    Ok(ContextSegmentSearchResult { results })
}

fn add_ranked(
    scores: &mut HashMap<Uuid, (f64, ContextSegment, HashSet<String>)>,
    ranked: Vec<ContextSegment>,
    source: &str,
    weight: f64,
) {
    for (rank, segment) in ranked.into_iter().enumerate() {
        let contribution = weight / (60.0 + rank as f64 + 1.0);
        scores
            .entry(segment.segment_id)
            .and_modify(|(score, _, sources)| {
                *score += contribution;
                sources.insert(source.into());
            })
            .or_insert_with(|| {
                let mut sources = HashSet::new();
                sources.insert(source.into());
                (contribution, segment, sources)
            });
    }
}

pub async fn get_context_window<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    params: ContextWindowParams,
) -> anyhow::Result<ContextWindowResult> {
    let hit = storage
        .context_segment_get(ctx, params.session_id, params.segment_id)
        .await?
        .ok_or_else(|| anyhow!("context segment not found: {}", params.segment_id))?;
    let mut previous = Vec::new();
    let mut cursor = hit.clone();
    for _ in 0..params.prev {
        let Some(prev_id) =
            previous_neighbor(storage, ctx, params.session_id, cursor.segment_id).await?
        else {
            break;
        };
        let Some(prev) = storage
            .context_segment_get(ctx, params.session_id, prev_id)
            .await?
        else {
            break;
        };
        previous.push(prev.clone());
        cursor = prev;
    }
    previous.reverse();

    let mut next = Vec::new();
    let mut cursor = hit.clone();
    for _ in 0..params.next {
        let Some(next_id) =
            next_neighbor(storage, ctx, params.session_id, cursor.segment_id).await?
        else {
            break;
        };
        let Some(n) = storage
            .context_segment_get(ctx, params.session_id, next_id)
            .await?
        else {
            break;
        };
        next.push(n.clone());
        cursor = n;
    }

    let mut token_count = 0;
    let mut window = Vec::new();
    for segment in previous {
        if token_count + segment.token_count > params.max_tokens {
            break;
        }
        token_count += segment.token_count;
        window.push(ContextWindowSegment {
            direction: "previous".into(),
            segment,
        });
    }
    if token_count + hit.token_count <= params.max_tokens || window.is_empty() {
        token_count += hit.token_count;
        window.push(ContextWindowSegment {
            direction: "hit".into(),
            segment: hit,
        });
    }
    for segment in next {
        if token_count + segment.token_count > params.max_tokens {
            break;
        }
        token_count += segment.token_count;
        window.push(ContextWindowSegment {
            direction: "next".into(),
            segment,
        });
    }
    Ok(ContextWindowResult {
        segments: window,
        token_count,
    })
}

async fn previous_neighbor<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    segment_id: Uuid,
) -> anyhow::Result<Option<Uuid>> {
    let edges = storage
        .temporal_edge_list_from(ctx, session_id, segment_id, "previous_context_segment")
        .await?;
    Ok(edges.into_iter().next().map(|e| e.dst_id))
}

async fn next_neighbor<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    segment_id: Uuid,
) -> anyhow::Result<Option<Uuid>> {
    let edges = storage
        .temporal_edge_list_from(ctx, session_id, segment_id, "next_context_segment")
        .await?;
    Ok(edges.into_iter().next().map(|e| e.dst_id))
}

#[allow(dead_code)]
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for i in 0..len {
        let av = a[i] as f64;
        let bv = b[i] as f64;
        dot += av * bv;
        norm_a += av * av;
        norm_b += bv * bv;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a.sqrt() * norm_b.sqrt())
    }
}
