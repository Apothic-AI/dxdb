//! Execution is injected, not assumed (D3 in `docs/DECISIONS.md`).
//!
//! Upstream relies on "fire and forget, await at the end" in places —
//! `persistFromMaster`, the un-awaited `setCheckpoint` — because JavaScript has a
//! free event loop. Rust has none, and PLAN.md 3.3/9 bar `tokio` from the core by
//! policy (it compiles for wasm32; its timers and IO do not work there). So a
//! place that wants to start work and not await it immediately takes a
//! [`Spawner`].
//!
//! # The default is sequential, and that is correct
//!
//! [`SequentialSpawner`] queues the future and drives it on the next
//! [`Spawner::drive_pending`] call, then resolves [`Spawner::yield_now`] so the
//! code that called `spawn` gets a chance to run first. That reproduces the
//! *observable* contract of upstream's `Promise.resolve().then(...)` — work
//! starts on a later turn, and a caller that awaits at the end sees every future
//! finished — without a runtime.
//!
//! It is slower than real parallelism and it is the behaviour the conformance
//! suites are run against (D3), so the sequential path is the one proven correct
//! rather than the one nobody tested.
//!
//! # Reporting whether it spawns
//!
//! [`Spawner::spawns_concurrently`] exists so a test can assert which path ran.
//! The sequential spawner returns `false`; a platform spawner returns `true`. A
//! test that asserts `false` proves the default; a separate test with a real
//! spawner proves the opt-in path, which is D3's stated consequence for tests.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

/// A future that is not yet finished, boxed so a spawner can hold futures of
/// different concrete types in one queue.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Decides where an un-awaited future runs.
pub trait Spawner {
    /// Hands `future` to the spawner.
    ///
    /// A spawner may begin polling immediately or queue it; either way the
    /// contract is only that the future *will* be driven, not when.
    ///
    /// # Panics
    ///
    /// Implementations may panic if polled from a different thread than the one
    /// they were created on; RxDB state is not `Send` (crate docs).
    fn spawn(&self, future: BoxFuture<'static, ()>);

    /// True when this spawner is backed by a real executor that can make progress
    /// on several futures at once. `false` means "this is the sequential default".
    ///
    /// This is the assertion hook D3 requires: a test calls it to prove it
    /// exercised the sequential path rather than silently getting a parallel one.
    fn spawns_concurrently(&self) -> bool;

    /// Yields to the caller's context.
    ///
    /// Upstream gets this from `Promise.resolve()`: the current synchronous run
    /// finishes, then the queued work continues. A sequential spawner makes it
    /// "run everything queued, then resolve"; a platform executor makes it a real
    /// yield.
    fn yield_now(&self) -> BoxFuture<'static, ()>;
}

/// The default spawner: a queue driven on demand, no executor, no runtime.
///
/// This is `#[derive(Clone)]` and shares its queue, so it is passed by value and
/// behaves like a handle.
#[derive(Clone, Default)]
pub struct SequentialSpawner {
    shared: Rc<RefCell<VecDeque<BoxFuture<'static, ()>>>>,
}

impl SequentialSpawner {
    /// An empty spawner.
    pub fn new() -> Self {
        SequentialSpawner::default()
    }

    /// Drives every queued future to completion.
    ///
    /// Returns how many futures were driven, so a caller can assert that work it
    /// queued actually ran instead of being silently dropped.
    ///
    /// A future that never completes would spin forever; that is a bug in the
    /// future, not here, and a real executor would hang on it too. The loop is
    /// bounded per future only by completion, matching how a `block_on` behaves.
    pub fn drive_pending(&self) -> usize {
        let mut driven = 0;
        loop {
            let next = self.shared.borrow_mut().pop_front();
            let Some(mut future) = next else {
                return driven;
            };
            // A no-op waker: these futures are `Pin<Box<dyn Future>>` owned by us
            // and polled in a tight loop, so a wake is only a request to re-poll,
            // which the loop does anyway.
            let waker = Waker::from(Arc::new(NoopWake));
            let mut context = Context::from_waker(&waker);
            loop {
                match future.as_mut().poll(&mut context) {
                    Poll::Ready(()) => break,
                    Poll::Pending => {
                        // A pending future in the sequential queue cannot make
                        // progress without an external event, and the only
                        // external event available is another queued future, so
                        // park it at the back and continue. This is the same
                        // shape as upstream's promise chain.
                        let requeued = future;
                        let more = self.shared.borrow_mut().pop_front();
                        match more {
                            Some(other) => {
                                self.shared.borrow_mut().push_back(requeued);
                                future = other;
                            }
                            None => {
                                // Nothing else to run; the future is waiting on a
                                // real event (a timer, a channel). Put it back so
                                // a later `drive_pending` can resume it.
                                self.shared.borrow_mut().push_back(requeued);
                                return driven;
                            }
                        }
                    }
                }
            }
            driven += 1;
        }
    }

    /// How many futures are queued but not finished.
    pub fn pending(&self) -> usize {
        self.shared.borrow().len()
    }
}

impl Spawner for SequentialSpawner {
    fn spawn(&self, future: BoxFuture<'static, ()>) {
        self.shared.borrow_mut().push_back(future);
    }

    fn spawns_concurrently(&self) -> bool {
        false
    }

    fn yield_now(&self) -> BoxFuture<'static, ()> {
        // Drive whatever is queued *before* resolving: that is what makes
        // "queue the write, await at the end" observe the write, which is the
        // only reason upstream's ordering works.
        self.drive_pending();
        Box::pin(std::future::ready(()))
    }
}

/// A waker that does nothing.
///
/// `Waker` only accepts `Arc<W: Wake + Send + Sync>`, which is why this is an
/// `Arc` and not an `Rc` like the spawner's own state. The spawner's *futures* are
/// not `Send`; the waker is a zero-sized unit struct, so sharing it across threads
/// is harmless and never actually happens.
struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn the_default_spawner_does_not_claim_concurrency() {
        // D3: a test must be able to assert it exercised the sequential path.
        assert!(!SequentialSpawner::new().spawns_concurrently());
    }

    #[test]
    fn queued_work_runs_before_a_yield_resolves() {
        let spawner = SequentialSpawner::new();
        let ran = Rc::new(Cell::new(0usize));

        let counter = Rc::clone(&ran);
        spawner.spawn(Box::pin(async move {
            counter.set(counter.get() + 1);
        }));
        let counter = Rc::clone(&ran);
        spawner.spawn(Box::pin(async move {
            // A future that yields to another queued future first.
            std::future::ready(()).await;
            counter.set(counter.get() + 1);
        }));

        assert_eq!(ran.get(), 0, "nothing runs until it is driven");
        // `yield_now` drives the queue, which is what makes the
        // queue-then-await pattern observable.
        drop(spawner.yield_now());
        assert_eq!(ran.get(), 2);
        assert_eq!(spawner.pending(), 0);
    }

    #[test]
    fn a_pending_future_is_parked_not_lost() {
        let spawner = SequentialSpawner::new();
        let ran = Rc::new(Cell::new(false));
        let moved = Rc::clone(&ran);
        // `pending()` never resolves without an external wake, so it must be
        // parked rather than dropped or spun on.
        spawner.spawn(Box::pin(async move {
            std::future::pending::<()>().await;
            moved.set(true);
        }));
        assert_eq!(spawner.drive_pending(), 0);
        assert_eq!(spawner.pending(), 1, "parked, still owned by the spawner");
        assert!(!ran.get());
    }
}
