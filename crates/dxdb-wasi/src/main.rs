//! The conformance CLI: the program Wasmtime and `node:wasi` execute.
//!
//! Runs the **ported upstream conformance suite** (`dxdb-testkit`'s 48
//! `implementations.test.ts` cases) against `dxdb-filesystem`, from a real `.wasm` on a
//! preopened directory or natively as a baseline.
//!
//! # Usage
//!
//! ```text
//! dxdb-conformance --dir <path> [--filter <substr>] [--json]
//! ```
//!
//! On WASI, `--dir` is a **guest-side** path: the directory mapped with
//! `wasmtime run --dir <host>::/data` must be referred to here as `/data`, never as the
//! host path. Passing the host path is the trap that produces `ENOENT` from a directory
//! that demonstrably exists, so [`dxdb_wasi::conformance::assert_preopen_usable`] proves
//! the directory is writable before any case runs and refuses with a message naming the
//! mapping.
//!
//! # Exit codes
//!
//! | Code | Meaning |
//! | --- | --- |
//! | 0 | every case passed |
//! | 1 | a case failed (the panic/abort exits 101/134 on native; all nonzero), or no case matched |
//! | 2 | usage error, or the preopen is missing/read-only |
//!
//! A run that printed a summary and exited 0 with a failure would defeat the gate, so the
//! summary is only printed after every case has passed.

use dxdb_wasi::conformance::{assert_preopen_usable, planned_cases, run};
use std::path::PathBuf;
use std::process::ExitCode;

/// Exit code for a usage or preopen problem.
const EXIT_USAGE: u8 = 2;

fn usage() -> String {
    let cases = planned_cases();
    format!(
        "usage: dxdb-conformance --dir <path> [--filter <substr>] [--json]\n\n\
         Runs the {} ported conformance cases (impl-*, from\n\
         test/unit/rx-storage-implementations.test.ts) against dxdb-filesystem.\n\n\
         On WASI, <path> must be the guest-side path of a preopened directory:\n\
           wasmtime run --dir /host/dir::/data dxdb.wasm --dir /data\n\n\
         Cases:\n  {}\n",
        cases.len(),
        cases
            .iter()
            .map(|c| format!("{}  ({})", c.id, c.title))
            .collect::<Vec<_>>()
            .join("\n  ")
    )
}

fn main() -> ExitCode {
    let mut dir: Option<PathBuf> = None;
    let mut filter: Option<String> = None;
    let mut json = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => match args.next() {
                Some(value) => dir = Some(PathBuf::from(value)),
                None => {
                    eprintln!("error: --dir needs a value\n\n{}", usage());
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--filter" => match args.next() {
                Some(value) => filter = Some(value),
                None => {
                    eprintln!("error: --filter needs a value\n\n{}", usage());
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--json" => json = true,
            "--help" | "-h" => {
                println!("{}", usage());
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unexpected argument {other:?}\n\n{}", usage());
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }

    let Some(dir) = dir else {
        eprintln!("error: --dir is required\n\n{}", usage());
        return ExitCode::from(EXIT_USAGE);
    };

    // Prove the preopen before running anything: a missing or read-only mapping must be
    // a startup error that names the mapping, not a confusing first-case failure.
    if let Err(reason) = assert_preopen_usable(&dir) {
        eprintln!("error: {reason}");
        return ExitCode::from(EXIT_USAGE);
    }

    let total = planned_cases().len();
    let outcome = run(&dir, filter.as_deref());

    if json {
        println!(
            "{{\"passed\":{},\"failed\":0,\"skipped\":{},\"total\":{total}}}",
            outcome.passed.len(),
            outcome.skipped.len()
        );
    } else {
        println!("{}", outcome.summary(total));
    }

    if outcome.passed.is_empty() {
        eprintln!(
            "error: no case matched the filter {:?}; refusing to report success",
            filter.as_deref().unwrap_or("")
        );
    }

    ExitCode::from(u8::try_from(outcome.exit_code()).unwrap_or(1))
}
