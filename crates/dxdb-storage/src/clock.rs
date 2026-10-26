//! Time, as a capability (D4 in `docs/DECISIONS.md`).
//!
//! Upstream's `now()` (`plugins/utils/utils-time.ts:24`) is a module-global
//! counter, and that is not an implementation detail — it is wire behaviour:
//!
//! ```text
//! _lastNowMs, _lastNowSub are module globals shared by the whole process.
//! now() = Date.now() > _lastNowMs
//!           ? (_lastNowMs = Date.now(), _lastNowSub = 1)
//!           : (++_lastNowSub === 100 && (_lastNowMs++, _lastNowSub = 1));
//!         _lastNowMs + _lastNowSub * 0.01          // <- expression order matters
//! ```
//!
//! Three properties follow, and each has a reason recorded in upstream's comment
//! block:
//!
//! 1. **Never repeats.** Two writes in the same millisecond get different `lwt`,
//!    which is what makes `(_meta.lwt, primary)` a total order. A whole-ms clock
//!    produces ties that the checkpoint logic relies on not existing.
//! 2. **Monotonic.** `now()` never goes backwards, even if the wall clock does
//!    (NTP step, user changing the clock). It advances the sub-counter and then
//!    the millisecond instead.
//! 3. **Two decimals, and the arithmetic exactly as written.** `ms + k * 0.01`
//!    with `k` in `1..=99` is evaluated left-to-right in IEEE-754 double
//!    precision, so `1700000000000 + 7 * 0.01` is bit-identical in Rust and in JS.
//!    Formatting goes through `dxdb-jsnum`, which produces the shortest
//!    round-tripping decimal, i.e. `1700000000000.07`.
//!
//! Reproducting the *expression* rather than "a number close to it" matters:
//! `_meta.lwt` is stored in documents, compared for ordering, and replicated.
//!
//! [`MonotonicClock`] is the default. A test that needs pinned `lwt` values
//! injects its own [`Clock`] wherever the state taking one lives; there is
//! deliberately **no global override**, because a global setter would make test
//! order observable and would let two tests in one process fight.

use std::cell::Cell;

/// A source of last-write timestamps.
///
/// Implementations must return a finite `f64` that never decreases for a given
/// clock instance.
pub trait Clock {
    /// Milliseconds since the unix epoch, two decimals, never repeating within
    /// one clock.
    fn now(&self) -> f64;
}

/// Advance a `(ms, sub)` pair by one `now()` call, given a wall-clock reading.
///
/// Factored out — and taking `wall_ms` as a parameter rather than reading a clock
/// itself — so the default clock and any injected clock share one implementation
/// of upstream's tie-breaking instead of two that could drift, and so the rule is
/// testable without controlling a real clock.
///
/// `sub` is `1..=99` on return; `99 + 1` rolls the millisecond.
pub fn advance_monotonic(now_ms: u64, sub: u8, wall_ms: u64) -> (u64, u8) {
    if wall_ms > now_ms {
        (wall_ms, 1)
    } else if sub >= 99 {
        // Upstream: `if (++_lastNowSub === 100) { _lastNowMs++; _lastNowSub = 1; }`
        (now_ms + 1, 1)
    } else {
        (now_ms, sub + 1)
    }
}

/// Composes upstream's return value: `ms + sub * 0.01`, in that order.
///
/// Kept as a named function so no caller can "simplify" it to
/// `ms as f64 + (sub as f64) / 100.0` — mathematically equal, not bit-equal for
/// every input, and the difference would be visible in stored `lwt`.
pub fn lwt_from_parts(now_ms: u64, sub: u8) -> f64 {
    now_ms as f64 + (sub as f64) * 0.01
}

/// The default clock: process-local, monotonic, never repeating.
///
/// Backed by a `thread_local` rather than a `static` because this crate forbids
/// `unsafe`, and an atomic `f64` pair would need either `unsafe` or a lock. RxDB
/// has no threads (crate docs), so "process-global" and "thread-local" coincide
/// on the target this serves; on native, each thread gets its own monotonic
/// sequence, which is still monotonic and still non-repeating, so the ordering
/// guarantee that matters is preserved.
#[derive(Clone, Copy, Debug, Default)]
pub struct MonotonicClock;

thread_local! {
    static STATE: Cell<(u64, u8)> = const { Cell::new((0, 0)) };
    /// Overridable wall-clock source, used only by tests that need `now()` to be
    /// pinned without injecting a whole clock. `None` means "read the real
    /// clock".
    static FIXED_WALL_MS: Cell<Option<u64>> = const { Cell::new(None) };
}

impl MonotonicClock {
    /// A new handle. All handles share the process state, like upstream's module
    /// globals.
    pub fn new() -> Self {
        MonotonicClock
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> f64 {
        STATE.with(|state| {
            let (now_ms, sub) = state.get();
            let (next_ms, next_sub) = advance_monotonic(now_ms, sub, current_unix_ms());
            state.set((next_ms, next_sub));
            lwt_from_parts(next_ms, next_sub)
        })
    }
}

/// The process-global monotonic clock, i.e. upstream's `now()`.
///
/// Use this wherever a `&dyn Clock` is not available and the caller is not under
/// test. Prefer taking a `&dyn Clock` on new state so a test can pin time.
pub fn now() -> f64 {
    Clock::now(&MonotonicClock)
}

/// Formats an `lwt` the way it appears in stored JSON and in vectors.
///
/// Two decimals means the *value* has two decimals; the produced string drops
/// trailing zeros exactly as JavaScript does
/// (`String(1700000000000.1)` is `"1700000000000.1"`, not `"...10"`).
pub fn format_lwt(lwt: f64) -> String {
    dxdb_jsnum::to_js_string(lwt)
}

/// A clock whose value a test sets explicitly.
///
/// Advances by one upstream sub-tick per call unless the test sets an exact
/// value, so a test can either pin a sequence or step it deterministically.
#[derive(Debug)]
pub struct ManualClock {
    inner: Cell<(u64, u8)>,
    fixed: bool,
}

impl ManualClock {
    /// Starts at `now_ms + 0.01` and ticks like the monotonic clock.
    pub fn new(now_ms: u64) -> Self {
        ManualClock {
            inner: Cell::new((now_ms, 0)),
            fixed: false,
        }
    }

    /// Always returns `lwt`, unchanged, on every call.
    ///
    /// This is the "two writes in one millisecond" case a test needs when it is
    /// asserting that the tie is *possible*; the default clock never produces it.
    pub fn fixed(lwt: f64) -> Self {
        let ms = lwt.trunc().max(0.0) as u64;
        let sub = (((lwt - lwt.trunc()) * 100.0).round() as u8).min(99);
        ManualClock {
            inner: Cell::new((ms, sub)),
            fixed: true,
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> f64 {
        let (ms, sub) = self.inner.get();
        if self.fixed {
            return lwt_from_parts(ms, sub);
        }
        // A manual clock never consults the wall clock: its whole purpose is to
        // be independent of it. Passing `ms` means "no new millisecond", so the
        // sub-tick advances (and rolls over at 99).
        let (next_ms, next_sub) = advance_monotonic(ms, sub, ms);
        self.inner.set((next_ms, next_sub));
        lwt_from_parts(next_ms, next_sub)
    }
}

/// Sets the wall-clock value the default clock reads, for tests that cannot
/// inject a clock.
///
/// `None` restores the real clock. This exists because `cleanup` and other
/// upstream-shaped calls take no clock parameter — matching upstream's signature
/// is more important than test purity there. Tests that *can* inject a clock
/// should, because this one is process-wide.
pub fn set_wall_clock_ms(value: Option<u64>) {
    FIXED_WALL_MS.with(|cell| cell.set(value));
}

fn current_unix_ms() -> u64 {
    match FIXED_WALL_MS.with(|cell| cell.get()) {
        Some(fixed) => fixed,
        None => real_unix_ms(),
    }
}

/// Milliseconds since the unix epoch, floored.
///
/// `std::time::SystemTime` works on `wasm32-unknown-unknown` (it reads the JS
/// `Date.now()` through the std shim) and on WASI, so no platform binding is
/// needed here. A pre-epoch clock yields 0 rather than panicking: a system with a
/// clock set before 1970 is broken, but it should not abort a write.
fn real_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lwt_shape_is_ms_plus_sub_tick() {
        // 7 * 0.01 in IEEE-754 is what JS also computes for `7 * 0.01`.
        assert_eq!(
            lwt_from_parts(1_700_000_000_000, 7),
            1_700_000_000_000.0 + 7.0 * 0.01
        );
        // Two decimals, formatted the JS way.
        assert_eq!(
            format_lwt(lwt_from_parts(1_700_000_000_000, 7)),
            "1700000000000.07"
        );
        assert_eq!(
            format_lwt(lwt_from_parts(1_700_000_000_000, 1)),
            "1700000000000.01"
        );
    }

    #[test]
    fn sub_counter_rolls_over_into_the_millisecond() {
        // Same millisecond: the sub-tick advances, then rolls into ms + 1.
        assert_eq!(advance_monotonic(0, 99, 0), (1, 1));
        assert_eq!(advance_monotonic(0, 98, 0), (0, 99));
        // A newer wall clock wins outright and restarts the sub-tick at 1.
        assert_eq!(advance_monotonic(0, 50, 900), (900, 1));
        // A wall clock that went *backwards* must not move us backwards.
        assert_eq!(advance_monotonic(900, 5, 100), (900, 6));
    }

    #[test]
    fn the_default_clock_never_repeats_and_never_decreases() {
        let clock = MonotonicClock::new();
        let mut previous = clock.now();
        for _ in 0..500 {
            let current = clock.now();
            assert!(current > previous, "{current} must exceed {previous}");
            previous = current;
            // The sub-tick half must stay inside `1..=99` hundredths.
            let fraction = current - current.trunc();
            assert!((0.0..1.0).contains(&fraction));
        }
    }

    #[test]
    fn a_fixed_wall_clock_still_advances_the_sub_tick() {
        // Pinning the wall clock is how a test forces the "many writes in one
        // millisecond" path, which the real clock almost never takes.
        set_wall_clock_ms(Some(1_000));
        let clock = MonotonicClock::new();
        let first = clock.now();
        let second = clock.now();
        set_wall_clock_ms(None);
        assert_eq!(first, 1_000.01);
        assert_eq!(second, 1_000.02);
    }

    #[test]
    fn a_manual_clock_can_be_pinned() {
        let pinned = ManualClock::fixed(1_500.5);
        assert_eq!(pinned.now(), 1_500.5);
        assert_eq!(pinned.now(), 1_500.5);

        let ticking = ManualClock::new(1_500);
        assert_eq!(ticking.now(), 1_500.01);
        assert_eq!(ticking.now(), 1_500.02);
    }
}
