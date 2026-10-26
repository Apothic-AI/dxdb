//! The wire protocol between a caller and a worker isolate.
//!
//! `specs/worker.md` sections 3.1 and 3.2.
//!
//! # Why a worker is not a thread
//!
//! A browser `Worker`, a `SharedWorker` and a Node `worker_thread` are separate isolates
//! with their own linear memory. They communicate by message passing, which means
//! **nothing here needs atomics** and nothing needs `wasm32-wasip1-threads` or a nightly
//! `-Z build-std`. The whole crate lands on stable `wasm32-unknown-unknown`
//! (`docs/CLEANROOM-GAPS.md` G-048).
//!
//! The cost is stated rather than hidden: every call and every result is copied across the
//! boundary, so a `query` returning 10 000 documents copies them.

use serde_json::Value;

/// The protocol version this build implements.
pub const PROTOCOL: u16 = 1;
/// The oldest protocol version this build can still talk to.
pub const MIN_SUPPORTED: u16 = 1;

/// Reserved correlation id, for the handshake and for pushes.
///
/// A storage call never uses it, so a storage response can never be mistaken for a
/// handshake reply or an event.
pub const RESERVED_ID: u64 = 0;

/// Which storage operation a request carries.
///
/// One variant per trait method, and the enum is exhaustive at the match site, so adding a
/// trait method is a compile error rather than an unhandled message.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestKind {
    /// Open an instance.
    CreateInstance {
        /// The creation params, serialised as JSON.
        params: Value,
    },
    /// Write documents.
    BulkWrite {
        /// The rows, serialised as JSON.
        rows: Value,
        /// The caller's context string, which identifies the operation on a death.
        context: String,
    },
    /// Fetch documents by primary key.
    FindDocumentsById {
        /// The ids to fetch.
        ids: Vec<String>,
        /// Whether tombstones are included.
        with_deleted: bool,
    },
    /// Run a prepared query.
    Query {
        /// The prepared query, serialised.
        prepared: Value,
    },
    /// Count.
    Count {
        /// The prepared query, serialised.
        prepared: Value,
    },
    /// Read an attachment.
    GetAttachmentData {
        /// Owning document.
        document_id: String,
        /// Attachment id.
        attachment_id: String,
        /// Expected digest.
        digest: String,
    },
    /// Read documents changed after a checkpoint.
    GetChangedDocumentsSince {
        /// Page size.
        limit: usize,
        /// Where to resume.
        checkpoint: Option<Value>,
    },
    /// Purge tombstones.
    Cleanup {
        /// Age threshold in milliseconds.
        minimum_deleted_time: f64,
    },
    /// Close the instance.
    Close,
    /// Remove the instance and its data.
    Remove,
}

impl RequestKind {
    /// The operation's name, for error messages and for telling a caller *which* call died.
    pub const fn name(&self) -> &'static str {
        match self {
            RequestKind::CreateInstance { .. } => "createStorageInstance",
            RequestKind::BulkWrite { .. } => "bulkWrite",
            RequestKind::FindDocumentsById { .. } => "findDocumentsById",
            RequestKind::Query { .. } => "query",
            RequestKind::Count { .. } => "count",
            RequestKind::GetAttachmentData { .. } => "getAttachmentData",
            RequestKind::GetChangedDocumentsSince { .. } => "getChangedDocumentsSince",
            RequestKind::Cleanup { .. } => "cleanup",
            RequestKind::Close => "close",
            RequestKind::Remove => "remove",
        }
    }

    /// Whether this is a write, which must never be retried after a worker death.
    ///
    /// `docs/CLEANROOM-GAPS.md` G-049: across a process boundary the caller cannot know
    /// which rows were applied, so a retry risks a double apply. The distinct error kind
    /// tells the layer above to re-read and reconcile instead.
    pub const fn is_write(&self) -> bool {
        matches!(self, RequestKind::BulkWrite { .. })
    }

    /// The context string, for a write.
    pub fn context(&self) -> Option<&str> {
        match self {
            RequestKind::BulkWrite { context, .. } => Some(context),
            _ => None,
        }
    }
}

/// A message sent from the caller to the worker.
#[derive(Debug, Clone, PartialEq)]
pub enum ToWorker {
    /// Negotiate a version before any storage call.
    Hello {
        /// The highest protocol the caller speaks.
        protocol: u16,
        /// The oldest the caller still supports.
        min_supported: u16,
    },
    /// A storage operation.
    Request {
        /// Correlation id, allocated by the caller. Never [`RESERVED_ID`].
        id: u64,
        /// What to do.
        kind: RequestKind,
    },
    /// A liveness check.
    Ping {
        /// Echoed back in the pong.
        id: u64,
    },
    /// Subscribe to the change feed.
    Subscribe,
    /// Drop the subscription.
    Unsubscribe,
    /// The caller is going away.
    Goodbye,
}

/// A message sent from the worker to the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum FromWorker {
    /// The handshake succeeded.
    HelloReply {
        /// The agreed protocol, the highest version in the intersection of the two ranges.
        protocol: u16,
        /// The worker's engine name.
        storage_name: String,
        /// Identifies this worker incarnation, so a reconnection after a restart is
        /// detectable (INV-10).
        instance_token: String,
    },
    /// The handshake failed: the ranges do not overlap.
    HelloRejected {
        /// The worker's highest.
        protocol: u16,
        /// The worker's oldest.
        min_supported: u16,
    },
    /// A storage result.
    Response {
        /// The id of the request this answers.
        id: u64,
        /// `Ok(value)` or `Err(message)`.
        result: Result<Value, String>,
    },
    /// A liveness reply.
    Pong {
        /// The id from the ping.
        id: u64,
    },
    /// A change event push. Always [`RESERVED_ID`], so it is never mistaken for a response.
    Event {
        /// The event bulk, serialised.
        bulk: Value,
    },
    /// The worker is shutting down cleanly.
    Shutdown,
}

/// Why a handshake failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolMismatch {
    /// No version is supported by both sides.
    #[error("no common protocol: caller supports {caller_min}..={caller_max}, worker supports {worker_min}..={worker_max}")]
    NoOverlap {
        /// The caller's highest.
        caller_max: u16,
        /// The caller's oldest.
        caller_min: u16,
        /// The worker's highest.
        worker_max: u16,
        /// The worker's oldest.
        worker_min: u16,
    },
}

/// The version two ranges agree on.
///
/// The **highest** version in the intersection, so an additive change does not force a
/// downgrade for peers that both understand the newer form. Equality-only negotiation would
/// make every minor addition a hard break.
pub fn negotiate(
    caller_max: u16,
    caller_min: u16,
    worker_max: u16,
    worker_min: u16,
) -> Result<u16, ProtocolMismatch> {
    let low = caller_min.max(worker_min);
    let high = caller_max.min(worker_max);
    if low > high {
        return Err(ProtocolMismatch::NoOverlap {
            caller_max,
            caller_min,
            worker_max,
            worker_min,
        });
    }
    Ok(high)
}

#[cfg(test)]
mod tests {
    use super::{negotiate, ProtocolMismatch, RequestKind, PROTOCOL};

    #[test]
    fn identical_ranges_agree_on_that_version() {
        assert_eq!(negotiate(1, 1, 1, 1), Ok(1));
        assert_eq!(negotiate(3, 2, 3, 2), Ok(3));
    }

    #[test]
    fn the_highest_common_version_wins() {
        // Worker knows 1..=3, caller knows 2..=4: the intersection is 2..=3.
        assert_eq!(negotiate(4, 2, 3, 1), Ok(3));
        // Caller older: 1..=2 against 1..=5.
        assert_eq!(negotiate(2, 1, 5, 1), Ok(2));
    }

    #[test]
    fn disjoint_ranges_fail_naming_both() {
        let error = negotiate(5, 4, 3, 1).expect_err("disjoint");
        assert_eq!(
            error,
            ProtocolMismatch::NoOverlap {
                caller_max: 5,
                caller_min: 4,
                worker_max: 3,
                worker_min: 1
            }
        );
        assert!(format!("{error}").contains("caller supports 4..=5"));
    }

    #[test]
    fn this_build_negotiates_with_itself() {
        assert_eq!(
            negotiate(
                PROTOCOL,
                super::MIN_SUPPORTED,
                PROTOCOL,
                super::MIN_SUPPORTED
            ),
            Ok(PROTOCOL)
        );
    }

    #[test]
    fn only_bulk_write_counts_as_a_write() {
        assert!(RequestKind::BulkWrite {
            rows: serde_json::json!([]),
            context: "c".to_string()
        }
        .is_write());
        for other in [
            RequestKind::Close,
            RequestKind::Remove,
            RequestKind::Cleanup {
                minimum_deleted_time: 0.0,
            },
            RequestKind::Query {
                prepared: serde_json::json!({}),
            },
        ] {
            assert!(
                !other.is_write(),
                "{other:?} must not be treated as a write"
            );
        }
    }

    #[test]
    fn a_write_carries_its_context() {
        let write = RequestKind::BulkWrite {
            rows: serde_json::json!([]),
            context: "replication-batch-7".to_string(),
        };
        assert_eq!(write.context(), Some("replication-batch-7"));
        assert_eq!(RequestKind::Close.context(), None);
    }
}
