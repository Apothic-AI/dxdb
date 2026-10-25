//! Time, injected rather than fetched.
//!
//! `std::time::Instant::now()` aborts on `wasm32-unknown-unknown` ("time not
//! implemented on this platform"), and this workspace's gate requires every crate
//! to build for that target (`make wasm`). So the logger never reads a clock
//! itself: it calls a [`Clock`] the caller supplies. That also makes timings exact
//! in tests instead of flaky (`docs/CLEANROOM-GAPS.md` G-022).

use std::sync::Mutex;

/// One clock reading: a monotonic instant plus, when available, wall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockReading {
    /// A monotonic nanosecond reading. Only differences are meaningful.
    pub monotonic_ns: u64,
    /// Unix time in nanoseconds, when the host can supply it.
    pub unix_ns: Option<u64>,
}

/// The clock the wrapper reads, and the only source of time it trusts.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Reads the clock once.
    ///
    /// The wrapper calls this exactly twice per logged operation with timing
    /// enabled, and never with timing disabled (`INV-3`, `INV-5`).
    fn read(&self) -> ClockReading;
}

/// A monotonic clock backed by `std::time`.
///
/// Not compiled for `wasm32-unknown-unknown`, where `std` has no clock; that
/// target passes its own [`Clock`] (a `performance.now()` adapter at the JS
/// boundary, or a [`ManualClock`] in tests).
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
#[derive(Debug, Clone)]
pub struct MonotonicClock {
    base: std::time::Instant,
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
impl MonotonicClock {
    /// Starts the monotonic reading at zero.
    pub fn new() -> Self {
        Self {
            base: std::time::Instant::now(),
        }
    }
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
impl Clock for MonotonicClock {
    fn read(&self) -> ClockReading {
        ClockReading {
            monotonic_ns: u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX),
            unix_ns: unix_now_ns(),
        }
    }
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn unix_now_ns() -> Option<u64> {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(u64::try_from(since_epoch.as_nanos()).unwrap_or(u64::MAX))
}

/// A clock the caller drives, counting how often it was read.
///
/// Reads are counted so a test can assert that a filtered or time-disabled
/// operation costs zero clock reads (`INV-3`, `INV-5`).
#[derive(Debug)]
pub struct ManualClock {
    state: Mutex<ManualState>,
}

#[derive(Debug, Clone, Copy)]
struct ManualState {
    monotonic_ns: u64,
    unix_ns: Option<u64>,
    step_ns: u64,
    reads: usize,
}

impl ManualClock {
    /// A clock whose first read is `monotonic_ns` (and the same Unix time) and
    /// which does not advance between reads.
    pub fn new(monotonic_ns: u64, unix_ns: Option<u64>) -> Self {
        Self {
            state: Mutex::new(ManualState {
                monotonic_ns,
                unix_ns,
                step_ns: 0,
                reads: 0,
            }),
        }
    }

    /// A clock that advances by `step_ns` on every read, so a duration is an
    /// exact, assertable number rather than a tolerance.
    pub fn stepping(first_ns: u64, step_ns: u64) -> Self {
        Self {
            state: Mutex::new(ManualState {
                monotonic_ns: first_ns,
                unix_ns: Some(first_ns),
                step_ns,
                reads: 0,
            }),
        }
    }

    /// Advances the monotonic reading by `delta_ns`, leaving wall time alone.
    pub fn advance(&self, delta_ns: u64) {
        let mut state = self.state.lock().expect("manual clock mutex poisoned");
        state.monotonic_ns = state.monotonic_ns.saturating_add(delta_ns);
    }

    /// Sets the wall-clock reading.
    pub fn set_unix_ns(&self, unix_ns: Option<u64>) {
        self.state
            .lock()
            .expect("manual clock mutex poisoned")
            .unix_ns = unix_ns;
    }

    /// How many times this clock has been read.
    pub fn reads(&self) -> usize {
        self.state.lock().expect("manual clock mutex poisoned").reads
    }
}

impl Clock for ManualClock {
    fn read(&self) -> ClockReading {
        let mut state = self.state.lock().expect("manual clock mutex poisoned");
        let reading = ClockReading {
            monotonic_ns: state.monotonic_ns,
            unix_ns: state.unix_ns,
        };
        state.reads += 1;
        state.monotonic_ns = state.monotonic_ns.saturating_add(state.step_ns);
        reading
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, ManualClock};

    #[test]
    fn manual_clock_counts_reads_and_advances_explicitly() {
        let clock = ManualClock::new(10, Some(1_000));
        assert_eq!(clock.reads(), 0);
        let first = clock.read();
        clock.advance(5);
        let second = clock.read();
        assert_eq!(clock.reads(), 2);
        assert_eq!(second.monotonic_ns - first.monotonic_ns, 5);
        assert_eq!(second.unix_ns, Some(1_000));
    }

    #[test]
    fn stepping_clock_advances_on_each_read() {
        let clock = ManualClock::stepping(1_000, 250);
        let first = clock.read();
        let second = clock.read();
        assert_eq!(second.monotonic_ns - first.monotonic_ns, 250);
        assert_eq!(clock.reads(), 2);
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    #[test]
    fn monotonic_clock_never_goes_backwards() {
        use super::MonotonicClock;
        let clock = MonotonicClock::new();
        let first = clock.read();
        let second = clock.read();
        assert!(second.monotonic_ns >= first.monotonic_ns);
    }
}
