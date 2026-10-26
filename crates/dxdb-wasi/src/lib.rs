//! dxdb WASI: the M5 conformance suite and the WIT world for a component host.
//!
//! # What this crate is for
//!
//! PLAN.md's M5 gate is "the conformance suite runs under Wasmtime from a `.wasm`
//! component with a preopened directory, and under Node from both the napi build
//! and the WASI build". This crate holds the suite ([`suite`]) and the CLI front
//! end that all three targets execute, so the gate is one program run three ways
//! rather than three implementations pretending to agree.
//!
//! # Provenance
//!
//! **No upstream counterpart.** RxDB has no WASI build, no component-model target
//! and no WIT surface, in either the Apache-2.0 or the premium tree. Everything
//! here is dxdb's own design; see `docs/lanes/m5-port-plan.md` section 3.
//!
//! # The preopen invariant, which is a real trap
//!
//! On WASI the guest sees **only** directories mapped in with `--dir`, and sees
//! nothing else: not `/`, not the host's `/tmp`. Measured on Wasmtime 50.0.0:
//!
//! ```text
//! wasmtime run --dir /tmp/demo::/data ... PROBE_DIR=/data   read_dir ok, 2 entries
//! wasmtime run --dir /tmp/demo::/data ... PROBE_DIR=/tmp    No such file or directory
//! wasmtime run ... --env PROBE_DIR=/data                    No such file or directory
//! ```
//!
//! So the rules are: every path the suite is given is a **guest-side** path, never
//! a host path; and a missing preopen is a startup failure naming the missing
//! capability, not a later `ENOENT` from inside storage code. [`guest_path_note`]
//! is the message the CLI prints, kept here so it has a test.
//!
//! # Capability matrix position
//!
//! | Target | What this crate does |
//! | --- | --- |
//! | `wasm32-wasip1` | the suite as a CLI, run by `node:wasi` or Wasmtime |
//! | `wasm32-wasip2` | the same suite, inside a WASI 0.2 component |
//! | native | the same suite, for a baseline |
//! | `wasm32-unknown-unknown` | builds, but the suite reports no filesystem |
//!
//! `wasm32-wasip3` is tracked and not shipped: nightly-only, `async` feature
//! (PLAN.md 3.4).

#![forbid(unsafe_code)]

pub mod suite;

/// The WIT world a component host can import, in source form.
///
/// Kept as a constant so `wasm-tools component wit` output and this file cannot
/// drift without a diff.
pub const WIT_SOURCE: &str = include_str!("../wit/dxdb.wit");

/// Which filesystem driver a component instance should open with.
///
/// Mirrors `driver` in `wit/dxdb.wit`, as a Rust enum so the mapping is exhaustive
/// and a new driver cannot be added on one side only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Driver {
    /// `dxdb-filesystem`: redb, files under a preopened directory. The driver the
    /// M5 suite uses, because it is the one that is real on WASI today.
    Filesystem,
    /// `dxdb-sqlite`: SQLite over the WASI VFS. Builds for this target (see
    /// `dxdb-sqlite`'s manifest) but is not yet wired into the suite.
    Sqlite,
    /// In-memory only. Legal on every target; the fallback when no preopen exists.
    Memory,
}

impl Driver {
    /// True when this driver needs a preopened directory to be usable.
    ///
    /// `Memory` is the only driver that works without one, which is what makes it
    /// the correct answer for a host that cannot give the component a directory: a
    /// persistent driver with no preopen must fail loudly, because silently
    /// degrading to ephemeral storage is a data-loss bug.
    pub const fn requires_preopen(self) -> bool {
        match self {
            Self::Filesystem | Self::Sqlite => true,
            Self::Memory => false,
        }
    }
}

/// The message to show when a caller passes a path that does not resolve on WASI.
///
/// A function rather than a string literal so the host-path trap stays testable:
/// the message must mention the *guest-side* path and the `--dir` mapping, because
/// a caller who passes a host path is confused in a specific way.
pub fn guest_path_note(path: &str) -> String {
    format!(
        "{path} does not resolve.\n\
         On WASI a path must be the guest-side name of a preopened directory. \
         Map it with `wasmtime run --dir <host-path>::<guest-path>` and pass \
         <guest-path> here. A host path will not resolve even when it exists on \
         the host, because the guest cannot see the host filesystem."
    )
}

/// One decoded conformance case result (re-exported for hosts).
pub use suite::{CaseResult, SuiteResult, CASE_IDS};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_is_the_only_driver_without_a_preopen() {
        assert!(!Driver::Memory.requires_preopen());
        assert!(Driver::Filesystem.requires_preopen());
        assert!(Driver::Sqlite.requires_preopen());
    }

    #[test]
    fn the_guest_path_note_names_the_mapping_and_the_guest_path() {
        let note = guest_path_note("/host/tmp/db");
        assert!(note.contains("--dir"), "must show the mapping: {note}");
        assert!(
            note.contains("<guest-path>"),
            "must name the guest path: {note}"
        );
        assert!(
            note.contains("host path will not resolve"),
            "must explain why a host path fails: {note}"
        );
    }

    #[test]
    fn the_wit_world_is_present_and_names_the_storage_exports() {
        assert!(WIT_SOURCE.contains("world dxdb-wasi"));
        assert!(WIT_SOURCE.contains("export storage;"));
        assert!(WIT_SOURCE.contains("export conformance;"));
    }

    #[test]
    fn every_case_id_is_reachable() {
        // Cheap guard against the list and the dispatch drifting apart; the
        // exhaustive check is in `suite`.
        assert!(!CASE_IDS.is_empty());
        assert!(CASE_IDS.contains(&"write_read_roundtrip"));
    }
}
