//! Injected time source. The only place that reads the system clock.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A source of unix-millisecond timestamps.
pub trait Clock: Send + Sync {
    /// Current time in unix milliseconds.
    fn now_ms(&self) -> i64;
}

/// The real wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        millis(since_epoch)
    }
}

/// A clock that only moves when told to.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicI64,
}

impl ManualClock {
    /// A clock starting at `now_ms`.
    #[must_use]
    pub fn new(now_ms: i64) -> Self {
        Self {
            now: AtomicI64::new(now_ms),
        }
    }

    /// Jump to an absolute time.
    pub fn set(&self, now_ms: i64) {
        self.now.store(now_ms, Ordering::SeqCst);
    }

    /// Move forward by `by`.
    pub fn advance(&self, by: Duration) {
        self.now.fetch_add(millis(by), Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_starts_where_told() {
        assert_eq!(ManualClock::new(42).now_ms(), 42);
    }

    #[test]
    fn manual_clock_set_jumps() {
        let c = ManualClock::new(1);
        c.set(1_000);
        assert_eq!(c.now_ms(), 1_000);
    }

    #[test]
    fn manual_clock_advance_adds_milliseconds() {
        let c = ManualClock::new(1_000);
        c.advance(Duration::from_secs(2));
        assert_eq!(c.now_ms(), 3_000);
    }

    #[test]
    fn system_clock_is_after_2020() {
        assert!(SystemClock.now_ms() > 1_577_836_800_000);
    }
}
