//! The record a logged operation produces, and the names it uses.
//!
//! One record per phase of one operation: [`Phase::Start`], then exactly one of
//! [`Phase::End`] or [`Phase::Error`] (`INV-2` in `specs/logger.md`). Records
//! never carry document payloads, only counts and identifiers (`INV-1`).

use crate::storage::StorageError;

/// Identifies one operation so its start record and terminal record pair up.
///
/// Allocated from a per-instance counter, so ids are unique within one wrapped
/// storage instance and monotonic (`INV-4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogId(pub u64);

impl LogId {
    /// The first id an instance hands out.
    pub const FIRST: LogId = LogId(1);

    /// The id as a `u64`.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for LogId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Which moment of an operation a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// The call is about to be forwarded to the inner storage.
    Start,
    /// The call returned a value.
    End,
    /// The call returned an error.
    Error,
    /// The caller dropped the future before it finished.
    ///
    /// A dropped future never reaches the inner storage's response, so it has no
    /// duration and no result. Reporting it keeps the pairing invariant true
    /// under `select!` and timeouts, and it is exactly the `cancelled` state the
    /// public Sentry status vocabulary documents as "the operation was cancelled,
    /// typically by the caller".
    Cancelled,
}

impl Phase {
    /// The name used in log output and in Sentry conversion.
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Start => "start",
            Phase::End => "end",
            Phase::Error => "error",
            Phase::Cancelled => "cancelled",
        }
    }

    /// Whether this is a terminal phase.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Phase::End | Phase::Error | Phase::Cancelled)
    }
}

/// The documented operation set.
///
/// These are exactly the operations named in the public settings list of the
/// logger plugin (`docs-src/docs/logger.md`), and nothing else. `Info` is
/// documented but is not part of the open storage interface, so it is an
/// extension point; see `docs/CLEANROOM-GAPS.md` G-005.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationName {
    /// `bulkWrite`
    BulkWrite,
    /// `findDocumentsById`
    FindDocumentsById,
    /// `query`
    Query,
    /// `count`
    Count,
    /// `info`
    Info,
    /// `getAttachmentData`
    GetAttachmentData,
    /// `getChangedDocumentsSince`
    GetChangedDocumentsSince,
    /// `cleanup`
    Cleanup,
    /// `close`
    Close,
    /// `remove`
    Remove,
}

impl OperationName {
    /// Every documented operation, in the documented order.
    ///
    /// Tests iterate this, which is why the settings space is enumerable rather
    /// than sampleable (`specs/logger.md`, property-testing note).
    pub const ALL: [OperationName; 10] = [
        OperationName::BulkWrite,
        OperationName::FindDocumentsById,
        OperationName::Query,
        OperationName::Count,
        OperationName::Info,
        OperationName::GetAttachmentData,
        OperationName::GetChangedDocumentsSince,
        OperationName::Cleanup,
        OperationName::Close,
        OperationName::Remove,
    ];

    /// The name as written in the public documentation and in log output.
    pub const fn as_str(self) -> &'static str {
        match self {
            OperationName::BulkWrite => "bulkWrite",
            OperationName::FindDocumentsById => "findDocumentsById",
            OperationName::Query => "query",
            OperationName::Count => "count",
            OperationName::Info => "info",
            OperationName::GetAttachmentData => "getAttachmentData",
            OperationName::GetChangedDocumentsSince => "getChangedDocumentsSince",
            OperationName::Cleanup => "cleanup",
            OperationName::Close => "close",
            OperationName::Remove => "remove",
        }
    }
}

/// A summary of a call's arguments, never its payloads.
///
/// Every field is optional because a record describes one operation, and one
/// operation does not have all of these. `document_ids` is capped at
/// [`OperationDetails::MAX_LOGGED_IDS`] so a large batch write cannot make a
/// record proportionally large; `document_count` always carries the true count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperationDetails {
    /// How many documents the call carried or returned, if it carried any.
    pub document_count: Option<usize>,
    /// The first few document ids the call touched.
    pub document_ids: Vec<String>,
    /// True when `document_ids` was capped; `document_count` remains exact.
    pub document_ids_truncated: bool,
    /// Per-row errors reported by a successful `bulkWrite` call.
    pub error_count: Option<usize>,
    /// The `withDeleted` argument of `findDocumentsById`.
    pub with_deleted: Option<bool>,
    /// The query limit.
    pub limit: Option<usize>,
    /// The query skip.
    pub skip: usize,
    /// The index the query requested.
    pub index: Option<Vec<String>>,
    /// Whether a `getChangedDocumentsSince` call carried a checkpoint.
    pub checkpoint_present: Option<bool>,
    /// The `cleanup` threshold.
    pub minimum_deleted_time_ms: Option<u64>,
    /// The `bulkWrite` context string.
    pub context: Option<String>,
    /// The attachment id of a `getAttachmentData` call.
    pub attachment_id: Option<String>,
}

impl OperationDetails {
    /// The most document ids one record will carry.
    pub const MAX_LOGGED_IDS: usize = 16;

    /// Records `ids` as this call's document ids, capping the list.
    pub fn set_document_ids(&mut self, ids: impl IntoIterator<Item = String>) {
        let mut count = 0usize;
        for id in ids {
            count += 1;
            if self.document_ids.len() < Self::MAX_LOGGED_IDS {
                self.document_ids.push(id);
            }
        }
        self.document_count = Some(count);
        self.document_ids_truncated = count > self.document_ids.len();
    }
}

/// One log record: everything the hooks and the `tracing` bridge see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// Pairs the start record with its terminal record.
    pub log_id: LogId,
    /// Which documented operation this is.
    pub operation: OperationName,
    /// Which moment of the operation this record describes.
    pub phase: Phase,
    /// The database name of the storage instance, not of any call.
    pub database_name: String,
    /// The collection name of the storage instance.
    pub collection_name: String,
    /// The configured prefix, verbatim.
    pub prefix: String,
    /// Wall clock at this record, in Unix nanoseconds.
    ///
    /// `None` when timing is disabled (the clock is then never read) or when the
    /// host's [`Clock`](crate::Clock) has no wall time available.
    pub wall_clock_ns: Option<u64>,
    /// Elapsed time in nanoseconds. `Some` only on an [`Phase::End`] record
    /// produced with timing enabled (`INV-5`).
    pub duration_ns: Option<u64>,
    /// The call's argument summary.
    pub details: OperationDetails,
    /// The error, on an [`Phase::Error`] record only.
    pub error: Option<StorageError>,
}
