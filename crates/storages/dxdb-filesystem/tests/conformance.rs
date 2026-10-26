//! `dxdb-filesystem` against the shared conformance suite.
//!
//! This is the file that answers "does the redb-backed storage actually pass the
//! contract?", which until now it could not: it implemented no `RxStorage` at all, so
//! the matrix reported `not runnable: does not implement RxStorage` for all 48 cases.
//!
//! # Why this is not the matrix file
//!
//! `crates/dxdb-testkit/tests/storage_matrix.rs` is another lane's file and builds its
//! rows behind `matrix-*` features. Rather than edit it, this test drives the same
//! public surface — `decl::exercised_cases()` and `suite::run_exercised_case` — against
//! this crate's storage. Same oracle, same cases, own file, so a failure here is
//! attributable to this storage rather than to a shared table.
//!
//! Filtering matches the matrix: only `implementations.test.ts` cases, which are the
//! storage-contract cases rather than the query-correctness ones.
//!
//! Run:
//! ```text
//! cargo test -p dxdb-filesystem --test conformance -- --nocapture
//! ```

// NOTE: per-case catching, so this reports all failures -- but only under
// `panic = "unwind"`, which cargo test provides. See
// dxdb_testkit::suite::PANIC_STRATEGY. The M5 `.wasm` runner deliberately does NOT
// catch for this reason (panic="abort" in the release profile).

use std::cell::RefCell;
use std::path::PathBuf;

use dxdb_testkit::{decl::exercised_cases, suite};

/// One case's outcome, kept so the report can name every failure at once.
#[derive(Clone, Debug)]
struct Outcome {
    id: &'static str,
    upstream_line: u32,
    title: &'static str,
    failure: Option<String>,
}

thread_local! {
    /// Panic text per upstream line, captured inside the panic hook so a failing case
    /// reports *why* rather than only that it failed.
    static FAILURES: RefCell<Vec<(u32, String)>> = const { RefCell::new(Vec::new()) };
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[test]
fn filesystem_passes_the_shared_conformance_suite() {
    // A fresh directory per run so no case can pass on another's rows.
    let dir: PathBuf =
        std::env::temp_dir().join(format!("dxdb-fs-conformance-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");

    let storage = dxdb_filesystem::FilesystemRxStorage::new(&dir);

    // Install a hook that records the message instead of printing it, and chain the
    // existing hook so a genuine unexpected panic is still visible.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        FAILURES.with(|slot| slot.borrow_mut().push((0, message)));
    }));

    let cases: Vec<&'static dxdb_testkit::Case> = exercised_cases()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .collect();
    let total = cases.len();

    let mut outcomes: Vec<Outcome> = Vec::with_capacity(total);
    for case in &cases {
        FAILURES.with(|slot| slot.borrow_mut().clear());
        let result = futures::FutureExt::catch_unwind(core::panic::AssertUnwindSafe(
            suite::run_exercised_case(&storage, case.upstream_line),
        ));
        let failure = match futures::executor::block_on(result) {
            Ok(()) => None,
            Err(payload) => {
                // Prefer the hook's message, which includes the assertion text.
                let from_hook = FAILURES.with(|slot| {
                    slot.borrow()
                        .iter()
                        .map(|(_, m)| m.clone())
                        .collect::<Vec<_>>()
                        .join(" | ")
                });
                let message = if from_hook.is_empty() {
                    panic_message(&payload)
                } else {
                    from_hook
                };
                Some(message)
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
    let _ = std::fs::remove_dir_all(&dir);

    let failed: Vec<&Outcome> = outcomes.iter().filter(|o| o.failure.is_some()).collect();
    let passed = total - failed.len();

    // Print the row in the matrix's shape, so this output can be compared to
    // `make gate-matrix` directly.
    println!("\n| storage | declared | exercised | passed | failed | status |");
    println!("| --- | ---: | ---: | ---: | ---: | --- |");
    println!(
        "| `filesystem` | {total} | {total} | {passed} | {} | {} |",
        failed.len(),
        if failed.is_empty() {
            "passing"
        } else {
            "failing"
        }
    );
    if !failed.is_empty() {
        println!("\n**`filesystem`** — {} failing case(s):", failed.len());
        for outcome in &failed {
            println!(
                "- `{}` :{} — {}\n  - {}",
                outcome.id,
                outcome.upstream_line,
                outcome.title,
                outcome.failure.as_deref().unwrap_or("")
            );
        }
    }

    assert!(
        failed.is_empty(),
        "{} of {total} conformance cases failed; see the table above",
        failed.len()
    );
    assert_eq!(
        total, 48,
        "the implementation suite should declare 48 cases"
    );
}
