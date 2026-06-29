//! `debug_stop` degraded-cluster alerts.
//!
//! When the hidden `[server].debug_stop` flag is set, every tool response reflects
//! cluster health so a developer's agent **stops and investigates** instead of
//! building on a silently-degraded cluster. Severity-based:
//!
//! - **Degraded but serving** (e.g. one DB node down but quorum holds) → a
//!   prominent `alert` block is attached to the response; tools still serve.
//! - **Critical** (DB quorum lost, or a *configured* external provider fully
//!   down) → the tool returns `is_error` so the agent halts immediately.
//!
//! When `debug_stop` is unset, [`evaluate`] returns `None` and behavior is
//! unchanged — this is a dev-only affordance, silent in production.
//!
//! This module owns the pure severity policy ([`evaluate`]), the serving-path
//! injection ([`apply_debug_stop`]), and a background TCP-reachability monitor
//! ([`HealthMonitor`]) that publishes the snapshot the serving path reads.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// Last-known health of a single monitored component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Up,
    Down,
}

/// A point-in-time view of the components `debug_stop` watches. `None` for a
/// provider means it is **not configured** (and therefore not monitored).
#[derive(Debug, Clone, Default)]
pub struct HealthSnapshot {
    pub db_nodes_up: usize,
    pub db_nodes_total: usize,
    pub embedding: Option<Health>,
    pub reranker: Option<Health>,
}

impl HealthSnapshot {
    /// Quorum holds when a strict majority of DB nodes are reachable. With no
    /// nodes configured (`db_nodes_total == 0`) we treat quorum as held — the
    /// DB component is simply not being monitored.
    pub fn quorum(&self) -> bool {
        self.db_nodes_total == 0 || self.db_nodes_up * 2 > self.db_nodes_total
    }
}

/// Whether the agent should warn-and-continue or stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Degraded but still serving — surface a warning; do not fail the call.
    Degraded,
    /// Critical — the call should fail so the agent halts.
    Critical,
}

/// The alert attached to tool responses (or surfaced as an error) when
/// `debug_stop` is set and a component is unhealthy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugStopAlert {
    pub debug_stop: bool,
    pub severity: Severity,
    /// Human-readable failed/degraded components.
    pub degraded: Vec<String>,
    pub action: &'static str,
}

impl DebugStopAlert {
    /// True when the call should fail loud (`is_error`) rather than warn.
    pub fn is_critical(&self) -> bool {
        self.severity == Severity::Critical
    }
}

/// Evaluate the `debug_stop` alert for a snapshot.
///
/// Returns `None` when `debug_stop` is off, or when everything monitored is
/// healthy. Otherwise returns an alert whose severity is `Critical` if DB quorum
/// is lost or a configured provider is fully down, else `Degraded`.
pub fn evaluate(snap: &HealthSnapshot, debug_stop: bool) -> Option<DebugStopAlert> {
    if !debug_stop {
        return None;
    }

    let mut degraded = Vec::new();
    let mut critical = false;

    if snap.db_nodes_total > 0 && snap.db_nodes_up < snap.db_nodes_total {
        if snap.quorum() {
            degraded.push(format!(
                "{} of {} DB nodes unreachable (quorum OK)",
                snap.db_nodes_total - snap.db_nodes_up,
                snap.db_nodes_total
            ));
        } else {
            degraded.push(format!(
                "DB quorum lost ({}/{} nodes reachable)",
                snap.db_nodes_up, snap.db_nodes_total
            ));
            critical = true;
        }
    }

    // A *configured* provider that is fully down is critical.
    if snap.embedding == Some(Health::Down) {
        degraded.push("embedding provider unreachable".to_string());
        critical = true;
    }
    if snap.reranker == Some(Health::Down) {
        degraded.push("LLM reranker unreachable".to_string());
        critical = true;
    }

    if degraded.is_empty() {
        return None;
    }

    Some(DebugStopAlert {
        debug_stop: true,
        severity: if critical {
            Severity::Critical
        } else {
            Severity::Degraded
        },
        degraded,
        action: "STOP and investigate",
    })
}

/// JSON-RPC error code returned when `debug_stop` fails a call on critical
/// degradation. Distinct from generic internal errors so clients can recognize it.
pub const DEBUG_STOP_CRITICAL: i32 = -32010;

/// Apply `debug_stop` to a tool result given the current `snapshot`.
///
/// - off / everything healthy → the result is returned unchanged.
/// - **critical** (quorum lost / configured provider down) → the call **fails**
///   (`Err`) so the agent halts, regardless of the tool's own outcome.
/// - **degraded but serving** → a `debug_stop_alert` is attached to a successful
///   object result (non-object results are wrapped under `result`); an already
///   failing call is left as-is.
pub fn apply_debug_stop(
    result: Result<Value, (i32, String)>,
    snapshot: &HealthSnapshot,
    debug_stop: bool,
) -> Result<Value, (i32, String)> {
    let Some(alert) = evaluate(snapshot, debug_stop) else {
        return result;
    };
    let summary = alert.degraded.join("; ");
    if alert.is_critical() {
        return Err((
            DEBUG_STOP_CRITICAL,
            format!(
                "debug_stop: cluster critically degraded — {summary}. {}",
                alert.action
            ),
        ));
    }
    result.map(|mut value| {
        let alert_json = serde_json::to_value(&alert).unwrap_or(Value::Null);
        match value.as_object_mut() {
            Some(obj) => {
                obj.insert("debug_stop_alert".to_string(), alert_json);
            }
            None => {
                value = serde_json::json!({ "result": value, "debug_stop_alert": alert_json });
            }
        }
        value
    })
}

/// Background reachability monitor. Periodically TCP-probes the configured DB
/// nodes and external providers and publishes a [`HealthSnapshot`] read by
/// [`apply_debug_stop`] on the serving path — so the per-call cost is a lock
/// read, never network I/O.
///
/// TCP-reachability is a deliberately coarse but **honest** signal: it confirms
/// the port answers, not that the component is fully healthy. We never fabricate
/// health — until the first probe completes the snapshot is `Default` (no nodes,
/// no providers), which [`evaluate`] treats as "nothing monitored yet" rather
/// than a false all-clear or a false alarm.
pub struct HealthMonitor {
    db_endpoints: Vec<String>,
    embedding: Option<String>,
    reranker: Option<String>,
    snapshot: Arc<Mutex<HealthSnapshot>>,
}

impl HealthMonitor {
    pub fn new(
        db_endpoints: Vec<String>,
        embedding: Option<String>,
        reranker: Option<String>,
        snapshot: Arc<Mutex<HealthSnapshot>>,
    ) -> Self {
        Self {
            db_endpoints,
            embedding,
            reranker,
            snapshot,
        }
    }

    /// Probe every component once and publish the resulting snapshot.
    pub async fn probe_once(&self) {
        let mut up = 0usize;
        for ep in &self.db_endpoints {
            if tcp_reachable(ep).await {
                up += 1;
            }
        }
        let embedding = match &self.embedding {
            Some(ep) => Some(health_of(ep).await),
            None => None,
        };
        let reranker = match &self.reranker {
            Some(ep) => Some(health_of(ep).await),
            None => None,
        };
        let next = HealthSnapshot {
            db_nodes_up: up,
            db_nodes_total: self.db_endpoints.len(),
            embedding,
            reranker,
        };
        if let Ok(mut guard) = self.snapshot.lock() {
            *guard = next;
        }
    }

    /// Probe forever at `interval`; spawn on the runtime at startup.
    pub async fn run(self, interval: Duration) {
        loop {
            self.probe_once().await;
            tokio::time::sleep(interval).await;
        }
    }
}

async fn health_of(host_port: &str) -> Health {
    if tcp_reachable(host_port).await {
        Health::Up
    } else {
        Health::Down
    }
}

async fn tcp_reachable(host_port: &str) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(host_port),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Extract a `host:port` authority from a URL (or bare authority). Returns `None`
/// when no explicit port is present — we don't guess a port across schemes, and
/// an unprobable endpoint is left unmonitored rather than faked.
pub fn endpoint_authority(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .unwrap_or(s);
    let authority = s.split('/').next().unwrap_or(s);
    authority.contains(':').then(|| authority.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap(up: usize, total: usize) -> HealthSnapshot {
        HealthSnapshot {
            db_nodes_up: up,
            db_nodes_total: total,
            ..Default::default()
        }
    }

    #[test]
    fn off_is_always_silent() {
        // debug_stop unset → never an alert, even with a dead cluster.
        let mut s = snap(0, 3);
        s.embedding = Some(Health::Down);
        assert_eq!(evaluate(&s, false), None);
    }

    #[test]
    fn all_healthy_is_silent() {
        assert_eq!(evaluate(&snap(3, 3), true), None);
    }

    #[test]
    fn one_node_down_quorum_ok_is_degraded() {
        let a = evaluate(&snap(2, 3), true).expect("alert");
        assert_eq!(a.severity, Severity::Degraded);
        assert!(!a.is_critical());
        assert_eq!(a.degraded, vec!["1 of 3 DB nodes unreachable (quorum OK)"]);
    }

    #[test]
    fn quorum_lost_is_critical() {
        let a = evaluate(&snap(1, 3), true).expect("alert");
        assert_eq!(a.severity, Severity::Critical);
        assert!(a.is_critical());
        assert!(a.degraded[0].contains("quorum lost"));
    }

    #[test]
    fn configured_provider_down_is_critical_even_with_db_quorum() {
        let mut s = snap(3, 3);
        s.embedding = Some(Health::Down);
        let a = evaluate(&s, true).expect("alert");
        assert_eq!(a.severity, Severity::Critical);
        assert!(a.degraded.iter().any(|d| d.contains("embedding")));
    }

    #[test]
    fn healthy_or_unconfigured_providers_do_not_alert() {
        let mut s = snap(3, 3);
        s.embedding = Some(Health::Up); // configured + healthy
        s.reranker = None; // not configured
        assert_eq!(evaluate(&s, true), None);
    }

    #[test]
    fn multiple_failures_all_listed_and_critical() {
        let mut s = snap(1, 3); // quorum lost
        s.reranker = Some(Health::Down);
        let a = evaluate(&s, true).expect("alert");
        assert_eq!(a.severity, Severity::Critical);
        assert_eq!(a.degraded.len(), 2);
        assert!(a.degraded.iter().any(|d| d.contains("quorum lost")));
        assert!(a.degraded.iter().any(|d| d.contains("reranker")));
    }

    #[test]
    fn apply_is_passthrough_when_off_or_healthy() {
        // off: returns the result untouched even with a dead cluster.
        let mut dead = snap(0, 3);
        dead.embedding = Some(Health::Down);
        let r = apply_debug_stop(Ok(json!({"x": 1})), &dead, false).unwrap();
        assert_eq!(r, json!({"x": 1}));
        // on but healthy: also untouched.
        let r = apply_debug_stop(Ok(json!({"x": 1})), &snap(3, 3), true).unwrap();
        assert_eq!(r, json!({"x": 1}));
    }

    #[test]
    fn apply_attaches_alert_on_degraded() {
        let r = apply_debug_stop(Ok(json!({"x": 1})), &snap(2, 3), true).unwrap();
        assert_eq!(r["x"], 1, "tool result preserved");
        assert_eq!(r["debug_stop_alert"]["severity"], "degraded");
        assert_eq!(r["debug_stop_alert"]["action"], "STOP and investigate");
    }

    #[test]
    fn apply_wraps_non_object_result_on_degraded() {
        let r = apply_debug_stop(Ok(json!([1, 2, 3])), &snap(2, 3), true).unwrap();
        assert_eq!(r["result"], json!([1, 2, 3]));
        assert_eq!(r["debug_stop_alert"]["severity"], "degraded");
    }

    #[test]
    fn apply_fails_loud_on_critical() {
        let (code, msg) = apply_debug_stop(Ok(json!({"x": 1})), &snap(1, 3), true).unwrap_err();
        assert_eq!(code, DEBUG_STOP_CRITICAL);
        assert!(msg.contains("quorum lost"), "{msg}");
        assert!(msg.contains("STOP and investigate"), "{msg}");
    }

    #[test]
    fn endpoint_authority_parses_url_and_requires_port() {
        assert_eq!(
            endpoint_authority("http://127.0.0.1:11434"),
            Some("127.0.0.1:11434".to_string())
        );
        assert_eq!(
            endpoint_authority("https://host:1234/v1/embeddings"),
            Some("host:1234".to_string())
        );
        assert_eq!(
            endpoint_authority("localhost:19042"),
            Some("localhost:19042".to_string())
        );
        // no explicit port → unprobable → not monitored (never guessed).
        assert_eq!(endpoint_authority("http://example.com"), None);
    }

    #[tokio::test]
    #[ignore = "requires the live 3-node dev cluster on 19042-19044"]
    async fn monitor_probes_live_cluster_reachability() {
        let snapshot = Arc::new(Mutex::new(HealthSnapshot::default()));
        let monitor = HealthMonitor::new(
            vec![
                "127.0.0.1:19042".to_string(),
                "127.0.0.1:19043".to_string(),
                "127.0.0.1:19044".to_string(),
            ],
            // a port that nothing listens on → must read as Down (honest, not faked)
            Some("127.0.0.1:1".to_string()),
            None,
            Arc::clone(&snapshot),
        );
        monitor.probe_once().await;
        let s = snapshot.lock().unwrap().clone();
        assert_eq!(s.db_nodes_total, 3);
        assert_eq!(s.db_nodes_up, 3, "all 3 dev nodes should be reachable");
        assert!(s.quorum());
        assert_eq!(
            s.embedding,
            Some(Health::Down),
            "unbound port reads as Down"
        );
        // With a configured provider down, evaluate must be Critical.
        assert_eq!(
            evaluate(&s, true).map(|a| a.severity),
            Some(Severity::Critical)
        );
    }
}
