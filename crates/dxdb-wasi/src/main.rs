//! The conformance CLI: the front end Wasmtime and `node:wasi` drive.
//!
//! This is the program the M5 gate actually executes. On `wasm32-wasip1` and
//! `wasm32-wasip2` it is a `.wasm`; on native it is the same binary, which makes
//! "works natively" and "works from a component" a comparison rather than two
//! separate claims.
//!
//! # Usage
//!
//! ```text
//! dxdb-conformance --dir <path> [--filter <substr>] [--json]
//! ```
//!
//! `--dir` is a **guest-side** path on WASI: the directory mapped in with
//! `wasmtime run --dir <host>::/data` must be referred to here as `/data`, not as
//! the host path. Passing the host path is the trap that produces `ENOENT` from a
//! directory that demonstrably exists (`docs/lanes/m5-port-plan.md` 3.4).
//!
//! # Exit codes
//!
//! | Code | Meaning |
//! | --- | --- |
//! | 0 | every case passed |
//! | 1 | at least one case failed, or no case matched the filter |
//! | 2 | usage error (bad arguments, missing directory) |
//!
//! Failures are printed as `FAIL <id>: <message>` per case, and the process exit
//! code carries the verdict. A run that prints a summary but exits 0 is the bug
//! this is written against.

use dxdb_wasi::suite::{run_suite, CASE_IDS};
use std::path::PathBuf;
use std::process::ExitCode;

/// Exit code for a usage error.
const EXIT_USAGE: u8 = 2;

fn usage() -> String {
    format!(
        "usage: dxdb-conformance --dir <path> [--filter <substr>] [--json]\n\n\
         Runs the M5 storage conformance suite against the directory given.\n\
         On WASI, <path> is the guest-side path of a preopened directory.\n\n\
         cases ({}):\n  {}\n",
        CASE_IDS.len(),
        CASE_IDS.join("\n  ")
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

    // Fail fast with a message that names the capability, rather than letting the
    // first case fail with an unrelated ENOENT inside redb.
    if !dir.exists() {
        eprintln!(
            "error: directory {} does not exist.\n\
             On WASI this must be the guest-side path of a preopened directory: \
             map it with `wasmtime run --dir <host>::<guest>`, and pass <guest> here. \
             A host path will not resolve even when it exists on the host.",
            dir.display()
        );
        return ExitCode::from(EXIT_USAGE);
    }

    let result = run_suite(&dir, filter.as_deref());

    if json {
        println!("{}", result.to_json());
    } else {
        println!("{}", result.summary());
    }

    // An empty run is a failure, so a filter typo cannot look green.
    if result.cases.is_empty() {
        eprintln!(
            "error: no case matched the filter {:?}; refusing to report success",
            filter.as_deref().unwrap_or("")
        );
    }

    ExitCode::from(u8::try_from(result.exit_code()).unwrap_or(1))
}
