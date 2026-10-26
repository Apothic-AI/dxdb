//! The real conformance suite, driven from a `.wasm` on any target.
//!
//! # What changed from the earlier version
//!
//! An earlier version of this crate carried a hand-written 12-case suite. It proved the
//! *target* worked (a component with a preopen could open redb and round-trip a
//! document) but it was not the conformance suite: M5's gate says "the conformance
//! suite runs under Wasmtime … and under Node", and the conformance suite is the ported
//! upstream contract in `dxdb-testkit`.
//!
//! This module runs **that** suite — the same `exercised_cases()` the storage matrix
//! uses, filtered identically to `implementations.test.ts` — against
//! `dxdb-filesystem`'s `RxStorage` implementation. So the case count is comparable to
//! `make gate-matrix` by construction rather than by assertion.
//!
//! # Why panics are not caught here
//!
//! The matrix runner wraps each case in `catch_unwind` to report all failures at once.
//! This runner deliberately does not, for two reasons:
//!
//! 1. The workspace release profile sets `panic = "abort"` (PLAN.md 5.3), and
//!    `catch_unwind` cannot catch an abort. Code that appeared to catch on native dev
//!    builds would silently stop catching in the artifact the gate actually runs.
//! 2. A gate wants a nonzero exit, and an abort gives one on every target: Wasmtime
//!    traps, `node:wasi` throws, and the native CLI aborts.
//!
//! The cost is that only the first failing case is reported per run. That is recovered
//! by printing `RUN <id>` before each case and flushing, so the last line before the
//! trap names the case that failed. `--rerun <id>` runs one case alone, which is how a
//! failure is then investigated.

use std::path::Path;

use dxdb_filesystem::FilesystemRxStorage;
use dxdb_testkit::decl::exercised_cases;

/// A case the suite will run.
#[derive(Clone, Debug)]
pub struct PlannedCase {
    /// Stable id, e.g. `impl-bulkwrite-07`.
    pub id: &'static str,
    /// Upstream line, which is what the suite dispatches on.
    pub upstream_line: u32,
    /// Upstream title.
    pub title: &'static str,
}

/// The cases this runner executes: the `implementations.test.ts` group, matching the
/// storage matrix's filter exactly so case counts are comparable.
pub fn planned_cases() -> Vec<PlannedCase> {
    exercised_cases()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .map(|case| PlannedCase {
            id: case.id,
            upstream_line: case.upstream_line,
            title: case.title,
        })
        .collect()
}

/// What happened when the suite ran.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// Cases that completed.
    pub passed: Vec<&'static str>,
    /// Cases skipped because a filter excluded them.
    pub skipped: Vec<&'static str>,
}

impl Outcome {
    /// Exit code for the process: 1 when nothing ran, which is how a mistyped filter is
    /// prevented from looking like a pass.
    pub fn exit_code(&self) -> i32 {
        if self.passed.is_empty() {
            1
        } else {
            0
        }
    }

    /// A one-line summary in the shape the matrix prints.
    pub fn summary(&self, total: usize) -> String {
        format!(
            "{}/{} passed, 0 failed, {} skipped",
            self.passed.len(),
            total,
            self.skipped.len()
        )
    }
}

/// Runs the suite against a `dxdb-filesystem` storage rooted at `base_dir`.
///
/// `base_dir` must be a **guest-side** path on WASI. The caller (the CLI) has already
/// refused anything that does not resolve, so a host path cannot reach here.
///
/// `filter` matches case ids by substring. A filter that matches nothing yields an empty
/// `passed`, which [`Outcome::exit_code`] treats as failure.
///
/// Each case runs in its own subdirectory, so a case cannot pass on another's rows.
pub fn run(base_dir: &Path, filter: Option<&str>) -> Outcome {
    let mut outcome = Outcome::default();

    for case in planned_cases() {
        if let Some(needle) = filter {
            if !case.id.contains(needle) {
                outcome.skipped.push(case.id);
                continue;
            }
        }

        // Printed *before* the case runs and flushed, so an abort still names the case
        // that failed. Without this a trap would report nothing but a stack trace.
        println!("RUN {} :: {}", case.id, case.title);
        use std::io::Write as _;
        let _ = std::io::stdout().flush();

        let case_dir = base_dir.join(case.id);
        let _ = std::fs::remove_dir_all(&case_dir);
        std::fs::create_dir_all(&case_dir).unwrap_or_else(|e| {
            panic!("cannot create {} for {}: {e}", case_dir.display(), case.id)
        });

        let storage = FilesystemRxStorage::new(&case_dir);
        // The suite's cases are async and this runner is synchronous on every target;
        // a single-threaded block_on is what the matrix does too.
        futures::executor::block_on(dxdb_testkit::suite::run_exercised_case(
            &storage,
            case.upstream_line,
        ));

        // Free the redb handle before the next case reuses nothing but its own dir; on
        // WASI a lingering handle would keep the file locked.
        drop(storage);
        let _ = std::fs::remove_dir_all(&case_dir);

        println!("PASS {}", case.id);
        outcome.passed.push(case.id);
    }

    outcome
}

/// Asserts that the preopened directory is usable, before any case runs.
///
/// This is the host-path-vs-guest-path trap made impossible to get wrong. A WASI guest
/// can see only what `--dir HOST::GUEST` maps in, so a caller who passes the *host* path
/// gets `ENOENT` from a directory that demonstrably exists. Rather than let that surface
/// as a confusing first-case failure, the runner proves the directory is writable by
/// creating and deleting a sentinel file, and refuses to start otherwise.
///
/// Returns the reason on failure so the CLI can print it.
pub fn assert_preopen_usable(base_dir: &Path) -> Result<(), String> {
    if !base_dir.exists() {
        return Err(format!(
            "{} does not exist. On WASI this must be the guest-side path of a preopened \
             directory: map it with `wasmtime run --dir <host>::<guest>` and pass <guest> \
             here. A host path will not resolve even when it exists on the host.",
            base_dir.display()
        ));
    }
    if !base_dir.is_dir() {
        return Err(format!("{} is not a directory", base_dir.display()));
    }
    let sentinel = base_dir.join(".dxdb-preopen-check");
    match std::fs::write(&sentinel, b"ok") {
        Ok(()) => {}
        Err(e) => {
            return Err(format!(
                "{} is not writable ({e}). A preopen granted read-only cannot host a \
                 storage; map it read-write.",
                base_dir.display()
            ))
        }
    }
    match std::fs::read(&sentinel) {
        Ok(bytes) if bytes == b"ok" => {}
        Ok(_) => {
            return Err(format!(
                "{} wrote but read back different bytes",
                base_dir.display()
            ))
        }
        Err(e) => {
            return Err(format!(
                "{} wrote but could not be read back ({e}); the preopen is not a \
                 persistent directory",
                base_dir.display()
            ))
        }
    }
    let _ = std::fs::remove_file(&sentinel);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_planned_cases_are_the_implementation_suite() {
        let cases = planned_cases();
        assert_eq!(cases.len(), 48, "the implementation suite is 48 cases");
        assert!(cases
            .iter()
            .all(|c| c.id.starts_with("impl-") || c.id.starts_with("qc-")));
        // The count is the thing the gate reports, so it is asserted rather than printed.
    }

    #[test]
    fn case_ids_are_unique_and_lines_distinct() {
        let cases = planned_cases();
        let ids: std::collections::HashSet<_> = cases.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), cases.len(), "duplicate case ids");
        let lines: std::collections::HashSet<_> = cases.iter().map(|c| c.upstream_line).collect();
        assert_eq!(
            lines.len(),
            cases.len(),
            "two cases dispatch on one upstream line"
        );
    }

    #[test]
    fn an_empty_run_is_not_a_pass() {
        let outcome = Outcome::default();
        assert_eq!(outcome.exit_code(), 1, "no cases run must not exit 0");
    }

    #[test]
    fn a_missing_directory_is_refused_with_the_guest_path_explanation() {
        let missing = std::env::temp_dir().join("dxdb-does-not-exist-here");
        let _ = std::fs::remove_dir_all(&missing);
        let err = assert_preopen_usable(&missing).expect_err("must refuse");
        assert!(err.contains("--dir"), "must name the mapping: {err}");
        assert!(err.contains("<guest>"), "must name the guest path: {err}");
    }

    #[test]
    fn a_writable_directory_is_accepted() {
        let dir = std::env::temp_dir().join(format!("dxdb-preopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert_preopen_usable(&dir).expect("a writable dir must pass");
        // And the sentinel must not be left behind.
        assert!(!dir.join(".dxdb-preopen-check").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
