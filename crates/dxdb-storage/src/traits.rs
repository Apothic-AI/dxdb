//! `RxStorage` / `RxStorageInstance`: the two traits every storage implements.
//!
//! # Naming, versus upstream
//!
//! Method names are the upstream names in `snake_case`, one for one, so a port
//! of a call site is mechanical:
//!
//! | upstream | here |
//! | --- | --- |
//! | `bulkWrite` | [`RxStorageInstance::bulk_write`] |
//! | `findDocumentsById` | [`RxStorageInstance::find_documents_by_id`] |
//! | `query` | [`RxStorageInstance::query`] |
//! | `count` | [`RxStorageInstance::count`] |
//! | `getAttachmentData` | [`RxStorageInstance::get_attachment_data`] |
//! | `getChangedDocumentsSince` | [`RxStorageInstance::get_changed_documents_since`] |
//! | `changeStream` | [`RxStorageInstance::change_stream`] |
//! | `cleanup` | [`RxStorageInstance::cleanup`] |
//! | `close` / `remove` | [`RxStorageInstance::close`] / [`RxStorageInstance::remove`] |
//! | `createStorageInstance` | [`RxStorage::create_storage_instance`] |
//!
//! Three deliberate divergences, all of them forced by the language, all listed
//! here so no lane has to discover them by guessing:
//!
//! 1. **`internals` and `options` are not trait members.** Upstream exposes them
//!    as readonly fields, mostly so a wrapper or a test can inspect the engine.
//!    A generic associated type here would infect every wrapper with a type
//!    parameter for a debugging affordance. Instead: the creation options are
//!    opaque JSON in [`InstanceCreationParams::options`] (not retained by the
//!    trait), and [`RxStorageInstance::as_any`] provides the downcast that
//!    `internals` was used for.
//! 2. **`getAttachmentData` returns `Vec<u8>`, not a `Blob`.** Upstream's `Blob`
//!    is both the data and its type; here the type lives in the stored stub
//!    ([`crate::AttachmentData::content_type`]) and this returns the bytes.
//! 3. **`changeStream` returns a pinned boxed `Stream`** instead of an
//!    `Observable`, per PLAN.md 3.3 and D2.
//!
//! Everything else — the eight async methods, the optional ninth, the
//! `cleanup -> bool` contract, the per-document atomic write — is upstream's
//! contract verbatim.
//!
//! # Async shape
//!
//! Methods return `-> impl Future` rather than being `async fn`, per PLAN.md 3.3.
//! `#[dynosaur]` generates the dyn-compatible twin ([`DynRxStorageInstance`],
//! [`DynRxStorage`]) that the wrapper stack needs, so there is no hand-written
//! second trait and no `async_trait` allocation on the hot path. Nothing here is
//! `Send`: see the crate docs.

use std::any::Any;
use std::future::Future;

use dynosaur::dynosaur;

use crate::changes::{ChangeEvent, ChangeStream, Checkpoint};
use crate::document::{DocumentData, DocumentWriteData};
use crate::query::{CountMode, PreparedQuery};
use crate::schema::StorageSchema;
use crate::write::{BulkWriteResponse, BulkWriteRow};

/// Errors a storage raises outside the per-row write-error channel.
///
/// A conflict or a missing attachment payload is **not** here: those are
/// per-row results ([`crate::WriteError`]), because they do not fail the call.
/// What is here is the class of failure that makes the whole call meaningless —
/// the instance is gone, the plan names an index that does not exist, the input
/// is malformed.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum StorageError {
    /// The instance was closed and cannot be used.
    #[error("storage instance is closed")]
    Closed,
    /// The instance was removed; every other instance on the same collection is
    /// dead too (upstream `ensureNotRemoved`).
    #[error("storage instance was removed")]
    Removed,
    /// A document is not shaped like a document (not an object, or no primary key).
    #[error("invalid document: {0}")]
    InvalidDocument(String),
    /// The schema projection could not be read.
    #[error("invalid schema: {0}")]
    InvalidSchema(String),
    /// The query plan names an index this instance did not materialise. Upstream
    /// throws `index does not exist <name>`, and the conformance suite depends on
    /// that being an error rather than a silent full scan.
    #[error("index does not exist: {0}")]
    UnknownIndex(String),
    /// The attachment is absent, or its digest does not match the request.
    #[error("attachment does not exist: {0}")]
    MissingAttachment(String),
    /// A capability this storage does not provide was requested (encryption
    /// without the encryption wrapper, key compression without its plugin).
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// `findDocumentsById` result type, named for readability at call sites.
pub type QueryResult = Vec<DocumentData>;

/// `count()` result: the number plus whether it came from an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountResult {
    /// Number of matching, non-deleted documents.
    pub count: usize,
    /// How it was computed. `Slow` is what `allowSlowCount: false` rejects.
    pub mode: CountMode,
}

/// `getChangedDocumentsSince()` result.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangedDocuments {
    /// The current state of every document changed after the checkpoint, in
    /// `(_meta.lwt, primary)` order, at most `limit` of them.
    pub documents: Vec<DocumentData>,
    /// Where the next call continues from.
    pub checkpoint: Checkpoint,
}

/// A stored attachment's bytes, with the digest the caller asked for already
/// verified.
pub type FindAttachmentResult = Vec<u8>;

/// Everything a storage needs to open an instance.
///
/// `options` is opaque JSON: upstream parameterises it per storage
/// (`RxStorageMemoryInstanceCreationOptions` etc.) and almost every storage
/// accepts `{}`. Passing it untyped keeps the trait object-usable and keeps a
/// storage's private options out of the shared ABI.
///
/// **Not present, on purpose:** `password`. Encryption is a wrapper
/// (`wrappedKeyEncryptionCryptoJsStorage` upstream), so the plain storage never
/// sees a password, and a storage that could see one would be a place to leak it.
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceCreationParams {
    /// Identifies the `RxDatabase` object that owns this instance. Upstream uses
    /// it to key per-database caches such as the cross-tab channel; revisions use
    /// it as the second half of `_rev`.
    pub database_instance_token: String,
    /// Database name, part of the storage's collection key.
    pub database_name: String,
    /// Collection name, part of the storage's collection key.
    pub collection_name: String,
    /// The schema projection. Defaulting/hashing already happened in
    /// `dxdb-schema`.
    pub schema: StorageSchema,
    /// Storage-specific creation options, untyped.
    pub options: serde_json::Value,
    /// True when more than one instance of the database may exist (other tabs,
    /// other processes) and so changes must be propagated between instances.
    pub multi_instance: bool,
    /// True when the caller wants expensive integrity checks. Upstream's memory
    /// storage uses this to reject a re-open with a different schema.
    pub dev_mode: bool,
}

impl InstanceCreationParams {
    /// The key a storage uses to share state between instances of the same
    /// collection, mirroring `getMemoryCollectionKey`
    /// (`memory-helper.ts:18`): `database--memory--collection--memory--version`.
    ///
    /// The version is part of the key so two schema versions of one collection
    /// never share state; upstream has a conformance test for exactly that
    /// ("should not mix up documents stored with different schema versions").
    pub fn collection_key(&self) -> String {
        format!(
            "{db}--memory--{collection}--memory--{version}",
            db = self.database_name,
            collection = self.collection_name,
            version = self.schema.version
        )
    }
}

/// One collection's storage, for one schema version.
///
/// # Required method behaviour
///
/// * [`bulk_write`](RxStorageInstance::bulk_write) is per-document atomic with no
///   cross-document transaction, and reports per-row failures. See
///   [`BulkWriteResponse`].
/// * A write whose `previous` does not match the stored `_rev` is a
///   [`crate::WriteError::Conflict`] and must not be applied.
/// * Every event a write causes must be emitted **before** `bulk_write` resolves.
/// * [`cleanup`](RxStorageInstance::cleanup) returning `false` means "more work
///   remains, call me again".
#[dynosaur(pub DynRxStorageInstance = dyn(box) RxStorageInstance)]
pub trait RxStorageInstance {
    /// The database name this instance was created with.
    fn database_name(&self) -> &str;

    /// The collection name this instance was created with.
    fn collection_name(&self) -> &str;

    /// The schema projection this instance was created with.
    fn schema(&self) -> &StorageSchema;

    /// Escape hatch for storage-specific features, replacing upstream's
    /// `internals` field.
    ///
    /// A caller that needs `dxdb-memory`'s internals (a test, a debugging tool, a
    /// storage-specific plugin) downcasts; every other caller uses the trait.
    fn as_any(&self) -> &dyn Any;

    /// Where replication and migration should actually run, if this instance is a
    /// cache in front of a persistent one.
    ///
    /// Upstream's `underlyingPersistentStorage`, used by the memory-synced
    /// storage so a replication runs against the disk-backed instance instead of
    /// the in-memory copy. `None` for a normal storage.
    fn underlying_persistent_storage(&self) -> Option<&DynRxStorageInstance<'_>> {
        None
    }

    /// Writes several documents.
    ///
    /// Per-document atomic: some rows may be written while others fail, and the
    /// failures are returned in [`BulkWriteResponse::error`]. There is no
    /// transaction spanning the rows, and callers above this layer depend on
    /// that.
    fn bulk_write(
        &self,
        document_writes: Vec<BulkWriteRow>,
        context: String,
    ) -> impl Future<Output = Result<BulkWriteResponse, StorageError>>;

    /// Fetches documents by primary value.
    ///
    /// `with_deleted: false` filters out tombstones; `true` includes them (the
    /// conformance suite checks both directions, because a deleted document that
    /// is invisible to `findDocumentsById(true)` would make conflict resolution
    /// impossible).
    fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> impl Future<Output = Result<Vec<DocumentData>, StorageError>>;

    /// Runs a prepared query.
    ///
    /// The plan is a hint: a storage may ignore any part of it. What it may not
    /// do is return documents in a different order than the query's sort when the
    /// plan claims `sort_satisfied_by_index`, or omit the selector check when the
    /// plan claims `selector_satisfied_by_index`.
    fn query(
        &self,
        prepared: PreparedQuery,
    ) -> impl Future<Output = Result<Vec<DocumentData>, StorageError>>;

    /// Counts matching documents.
    ///
    /// Must equal the length of the equivalent `query()` — including with a
    /// `limit`, which upstream spells out: `.count({limit: 3})` must return what
    /// `.query({limit: 3}).documents.length` returns.
    fn count(
        &self,
        prepared: PreparedQuery,
    ) -> impl Future<Output = Result<CountResult, StorageError>>;

    /// Reads one attachment's bytes, verifying `digest`.
    ///
    /// A wrong digest is an error, not a mismatch to be reconciled: the digest is
    /// how a caller knows the bytes it received are the bytes it asked for.
    fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
        digest: String,
    ) -> impl Future<Output = Result<FindAttachmentResult, StorageError>>;

    /// Returns documents changed after `checkpoint`, in `(_meta.lwt, primary)`
    /// order, at most `limit` of them, each at its *current* state.
    ///
    /// # The documented fallback
    ///
    /// Upstream marks this method optional and defines the fallback in
    /// `rx-storage-helper.ts:936`: run the query
    /// [`changed_documents_since_query`] would build, then checkpoint on the last
    /// returned document.
    ///
    /// This trait goes one step further and *implements* the fallback as the
    /// default body, so a storage need not. It is exact rather than approximate
    /// because the natural index range for `(_meta.lwt, primary)` starting just
    /// after `(checkpoint.lwt, checkpoint.id)` already contains exactly the
    /// post-checkpoint documents, so no selector filtering is needed at all. A
    /// storage with a native change feed overrides this for performance.
    fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<Checkpoint>,
    ) -> impl Future<Output = Result<ChangedDocuments, StorageError>> {
        async move {
            let primary_path = self.schema().primary_path.clone();
            let start = checkpoint.clone().unwrap_or_else(Checkpoint::beginning);
            let mut prepared = crate::crate_changed_documents_since_query(
                &primary_path,
                &start,
                limit,
            );
            // The range is exact, so the plan may assert both properties and the
            // storage may skip per-document filtering entirely.
            prepared.query_plan.selector_satisfied_by_index = true;
            prepared.query_plan.sort_satisfied_by_index = true;
            let documents = self.query(prepared).await?;
            let checkpoint = match documents.last() {
                Some(last) => Checkpoint::new(
                    last.id(&primary_path)
                        .map_err(|e| StorageError::InvalidDocument(e.to_string()))?
                        .to_string(),
                    last.lwt().unwrap_or(start.lwt),
                ),
                None => start,
            };
            Ok(ChangedDocuments {
                documents,
                checkpoint,
            })
        }
    }

    /// The ongoing change feed.
    ///
    /// Dropping the stream unsubscribes. A storage that supports multiple
    /// instances on one collection must also forward changes made by the others
    /// (upstream's `rx-storage-multiinstance.ts`); the conformance suite has a
    /// test for it.
    fn change_stream(&self) -> ChangeStream;

    /// Purges tombstones older than `minimum_deleted_time` milliseconds.
    ///
    /// Returns `true` when everything cleanable has been cleaned, `false` when
    /// work remains and the caller should call again. A storage is free to purge
    /// in batches to avoid blocking; returning `true` after a partial pass would
    /// be a bug, because callers stop on `true`.
    fn cleanup(
        &self,
        minimum_deleted_time: f64,
    ) -> impl Future<Output = Result<bool, StorageError>>;

    /// Closes the instance. Calling it twice must not error (upstream's
    /// "closing multiple times should not error" test).
    fn close(&self) -> impl Future<Output = Result<(), StorageError>>;

    /// Closes the instance and deletes all of its data.
    ///
    /// Must error if the instance is already closed, but must *not* error if it
    /// was already removed ("should throw on call to .remove() after .close()"
    /// and "should NOT throw on call to .close() after .remove()" are two
    /// separate assertions upstream).
    fn remove(&self) -> impl Future<Output = Result<(), StorageError>>;
}

/// A storage engine: a factory for [`RxStorageInstance`]s.
#[dynosaur(pub DynRxStorage = dyn(box) RxStorage)]
pub trait RxStorage {
    /// Engine name, used to detect plugin/storage mismatches (`memory`,
    /// `dexie`, `sqlite`, ...).
    fn name(&self) -> &str;

    /// The RxDB version whose behaviour this storage implements. Part of the
    /// storage contract upstream so a mixed-version stack is diagnosable.
    fn rxdb_version(&self) -> &str {
        crate::RXDB_VERSION
    }

    /// Opens an instance.
    ///
    /// A storage may return `Err(StorageError::Unsupported)` here for a schema it
    /// cannot honour — encryption or key compression without their wrapper, a
    /// schema version it cannot migrate — rather than storing data it would then
    /// read back wrong.
    fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> impl Future<Output = Result<Box<DynRxStorageInstance<'static>>, StorageError>>;
}

/// Builds the query `get_changed_documents_since` paginates over.
///
/// Mirrors `getChangedDocumentsSinceQuery` (`rx-storage-helper.ts:889`) but
/// returns the *exact* plan rather than a selector, because the plan is what a
/// storage consumes. The bounds are:
///
/// * index `(_meta.lwt, primary)`;
/// * start `(checkpoint.lwt, checkpoint.id)`, exclusive — so the checkpoint
///   document itself is not returned again;
/// * end `(INDEX_MAX, INDEX_MAX)`, inclusive.
///
/// `limit` becomes the query limit, which a storage honours after ordering.
pub fn crate_changed_documents_since_query(
    primary_path: &str,
    checkpoint: &Checkpoint,
    limit: usize,
) -> PreparedQuery {
    use crate::query::{MangoQuery, PlanKey, QueryPlan, SortDirection, SortPart};

    let index = vec![
        "_meta.lwt".to_string(),
        primary_path.to_string(),
    ];
    let query = MangoQuery {
        // The range carries the whole predicate, so the selector is empty. It is
        // still an object, because a normalised selector always is.
        selector: serde_json::json!({}),
        sort: vec![
            SortPart {
                field: "_meta.lwt".to_string(),
                direction: SortDirection::Asc,
            },
            SortPart::asc(primary_path),
        ],
        skip: 0,
        limit: Some(limit),
        index: Some(index.clone()),
    };
    let query_plan = QueryPlan {
        index,
        sort_satisfied_by_index: true,
        selector_satisfied_by_index: true,
        start_keys: vec![
            PlanKey::Number(checkpoint.lwt),
            PlanKey::String(checkpoint.id.clone()),
        ],
        end_keys: vec![PlanKey::index_max(), PlanKey::index_max()],
        inclusive_start: false,
        inclusive_end: true,
    };
    PreparedQuery::new(query, query_plan)
}

/// Convenience: a write row for a brand-new document (no previous state).
pub fn insert_row(document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::insert(DocumentWriteData::new(document))
}

/// Convenience: a write row that replaces a known previous state.
pub fn update_row(previous: DocumentData, document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::update(previous, DocumentWriteData::new(document))
}

/// The event a single-document write produced, if any.
///
/// Storages emit bulks; a caller that wrote one row and wants "the event" uses
/// this rather than indexing into the bulk by position.
pub fn single_event(bulk: &crate::changes::EventBulk) -> Option<&ChangeEvent> {
    match bulk.events.len() {
        1 => bulk.events.first(),
        _ => None,
    }
}
