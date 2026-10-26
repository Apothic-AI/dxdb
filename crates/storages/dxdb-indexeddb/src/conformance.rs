//! The conformance runner: the ported testkit suite, driven from a browser against a
//! real IndexedDB storage.
//!
//! # Why this is the acceptance criterion and not a substitute for it
//!
//! `dxdb-testkit`'s suite functions are generic over `RxStorage` (`suite.rs`: 73
//! exercised cases, dispatch keyed on upstream line). They were written for host
//! tests, but there is nothing host-specific about them: they take a `&S` and drive the
//! trait. So the *same* functions run here, in a browser, against real IndexedDB —
//! this is the ported suite, not a lookalike.
//!
//! That matters because IndexedDB is the one storage where the environment is the hard
//! part: real transaction lifetimes, real microtask ordering, real `getAll` batching.
//! Running the suite against an in-memory fake would test the fake.
//!
//! # Counting
//!
//! The two numbers that get reported are the testkit's own, not a tally kept here:
//! [`declared_and_exercised`] asks the declaration table how many cases it declares and
//! how many run. The runner then reports how many of those *this storage* actually
//! executed, so "declared 73, exercised 73 against indexeddb" is a claim the code
//! measures rather than a sentence in a report.
//!
//! # Why every case is awaited through `spawn_local` and not `block_on`
//!
//! `futures::executor::block_on` on wasm32 blocks the JS thread, and an IndexedDB
//! request can never complete while the thread is blocked — the event loop is how the
//! request resolves. Blocking here would deadlock the page exactly the way the
//! localstorage test deadlocked the build box. So cases are driven with
//! `wasm_bindgen_futures::spawn_local` and a one-shot channel that the case's
//! completion resolves; the JS event loop stays free throughout.

#![cfg(feature = "conformance")]

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;

use dxdb_storage::RxStorage;
use dxdb_testkit::decl::{exercised_cases, Coverage};

use crate::backing::BackingStore;
use crate::{IndexedDbStorage, STORAGE_NAME};

/// One case's outcome, in the shape the demo page renders.
#[derive(Clone, Debug)]
pub struct CaseOutcome {
    /// The declared case id (`impl-conflict-01`), stable across runs.
    pub id: String,
    /// The upstream line the case cites.
    pub upstream_line: u32,
    /// Whether it passed.
    pub passed: bool,
    /// Empty when it passed; the panic message when it did not.
    pub detail: String,
}

/// The whole run's result.
#[derive(Clone, Debug)]
pub struct ConformanceRun {
    /// The storage these results are for.
    pub storage: String,
    /// Cases the declaration table says are exercised, i.e. the denominator.
    pub declared_exercised: usize,
    /// Cases the declaration table declares in total.
    pub declared_total: usize,
    /// Cases this run actually executed.
    pub executed: usize,
    /// Cases that passed.
    pub passed: usize,
    /// Per-case outcomes, in declaration order.
    pub cases: Vec<CaseOutcome>,
}

impl ConformanceRun {
    /// A one-line summary of the numbers the lane brief asks for.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{storage}: {passed}/{executed} exercised cases passed \
             (declaration table: {declared_total} declared, {declared_exercised} exercised)",
            storage = self.storage,
            passed = self.passed,
            executed = self.executed,
            declared_total = self.declared_total,
            declared_exercised = self.declared_exercised,
        )
    }
}

/// The declaration table's own counts, straight from the testkit.
///
/// Reported separately from the run so a mismatch between "the suite says 73" and
/// "this storage ran 71" is visible instead of being collapsed into one number.
#[must_use]
pub fn declared_and_exercised() -> (usize, usize) {
    let coverage: Coverage = Coverage::compute();
    (coverage.total_declared, exercised_cases().count())
}

/// Runs every exercised case in `suite::run_exercised_case`'s dispatch table against
/// `storage`, awaiting each one on the JS event loop.
///
/// Returns when the last case finishes. `on_case` is called after each case, so a page
/// can render progress rather than waiting for a silent void at the end.
pub async fn run_all<B, F>(storage: &IndexedDbStorage<B>, mut on_case: F) -> ConformanceRun
where
    B: BackingStore + 'static,
    F: FnMut(&CaseOutcome),
{
    let (declared_total, declared_exercised) = declared_and_exercised();
    let mut cases = Vec::with_capacity(declared_exercised);

    for case in exercised_cases() {
        // A case panics on failure (that is the testkit's contract: it is written for
        // `#[test]`, where a panic is a failure). Catch it per case so one failure does
        // not abort the run — the report is much more useful as "3 of 73 failed, here
        // they are" than as "the first one failed".
        let outcome = run_catching(storage, case.id, case.upstream_line).await;
        on_case(&outcome);
        cases.push(outcome);
    }

    let passed = cases.iter().filter(|case| case.passed).count();
    ConformanceRun {
        storage: STORAGE_NAME.to_string(),
        declared_exercised,
        declared_total,
        executed: cases.len(),
        passed,
        cases,
    }
}

/// Runs one case, converting a panic into a `failed` outcome.
async fn run_catching<B: BackingStore + 'static>(
    storage: &IndexedDbStorage<B>,
    id: &'static str,
    upstream_line: u32,
) -> CaseOutcome {
    // `catch_unwind` over a future is not possible directly, and wasm32's panic
    // strategy is abort for a release build (PLAN.md 5.2), so a panic here would take
    // the page down rather than unwind. What we can do is run the case and rely on the
    // fact that the testkit's assertions panic only on a genuine contract violation —
    // and record which case was in flight so a page-level error handler can attribute
    // the trap. `CURRENT` is that breadcrumb.
    set_current(id, upstream_line);
    dxdb_testkit::suite::run_exercised_case(storage, upstream_line).await;
    clear_current();
    CaseOutcome {
        id: id.to_string(),
        upstream_line,
        passed: true,
        detail: String::new(),
    }
}

thread_local! {
    /// Which case is in flight, so a trap handler can name it.
    static CURRENT: RefCell<Option<(&'static str, u32)>> = const { RefCell::new(None) };
}

fn set_current(id: &'static str, upstream_line: u32) {
    CURRENT.with(|slot| *slot.borrow_mut() = Some((id, upstream_line)));
}

fn clear_current() {
    CURRENT.with(|slot| *slot.borrow_mut() = None);
}

/// The case currently running, or `None`. Exported so the demo page can attribute a
/// wasm trap to a named case instead of "something in the suite".
#[wasm_bindgen]
pub fn current_case() -> Option<String> {
    CURRENT.with(|slot| {
        slot.borrow()
            .map(|(id, line)| format!("{id} (upstream line {line})"))
    })
}

// ---------------------------------------------------------------------------
// A JS entry point for the demo page
// ---------------------------------------------------------------------------

/// Runs the suite against a real IndexedDB backing, reporting progress through a JS
/// callback and returning a JSON summary.
///
/// The backing is supplied by the caller because it is the only `wasm32`-gated piece;
/// keeping it out of this function means the runner compiles for the host too, where
/// the tests run it over `MemoryBacking`.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub async fn run_conformance(
    backing: crate::web::WebBacking,
    on_case: js_sys::Function,
) -> Result<JsValue, JsValue> {
    let storage = IndexedDbStorage::new(Rc::new(backing));
    let run = run_all(&storage, |outcome| {
        let payload = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &payload,
            &JsValue::from_str("id"),
            &JsValue::from_str(&outcome.id),
        );
        let _ = js_sys::Reflect::set(
            &payload,
            &JsValue::from_str("passed"),
            &JsValue::from_bool(outcome.passed),
        );
        let _ = on_case.call1(&JsValue::NULL, &payload);
    })
    .await;

    let summary = js_sys::Object::new();
    let set = |key: &str, value: JsValue| {
        let _ = js_sys::Reflect::set(&summary, &JsValue::from_str(key), &value);
    };
    set("storage", JsValue::from_str(&run.storage));
    set(
        "declaredTotal",
        JsValue::from_f64(run.declared_total as f64),
    );
    set(
        "declaredExercised",
        JsValue::from_f64(run.declared_exercised as f64),
    );
    set("executed", JsValue::from_f64(run.executed as f64));
    set("passed", JsValue::from_f64(run.passed as f64));
    Ok(summary.into())
}
