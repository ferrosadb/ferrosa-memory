//! Backing off a poll that keeps finding nothing.
//!
//! The consolidation worker polls a durable queue on a fixed interval. That
//! poll is a full ring scan by construction — `consolidation_requests` is
//! keyed by `(tenant_id, session_id)`, so "outstanding for this tenant" cannot
//! address a partition (see `t_a553df97`). It therefore costs the same whether
//! there is work or not, and an idle deployment pays it forever.
//!
//! The real fix is event-driven CDC (`t_25c736b8`). This is not a stopgap that
//! that work deletes: the CDC decision explicitly keeps "a bounded
//! low-frequency sweep as the correctness backstop", because the CDC bus is
//! lossy by design with no replay or checkpoint. A sweep that slows down when
//! idle is precisely the backstop that design asks for.
//!
//! ## Why this is a separate type
//!
//! The decision — *how long until the next poll* — is a pure function of what
//! the last passes found. Extracting it from the async loop is what makes it
//! testable at all; the loop itself is closed over a runtime, a storage handle
//! and a channel, and runs forever.
//!
//! ## Reporting edges, not events
//!
//! [`Transition`] exists so a caller logs twice per idle period — once when it
//! starts backing off, once when work returns — rather than once per tick. A
//! line per tick is what made the original scan invisible for two months: the
//! polls were happening the whole time, finding nothing, at full cost, and
//! looking exactly like a worker doing its job.

use std::time::Duration;

/// What changed about the backoff on the last pass, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Nothing worth saying.
    Steady,
    /// The poll has gone idle and is now slowing down.
    BeganBackingOff,
    /// Work returned; the poll is back at its base interval.
    Resumed,
}

/// How long to wait before polling again, given what the last passes found.
#[derive(Debug, Clone)]
pub struct IdleBackoff {
    base: Duration,
    max: Duration,
    current: Duration,
    idle_passes: u32,
}

impl IdleBackoff {
    /// A base interval to use while there is work, and a ceiling to grow to
    /// while there is not.
    ///
    /// Both are clamped into something usable. Config is a file a person
    /// edits, and a base of zero would spin the loop as fast as the runtime
    /// allows — worse than the cost this exists to remove. A max below the
    /// base is taken to mean "do not back off" rather than "poll faster than
    /// asked".
    pub fn new(base: Duration, max: Duration) -> Self {
        let base = base.max(Duration::from_secs(1));
        Self {
            base,
            max: max.max(base),
            current: base,
            idle_passes: 0,
        }
    }

    /// How long to wait before the next poll.
    pub fn delay(&self) -> Duration {
        self.current
    }

    /// How many consecutive passes have found nothing.
    pub fn idle_passes(&self) -> u32 {
        self.idle_passes
    }

    /// Record what a pass found, and get back whether that is worth saying.
    pub fn record(&mut self, found_work: bool) -> Transition {
        if found_work {
            let was_backing_off = self.idle_passes > 0;
            self.idle_passes = 0;
            // Reset outright rather than decaying back down. The moment there
            // IS work, latency matters more than scan cost, and the first
            // request after a quiet night should not wait through a curve.
            self.current = self.base;
            return if was_backing_off {
                Transition::Resumed
            } else {
                Transition::Steady
            };
        }

        self.idle_passes = self.idle_passes.saturating_add(1);
        let first = self.idle_passes == 1;
        // Double, capped. Capped because a queue that went quiet overnight
        // must still notice the morning's first request promptly.
        self.current = self.current.saturating_mul(2).min(self.max);
        if first {
            Transition::BeganBackingOff
        } else {
            Transition::Steady
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_secs(20);
    const MAX: Duration = Duration::from_secs(300);

    fn backoff() -> IdleBackoff {
        IdleBackoff::new(BASE, MAX)
    }

    #[test]
    fn a_fresh_poll_runs_at_its_base_interval() {
        assert_eq!(backoff().delay(), BASE);
    }

    #[test]
    fn finding_work_keeps_it_at_the_base_interval() {
        let mut b = backoff();
        for _ in 0..5 {
            assert_eq!(b.record(true), Transition::Steady);
            assert_eq!(b.delay(), BASE, "a busy queue must not be slowed down");
        }
    }

    #[test]
    fn an_idle_pass_slows_the_next_one_down() {
        let mut b = backoff();
        b.record(false);
        assert!(b.delay() > BASE);
    }

    #[test]
    fn the_delay_is_capped_however_long_it_stays_idle() {
        // Unbounded growth would mean a queue that went quiet overnight takes
        // hours to notice the morning's first request.
        let mut b = backoff();
        for _ in 0..100 {
            b.record(false);
        }
        assert_eq!(b.delay(), MAX);
    }

    #[test]
    fn work_resets_it_immediately_rather_than_stepping_back_down() {
        // Latency matters more than scan cost the moment there IS work: the
        // first request after a quiet night should not wait through a decay
        // curve.
        let mut b = backoff();
        for _ in 0..10 {
            b.record(false);
        }
        assert_eq!(b.delay(), MAX);
        assert_eq!(b.record(true), Transition::Resumed);
        assert_eq!(b.delay(), BASE);
    }

    // ── Edges, so a caller can log twice per outage ───────────────

    #[test]
    fn backing_off_is_announced_once_not_every_tick() {
        let mut b = backoff();
        assert_eq!(b.record(false), Transition::BeganBackingOff);
        for _ in 0..20 {
            assert_eq!(
                b.record(false),
                Transition::Steady,
                "a line per idle tick is what made the original scan invisible"
            );
        }
    }

    #[test]
    fn resuming_is_announced_once_and_only_after_backing_off() {
        let mut b = backoff();
        // Never idle, so returning work is not an event.
        assert_eq!(b.record(true), Transition::Steady);

        b.record(false);
        assert_eq!(b.record(true), Transition::Resumed);
        assert_eq!(b.record(true), Transition::Steady);
    }

    #[test]
    fn it_reports_how_long_it_has_been_idle() {
        // So the resume line can say what was skipped, rather than leaving
        // someone to infer it from timestamps.
        let mut b = backoff();
        assert_eq!(b.idle_passes(), 0);
        for _ in 0..4 {
            b.record(false);
        }
        assert_eq!(b.idle_passes(), 4);
        b.record(true);
        assert_eq!(b.idle_passes(), 0);
    }

    // ── The property that motivates the whole thing ───────────────

    #[test]
    fn an_idle_day_costs_far_fewer_scans_than_a_fixed_interval() {
        // The measured burn was a full ring scan every tick, forever. This is
        // the number that has to come down.
        let mut b = backoff();
        let day = Duration::from_secs(24 * 60 * 60);

        let mut elapsed = Duration::ZERO;
        let mut scans = 0u32;
        while elapsed < day {
            elapsed += b.delay();
            b.record(false);
            scans += 1;
        }

        let fixed = day.as_secs() / BASE.as_secs();
        assert!(
            u64::from(scans) < fixed / 10,
            "expected an order of magnitude fewer than {fixed} scans, got {scans}"
        );
    }

    #[test]
    fn a_zero_or_inverted_range_still_produces_a_usable_interval() {
        // Config comes from a file a person edits. A base of zero would spin
        // the loop as fast as the runtime allows, which is worse than the bug
        // being fixed.
        let b = IdleBackoff::new(Duration::ZERO, Duration::ZERO);
        assert!(b.delay() >= Duration::from_secs(1), "never a busy loop");

        let mut inverted = IdleBackoff::new(Duration::from_secs(60), Duration::from_secs(5));
        for _ in 0..10 {
            inverted.record(false);
        }
        assert!(
            inverted.delay() >= Duration::from_secs(60),
            "a max below the base must not shorten the base"
        );
    }
}
