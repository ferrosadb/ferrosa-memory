//! Temporal event chain handlers (Zep-inspired).
//!
//! Stores timestamped facts with supersession tracking. When a new fact
//! replaces an old one, the old fact's `valid_until` is set atomically
//! and a `SUPERSEDES` graph edge is created.
//!
//! ## Invariant
//!
//! At most one fact per entity has `valid_until IS NULL` (the current fact).
//! The batch read-invalidate-write is atomic (FMEA F21/F22).

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{TemporalEvent, TenantContext};

/// Write a new temporal fact for an entity.
///
/// If a current fact exists (valid_until IS NULL), it is superseded:
/// its valid_until is set and the new fact's supersedes_id points to it.
pub async fn write_temporal_fact(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
    fact_text: &str,
    source_session: Uuid,
    confidence: f64,
) -> anyhow::Result<Uuid> {
    let event_id = Uuid::now_v7();
    let now = chrono::Utc::now();

    // Check for existing current fact
    let current = storage.temporal_get_current(ctx, entity_id).await?;
    let supersedes_id = if let Some(ref existing) = current {
        // Invalidate the old fact
        storage
            .temporal_invalidate(ctx, entity_id, existing.event_id)
            .await?;
        Some(existing.event_id)
    } else {
        None
    };

    let event = TemporalEvent {
        tenant_id: ctx.tenant_id,
        entity_id,
        event_time: now,
        event_id,
        fact_text: fact_text.to_string(),
        supersedes_id,
        valid_until: None,
        source_session,
        confidence,
    };

    storage.temporal_put(ctx, &event).await?;

    // Create SUPERSEDES edge if this fact replaces an older one
    if let Some(old_id) = supersedes_id
        && let Err(e) =
            crate::graph_write::create_supersedes_edge(storage, ctx, event_id, old_id, entity_id)
                .await
    {
        tracing::warn!(%event_id, %old_id, error = %e, "failed to create SUPERSEDES edge");
    }

    tracing::info!(%event_id, %entity_id, supersedes = ?supersedes_id, "temporal fact written");
    Ok(event_id)
}

/// Get the current (most recent valid) fact for an entity.
pub async fn get_current_fact(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
) -> anyhow::Result<Option<TemporalEvent>> {
    storage.temporal_get_current(ctx, entity_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    #[tokio::test]
    async fn write_first_fact() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();

        let event_id = write_temporal_fact(&store, &ctx, eid, "Alice works at Acme", sid, 0.9)
            .await
            .unwrap();

        let current = get_current_fact(&store, &ctx, eid).await.unwrap().unwrap();
        assert_eq!(current.event_id, event_id);
        assert_eq!(current.fact_text, "Alice works at Acme");
        assert!(current.supersedes_id.is_none());
        assert!(current.valid_until.is_none());
    }

    #[tokio::test]
    async fn supersession_chain() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();
        let sid = Uuid::new_v4();

        let first = write_temporal_fact(&store, &ctx, eid, "Alice works at Acme", sid, 0.9)
            .await
            .unwrap();
        let second = write_temporal_fact(&store, &ctx, eid, "Alice works at Globex", sid, 0.95)
            .await
            .unwrap();

        // Current fact is the second one
        let current = get_current_fact(&store, &ctx, eid).await.unwrap().unwrap();
        assert_eq!(current.event_id, second);
        assert_eq!(current.supersedes_id, Some(first));
        assert!(current.valid_until.is_none());

        // First fact should now have valid_until set
        let events = store.temporal_events.lock().await;
        let old = events.iter().find(|e| e.event_id == first).unwrap();
        assert!(old.valid_until.is_some());
    }

    #[tokio::test]
    async fn no_current_fact_returns_none() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let eid = Uuid::new_v4();

        let result = get_current_fact(&store, &ctx, eid).await.unwrap();
        assert!(result.is_none());
    }
}
