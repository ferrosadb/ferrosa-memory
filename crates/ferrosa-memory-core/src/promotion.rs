//! Workload-driven promotion pipeline for durable materialization.
//!
//! Predicates are promoted from ephemeral cache to durable storage when
//! query frequency x compute cost exceeds a threshold, bounded by size budget.
//!
//! Promotion score = query_count_7d x median_compute_ms x reuse_factor / max(update_rate_7d, 1)

use uuid::Uuid;

use crate::config::PromotionConfig;
use crate::datalog;
use crate::storage::Storage;
use crate::types::*;

/// Compute the promotion score for a predicate based on heat telemetry.
///
/// Formula: query_count x avg_compute_ms x reuse_factor / max(update_rate, 1)
/// From Datalog spec section 15.3.
pub fn compute_promotion_score(heat: &PredicateHeat, config: &PromotionConfig) -> f64 {
    let avg_compute_ms = if heat.total_requests > 0 {
        heat.total_compute_ms as f64 / heat.total_requests as f64
    } else {
        0.0
    };
    let update_rate = 1.0_f64; // TODO: track update rate per predicate
    heat.total_hits as f64 * avg_compute_ms * config.reuse_factor / update_rate.max(1.0)
}

/// Evaluate whether a predicate should be promoted to durable materialization.
///
/// Checks: promotion_score >= threshold AND estimated_rows <= size_budget.
pub fn should_promote(
    heat: &PredicateHeat,
    estimated_rows: usize,
    config: &PromotionConfig,
) -> bool {
    let score = compute_promotion_score(heat, config);
    score >= config.promotion_threshold && estimated_rows <= config.size_budget_rows
}

/// Materialize a predicate's derived facts to durable storage.
///
/// 1. Load session facts and evaluate Datalog rules
/// 2. Filter to the target predicate
/// 3. Clear existing materializations for this predicate
/// 4. Write new materializations with batch_id
/// 5. Write provenance for each materialized edge
/// 6. Update promotion registry
pub async fn batch_materialize(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    predicate: &str,
    config: &PromotionConfig,
) -> anyhow::Result<usize> {
    let datalog_config = crate::config::DatalogConfig::default();

    // Load and evaluate
    let facts = datalog::load_session_facts(storage, ctx, session_id).await?;
    let rules = datalog::builtin_rules();
    let (_all_facts, derived) = datalog::evaluate(
        &rules,
        &facts,
        datalog_config.max_iterations,
        datalog_config.max_facts,
    );

    // Filter to target predicate
    let target_facts: Vec<&DerivedFact> = derived.iter().filter(|d| d.pred == predicate).collect();

    let count = target_facts.len();
    if count == 0 {
        return Ok(0);
    }

    let batch_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();

    // Clear old materializations
    storage.materialized_edges_clear(ctx, predicate).await?;

    // Write new materializations
    let shard = 0_i16; // Single shard for now
    for fact in &target_facts {
        let edge = MaterializedEdge {
            tenant_id: ctx.tenant_id,
            src_id: fact.src_id.clone(),
            shard,
            pred: fact.pred.clone(),
            dst_id: fact.dst_id.clone(),
            rule_id: fact.rule_id.clone(),
            support_count: fact.support_count,
            confidence: fact.confidence,
            batch_id: batch_id.clone(),
            materialized_at: now,
        };
        storage.materialized_edge_put(ctx, &edge).await?;

        // Write provenance
        if !fact.provenance.is_empty() {
            let edge_id = format!("{}:{}:{}", fact.src_id, fact.pred, fact.dst_id);
            storage
                .provenance_put(ctx, &edge_id, &fact.provenance)
                .await?;
        }
    }

    // Update promotion registry
    let heat = PredicateHeat {
        pred: predicate.to_string(),
        total_hits: 0,
        total_compute_ms: 0,
        total_requests: 0,
        days_observed: 0,
    };
    let score = compute_promotion_score(&heat, config);
    let entry = PromotedPredicate {
        tenant_id: ctx.tenant_id,
        pred: predicate.to_string(),
        promotion_score: score,
        estimated_rows: count as i32,
        materialized_at: Some(now),
        batch_id: Some(batch_id),
        status: PromotionStatus::Promoted,
    };
    storage.promoted_predicate_put(ctx, &entry).await?;

    Ok(count)
}

/// Check all derived predicates and promote those meeting the threshold.
/// Called during consolidation.
pub async fn check_and_promote(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    config: &PromotionConfig,
) -> anyhow::Result<Vec<String>> {
    // Get heat data for known predicates
    let known_predicates = [
        "related",
        "cluster",
        "reachable",
        "isa",
        "class_ancestor",
        "ancestor_part",
    ];
    let mut promoted = Vec::new();

    for pred in &known_predicates {
        let (hits, compute_ms) = storage.heat_get(ctx, pred, config.window_days).await?;
        if hits == 0 {
            continue;
        }

        let heat = PredicateHeat {
            pred: pred.to_string(),
            total_hits: hits,
            total_compute_ms: compute_ms,
            total_requests: hits, // approximate
            days_observed: config.window_days,
        };

        // Estimate rows (rough: use entity count as proxy)
        let entity_count = storage.entity_count(ctx, session_id).await.unwrap_or(0);
        let estimated_rows = entity_count * entity_count / 10; // rough estimate for transitive predicates

        if should_promote(&heat, estimated_rows, config) {
            match batch_materialize(storage, ctx, session_id, pred, config).await {
                Ok(count) => {
                    tracing::info!(pred, count, "promoted predicate to durable materialization");
                    promoted.push(pred.to_string());
                }
                Err(e) => {
                    tracing::warn!(pred, error = %e, "failed to materialize predicate");
                }
            }
        }
    }

    Ok(promoted)
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

    #[test]
    fn test_promotion_score_zero_requests() {
        let heat = PredicateHeat {
            pred: "related".into(),
            total_hits: 100,
            total_compute_ms: 0,
            total_requests: 0,
            days_observed: 7,
        };
        let config = PromotionConfig::default();
        assert!((compute_promotion_score(&heat, &config) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_promotion_score_formula() {
        let heat = PredicateHeat {
            pred: "related".into(),
            total_hits: 100,
            total_compute_ms: 5000,
            total_requests: 100,
            days_observed: 7,
        };
        let config = PromotionConfig {
            reuse_factor: 1.0,
            ..Default::default()
        };
        // score = 100 * (5000/100) * 1.0 / 1.0 = 5000
        let score = compute_promotion_score(&heat, &config);
        assert!((score - 5000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_should_promote_above_threshold() {
        let heat = PredicateHeat {
            pred: "related".into(),
            total_hits: 1000,
            total_compute_ms: 50000,
            total_requests: 1000,
            days_observed: 7,
        };
        let config = PromotionConfig {
            promotion_threshold: 1000.0,
            size_budget_rows: 100000,
            ..Default::default()
        };
        assert!(should_promote(&heat, 500, &config));
    }

    #[test]
    fn test_should_not_promote_below_threshold() {
        let heat = PredicateHeat {
            pred: "related".into(),
            total_hits: 1,
            total_compute_ms: 10,
            total_requests: 1,
            days_observed: 7,
        };
        let config = PromotionConfig::default();
        assert!(!should_promote(&heat, 500, &config));
    }

    #[test]
    fn test_should_not_promote_exceeds_budget() {
        let heat = PredicateHeat {
            pred: "related".into(),
            total_hits: 10000,
            total_compute_ms: 100000,
            total_requests: 10000,
            days_observed: 7,
        };
        let config = PromotionConfig {
            promotion_threshold: 100.0,
            size_budget_rows: 10,
            ..Default::default()
        };
        assert!(!should_promote(&heat, 1000, &config)); // 1000 > budget 10
    }

    #[tokio::test]
    async fn test_batch_materialize_empty() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();
        let config = PromotionConfig::default();

        let count = batch_materialize(&storage, &ctx, sid, "related", &config)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_batch_materialize_with_data() {
        let storage = MockStorage::new();
        let ctx = test_ctx();
        let sid = Uuid::new_v4();

        // Create entities with co-occurs edges
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        for (eid, name) in [(a, "alpha"), (b, "beta"), (c, "gamma")] {
            storage
                .entity_put(
                    &ctx,
                    &EntityEntry {
                        tenant_id: ctx.tenant_id,
                        entity_id: eid,
                        session_id: sid,
                        entity_name: name.into(),
                        entity_type: "concept".into(),
                        source_fold_id: None,
                        context_snippet: format!("{name} entity"),
                        entity_embedding: None,
                        confidence: 0.9,
                        state: MemoryState::Active,
                        created_at: chrono::Utc::now(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }

        storage.edge_co_occurs(&ctx, a, b, sid, 0.8).await.unwrap();
        storage.edge_co_occurs(&ctx, b, c, sid, 0.7).await.unwrap();

        let config = PromotionConfig::default();
        let count = batch_materialize(&storage, &ctx, sid, "related", &config)
            .await
            .unwrap();

        // Should have materialized related(a,c) at minimum (transitive through b)
        assert!(count >= 1);

        // Check promotion registry
        let promoted = storage
            .promoted_predicate_get(&ctx, "related")
            .await
            .unwrap();
        assert!(promoted.is_some());
        assert_eq!(promoted.unwrap().status, PromotionStatus::Promoted);
    }
}
