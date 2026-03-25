//! Per-tenant storage quotas (FMEA D1).
//!
//! Tracks approximate storage usage per tenant and rejects writes
//! when the configured limit is exceeded.

use crate::config::MemoryConfig;

/// Check if a write would exceed the tenant's quota.
///
/// Returns Ok(()) if under quota, Err if exceeded.
pub fn check_quota(current_entity_count: usize, max_entities: usize) -> Result<(), QuotaExceeded> {
    if current_entity_count >= max_entities {
        Err(QuotaExceeded {
            current: current_entity_count,
            limit: max_entities,
        })
    } else {
        Ok(())
    }
}

/// Per-tenant memo result limit check.
pub fn check_memo_quota(current_count: usize, config: &MemoryConfig) -> Result<(), QuotaExceeded> {
    let limit = config.max_memo_results as usize;
    if current_count >= limit {
        Err(QuotaExceeded {
            current: current_count,
            limit,
        })
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub struct QuotaExceeded {
    pub current: usize,
    pub limit: usize,
}

impl std::fmt::Display for QuotaExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "quota exceeded: {} >= {} limit",
            self.current, self.limit
        )
    }
}

impl std::error::Error for QuotaExceeded {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_quota_passes() {
        assert!(check_quota(5, 1000).is_ok());
    }

    #[test]
    fn at_quota_fails() {
        assert!(check_quota(1000, 1000).is_err());
    }

    #[test]
    fn over_quota_fails() {
        let err = check_quota(1001, 1000).unwrap_err();
        assert_eq!(err.current, 1001);
        assert_eq!(err.limit, 1000);
    }
}
