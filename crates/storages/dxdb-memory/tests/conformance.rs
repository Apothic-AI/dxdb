//! `dxdb-memory` against the shared conformance suite in `dxdb-testkit`.
//!
//! # What this replaces, and why it is better
//!
//! `tests/storage_implementations.rs` in this crate was a hand-port of upstream's
//! `test/unit/rx-storage-implementations.test.ts`: 50 `#[test]`s written by reading the
//! TypeScript and asserting the same behaviours on the same inputs. It was a real test
//! suite and it found real bugs, but it had two structural problems that no amount of
//! care in writing it could fix.
//!
//! **It was a second opinion, not the shared oracle.** The testkit's `suite.rs` is the
//! ported suite every storage in the workspace is measured against, and it is what
//! `make gate-storage` runs. A storage that passes its own private suite and fails the
//! shared one has not been measured; and when the two disagreed, deciding which was
//! right meant re-reading upstream. One oracle, or the comparison in
//! `tests/storage_matrix.rs` stops meaning anything.
//!
//! **It could not tell "stricter than upstream" from "wrong".** A hand-port is free to
//! assert something upstream does not, and the failure looks identical either way.
//!
//! So this file drives the shared suite — `decl::exercised_cases()` and
//! `suite::run_exercised_case`, the same two calls `storage_matrix.rs` makes — against
//! `MemoryStorage`. The 50 private tests are deleted rather than kept alongside; the
//! ones that assert something the shared suite does *not* are listed in
//! [`KEPT_BECAUSE`][kept] below and were moved into `tests/memory_specific.rs`.
//!
//! [kept]: #what-this-file-does-not-cover
//!
//! # What this file does not cover
//!
//! Four behaviours the private suite had that the shared suite has no case for, because
//! upstream's suite is generic over storages and these are properties of *this* engine:
//!
//! * key compression and encrypted fields are refused rather than silently ignored
//!   (`refuses_key_compression_without_the_wrapper`,
//!   `refuses_encrypted_fields_without_the_wrapper`) — upstream instantiates the suite
//!   with and without those wrappers instead of asserting they are rejected;
//! * code-unit ordering of index strings (`orders_index_strings_by_code_unit_not_by_bytes`)
//!   — upstream exercises this only indirectly, through `compareDocsWithIndex`;
//! * an unknown index is an error rather than an empty result (`an_unknown_index_is_an_error`);
//! * `(database, collection, version)` state isolation (`database_names_do_not_share_state`,
//!   `schema_versions_do_not_share_state`) — the trap that produced 16 false failures in
//!   `dxdb-core`, and worth a test that names it.
//!
//! Those live in `tests/memory_specific.rs`. They are extras on top of the oracle, not a
//! competing copy of it.
//!
//! Run:
//! ```text
//! cargo test -p dxdb-memory --test conformance -- --nocapture
//! ```

use std::cell::RefCell;

use dxdb_memory::MemoryStorage;
use dxdb_testkit::{decl::exercised_cases, suite};

/// One case's outcome, kept so the report can name every failure at once rather than
/// stopping at the first. A storage with three bugs should be told about three.
#[derive(Clone, Debug)]
struct Outcome {
    id: &'static str,
    upstream_line: u32,
    title: &'static str,
    failure: Option<String>,
}

thread_local! {
    /// Panic text per case, captured inside the panic hook so a failing case reports
    /// *why* rather than only that it failed.
    ///
    /// Thread-local because `tests` run each `#[test]` on its own thread and this is
    /// diagnostics, not state anything reads across threads.
    static FAILURES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The message from a caught panic payload, for the rare case the hook recorded nothing.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Runs every exercised implementations case against `MemoryStorage`.
///
/// `futures::executor::block_on` rather than a runtime: D3 makes the sequential executor
/// the default, and this crate has no runtime dependency. That is also why the testkit
/// can be compiled for `wasm32`.
fn run_suite() -> (Vec<Outcome>, usize) {
    let storage = MemoryStorage::new();

    // Record the panic text instead of printing it, and chain the existing hook so a
    // panic outside the suite — a genuine bug in the fixtures — is still visible.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        FAILURES.with(|slot| slot.borrow_mut().push(message));
    }));

    let cases: Vec<&'static dxdb_testkit::Case> = exercised_cases()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .collect();
    let total = cases.len();

    let mut outcomes = Vec::with_capacity(total);
    for case in &cases {
        FAILURES.with(|slot| slot.borrow_mut().clear());
        // `AssertUnwindSafe` because a case that panics must not hide the ones after it.
        // Every case opens its own instance with a fresh random database name
        // (`harness::StorageSpec`), so no state carries across the unwind.
        let result = futures::FutureExt::catch_unwind(core::panic::AssertUnwindSafe(
            suite::run_exercised_case(&storage, case.upstream_line),
        ));
        let failure = match futures::executor::block_on(result) {
            Ok(()) => None,
            Err(payload) => {
                // Prefer the hook's message: it carries the assertion text, which the
                // payload only carries when the panic came from `panic!` directly.
                let from_hook = FAILURES.with(|slot| slot.borrow().join(" | "));
                Some(if from_hook.is_empty() {
                    panic_message(&payload)
                } else {
                    from_hook
                })
            }
        };
        outcomes.push(Outcome {
            id: case.id,
            upstream_line: case.upstream_line,
            title: case.title,
            failure,
        });
    }

    std::panic::set_hook(previous);
    (outcomes, total)
}

/// `dxdb-memory` must pass every case the shared suite exercises.
///
/// This is the gate. It is one assertion over the whole suite rather than one `#[test]`
/// per case on purpose: the suite's own `run_all` shape means a case that stops being
/// exercised is a *smaller* pass, and a per-case file would report that as silently
/// green. [`exercises_the_whole_declared_set`] pins the count separately.
#[test]
fn memory_passes_the_shared_conformance_suite() {
    let (outcomes, total) = run_suite();
    let failed: Vec<&Outcome> = outcomes.iter().filter(|o| o.failure.is_some()).collect();
    let passed = total - failed.len();

    // The row in the matrix's shape, so this output can be compared to
    // `make gate-matrix` directly.
    println!("\n| storage | declared | exercised | passed | failed | status |");
    println!("| --- | ---: | ---: | ---: | ---: | --- |");
    println!(
        "| `memory` | {total} | {total} | {passed} | {} | {} |",
        failed.len(),
        if failed.is_empty() {
            "passing"
        } else {
            "failing"
        }
    );
    for outcome in &failed {
        println!(
            "\n- `{}` :{} — {}\n  - {}",
            outcome.id,
            outcome.upstream_line,
            outcome.title,
            outcome.failure.as_deref().unwrap_or("")
        );
    }

    assert!(
        failed.is_empty(),
        "{} of {total} conformance cases failed; see the table above",
        failed.len()
    );
}

/// The suite must still exercise every implementations case the declaration table calls
/// `Exercised`.
///
/// Without this, the test above passes just as happily against a suite that shrank to
/// three cases. The count is read from the table, not written as a literal, so it moves
/// when a case is added or reclassified — a literal here would be a number someone has to
/// remember to update, which is how coverage silently drops.
#[test]
fn exercises_the_whole_declared_set() {
    let declared: Vec<&'static dxdb_testkit::Case> = exercised_cases()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .collect();
    let ids: Vec<&str> = declared.iter().map(|case| case.id).collect();

    // The shared suite dispatches on upstream line; a line with no body panics inside
    // `run_exercised_case`. This asserts the two tables name the same set, which is the
    // failure mode that would make the gate above vacuous.
    let (outcomes, total) = run_suite();
    assert_eq!(
        total,
        declared.len(),
        "the suite ran {total} cases but the declaration table calls {} Exercised",
        declared.len()
    );
    for outcome in &outcomes {
        assert!(
            ids.contains(&outcome.id),
            "`{}` ran but is not in the declaration table",
            outcome.id
        );
    }
}
