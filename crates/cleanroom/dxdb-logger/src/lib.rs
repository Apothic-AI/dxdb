//! `dxdb-logger`: structured records for every storage operation.
//!
//! Implemented from `specs/logger.md`, not from any closed-source module. The
//! specification was written from public documentation, the Apache-2.0 storage
//! interface, and the public Sentry span interface; this crate's provenance
//! record is `specs/READING-LIST.md`. No upstream paid source was consulted.
//!
//! The shape is the wrapper pattern from PLAN.md section 4.2: a type that
//! implements the storage operations over an inner storage and forwards every
//! result unchanged. The logger is the cheapest clean-room feature and exists to
//! prove the pipeline end to end (CLEANROOM.md sections 5.13 and 7, stage B1).
//!
//! Three properties hold for every setting combination and every call:
//!
//! * transparency: the value returned is the inner storage's value, and no
//!   document contents reach a record (`INV-1`);
//! * pairing: a logged operation emits exactly one start record and exactly one
//!   terminal record, never both terminal kinds (`INV-2`);
//! * silence when filtered: a disabled operation or a filtered meta storage
//!   instance produces no records and reads no clock (`INV-3`).
//!
//! ```
//! use dxdb_logger::{LoggerSettings, OperationName};
//!
//! // The documented defaults: everything on, empty prefix.
//! let settings = LoggerSettings::default();
//! assert!(settings.times);
//! assert!(settings.meta_storage_instances);
//! assert_eq!(settings.prefix, "");
//! assert!(settings.should_log(OperationName::BulkWrite, "humans"));
//!
//! // Turning one operation off silences it for every collection.
//! let quiet = settings.clone().with_operation(OperationName::BulkWrite, false);
//! assert!(!quiet.should_log(OperationName::BulkWrite, "humans"));
//! assert!(quiet.should_log(OperationName::Query, "humans"));
//! ```

#![forbid(unsafe_code)]

pub mod clock;
pub mod hooks;
pub mod record;
pub mod sentry;
pub mod settings;
pub mod storage;
mod wrapper;

pub use clock::{Clock, ClockReading, ManualClock};
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub use clock::MonotonicClock;
pub use hooks::{NoopHooks, OperationHooks};
pub use record::{LogId, LogRecord, OperationDetails, OperationName, Phase};
pub use sentry::{LogError, SentrySpan, SpanIds, SpanStatus};
pub use settings::{LoggerSettings, OperationToggles};
pub use storage::meta::{INTERNAL_COLLECTION_NAME, LOCAL_DOCUMENTS_COLLECTION_NAME};
pub use storage::{
    AttachmentData, BoxFuture, BulkWriteResponse, ChangedDocuments, Checkpoint, CountResult,
    InfoReport, InstanceParams, LoggedStorage, LoggedStorageInstance, PreparedQuery, QueryResult,
    RawDocument, RawWriteRow, StorageError, StorageErrorKind, WriteError, WriteErrorKind,
};
pub use wrapper::{LoggerInstance, LoggerStorage, WRAPPED_STORAGE_NAME};

/// Whether `collection_name` names an internal ("meta") storage instance.
///
/// This is the documented `settings.metaStorageInstances` filter's test, exposed
/// because a caller configuring storage names wants to ask the same question.
pub use storage::meta::is_meta_collection;
