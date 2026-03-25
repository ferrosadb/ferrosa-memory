//! Append-only audit log (STRIDE R1).
//!
//! Records every write operation to the audit_log table. Audit rows cannot
//! be deleted via MCP tools — they are write-only through this module.
//!
//! Also tracks entity retrieval frequency for anomaly detection (FMEA F19).

use uuid::Uuid;

use crate::config::SecurityConfig;
use crate::metrics::MemoryMetrics;
use crate::storage::Storage;
use crate::types::{AuditEntry, TenantContext};

/// Log an audit entry for a write operation and persist it to storage.
///
/// Returns the `audit_id` of the created entry.
pub async fn log_write(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    operation: &str,
    target_table: &str,
    target_id: &str,
    session_id: Uuid,
) -> anyhow::Result<Uuid> {
    let entry = AuditEntry {
        tenant_id: ctx.tenant_id,
        audit_id: Uuid::now_v7(),
        operation: operation.to_string(),
        target_table: target_table.to_string(),
        target_id: target_id.to_string(),
        session_id,
        created_at: chrono::Utc::now(),
    };
    tracing::debug!(operation, target_table, target_id, "audit log entry");
    storage.audit_put(ctx, &entry).await?;
    Ok(entry.audit_id)
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
    use crate::storage::mock::MockStorage;

    fn unwrap_tool_result(result: serde_json::Value) -> serde_json::Value {
        let text = result["content"][0]["text"]
            .as_str()
            .expect("CallToolResult missing content[0].text");
        serde_json::from_str(text).unwrap_or(serde_json::Value::String(text.to_string()))
    }

    #[tokio::test]
    async fn audit_entry_created() {
        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let session_id = Uuid::new_v4();
        let audit_id = log_write(
            &storage,
            &ctx,
            "entity_put",
            "entity_store",
            "some-entity-id",
            session_id,
        )
        .await
        .unwrap();

        let entries = storage.audit_entries.lock().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].audit_id, audit_id);
        assert_eq!(entries[0].operation, "entity_put");
        assert_eq!(entries[0].target_table, "entity_store");
        assert_eq!(entries[0].target_id, "some-entity-id");
        assert_eq!(entries[0].session_id, session_id);
        assert_eq!(entries[0].tenant_id, ctx.tenant_id);
    }

    #[test]
    fn anomaly_not_triggered_below_threshold() {
        let config = SecurityConfig {
            anomaly_detection_enabled: true,
            anomaly_sigma_threshold: 3.0,
            audit_log_enabled: true,
            anomaly_alerts_enabled: true,
        };
        assert!(!check_anomaly(5, 3.0, 1.0, &config, None));
    }

    #[test]
    fn anomaly_triggered_above_threshold() {
        let config = SecurityConfig {
            anomaly_detection_enabled: true,
            anomaly_sigma_threshold: 3.0,
            audit_log_enabled: true,
            anomaly_alerts_enabled: true,
        };
        assert!(check_anomaly(10, 3.0, 1.0, &config, None));
    }

    #[test]
    fn anomaly_disabled() {
        let config = SecurityConfig {
            anomaly_detection_enabled: false,
            anomaly_sigma_threshold: 3.0,
            audit_log_enabled: true,
            anomaly_alerts_enabled: true,
        };
        assert!(!check_anomaly(100, 3.0, 1.0, &config, None));
    }

    #[tokio::test]
    async fn upsert_entity_creates_audit_entry() {
        let storage = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let session = crate::dispatch::SessionState::default();
        let sid = Uuid::new_v4();

        let params = serde_json::json!({
            "name": "upsert_entity",
            "arguments": {
                "session_id": sid.to_string(),
                "entity_name": "AuditTestEntity",
                "entity_type": "concept",
                "context_snippet": "testing audit persistence",
                "confidence": 0.9
            }
        });
        let result = crate::dispatch::dispatch("tools/call", params, &storage, &ctx, &session)
            .await
            .unwrap();
        let result = unwrap_tool_result(result);
        assert!(result["entity_id"].is_string());

        let entries = storage.audit_entries.lock().await;
        assert_eq!(entries.len(), 1, "expected one audit entry after upsert");
        assert_eq!(entries[0].operation, "upsert");
        assert_eq!(entries[0].target_table, "entity_store");
        assert_eq!(entries[0].session_id, sid);
        assert_eq!(entries[0].tenant_id, ctx.tenant_id);
    }
}
