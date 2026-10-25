//! dxdb filesystem storage: `redb` on native, WASI preopens on `wasm32-wasip1`/
//! `wasip2`, and a stub elsewhere.
//!
//! # Provenance: clean-room, and that has a concrete consequence
//!
//! **No upstream source may be read for this crate.** The filesystem storages
//! (`storage-filesystem-node`, `storage-filesystem-expo`) ship in the closed
//! `rxdb-premium` package, and CLEANROOM.md section 2 forbids reading that code
//! at all. Unlike `dxdb-sqlite`, whose upstream is the Apache-2.0
//! `src/plugins/storage-sqlite/`, there is no readable reference implementation
//! for this crate. It is designed from public documentation plus our own
//! engineering, per CLEANROOM.md 5.4.
//!
//! What that means in practice: if you are about to open `rxdb-premium` to see
//! "how they did it", stop. Read `specs/` and `docs/lanes/m5-port-plan.md`
//! section 2 instead, or ask for a decision.
//!
//! # Why redb
//!
//! Multi-process safety has to come from the engine, not from hand-rolled
//! locking (CLEANROOM.md 5.4). redb is pure Rust, MVCC, and crash safe, and its
//! file locking plus single-writer model is what makes `multiInstance: true` a
//! configuration rather than a project. See `docs/lanes/m5-port-plan.md`
//! section 2.6 for the three separate claims hidden inside "multiInstance safe"
//! and which of them we actually get.
//!
//! # Capability matrix position
//!
//! | Target | Backend |
//! | --- | --- |
//! | native | redb file under the configured `basePath` |
//! | `wasm32-wasip1`, `wasm32-wasip2` | redb over the WASI preopened directory |
//! | `wasm32-unknown-unknown` | stub returning `Unsupported`, so the crate still builds (PLAN.md 4.1) |
//!
//! Browser OPFS is **not** here: it is `dxdb-opfs`, lane F's crate, with a
//! different concurrency model (sync access handles, worker-only).

#![forbid(unsafe_code)]

/// What this backend can do on the target it was compiled for.
///
/// Exposed as a runtime value rather than only as a `cfg` so a user of the crate
/// can ask "did persistence actually happen?" without reading the build log.
/// That question is the one that matters: a storage that silently becomes
/// ephemeral on the wrong target is the failure mode this probes for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capabilities {
    /// Writes survive process exit.
    pub persistent: bool,
    /// Two processes may open the same folder at once.
    pub multi_process: bool,
    /// Two instances in one process see each other's change events.
    pub multi_instance_events: bool,
}

impl Capabilities {
    /// Capabilities of the backend selected at compile time.
    ///
    /// `multi_instance_events` is `false` everywhere today: PLAN.md's design for
    /// cross-process change delivery is `chg`-table tailing, which gives bounded
    /// latency rather than immediate delivery, and it is not implemented yet.
    /// Reporting it honestly now is cheaper than discovering it in a suite run.
    pub const fn current() -> Self {
        Self {
            persistent: cfg!(any(
                not(target_arch = "wasm32"),
                target_os = "wasi"
            )),
            multi_process: cfg!(any(
                not(target_arch = "wasm32"),
                target_os = "wasi"
            )),
            multi_instance_events: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_persistent_build_says_so() {
        // On the host the backend is redb, so both must hold. On
        // wasm32-unknown-unknown the stub reports `false` and a caller can tell
        // the difference without a build log.
        let caps = Capabilities::current();
        if cfg!(any(not(target_arch = "wasm32"), target_os = "wasi")) {
            assert!(caps.persistent, "native/WASI backend must be persistent");
            assert!(caps.multi_process, "redb file locking gives multi-process");
        }
        assert!(
            !caps.multi_instance_events,
            "cross-process change delivery is not implemented yet; do not claim it"
        );
    }
}
