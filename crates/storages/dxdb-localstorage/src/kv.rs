//! The `Kv` abstraction: everything a key-value store has to provide, and
//! nothing more.
//!
//! `localStorage` is synchronous, string-only, and has no transactions. Rather
//! than write the storage twice — once against `web_sys::Storage` and once
//! against a test fake — the storage logic is written once against this trait and
//! the browser supplies a `web_sys` implementation.
//!
//! That is what makes the host contract tests meaningful: they run the *same*
//! `LocalStorageInstance` code the browser runs, over a different `Kv`. A bug in
//! the storage's index maintenance or conflict handling cannot hide behind "that
//! path only runs in the browser", because it is not a different path.
//!
//! # Why synchronous, when the trait is async
//!
//! The storage trait returns `impl Future`, and `localStorage` has no async API to
//! await — every operation is synchronous and blocking. So the futures here are
//! immediately ready. That is not a shortcut: it faithfully models the platform,
//! and it is exactly why `localStorage` is the right first storage to port. If the
//! trait had required a suspension point that `localStorage` cannot provide, that
//! conflict would surface here — cheaply — rather than in `dxdb-opfs`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// An error a `Kv` backend can raise.
///
/// Deliberately tiny. `localStorage` fails in exactly two interesting ways: the
/// quota is exhausted (`QuotaExceededError`), or the store is unavailable
/// (Safari in a partitioned/blocked context throws on access while leaving the
/// global defined). Everything else is a programmer error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KvError {
    /// The backing store rejected the write because it would exceed its quota.
    /// Upstream throws `QuotaExceededError` here and the docs tell applications to
    /// handle it by showing the user a message, so it is a first-class variant
    /// rather than an opaque string.
    #[error("storage quota exceeded")]
    QuotaExceeded,
    /// The backing store is not available in this context.
    #[error("storage is unavailable: {0}")]
    Unavailable(String),
    /// Anything else the backend wants to report.
    #[error("kv error: {0}")]
    Other(String),
}

/// A string-keyed, string-valued store.
///
/// The signatures mirror `localStorage` on purpose, including `get` returning
/// `Option<String>` rather than a `Result` for a missing key: a missing key is not
/// an error in a KV store, and making it one would push a `?` into every read.
pub trait Kv {
    /// Reads a key. `None` when absent.
    fn get_item(&self, key: &str) -> Result<Option<String>, KvError>;

    /// Writes a key.
    fn set_item(&self, key: &str, value: &str) -> Result<(), KvError>;

    /// Removes a key. Removing an absent key succeeds, as in `localStorage`.
    fn remove_item(&self, key: &str) -> Result<(), KvError>;

    /// Every key currently present. Used by `cleanup` and by `remove`, which have
    /// to find an instance's keys without a prefix scan primitive.
    ///
    /// `localStorage` exposes `length` + `key(i)`; a `HashMap` backend can hand
    /// this over directly. Requiring it is cheaper for a storage than pretending
    /// the platform has a prefix iterator it does not have.
    fn keys(&self) -> Result<Vec<String>, KvError>;
}

// ---------------------------------------------------------------------------
// The in-memory backend, used by this crate's tests and nowhere else
// ---------------------------------------------------------------------------

/// A `Kv` backed by a `HashMap`, shared between handles via `Rc<RefCell<..>>` so a
/// test can open two instances on one store and observe the multi-instance
/// behaviour that a real browser gives for free.
///
/// Exposed publicly because the contract tests live in `tests/`, which is a
/// separate crate from this one and therefore cannot see a `#[cfg(test)]` item.
/// Nothing in `LocalStorageStorage` constructs it, so it contributes no reachable
/// code to a browser bundle that only opens the real `WebStorage`.
#[derive(Clone, Default)]
pub struct MemoryKv {
    inner: Rc<RefCell<HashMap<String, String>>>,
    /// When set, every write fails with `QuotaExceeded`. Tests use it to prove the
    /// quota path is handled rather than panicking, which is the failure mode a
    /// browser user actually hits.
    fail_writes: Rc<RefCell<bool>>,
}

impl MemoryKv {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every subsequent write fail, simulating a full quota.
    pub fn fail_all_writes(&self) {
        *self.fail_writes.borrow_mut() = true;
    }

    /// How many keys are stored. For test assertions about cleanup.
    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every key, sorted, so a test failure prints a stable diff.
    pub fn sorted_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.inner.borrow().keys().cloned().collect();
        keys.sort();
        keys
    }
}

impl Kv for MemoryKv {
    fn get_item(&self, key: &str) -> Result<Option<String>, KvError> {
        Ok(self.inner.borrow().get(key).cloned())
    }

    fn set_item(&self, key: &str, value: &str) -> Result<(), KvError> {
        if *self.fail_writes.borrow() {
            return Err(KvError::QuotaExceeded);
        }
        self.inner
            .borrow_mut()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn remove_item(&self, key: &str) -> Result<(), KvError> {
        self.inner.borrow_mut().remove(key);
        Ok(())
    }

    fn keys(&self) -> Result<Vec<String>, KvError> {
        Ok(self.inner.borrow().keys().cloned().collect())
    }
}
