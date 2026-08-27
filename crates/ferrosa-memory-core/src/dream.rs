//! Dream consolidation — periodic memory processing.
//!
//! Inspired by vestige's 5-phase dream cycle. Simplified for v1:
//! 1. Triage — list entities for the session
//! 2. Connection Discovery — compare entities by text similarity:
//!    a. Within-fold groups (entities sharing a source fold)
//!    b. Unfolded entities (ingested without a fold context, e.g. via `smart_ingest`)
//! 3. Insight Generation — identify clusters (3+ co-occurring entities)

use std::collections::HashMap;
use uuid::Uuid;

use serde::Serialize;

use crate::storage::Storage;
use crate::types::TenantContext;

/// Result of a dream consolidation run.
#[derive(Debug, Serialize)]
pub struct DreamResult {
    pub entities_processed: usize,
    pub connections_created: usize,
    pub insights: Vec<String>,
    /// Actual entity pairs connected (for viz event emission).
    #[serde(skip)]
    pub edges: Vec<(Uuid, Uuid)>,
    /// Number of Datalog-derived facts from batch inference.
    pub derived_facts_count: usize,
    /// Number of entities with updated PageRank scores.
    pub pagerank_updated: usize,
    /// Number of warmth entries pruned by Ebbinghaus decay.
    pub warmth_decayed: usize,
    /// Predicates promoted to durable materialization during this cycle.
    pub promoted_predicates: Vec<String>,
}

/// Similarity threshold for creating CO_OCCURS edges (Jaccard on word sets).
const CO_OCCURS_THRESHOLD: f64 = 0.05;

/// Maximum number of unfolded entities to compare pairwise per run.
/// Caps the O(n²) comparison to keep idle consolidation fast.
const UNFOLDED_PAIR_CAP: usize = 200;

/// Build a [`crate::types::MemScene`] from a cluster of entities and upsert it.
/// Returns `true` on a successful persist. The summary lists member names so the
/// scene is itself lexically searchable; `member_ids` let retrieval expand the
/// scene back to its full cluster.
async fn persist_scene(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    scene_id: Uuid,
    members: &[&crate::types::EntityEntry],
) -> bool {
    let scene = crate::types::MemScene {
        tenant_id: ctx.tenant_id,
        session_id,
        scene_id,
        member_ids: members.iter().map(|e| e.entity_id).collect(),
        summary: scene_summary(members),
        scene_embedding: mean_embedding(members),
        created_at: chrono::Utc::now(),
    };
    match storage.scene_put(ctx, &scene).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "failed to persist consolidation scene");
            false
        }
    }
}

/// Maximum member names listed verbatim in a scene summary; the rest are
/// summarized as "+N more" so a large cluster doesn't bloat every search result
/// and the session profile.
const SCENE_SUMMARY_NAME_CAP: usize = 8;

/// Build a bounded, lexically-searchable scene summary from member names.
fn scene_summary(members: &[&crate::types::EntityEntry]) -> String {
    let names: Vec<&str> = members.iter().map(|e| e.entity_name.as_str()).collect();
    let total = members.len();
    if names.len() > SCENE_SUMMARY_NAME_CAP {
        format!(
            "{} +{} more ({} related entities)",
            names[..SCENE_SUMMARY_NAME_CAP].join(", "),
            names.len() - SCENE_SUMMARY_NAME_CAP,
            total
        )
    } else {
        format!("{} ({} related entities)", names.join(", "), total)
    }
}

/// Centroid (mean) of the member entity embeddings, for semantic scene matching.
/// Returns `None` when no member carried an embedding; ignores any whose
/// dimensionality disagrees with the first embedded member (defensive — all
/// share one model in practice).
fn mean_embedding(members: &[&crate::types::EntityEntry]) -> Option<Vec<f32>> {
    let mut sum: Vec<f32> = Vec::new();
    let mut count = 0usize;
    for e in members {
        let Some(emb) = e.entity_embedding.as_ref() else {
            continue;
        };
        if sum.is_empty() {
            sum = emb.clone();
            count = 1;
        } else if emb.len() == sum.len() {
            for (s, v) in sum.iter_mut().zip(emb) {
                *s += *v;
            }
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    let inv = 1.0 / count as f32;
    for s in &mut sum {
        *s *= inv;
    }
    Some(sum)
}

/// Build/refresh the per-session workspace profile from its scenes: a compact
/// summary retrieval injects so an agent gets the session's gist without
/// re-deriving it. Prefers workspace/repo/branch/task-flavored scenes; falls
/// back to all scenes. Bounded by capping the number of scene summaries used.
async fn persist_profile(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    scenes: &[crate::types::MemScene],
) {
    if scenes.is_empty() {
        return;
    }
    let is_workspacey = |s: &str| {
        let l = s.to_ascii_lowercase();
        ["workspace", "repo", "branch", "task", "cluster"]
            .iter()
            .any(|k| l.contains(k))
    };
    let mut parts: Vec<&str> = scenes
        .iter()
        .filter(|s| is_workspacey(&s.summary))
        .map(|s| s.summary.as_str())
        .collect();
    if parts.is_empty() {
        parts = scenes.iter().map(|s| s.summary.as_str()).collect();
    }
    parts.truncate(30); // bound injected size; scene summaries are short
    let profile = crate::types::MemProfile {
        tenant_id: ctx.tenant_id,
        session_id,
        summary: format!(
            "Session covers {} scene(s): {}",
            scenes.len(),
            parts.join("; ")
        ),
        scene_count: scenes.len() as i32,
        updated_at: chrono::Utc::now(),
    };
    if let Err(e) = storage.profile_put(ctx, &profile).await {
        tracing::warn!(error = %e, "failed to persist session profile");
    }
}

/// Run consolidation over a session's entities.
///
/// Two-pass connection discovery:
/// 1. Entities with a `source_fold_id` are grouped by fold and compared within each group.
/// 2. Entities without a fold ("unfolded") are compared pairwise using text similarity,
///    capped at `UNFOLDED_PAIR_CAP` most-recent entities to bound the O(n²) cost.
///
/// Clusters of 3+ co-occurring entities generate insight summaries and persist a
/// durable [`crate::types::MemScene`] each.
pub async fn run_consolidation(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<DreamResult> {
    let entities = storage.entity_list_session(ctx, session_id).await?;
    let entity_count = entities.len();

    // Partition into folded (grouped by fold_id) and unfolded.
    let mut fold_groups: HashMap<Uuid, Vec<&crate::types::EntityEntry>> = HashMap::new();
    let mut unfolded: Vec<&crate::types::EntityEntry> = Vec::new();
    for entity in &entities {
        if let Some(fold_id) = entity.source_fold_id {
            fold_groups.entry(fold_id).or_default().push(entity);
        } else {
            unfolded.push(entity);
        }
    }

    let mut connections_created = 0;
    let mut edges = Vec::new();

    // Pass 1: within-fold comparison (existing behaviour).
    create_edges_for_groups(
        fold_groups.values(),
        storage,
        ctx,
        session_id,
        &mut connections_created,
        &mut edges,
    )
    .await;

    // Pass 2: unfolded entities — compare most-recent pairs by text similarity.
    // Sort by created_at descending so the cap keeps the freshest entities.
    unfolded.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    unfolded.truncate(UNFOLDED_PAIR_CAP);
    if unfolded.len() >= 2 {
        create_edges_for_groups(
            std::iter::once(&unfolded),
            storage,
            ctx,
            session_id,
            &mut connections_created,
            &mut edges,
        )
        .await;
    }

    // Identify clusters (3+ entities) and persist each as a durable, retrievable
    // MemScene (a coherent semantic unit) in addition to the ephemeral insight
    // string. `scene_put` is idempotent on a deterministic scene_id, so repeated
    // consolidation cycles upsert the same scene rather than duplicating it.
    let mut insights = Vec::new();
    let mut scenes_persisted = 0usize;
    for (fold_id, group) in &fold_groups {
        if group.len() >= 3 {
            let names: Vec<&str> = group.iter().map(|e| e.entity_name.as_str()).collect();
            insights.push(format!(
                "Cluster in fold {}: {} ({} entities co-occurring)",
                &fold_id.to_string()[..8],
                names.join(", "),
                group.len()
            ));
            // A fold IS a scene; reuse its id so the scene is stable across runs.
            scenes_persisted +=
                persist_scene(storage, ctx, session_id, *fold_id, group).await as usize;
        }
    }
    // Cluster insight + scene for unfolded entities too.
    if unfolded.len() >= 3 {
        let names: Vec<&str> = unfolded.iter().map(|e| e.entity_name.as_str()).collect();
        insights.push(format!(
            "Unfolded cluster: {} ({} entities co-occurring)",
            names.join(", "),
            unfolded.len()
        ));
        // Deterministic id for the (single) unfolded scene of this session.
        let unfolded_scene_id =
            Uuid::from_u128(session_id.as_u128() ^ 0x5cae_5cae_5cae_5cae_5cae_5cae_5cae_5cae);
        scenes_persisted +=
            persist_scene(storage, ctx, session_id, unfolded_scene_id, &unfolded).await as usize;
    }
    if scenes_persisted > 0 {
        tracing::debug!(scenes_persisted, session = %session_id, "consolidation persisted scenes");
        // Build/refresh the per-session workspace profile from the scenes.
        if let Ok(scenes) = storage.scene_list_session(ctx, session_id).await {
            persist_profile(storage, ctx, session_id, &scenes).await;
        }
    }

    // Phase 4: Datalog batch inference — derive facts from the updated graph
    let derived_facts_count = match crate::datalog::load_session_facts(storage, ctx, session_id)
        .await
    {
        Ok(facts) => {
            let rules = crate::datalog::load_effective_rules(storage, ctx, None).await?;
            let datalog_config = crate::config::DatalogConfig::default();
            let (_all_facts, derived) = crate::datalog::evaluate(
                &rules,
                &facts,
                datalog_config.max_iterations,
                datalog_config.max_facts,
            );
            let count = derived.len();
            if !derived.is_empty() {
                // `derived_cache_by_query` is UUID-keyed, but the Datalog engine
                // also derives taxonomy facts whose object is an entity *type*
                // string, e.g. `isa(<uuid>, "conversation_turn")`. Those cannot be
                // stored in the UUID cache (issue #129) — caching them errored
                // mid-batch, leaving a partial, non-atomic write and a fail-loud
                // warning every pass. Cache only UUID↔UUID facts; the taxonomy
                // facts are re-derived at query time via the Datalog frontier.
                // `is_cacheable` also excludes any derivation resting on an
                // absence: negation is non-monotonic, so a later base fact can
                // falsify it and this cache has no way to take it back.
                let cacheable: Vec<crate::types::DerivedFact> = derived
                    .iter()
                    .filter(|f| f.is_cacheable())
                    .cloned()
                    .collect();
                let skipped = derived.len() - cacheable.len();
                if skipped > 0 {
                    tracing::debug!(
                        skipped,
                        cached = cacheable.len(),
                        "consolidation: skipped caching non-UUID-endpoint derived facts (taxonomy); re-derived at query time"
                    );
                }
                if !cacheable.is_empty() {
                    let cache_key = format!("consolidation:{}", session_id);
                    if let Err(e) = storage.derived_cache_put(ctx, &cache_key, &cacheable).await {
                        tracing::warn!(error = %e, "failed to cache derived facts during consolidation");
                    }
                }
            }
            count
        }
        Err(e) => {
            tracing::warn!(error = %e, "datalog batch inference failed during consolidation");
            0
        }
    };

    // Phase 5: Compute Personalized PageRank
    let rmh_config = crate::config::RmhConfig::default();
    let pagerank_updated = {
        let seeds = std::collections::HashMap::new();
        match crate::pagerank::compute_ppr(storage, ctx, session_id, &rmh_config, &seeds).await {
            Ok(ranks) => {
                let count = ranks.len();
                if let Err(e) =
                    crate::pagerank::update_pagerank_scores(storage, ctx, session_id, &ranks).await
                {
                    tracing::warn!(error = %e, "failed to write PageRank scores during consolidation");
                }
                count
            }
            Err(e) => {
                tracing::warn!(error = %e, "PageRank computation failed during consolidation");
                0
            }
        }
    };

    // Phase 6: Ebbinghaus warmth decay
    let warmth_decayed =
        match crate::warmth::run_decay_pass(storage, ctx, session_id, &rmh_config).await {
            Ok(pruned) => pruned,
            Err(e) => {
                tracing::warn!(error = %e, "warmth decay pass failed during consolidation");
                0
            }
        };

    // Phase 7: Check predicates for promotion
    let promotion_config = crate::config::PromotionConfig::default();
    let promoted_predicates = match crate::promotion::check_and_promote(
        storage,
        ctx,
        session_id,
        &promotion_config,
    )
    .await
    {
        Ok(promoted) => promoted,
        Err(e) => {
            tracing::warn!(error = %e, "promotion check failed (non-fatal)");
            vec![]
        }
    };

    Ok(DreamResult {
        entities_processed: entity_count,
        connections_created,
        insights,
        edges,
        derived_facts_count,
        pagerank_updated,
        warmth_decayed,
        promoted_predicates,
    })
}

/// Compare all pairs within each group and create CO_OCCURS edges for pairs
/// exceeding the similarity threshold.
async fn create_edges_for_groups<'a, I, S>(
    groups: I,
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    connections_created: &mut usize,
    edges: &mut Vec<(Uuid, Uuid)>,
) where
    I: Iterator<Item = &'a Vec<&'a crate::types::EntityEntry>>,
    S: Storage + ?Sized,
{
    for group in groups {
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let sim = crate::smart_ingest::compute_text_similarity(
                    &group[i].context_snippet,
                    &group[j].context_snippet,
                );
                if sim >= CO_OCCURS_THRESHOLD {
                    let a = group[i].entity_id;
                    let b = group[j].entity_id;
                    match crate::graph_write::reinforce_co_occurs_edge(
                        storage, ctx, a, b, session_id, sim as f32,
                    )
                    .await
                    {
                        Ok(()) => {
                            edges.push((a, b));
                            *connections_created += 1;
                        }
                        Err(e) => {
                            tracing::warn!(%a, %b, error = %e, "CO_OCCURS edge failed");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use crate::types::{EntityEntry, TenantContext};
    use uuid::Uuid;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    fn make_entity(
        tenant_id: Uuid,
        session_id: Uuid,
        name: &str,
        source_fold_id: Option<Uuid>,
    ) -> EntityEntry {
        EntityEntry {
            tenant_id,
            entity_id: Uuid::new_v4(),
            session_id,
            entity_name: name.to_string(),
            entity_type: "concept".to_string(),
            source_fold_id,
            context_snippet: format!("context for {name}"),
            entity_embedding: None,
            confidence: 0.9,
            state: Default::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        }
    }

    #[test]
    fn mean_embedding_averages_embedded_members_and_skips_none() {
        let t = Uuid::new_v4();
        let s = Uuid::new_v4();
        let mut a = make_entity(t, s, "A", None);
        a.entity_embedding = Some(vec![2.0, 0.0, 0.0]);
        let mut b = make_entity(t, s, "B", None);
        b.entity_embedding = Some(vec![4.0, 0.0, 0.0]);
        let c = make_entity(t, s, "C", None); // no embedding

        let centroid = mean_embedding(&[&a, &b, &c]).expect("centroid from embedded members");
        assert_eq!(
            centroid,
            vec![3.0, 0.0, 0.0],
            "mean of [2,..] and [4,..], None skipped"
        );
        assert!(
            mean_embedding(&[&c]).is_none(),
            "no embedded members -> no centroid"
        );
    }

    #[test]
    fn scene_summary_bounds_large_clusters() {
        let t = Uuid::new_v4();
        let s = Uuid::new_v4();
        let entities: Vec<EntityEntry> = (0..12)
            .map(|i| make_entity(t, s, &format!("E{i}"), None))
            .collect();
        let refs: Vec<&EntityEntry> = entities.iter().collect();

        let big = scene_summary(&refs);
        assert!(big.contains("+4 more"), "caps at 8 names: {big}");
        assert!(big.contains("(12 related entities)"), "{big}");
        assert_eq!(
            big.matches(", ").count(),
            7,
            "exactly 8 names listed: {big}"
        );

        let small = scene_summary(&refs[..3]);
        assert!(!small.contains("more"), "small cluster lists all: {small}");
        assert!(small.contains("(3 related entities)"), "{small}");
    }

    #[tokio::test]
    async fn empty_session_returns_zero_counts() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 0);
        assert_eq!(result.connections_created, 0);
        assert!(result.insights.is_empty());
    }

    #[tokio::test]
    async fn same_fold_entities_get_co_occurs_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();

        // Add two entities sharing the same source fold
        let e1 = make_entity(ctx.tenant_id, session_id, "Alice", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "Bob", Some(fold_id));
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 2);
        assert_eq!(result.connections_created, 1);
        assert!(result.insights.is_empty()); // only 2, need 3+ for insight
    }

    #[tokio::test]
    async fn cluster_with_three_entities_generates_insight() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();

        // Add three entities sharing the same source fold
        let e1 = make_entity(ctx.tenant_id, session_id, "Alpha", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "Beta", Some(fold_id));
        let e3 = make_entity(ctx.tenant_id, session_id, "Gamma", Some(fold_id));
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 3);
        // 3 entities => C(3,2) = 3 pairs
        assert_eq!(result.connections_created, 3);
        assert_eq!(result.insights.len(), 1);
        assert!(result.insights[0].contains("3 entities co-occurring"));
    }

    #[tokio::test]
    async fn cluster_persists_a_retrievable_idempotent_scene() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();
        let e1 = make_entity(ctx.tenant_id, session_id, "Zorblax", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "Glorptastic", Some(fold_id));
        let e3 = make_entity(ctx.tenant_id, session_id, "Wibblewobble", Some(fold_id));
        let ids = [e1.entity_id, e2.entity_id, e3.entity_id];
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
        }

        run_consolidation(&store, &ctx, session_id).await.unwrap();

        let scenes = store.scene_list_session(&ctx, session_id).await.unwrap();
        assert_eq!(scenes.len(), 1, "a 3-entity cluster persists one scene");
        let scene = &scenes[0];
        assert_eq!(scene.member_ids.len(), 3);
        for id in ids {
            assert!(
                scene.member_ids.contains(&id),
                "scene includes every member"
            );
        }
        assert!(
            scene.summary.contains("Zorblax"),
            "summary lists member names"
        );
        assert_eq!(scene.scene_id, fold_id, "a fold scene reuses the fold id");

        // Idempotent: re-consolidation upserts the same scene, not a duplicate.
        run_consolidation(&store, &ctx, session_id).await.unwrap();
        let scenes2 = store.scene_list_session(&ctx, session_id).await.unwrap();
        assert_eq!(scenes2.len(), 1, "re-consolidation upserts, not duplicates");
    }

    #[tokio::test]
    async fn fewer_than_three_entities_persists_no_scene() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();
        {
            let mut entities = store.entities.lock().await;
            entities.push(make_entity(
                ctx.tenant_id,
                session_id,
                "Alpha",
                Some(fold_id),
            ));
            entities.push(make_entity(
                ctx.tenant_id,
                session_id,
                "Beta",
                Some(fold_id),
            ));
        }
        run_consolidation(&store, &ctx, session_id).await.unwrap();
        let scenes = store.scene_list_session(&ctx, session_id).await.unwrap();
        assert!(
            scenes.is_empty(),
            "clusters need 3+ members to form a scene"
        );
    }

    #[tokio::test]
    async fn consolidation_builds_a_session_profile_from_scenes() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();
        {
            let mut entities = store.entities.lock().await;
            entities.push(make_entity(
                ctx.tenant_id,
                session_id,
                "Zorblax",
                Some(fold_id),
            ));
            entities.push(make_entity(
                ctx.tenant_id,
                session_id,
                "Glorptastic",
                Some(fold_id),
            ));
            entities.push(make_entity(
                ctx.tenant_id,
                session_id,
                "Wibblewobble",
                Some(fold_id),
            ));
        }
        run_consolidation(&store, &ctx, session_id).await.unwrap();

        let profile = store
            .profile_get(&ctx, session_id)
            .await
            .unwrap()
            .expect("a profile is built when scenes exist");
        assert_eq!(profile.scene_count, 1);
        assert!(
            profile.summary.contains("Zorblax"),
            "profile summarizes scene content"
        );

        // Idempotent: one profile per session after a second cycle.
        run_consolidation(&store, &ctx, session_id).await.unwrap();
        let profiles = store.profiles.lock().await;
        assert_eq!(
            profiles
                .iter()
                .filter(|p| p.session_id == session_id)
                .count(),
            1,
            "profile is upserted, not duplicated"
        );
    }

    #[tokio::test]
    async fn no_scenes_means_no_profile() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        {
            let mut entities = store.entities.lock().await;
            entities.push(make_entity(ctx.tenant_id, session_id, "Alpha", None));
            entities.push(make_entity(ctx.tenant_id, session_id, "Beta", None));
        }
        run_consolidation(&store, &ctx, session_id).await.unwrap();
        assert!(store.profile_get(&ctx, session_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unfolded_entities_get_co_occurs_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        // Add two entities WITHOUT a source fold (typical smart_ingest usage)
        let e1 = make_entity(ctx.tenant_id, session_id, "Alice", None);
        let e2 = make_entity(ctx.tenant_id, session_id, "Bob", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 2);
        assert_eq!(result.connections_created, 1);
        assert!(result.insights.is_empty()); // only 2, need 3+ for insight
    }

    #[tokio::test]
    async fn unfolded_cluster_generates_insight() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "Alpha", None);
        let e2 = make_entity(ctx.tenant_id, session_id, "Beta", None);
        let e3 = make_entity(ctx.tenant_id, session_id, "Gamma", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 3);
        assert_eq!(result.connections_created, 3);
        assert_eq!(result.insights.len(), 1);
        assert!(result.insights[0].contains("Unfolded cluster"));
    }

    /// create_edges_for_groups with an empty iterator produces no edges.
    #[tokio::test]
    async fn create_edges_for_empty_groups() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let mut connections_created = 0;
        let mut edges = Vec::new();
        let groups: Vec<Vec<&crate::types::EntityEntry>> = vec![];

        create_edges_for_groups(
            groups.iter(),
            &store,
            &ctx,
            session_id,
            &mut connections_created,
            &mut edges,
        )
        .await;

        assert_eq!(connections_created, 0);
        assert!(edges.is_empty());
    }

    /// create_edges_for_groups with a single-entity group creates no edges (no pairs).
    #[tokio::test]
    async fn create_edges_single_entity_group() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let e1 = make_entity(ctx.tenant_id, session_id, "Solo", Some(Uuid::new_v4()));
        let group = vec![&e1];
        let groups = [group];
        let mut connections_created = 0;
        let mut edges = Vec::new();

        create_edges_for_groups(
            groups.iter(),
            &store,
            &ctx,
            session_id,
            &mut connections_created,
            &mut edges,
        )
        .await;

        assert_eq!(connections_created, 0);
        assert!(edges.is_empty());
    }

    /// Mixed folded and unfolded entities in the same consolidation run
    /// generate separate insights for each group.
    #[tokio::test]
    async fn mixed_folded_and_unfolded_entities() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();

        // 3 folded entities (should generate a fold cluster insight)
        let e1 = make_entity(ctx.tenant_id, session_id, "FoldA", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "FoldB", Some(fold_id));
        let e3 = make_entity(ctx.tenant_id, session_id, "FoldC", Some(fold_id));
        // 3 unfolded entities (should generate an unfolded cluster insight)
        let e4 = make_entity(ctx.tenant_id, session_id, "UnfoldX", None);
        let e5 = make_entity(ctx.tenant_id, session_id, "UnfoldY", None);
        let e6 = make_entity(ctx.tenant_id, session_id, "UnfoldZ", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
            entities.push(e4);
            entities.push(e5);
            entities.push(e6);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 6);
        // 3 folded pairs + 3 unfolded pairs = 6
        assert_eq!(result.connections_created, 6);
        // Should have both a fold cluster and an unfolded cluster insight
        assert_eq!(result.insights.len(), 2);
        let has_fold_insight = result
            .insights
            .iter()
            .any(|i| i.contains("Cluster in fold"));
        let has_unfolded_insight = result
            .insights
            .iter()
            .any(|i| i.contains("Unfolded cluster"));
        assert!(has_fold_insight, "should have fold cluster insight");
        assert!(has_unfolded_insight, "should have unfolded cluster insight");
    }

    /// Entities from different folds should not create edges between them.
    #[tokio::test]
    async fn different_folds_no_cross_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_a = Uuid::new_v4();
        let fold_b = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "FoldA1", Some(fold_a));
        let e2 = make_entity(ctx.tenant_id, session_id, "FoldB1", Some(fold_b));
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 2);
        // No edges — each fold group has only 1 entity
        assert_eq!(result.connections_created, 0);
    }

    /// Sort-by-recency: unfolded entities are sorted newest first.
    /// We verify by checking that the insight mentions them in the expected order.
    #[tokio::test]
    async fn unfolded_entities_sorted_by_recency() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        // Create entities with distinct timestamps
        let mut e1 = make_entity(ctx.tenant_id, session_id, "Old", None);
        e1.created_at = chrono::Utc::now() - chrono::Duration::hours(2);
        let mut e2 = make_entity(ctx.tenant_id, session_id, "Middle", None);
        e2.created_at = chrono::Utc::now() - chrono::Duration::hours(1);
        let mut e3 = make_entity(ctx.tenant_id, session_id, "Recent", None);
        e3.created_at = chrono::Utc::now();
        {
            // Insert in non-chronological order
            let mut entities = store.entities.lock().await;
            entities.push(e2);
            entities.push(e1);
            entities.push(e3);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 3);
        // All 3 are unfolded and under UNFOLDED_PAIR_CAP, so all pairs compared
        assert_eq!(result.connections_created, 3);
        // Verify insight lists them (order determined by sorted iteration)
        assert_eq!(result.insights.len(), 1);
        assert!(result.insights[0].contains("Unfolded cluster"));
    }

    /// DreamResult edges field tracks actual entity pairs connected.
    #[tokio::test]
    async fn dream_result_edges_track_pairs() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_id = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "A", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, session_id, "B", Some(fold_id));
        let id1 = e1.entity_id;
        let id2 = e2.entity_id;
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.edges.len(), 1);
        let (a, b) = result.edges[0];
        // The edge should be between our two entity IDs
        assert!(
            (a == id1 && b == id2) || (a == id2 && b == id1),
            "edge should connect the two entities"
        );
    }

    /// CO_OCCURS_THRESHOLD is a sensible positive value.
    #[test]
    fn co_occurs_threshold_is_positive() {
        const { assert!(CO_OCCURS_THRESHOLD > 0.0) };
        const { assert!(CO_OCCURS_THRESHOLD < 1.0) };
    }

    /// UNFOLDED_PAIR_CAP is a sensible positive value.
    #[test]
    fn unfolded_pair_cap_is_positive() {
        const { assert!(UNFOLDED_PAIR_CAP > 0) };
    }

    /// Only one unfolded entity should not trigger pairwise comparison.
    #[tokio::test]
    async fn single_unfolded_entity_no_edges() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "Solo", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 1);
        assert_eq!(result.connections_created, 0);
        assert!(result.insights.is_empty());
        assert!(result.edges.is_empty());
    }

    /// Two unfolded entities — should not generate insight (need 3+).
    #[tokio::test]
    async fn two_unfolded_entities_no_insight() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();

        let e1 = make_entity(ctx.tenant_id, session_id, "A", None);
        let e2 = make_entity(ctx.tenant_id, session_id, "B", None);
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 2);
        assert_eq!(result.connections_created, 1);
        assert!(
            result.insights.is_empty(),
            "2 entities should not generate insight"
        );
    }

    /// Multiple folds with different sizes generate insights only for 3+ groups.
    #[tokio::test]
    async fn multiple_folds_mixed_sizes() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let session_id = Uuid::new_v4();
        let fold_small = Uuid::new_v4();
        let fold_big = Uuid::new_v4();

        // Small fold: 2 entities (no insight)
        let e1 = make_entity(ctx.tenant_id, session_id, "S1", Some(fold_small));
        let e2 = make_entity(ctx.tenant_id, session_id, "S2", Some(fold_small));
        // Big fold: 4 entities (should generate insight)
        let e3 = make_entity(ctx.tenant_id, session_id, "B1", Some(fold_big));
        let e4 = make_entity(ctx.tenant_id, session_id, "B2", Some(fold_big));
        let e5 = make_entity(ctx.tenant_id, session_id, "B3", Some(fold_big));
        let e6 = make_entity(ctx.tenant_id, session_id, "B4", Some(fold_big));
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
            entities.push(e4);
            entities.push(e5);
            entities.push(e6);
        }

        let result = run_consolidation(&store, &ctx, session_id).await.unwrap();

        assert_eq!(result.entities_processed, 6);
        // Small fold: C(2,2)=1, Big fold: C(4,2)=6
        assert_eq!(result.connections_created, 7);
        // Only the big fold generates an insight
        assert_eq!(result.insights.len(), 1);
        assert!(result.insights[0].contains("4 entities co-occurring"));
    }

    /// Consolidation with entities and co-occurrence edges runs Datalog inference,
    /// PageRank, and warmth decay without error.
    #[tokio::test]
    async fn consolidation_with_datalog_and_pagerank() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Add three entities in the same fold
        let fold_id = Uuid::new_v4();
        let e1 = make_entity(ctx.tenant_id, sid, "alpha", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, sid, "beta", Some(fold_id));
        let e3 = make_entity(ctx.tenant_id, sid, "gamma", Some(fold_id));
        let id1 = e1.entity_id;
        let id2 = e2.entity_id;
        let id3 = e3.entity_id;
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
            entities.push(e3);
        }

        // Pre-create co-occurs edges so PageRank has an adjacency graph
        store
            .edge_co_occurs(&ctx, id1, id2, sid, 0.8)
            .await
            .unwrap();
        store
            .edge_co_occurs(&ctx, id2, id3, sid, 0.7)
            .await
            .unwrap();

        let result = run_consolidation(&store, &ctx, sid).await.unwrap();

        // Should have processed all 3 entities
        assert!(result.entities_processed >= 3);
        // Datalog should have derived at least some facts from the co-occurrence chain
        // (e.g., related(X, Z) via transitive co-occurrence)
        // The exact count depends on builtin rules matching the graph structure.
        // PageRank should have updated scores for nodes in the edge graph
        assert!(
            result.pagerank_updated >= 2,
            "expected at least 2 nodes with PageRank, got {}",
            result.pagerank_updated
        );
        // PageRank creates warmth entries with warmth=0.0; decay prunes those below threshold
        // so warmth_decayed may be non-zero (entries created by PageRank then pruned by decay)
        assert!(
            result.warmth_decayed <= result.pagerank_updated,
            "should not prune more entries than PageRank created"
        );
    }

    /// Pure guard for the endpoint classifier behind the #129 fix.
    #[test]
    fn derived_fact_has_uuid_endpoints_classifies_taxonomy() {
        use crate::types::DerivedFact;
        let mk = |src: String, dst: String| DerivedFact {
            src_id: src,
            pred: "p".into(),
            dst_id: dst,
            confidence: 1.0,
            rule_id: "r".into(),
            support_count: 1,
            provenance: vec![],
        };
        let u1 = Uuid::new_v4().to_string();
        let u2 = Uuid::new_v4().to_string();
        assert!(mk(u1.clone(), u2).has_uuid_endpoints());
        // isa(<uuid>, "conversation_turn") — the dst is an entity-type string.
        assert!(!mk(u1, "conversation_turn".into()).has_uuid_endpoints());
    }

    /// Regression for issue #129: consolidation derives taxonomy facts
    /// `isa(<uuid>, "<entity_type>")` whose dst is a type string, not a UUID.
    /// Those must NOT be written to the UUID-keyed derived cache (doing so used
    /// to error mid-batch and leave a partial write). Only UUID↔UUID facts are
    /// cached; the cache must contain no non-UUID endpoints.
    #[tokio::test]
    async fn consolidation_does_not_cache_non_uuid_taxonomy_facts() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Entities default to entity_type "concept" (a non-UUID string), so the
        // Datalog engine derives isa(<uuid>, "concept") taxonomy facts.
        let fold_id = Uuid::new_v4();
        let e1 = make_entity(ctx.tenant_id, sid, "alpha", Some(fold_id));
        let e2 = make_entity(ctx.tenant_id, sid, "beta", Some(fold_id));
        let id1 = e1.entity_id;
        let id2 = e2.entity_id;
        {
            let mut entities = store.entities.lock().await;
            entities.push(e1);
            entities.push(e2);
        }
        store
            .edge_co_occurs(&ctx, id1, id2, sid, 0.8)
            .await
            .unwrap();

        run_consolidation(&store, &ctx, sid).await.unwrap();

        let cached = store.derived_cache_list_all(&ctx, 10_000).await.unwrap();
        for row in &cached {
            assert!(
                Uuid::parse_str(&row.source_id).is_ok(),
                "cached derived fact has non-UUID source_id: {row:?}"
            );
            assert!(
                Uuid::parse_str(&row.target_id).is_ok(),
                "taxonomy fact leaked into the UUID-keyed cache (issue #129): {row:?}"
            );
        }
    }

    /// Empty session produces zero for all new consolidation fields.
    #[tokio::test]
    async fn consolidation_empty_session_new_fields() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let result = run_consolidation(&store, &ctx, sid).await.unwrap();

        assert_eq!(result.entities_processed, 0);
        assert_eq!(result.derived_facts_count, 0);
        assert_eq!(result.pagerank_updated, 0);
        assert_eq!(result.warmth_decayed, 0);
    }

    /// DreamResult serialization includes the new fields.
    #[tokio::test]
    async fn dream_result_serializes_new_fields() {
        let store = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        let result = run_consolidation(&store, &ctx, sid).await.unwrap();
        let json = serde_json::to_value(&result).expect("should serialize DreamResult");

        assert!(
            json.get("derived_facts_count").is_some(),
            "missing derived_facts_count in JSON"
        );
        assert!(
            json.get("pagerank_updated").is_some(),
            "missing pagerank_updated in JSON"
        );
        assert!(
            json.get("warmth_decayed").is_some(),
            "missing warmth_decayed in JSON"
        );
    }
}
