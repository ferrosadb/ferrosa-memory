//! Centralized graph-write seam for ferrosa-memory.
//!
//! Feature modules should route graph mutations here instead of calling raw
//! storage edge methods directly. This keeps the eventual Ferrosa graph-write
//! cutover localized to one module.

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{TenantContext, TypedEdge};

#[allow(clippy::too_many_arguments)]
pub async fn create_typed_edge<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    src_id: Uuid,
    edge_type: impl Into<String>,
    dst_id: Uuid,
    weight: f64,
    metadata: Option<String>,
) -> anyhow::Result<TypedEdge> {
    let edge = TypedEdge {
        tenant_id: ctx.tenant_id,
        session_id,
        src_id,
        edge_type: edge_type.into(),
        dst_id,
        weight,
        metadata,
        created_at: chrono::Utc::now(),
    };
    storage.typed_edge_put(ctx, &edge).await?;
    Ok(edge)
}

pub async fn create_folded_into_edge<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    source_fold_id: Uuid,
    target_fold_id: Uuid,
    session_id: Uuid,
) -> anyhow::Result<()> {
    storage
        .edge_folded_into(ctx, source_fold_id, target_fold_id, session_id)
        .await
}

pub async fn create_mentioned_in_edge<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    entity_id: Uuid,
    fold_id: Uuid,
    session_id: Uuid,
) -> anyhow::Result<()> {
    storage
        .edge_mentioned_in(ctx, entity_id, fold_id, session_id)
        .await
}

pub async fn reinforce_co_occurs_edge<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    entity_a: Uuid,
    entity_b: Uuid,
    session_id: Uuid,
    strength: f32,
) -> anyhow::Result<()> {
    storage
        .edge_co_occurs(ctx, entity_a, entity_b, session_id, strength)
        .await
}

pub async fn create_supersedes_edge<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    new_event_id: Uuid,
    old_event_id: Uuid,
    entity_id: Uuid,
) -> anyhow::Result<()> {
    storage
        .edge_supersedes(ctx, new_event_id, old_event_id, entity_id)
        .await
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
    async fn create_typed_edge_writes_through_storage() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let edge = create_typed_edge(
            &storage,
            &ctx,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "REQUIRES",
            Uuid::new_v4(),
            1.0,
            Some("metadata".into()),
        )
        .await
        .unwrap();

        assert_eq!(edge.edge_type, "REQUIRES");
        assert_eq!(storage.typed_edges.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn reinforce_co_occurs_edge_uses_storage_edge_path() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let entity_a = Uuid::new_v4();
        let entity_b = Uuid::new_v4();

        reinforce_co_occurs_edge(&storage, &ctx, entity_a, entity_b, Uuid::new_v4(), 0.75)
            .await
            .unwrap();

        let edges = storage.edges.lock().await;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source, entity_a);
        assert_eq!(edges[0].target, entity_b);
        assert_eq!(edges[0].edge_type, "CO_OCCURS");
    }
}
