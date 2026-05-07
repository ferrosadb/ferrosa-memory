use chrono::{Duration, Utc};
use ferrosa_memory_core::context_segment::{
    ContextMessage, ContextSegmentSearchParams, ContextWindowParams, IngestContextSegmentsParams,
    SegmentationConfig, get_context_window, ingest_context_segments, search_context_segments,
    segment_messages,
};
use ferrosa_memory_core::storage::{Storage, mock::MockStorage};
use ferrosa_memory_core::types::TenantContext;
use uuid::Uuid;

fn ctx() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "context-segments-test".into(),
    }
}

fn msg(turn_index: i32, role: &str, content: &str) -> ContextMessage {
    ContextMessage {
        role: role.into(),
        content: content.into(),
        turn_index,
        created_at: Some(Utc::now() + Duration::seconds(turn_index as i64)),
        metadata: serde_json::Value::Null,
    }
}

#[test]
fn deterministic_segmenter_splits_on_message_boundaries_and_token_limits() {
    let messages = vec![
        msg(0, "user", "alpha alpha alpha alpha"),
        msg(1, "assistant", "beta beta beta beta"),
        msg(2, "user", "gamma gamma gamma gamma"),
    ];
    let config = SegmentationConfig {
        target_tokens: 4,
        max_tokens: 8,
        ..SegmentationConfig::default()
    };

    let segments = segment_messages(Uuid::new_v4(), "discord:thread-1", &messages, &config)
        .expect("segmentation should succeed");

    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].segment_index, 0);
    assert_eq!(segments[0].start_turn, 0);
    assert_eq!(segments[0].end_turn, 0);
    assert_eq!(segments[1].start_turn, 1);
    assert_eq!(segments[2].start_turn, 2);
    assert!(segments.iter().all(|s| s.token_count <= 8));
    assert!(
        segments
            .iter()
            .all(|s| s.content_hash.starts_with("sha256:"))
    );
}

#[tokio::test]
async fn ingest_context_segments_is_idempotent_by_content_hash() {
    let storage = MockStorage::new();
    let ctx = ctx();
    let session_id = Uuid::new_v4();
    let params = IngestContextSegmentsParams {
        session_id,
        conversation_id: "discord:chan:thread".into(),
        messages: vec![
            msg(0, "user", "we debugged discord threading"),
            msg(1, "assistant", "the role mention id needed configuration"),
        ],
        segmentation: SegmentationConfig::default(),
        embed_missing: false,
    };

    let first = ingest_context_segments(&storage, &ctx, params.clone(), None)
        .await
        .expect("first ingest should succeed");
    let second = ingest_context_segments(&storage, &ctx, params, None)
        .await
        .expect("second ingest should be idempotent");

    assert_eq!(first.segments_created, 1);
    assert_eq!(second.segments_created, 0);
    assert_eq!(second.segments_skipped, 1);
    assert_eq!(storage.context_segments.lock().await.len(), 1);
}

#[tokio::test]
async fn ingest_context_segments_creates_bidirectional_temporal_edges() {
    let storage = MockStorage::new();
    let ctx = ctx();
    let session_id = Uuid::new_v4();
    let params = IngestContextSegmentsParams {
        session_id,
        conversation_id: "hermes:session".into(),
        messages: vec![
            msg(0, "user", "alpha alpha alpha alpha"),
            msg(1, "assistant", "beta beta beta beta"),
            msg(2, "user", "gamma gamma gamma gamma"),
        ],
        segmentation: SegmentationConfig {
            target_tokens: 4,
            max_tokens: 8,
            ..SegmentationConfig::default()
        },
        embed_missing: false,
    };

    let result = ingest_context_segments(&storage, &ctx, params, None)
        .await
        .expect("ingest should create segments and temporal edges");

    assert_eq!(result.segments_created, 3);
    assert_eq!(result.edges_created, 4);
    let edges = storage.temporal_edges.lock().await;
    assert_eq!(
        edges
            .iter()
            .filter(|e| e.edge_type == "next_context_segment")
            .count(),
        2
    );
    assert_eq!(
        edges
            .iter()
            .filter(|e| e.edge_type == "previous_context_segment")
            .count(),
        2
    );
}

#[tokio::test]
async fn search_context_segments_rrf_merges_bm25_and_ann_candidates() {
    let storage = MockStorage::new();
    let ctx = ctx();
    let session_id = Uuid::new_v4();
    let params = IngestContextSegmentsParams {
        session_id,
        conversation_id: "hermes:session".into(),
        messages: vec![
            msg(0, "user", "gateway threading role mention discord"),
            msg(1, "assistant", "temporal chunks use nomic embeddings"),
        ],
        segmentation: SegmentationConfig {
            target_tokens: 4,
            max_tokens: 8,
            ..SegmentationConfig::default()
        },
        embed_missing: true,
    };
    let embeddings = vec![vec![1.0; 768], vec![0.0; 768]];
    ingest_context_segments(&storage, &ctx, params, Some(embeddings))
        .await
        .expect("ingest should store test embeddings");

    let results = search_context_segments(
        &storage,
        &ctx,
        ContextSegmentSearchParams {
            session_id,
            query: "discord role gateway".into(),
            query_embedding: Some(vec![1.0; 768]),
            limit: 5,
            expand_prev: 0,
            expand_next: 0,
            max_expanded_tokens: 4000,
        },
    )
    .await
    .expect("search should succeed");

    assert!(!results.results.is_empty());
    assert_eq!(results.results[0].segment.start_turn, 0);
    assert!(results.results[0].sources.iter().any(|s| s == "bm25"));
    assert!(results.results[0].sources.iter().any(|s| s == "ann"));
}

#[tokio::test]
async fn get_context_window_returns_ordered_prev_hit_next_pages() {
    let storage = MockStorage::new();
    let ctx = ctx();
    let session_id = Uuid::new_v4();
    let params = IngestContextSegmentsParams {
        session_id,
        conversation_id: "hermes:session".into(),
        messages: vec![
            msg(0, "user", "before context"),
            msg(1, "assistant", "hit context"),
            msg(2, "user", "after context"),
        ],
        segmentation: SegmentationConfig {
            target_tokens: 2,
            max_tokens: 6,
            ..SegmentationConfig::default()
        },
        embed_missing: false,
    };
    let result = ingest_context_segments(&storage, &ctx, params, None)
        .await
        .expect("ingest should succeed");
    let hit = result.segments[1].segment_id;

    let window = get_context_window(
        &storage,
        &ctx,
        ContextWindowParams {
            session_id,
            segment_id: hit,
            prev: 1,
            next: 1,
            max_tokens: 100,
        },
    )
    .await
    .expect("window should traverse temporal prev/next edges");

    assert_eq!(window.segments.len(), 3);
    assert_eq!(window.segments[0].direction, "previous");
    assert_eq!(window.segments[1].direction, "hit");
    assert_eq!(window.segments[2].direction, "next");
    assert!(window.segments[0].segment.segment_text.contains("before"));
    assert!(window.segments[2].segment.segment_text.contains("after"));
}

#[tokio::test]
async fn search_context_segments_promotes_unsummarized_hit_into_fold() {
    let storage = MockStorage::new();
    let ctx = ctx();
    let session_id = Uuid::new_v4();
    let params = IngestContextSegmentsParams {
        session_id,
        conversation_id: "discord:long-running-orchestration".into(),
        messages: vec![
            msg(
                0,
                "user",
                "ferrosa memory should own folding for every harness",
            ),
            msg(
                1,
                "assistant",
                "retrieval summaries promote useful context into longer term memory",
            ),
        ],
        segmentation: SegmentationConfig::default(),
        embed_missing: false,
    };
    ingest_context_segments(&storage, &ctx, params, None)
        .await
        .expect("ingest should succeed");

    let results = search_context_segments(
        &storage,
        &ctx,
        ContextSegmentSearchParams {
            session_id,
            query: "folding retrieval summaries".into(),
            query_embedding: None,
            limit: 5,
            expand_prev: 0,
            expand_next: 0,
            max_expanded_tokens: 4000,
        },
    )
    .await
    .expect("search should promote useful hits");

    let hit = results.results.first().expect("expected a search hit");
    let summary = hit
        .segment
        .segment_summary
        .as_ref()
        .expect("retrieved hit should include a generated summary");
    assert!(
        summary.contains("folding"),
        "summary should preserve salient query terms: {summary}"
    );

    let persisted = storage
        .context_segment_get(&ctx, session_id, hit.segment.segment_id)
        .await
        .expect("storage read should succeed")
        .expect("promoted segment should still exist");
    assert_eq!(persisted.segment_summary.as_deref(), Some(summary.as_str()));

    let folds = storage.folds.lock().await;
    assert_eq!(
        folds.len(),
        1,
        "retrieval-time promotion should create an fmem fold"
    );
    assert_eq!(folds[0].parent_fold_id, None);
    assert_eq!(folds[0].session_id, session_id);
    assert_eq!(folds[0].fold_summary.as_deref(), Some(summary.as_str()));
    assert_eq!(
        folds[0].status,
        ferrosa_memory_core::types::FoldStatus::Folded
    );
}
