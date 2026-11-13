//! Capped exponential backoff with jitter, shared by connection, audit, growth
//! and subscription retries.

use std::time::Duration;
use tokio::time::Instant;

/// Delays grow as `base * 2^attempt`, capped, plus up to 25% jitter. `fail`
/// also records a wake-up time so callers can await independent operations.
pub(super) struct Backoff {
    attempt: u32,
    base: Duration,
    cap: Duration,
    until: Option<Instant>,
}

impl Backoff {
    pub(super) fn new(base: Duration, cap: Duration) -> Self {
        Self {
            attempt: 0,
            base,
            cap,
            until: None,
        }
    }

    pub(super) fn reset(&mut self) {
        *self = Self::new(self.base, self.cap);
    }

    /// The current retry delay, without advancing the attempt counter.
    fn delay(&self) -> Duration {
        let base = self.base.as_millis().min(u128::from(u64::MAX)) as u64;
        let cap = self.cap.as_millis().min(u128::from(u64::MAX)) as u64;
        let millis = base.saturating_mul(1u64 << self.attempt.min(20)).min(cap);
        Duration::from_millis(millis) + jitter(millis)
    }

    pub(super) fn next_delay(&mut self) -> Duration {
        let delay = self.delay();
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    pub(super) fn ready(&self, now: Instant) -> bool {
        self.until.is_none_or(|until| now >= until)
    }

    pub(super) fn until(&self) -> Option<Instant> {
        self.until
    }

    pub(super) fn set_until(&mut self, until: Instant) {
        self.until = Some(until);
    }

    /// Record a failed attempt: the next retry becomes due after the delay.
    ///
    /// The deadline is measured from when the failure was observed, never from a
    /// timestamp captured before the attempt started: an operation that takes
    /// longer than its own backoff (a slow HTTP timeout, say) would otherwise
    /// come back already due and skip the whole schedule.
    pub(super) fn fail(&mut self) {
        let delay = self.delay();
        self.attempt = self.attempt.saturating_add(1);
        self.until = Some(Instant::now() + delay);
    }
}

fn jitter(delay_millis: u64) -> Duration {
    let span = (delay_millis / 4).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_millis(nanos % span)
}
