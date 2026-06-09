//! Clock port. The scheduler reads "now" repeatedly (soft-gate deadlines, run
//! timestamps); injecting it keeps scheduler/pipeline decisions deterministic
//! under test. Production uses [`SystemClock`]; tests use a fixed/:advanceable
//! fake.

use std::sync::Arc;

/// Source of the current time in Unix epoch milliseconds.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Convenience: a shared `SystemClock`.
pub fn system() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}
