//! When to try a dependency again after it refused a connection.
//!
//! # Why this exists
//!
//! The shell extension held its four cluster connections — the task board, the
//! memory tiers, knowledge, and rules — in a `tokio::sync::OnceCell<Option<_>>`.
//! A failed connect stored `Some(None)`: the cell was now initialised, so every
//! later call returned that cached `None` without retrying. One transient
//! failure made the dependency unreachable for the life of the listener
//! process, curable only by a restart.
//!
//! That is a fallback that is designed and observable but NOT recoverable, and
//! the un-recoverable half is what makes it a bug. It was observed in the
//! field: `control-listen` started 47 seconds after the cluster, its first
//! board connect lost the race, and the Work screen read "The task board could
//! not be reached from this machine." for the next fifteen hours while the
//! board sat healthy and listening on the port it had always used.
//!
//! # Why not simply retry every time
//!
//! Because the original comment was right about the other half: "An unreachable
//! board must fail in seconds and say so, not hold the listener while a phone
//! waits." A connect attempt costs up to ten seconds, so retrying on every
//! frame would make a down cluster hang every screen that touches it.
//!
//! So: retry, but no more often than [`RETRY_INTERVAL`]. Between attempts the
//! answer is an immediate `None`, which is the same speed the cache gave and
//! stops being permanent.
//!
//! # Reporting edges, not events
//!
//! A line per failed call would be one line per frame for as long as the outage
//! lasts, which buries the one that mattered. This reports the EDGES: it began
//! failing, and it recovered. Two lines per outage however long it runs.
//!
//! # One concept, not four copies
//!
//! The four dependencies had four copy-pasted `get_or_init` blocks differing
//! only in type, connect call and log wording — which is why they all had the
//! same bug and why fixing it four times would have been the wrong shape.
//! [`ClusterView`] is the single concept: a cluster connection made on first
//! use, kept once it works, retried when it does not, and loud at both edges.
//! Each dependency is now one field and one call.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long to wait before trying a refused dependency again.
///
/// Long enough that a down cluster is not re-dialled on every frame, short
/// enough that a person who restarts the cluster sees the screen recover
/// without restarting anything else.
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Whether a failure is worth a log line, given what was already reported.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Report {
    /// The edge into failure. Say so.
    StartedFailing,
    /// The edge back out. Say so.
    Recovered,
    /// Same state as last time. Stay quiet.
    Unchanged,
}

/// Tracks one dependency's connection attempts.
#[derive(Debug, Default)]
pub(crate) struct RetryGate {
    last_attempt: Option<Instant>,
    failing: bool,
}

impl RetryGate {
    /// Whether to attempt a connection now.
    ///
    /// The first call always attempts: nothing has been tried, so there is no
    /// reason to wait.
    pub(crate) fn should_attempt(&self, now: Instant) -> bool {
        match self.last_attempt {
            None => true,
            Some(last) => now.duration_since(last) >= RETRY_INTERVAL,
        }
    }

    /// Record a connection that succeeded.
    pub(crate) fn record_success(&mut self, now: Instant) -> Report {
        self.last_attempt = Some(now);
        if std::mem::replace(&mut self.failing, false) {
            Report::Recovered
        } else {
            Report::Unchanged
        }
    }

    /// Record a connection that failed.
    pub(crate) fn record_failure(&mut self, now: Instant) -> Report {
        self.last_attempt = Some(now);
        if std::mem::replace(&mut self.failing, true) {
            Report::Unchanged
        } else {
            Report::StartedFailing
        }
    }
}

/// A cluster connection made on first use, kept once it works, and retried
/// rather than written off when it does not.
///
/// Holds a `Mutex` rather than a `OnceCell` because the answer has to be able
/// to change: a `OnceCell` can only go from empty to decided, and "decided"
/// was the bug. Contention is not a concern — the lock is held for one clone
/// on the hot path, and only spans a connect attempt when there is nothing
/// cached to return.
pub(crate) struct ClusterView<T> {
    /// What this is, for the log line. "task board", "memory tiers".
    label: &'static str,
    state: tokio::sync::Mutex<ViewState<T>>,
}

struct ViewState<T> {
    connected: Option<Arc<T>>,
    gate: RetryGate,
}

impl<T> ClusterView<T> {
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            label,
            state: tokio::sync::Mutex::new(ViewState {
                connected: None,
                gate: RetryGate::default(),
            }),
        }
    }

    /// The connection, connecting if it is time to try.
    ///
    /// `None` means unreachable right now, not unreachable forever: the next
    /// call after [`RETRY_INTERVAL`] tries again. Between attempts this returns
    /// immediately, so a down cluster costs one screen its content rather than
    /// costing every screen ten seconds.
    pub(crate) async fn get<F, Fut>(&self, connect: F) -> Option<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        let mut state = self.state.lock().await;
        if let Some(existing) = state.connected.clone() {
            return Some(existing);
        }
        if !state.gate.should_attempt(Instant::now()) {
            return None;
        }
        match connect().await {
            Ok(view) => {
                let view = Arc::new(view);
                state.connected = Some(Arc::clone(&view));
                if state.gate.record_success(Instant::now()) == Report::Recovered {
                    tracing::info!(dependency = self.label, "reachable again");
                }
                Some(view)
            }
            Err(error) => {
                // Loud, and not fatal. Agents keep working without it. Once
                // per outage, not once per frame.
                if state.gate.record_failure(Instant::now()) == Report::StartedFailing {
                    tracing::warn!(%error, dependency = self.label, "unavailable");
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing has been tried, so there is nothing to wait for.
    #[test]
    fn the_first_attempt_is_immediate() {
        assert!(RetryGate::default().should_attempt(Instant::now()));
    }

    /// THE BUG. A failure used to be permanent; now it is a wait.
    #[test]
    fn a_failure_is_retried_once_the_interval_passes() {
        let start = Instant::now();
        let mut gate = RetryGate::default();
        gate.record_failure(start);

        assert!(
            !gate.should_attempt(start + Duration::from_secs(1)),
            "a down cluster must not be re-dialled on every frame"
        );
        assert!(
            gate.should_attempt(start + RETRY_INTERVAL),
            "and must not stay unreachable for the life of the process"
        );
    }

    /// A success is kept. The caller holds the connection and does not come
    /// back through the gate while it works.
    #[test]
    fn a_success_needs_no_further_attempts() {
        let start = Instant::now();
        let mut gate = RetryGate::default();
        assert_eq!(gate.record_success(start), Report::Unchanged);
    }

    /// The two edges, and only the edges.
    #[test]
    fn only_the_edges_are_reported() {
        let start = Instant::now();
        let mut gate = RetryGate::default();

        assert_eq!(gate.record_failure(start), Report::StartedFailing);
        // However long the outage runs, it is not re-announced.
        assert_eq!(
            gate.record_failure(start + RETRY_INTERVAL),
            Report::Unchanged
        );
        assert_eq!(
            gate.record_failure(start + RETRY_INTERVAL * 2),
            Report::Unchanged
        );
        // Coming back is worth exactly one line.
        assert_eq!(
            gate.record_success(start + RETRY_INTERVAL * 3),
            Report::Recovered
        );
        assert_eq!(
            gate.record_success(start + RETRY_INTERVAL * 4),
            Report::Unchanged
        );
    }

    /// A dependency that fails, recovers, then fails again announces the second
    /// outage. Latching the flag would report the first one only.
    #[test]
    fn a_second_outage_is_announced_too() {
        let start = Instant::now();
        let mut gate = RetryGate::default();
        gate.record_failure(start);
        gate.record_success(start + RETRY_INTERVAL);
        assert_eq!(
            gate.record_failure(start + RETRY_INTERVAL * 2),
            Report::StartedFailing
        );
    }

    /// A successful connection is made once and then handed out, not remade.
    #[tokio::test]
    async fn a_working_dependency_connects_once() {
        let view = ClusterView::<u32>::new("test");
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let connect = || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(7u32) }
        };

        assert_eq!(view.get(connect).await.as_deref(), Some(&7));
        assert_eq!(view.get(connect).await.as_deref(), Some(&7));
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a connection that works is kept, not remade per call"
        );
    }

    /// THE BUG, at the level the callers see. A refused connection used to be
    /// cached as a permanent `None`; the second call must not repeat it, and
    /// must not hang either.
    #[tokio::test]
    async fn a_refused_dependency_is_not_written_off() {
        let view = ClusterView::<u32>::new("test");
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let refuse = || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { anyhow::bail!("cluster down") }
        };

        assert!(view.get(refuse).await.is_none());
        // Immediately after, the gate holds it off -- a down cluster must not
        // be re-dialled on every frame.
        assert!(view.get(refuse).await.is_none());
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the retry is rate limited, not attempted per call"
        );

        // And the failure is not permanent: once the interval passes it
        // reconnects, which is the whole point.
        view.state.lock().await.gate.last_attempt = Some(Instant::now() - RETRY_INTERVAL);
        assert_eq!(view.get(|| async { Ok(7u32) }).await.as_deref(), Some(&7));
    }
}
