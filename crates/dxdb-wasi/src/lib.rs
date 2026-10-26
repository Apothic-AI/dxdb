//! dxdb WASI host binding: a `wasm32-wasip2` component exporting a WIT world,
//! plus the conformance CLI so the suite can run from a `.wasm` under Wasmtime
//! with a preopened directory. That run is M5's gate (PLAN.md section 6).
//!
//! # Provenance
//!
//! **This crate has no upstream counterpart.** RxDB has no WASI build, no
//! component model target and no WIT surface, in either the Apache-2.0 tree or
//! the premium tree. Everything here is dxdb's own design; see
//! `docs/lanes/m5-port-plan.md` section 3.
//!
//! # Capability matrix position
//!
//! | Target | What this crate does |
//! | --- | --- |
//! | `wasm32-wasip2` | the real thing: a WASI 0.2 component via `wit-bindgen` 0.63.0 |
//! | `wasm32-wasip1` | native WASI 0.1 host functions, no component model |
//! | `wasm32-unknown-unknown` | builds to a stub, because PLAN.md 4.1 requires every crate to build here |
//! | `wasm32-wasip3` | tracked, never shipped: nightly-only, `async` feature (PLAN.md 3.4) |
//!
//! # The preopen rule
//!
//! The guest sees only directories mapped in with `--dir HOST::GUEST`, and sees
//! nothing else: not `/`, not the host's `/tmp`. So every path this crate hands
//! to a storage is a guest-side path, and a missing preopen is a startup error
//! rather than a later `ENOENT`. Both rules are load-bearing; the probe evidence
//! is in `docs/lanes/m5-port-plan.md` section 3.4.

#![forbid(unsafe_code)]

/// The WIT world this crate implements, in source form.
///
/// Lives here rather than as a string so `wasm-tools component wit` output and
/// this file cannot drift without a diff.
pub const WIT_SOURCE: &str = include_str!("../wit/dxdb.wit");

/// Which filesystem driver a component instance should open with.
///
/// Mirrors `driver` in `wit/dxdb.wit`. Kept as a Rust enum so the mapping is
/// exhaustive and a new driver cannot be added on one side only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Driver {
    /// `dxdb-filesystem`: redb, files under a preopened directory.
    Filesystem,
    /// `dxdb-sqlite`: SQLite over the WASI VFS, file under a preopened directory.
    Sqlite,
    /// In-memory only. Legal on every target; the fallback when no preopen exists.
    Memory,
}

impl Driver {
    /// True when this driver needs a preopened directory to be usable.
    ///
    /// `Memory` is the only driver that works without one, which is what makes
    /// it the correct answer for a host that cannot give the component a
    /// directory: a persistent driver with no preopen must fail loudly, because
    /// silently degrading to ephemeral storage is a data-loss bug.
    pub const fn requires_preopen(self) -> bool {
        match self {
            Self::Filesystem | Self::Sqlite => true,
            Self::Memory => false,
        }
    }
}

/// One decoded conformance case result.
///
/// Deliberately not the same type as the WIT `report`: this is the internal
/// accumulator, and `wit/dxdb.wit` is the wire shape. Keeping them separate
/// means the wire format can change without touching the runner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub passed: u32,
    pub failed: u32,
    /// `case-id: message`, one per failure.
    pub failures: Vec<String>,
}

impl Report {
    /// A run is green only when at least one case ran and none failed.
    ///
    /// The "at least one" half matters: a runner that matches no cases and
    /// reports zero failures would otherwise look like a pass.
    pub fn is_green(&self) -> bool {
        self.failed == 0 && self.passed > 0
    }
}

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
    fn an_empty_run_is_not_green() {
        // Guards the vacuous-pass failure mode.
        assert!(!Report::default().is_green());
        assert!(Report {
            passed: 1,
            failed: 0,
            failures: Vec::new()
        }
        .is_green());
        assert!(!Report {
            passed: 1,
            failed: 1,
            failures: vec!["x".into()]
        }
        .is_green());
    }

    #[test]
    fn the_wit_world_is_present_and_names_the_storage_exports() {
        assert!(WIT_SOURCE.contains("world dxdb-wasi"));
        assert!(WIT_SOURCE.contains("export storage;"));
        assert!(WIT_SOURCE.contains("export conformance;"));
    }
}
