//! dxdb build automation.
//!
//! Run via the Makefile (`make status`, `make vectors`), never directly in CI
//! logs, so the invocation is recorded in one place.
//!
//! Vector generation lives in `xtask/vectors/` because it is a Node program: the
//! oracle has to be actual upstream RxDB, not a reimplementation of it.

fn main() {
    eprintln!(
        "xtask: run `make status` (parity matrix) or `make vectors` (oracle vectors).\n\
         The vector generator is Node: xtask/vectors/generate.mjs."
    );
    std::process::exit(2);
}
