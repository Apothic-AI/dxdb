//! The backing: a flat file store, and the **explicit** choice of which OPFS API
//! implements it.
//!
//! # The constraint this module exists to make visible
//!
//! `FileSystemFileHandle.createSyncAccessHandle()` — and therefore the synchronous
//! `read`/`write`/`truncate`/`getSize` methods, and therefore the SAH pool that makes
//! OPFS fast — **exists only in a dedicated worker** (PLAN.md 5.4; MDN: "only available
//! in dedicated workers"; upstream's own OPFS docs say the same and ship a separate
//! main-thread storage because of it).
//!
//! So there are two real implementations and no way to have one:
//!
//! | path | API | cost |
//! | --- | --- | --- |
//! | [`AccessPath::SyncAccessHandle`] | `createSyncAccessHandle` in a worker | one syscall-ish call per read/write; values cross a `postMessage` boundary |
//! | [`AccessPath::AsyncWritable`] | `createWritable` anywhere | an `await` and a stream round trip per write |
//!
//! This crate does not paper over that. It picks the path from what the environment
//! actually offers, records which one it picked, and exposes it as
//! [`OpfsStorage::path`] so a caller (a test, a log line, a support report) can assert
//! on it instead of guessing. An "accidental" fallback — one nobody can observe — is
//! the failure mode this design exists to prevent.

use std::future::Future;

/// An error an OPFS backing can raise.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpfsError {
    /// OPFS is not available in this context at all.
    #[error("opfs is unavailable: {0}")]
    Unavailable(String),
    /// The write would exceed the origin's quota (`QuotaExceededError`).
    #[error("storage quota exceeded")]
    Quota,
    /// A file or directory that should exist does not.
    #[error("not found: {0}")]
    NotFound(String),
    /// Anything else, including a `DOMException` this tier does not classify.
    #[error("opfs error: {0}")]
    Other(String),
}

/// Which OPFS API the backing is using.
///
/// Printed, compared and asserted on. Deliberately not a bool: "sync" alone does not
/// say *why* it is sync, and a future third path (JSPI) should not need a new field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessPath {
    /// `createSyncAccessHandle`, only reachable inside a dedicated worker.
    SyncAccessHandle,
    /// `createWritable`, the async path, usable on the main thread.
    AsyncWritable,
    /// A non-OPFS in-memory backend, used by host tests.
    InMemory,
}

impl AccessPath {
    /// The upstream-shaped name, for a report or a log line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AccessPath::SyncAccessHandle => "opfs-sync-access-handle (dedicated worker)",
            AccessPath::AsyncWritable => "opfs-async-writable (main thread fallback)",
            AccessPath::InMemory => "in-memory (test backend)",
        }
    }

    /// Whether this is the fast path PLAN.md 5.4 describes.
    #[must_use]
    pub fn is_fast_path(self) -> bool {
        matches!(self, AccessPath::SyncAccessHandle)
    }
}

/// A flat, path-addressed file store.
///
/// Flat on purpose: OPFS is a directory tree, and the storage's on-disk layout is a
/// design decision that belongs in [`crate`], not in the backend. A backend only has to
/// store bytes under a string key.
pub trait FileStore {
    /// Which API this backend is using.
    fn path(&self) -> AccessPath;

    /// Creates (or confirms) a directory. Idempotent.
    fn create_dir(&self, dir: &str) -> impl Future<Output = Result<(), OpfsError>>;

    /// Reads a whole file. `None` when absent.
    fn read(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, OpfsError>>;

    /// Replaces a whole file's contents.
    fn write(&self, key: &str, bytes: &[u8]) -> impl Future<Output = Result<(), OpfsError>>;

    /// Removes a file. Removing an absent key succeeds.
    fn remove(&self, key: &str) -> impl Future<Output = Result<(), OpfsError>>;

    /// Every key currently present, in sorted order.
    ///
    /// Sorted so a caller that walks keys gets a deterministic order and a failing
    /// assertion prints a stable diff — the same reason [`crate::MemoryFiles`] uses a
    /// `BTreeMap`.
    fn keys(&self) -> impl Future<Output = Result<Vec<String>, OpfsError>>;

    /// Recursively removes a directory. Used by `remove()`.
    fn remove_dir(&self, dir: &str) -> impl Future<Output = Result<(), OpfsError>>;
}

// ---------------------------------------------------------------------------
// The in-memory backend, used by this crate's tests
// ---------------------------------------------------------------------------

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// A [`FileStore`] over a `BTreeMap`.
///
/// Public because the contract tests live in `tests/`, a separate crate that cannot see
/// `#[cfg(test)]` items. Nothing in [`crate::OpfsStorage`] constructs it.
#[derive(Clone, Default)]
pub struct MemoryFiles {
    files: Rc<RefCell<BTreeMap<String, Vec<u8>>>>,
    dirs: Rc<RefCell<Vec<String>>>,
    /// When set, every write fails with quota pressure.
    fail_writes: Rc<RefCell<bool>>,
}

impl MemoryFiles {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every subsequent write fail, simulating a full origin quota.
    pub fn fail_all_writes(&self) {
        *self.fail_writes.borrow_mut() = true;
    }

    /// Number of files stored.
    pub fn len(&self) -> usize {
        self.files.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every key, sorted.
    pub fn sorted_keys(&self) -> Vec<String> {
        self.files.borrow().keys().cloned().collect()
    }

    /// The raw bytes of one file, for a test that compares on-disk shape.
    pub fn raw(&self, key: &str) -> Option<Vec<u8>> {
        self.files.borrow().get(key).cloned()
    }

    /// Writes a file directly, bypassing the storage, so a test can represent a store
    /// written by something else.
    pub fn seed(&self, key: &str, bytes: Vec<u8>) {
        self.files.borrow_mut().insert(key.to_string(), bytes);
    }
}

impl FileStore for MemoryFiles {
    fn path(&self) -> AccessPath {
        AccessPath::InMemory
    }

    async fn create_dir(&self, dir: &str) -> Result<(), OpfsError> {
        let mut dirs = self.dirs.borrow_mut();
        if !dirs.iter().any(|existing| existing == dir) {
            dirs.push(dir.to_string());
        }
        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Option<Vec<u8>>, OpfsError> {
        Ok(self.files.borrow().get(key).cloned())
    }

    async fn write(&self, key: &str, bytes: &[u8]) -> Result<(), OpfsError> {
        if *self.fail_writes.borrow() {
            return Err(OpfsError::Quota);
        }
        self.files
            .borrow_mut()
            .insert(key.to_string(), bytes.to_vec());
        Ok(())
    }

    async fn remove(&self, key: &str) -> Result<(), OpfsError> {
        self.files.borrow_mut().remove(key);
        Ok(())
    }

    async fn keys(&self) -> Result<Vec<String>, OpfsError> {
        Ok(self.files.borrow().keys().cloned().collect())
    }

    async fn remove_dir(&self, dir: &str) -> Result<(), OpfsError> {
        let prefix = format!("{dir}/");
        self.files
            .borrow_mut()
            .retain(|key, _| !key.starts_with(&prefix));
        self.dirs.borrow_mut().retain(|existing| existing != dir);
        Ok(())
    }
}
