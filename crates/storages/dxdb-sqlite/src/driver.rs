//! The `SqliteDriver` adapter trait and its two implementations.
//!
//! One trait, two drivers: [`rusqlite_driver::RusqliteDriver`] for native and
//! for wasm32 (the same `rusqlite` API, different FFI underneath), and room for a
//! `sqlite-wasm-rs`-specific driver if a target ever needs one. Upstream ships ten
//! implementations of its `sqliteBasics` interface; this is the crate that
//! replaces that with two.
//!
//! # Design rules that are not stylistic
//!
//! * **No `Bool` parameter variant.** SQLite has no boolean type, and several
//!   bindings reject `true`/`false` at bind time. Upstream carries a
//!   `boolParamsToInt()` coercion pass for exactly this (`sqlite-helpers.ts:257`)
//!   plus a second copy for Node-native (`sqlite-basics-helpers.ts:118`). Removing
//!   `bool` from the parameter type deletes the bug class and both functions
//!   instead of remembering to call them.
//!
//! * **Statements are compiled once and named by a closed enum.** The storage
//!   layer keys a cache on [`StatementId`], not on SQL text, so a full table scan
//!   does not re-parse its SQL on every call.
//!
//! * **Rows are delivered to a callback, not collected into a `Vec`.** `rusqlite`'s
//!   `Rows` borrow the statement, and copying every row into owned strings on the
//!   way to discarding most of them is the cost this avoids. The sink decides what
//!   to materialise.
//!
//! * **Transactions are `BEGIN IMMEDIATE`.** Plain `BEGIN` acquires no write lock
//!   until the first write, which is why upstream ends up with a `BEGIN`-retry
//!   loop (`sqlite-helpers.ts:191`) polling `promiseWait(0)` on contention.
//!   `IMMEDIATE` takes the write lock up front, and `busy_timeout` — SQLite's own
//!   mechanism for "another writer holds it, wait" — handles the contention without
//!   burning CPU. Same behaviour, kernel-assisted.

use std::fmt;

/// Why a driver operation failed.
///
/// Deliberately carries the SQLite result code and the operation that produced
/// it. A bare "query failed" is the message that makes a storage bug take a day
/// to find, so the driver's job is to never produce one.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// The underlying SQLite call failed.
    #[error("sqlite {op} failed ({code}): {message}")]
    Sqlite {
        /// Which driver operation was running.
        op: &'static str,
        /// SQLite's primary result code.
        code: i32,
        /// SQLite's message text.
        message: String,
    },

    /// A caller passed a parameter count that does not match the statement.
    ///
    /// Checked before binding rather than left to SQLite, because SQLite's own
    /// error for this is indistinguishable from a malformed statement.
    #[error("statement {id:?} takes {expected} parameters, got {actual}")]
    ParamCount {
        /// The statement.
        id: StatementId,
        /// Placeholders in the SQL.
        expected: usize,
        /// Parameters supplied.
        actual: usize,
    },

    /// A column was read as a type it does not hold.
    #[error("column {column} of {op} is {actual}, not {expected}")]
    TypeMismatch {
        /// Which driver operation was running.
        op: &'static str,
        /// Column index.
        column: usize,
        /// What the caller asked for.
        expected: &'static str,
        /// What SQLite actually had.
        actual: &'static str,
    },

    /// A row callback returned an error, which aborts the scan.
    #[error("row callback aborted: {0}")]
    Callback(String),

    /// The requested driver capability is not available in this build.
    #[error("{0}")]
    Unsupported(&'static str),

    /// Opening a file path when the resolved VFS is in-memory.
    ///
    /// This is the data-loss guard: see [`Vfs`]. A distinct variant because a
    /// caller must be able to tell "configuration is wrong" from "the disk is
    /// full", and because a test asserts on it.
    #[error(
        "refusing to open {path:?}: the active SQLite VFS is in-memory, so the write would be \
         lost on reload. {remedy}"
    )]
    WouldBeEphemeral {
        /// The path that was refused.
        path: String,
        /// The build feature or host change that fixes it.
        remedy: &'static str,
    },
}

/// A prepared statement this storage uses, named rather than spelled.
///
/// A closed enum because it is the cache key: adding a statement is a compile
/// error at every `match` until it is handled, which is what you want from a set
/// of SQL strings where a missing arm would silently run the wrong SQL.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StatementId {
    /// Insert one document row.
    InsertDocument,
    /// Replace one document row.
    ReplaceDocument,
    /// Read one document row by primary value.
    SelectDocumentById,
    /// Read every document row of a collection, id-ordered.
    SelectAllDocuments,
    /// Delete one document row by primary value.
    DeleteDocumentById,
    /// Delete tombstones with `_meta.lwt` below a cutoff.
    DeleteTombstonesBelowLwt,
    /// Read document ids whose `_meta.lwt` is above a cursor, ascending.
    SelectIdsChangedSince,
    /// Count rows in the collection with the given `deleted` flag.
    CountDocuments,
    /// Upsert one index-string entry.
    InsertIndexEntry,
    /// Delete one index-string entry.
    DeleteIndexEntry,
    /// Read ids from an index-string range.
    SelectIdsInIndexRange,
    /// Upsert one attachment blob.
    InsertAttachment,
    /// Read one attachment blob.
    SelectAttachment,
    /// Delete every row of one attachment's document.
    DeleteAttachmentsForDocument,
}

impl StatementId {
    /// The SQL for this statement, given the resolved table names.
    ///
    /// Table names are interpolated because SQLite has no bind parameters for
    /// identifiers. They come from `StorageSchema` plus the collection name, both
    /// validated by [`is_safe_table_name`] before they get here, so this is not an
    /// injection path.
    pub fn sql(self, tables: &TableNames) -> String {
        match self {
            StatementId::InsertDocument => format!(
                "INSERT INTO {docs} (id, revision, deleted, lwt, data) VALUES (?, ?, ?, ?, ?)",
                docs = tables.docs
            ),
            StatementId::ReplaceDocument => format!(
                "UPDATE {docs} SET revision = ?, deleted = ?, lwt = ?, data = ? WHERE id = ?",
                docs = tables.docs
            ),
            StatementId::SelectDocumentById => {
                format!(
                    "SELECT id, revision, deleted, lwt, data FROM {docs} WHERE id = ?",
                    docs = tables.docs
                )
            }
            StatementId::SelectAllDocuments => format!(
                "SELECT id, revision, deleted, lwt, data FROM {docs} ORDER BY id",
                docs = tables.docs
            ),
            StatementId::DeleteDocumentById => {
                format!("DELETE FROM {docs} WHERE id = ?", docs = tables.docs)
            }
            StatementId::DeleteTombstonesBelowLwt => format!(
                "DELETE FROM {docs} WHERE deleted = 1 AND lwt < ?",
                docs = tables.docs
            ),
            StatementId::SelectIdsChangedSince => format!(
                "SELECT id, lwt FROM {docs} WHERE lwt > ? ORDER BY lwt, id LIMIT ?",
                docs = tables.docs
            ),
            StatementId::CountDocuments => format!(
                "SELECT COUNT(*) FROM {docs} WHERE deleted = ?",
                docs = tables.docs
            ),
            StatementId::InsertIndexEntry => format!(
                "INSERT OR REPLACE INTO {idx} (index_string, id) VALUES (?, ?)",
                idx = tables.index
            ),
            StatementId::DeleteIndexEntry => format!(
                "DELETE FROM {idx} WHERE index_string = ? AND id = ?",
                idx = tables.index
            ),
            StatementId::SelectIdsInIndexRange => format!(
                "SELECT index_string, id FROM {idx} WHERE index_string >= ? AND index_string <= ? \
                 ORDER BY index_string, id",
                idx = tables.index
            ),
            StatementId::InsertAttachment => format!(
                "INSERT OR REPLACE INTO {att} (id, attachment_id, data) VALUES (?, ?, ?)",
                att = tables.attachments
            ),
            StatementId::SelectAttachment => format!(
                "SELECT data FROM {att} WHERE id = ? AND attachment_id = ?",
                att = tables.attachments
            ),
            StatementId::DeleteAttachmentsForDocument => {
                format!("DELETE FROM {att} WHERE id = ?", att = tables.attachments)
            }
        }
    }

    /// How many `?` placeholders the SQL above has.
    ///
    /// Kept next to [`Self::sql`] so the two cannot drift, and asserted by a test
    /// that counts the placeholders in the generated SQL. Upstream's
    /// `ensureParamsCountIsCorrect()` (`sqlite-helpers.ts:230`) does this check at
    /// runtime in dev mode; here it is structural *and* checked at runtime.
    pub const fn param_count(self) -> usize {
        match self {
            StatementId::SelectAllDocuments => 0,
            StatementId::SelectDocumentById => 1,
            StatementId::DeleteDocumentById => 1,
            StatementId::DeleteTombstonesBelowLwt => 1,
            StatementId::CountDocuments => 1,
            StatementId::DeleteAttachmentsForDocument => 1,
            StatementId::InsertDocument => 5,
            StatementId::ReplaceDocument => 5,
            StatementId::SelectIdsChangedSince => 2,
            StatementId::InsertIndexEntry => 2,
            StatementId::DeleteIndexEntry => 2,
            StatementId::SelectIdsInIndexRange => 2,
            StatementId::InsertAttachment => 3,
            StatementId::SelectAttachment => 2,
        }
    }
}

impl fmt::Display for StatementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// The resolved table names for one collection instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableNames {
    /// `"<collection>_<version>"`.
    pub docs: String,
    /// `"<collection>_<version>_idx"`.
    pub index: String,
    /// `"<collection>_<version>_att"`.
    pub attachments: String,
}

impl TableNames {
    /// Builds the three table names for a collection at a schema version.
    ///
    /// The version is part of every name so two schema versions of one collection
    /// coexist in one file, which upstream also does with
    /// `collectionName + '-' + schema.version`
    /// (`sqlite-storage-instance.ts:474`). We use `_` rather than `-` because
    /// these double as unquoted-safe identifiers in our own tooling.
    pub fn new(collection: &str, version: u32) -> Self {
        let base = format!("{collection}_{version}");
        TableNames {
            docs: base.clone(),
            index: format!("{base}_idx"),
            attachments: format!("{base}_att"),
        }
    }
}

/// True when a name is safe to interpolate into SQL as an identifier.
///
/// The allowlist is exactly what an RxDB collection name and a primary path can
/// be, which is what makes [`StatementId::sql`]'s interpolation safe: a name that
/// fails this check never reaches it.
pub fn is_safe_table_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A bound parameter.
///
/// No `Bool` and no `Option`: `Param::Null` covers absence, and a caller with a
/// `bool` writes `if b { Param::Int(1) } else { Param::Int(0) }` at the one place
/// it knows the meaning. `Text` and `Blob` borrow, so binding a document does not
/// copy it.
#[derive(Clone, Copy, Debug)]
pub enum Param<'a> {
    /// SQL `NULL`.
    Null,
    /// Integer, including the `0`/`1` a boolean becomes.
    Int(i64),
    /// Floating point.
    Real(f64),
    /// UTF-8 text.
    Text(&'a str),
    /// Arbitrary bytes.
    Blob(&'a [u8]),
}

impl Param<'_> {
    /// The name SQLite would use for this value's storage class.
    pub const fn type_name(&self) -> &'static str {
        match self {
            Param::Null => "null",
            Param::Int(_) => "integer",
            Param::Real(_) => "real",
            Param::Text(_) => "text",
            Param::Blob(_) => "blob",
        }
    }
}

/// A decoded result row.
///
/// Borrowed from the statement's scratch buffer, so it is only valid inside the
/// callback it was handed to. That constraint is what lets `get_text` return
/// `&str` instead of `String`.
pub trait Row {
    /// True when the column holds `NULL`.
    fn is_null(&self, column: usize) -> bool;
    /// Reads an integer.
    fn get_i64(&self, column: usize) -> Result<i64, DriverError>;
    /// Reads a float.
    fn get_f64(&self, column: usize) -> Result<f64, DriverError>;
    /// Reads text.
    fn get_text(&self, column: usize) -> Result<&str, DriverError>;
    /// Reads bytes.
    fn get_blob(&self, column: usize) -> Result<&[u8], DriverError>;
}

/// Which SQLite VFS is in force, and therefore whether a path means a file.
///
/// This exists because the answer differs by target and a wrong answer is silent
/// data loss rather than an error:
///
/// | Target | Default VFS | A file path means |
/// | --- | --- | --- |
/// | native | the real file VFS | a file |
/// | `wasm32-wasip1`/`wasip2` without `wasm32-wasi-vfs` | in-memory | **nothing, it is discarded on reload** |
/// | `wasm32-wasip1`/`wasip2` with `wasm32-wasi-vfs` | the preopens | a file under a preopened directory |
/// | `wasm32-unknown-unknown` | in-memory | nothing, until a VFS is installed |
///
/// Callers read [`Vfs::selected`] and refuse a file path when it is not
/// [`Vfs::File`]; [`DriverError::WouldBeEphemeral`] is the refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vfs {
    /// A real file. Paths persist.
    File,
    /// In-memory. A path is a name for a scratch database, not a file.
    Memory,
    /// A worker-only OPFS pool (browser). Files persist, but only in a worker.
    OpfsSahPool,
}

impl Vfs {
    /// The VFS selected by the current build configuration and target.
    pub const fn selected() -> Self {
        #[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
        {
            if cfg!(feature = "wasm32-wasi-vfs") {
                Vfs::File
            } else {
                Vfs::Memory
            }
        }
        #[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
        {
            if cfg!(feature = "opfs-sah-pool") {
                Vfs::OpfsSahPool
            } else {
                Vfs::Memory
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Vfs::File
        }
    }

    /// Whether a filesystem path is meaningful under this VFS.
    pub const fn is_file_backed(self) -> bool {
        matches!(self, Vfs::File | Vfs::OpfsSahPool)
    }

    /// A one-line description for logs and error messages.
    pub const fn describe(self) -> &'static str {
        match self {
            Vfs::File => "file",
            Vfs::Memory => "memory",
            Vfs::OpfsSahPool => "opfs-sah-pool (worker only)",
        }
    }

    /// The build feature that would make a path persist, for the error message.
    pub const fn remedy(self) -> &'static str {
        match self {
            Vfs::File | Vfs::OpfsSahPool => "",
            Vfs::Memory => {
                "Enable the `wasm32-wasi-vfs` feature on wasm32-wasip1/wasip2, or install a \
                 persistent VFS (the `opfs-sah-pool` feature in a dedicated worker) before \
                 opening a file path."
            }
        }
    }
}

/// Whether this build can store attachments.
///
/// Upstream's trial storage throws on attachments
/// (`sqlite-storage-instance.ts:405`) and refuses a schema with an `attachments`
/// block (`:474`). That is one of the premium limits `docs/STATUS.md` records as
/// intentionally not ported, so this is `true` everywhere and exists only so a
/// test can assert the divergence.
pub const ATTACHMENTS_SUPPORTED: bool = true;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_param_count_matches_its_sql() {
        // The invariant that makes `param_count` worth having: count the
        // placeholders in the generated SQL and compare. Upstream does this at
        // runtime in dev mode (`ensureParamsCountIsCorrect`); here it is a test.
        let tables = TableNames::new("todos", 1);
        let all = [
            StatementId::InsertDocument,
            StatementId::ReplaceDocument,
            StatementId::SelectDocumentById,
            StatementId::SelectAllDocuments,
            StatementId::DeleteDocumentById,
            StatementId::DeleteTombstonesBelowLwt,
            StatementId::SelectIdsChangedSince,
            StatementId::CountDocuments,
            StatementId::InsertIndexEntry,
            StatementId::DeleteIndexEntry,
            StatementId::SelectIdsInIndexRange,
            StatementId::InsertAttachment,
            StatementId::SelectAttachment,
            StatementId::DeleteAttachmentsForDocument,
        ];
        for id in all {
            let sql = id.sql(&tables);
            let placeholders = sql.matches('?').count();
            assert_eq!(
                placeholders,
                id.param_count(),
                "{id:?} declares {} params but its SQL has {placeholders}: {sql}",
                id.param_count()
            );
        }
    }

    #[test]
    fn table_names_embed_the_schema_version() {
        // Two schema versions must not share tables, or a migration would read
        // the other version's rows.
        let v1 = TableNames::new("todos", 1);
        let v2 = TableNames::new("todos", 2);
        assert_ne!(v1.docs, v2.docs);
        assert!(v1.docs.ends_with("_1"));
        assert!(v2.docs.ends_with("_2"));
        assert!(is_safe_table_name(&v1.docs));
        assert!(is_safe_table_name(&v1.index));
        assert!(is_safe_table_name(&v1.attachments));
    }

    #[test]
    fn unsafe_names_are_rejected() {
        // This is the guard that makes interpolation into SQL safe.
        assert!(is_safe_table_name("todos_1"));
        assert!(is_safe_table_name("my-collection_2"));
        assert!(!is_safe_table_name(""));
        assert!(!is_safe_table_name("todos; DROP TABLE x"));
        assert!(!is_safe_table_name("todos\"; --"));
        assert!(!is_safe_table_name("todos\u{0}"));
        assert!(!is_safe_table_name(&"x".repeat(129)));
    }

    #[test]
    fn parameter_type_names_are_distinct() {
        // A wrong-type error message is only useful if it names the right type.
        let values = [
            Param::Null,
            Param::Int(0),
            Param::Real(0.0),
            Param::Text(""),
            Param::Blob(&[]),
        ];
        let names: Vec<_> = values.iter().map(Param::type_name).collect();
        assert_eq!(names, ["null", "integer", "real", "text", "blob"]);
    }

    #[test]
    fn the_host_build_is_file_backed() {
        if !cfg!(target_arch = "wasm32") {
            assert_eq!(Vfs::selected(), Vfs::File);
            assert!(Vfs::selected().is_file_backed());
        }
    }

    #[test]
    fn a_memory_vfs_explains_its_remedy() {
        // The refusal message has to say what to change, or it is just a wall.
        assert!(Vfs::Memory.remedy().contains("wasm32-wasi-vfs"));
        assert!(!Vfs::Memory.is_file_backed());
        assert!(Vfs::File.is_file_backed());
    }
}
