//! The storage trait layer: the spine of the system (PLAN.md section 4.2).
//!
//! Every wrapper feature — compression, encryption, sharding, worker, memory
//! tier, logging — is a type that implements [`RxStorageInstance`] over an inner
//! storage, which is also why the clean-room crates can plug in without touching
//! core.
//!
//! # What this crate contains
//!
//! | Item | State |
//! | --- | --- |
//! | [`DocumentData`], [`DocumentWriteData`], [`AttachmentData`], [`AttachmentWriteData`] | implemented |
//! | [`create_revision`], [`parse_revision`], [`get_height_of_revision`] | implemented |
//! | [`ChangeEvent`], [`EventBulk`], [`Checkpoint`], [`ChangeStream`] | implemented |
//! | [`BulkWriteRow`], [`BulkWriteResponse`], [`WriteError`] | implemented |
//! | [`MangoQuery`], [`QueryPlan`], [`PreparedQuery`], [`CountMode`] (the storage ABI) | implemented |
//! | [`StorageSchema`] (the schema projection a storage needs) | implemented |
//! | [`RxStorageInstance`] + dyn mirror [`DynRxStorageInstance`] | implemented |
//! | [`RxStorage`] + dyn mirror [`DynRxStorage`] | implemented |
//! | [`categorize_bulk_write_rows`] (PLAN.md 4.3's "heart of the consistency story") | implemented |
//! | [`Clock`] / [`now`] (D4) | implemented |
//! | [`Spawner`] / [`SequentialSpawner`] (D3) | implemented |
//!
//! There are no `todo!()` or `unimplemented!()` bodies in this crate. What is
//! deliberately absent is listed below.
//!
//! # Ownership boundary
//!
//! This crate is the **bottom of the dependency graph** (D1 in
//! `docs/DECISIONS.md`). It may depend on `dxdb-jsnum`, `dxdb-jsstr` and
//! `dxdb-json` and on nothing else from this workspace. The document type lives
//! here, not in `dxdb-core`, so that a storage (including a clean-room storage,
//! which must not depend on the port's core) can name the type it stores.
//!
//! # Deliberate omissions, and where they live instead
//!
//! * **Schema defaulting, meta injection, hashing, validation** — `dxdb-schema`.
//!   [`StorageSchema`] is only the projection a storage engine reads.
//! * **Index strings, the planner, selectors, sort comparators** — `dxdb-query`.
//!   This crate defines the *ABI* those produce ([`QueryPlan`], [`PreparedQuery`])
//!   and nothing about how they are computed.
//! * **Database/collection/document/hooks/live queries** — `dxdb-core`.
//! * **Replication, attachments protocol, plugins, JS bindings** — their lanes.
//!   Attachment *bytes* appear here only as what a storage persists.
//!
//! # Divergences from upstream, all named
//!
//! Method names are upstream's in `snake_case` one-for-one so a ported call site
//! is mechanical. Three shapes differ, forced by the language; see [`traits`] for
//! the table and the reasoning:
//!
//! 1. `internals` / `options` are not trait members — use [`RxStorageInstance::as_any`]
//!    to downcast, and [`InstanceCreationParams::options`] (opaque JSON) for
//!    storage-specific creation options.
//! 2. `getAttachmentData` returns `Vec<u8>`; the MIME type lives in the stored
//!    stub ([`AttachmentData::content_type`]).
//! 3. `changeStream` returns a pinned boxed `futures::Stream`, not an `Observable`.
//!
//! Plus one behavioural divergence, documented on
//! [`categorize_bulk_write_rows`]: an attachment whose digest changed without a
//! payload is a `510` rather than a silently inconsistent stub.
//!
//! # Documents are ordered JSON
//!
//! [`DocumentData`] wraps `serde_json::Value` with `preserve_order` on, because
//! key order is observable (PLAN.md 3.1): it reaches schema hashing, `_meta`, and
//! every index string. There is no generic document type parameter. Upstream's
//! `RxDocType` is a compile-time *view* of a JSON object the storage never
//! validates, so erasing it here is what the JS type does at runtime; it also keeps
//! the wrapper stack dyn-dispatchable without a type parameter on every wrapper.
//!
//! # Threading
//!
//! No method requires `Send` or `Sync`. RxDB has no threads: a database is
//! single-threaded and the platform binding supplies the executor (PLAN.md 3.3,
//! D3). Putting `Send` on these traits would exclude the browser storages, whose
//! handles are not `Send`.

#![forbid(unsafe_code)]

mod categorize;
mod changes;
mod clock;
mod document;
mod query;
mod revision;
mod schema;
mod spawner;
mod traits;
mod write;

pub use categorize::{
    categorize_bulk_write_rows, document_lwt, payloads_by_document, AttachmentAdd,
    AttachmentRemove, AttachmentUpdate, CategorizeBulkWriteRowsOutput, RowDecision,
    ATTACHMENT_STATUS, DELETED_FIELD, CONFLICT_STATUS, LWT_FIELD,
};
pub use changes::{ChangeEvent, ChangeStream, Checkpoint, EventBulk, Operation};
pub use clock::{
    advance_monotonic, format_lwt, lwt_from_parts, now, set_wall_clock_ms, Clock, ManualClock,
    MonotonicClock,
};
pub use document::{
    get_object_path, AttachmentData, AttachmentWriteData, DocumentData, DocumentWriteData,
    FIELD_ATTACHMENTS, FIELD_DELETED, FIELD_META, FIELD_REV, META_FIELD_LWT, META_FIELD_OWNER,
    META_LWT_MINIMUM, META_LWT_UNIX_TIME_MAX,
};
pub use query::{
    CountMode, MangoQuery, PlanKey, PreparedQuery, QueryPlan, SortDirection, SortPart, INDEX_MAX,
    INDEX_MIN,
};
pub use revision::{
    create_revision, get_height_of_revision, parse_revision, try_get_height_of_revision,
    ParsedRevision, RevisionError,
};
pub use schema::{index_name, StorageSchema};
pub use spawner::{BoxFuture, SequentialSpawner, Spawner};
pub use traits::{
    crate_changed_documents_since_query, insert_row, single_event, update_row, ChangedDocuments,
    CountResult, DynRxStorage, DynRxStorageInstance, FindAttachmentResult, InstanceCreationParams,
    QueryResult, RxStorage, RxStorageInstance, StorageError,
};
pub use write::{
    BulkWriteResponse, BulkWriteRow, BulkWriteRowProcessed, WriteError, WRITE_ERROR_ATTACHMENT,
    WRITE_ERROR_CONFLICT, WRITE_ERROR_VALIDATION,
};

/// The RxDB version whose behaviour this port tracks, exposed by every storage's
/// [`RxStorage::rxdb_version`] so a mixed-version stack is diagnosable.
pub const RXDB_VERSION: &str = "17.6.0";
