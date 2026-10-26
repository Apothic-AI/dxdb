//! `dxdb-meta-cache`: a validated snapshot of collection metadata for fast startup.
//!
//! Implemented from `specs/meta-cache.md`, which was written from public documentation
//! before any code existed. Provenance per crate is in `specs/READING-LIST.md`.
//!
//! # What this accelerates
//!
//! Upstream's metadata optimizer exists because RxDB's startup reads *N* collection
//! documents plus a storage-token document from the internal store before it can open
//! anything. This crate turns that into **one** read: a single generation document,
//! plus a synchronous `localStorage` read.
//!
//! # The divergence that matters
//!
//! The documented upstream design "replace[s] that with a localstorage based instance"
//! — `localStorage` becomes authoritative. dxdb does **not** do that
//! (`docs/CLEANROOM-GAPS.md` G-041). `localStorage` is evictable (Safari's scripted-storage
//! cap, private browsing, "clear site data") and readable by any script on the origin, so
//! making it the collection registry would turn a cache clear into user data that is
//! intact but unopenable. Here the real store stays authoritative and `localStorage`
//! holds a **validated snapshot**.
//!
//! It also means this crate is **not** wire-compatible with upstream's meta-optimized
//! databases (G-002): if upstream keeps its registry in `localStorage`, there is no
//! internal store here for us to cache.
//!
//! # The one idea
//!
//! Validating a snapshot normally costs the reads you were avoiding. The generation
//! counter fixes that: one small document whose value changes on every metadata
//! mutation, compared by equality. See [`MetaCache::open`] and section 3.3 of the spec
//! for the write-ordering rule that makes the only possible error direction "discard a
//! good snapshot" rather than "accept a stale one".

#![forbid(unsafe_code)]

pub mod generation;
pub mod snapshot;
mod wrapper;

pub use generation::{GenerationDoc, GENERATION_KEY, SNAPSHOT_FORMAT, SNAPSHOT_KEY_PREFIX};
pub use snapshot::{snapshot_key, Snapshot, StaleReason};
pub use wrapper::{
    CacheOutcome, MetaCache, MetaCacheSettings, MetadataLedger, MetadataWrite, RejectReason,
};

/// The string-keyed store the snapshot lives in.
///
/// Re-exported from `dxdb-localstorage` rather than declared here, because that crate
/// already defines `localStorage`'s exact signatures (including a missing key being
/// `Option` rather than an error) and a browser build wires the same object to both.
pub use dxdb_localstorage::{Kv as SnapshotStore, KvError as SnapshotError, MemoryKv};

/// Everything this crate can fail with.
///
/// Deliberately short: the cache absorbs almost every failure into "no snapshot"
/// (`INV-7`). The only genuine errors are ones the *caller* caused — asking for a
/// metadata read on a closed cache, or handing over a write that cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetaCacheError {
    /// The cache was closed and cannot serve reads.
    #[error("the metadata cache is closed")]
    Closed,
    /// The underlying metadata store failed in a way the cache cannot absorb.
    #[error("metadata store error: {0}")]
    Store(String),
    /// A metadata write could not be applied to the store.
    #[error("metadata write was rejected: {0}")]
    WriteRejected(String),
}
