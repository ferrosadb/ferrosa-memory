//! Confidence scoring for temporal facts.
//!
//! Confidence = source_support * recency_bonus * (1 - contradiction_penalty)
//! - source_support: min(source_count / 5, 1.0)
//! - recency_bonus: exp(-age_in_days / 30)
//! - contradiction_penalty: 0.2 * contradiction_count (capped at 0.5)

use sha2::Digest;
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{ConfidenceScore, TenantContext};

pub fn compute_confidence(
    source_count: usize,
    last_confirmed_at: chrono::DateTime<chrono::Utc>,
    contradiction_count: usize,
) -> f64 {
    let source_support = (source_count as f64 / 5.0).min(1.0);
    let age_days = chrono::Utc::now()
        .signed_duration_since(last_confirmed_at)
        .num_days() as f64;
    let recency_bonus = (-age_days / 30.0).exp();
    let contradiction_penalty = (0.2 * contradiction_count as f64).min(0.5);
    (source_support * recency_bonus * (1.0 - contradiction_penalty)).clamp(0.0, 1.0)
}

pub async fn record_fact_confidence(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    entity_id: Uuid,
    fact_text: &str,
    source_count: usize,
    contradiction_count: usize,
) -> anyhow::Result<f64> {
    let fact_hash = hex::encode(sha2::Sha256::digest(fact_text.as_bytes()));
    let now = chrono::Utc::now();
    let confidence = compute_confidence(source_count, now, contradiction_count);
    let score = ConfidenceScore {
        entity_id,
        fact_hash,
        confidence,
        source_count: source_count as i32,
        last_confirmed_at: now,
        contradiction_count: contradiction_count as i32,
    };
    storage.confidence_put(ctx, &score).await?;
    Ok(confidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_confidence_fresh_high_sources() {
        assert!((0.9..=1.0).contains(&compute_confidence(5, chrono::Utc::now(), 0)));
    }

    #[test]
    fn compute_confidence_decayed() {
        let old = chrono::Utc::now() - chrono::Duration::days(60);
        assert!(compute_confidence(5, old, 0) < 0.5);
    }

    #[test]
    fn compute_confidence_contradicted() {
        assert!(compute_confidence(2, chrono::Utc::now(), 3) < 0.5);
    }
}
