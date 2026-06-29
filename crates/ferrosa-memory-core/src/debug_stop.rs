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
//! This module is the pure severity policy; detection (counting reachable DB
//! nodes, probing providers) and response injection live at the call sites.

use serde::Serialize;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
