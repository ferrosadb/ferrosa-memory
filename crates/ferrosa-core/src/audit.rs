//! Append-only audit log (STRIDE R1).
//!
//! Records every write operation to the audit_log table. Audit rows cannot
//! be deleted via MCP tools — they are write-only through this module.
//!
//! Also tracks entity retrieval frequency for anomaly detection (FMEA F19).

use uuid::Uuid;

use crate::config::SecurityConfig;
use crate::metrics::MemoryMetrics;
use crate::types::AuditEntry;

/// Log an audit entry for a write operation.
///
/// Non-fatal: if the audit write fails, it's logged as a warning but
/// doesn't block the primary operation.
pub fn log_write(
    tenant_id: Uuid,
    session_id: Uuid,
    operation: &str,
    target_table: &str,
    target_id: &str,
) -> AuditEntry {
    let entry = AuditEntry {
        tenant_id,
        audit_id: Uuid::now_v7(),
        operation: operation.to_string(),
        target_table: target_table.to_string(),
        target_id: target_id.to_string(),
        session_id,
        created_at: chrono::Utc::now(),
    };
    tracing::debug!(operation, target_table, target_id, "audit log entry");
    entry
}

/// Check if an entity's retrieval frequency exceeds the anomaly threshold.
///
/// Returns `true` if the entity has been retrieved more than `mean + sigma * stddev`
/// times compared to the session baseline.
pub fn check_anomaly(
    retrieval_count: usize,
    session_mean: f64,
    session_stddev: f64,
    config: &SecurityConfig,
    metrics: Option<&MemoryMetrics>,
) -> bool {
    if !config.anomaly_detection_enabled || session_stddev == 0.0 {
        return false;
    }

    let threshold = session_mean + config.anomaly_sigma_threshold * session_stddev;
    let is_anomalous = (retrieval_count as f64) > threshold;

    if is_anomalous {
        tracing::warn!(
            retrieval_count,
            threshold,
            sigma = config.anomaly_sigma_threshold,
            "entity retrieval anomaly detected"
        );
        if let Some(m) = metrics {
            m.poisoning_flags.with_label_values(&["anomaly"]).inc();
        }
    }

    is_anomalous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_entry_created() {
        let entry = log_write(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "entity_put",
            "entity_store",
            "some-entity-id",
        );
        assert_eq!(entry.operation, "entity_put");
        assert_eq!(entry.target_table, "entity_store");
    }

    #[test]
    fn anomaly_not_triggered_below_threshold() {
        let config = SecurityConfig {
            anomaly_detection_enabled: true,
            anomaly_sigma_threshold: 3.0,
            audit_log_enabled: true,
        };
        assert!(!check_anomaly(5, 3.0, 1.0, &config, None));
    }

    #[test]
    fn anomaly_triggered_above_threshold() {
        let config = SecurityConfig {
            anomaly_detection_enabled: true,
            anomaly_sigma_threshold: 3.0,
            audit_log_enabled: true,
        };
        assert!(check_anomaly(10, 3.0, 1.0, &config, None));
    }

    #[test]
    fn anomaly_disabled() {
        let config = SecurityConfig {
            anomaly_detection_enabled: false,
            anomaly_sigma_threshold: 3.0,
            audit_log_enabled: true,
        };
        assert!(!check_anomaly(100, 3.0, 1.0, &config, None));
    }
}
