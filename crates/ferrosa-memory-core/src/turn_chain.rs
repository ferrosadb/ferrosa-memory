//! Automatic temporal edge chaining for `turn` entities.
//!
//! When a new turn entity is ingested for the first time, this module finds
//! the most recent prior turn entity in the same session partition and writes
//! bidirectional temporal edges so that sessions form traversable linked
//! threads rather than disjoint nodes.
//!
//! ## Edge types
//!
//! | name              | direction     | meaning                         |
//! |-------------------|---------------|---------------------------------|
//! | `next_turn`       | prev → new    | follow-forward in time          |
//! | `previous_turn`   | new  → prev   | walk backward from new turn     |
//!
//! These mirror the `next_context_segment` / `previous_context_segment`
//! pattern in [`crate::context_segment`].
//!
//! ## Ordering semantics
//!
//! Turns are ordered by **wall-clock insertion order** (`created_at` on the
//! `EntityEntry`).  The "most recent prior turn" is the turn with the largest
//! `created_at` strictly less than the newly-inserted turn's `created_at`,
//! across all turns in the same `(tenant_id, session_id)` partition.
//!
//! **Rationale**: `captured_at_ms` lives in the JSON `properties` blob and
//! would require a full table scan plus client-side sort.  `created_at` is a
//! first-class column that the storage layer already sorts on in
//! `entity_list_matching`.  Because turns arrive in real time (the hook fires
//! immediately after a turn completes), insertion order reliably reflects
//! conversation order.  This is documented here rather than buried in code so
//! that a future migration to `captured_at_ms` ordering has a clear decision
//! record.
//!
//! ## Out-of-order arrival
//!
//! Out-of-order turns (a later captured_at arriving before an earlier one)
//! will link by insertion order, not captured time.  The resulting chain
//! reflects the order in which the server saw the turns, not the strict
//! wall-clock conversation order.  This is acceptable because:
//!
//! - Out-of-order arrival is rare (the hook fires synchronously per turn).
//! - The `captured_at_ms` property is preserved on each entity for consumers
//!   that need strict conversation ordering.
//! - Corrupting the chain by re-linking would be worse than an occasional
//!   inversion.
//!
//! ## Idempotency
//!
//! This function is called **only for newly-inserted** entities
//! (`entity_inserted` path in `handle_ingest_entities`).  Callers that use
//! `on_conflict: skip` (the turn hook default) will skip the entity
//! entirely on re-ingest, so this function is never called twice for the same
//! turn entity.
//!
//! `temporal_edge_put` is an upsert by primary key
//! `(tenant_id, session_id, src_id, edge_type, dst_id)`, so even if this
//! function were called twice it would write identical rows rather than
//! creating duplicates.
//!
//! ## Session anchor (skipped)
//!
//! An optional "session" sentinel entity + `contains_turn` edge was
//! considered but skipped to keep scope narrow.  The chain formed by
//! `next_turn` / `previous_turn` edges already gives traversal from any
//! turn.  A session anchor would add value for visualizers that want a single
//! root node per thread — that can be added later without touching this
//! module.

use chrono::Utc;
use uuid::Uuid;

use crate::context_segment::TemporalEdge;
use crate::storage::Storage;
use crate::types::{EntityEntry, EntityListQuery, EntityListScope, TenantContext};

/// Write `next_turn` / `previous_turn` temporal edges linking `new_turn` to
/// the most recently inserted prior turn in the same session, if one exists.
///
/// Returns `Ok(true)` if edges were written, `Ok(false)` if no prior turn
/// was found (first turn in session).
///
/// Errors are non-fatal from the caller's perspective: the caller logs them
/// but does not fail the ingest.
pub async fn link_turn_to_predecessor<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    new_turn: &EntityEntry,
) -> anyhow::Result<bool> {
    debug_assert_eq!(
        new_turn.entity_type, "turn",
        "link_turn_to_predecessor called with non-turn entity"
    );

    // List all turn entities in this session partition, then find the most
    // recent one that was created strictly before the new turn.
    //
    // `entity_list_matching` returns results sorted descending by
    // updated_at/created_at (see storage.rs ~line 362), so the first hit
    // with created_at < new_turn.created_at is exactly what we need.
    let query = EntityListQuery {
        session_id,
        entity_type: Some("turn".into()),
        filters: Default::default(),
        scope: EntityListScope::Session,
        // We only need the immediate predecessor; fetch a small page.
        // We use 50 (the max allowed by entity_list_matching callers) to
        // handle clock skew where updated_at == created_at ordering may
        // put the new entity anywhere in the list on a re-check, though in
        // practice we expect it to be first.
        limit: 50,
    };

    let candidates = storage.entity_list_matching(ctx, query).await?;

    // The list is descending by updated_at/created_at.  We skip the new
    // turn itself (same entity_id) and find the first candidate whose
    // `created_at` is strictly earlier.
    let predecessor = candidates
        .iter()
        .find(|e| e.entity_id != new_turn.entity_id && e.created_at < new_turn.created_at);

    let Some(prev) = predecessor else {
        return Ok(false);
    };

    let now = Utc::now();

    // next_turn: prev → new  (ordinal = 0, single edge per src+type pair)
    let next_edge = TemporalEdge {
        tenant_id: ctx.tenant_id,
        session_id,
        src_id: prev.entity_id,
        edge_type: "next_turn".into(),
        dst_id: new_turn.entity_id,
        relation_time: new_turn.created_at,
        ordinal: 0,
        metadata: format!("session_id={}", session_id),
        created_at: now,
    };

    // previous_turn: new → prev
    let prev_edge = TemporalEdge {
        tenant_id: ctx.tenant_id,
        session_id,
        src_id: new_turn.entity_id,
        edge_type: "previous_turn".into(),
        dst_id: prev.entity_id,
        relation_time: prev.created_at,
        ordinal: 0,
        metadata: format!("session_id={}", session_id),
        created_at: now,
    };

    storage.temporal_edge_put(ctx, &next_edge).await?;
    storage.temporal_edge_put(ctx, &prev_edge).await?;

    Ok(true)
}

/// Walk the `next_turn` chain forward from `start_turn_id`, returning up to
/// `limit` turns in chronological order (start_turn first).
///
/// This is the traversal primitive exposed through the MCP tool
/// `get_turn_chain`.  It mirrors the segment window walker in
/// [`crate::context_segment::get_context_window`].
pub async fn walk_turn_chain_forward<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    start_turn_id: Uuid,
    limit: usize,
) -> anyhow::Result<Vec<EntityEntry>> {
    let limit = limit.clamp(1, 50);
    let mut result = Vec::with_capacity(limit);

    // Load the starting turn.
    let start = storage
        .entity_get_by_id(ctx, session_id, start_turn_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("turn entity not found: {}", start_turn_id))?;
    result.push(start);

    // Walk forward through next_turn edges.
    for _ in 1..limit {
        let current_id = result.last().expect("result is non-empty").entity_id;
        let edges = storage
            .temporal_edge_list_from(ctx, session_id, current_id, "next_turn")
            .await?;
        let Some(edge) = edges.into_iter().next() else {
            break;
        };
        let Some(next) = storage
            .entity_get_by_id(ctx, session_id, edge.dst_id)
            .await?
        else {
            break;
        };
        result.push(next);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use crate::types::{EntityEntry, EntityScope, MemoryState};

    fn ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "turn-chain-test".into(),
        }
    }

    /// Build a minimal turn EntityEntry with a given created_at offset from
    /// the Unix epoch (seconds).  Using distinct offsets keeps ordering
    /// deterministic without sleeping.
    fn turn_entity(ctx: &TenantContext, session_id: Uuid, created_offset_secs: i64) -> EntityEntry {
        let id = Uuid::new_v4();
        let ts = chrono::DateTime::from_timestamp(created_offset_secs, 0).unwrap_or_else(Utc::now);
        EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: id,
            session_id,
            entity_name: format!("turn-{id}"),
            entity_type: "turn".into(),
            source_fold_id: None,
            context_snippet: "test turn".into(),
            entity_embedding: None,
            confidence: 0.7,
            state: MemoryState::Active,
            created_at: ts,
            description: None,
            description_embedding: None,
            tags: vec![],
            properties: serde_json::json!({}),
            content_hash: None,
            updated_at: Some(ts),
            scope: EntityScope::Session,
            ingested_by_session: Some(session_id),
        }
    }

    async fn store_turn(storage: &MockStorage, ctx: &TenantContext, entry: &EntityEntry) {
        storage
            .entity_put(ctx, entry)
            .await
            .expect("entity_put should succeed");
    }

    // -----------------------------------------------------------------------
    // link_turn_to_predecessor: basic happy path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn first_turn_in_session_produces_no_edges() {
        let storage = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        let t1 = turn_entity(&ctx, sid, 1000);
        store_turn(&storage, &ctx, &t1).await;

        let linked = link_turn_to_predecessor(&storage, &ctx, sid, &t1)
            .await
            .expect("link should succeed");
        assert!(!linked, "first turn should produce no edges");

        let edges = storage.temporal_edges.lock().await;
        assert!(edges.is_empty(), "no edges expected for first turn");
    }

    #[tokio::test]
    async fn second_turn_creates_bidirectional_edges() {
        let storage = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        let t1 = turn_entity(&ctx, sid, 1000);
        let t2 = turn_entity(&ctx, sid, 2000);
        store_turn(&storage, &ctx, &t1).await;
        store_turn(&storage, &ctx, &t2).await;

        let linked = link_turn_to_predecessor(&storage, &ctx, sid, &t2)
            .await
            .expect("link should succeed");
        assert!(linked, "should have found predecessor");

        let edges = storage.temporal_edges.lock().await;
        assert_eq!(edges.len(), 2, "expected next_turn + previous_turn");

        let next = edges
            .iter()
            .find(|e| e.edge_type == "next_turn")
            .expect("next_turn edge must exist");
        assert_eq!(next.src_id, t1.entity_id);
        assert_eq!(next.dst_id, t2.entity_id);

        let prev = edges
            .iter()
            .find(|e| e.edge_type == "previous_turn")
            .expect("previous_turn edge must exist");
        assert_eq!(prev.src_id, t2.entity_id);
        assert_eq!(prev.dst_id, t1.entity_id);
    }

    // -----------------------------------------------------------------------
    // N turns → 2*(N-1) edges with correct forward/backward types
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn n_turns_produce_2n_minus_2_edges_in_correct_order() {
        let storage = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        let n = 5;
        let mut turns = Vec::new();
        for i in 0..n {
            let t = turn_entity(&ctx, sid, 1000 + i as i64 * 1000);
            store_turn(&storage, &ctx, &t).await;
            turns.push(t);
        }

        // Link each turn to its predecessor (simulate sequential ingest).
        for t in &turns {
            link_turn_to_predecessor(&storage, &ctx, sid, t)
                .await
                .expect("link should succeed");
        }

        let edges = storage.temporal_edges.lock().await;
        let next_count = edges.iter().filter(|e| e.edge_type == "next_turn").count();
        let prev_count = edges
            .iter()
            .filter(|e| e.edge_type == "previous_turn")
            .count();

        assert_eq!(
            next_count,
            n - 1,
            "expected {n}-1 next_turn edges, got {next_count}"
        );
        assert_eq!(
            prev_count,
            n - 1,
            "expected {n}-1 previous_turn edges, got {prev_count}"
        );
        assert_eq!(edges.len(), 2 * (n - 1), "total edges should be 2*(N-1)");

        // Verify the chain is fully connected: t[i] --next_turn--> t[i+1]
        for i in 0..(n - 1) {
            let has_forward = edges.iter().any(|e| {
                e.edge_type == "next_turn"
                    && e.src_id == turns[i].entity_id
                    && e.dst_id == turns[i + 1].entity_id
            });
            assert!(
                has_forward,
                "missing next_turn edge from turn[{i}] to turn[{}]",
                i + 1
            );
        }
    }

    // -----------------------------------------------------------------------
    // Cross-session isolation: turns in different sessions must not link
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cross_session_turns_do_not_link() {
        let storage = MockStorage::new();
        let ctx = ctx();
        let sid_a = Uuid::new_v4();
        let sid_b = Uuid::new_v4();

        // Insert a turn into session A.
        let ta = turn_entity(&ctx, sid_a, 1000);
        store_turn(&storage, &ctx, &ta).await;

        // Insert a turn into session B and try to link it.
        let tb = turn_entity(&ctx, sid_b, 2000);
        store_turn(&storage, &ctx, &tb).await;

        let linked = link_turn_to_predecessor(&storage, &ctx, sid_b, &tb)
            .await
            .expect("link should succeed");
        assert!(
            !linked,
            "turn in session B must not link to turn in session A"
        );

        let edges = storage.temporal_edges.lock().await;
        assert!(edges.is_empty(), "no cross-session edges expected");
    }

    // -----------------------------------------------------------------------
    // Re-ingest idempotency: calling link twice with the same new turn must
    // not produce duplicate edges (temporal_edge_put is an upsert).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn re_linking_same_turn_does_not_duplicate_edges() {
        let storage = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        let t1 = turn_entity(&ctx, sid, 1000);
        let t2 = turn_entity(&ctx, sid, 2000);
        store_turn(&storage, &ctx, &t1).await;
        store_turn(&storage, &ctx, &t2).await;

        // Link twice.
        link_turn_to_predecessor(&storage, &ctx, sid, &t2)
            .await
            .expect("first link should succeed");
        link_turn_to_predecessor(&storage, &ctx, sid, &t2)
            .await
            .expect("second link should succeed");

        // MockStorage::temporal_edge_put is an upsert — duplicates are
        // replaced, so the count must remain 2.
        let edges = storage.temporal_edges.lock().await;
        assert_eq!(edges.len(), 2, "re-linking must not create duplicate edges");
    }

    #[tokio::test]
    async fn equal_timestamp_turns_do_not_link() {
        let storage = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        let t1 = turn_entity(&ctx, sid, 1000);
        let t2 = turn_entity(&ctx, sid, 1000);
        store_turn(&storage, &ctx, &t1).await;
        store_turn(&storage, &ctx, &t2).await;

        let linked = link_turn_to_predecessor(&storage, &ctx, sid, &t2)
            .await
            .expect("link should succeed");
        assert!(
            !linked,
            "equal timestamps are not a strict predecessor relationship"
        );

        let edges = storage.temporal_edges.lock().await;
        assert!(edges.is_empty(), "equal timestamp turns must not link");
    }

    // -----------------------------------------------------------------------
    // Traversal: walk_turn_chain_forward returns turns in arrival order
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn walk_forward_returns_turns_in_order() {
        let storage = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        let n = 4;
        let mut turns = Vec::new();
        for i in 0..n {
            let t = turn_entity(&ctx, sid, 1000 + i as i64 * 1000);
            store_turn(&storage, &ctx, &t).await;
            turns.push(t);
        }
        for t in &turns {
            link_turn_to_predecessor(&storage, &ctx, sid, t)
                .await
                .expect("link should succeed");
        }

        let chain = walk_turn_chain_forward(&storage, &ctx, sid, turns[0].entity_id, 10)
            .await
            .expect("walk should succeed");

        assert_eq!(chain.len(), n, "should return all {n} turns");
        for i in 0..n {
            assert_eq!(
                chain[i].entity_id, turns[i].entity_id,
                "turn[{i}] should be at position {i} in the chain"
            );
        }
    }

    #[tokio::test]
    async fn walk_forward_respects_limit() {
        let storage = MockStorage::new();
        let ctx = ctx();
        let sid = Uuid::new_v4();

        let mut turns = Vec::new();
        for i in 0..5 {
            let t = turn_entity(&ctx, sid, 1000 + i as i64 * 1000);
            store_turn(&storage, &ctx, &t).await;
            turns.push(t);
        }
        for t in &turns {
            link_turn_to_predecessor(&storage, &ctx, sid, t)
                .await
                .expect("link should succeed");
        }

        let chain = walk_turn_chain_forward(&storage, &ctx, sid, turns[0].entity_id, 3)
            .await
            .expect("walk should succeed");

        assert_eq!(chain.len(), 3, "walk should stop at limit=3");
        assert_eq!(chain[0].entity_id, turns[0].entity_id);
        assert_eq!(chain[1].entity_id, turns[1].entity_id);
        assert_eq!(chain[2].entity_id, turns[2].entity_id);
    }
}
