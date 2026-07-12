//! Time, injected.
//!
//! Every timestamp the store *originates* — a lease's expiry, a cache entry's
//! deadline, a flow's `updated_at` — comes from a [`Clock`], never from
//! `SystemTime::now()` reached for in place. Production uses [`SystemClock`];
//! tests and simulation use [`ManualClock`], whose reading only moves when
//! told to, so a lease-expiry or TTL test is exact rather than racy.

use core::fmt;
use core::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of "now", as integer milliseconds since the Unix epoch — the unit
/// every timestamp column in the journal is measured in.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now_ms(&self) -> i64 {
        (**self).now_ms()
    }
}

/// The wall clock. Pre-1970 system times (only reachable via a
/// grossly-misconfigured host) clamp to 0 rather than panic.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}

/// A settable clock for deterministic tests and simulation. Clones share one
/// reading (via [`Arc`]), so two store handles built from clones of the same
/// `ManualClock` observe identical time — exactly what lease-contention and
/// TTL tests need.
#[derive(Debug, Clone)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    /// Start the clock at `now_ms`.
    pub fn new(now_ms: i64) -> Self {
        Self(Arc::new(AtomicI64::new(now_ms)))
    }

    /// Jump the clock to an absolute reading.
    pub fn set(&self, now_ms: i64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }

    /// Move the clock forward by `delta_ms`.
    pub fn advance(&self, delta_ms: i64) {
        self.0.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
