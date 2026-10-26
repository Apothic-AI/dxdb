//! The conformance runner, **in the browser**.
//!
//! # Why this is the M4 gate rather than a convenience
//!
//! PLAN.md M4's exit condition is "a demo app, built from the repo, that runs the same
//! conformance suite in a browser via Playwright, on Chromium, Firefox, and WebKit."
//! The suites live in `dxdb-testkit` and are generic over `RxStorage`, so the only
//! question was whether that crate could be compiled *into* a browser build.
//!
//! It can: `cargo check --manifest-path crates/dxdb-testkit/Cargo.toml --target
//! wasm32-unknown-unknown --no-default-features` succeeds with no host-only
//! dependencies. That is the finding this module exists to exploit — no JS shim, no
//! wasm-bindgen-test, no reimplementation of the suite. The same functions that
//! `make gate-storage` runs on the host are compiled to wasm and run here, against a
//! real browser storage.
//!
//! # Driving an async suite from JS
//!
//! Each case is an `async fn`. `wasm_bindgen_futures::future_to_promise` turns the whole
//! run into one `Promise`, so JS awaits a single call and gets a JSON report. That is
//! also why nothing here uses `futures::executor::block_on`: blocking the JS thread
//! would prevent the OPFS and IndexedDB operations the cases are awaiting from ever
//! completing — the deadlock that already cost this fleet an hour once.
//!
//! # Which path was exercised
//!
//! `path` in the report is the backing's own answer ([`AccessPath::as_str`]), not a
//! guess made here. A run against the main-thread fallback says so, and cannot be
//! mistaken for a run against the worker fast path.

use std::rc::Rc;

use wasm_bindgen::prelude::*;

use dxdb_storage::RxStorage;
use dxdb_testkit::decl::exercised_cases;

use crate::backing::FileStore;
use crate::OpfsStorage;

/// One case's outcome.
#[derive(Clone, Debug)]
struct Outcome {
    id: String,
    passed: bool,
    detail: String,
}

/// Runs every exercised case in the testkit's dispatch table against `storage`.
///
/// `on_case` is called after each case so a page can stream progress. Panics cannot be
/// caught on wasm32 with `panic = "abort"` (PLAN.md 5.2), so a failing assertion unwinds
/// the whole run; the caller reports the last `on_case` id as the case that trapped.
async fn run_all<S, F>(storage: &S, mut on_case: F) -> Vec<Outcome>
where
    S: RxStorage,
    F: FnMut(&Outcome),
{
    let mut outcomes = Vec::new();
    for case in exercised_cases() {
        dxdb_testkit::suite::run_exercised_case(storage, case.upstream_line).await;
        let outcome = Outcome {
            id: case.id.to_string(),
            passed: true,
            detail: String::new(),
        };
        on_case(&outcome);
        outcomes.push(outcome);
    }
    outcomes
}

fn declared_exercised() -> usize {
    exercised_cases().count()
}

/// Populates a JS object with the run's shape.
fn report(
    storage: &str,
    path: &str,
    declared: usize,
    executed: usize,
    passed: usize,
) -> js_sys::Object {
    let summary = js_sys::Object::new();
    let set = |key: &str, value: JsValue| {
        let _ = js_sys::Reflect::set(&summary, &JsValue::from_str(key), &value);
    };
    set("storage", JsValue::from_str(storage));
    set("path", JsValue::from_str(path));
    set("declaredExercised", JsValue::from_f64(declared as f64));
    set("executed", JsValue::from_f64(executed as f64));
    set("passed", JsValue::from_f64(passed as f64));
    summary
}

// ---------------------------------------------------------------------------
// The JS entry points, one per browser storage
// ---------------------------------------------------------------------------

/// Runs the ported suite against **OPFS**, over whichever path this environment offers.
///
/// Returns a Promise resolving to `{ storage, path, declaredExercised, executed, passed }`.
#[wasm_bindgen]
pub fn run_conformance_opfs(on_case: js_sys::Function) -> js_sys::Promise {
    wasm_bindgen_futures::future_to_promise(async move {
        let store = crate::web::WebFiles::open()
            .await
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let path = store.path();
        let storage = OpfsStorage::new(Rc::new(store));
        let declared = declared_exercised();
        let outcomes = run_all(&storage, |outcome| {
            report_case(&on_case, &outcome.id, outcome.passed);
        })
        .await;
        let passed = outcomes.iter().filter(|outcome| outcome.passed).count();
        Ok(report(
            crate::STORAGE_NAME,
            path.as_str(),
            declared,
            outcomes.len(),
            passed,
        )
        .into())
    })
}

/// The path OPFS would use here, without opening anything.
///
/// A caller can decide to start a worker *before* creating a database, which is the
/// difference between "the fallback is explicit" and "the fallback is discovered later".
#[wasm_bindgen]
pub fn opfs_path() -> String {
    crate::web::detect_path().as_str().to_string()
}

fn report_case(on_case: &js_sys::Function, id: &str, passed: bool) {
    let payload = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&payload, &JsValue::from_str("id"), &JsValue::from_str(id));
    let _ = js_sys::Reflect::set(
        &payload,
        &JsValue::from_str("passed"),
        &JsValue::from_bool(passed),
    );
    let _ = on_case.call1(&JsValue::NULL, &payload);
}

/// Exposed so a test can assert the backing is the one it thinks it is.
#[wasm_bindgen]
pub fn opfs_declared_cases() -> usize {
    declared_exercised()
}

/// Compile-time proof that `FileStore` is object-safe enough for the runner's use.
///
/// Not called; its presence means a change to the trait that would break the runner is a
/// build error rather than a runtime surprise.
#[allow(dead_code)]
fn _assert_store_is_usable<S: FileStore>(_store: &S) {}
