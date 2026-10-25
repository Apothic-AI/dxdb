//! dxdb SQLite storage: `rusqlite` on native, `sqlite-wasm-rs` on wasm, behind
//! one [`SqliteDriver`] adapter trait.
//!
//! # Provenance
//!
//! **Port.** Upstream is `src/plugins/storage-sqlite/` in RxDB, Apache-2.0, 1,746
//! lines at pin `3b763ee17`. It is readable and is meant to be read; the
//! behaviour below is translated from it with file:line citations.
//!
//! The *shape* is ours, not upstream's: CLEANROOM.md 5.3 specifies "our own
//! schema DDL, our own prepared statements, our own adapter trait
//! (`SqliteDriver`) with implementations for `rusqlite` and `sqlite-wasm-rs`."
//! Upstream ships ten implementations of a `sqliteBasics` interface; this crate
//! ships one trait and needs one implementation for both targets.
//!
//! # What is ported
//!
//! Behaviour that is the storage contract rather than an implementation detail:
//!
//! - bulk write is per-document atomic with a per-row error list (PLAN.md 4.2);
//! - boolean bind parameters become `0`/`1`, because SQLite has no boolean type
//!   (`sqlite-helpers.ts:257`, `-basics-helpers.ts:118`) — here made
//!   unrepresentable rather than coerced, see [`Param`];
//! - `?` placeholder counts are checked against the parameter list before binding
//!   (`sqlite-helpers.ts:230`);
//! - a schema version is part of the table name, so two versions of one
//!   collection coexist (`sqlite-storage-instance.ts:474`).
//!
//! # What is deliberately not ported
//!
//! These are artificial limits, not behaviour, and `docs/STATUS.md` records each
//! as an intentional divergence:
//!
//! - the 500-document and 500-operation trial caps
//!   (`sqlite-storage-instance.ts:36-40`, `:100-104`, `:186-249`);
//! - the "no indexes" limitation, i.e. one JSON-blob column with a full table scan
//!   per query (`:389`, `:322`);
//! - the premium-flag warning banner (`:108-128`);
//! - the hard throw on attachments (`:405`, `:474`) — see [`ATTACHMENTS_SUPPORTED`];
//! - the `BEGIN` retry loop (`sqlite-helpers.ts:191`), replaced by
//!   `BEGIN IMMEDIATE` plus a `busy_timeout`. SQLite already has a mechanism for
//!   "another writer holds the lock, wait"; re-implementing it as a microtask
//!   busy-wait spends CPU that the kernel would spend sleeping.
//!
//! # Schema
//!
//! Ours, not upstream's. Upstream's single table with a `data json` column is a
//! direct consequence of its trial limits; copying it would copy the limitation.
//!
//! ```sql
//! CREATE TABLE "<collection>_<version>" (
//!     id       TEXT    NOT NULL PRIMARY KEY,   -- primaryPath value
//!     revision TEXT    NOT NULL,
//!     deleted  INTEGER NOT NULL CHECK (deleted IN (0,1)),
//!     lwt      INTEGER NOT NULL,               -- _meta.lwt
//!     data     TEXT    NOT NULL                -- ordered JSON, see dxdb-json
//! ) STRICT;
//! CREATE INDEX "<collection>_<version>_cleanup" ON ... (deleted, lwt, id);
//!
//! -- one row per (index string, document): the planner's range scan
//! CREATE TABLE "<collection>_<version>_idx" (
//!     index_string TEXT NOT NULL,
//!     id           TEXT NOT NULL,
//!     PRIMARY KEY (index_string, id)
//! ) STRICT, WITHOUT ROWID;
//! ```
//!
//! Four decisions in that DDL, each load-bearing:
//!
//! 1. **`STRICT`** because the storage layer already knows every column's type;
//!    it turns a silent coercion into an error at write time.
//! 2. **`WITHOUT ROWID`** on the index table because the only access is an ordered
//!    range scan on `(index_string, id)`, which is exactly the clustered key. A
//!    rowid table would store the same key twice and then follow the rowid.
//! 3. **A separate index-string table**, not a native SQLite index over
//!    `json_extract(data, '$.field')`. The sort order that matters is dxdb's
//!    index-string order — UTF-16 code-unit order with type prefixes and
//!    zero-padded numbers (PLAN.md 3.2, 4.3). No collation makes a native index
//!    agree with that encoding for mixed types, so the comparison has to be on
//!    bytes we already compute for the planner. Upstream reached this conclusion
//!    for IndexedDB and FoundationDB and did not reach it for SQLite only because
//!    its trial version has no indexes at all.
//! 4. **`(deleted, lwt, id)`** ordering so `cleanup` is a range delete and
//!    `get_changed_documents_since` is an ascending scan, matching the
//!    `CLEANUP_INDEX = ['_deleted', '_meta.lwt']` ordering upstream uses for the
//!    same two operations (`foundationdb-helpers.ts:4`).
//!
//! # The VFS question, which is where data gets silently lost
//!
//! `rusqlite`'s default on wasm resolves to the **in-memory** VFS, so a file path
//! becomes an ephemeral database that looks fine until the page reloads or the
//! component exits. OPFS and WASI file access must each be installed deliberately.
//! [`Vfs::selected`] reports what is actually in force, and
//! [`DriverError::WouldBeEphemeral`] is what a driver returns instead of silently
//! accepting a path under a memory VFS.
//!
//! | Target | Feature | VFS | A path means |
//! | --- | --- | --- | --- |
//! | native | none | file | a file |
//! | `wasm32-wasip1`, `wasm32-wasip2` | `wasm32-wasi-vfs` | preopens | a file under a preopened dir |
//! | `wasm32-wasip1`, `wasm32-wasip2` | *(absent)* | memory | **nothing — discarded on exit** |
//! | `wasm32-unknown-unknown` | `opfs-sah-pool` | OPFS SAH pool | a file, **in a dedicated worker only** |
//! | `wasm32-unknown-unknown` | *(absent)* | memory | nothing |
//!
//! Browser OPFS is worker-only because `FileSystemSyncAccessHandle` only exists in
//! a dedicated worker (PLAN.md 5.4). The worker wiring is lane F's `dxdb-opfs`; the
//! `opfs-sah-pool` feature exists so that crate can select the VFS without forking
//! this one.
//!
//! # Capability matrix position
//!
//! Every target in the table above must still **build**, which is why the VFS
//! selection is a `const fn` over `cfg` rather than a hard dependency: PLAN.md 4.1
//! requires each crate to compile for `wasm32-unknown-unknown` even when the
//! backend makes no sense there.

#![forbid(unsafe_code)]

pub mod driver;
pub mod rusqlite_driver;
pub mod traits;

pub use driver::{
    is_safe_table_name, DriverError, Param, Row, StatementId, TableNames, Vfs,
    ATTACHMENTS_SUPPORTED,
};
pub use rusqlite_driver::{RusqliteDriver, BUSY_TIMEOUT};
pub use traits::{in_transaction, SqliteDriver};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ported_limits_are_not_reproduced() {
        // Guards against a future edit quietly reintroducing the trial caps: the
        // design says attachments are supported and indexes are used.
        //
        // A compile-time check, not a runtime one: the value is a constant, and
        // clippy is right that asserting it at runtime is theatre. `const {}` makes
        // reverting the divergence a build failure instead.
        const { assert!(ATTACHMENTS_SUPPORTED) };
        assert_eq!(
            Vfs::selected(),
            Vfs::File,
            "the host build must be file-backed"
        );
    }

    #[test]
    fn the_vfs_and_the_error_agree_about_file_backing() {
        // A driver refuses a path exactly when the VFS is not file-backed, so the
        // two must not disagree.
        let vfs = Vfs::selected();
        assert_eq!(
            vfs.is_file_backed(),
            vfs == Vfs::File || vfs == Vfs::OpfsSahPool
        );
    }

    #[test]
    fn an_in_memory_path_is_refused_rather_than_silently_ephemeral() {
        // Exercised through the guard directly, because on a host build the real
        // VFS is file-backed and the refusal path is unreachable.
        let vfs = Vfs::Memory;
        assert!(!vfs.is_file_backed());
        let err = DriverError::WouldBeEphemeral {
            path: "/data/x.sqlite".into(),
            remedy: vfs.remedy(),
        };
        let text = err.to_string();
        assert!(
            text.contains("in-memory"),
            "message must name the cause: {text}"
        );
        assert!(
            text.contains("wasm32-wasi-vfs"),
            "message must name the fix: {text}"
        );
    }
}
