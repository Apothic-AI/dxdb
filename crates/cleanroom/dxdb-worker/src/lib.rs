//! `dxdb-worker`: a storage behind a message-passing isolate boundary.
//!
//! Implemented from `specs/worker.md`, which was written from public documentation before
//! any code existed. Provenance per crate is in `specs/READING-LIST.md`.
//!
//! # A worker is not a thread
//!
//! A browser `Worker`, a `SharedWorker` and a Node `worker_thread` are separate isolates
//! with their own linear memory. They communicate by message passing, which means **nothing
//! here needs atomics** and nothing needs `wasm32-wasip1-threads` or a nightly
//! `-Z build-std`. The whole crate lands on stable `wasm32-unknown-unknown`
//! (`docs/CLEANROOM-GAPS.md` G-048).
//!
//! The cost, stated because it is a trade rather than a win: every call and every result is
//! copied across the boundary, so a `query` returning 10 000 documents copies them, and
//! every call pays a message hop. That is worth paying to move encryption, query scanning
//! and index maintenance off the main thread — the documented benefit is *where the work
//! runs*, not shared memory.
//!
//! # The failure mode this crate exists for
//!
//! A worker can die mid-write. `bulk_write` is per-document atomic *inside* a storage, but
//! the caller cannot know which rows were applied when the isolate vanished. So writes are
//! **never retried**, and they fail with [`WorkerError::WorkerDiedDuringWrite`] carrying the
//! caller's context (`docs/CLEANROOM-GAPS.md` G-049). The recovery is to re-read and
//! reconcile, which is `dxdb-core`'s business, not the transport's.
//!
//! # What is not implemented yet
//!
//! This tranche lands the **protocol, the supervision state machine, and the transport
//! seam**, which is where every testable failure mode lives. The `RxStorageInstance`
//! wrapper and the platform adapters (browser `Worker`, Node `worker_threads`) are the
//! remaining pieces; `specs/worker.md` section 9 records why the adapters come second and
//! what each needs. Saying so here rather than shipping a wrapper that cannot be driven
//! keeps the crate honest about what is proven.

#![forbid(unsafe_code)]

pub mod protocol;
pub mod supervision;

pub use protocol::{
    negotiate, FromWorker, ProtocolMismatch, RequestKind, ToWorker, MIN_SUPPORTED, PROTOCOL,
    RESERVED_ID,
};
pub use supervision::{
    Counters, Liveness, Supervision, SupervisionSettings, TransportError, WorkerError,
    WorkerTransport,
};
