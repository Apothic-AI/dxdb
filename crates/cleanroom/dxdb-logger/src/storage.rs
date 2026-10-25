//! The storage contract this crate wraps.
//!
//! This is an explicit *specification envelope* over the operation set that the
//! public documentation lists for the logger plugin, mirroring the Apache-2.0
//! interface (`src/types/rx-storage.interface.d.ts`, methods at lines 164, 180,
//! 208, 220, 227, 246, 288, 308, 314) one-to-one in argument and return shape.
//!
//! It exists because `dxdb-storage` had not published its own traits when stage
//! B1 ran (`docs/CLEANROOM-GAPS.md` G-020). Deliberately *not* a second
//! public-contract storage trait: when lane D lands the real traits, a bridge
//! implements them over this envelope and this module becomes the adapter.
//!
//! Documents, queries and checkpoints are opaque here. The logger must not be
//! able to inspect them, which is how payload transparency (`INV-1`) is enforced
//! by the type system rather than by reviewer attention.
//!
//! Futures are boxed because the wrapper must be usable as
//! `Arc<dyn LoggedStorageInstance>` in a storage stack (the port's wrapper
//! pattern, PLAN.md section 4.2), and because a boxed future is the one shape
//! that compiles identically on every target in the WASM matrix.

use std::future::Future;
use std::pin::Pin;

/// Collection names that mark a storage instance as internal.
pub mod meta {
    /// RxDB's own meta storage instance collection name.
    ///
    /// Taken from the Apache-2.0 source (`src/rx-storage-helper.ts:51`, used at
    /// `src/rx-database.ts:769`); see `docs/CLEANROOM-GAPS.md` G-024.
    pub const INTERNAL_COLLECTION_NAME: &str = "_rxdb_internal";

    /// The local-documents storage instance collection name.
    ///
    /// Taken from the Apache-2.0 source (`src/rx-storage-helper.ts:52`). The
    /// pinned tree declares the constant but the port has no local-documents
    /// plugin yet, so this is a forward-compatible entry, not a live one.
    pub const LOCAL_DOCUMENTS_COLLECTION_NAME: &str = "rxdatabase_storage_local";

    /// Whether `collection_name` names an internal ("meta") storage instance.
    ///
    /// This is the is-it-meta test behind the documented
    /// `settings.metaStorageInstances` filter (`docs/CLEANROOM-GAPS.md` G-024).
    pub fn is_meta_collection(collection_name: &str) -> bool {
        collection_name == INTERNAL_COLLECTION_NAME
            || collection_name == LOCAL_DOCUMENTS_COLLECTION_NAME
    }
}

/// A boxed future borrowed from `self`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A stored document, opaque to this crate.
///
/// The wrapper forwards it by value. Nothing in this crate reads, rewrites or
/// logs its contents.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDocument(serde_json::Value);

impl RawDocument {
    /// Wraps a JSON value as a stored document.
    pub fn from_value(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// The document as JSON. Callers above the wrapper use this; the logger does
    /// not.
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    /// Unwraps into the JSON value.
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }
}

/// One row of a `bulkWrite` call.
#[derive(Debug, Clone, PartialEq)]
pub struct RawWriteRow {
    /// The document to write.
    pub document: RawDocument,
    /// The previously stored revision, when the caller supplied one.
    pub previous: Option<RawDocument>,
}

/// A per-document write failure reported by a successful `bulkWrite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteErrorKind {
    /// The write conflicts with a newer stored revision.
    Conflict,
    /// The document failed validation.
    InvalidDocument,
    /// The attachment data was rejected.
    Attachment,
    /// Anything else the inner storage reports.
    Other,
}

/// One per-document write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteError {
    /// The primary key of the failing document.
    pub document_id: String,
    /// Why it failed.
    pub kind: WriteErrorKind,
}

/// The result of `bulkWrite`. Partial success is normal, as upstream documents:
/// "It must be possible that some document writes succeed and others error."
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BulkWriteResponse {
    /// Every per-document failure.
    pub error: Vec<WriteError>,
}

/// A normalized query, opaque to this crate beyond the fields used for logging.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreparedQuery {
    /// The selector, serialized. Not inspected.
    pub selector_json: String,
    /// The sort, serialized. Not inspected.
    pub sort_json: String,
    /// The index the query asked for.
    pub index: Option<Vec<String>>,
    /// Documents to skip.
    pub skip: usize,
    /// Maximum documents to return.
    pub limit: Option<usize>,
}

/// The result of `query`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryResult {
    /// Matching documents, in the query's sort order.
    pub documents: Vec<RawDocument>,
}

/// The result of `count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CountResult {
    /// The number of matching non-deleted documents.
    pub count: usize,
}

/// A replication checkpoint, opaque to this crate.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint(serde_json::Value);

impl Checkpoint {
    /// Wraps a JSON value as a checkpoint.
    pub fn from_value(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// The checkpoint as JSON.
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

/// The result of `getChangedDocumentsSince`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedDocuments {
    /// The changed documents.
    pub documents: Vec<RawDocument>,
    /// The checkpoint to continue from.
    pub checkpoint: Checkpoint,
}

/// Attachment bytes and their declared type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentData {
    /// The attachment bytes.
    pub bytes: Vec<u8>,
    /// The attachment's content type.
    pub content_type: String,
}

/// The informational report `info` returns, for storages that support it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InfoReport {
    /// The storage engine's own name.
    pub storage_name: String,
    /// The document count, when the engine can supply one cheaply.
    pub document_count: Option<usize>,
}

/// Coarse error classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageErrorKind {
    /// The requested document or attachment does not exist.
    NotFound,
    /// The write conflicts with a newer revision.
    Conflict,
    /// The storage instance is closed.
    Closed,
    /// The operation is not supported by this storage.
    NotSupported,
    /// The caller supplied an invalid argument.
    InvalidArgument,
    /// Anything else.
    Other,
}

impl StorageErrorKind {
    /// A stable, lower-case identifier for logs and span data.
    pub const fn as_str(self) -> &'static str {
        match self {
            StorageErrorKind::NotFound => "not_found",
            StorageErrorKind::Conflict => "conflict",
            StorageErrorKind::Closed => "closed",
            StorageErrorKind::NotSupported => "not_supported",
            StorageErrorKind::InvalidArgument => "invalid_argument",
            StorageErrorKind::Other => "other",
        }
    }
}

/// The error type the envelope carries.
///
/// The wrapper adds exactly one error of its own, and it lives in
/// [`crate::sentry::LogError`], not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError {
    /// The coarse classification.
    pub kind: StorageErrorKind,
    /// The human-readable message.
    pub message: String,
}

impl StorageError {
    /// Builds an error.
    pub fn new(kind: StorageErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The error reported for an optional operation the inner storage lacks.
    ///
    /// Reporting is the whole point: the storage layer above interprets this as
    /// "use your documented fallback" instead of receiving a fabricated result
    /// (`docs/CLEANROOM-GAPS.md` G-023).
    pub fn not_supported(operation: &str) -> Self {
        Self::new(
            StorageErrorKind::NotSupported,
            format!("operation {operation} is not supported by this storage"),
        )
    }

    /// The error reported for a closed instance.
    pub fn closed() -> Self {
        Self::new(StorageErrorKind::Closed, "storage instance is closed")
    }

    /// The error reported for an invalid argument.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(StorageErrorKind::InvalidArgument, message)
    }

    /// An unclassified error.
    pub fn other(message: impl Into<String>) -> Self {
        Self::new(StorageErrorKind::Other, message)
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = self.kind.as_str();
        let message = &self.message;
        write!(f, "{kind}: {message}")
    }
}

impl std::error::Error for StorageError {}

/// The parameters a storage factory receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceParams {
    /// The database name.
    pub database_name: String,
    /// The collection name.
    pub collection_name: String,
    /// Whether more than one process or tab may open the same data.
    pub multi_instance: bool,
}

impl InstanceParams {
    /// Parameters for one collection.
    pub fn new(database_name: impl Into<String>, collection_name: impl Into<String>) -> Self {
        Self {
            database_name: database_name.into(),
            collection_name: collection_name.into(),
            multi_instance: false,
        }
    }

    /// Sets `multi_instance`.
    pub const fn with_multi_instance(mut self, multi_instance: bool) -> Self {
        self.multi_instance = multi_instance;
        self
    }
}

/// A storage engine: a factory for instances.
///
/// Mirrors upstream `RxStorage`: a name and `createStorageInstance`. The wrapped
/// storage reports [`crate::WRAPPED_STORAGE_NAME`] so a caller can tell, from the
/// storage alone, that logging is in the stack.
pub trait LoggedStorage: Send + Sync {
    /// The instance type this storage creates.
    type Instance: LoggedStorageInstance + Send + Sync + 'static;

    /// The engine's own name.
    fn storage_name(&self) -> &str;

    /// Creates an instance.
    fn create_instance(&self, params: InstanceParams) -> Result<Self::Instance, StorageError>;
}

/// One collection's storage instance.
///
/// Every method is the documented operation of the same name. The two optional
/// operations report support through `supports_*` and default to not-supported,
/// matching upstream, where `getChangedDocumentsSince` is optional and the layer
/// above supplies a fallback (`src/rx-storage-helper.ts:936-960`).
pub trait LoggedStorageInstance: Send + Sync {
    /// Writes documents. Per-document atomic, partial success allowed.
    fn bulk_write(
        &self,
        rows: Vec<RawWriteRow>,
        context: String,
    ) -> BoxFuture<'_, Result<BulkWriteResponse, StorageError>>;

    /// Reads documents by primary key.
    fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> BoxFuture<'_, Result<Vec<RawDocument>, StorageError>>;

    /// Runs a prepared query.
    fn query(&self, prepared: PreparedQuery) -> BoxFuture<'_, Result<QueryResult, StorageError>>;

    /// Counts matching documents.
    fn count(&self, prepared: PreparedQuery) -> BoxFuture<'_, Result<CountResult, StorageError>>;

    /// Reads attachment bytes.
    fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
    ) -> BoxFuture<'_, Result<AttachmentData, StorageError>>;

    /// Whether this storage implements [`Self::get_changed_documents_since`].
    ///
    /// Defaults to `false`, and the default implementation of the operation
    /// reports not-supported, so no wrapper can accidentally invent the
    /// operation (`docs/CLEANROOM-GAPS.md` G-023).
    fn supports_get_changed_documents_since(&self) -> bool {
        false
    }

    /// Reads documents changed after a checkpoint.
    fn get_changed_documents_since(
        &self,
        _limit: usize,
        _checkpoint: Option<Checkpoint>,
    ) -> BoxFuture<'_, Result<ChangedDocuments, StorageError>> {
        Box::pin(async { Err(StorageError::not_supported("getChangedDocumentsSince")) })
    }

    /// Whether this storage implements [`Self::info`].
    ///
    /// `info` is documented as a loggable operation but is absent from the
    /// Apache-2.0 interface (`docs/CLEANROOM-GAPS.md` G-005), so it is an
    /// extension point with the same not-supported default.
    fn supports_info(&self) -> bool {
        false
    }

    /// Reports storage information.
    fn info(&self) -> BoxFuture<'_, Result<InfoReport, StorageError>> {
        Box::pin(async { Err(StorageError::not_supported("info")) })
    }

    /// Removes purged tombstones. `false` means "more work remains".
    fn cleanup(&self, minimum_deleted_time_ms: u64) -> BoxFuture<'_, Result<bool, StorageError>>;

    /// Closes the instance.
    fn close(&self) -> BoxFuture<'_, Result<(), StorageError>>;

    /// Removes the instance's data.
    fn remove(&self) -> BoxFuture<'_, Result<(), StorageError>>;
}

#[cfg(test)]
mod tests {
    use super::meta::{
        is_meta_collection, INTERNAL_COLLECTION_NAME, LOCAL_DOCUMENTS_COLLECTION_NAME,
    };

    #[test]
    fn internal_collection_names_are_recognised() {
        assert!(is_meta_collection(INTERNAL_COLLECTION_NAME));
        assert!(is_meta_collection(LOCAL_DOCUMENTS_COLLECTION_NAME));
        assert!(!is_meta_collection("humans"));
        assert!(!is_meta_collection("_rxdb_internal_extra"));
    }
}
