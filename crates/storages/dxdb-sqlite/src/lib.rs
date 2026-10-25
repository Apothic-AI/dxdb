//! dxdb SQLite storage: `rusqlite` on native, `sqlite-wasm-rs` on wasm, behind
//! one `SqliteDriver` adapter trait.
//!
//! # Provenance
//!
//! **Port.** Upstream is `src/plugins/storage-sqlite/` in RxDB, Apache-2.0, 1,746
//! lines at pin `3b763ee17`. It is readable and is meant to be read; the
//! behaviour below is translated from it with file:line citations in
//! `docs/lanes/m5-port-plan.md` section 1.1.
//!
//! The *shape* is ours, not upstream's: CLEANROOM.md 5.3 specifies "our own
//! schema DDL, our own prepared statements, our own adapter trait
//! (`SqliteDriver`) with implementations for `rusqlite` and `sqlite-wasm-rs`.
//! Strictly better position than upstream: we own both sides, so no adapter
//! soup." Upstream ships ten implementations of a `sqliteBasics` interface
//! (`sqlite-basics-helpers.ts`) for Node, Node-native, Capacitor, quick-sqlite,
//! two Expo variants, WebSQL, wasm, Tauri; we ship two drivers.
//!
//! # What is ported and what is not
//!
//! Ported behaviour, because it is the storage contract rather than an
//! implementation detail:
//!
//! - bulk write is per-document atomic with a per-row error list (PLAN.md 4.2);
//! - boolean bind parameters become `0`/`1` because SQLite has no boolean type
//!   (`sqlite-helpers.ts:257`, `-basics-helpers.ts:118`);
//! - `?` placeholder counts are checked against the parameter list
//!   (`sqlite-helpers.ts:230`);
//! - result rows arrive in either the object shape `{id, data}` or the
//!   positional `[id, rev, deleted, lwt, data]` shape, and index 4 wins when
//!   present (`sqlite-helpers.ts:62`).
//!
//! Deliberately **not** ported, because they are artificial limits rather than
//! behaviour, and `docs/STATUS.md` records each as an intentional divergence:
//!
//! - the 500-document and 500-operation trial caps
//!   (`sqlite-storage-instance.ts:36-40`, `:100-104`, `:186-249`);
//! - the "no indexes" limitation, i.e. one JSON-blob column and a full table
//!   scan per query (`:389`, `:322`);
//! - the premium-flag warning banner (`:108-128`);
//! - the hard throw on attachments (`:405`, `:474`);
//! - the `BEGIN`-retry loop (`sqlite-helpers.ts:191`), replaced by
//!   `BEGIN IMMEDIATE` plus a `busy_timeout`, which is SQLite's own answer to
//!   the same contention.
//!
//! # Capability matrix position
//!
//! | Target | Driver | VFS |
//! | --- | --- | --- |
//! | native | `rusqlite` 0.41 | default (file) |
//! | `wasm32-wasip1`, `wasm32-wasip2` | `rusqlite` + `sqlite-wasm-rs` via the default `ffi-sqlite-wasm-rs` feature | `wasm32-wasi-vfs`, i.e. the preopens |
//! | `wasm32-unknown-unknown` | same | **in-memory by default**, so a file path must be refused rather than silently becoming ephemeral |
//! | browser OPFS | not here | the `sqlite-wasm-vfs` SAH-pool is worker-only; lane F owns that |
//!
//! The `wasm32-unknown-unknown` row is the one to keep in mind: `rusqlite`
//! defaults to the in-memory VFS on wasm, so OPFS must be installed explicitly.
//! A driver that accepts a path and loses the data on reload is the bug this
//! crate is designed to make impossible.

#![forbid(unsafe_code)]

/// Whether the driver in this build can survive a process restart with the same
/// path, and whether it can store attachments.
///
/// Reported rather than assumed, because the wasm answers differ per target and
/// a silent downgrade is the failure mode that costs users their data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capabilities {
    /// A file path means a file, not an in-memory database with a name.
    pub file_backed: bool,
    /// `get_attachment_data` is implemented rather than throwing. Upstream's
    /// trial storage throws (`sqlite-storage-instance.ts:405`); ours does not.
    pub attachments: bool,
    /// The query planner's `startKeys`/`endKeys` narrow the scan, via the
    /// precomputed index-string tables. Upstream's trial storage cannot do this.
    pub index_pushdown: bool,
}

impl Capabilities {
    /// Capabilities of the driver selected at compile time.
    pub const fn current() -> Self {
        Self {
            // Not wasm32-unknown-unknown, where the default VFS is in-memory.
            file_backed: cfg!(any(
                not(target_arch = "wasm32"),
                all(target_family = "wasm", target_os = "wasi")
            )),
            attachments: true,
            index_pushdown: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ported_limits_are_not_reproduced() {
        // Guards against a future edit quietly reintroducing the trial caps:
        // the design says attachments and index pushdown are supported.
        let caps = Capabilities::current();
        assert!(caps.attachments, "trial cap ('no attachments') is not ported");
        assert!(caps.index_pushdown, "trial cap ('no indexes') is not ported");
    }

    #[test]
    fn the_host_build_is_file_backed() {
        // On any non-wasm build the file VFS is the default, so a path must mean
        // a file. On wasm32-unknown-unknown this is false by design.
        if !cfg!(target_arch = "wasm32") {
            assert!(Capabilities::current().file_backed);
        }
    }
}
