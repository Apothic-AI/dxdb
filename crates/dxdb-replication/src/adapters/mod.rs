//! Replication adapters: one [`ReplicationHandler`] per provider.
//!
//! Each adapter turns the protocol's three methods into one provider's wire calls. The
//! protocol itself never learns any of it: [`ReplicationHandler`] stays three methods, and an
//! adapter is a type that implements it. That is the seam, and it is the reason a new provider
//! is a new module rather than a change to `state.rs`.
//!
//! # The rule this module exists to enforce
//!
//! **Nothing provider-specific may appear in the protocol.** If an adapter needs the protocol
//! to know something — a callback, a flag, a checkpoint accessor — that is a design finding,
//! not an adapter detail, because it means the seam is in the wrong place. Two cases have
//! already come up and are recorded rather than papered over:
//!
//! 1. **Attachment payloads** ([`crate::state::WriteToMasterRow::attachment_payloads`]). A
//!    Rust document is ordered JSON and cannot carry bytes, so payloads travel beside the
//!    document. The protocol populates the field; each adapter decides how to encode it (a
//!    byte array inside the stub for CouchDB and WebSocket, a base64 `_attachments_data` field
//!    for the file-based transports). No adapter forced a protocol change to get this.
//! 2. **Checkpoints are opaque** (D7). Every adapter in this module has a different checkpoint
//!    shape, and none of them had to touch the protocol to express it: CouchDB's is
//!    `{sequence}`, GraphQL's is whatever the caller's server returns. That is D7 working.
//!
//! # What stays out
//!
//! Reconnection, backoff, heartbeats and auth refresh are **not** here. They need a clock and a
//! socket lifecycle, which belong to whichever binding owns the executor (D3). An adapter in
//! this module performs one call at a time and reports failure; retrying is above it.
//!
//! # Modules
//!
//! | module | upstream | transport |
//! | --- | --- | --- |
//! | [`websocket`] | `plugins/replication-websocket/` (638) | `{id, collection, method, params}` frames |
//! | [`couchdb`] | `plugins/replication-couchdb/` (475) | HTTP `_changes` / `_all_docs` / `_bulk_docs` |

pub mod couchdb;
pub mod websocket;

/// A transport failure an adapter can report.
///
/// Deliberately small and provider-neutral: an adapter maps its own provider's errors onto
/// these, so the protocol layer never sees a CouchDB `error: "conflict"` string or a WebSocket
/// close code. Retry policy is above this (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The request could not be made at all: no connection, DNS, TLS, a closed socket.
    Unreachable(String),
    /// The provider answered with a non-success status. `status` is the provider's, kept for
    /// diagnostics and for a caller that wants to distinguish 401 from 503.
    Status { status: u16, body: String },
    /// The provider's response was not the shape the adapter expects. This is the adapter's
    /// own bug or a version skew, never a network problem, so it is separate.
    Malformed(String),
}

impl TransportError {
    /// A short human-readable summary, for an error the caller will surface.
    pub fn message(&self) -> String {
        match self {
            TransportError::Unreachable(message) => format!("unreachable: {message}"),
            TransportError::Status { status, body } => {
                let body = body.trim();
                let body = if body.len() > 300 { &body[..300] } else { body };
                format!("provider returned {status}: {body}")
            }
            TransportError::Malformed(message) => format!("malformed response: {message}"),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Converts an adapter failure into the protocol's error type.
///
/// Every adapter needs this and none of them should invent its own mapping, because the
/// protocol's `StorageError::Unsupported` is what surfaces a transport problem to a caller.
pub fn to_storage_error(error: TransportError) -> dxdb_storage::StorageError {
    dxdb_storage::StorageError::Unsupported(error.message())
}
