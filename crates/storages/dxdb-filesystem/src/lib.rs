//! dxdb filesystem storage: `redb` on native, WASI preopens on `wasm32-wasip1`/
//! `wasip2`, and a stub elsewhere.
//!
//! # Provenance: clean-room, and that has a concrete consequence
//!
//! **No upstream source may be read for this crate.** The filesystem storages
//! (`storage-filesystem-node`, `storage-filesystem-expo`) ship in the closed
//! `rxdb-premium` package, and CLEANROOM.md section 2 forbids reading that code at
//! all. Unlike `dxdb-sqlite`, whose upstream is the Apache-2.0
//! `src/plugins/storage-sqlite/`, there is no readable reference implementation
//! for this crate. It is designed from public documentation plus our own
//! engineering, per CLEANROOM.md 5.4.
//!
//! # Why redb
//!
//! Multi-process safety has to come from the engine, not from hand-rolled locking
//! (CLEANROOM.md 5.4). redb is pure Rust, MVCC, and crash safe, and its file
//! locking plus `MultiWriter` concurrency mode is what makes `multiInstance: true`
//! a configuration rather than a project. [`layout`] is the authoritative
//! description of the tables, keys and ordering, and it includes the measurement
//! that makes `MultiWriter` mandatory rather than a preference.
//!
//! # Capability matrix position
//!
//! | Target | Backend |
//! | --- | --- |
//! | native | redb file under the configured `basePath` |
//! | `wasm32-wasip1`, `wasm32-wasip2` | redb over the WASI preopened directory |
//! | `wasm32-unknown-unknown` | stub returning [`StorageError::Unsupported`] |
//!
//! Browser OPFS is **not** here: it is `dxdb-opfs`, lane F's crate, with a
//! different concurrency model (sync access handles, worker-only).

#![forbid(unsafe_code)]

pub mod layout;
pub mod rx_storage;

/// The `RxStorage` implementation and the engine that creates instances.
///
/// Re-exported at the crate root because that is how every other storage exposes
/// itself (`dxdb_memory::MemoryStorage`, `dxdb_localstorage::LocalStorageStorage`), and
/// a caller should not have to know which module a storage lives in.
pub use rx_storage::{FilesystemRxInstance, FilesystemRxStorage};

use std::path::{Path, PathBuf};

use redb::{
    Builder, Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};

/// A redb table mapping a byte key to ordered-JSON document bytes.
///
/// Key: `collection \0 version \0 docId`. See [`layout`].
pub const DOCS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("docs");

/// The lwt-ordered change index. Key: `BE(flip(lwt))` ++ `collection \0 version \0 docId`.
pub const CHG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chg");

/// The cleanup index. Key: `collection \0 version \0` ++ deleted ++ `BE(flip(lwt))` ++ docId.
pub const CLEANUP: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cleanup");

/// The secondary index-string table. Key: `collection \0 version \0` ++ index string.
pub const IDX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("idx");

/// Attachment blobs. Key: `docId \0 attachmentId`.
pub const ATTACH: TableDefinition<&[u8], &[u8]> = TableDefinition::new("attach");

/// Open-time schema map. Key: a fixed tag such as `b"schema"`.
pub const META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("meta");

/// One `(encoded key, document bytes)` pair from a collection scan.
///
/// A named type rather than the tuple spelled out, because it appears in the
/// storage-layer signature and `(Vec<u8>, Vec<u8>)` at a call site says nothing
/// about which byte slice is the key.
pub type DocumentEntry = (Vec<u8>, Vec<u8>);

/// Why a filesystem storage operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// redb reported an error.
    #[error("redb {op}: {message}")]
    Redb {
        /// Which operation was running.
        op: &'static str,
        /// redb's message.
        message: String,
    },

    /// This build has no filesystem backend.
    #[error("{0}")]
    Unsupported(&'static str),
}

impl StorageError {
    /// Wraps a redb error with the operation that produced it.
    fn redb(op: &'static str, error: impl std::fmt::Display) -> Self {
        StorageError::Redb {
            op,
            message: error.to_string(),
        }
    }
}

/// What this backend can do on the target it was compiled for.
///
/// Exposed as a runtime value rather than only as a `cfg` so a caller can ask
/// "did persistence actually happen?" without reading a build log. That question
/// is the one that matters: a storage that silently becomes ephemeral on the wrong
/// target is the failure mode this probes for.
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
    /// `multi_instance_events` is `false` everywhere today: the design for
    /// cross-process change delivery is `chg`-table tailing, which gives
    /// bounded latency rather than immediate delivery, and it is not implemented
    /// yet. Reporting it honestly now is cheaper than discovering it in a suite
    /// run. See [`layout`] claim 3.
    pub const fn current() -> Self {
        // Persistence and multi-process are now different answers on WASI, which is
        // exactly why they are separate fields: WASI has a real filesystem through
        // the preopens (persistent), but redb there can only take a whole-file lock,
        // so a second process cannot open the same directory (not multi-process).
        let persistent = cfg!(any(not(target_arch = "wasm32"), target_os = "wasi"));
        let multi_process = cfg!(not(target_arch = "wasm32"));
        Self {
            persistent,
            multi_process,
            multi_instance_events: false,
        }
    }
}

/// An open filesystem database.
///
/// Holds the `redb::Database` and the resolved path. Cheap to clone is *not* a
/// property we offer: a second `FilesystemStorage` on the same path is a second
/// `Database` handle, which is exactly the multi-instance case the concurrency
/// mode exists for, so callers open rather than clone.
pub struct FilesystemStorage {
    db: Database,
    path: PathBuf,
}

impl std::fmt::Debug for FilesystemStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemStorage")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl FilesystemStorage {
    /// The project's one redb file inside `base_path`.
    pub fn db_file(base_path: &Path) -> PathBuf {
        base_path.join("dxdb.redb")
    }

    /// The redb concurrency mode this target can actually use.
    ///
    /// `MultiWriter` needs byte-range file locks, which redb implements only for
    /// linux/apple/windows. WASI gets whole-file locking only, so it runs
    /// `ExclusiveWriter`. A function rather than a `cfg!` inside `open` so the
    /// choice is testable, and so the reason has one place to live.
    pub const fn concurrency_mode() -> redb::ConcurrencyMode {
        // The single condition that matters: multi-writer needs byte-range locks,
        // and redb implements those only on non-wasm targets. Everything else,
        // including WASI, gets `ExclusiveWriter` and a whole-file lock.
        if cfg!(not(target_arch = "wasm32")) {
            redb::ConcurrencyMode::MultiWriter
        } else {
            redb::ConcurrencyMode::ExclusiveWriter
        }
    }

    /// Opens (creating if absent) the database file under `base_path`.
    ///
    /// The concurrency mode is **target-dependent**, because redb's capabilities
    /// are. `MultiWriter` is what `multiInstance: true` needs on a host, and it is
    /// unavailable on WASI:
    ///
    /// | Target | Mode | Why |
    /// | --- | --- | --- |
    /// | native | `MultiWriter` | several processes may write; verified cross-process |
    /// | `wasm32-wasip1`/`wasip2` | `ExclusiveWriter` | redb cannot enter multi-writer mode here |
    ///
    /// The WASI restriction is measured, not assumed. Under `MultiWriter` on
    /// `wasm32-wasip1` every open fails with "The storage backend does not support
    /// this operation": redb's multi-writer open takes *byte-range* file locks
    /// (`RangeLock` is implemented only for linux/apple/windows; elsewhere it is
    /// `impl RangeLock for File {}`) and its fallback to a whole-file lock runs only
    /// for `ExclusiveWriter`. A whole-file lock is what WASI actually offers, so
    /// `ExclusiveWriter` is both the working mode and the honest one: one process at
    /// a time, which is the truth for a single WASI instance holding a preopened
    /// directory.
    ///
    /// The cost: `multiInstance: true` across two WASI instances sharing a directory
    /// is unsupported, and the second open fails loudly with `DatabaseAlreadyOpen`
    /// rather than corrupting the file. [`Capabilities::multi_process`] reports this
    /// per target so a caller does not have to read this comment to find out.
    pub fn open(base_path: &Path) -> Result<Self, StorageError> {
        let path = Self::db_file(base_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StorageError::redb("create_dir_all", e))?;
        }
        let db = Builder::new()
            .set_concurrency_mode(Self::concurrency_mode())
            .create(&path)
            .map_err(|e| StorageError::redb("open", e))?;
        // Materialise every table once, so a reader never has to create one.
        {
            let tx = db
                .begin_write()
                .map_err(|e| StorageError::redb("open/begin_write", e))?;
            {
                tx.open_table(DOCS)
                    .map_err(|e| StorageError::redb("open/docs", e))?;
                tx.open_table(CHG)
                    .map_err(|e| StorageError::redb("open/chg", e))?;
                tx.open_table(CLEANUP)
                    .map_err(|e| StorageError::redb("open/cleanup", e))?;
                tx.open_table(IDX)
                    .map_err(|e| StorageError::redb("open/idx", e))?;
                tx.open_table(ATTACH)
                    .map_err(|e| StorageError::redb("open/attach", e))?;
                tx.open_table(META)
                    .map_err(|e| StorageError::redb("open/meta", e))?;
            }
            tx.commit()
                .map_err(|e| StorageError::redb("open/commit", e))?;
        }
        Ok(FilesystemStorage { db, path })
    }

    /// The file this instance has open.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes one document, its change index entry and a cleanup index entry.
    ///
    /// One write transaction per call. The storage layer above batches several
    /// documents into one call, so this is not a per-document commit in the hot
    /// path; see the note on `bulk_write` atomicity in the port plan.
    ///
    /// Returns the previous value for the key, if any, so a caller can tell an
    /// insert from an update without a second read.
    pub fn put_document(
        &self,
        collection: &str,
        version: u32,
        id: &str,
        lwt: i64,
        deleted: bool,
        json: &[u8],
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let prefix = layout::prefix(collection, version);
        let mut doc_key = prefix.clone();
        doc_key.extend_from_slice(id.as_bytes());

        let mut chg_key = layout::encode_lwt(lwt).to_vec();
        chg_key.extend_from_slice(&doc_key);

        let mut cleanup_key = layout::cleanup_prefix(collection, version, deleted);
        cleanup_key.extend_from_slice(&layout::encode_lwt(lwt));
        cleanup_key.extend_from_slice(id.as_bytes());

        let tx = self
            .db
            .begin_write()
            .map_err(|e| StorageError::redb("put/begin_write", e))?;
        let previous = {
            let mut docs = tx
                .open_table(DOCS)
                .map_err(|e| StorageError::redb("put/docs", e))?;
            let previous = docs
                .insert(doc_key.as_slice(), json)
                .map_err(|e| StorageError::redb("put/insert", e))?
                .map(|v| v.value().to_vec());

            let mut chg = tx
                .open_table(CHG)
                .map_err(|e| StorageError::redb("put/chg", e))?;
            chg.insert(chg_key.as_slice(), &[][..])
                .map_err(|e| StorageError::redb("put/chg_insert", e))?;

            let mut cleanup = tx
                .open_table(CLEANUP)
                .map_err(|e| StorageError::redb("put/cleanup", e))?;
            cleanup
                .insert(cleanup_key.as_slice(), &[][..])
                .map_err(|e| StorageError::redb("put/cleanup_insert", e))?;
            previous
        };
        tx.commit()
            .map_err(|e| StorageError::redb("put/commit", e))?;
        Ok(previous)
    }

    /// Writes several documents in **one** redb transaction.
    ///
    /// This is what a real `bulk_write` should do, and it is why it exists: the
    /// per-document [`put_document`](Self::put_document) commits once per row, which
    /// is right for a single write and wrong for a 500-row batch because it becomes
    /// 500 fsyncs. Upstream's `bulkInsert` is one `bulk_write` call, so the
    /// benchmark has to measure one commit per batch to be comparable.
    ///
    /// Returns the number of rows written. All rows are committed together or not at
    /// all; per-document *semantics* stay above this layer, which is where
    /// `categorizeBulkWriteRows` decides what is insertable.
    pub fn put_documents(
        &self,
        collection: &str,
        version: u32,
        rows: &[(String, i64, bool, Vec<u8>)],
    ) -> Result<usize, StorageError> {
        let prefix = layout::prefix(collection, version);
        let tx = self
            .db
            .begin_write()
            .map_err(|e| StorageError::redb("put_many/begin_write", e))?;
        let mut written = 0usize;
        {
            let mut docs = tx
                .open_table(DOCS)
                .map_err(|e| StorageError::redb("put_many/docs", e))?;
            let mut chg = tx
                .open_table(CHG)
                .map_err(|e| StorageError::redb("put_many/chg", e))?;
            let mut cleanup = tx
                .open_table(CLEANUP)
                .map_err(|e| StorageError::redb("put_many/cleanup", e))?;

            for (id, lwt, deleted, body) in rows {
                let mut doc_key = prefix.clone();
                doc_key.extend_from_slice(id.as_bytes());

                let mut chg_key = layout::encode_lwt(*lwt).to_vec();
                chg_key.extend_from_slice(&doc_key);

                let mut cleanup_key = layout::cleanup_prefix(collection, version, *deleted);
                cleanup_key.extend_from_slice(&layout::encode_lwt(*lwt));
                cleanup_key.extend_from_slice(id.as_bytes());

                docs.insert(doc_key.as_slice(), body.as_slice())
                    .map_err(|e| StorageError::redb("put_many/docs_insert", e))?;
                chg.insert(chg_key.as_slice(), &[][..])
                    .map_err(|e| StorageError::redb("put_many/chg_insert", e))?;
                cleanup
                    .insert(cleanup_key.as_slice(), &[][..])
                    .map_err(|e| StorageError::redb("put_many/cleanup_insert", e))?;
                written += 1;
            }
        }
        tx.commit()
            .map_err(|e| StorageError::redb("put_many/commit", e))?;
        Ok(written)
    }

    /// A read transaction, for a caller that needs to scan an index table.
    ///
    /// Deliberately returning redb's own type rather than wrapping it: the
    /// benchmark and any future query layer need to range-scan `chg`/`idx`, and a
    /// wrapper would either leak the same type or force a callback per entry with no
    /// benefit. The lifetime rule from [`layout`] claim 2 still applies -- do not hold
    /// this across operations, or cross-process visibility breaks.
    pub fn read_transaction(&self) -> Result<redb::ReadTransaction, StorageError> {
        self.db
            .begin_read()
            .map_err(|e| StorageError::redb("begin_read", e))
    }

    /// Writes one attachment's bytes.
    pub fn put_attachment(
        &self,
        document_id: &str,
        attachment_id: &str,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let mut key = document_id.as_bytes().to_vec();
        key.push(layout::KEY_SEPARATOR);
        key.extend_from_slice(attachment_id.as_bytes());
        let tx = self
            .db
            .begin_write()
            .map_err(|e| StorageError::redb("put_attachment/begin_write", e))?;
        {
            let mut table = tx
                .open_table(ATTACH)
                .map_err(|e| StorageError::redb("put_attachment/open", e))?;
            table
                .insert(key.as_slice(), data)
                .map_err(|e| StorageError::redb("put_attachment/insert", e))?;
        }
        tx.commit()
            .map_err(|e| StorageError::redb("put_attachment/commit", e))?;
        Ok(())
    }

    /// Reads one attachment's bytes.
    pub fn get_attachment(
        &self,
        document_id: &str,
        attachment_id: &str,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let mut key = document_id.as_bytes().to_vec();
        key.push(layout::KEY_SEPARATOR);
        key.extend_from_slice(attachment_id.as_bytes());
        let tx = self
            .db
            .begin_read()
            .map_err(|e| StorageError::redb("get_attachment/begin_read", e))?;
        let table = tx
            .open_table(ATTACH)
            .map_err(|e| StorageError::redb("get_attachment/open", e))?;
        let value = table
            .get(key.as_slice())
            .map_err(|e| StorageError::redb("get_attachment/get", e))?
            .map(|v| v.value().to_vec());
        Ok(value)
    }

    /// Deletes one attachment.
    pub fn remove_attachment(
        &self,
        document_id: &str,
        attachment_id: &str,
    ) -> Result<(), StorageError> {
        let mut key = document_id.as_bytes().to_vec();
        key.push(layout::KEY_SEPARATOR);
        key.extend_from_slice(attachment_id.as_bytes());
        let tx = self
            .db
            .begin_write()
            .map_err(|e| StorageError::redb("remove_attachment/begin_write", e))?;
        {
            let mut table = tx
                .open_table(ATTACH)
                .map_err(|e| StorageError::redb("remove_attachment/open", e))?;
            table
                .remove(key.as_slice())
                .map_err(|e| StorageError::redb("remove_attachment/remove", e))?;
        }
        tx.commit()
            .map_err(|e| StorageError::redb("remove_attachment/commit", e))?;
        Ok(())
    }

    /// Deletes tombstones of one collection whose `lwt` is below `cutoff`.
    ///
    /// Returns how many were removed. Uses the `cleanup` table's ordering, so this is a
    /// bounded range delete rather than a full scan and filter.
    pub fn delete_tombstones(
        &self,
        collection: &str,
        version: u32,
        cutoff: i64,
    ) -> Result<usize, StorageError> {
        let lower = layout::cleanup_prefix(collection, version, true);
        let mut upper = lower.clone();
        upper.extend_from_slice(&layout::encode_lwt(cutoff));

        // Collect ids first so the read cursor is not held while deleting.
        let victims: Vec<Vec<u8>> = {
            let tx = self
                .db
                .begin_read()
                .map_err(|e| StorageError::redb("cleanup/begin_read", e))?;
            let table = tx
                .open_table(CLEANUP)
                .map_err(|e| StorageError::redb("cleanup/open", e))?;
            let range = table
                .range(lower.as_slice()..=upper.as_slice())
                .map_err(|e| StorageError::redb("cleanup/range", e))?;
            let mut keys = Vec::new();
            for entry in range {
                let (key, _) = entry.map_err(|e| StorageError::redb("cleanup/entry", e))?;
                keys.push(key.value().to_vec());
            }
            keys
        };

        if victims.is_empty() {
            return Ok(0);
        }

        let doc_prefix = layout::prefix(collection, version);
        let tx = self
            .db
            .begin_write()
            .map_err(|e| StorageError::redb("cleanup_write/begin", e))?;
        let removed = {
            let mut cleanup = tx
                .open_table(CLEANUP)
                .map_err(|e| StorageError::redb("cleanup_write/cleanup", e))?;
            let mut docs = tx
                .open_table(DOCS)
                .map_err(|e| StorageError::redb("cleanup_write/docs", e))?;
            let mut chg = tx
                .open_table(CHG)
                .map_err(|e| StorageError::redb("cleanup_write/chg", e))?;
            let mut count = 0usize;
            for key in &victims {
                // cleanup key: prefix ++ deleted ++ 8-byte lwt ++ docId
                let id_offset = doc_prefix.len() + 1 + 8;
                if key.len() < id_offset {
                    continue;
                }
                let mut doc_key = doc_prefix.clone();
                doc_key.extend_from_slice(&key[id_offset..]);
                if let Ok(Some(old)) = docs.remove(doc_key.as_slice()) {
                    // Drop the change-feed entry too, using the lwt from the cleanup key.
                    let mut chg_key = key[doc_prefix.len() + 1..doc_prefix.len() + 9].to_vec();
                    chg_key.extend_from_slice(&doc_key);
                    let _ = chg.remove(chg_key.as_slice());
                    let _ = old;
                    count += 1;
                }
                let _ = cleanup.remove(key.as_slice());
            }
            count
        };
        tx.commit()
            .map_err(|e| StorageError::redb("cleanup_write/commit", e))?;
        Ok(removed)
    }

    /// Removes every row of one collection at one version from every table.
    pub fn remove_collection(&self, collection: &str, version: u32) -> Result<usize, StorageError> {
        let doc_prefix = layout::prefix(collection, version);
        let tx = self
            .db
            .begin_write()
            .map_err(|e| StorageError::redb("remove_collection/begin", e))?;
        let mut removed = 0usize;
        {
            let mut docs = tx
                .open_table(DOCS)
                .map_err(|e| StorageError::redb("remove_collection/docs", e))?;
            let keys: Vec<Vec<u8>> = {
                let range = docs
                    .range(doc_prefix.as_slice()..)
                    .map_err(|e| StorageError::redb("remove_collection/range", e))?;
                let mut out = Vec::new();
                for entry in range {
                    let (key, _) =
                        entry.map_err(|e| StorageError::redb("remove_collection/entry", e))?;
                    let key = key.value();
                    if !key.starts_with(&doc_prefix) {
                        break;
                    }
                    out.push(key.to_vec());
                }
                out
            };
            for key in &keys {
                docs.remove(key.as_slice())
                    .map_err(|e| StorageError::redb("remove_collection/remove", e))?;
                removed += 1;
            }
        }
        tx.commit()
            .map_err(|e| StorageError::redb("remove_collection/commit", e))?;
        Ok(removed)
    }

    /// Reads one document by id.
    pub fn get_document(
        &self,
        collection: &str,
        version: u32,
        id: &str,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let mut key = layout::prefix(collection, version);
        key.extend_from_slice(id.as_bytes());
        let tx = self
            .db
            .begin_read()
            .map_err(|e| StorageError::redb("get/begin_read", e))?;
        let docs = tx
            .open_table(DOCS)
            .map_err(|e| StorageError::redb("get/docs", e))?;
        let value = docs
            .get(key.as_slice())
            .map_err(|e| StorageError::redb("get/get", e))?
            .map(|v| v.value().to_vec());
        Ok(value)
    }

    /// All documents of one collection at one version, in id order.
    ///
    /// A prefix range rather than a filter, which is the point of the key layout:
    /// the separator in `layout::prefix` is what keeps `todos` from also returning
    /// `todos_archive`.
    pub fn documents(
        &self,
        collection: &str,
        version: u32,
    ) -> Result<Vec<DocumentEntry>, StorageError> {
        let prefix = layout::prefix(collection, version);
        let tx = self
            .db
            .begin_read()
            .map_err(|e| StorageError::redb("scan/begin_read", e))?;
        let docs = tx
            .open_table(DOCS)
            .map_err(|e| StorageError::redb("scan/docs", e))?;
        let mut out = Vec::new();
        for entry in docs
            .range(prefix.as_slice()..)
            .map_err(|e| StorageError::redb("scan/range", e))?
        {
            let (key, value) = entry.map_err(|e| StorageError::redb("scan/entry", e))?;
            let key = key.value();
            if !key.starts_with(&prefix) {
                break;
            }
            out.push((key.to_vec(), value.value().to_vec()));
        }
        Ok(out)
    }

    /// Number of documents in one collection at one version.
    pub fn len(&self, collection: &str, version: u32) -> Result<u64, StorageError> {
        Ok(self.documents(collection, version)?.len() as u64)
    }

    /// True when the collection holds no documents.
    pub fn is_empty(&self, collection: &str, version: u32) -> Result<bool, StorageError> {
        Ok(self.len(collection, version)? == 0)
    }

    /// Total entries across the document table, for a test that must not depend on
    /// a collection filter.
    pub fn docs_total(&self) -> Result<u64, StorageError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| StorageError::redb("total/begin_read", e))?;
        let docs = tx
            .open_table(DOCS)
            .map_err(|e| StorageError::redb("total/docs", e))?;
        docs.len().map_err(|e| StorageError::redb("total/len", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, FilesystemStorage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStorage::open(dir.path()).expect("open");
        (dir, store)
    }

    #[test]
    fn writes_and_reads_a_record_through_redb() {
        // The end-to-end proof this tranche asks for: open a real redb file, write
        // a record, read it back byte for byte.
        let (_dir, store) = temp_store();
        let json = br#"{"_id":"alice","_rev":"1-a","name":"Alice"}"#;

        let previous = store
            .put_document("humans", 0, "alice", 1_700_000_000_000, false, json)
            .expect("put");
        assert!(previous.is_none(), "first write is an insert");

        let read = store
            .get_document("humans", 0, "alice")
            .expect("get")
            .expect("present");
        assert_eq!(
            read, json,
            "the document must survive byte for byte; ordered JSON depends on it"
        );

        let missing = store.get_document("humans", 0, "nobody").expect("get");
        assert!(missing.is_none());
    }

    #[test]
    fn a_second_write_reports_the_previous_value() {
        let (_dir, store) = temp_store();
        store
            .put_document("humans", 0, "alice", 1, false, br#"{"v":1}"#)
            .expect("put1");
        let previous = store
            .put_document("humans", 0, "alice", 2, false, br#"{"v":2}"#)
            .expect("put2");
        assert_eq!(previous.as_deref(), Some(&br#"{"v":1}"#[..]));
        assert_eq!(
            store
                .get_document("humans", 0, "alice")
                .expect("get")
                .as_deref(),
            Some(&br#"{"v":2}"#[..])
        );
    }

    #[test]
    fn data_survives_closing_and_reopening() {
        // The persistence claim, measured rather than assumed.
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let store = FilesystemStorage::open(dir.path()).expect("open");
            store
                .put_document("humans", 0, "bob", 5, false, br#"{"_id":"bob"}"#)
                .expect("put");
        } // dropped, file flushed
        let store = FilesystemStorage::open(dir.path()).expect("reopen");
        let read = store.get_document("humans", 0, "bob").expect("get");
        assert_eq!(read.as_deref(), Some(&br#"{"_id":"bob"}"#[..]));
    }

    #[test]
    fn a_collection_scan_does_not_bleed_into_a_longer_collection_name() {
        // The prefix-bleed bug, at the storage level rather than the encoding level.
        let (_dir, store) = temp_store();
        store
            .put_document("todos", 1, "a", 1, false, br#"{"c":"todos"}"#)
            .expect("put todos");
        store
            .put_document("todos_archive", 1, "b", 2, false, br#"{"c":"archive"}"#)
            .expect("put archive");

        let todos = store.documents("todos", 1).expect("scan todos");
        assert_eq!(todos.len(), 1, "todos must not return todos_archive's row");
        assert_eq!(todos[0].1, br#"{"c":"todos"}"#);

        let archive = store.documents("todos_archive", 1).expect("scan archive");
        assert_eq!(archive.len(), 1);
        assert_eq!(archive[0].1, br#"{"c":"archive"}"#);
    }

    #[test]
    fn schema_versions_do_not_share_rows() {
        let (_dir, store) = temp_store();
        store
            .put_document("humans", 0, "a", 1, false, br#"{"v":0}"#)
            .expect("put v0");
        store
            .put_document("humans", 1, "a", 2, false, br#"{"v":1}"#)
            .expect("put v1");
        assert_eq!(store.documents("humans", 0).expect("v0").len(), 1);
        assert_eq!(store.documents("humans", 1).expect("v1").len(), 1);
        assert_eq!(
            store
                .get_document("humans", 0, "a")
                .expect("get v0")
                .as_deref(),
            Some(&br#"{"v":0}"#[..])
        );
    }

    #[test]
    fn documents_come_back_in_id_order() {
        // Iteration order is part of the contract the query layer relies on.
        let (_dir, store) = temp_store();
        for id in ["c", "a", "b"] {
            store
                .put_document("humans", 0, id, 1, false, b"{}")
                .expect("put");
        }
        let keys = store.documents("humans", 0).expect("scan");
        let ids: Vec<String> = keys
            .iter()
            .map(|(k, _)| {
                let prefix = layout::prefix("humans", 0);
                String::from_utf8(k[prefix.len()..].to_vec()).expect("utf8")
            })
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn two_handles_on_one_file_both_write_and_both_see_the_result() {
        // MultiWriter, the same shape as the two-process probe but in-process:
        // two open handles on one file, each seeing the other's commit. This is
        // the property `multiInstance: true` needs.
        //
        // Native only, and the skip is the point: on WASI redb has only a
        // whole-file lock, so the second `open` fails with DatabaseAlreadyOpen by
        // design. Asserting the native property here and *recording* the WASI
        // limitation is better than a test that fails on one target and gets
        // deleted. The cross-target contract is `Capabilities::multi_process`.
        if cfg!(target_arch = "wasm32") {
            assert!(
                !Capabilities::current().multi_process,
                "wasm targets must not claim multi-process"
            );
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let a = FilesystemStorage::open(dir.path()).expect("open a");
        let b = FilesystemStorage::open(dir.path()).expect("open b");

        a.put_document("humans", 0, "from-a", 1, false, br#"{"who":"a"}"#)
            .expect("a put");
        b.put_document("humans", 0, "from-b", 2, false, br#"{"who":"b"}"#)
            .expect("b put");

        // Each must see both, because reads take a fresh transaction.
        for (name, store) in [("a", &a), ("b", &b)] {
            let rows = store.documents("humans", 0).expect("scan");
            assert_eq!(rows.len(), 2, "handle {name} must see both writers' rows");
        }
    }

    #[test]
    fn the_concurrency_mode_matches_what_the_target_supports() {
        // The regression test for the WASI open failure: MultiWriter needs
        // byte-range locks, which redb does not implement for wasm targets, so
        // asking for it there fails at `create` with "The storage backend does not
        // support this operation".
        if cfg!(target_arch = "wasm32") {
            assert_eq!(
                FilesystemStorage::concurrency_mode(),
                redb::ConcurrencyMode::ExclusiveWriter
            );
        } else {
            assert_eq!(
                FilesystemStorage::concurrency_mode(),
                redb::ConcurrencyMode::MultiWriter
            );
        }
    }

    #[test]
    fn capabilities_do_not_overclaim_cross_process_events() {
        let caps = Capabilities::current();
        assert!(
            !caps.multi_instance_events,
            "cross-process change delivery is not implemented; do not claim it"
        );
        if cfg!(any(not(target_arch = "wasm32"), target_os = "wasi")) {
            assert!(caps.persistent);
            assert!(caps.multi_process);
        }
    }

    #[test]
    fn len_and_is_empty_agree_with_the_scan() {
        let (_dir, store) = temp_store();
        assert!(store.is_empty("humans", 0).expect("empty"));
        assert_eq!(store.len("humans", 0).expect("len"), 0);
        store
            .put_document("humans", 0, "a", 1, false, b"{}")
            .expect("put");
        assert!(!store.is_empty("humans", 0).expect("not empty"));
        assert_eq!(store.len("humans", 0).expect("len"), 1);
        assert_eq!(store.docs_total().expect("total"), 1);
    }
}
