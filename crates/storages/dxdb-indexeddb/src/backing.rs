//! The backing store: what an IndexedDB database has to provide, and nothing more.
//!
//! Same discipline as `dxdb-localstorage`'s `Kv`, for the same reason: the storage
//! logic is written **once** and the browser supplies a real `web_sys` implementation,
//! while the host tests supply an in-memory one. The code that runs in the browser is
//! the code the tests exercise — there is no browser-only branch to hide a bug in.
//!
//! Unlike `localStorage`, IndexedDB is genuinely asynchronous, so every method here
//! returns a `Future`. That is the honest shape, and it is also what makes this
//! storage a real test of the trait's async form.
//!
//! # Why `get_index_batch` and not `get_all(range)`
//!
//! The brief requires "batched cursor reads with `getAll` over explicit key ranges".
//! Exposing a *batch* read rather than a whole-range read is what lets the storage
//! stop early: a query with `limit: 3` must not materialise 10,000 index entries, and
//! `getAll(range, count)` is exactly the batching primitive IDB provides. The in-memory
//! implementation therefore has the same shape, so the early-stop logic is tested
//! rather than assumed.

use std::future::Future;

use serde_json::Value;

/// An error a backing store can raise.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdbError {
    /// IndexedDB is not available in this context (private mode, blocked storage).
    #[error("indexeddb is unavailable: {0}")]
    Unavailable(String),
    /// The write would exceed the origin's quota (`QuotaExceededError`).
    #[error("storage quota exceeded")]
    Quota,
    /// Anything else, including a `DOMException` this tier does not classify.
    #[error("indexeddb error: {0}")]
    Other(String),
}

/// The operations the storage needs from an IndexedDB-like database.
///
/// `store` is the database name, which carries the schema version. A real
/// implementation opens one IDB database per name; the in-memory one keys a map by it.
pub trait BackingStore {
    /// Opens (creating if absent) the database `store`.
    fn open(&self, store: &str) -> impl Future<Output = Result<(), IdbError>>;

    /// Reads documents by primary key. Missing keys are simply absent from the result.
    fn get_docs(
        &self,
        store: &str,
        ids: &[String],
    ) -> impl Future<Output = Result<Vec<Value>, IdbError>>;

    /// One batch of `(indexRowKey, primaryKey)` pairs, starting at `cursor`, in key
    /// order, at most `limit` of them.
    ///
    /// An empty result means the range is exhausted. `cursor` is exclusive: a caller
    /// passes the last key it saw plus a `\u{0}` suffix, which is how the batching loop
    /// advances without an IDB cursor object.
    fn get_index_batch(
        &self,
        store: &str,
        index_name: &str,
        cursor: &str,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<(String, String)>, IdbError>>;

    /// Writes documents and their index entries in **one transaction**.
    ///
    /// One transaction matters: an index entry pointing at a document that is not
    /// there makes a query return a key it cannot resolve, and the conformance suite
    /// writes concurrently. IDB gives us cross-store atomicity for free here.
    fn commit(
        &self,
        store: &str,
        docs: Vec<(String, Value)>,
        index_puts: Vec<(String, String)>,
        index_deletes: Vec<String>,
    ) -> impl Future<Output = Result<(), IdbError>>;

    /// Deletes documents and their index entries.
    fn delete_docs(
        &self,
        store: &str,
        ids: &[String],
    ) -> impl Future<Output = Result<(), IdbError>>;

    /// Stores attachment payloads, base64-encoded by the caller.
    fn put_attachments(
        &self,
        store: &str,
        attachments: &[(String, Vec<u8>)],
    ) -> impl Future<Output = Result<(), IdbError>>;

    /// Reads one attachment payload.
    fn get_attachment(
        &self,
        store: &str,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, IdbError>>;

    /// Deletes attachment payloads by key.
    fn delete_attachments(
        &self,
        store: &str,
        keys: &[String],
    ) -> impl Future<Output = Result<(), IdbError>>;

    /// Deletes the whole database (`remove()`).
    fn drop_store(&self, store: &str) -> impl Future<Output = Result<(), IdbError>>;
}

// ---------------------------------------------------------------------------
// The in-memory backend, used by this crate's tests
// ---------------------------------------------------------------------------

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// One database's contents.
///
/// `docs` is a `BTreeMap` so iteration is deterministic and a failing assertion prints
/// a stable diff. `index` is a `BTreeMap<indexRowKey, primaryKey>`, which is the crucial
/// part: iterating it in key order *is* iterating an index in encoded-index-string
/// order, because the encoded string is the middle component of the key. That is why
/// the batching loop can be tested here and behave identically against real IDB.
#[derive(Default)]
struct StoreContents {
    docs: BTreeMap<String, Value>,
    index: BTreeMap<String, String>,
    attachments: BTreeMap<String, Vec<u8>>,
}

/// An in-memory `BackingStore`.
///
/// Not compiled into a browser bundle in practice — nothing in `IndexedDbStorage`
/// constructs it — but public because the contract tests live in `tests/`, a separate
/// crate that cannot see `#[cfg(test)]` items.
#[derive(Clone, Default)]
pub struct MemoryBacking {
    stores: Rc<RefCell<BTreeMap<String, StoreContents>>>,
    /// When set, every write fails with `Quota`, to exercise the quota path.
    fail_writes: Rc<RefCell<bool>>,
}

impl MemoryBacking {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every subsequent write fail, simulating a full origin quota.
    pub fn fail_all_writes(&self) {
        *self.fail_writes.borrow_mut() = true;
    }

    /// Number of documents in `store`.
    pub fn doc_count(&self, store: &str) -> usize {
        self.stores
            .borrow()
            .get(store)
            .map(|contents| contents.docs.len())
            .unwrap_or(0)
    }

    /// Number of index rows in `store`, across all indexes.
    pub fn index_row_count(&self, store: &str) -> usize {
        self.stores
            .borrow()
            .get(store)
            .map(|contents| contents.index.len())
            .unwrap_or(0)
    }

    /// Every document id in `store`, sorted.
    pub fn doc_ids(&self, store: &str) -> Vec<String> {
        self.stores
            .borrow()
            .get(store)
            .map(|contents| contents.docs.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The raw stored JSON of one document, for a migration assertion that compares
    /// bytes rather than parsed values.
    pub fn raw_doc(&self, store: &str, id: &str) -> Option<Value> {
        self.stores
            .borrow()
            .get(store)
            .and_then(|contents| contents.docs.get(id).cloned())
    }

    /// Seeds a document directly, bypassing the storage, so a test can represent a
    /// store *written by something else* (the migration fixture case).
    pub fn seed_doc(&self, store: &str, id: &str, document: Value) {
        let mut stores = self.stores.borrow_mut();
        let contents = stores.entry(store.to_string()).or_default();
        contents.docs.insert(id.to_string(), document);
    }

    fn with_store<T>(
        &self,
        store: &str,
        f: impl FnOnce(&mut StoreContents) -> T,
    ) -> Result<T, IdbError> {
        let mut stores = self.stores.borrow_mut();
        let contents = stores.entry(store.to_string()).or_default();
        Ok(f(contents))
    }

    fn check_writable(&self) -> Result<(), IdbError> {
        if *self.fail_writes.borrow() {
            return Err(IdbError::Quota);
        }
        Ok(())
    }
}

impl BackingStore for MemoryBacking {
    async fn open(&self, store: &str) -> Result<(), IdbError> {
        self.with_store(store, |_| ())
    }

    async fn get_docs(&self, store: &str, ids: &[String]) -> Result<Vec<Value>, IdbError> {
        self.with_store(store, |contents| {
            ids.iter()
                .filter_map(|id| contents.docs.get(id).cloned())
                .collect()
        })
    }

    async fn get_index_batch(
        &self,
        store: &str,
        index_name: &str,
        cursor: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, IdbError> {
        self.with_store(store, |contents| {
            let prefix = format!("{index_name}|");
            contents
                .index
                .range(cursor.to_string()..)
                .filter(|(key, _)| key.starts_with(&prefix))
                .take(limit)
                .map(|(key, primary)| (key.clone(), primary.clone()))
                .collect()
        })
    }

    async fn commit(
        &self,
        store: &str,
        docs: Vec<(String, Value)>,
        index_puts: Vec<(String, String)>,
        index_deletes: Vec<String>,
    ) -> Result<(), IdbError> {
        self.check_writable()?;
        self.with_store(store, |contents| {
            for key in index_deletes {
                contents.index.remove(&key);
            }
            for (key, primary) in index_puts {
                contents.index.insert(key, primary);
            }
            for (id, document) in docs {
                contents.docs.insert(id, document);
            }
        })
    }

    async fn delete_docs(&self, store: &str, ids: &[String]) -> Result<(), IdbError> {
        self.check_writable()?;
        self.with_store(store, |contents| {
            for id in ids {
                contents.docs.remove(id);
                // Drop every index row that points at the document, so a deleted
                // document cannot leave a dangling key behind.
                contents.index.retain(|_, primary| primary != id);
            }
        })
    }

    async fn put_attachments(
        &self,
        store: &str,
        attachments: &[(String, Vec<u8>)],
    ) -> Result<(), IdbError> {
        self.check_writable()?;
        self.with_store(store, |contents| {
            for (key, data) in attachments {
                contents.attachments.insert(key.clone(), data.clone());
            }
        })
    }

    async fn get_attachment(&self, store: &str, key: &str) -> Result<Option<Vec<u8>>, IdbError> {
        self.with_store(store, |contents| contents.attachments.get(key).cloned())
    }

    async fn delete_attachments(&self, store: &str, keys: &[String]) -> Result<(), IdbError> {
        self.check_writable()?;
        self.with_store(store, |contents| {
            for key in keys {
                contents.attachments.remove(key);
            }
        })
    }

    async fn drop_store(&self, store: &str) -> Result<(), IdbError> {
        self.stores.borrow_mut().remove(store);
        Ok(())
    }
}
