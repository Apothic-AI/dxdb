//! The hang watchdog, by the same pattern used for `dxdb-localstorage`.
//!
//! A storage conformance test that can hang is worse than one that fails: it holds the
//! fleet-wide build lock, and from outside it looks like an idle box rather than a bug.
//! That happened once already (`a_failed_write_emits_nothing`, awaiting a change-stream
//! event that was correctly never emitted). This is the cure, applied pre-emptively.
//!
//! A thread plus `process::abort`, deliberately not `FutureExt::timeout`: a timer future
//! needs a reactor, this crate has no runtime by policy, and a timeout driven by the
//! executor cannot fire when the executor is the blocked thing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long any single case may take. These are in-memory operations, so anything near
/// this is a deadlock rather than slowness.
const DEADLINE: Duration = Duration::from_secs(20);

/// Drives `future` to completion, aborting the process if it exceeds [`DEADLINE`].
pub fn within<F: std::future::Future>(label: &str, future: F) -> F::Output {
    let finished = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finished);
    let label = label.to_string();
    std::thread::spawn(move || {
        let steps = DEADLINE.as_millis() / 25;
        for _ in 0..steps {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!(
            "\nHANG: `{label}` did not complete within {DEADLINE:?}.\n\
             A storage conformance case blocked forever. This is a deadlock, not slow \
             code: nothing is computing. Usual causes are a channel whose sender was \
             dropped without closing, a lock held across an await, or awaiting an event \
             the write path never emits.\n\
             Aborting so the build lock is released and the failure has a name.\n"
        );
        std::process::abort();
    });
    let output = futures::executor::block_on(future);
    finished.store(true, Ordering::SeqCst);
    output
}
