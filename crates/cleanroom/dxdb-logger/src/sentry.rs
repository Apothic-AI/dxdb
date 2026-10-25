//! Sentry span conversion: a documented field set, not an SDK.
//!
//! The public docs describe the logger as useful for "Application Performance
//! Monitoring (APM) tools like Bugsnag, Datadog, Elastic, Sentry and others".
//! dxdb's interpretation: emit records that carry enough structure to become a
//! Sentry span (or an OpenTelemetry database span) without taking a dependency
//! on any vendor SDK, opening a socket, or requiring an API key
//! (`docs/CLEANROOM-GAPS.md` G-026).
//!
//! The field set is the public span interface
//! (`develop.sentry.dev/sdk/foundations/envelopes/event-payloads/span/`):
//! required `span_id` (8 bytes, hex), `trace_id` (16 bytes, hex),
//! `start_timestamp`, `timestamp`; recommended `op`; optional `description`,
//! `status`, `data`, `tags`, `origin`, `parent_span_id`.

use crate::record::{LogRecord, OperationName, Phase};
use crate::storage::StorageErrorKind;

/// Why a conversion to a span failed.
///
/// This is the only error this crate produces itself; every other error it
/// reports is the inner storage's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogError {
    /// The record carried no wall clock, because timing is disabled or the
    /// host's clock has no wall time. A span without a timestamp is invalid, so
    /// none is invented (`docs/CLEANROOM-GAPS.md` G-022).
    NoWallClock,
    /// The given record is a start record, not a terminal one.
    NotTerminal,
    /// The two records belong to different operations or different log ids.
    MismatchedRecords,
    /// The terminal timestamp is earlier than the start timestamp.
    NonMonotonicTimestamps,
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            LogError::NoWallClock => "record has no wall clock reading",
            LogError::NotTerminal => "record is not a terminal record",
            LogError::MismatchedRecords => "records do not belong to the same operation",
            LogError::NonMonotonicTimestamps => "terminal timestamp precedes start timestamp",
        };
        f.write_str(text)
    }
}

impl std::error::Error for LogError {}

/// The public Sentry span status vocabulary.
///
/// Kept whole rather than narrowed to the states this crate can emit, so the
/// mapping stays readable against the public table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpanStatus {
    /// The operation completed successfully.
    Ok,
    /// The operation was cancelled by the caller.
    Cancelled,
    /// An unknown error without enough information to classify it.
    Unknown,
    /// The caller supplied an invalid argument.
    InvalidArgument,
    /// A deadline expired.
    DeadlineExceeded,
    /// The requested entity was not found.
    NotFound,
    /// The entity already exists.
    AlreadyExists,
    /// The caller lacks permission.
    PermissionDenied,
    /// A resource (quota, disk) is exhausted.
    ResourceExhausted,
    /// The system state must be handled before retrying.
    FailedPrecondition,
    /// The operation was aborted.
    Aborted,
    /// The operation went past a valid range.
    OutOfRange,
    /// Not implemented or not enabled for this operation.
    Unimplemented,
    /// An expected invariant was broken.
    InternalError,
    /// A transient unavailability.
    Unavailable,
    /// Unrecoverable data loss or corruption.
    DataLoss,
    /// The requester has no valid credentials.
    Unauthenticated,
}

impl SpanStatus {
    /// The wire spelling from the public interface.
    pub const fn as_str(self) -> &'static str {
        match self {
            SpanStatus::Ok => "ok",
            SpanStatus::Cancelled => "cancelled",
            SpanStatus::Unknown => "unknown",
            SpanStatus::InvalidArgument => "invalid_argument",
            SpanStatus::DeadlineExceeded => "deadline_exceeded",
            SpanStatus::NotFound => "not_found",
            SpanStatus::AlreadyExists => "already_exists",
            SpanStatus::PermissionDenied => "permission_denied",
            SpanStatus::ResourceExhausted => "resource_exhausted",
            SpanStatus::FailedPrecondition => "failed_precondition",
            SpanStatus::Aborted => "aborted",
            SpanStatus::OutOfRange => "out_of_range",
            SpanStatus::Unimplemented => "unimplemented",
            SpanStatus::InternalError => "internal_error",
            SpanStatus::Unavailable => "unavailable",
            SpanStatus::DataLoss => "data_loss",
            SpanStatus::Unauthenticated => "unauthenticated",
        }
    }
}

/// Identifiers for a span. The caller supplies them so this crate needs no RNG
/// and so tests are deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanIds {
    /// 8 bytes, rendered as 16 lower-case hex characters.
    pub span_id: u64,
    /// 16 bytes, rendered as 32 lower-case hex characters.
    pub trace_id: [u8; 16],
    /// Optional parent span.
    pub parent_span_id: Option<u64>,
}

impl SpanIds {
    /// Derives ids from the operation's log id using splitmix64.
    ///
    /// Deterministic on purpose: the tests assert exact ids, and a real
    /// deployment passes its own random ids from the APM client. This is not a
    /// cryptographic generator and is not presented as one.
    pub fn deterministic(log_id: u64) -> Self {
        let mut state = log_id ^ 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        let span_id = next();
        let mut trace_id = [0u8; 16];
        for chunk in trace_id.chunks_mut(8) {
            let word = next().to_be_bytes();
            chunk.copy_from_slice(&word);
        }
        Self {
            span_id,
            trace_id,
            parent_span_id: None,
        }
    }
}

/// One span in the public Sentry shape.
///
/// Field names match the wire format, including `op`, `start_timestamp` and
/// `timestamp`, so serializing this struct produces a valid span object.
#[derive(Debug, Clone, PartialEq)]
pub struct SentrySpan {
    /// 16 lower-case hex characters.
    pub span_id: String,
    /// 32 lower-case hex characters.
    pub trace_id: String,
    /// Present when the span is a child of another span.
    pub parent_span_id: Option<String>,
    /// Short operation code, e.g. `db.query`.
    pub op: String,
    /// Longer, consistent description of the span.
    pub description: String,
    /// Status from the documented vocabulary.
    pub status: SpanStatus,
    /// Unix seconds.
    pub start_timestamp: f64,
    /// Unix seconds, always `>= start_timestamp`.
    pub timestamp: f64,
    /// Span data, keys lower-case with underscores, OpenTelemetry-shaped.
    pub data: std::collections::BTreeMap<String, String>,
    /// What created the span.
    pub origin: String,
}

impl SentrySpan {
    /// Converts a start/terminal record pair into one span.
    ///
    /// Both records must belong to the same `LogId` and the terminal one must be
    /// an `End` or `Error` record. The records must carry wall-clock readings;
    /// if they do not, this returns [`LogError::NoWallClock`] rather than
    /// producing a span with a fabricated timestamp.
    pub fn from_records(
        start: &LogRecord,
        terminal: &LogRecord,
        ids: SpanIds,
    ) -> Result<Self, LogError> {
        if start.log_id != terminal.log_id || start.operation != terminal.operation {
            return Err(LogError::MismatchedRecords);
        }
        if start.phase != Phase::Start {
            return Err(LogError::NotTerminal);
        }
        if !terminal.phase.is_terminal() {
            return Err(LogError::NotTerminal);
        }
        let (Some(start_ns), Some(end_ns)) = (start.wall_clock_ns, terminal.wall_clock_ns) else {
            return Err(LogError::NoWallClock);
        };
        if end_ns < start_ns {
            return Err(LogError::NonMonotonicTimestamps);
        }

        let status = match (
            terminal.phase,
            terminal.error.as_ref().map(|error| error.kind),
        ) {
            (Phase::End, _) => SpanStatus::Ok,
            (_, Some(StorageErrorKind::NotFound)) => SpanStatus::NotFound,
            (_, Some(StorageErrorKind::Conflict)) => SpanStatus::AlreadyExists,
            (_, Some(StorageErrorKind::Closed)) => SpanStatus::FailedPrecondition,
            (_, Some(StorageErrorKind::NotSupported)) => SpanStatus::Unimplemented,
            (_, Some(StorageErrorKind::InvalidArgument)) => SpanStatus::InvalidArgument,
            (_, Some(StorageErrorKind::Other)) | (_, None) => SpanStatus::Unknown,
        };

        let mut data = std::collections::BTreeMap::new();
        data.insert("db.system".to_string(), "dxdb".to_string());
        data.insert(
            "db.operation.name".to_string(),
            terminal.operation.as_str().to_string(),
        );
        data.insert(
            "db.collection.name".to_string(),
            terminal.collection_name.clone(),
        );
        data.insert("db.namespace".to_string(), terminal.database_name.clone());
        data.insert("dxdb.logger.prefix".to_string(), terminal.prefix.clone());
        data.insert(
            "dxdb.logger.log_id".to_string(),
            terminal.log_id.get().to_string(),
        );
        data.insert(
            "dxdb.logger.duration_ns".to_string(),
            terminal.duration_ns.unwrap_or_default().to_string(),
        );
        if let Some(count) = terminal.details.document_count {
            data.insert("db.document.count".to_string(), count.to_string());
        }
        if let Some(count) = terminal.details.error_count {
            data.insert("dxdb.write.error_count".to_string(), count.to_string());
        }
        if let Some(error) = terminal.error.as_ref() {
            data.insert("error.kind".to_string(), error.kind.as_str().to_string());
            data.insert("error.message".to_string(), error.message.clone());
        }

        let span_id = ids.span_id;
        Ok(Self {
            span_id: format!("{span_id:016x}"),
            trace_id: hex16(&ids.trace_id),
            parent_span_id: ids.parent_span_id.map(|id| format!("{id:016x}")),
            op: span_op(terminal.operation),
            description: describe(terminal),
            status,
            start_timestamp: nanos_to_seconds(start_ns),
            timestamp: nanos_to_seconds(end_ns),
            data,
            origin: "auto.dxdb.logger".to_string(),
        })
    }
}

/// The span `op` for a logged operation: a short `db.`-prefixed code.
fn span_op(operation: OperationName) -> String {
    let suffix = match operation {
        OperationName::BulkWrite => "bulk_write",
        OperationName::FindDocumentsById => "find_documents_by_id",
        OperationName::Query => "query",
        OperationName::Count => "count",
        OperationName::Info => "info",
        OperationName::GetAttachmentData => "get_attachment_data",
        OperationName::GetChangedDocumentsSince => "get_changed_documents_since",
        OperationName::Cleanup => "cleanup",
        OperationName::Close => "close",
        OperationName::Remove => "remove",
    };
    format!("db.{suffix}")
}

fn describe(record: &LogRecord) -> String {
    let operation = record.operation.as_str();
    let collection = &record.collection_name;
    if record.prefix.is_empty() {
        format!("{operation} {collection}")
    } else {
        let prefix = &record.prefix;
        format!("{prefix}{operation} {collection}")
    }
}

fn hex16(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

#[allow(clippy::cast_precision_loss)]
fn nanos_to_seconds(nanos: u64) -> f64 {
    nanos as f64 / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::{hex16, nanos_to_seconds, SpanIds};

    #[test]
    fn span_ids_have_the_documented_lengths() {
        let ids = SpanIds::deterministic(7);
        assert_eq!(format!("{:016x}", ids.span_id).len(), 16);
        assert_eq!(hex16(&ids.trace_id).len(), 32);
    }

    #[test]
    fn identical_log_ids_give_identical_span_ids() {
        assert_eq!(SpanIds::deterministic(42), SpanIds::deterministic(42));
        assert_ne!(SpanIds::deterministic(42), SpanIds::deterministic(43));
    }

    #[test]
    fn timestamps_convert_to_unix_seconds() {
        assert!((nanos_to_seconds(1_500_000_000) - 1.5).abs() < f64::EPSILON);
        assert_eq!(nanos_to_seconds(0), 0.0);
    }
}
