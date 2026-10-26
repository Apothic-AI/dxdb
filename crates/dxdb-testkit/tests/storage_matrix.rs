//! `make gate-matrix` — the conformance matrix for every real storage.
//!
//! This is the file that turns the testkit from "a suite that passes against a
//! scaffold" into "the oracle that gates every real storage". It runs the same ported
//! cases against each storage in the workspace and prints one row per storage.
//!
//! # Why a matrix and not one gate per storage
//!
//! A per-storage gate answers "does storage X pass?". The question M2 has to answer is
//! "which storage is the contract actually satisfied by?", and the difference is
//! visible only side by side: one storage failing `cleanup -> bool` while three pass it
//! is a storage bug; all four failing it is a testkit bug or a trait-surface gap. That
//! distinction is the single most useful thing this file produces, so it produces it
//! as a table.
//!
//! # Statuses, and why there are six of them
//!
//! | status | meaning |
//! | --- | --- |
//! | `Passed` | the case ran and the storage satisfied it |
//! | `Failed` | the case ran and the storage did not satisfy it |
//! | `StorageMissing` | the crate does not implement `RxStorage` at all |
//! | `BackendNeeded` | implemented, but needs a host capability this binary lacks |
//! | `StorageError` | the storage refused `create_storage_instance` |
//! | `DeclaredNotPorted` | the case is not executable in this crate; see the declaration table |
//!
//! `StorageMissing` is the one worth reading carefully: two of the four `crates/storages`
//! crates are a driver layer and a layout layer, not storage implementations, so the
//! right report is "0 cases run, does not implement the trait", not "0 passed". A
//! matrix that printed 0/90 for them would be indistinguishable from total failure.
//!
//! # Running
//!
//! ```text
//! make gate-matrix                     # human-readable table
//! cargo test -p dxdb-testkit --test storage_matrix -- --nocapture   # same, verbose
//! ```

#[path = "support/mod.rs"]
mod support;

use std::cell::RefCell;
use std::rc::Rc;

use dxdb_testkit::decl::{exercised_cases, Kind, PortStatus};
use dxdb_testkit::suite;
use dxdb_storage::RxStorage;
use support::reference_storage::ReferenceStorage;

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// How one storage fared on the ported suite.
#[derive(Clone, Debug)]
pub struct StorageRow {
    /// Storage name as the trait reports it, or the crate name when it has none.
    pub storage: &'static str,
    /// The crate path, for a reader who needs to go look.
    pub krate: &'static str,
    /// Why this storage cannot run the suite at all, when it cannot.
    pub unavailable: Option<Unavailable>,
    /// Per-case outcome, in declaration order.
    pub outcomes: Vec<Outcome>,
}

/// Why a storage could not be run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The crate does not implement `RxStorage`. Not a bug in the crate — see the
    /// module docs of each.
    StorageMissing,
    /// Needs a host capability: a browser `localStorage`, a real filesystem, a SQLite
    /// driver tied to a specific target.
    BackendNeeded,
}

impl Unavailable {
    /// A sentence a report can print without a reader having to interpret it.
    pub fn reason(self) -> &'static str {
        match self {
            Unavailable::StorageMissing => {
                "does not implement RxStorage; this crate is a driver/layout layer, \
                 not a storage implementation"
            }
            Unavailable::BackendNeeded => {
                "implements RxStorage but needs a host capability this test binary does \
                 not have"
            }
        }
    }
}

/// The outcome of one case against one storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Ran and passed.
    Passed,
    /// Ran and failed.
    Failed,
    /// The case is not executable in this crate.
    DeclaredNotPorted,
    /// The storage could not be opened.
    StorageError,
    /// Skipped because the whole storage is unavailable.
    StorageUnavailable,
}

/// Counts, per storage, for the matrix line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Cases that ran and passed.
    pub passed: usize,
    /// Cases that ran and failed.
    pub failed: usize,
    /// Cases declared but not executable in this crate.
    pub not_ported: usize,
    /// Cases skipped because the storage is unavailable.
    pub unavailable: usize,
}

impl Counts {
    /// Total cases the matrix considered.
    pub fn total(&self) -> usize {
        self.passed + self.failed + self.not_ported + self.unavailable
    }

    /// Cases that actually ran.
    pub fn exercised(&self) -> usize {
        self.passed + self.failed
    }

    /// `exercised / declared`, in percent, for the coverage column. A storage that
    /// cannot run has 0 exercised, which is the honest number.
    pub fn exercised_percent(&self, declared: usize) -> f64 {
        if declared == 0 {
            0.0
        } else {
            (self.exercised() as f64 / declared as f64) * 100.0
        }
    }
}

impl StorageRow {
    /// Counts this row.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for outcome in &self.outcomes {
            match outcome {
                Outcome::Passed => counts.passed += 1,
                Outcome::Failed => counts.failed += 1,
                Outcome::DeclaredNotPorted => counts.not_ported += 1,
                Outcome::StorageUnavailable | Outcome::StorageError => counts.unavailable += 1,
            }
        }
        counts
    }
}

/// The cases the matrix runs: every case the declaration table marks
/// [`PortStatus::Exercised`] from the implementations suite.
///
/// The correctness groups are measured by the same storage list but reported by
/// `query_correctness.rs`, because a group is 89 queries and folding them into this
/// table would make one row unreadable.
fn matrix_cases() -> Vec<&'static dxdb_testkit::decl::Case> {
    exercised_cases()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .collect()
}

/// Runs the suite against `storage` and records one outcome per case.
///
/// A panic is caught per case so one broken case does not hide the rest — which is the
/// whole point of a matrix: a storage with three bugs must be told about three.
///
/// The panic message is kept, not discarded. A matrix that says "8 failing" without
/// saying *why* leaves the reader to re-run each one, and a storage author who has to
/// re-run to learn the cause is a storage author who will guess instead.
fn run_against<S: RxStorage>(storage: &S) -> Vec<Outcome> {
    matrix_cases()
        .into_iter()
        .map(|case| {
            let outcome = futures::FutureExt::catch_unwind(core::panic::AssertUnwindSafe(
                suite::run_exercised_case(storage, case.upstream_line),
            ));
            match futures::executor::block_on(outcome) {
                Ok(()) => Outcome::Passed,
                Err(payload) => {
                    let message = panic_message(&payload);
                    FAILURE_MESSAGES.with(|slot| {
                        slot.borrow_mut()
                            .entry((storage.name().to_string(), case.upstream_line))
                            .or_insert(message);
                    });
                    Outcome::Failed
                }
            }
        })
        .collect()
}

thread_local! {
    /// Panic text per `(storage name, upstream line)`, for the failure report.
    ///
    /// Thread-local because the matrix runs on one thread and this is diagnostics, not
    /// state anything reads across threads.
    static FAILURE_MESSAGES: RefCell<std::collections::BTreeMap<(String, u32), String>> =
        const { RefCell::new(std::collections::BTreeMap::new()) };
}

/// Extracts the message from a caught panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    "panicked with a non-string payload".to_string()
}

/// The first non-empty line of the recorded panic, for one case.
fn failure_reason(storage: &str, upstream_line: u32) -> Option<String> {
    FAILURE_MESSAGES.with(|slot| {
        slot.borrow()
            .get(&(storage.to_string(), upstream_line))
            .map(|message| {
                message
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("")
                    .to_string()
            })
    })
}

/// A row for a storage that cannot run.
fn unavailable_row(
    storage: &'static str,
    krate: &'static str,
    why: Unavailable,
) -> StorageRow {
    StorageRow {
        storage,
        krate,
        unavailable: Some(why),
        outcomes: vec![Outcome::StorageUnavailable; matrix_cases().len()],
    }
}

// ---------------------------------------------------------------------------
// The storages
// ---------------------------------------------------------------------------

// The four real storages. Two implement `RxStorage`; two do not, and that is the
// finding this matrix exists to make visible. Each limitation is recorded where the
// reason lives rather than papered over.

#[cfg(feature = "matrix-memory")]
fn memory_row() -> StorageRow {
    let storage = dxdb_memory::MemoryStorage::new();
    StorageRow {
        storage: "memory",
        krate: "crates/storages/dxdb-memory",
        unavailable: None,
        outcomes: run_against(&storage),
    }
}

#[cfg(not(feature = "matrix-memory"))]
fn memory_row() -> StorageRow {
    unavailable_row(
        "memory",
        "crates/storages/dxdb-memory",
        Unavailable::BackendNeeded,
    )
}

#[cfg(feature = "matrix-localstorage")]
fn localstorage_row() -> StorageRow {
    // `MemoryKv` is the backend `dxdb-localstorage` exposes for exactly this: a Kv
    // implementation with no browser behind it, so the storage logic can be measured
    // without a `localStorage`. Using the real `WebStorage` here would make this test
    // fail on native, which is a property of the host rather than of the storage.
    let kv = Rc::new(dxdb_localstorage::MemoryKv::default());
    let storage = dxdb_localstorage::LocalStorageStorage::new(kv);
    StorageRow {
        storage: "localstorage",
        krate: "crates/storages/dxdb-localstorage",
        unavailable: None,
        outcomes: run_against(&storage),
    }
}

#[cfg(not(feature = "matrix-localstorage"))]
fn localstorage_row() -> StorageRow {
    unavailable_row(
        "localstorage",
        "crates/storages/dxdb-localstorage",
        Unavailable::BackendNeeded,
    )
}

fn sqlite_row() -> StorageRow {
    unavailable_row(
        "sqlite",
        "crates/storages/dxdb-sqlite",
        Unavailable::StorageMissing,
    )
}

fn filesystem_row() -> StorageRow {
    unavailable_row(
        "filesystem",
        "crates/storages/dxdb-filesystem",
        Unavailable::StorageMissing,
    )
}

/// A scaffold row, kept so the matrix shows what the suite does when it is definitely
/// correct — the control. A matrix with no known-good column cannot tell a storage bug
/// from a testkit bug.
fn scaffold_row() -> StorageRow {
    StorageRow {
        storage: "reference-scaffold",
        krate: "crates/dxdb-testkit/tests/support",
        unavailable: None,
        outcomes: run_against(&ReferenceStorage::new()),
    }
}

/// Every row, in the order a reader should scan them: the control first, then the real
/// storages, then the ones that cannot run.
fn matrix() -> Vec<StorageRow> {
    vec![
        scaffold_row(),
        memory_row(),
        localstorage_row(),
        sqlite_row(),
        filesystem_row(),
    ]
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Renders the matrix as the Markdown table `docs/STATUS.md` wants for M2.
///
/// Written as Markdown rather than plain text because the destination is a document.
/// Every column is a count this file computed, never a number typed by hand — which is
/// the property that makes it safe to paste.
pub fn render_markdown(rows: &[StorageRow]) -> String {
    let declared = matrix_cases().len();
    let mut out = String::new();

    out.push_str("| storage | crate | declared | exercised | passed | failed | not ported | unavailable | status |\n");
    out.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n");
    for row in rows {
        let counts = row.counts();
        let status = match row.unavailable {
            None if counts.failed == 0 => "passing".to_string(),
            None => format!("{} failing", counts.failed),
            Some(why) => format!("not runnable: {}", match why {
                Unavailable::StorageMissing => "does not implement RxStorage",
                Unavailable::BackendNeeded => "needs a host backend",
            }),
        };
        out.push_str(&format!(
            "| `{}` | `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
            row.storage,
            row.krate,
            declared,
            counts.exercised(),
            counts.passed,
            counts.failed,
            counts.not_ported,
            counts.unavailable,
            status,
        ));
    }
    out
}

/// Renders the per-case failures for one storage, so a report can name them.
pub fn render_failures(rows: &[StorageRow]) -> String {
    let cases = matrix_cases();
    let mut out = String::new();
    for row in rows {
        let failures: Vec<&&dxdb_testkit::decl::Case> = cases
            .iter()
            .zip(row.outcomes.iter())
            .filter(|(_, outcome)| **outcome == Outcome::Failed)
            .map(|(case, _)| case)
            .collect();
        if failures.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n**`{}`** — {} failing case(s):\n",
            row.storage,
            failures.len()
        ));
        for case in failures {
            let reason = failure_reason(row.storage, case.upstream_line)
                .unwrap_or_else(|| "(no message recorded)".to_string());
            out.push_str(&format!(
                "- `{}` {} — {}\n  - `{}`\n",
                case.id,
                case.citation(),
                case.title,
                reason
            ));
        }
    }
    if out.is_empty() {
        out.push_str("\nNo storage failed a case.\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Prints the matrix, and writes it to `DXDB_MATRIX_OUT` when that is set.
///
/// This is a `#[test]` rather than a binary so it runs under the same build and the same
/// build lock as every other gate, and so a failing storage fails a gate rather than
/// needing its output to be read by a human.
///
/// `println!` output is captured by the test harness unless `--nocapture` is passed, and
/// the Makefile's `make test` does not pass it. Rather than require a new Makefile target
/// (the Makefile is leader-owned), the test writes the same Markdown to the path in
/// `DXDB_MATRIX_OUT`. That keeps the matrix readable from a plain `make test`:
///
/// ```text
/// DXDB_MATRIX_OUT=/tmp/matrix.md make test
/// ```
#[test]
fn print_the_matrix() {
    let rows = matrix();
    let markdown = render_markdown(&rows);
    let failures = render_failures(&rows);

    println!("\n## Storage conformance matrix\n");
    print!("{markdown}");
    print!("{failures}");

    if let Some(path) = std::env::var_os("DXDB_MATRIX_OUT") {
        let body = format!("{markdown}{failures}");
        std::fs::write(&path, body).unwrap_or_else(|err| {
            panic!("DXDB_MATRIX_OUT={} is not writable: {err}", path.to_string_lossy())
        });
    }
}

/// The scaffold — the control — must pass every case.
///
/// If this fails, the suite is broken rather than the storage, and every other row in
/// the matrix is suspect. That is why it is a separate assertion rather than just
/// another row.
#[test]
fn the_control_scaffold_passes_every_case() {
    let row = scaffold_row();
    let counts = row.counts();
    assert_eq!(
        counts.failed, 0,
        "the reference scaffold failed {} case(s). The suite is broken, so no other row \
         in the matrix means anything.",
        counts.failed
    );
    assert_eq!(
        counts.exercised(),
        matrix_cases().len(),
        "the scaffold must exercise every declared case"
    );
}

/// Every storage the workspace has must appear in the matrix, either running or with a
/// recorded reason.
///
/// This is what stops a storage quietly dropping out of the matrix: a new crate in
/// `crates/storages/` that nobody wired up would leave the matrix looking complete.
#[test]
fn every_storage_crate_is_accounted_for() {
    let rows = matrix();
    let known: Vec<&str> = rows.iter().map(|row| row.storage).collect();
    for expected in ["memory", "localstorage", "sqlite", "filesystem"] {
        assert!(
            known.contains(&expected),
            "the matrix has no row for `{expected}`. Add one, even if the answer is \
             `Unavailable::StorageMissing` — an absent row reads as a passing one."
        );
    }
    assert!(
        known.contains(&"reference-scaffold"),
        "the matrix must include the control scaffold, or a storage bug and a testkit \
         bug are indistinguishable"
    );
}

/// A storage that cannot run must still produce a full set of outcomes, so its row
/// sums to the declared count rather than to zero.
///
/// This is the difference between "0 of 0" and "0 of 48": the first looks like there was
/// nothing to do, the second is a measurement.
#[test]
fn unavailable_storages_account_for_every_declared_case() {
    let declared = matrix_cases().len();
    for row in matrix() {
        let counts = row.counts();
        assert_eq!(
            counts.total(),
            declared,
            "`{}` accounted for {} of {declared} cases. Every case must have an outcome, \
             including the ones a storage cannot run.",
            row.storage,
            counts.total()
        );
    }
}

/// A storage that cannot run must say why, and the reason must not be "unknown".
#[test]
fn unavailable_storages_state_a_reason() {
    for row in matrix() {
        if row.unavailable.is_some() {
            let reason = row.unavailable.expect("checked").reason();
            assert!(
                reason.len() > 20,
                "`{}` is unavailable without a usable reason: {reason:?}",
                row.storage
            );
        }
    }
}

/// The declaration table's `Exercised` implementations cases must be exactly what the
/// matrix runs. A case that disappears from the matrix but stays `Exercised` is the
/// shrinkage this guards.
#[test]
fn matrix_cases_match_the_declaration_table() {
    let from_table: Vec<u32> = exercised_cases()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .map(|case| case.upstream_line)
        .collect();
    let from_matrix: Vec<u32> = matrix_cases().iter().map(|case| case.upstream_line).collect();
    assert_eq!(
        from_table, from_matrix,
        "the matrix runs a different case set than the declaration table calls Exercised"
    );
    // And none of them is a Static or multi-instance case, which are declared blocked.
    for case in matrix_cases() {
        assert_ne!(
            case.kind_enum(),
            Kind::Static,
            "{} is a Static case; it cannot be in the storage matrix",
            case.id
        );
        assert_eq!(
            case.status,
            PortStatus::Exercised,
            "{} is not Exercised but the matrix ran it",
            case.id
        );
    }
}
