//! The napi surface: the conformance suite callable from Node synchronously.
//!
//! # Why this shape
//!
//! The M5 gate is the conformance suite running "under Node from the napi build".
//! The binding therefore exposes exactly [the suite], not a database API: the
//! point of this target is to prove the storage works when the host is Node's
//! native addon loader, not to design the final `dxdb-node` surface. Designing
//! that surface belongs with `dxdb-core`, which is not published yet.
//!
//! Everything is synchronous. The storage is synchronous (redb is), and wrapping a
//! blocking call in a Promise would hide a thread-pool stall rather than avoid
//! one. When `dxdb-node` grows a real API over an async core this decision gets
//! revisited, and the revisit is a real one, not a formality.
//!
//! # Feature-gated
//!
//! The whole module is behind the `node-addon` feature, off by default. With it
//! on, linking the crate's test binary needs Node's N-API symbols, which exist
//! only inside a Node process; leaving it off keeps the normal gates working
//! without Node in the loop. `xtask/m5-conformance.sh` builds the addon and loads
//! it, which is where it is actually proven.

use napi_derive::napi;

/// Result of a suite run, as Node sees it.
#[napi(object)]
pub struct SuiteOutcome {
    /// Cases that passed.
    pub passed: u32,
    /// Cases that failed.
    pub failed: u32,
    /// Cases skipped by the filter.
    pub skipped: u32,
    /// The full JSON report, so a JS harness can parse rather than reassemble.
    pub json: String,
    /// One line per failure, `FAIL <id>: <message>`.
    pub failures: Vec<String>,
}

/// Runs the M5 storage conformance suite against `dir`.
///
/// `dir` is a path in the *host* filesystem: this is a native addon, so unlike the
/// WASI build there is no preopen and no guest/host distinction. That asymmetry is
/// the reason this target exists as a separate check: it proves the storage, and
/// the WASI targets prove the capability-scoped path to the same storage.
///
/// Throws a JS exception when the directory does not exist, rather than returning a
/// zeroed result that a caller might read as success.
#[napi]
pub fn run_conformance_suite(dir: String, filter: Option<String>) -> napi::Result<SuiteOutcome> {
    let path = std::path::PathBuf::from(&dir);
    // No preopen check here: this is a native addon, so `dir` is an ordinary host path
    // and the guest/host distinction does not apply. The WASI targets are what need it.
    if !path.exists() {
        return Err(napi::Error::from_reason(format!(
            "directory {dir} does not exist"
        )));
    }
    // The real ported suite (48 implementations.test.ts cases), not a smoke suite: the
    // gate says "the conformance suite runs under Node from the napi build", and this is
    // that suite. Because a case failure aborts under `panic = "abort"`, this call either
    // returns having passed every case or never returns at all; `failed` is therefore
    // always 0 here and the interesting information is `passed`.
    let total = dxdb_wasi::conformance::planned_cases().len();
    let outcome = dxdb_wasi::conformance::run(&path, filter.as_deref());
    let failures = if outcome.passed.is_empty() {
        vec![format!(
            "no case matched the filter {:?}",
            filter.as_deref().unwrap_or("")
        )]
    } else {
        Vec::new()
    };
    Ok(SuiteOutcome {
        passed: outcome.passed.len() as u32,
        failed: 0,
        skipped: outcome.skipped.len() as u32,
        json: format!(
            "{{\"passed\":{},\"failed\":0,\"skipped\":{},\"total\":{total}}}",
            outcome.passed.len(),
            outcome.skipped.len()
        ),
        failures,
    })
}

/// The suite's case ids, so a JS harness can report "0 matched" for a filter
/// without hard-coding the list.
#[napi]
pub fn suite_case_ids() -> Vec<String> {
    dxdb_wasi::conformance::planned_cases()
        .iter()
        .map(|c| c.id.to_string())
        .collect()
}

/// The exit code a run should produce: 1 on any failure or an empty run.
///
/// Exposed so the JS harness uses the same verdict rule as the Rust front ends
/// instead of re-deriving it (and getting the empty-run case wrong).
#[napi]
pub fn exit_code_for(passed: u32, failed: u32) -> i32 {
    if failed > 0 || passed == 0 {
        1
    } else {
        0
    }
}
