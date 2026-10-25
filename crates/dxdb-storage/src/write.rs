//! Write rows, write results, and the per-row error model.
//!
//! # The contract this module encodes
//!
//! From `rx-storage.interface.d.ts:116` (quoted in PLAN.md 4.2) and from the
//! behaviour the two conformance suites assert:
//!
//! * **Per-document atomic. No cross-document transaction.** "It must be possible
//!   that some document writes succeed and others error." A `bulk_write` of ten
//!   rows where three conflict writes seven documents and returns three errors —
//!   it does not roll back.
//! * **The written document must be the newest revision.** A row whose
//!   `previous` does not match the stored `_rev` is a `409` conflict and must not
//!   be written. The exception, and it is load-bearing, is that a row with *no*
//!   `previous` against an existing document is also a `409` — you cannot
//!   silently insert over a live document.
//! * **Errors are returned, not thrown.** A `bulk_write` that could throw would
//!   make partial success unobservable.
//!
//! [`BulkWriteResponse`] is therefore just the error list. Upstream removed the
//! written documents from the response for performance and reconstructs them from
//! the input minus the errors; doing the same keeps a worker-boundary transfer a
//! fraction of the size.

use std::collections::HashMap;

use crate::document::{DocumentData, DocumentWriteData};

/// Status code for a revision conflict.
pub const WRITE_ERROR_CONFLICT: u16 = 409;
/// Status code for a schema validation failure (added by the validator plugin,
/// which is why no storage produces it on its own).
pub const WRITE_ERROR_VALIDATION: u16 = 422;
/// Status code for a new attachment supplied without its payload.
pub const WRITE_ERROR_ATTACHMENT: u16 = 510;

/// One row handed to [`crate::RxStorageInstance::bulk_write`].
///
/// `previous` is the *whole* previous document, not just its revision, because
/// upstream already has it in hand and passing it avoids a re-read; conflict
/// detection only reads `previous._rev`.
#[derive(Clone, Debug, PartialEq)]
pub struct BulkWriteRow {
    /// The state the caller believes is stored. `None` means "this is an insert".
    pub previous: Option<DocumentData>,
    /// The state to store.
    pub document: DocumentWriteData,
}

impl BulkWriteRow {
    /// An insert: no previous state.
    pub fn insert(document: DocumentWriteData) -> Self {
        BulkWriteRow {
            previous: None,
            document,
        }
    }

    /// An update or delete: the previous state plus the new one.
    pub fn update(previous: DocumentData, document: DocumentWriteData) -> Self {
        BulkWriteRow {
            previous: Some(previous),
            document,
        }
    }
}

/// A processed row: the same row, with attachment payloads removed and the
/// document narrowed to its stored shape.
///
/// Upstream's `BulkWriteRowProcessed`. Storages keep these in their index
/// structures, so it is a distinct type from [`BulkWriteRow`] to make "you cannot
/// accidentally store a payload in an index" a compile error.
#[derive(Clone, Debug, PartialEq)]
pub struct BulkWriteRowProcessed {
    /// The previous state, as given.
    pub previous: Option<DocumentData>,
    /// The stored shape: `_attachments` holds stubs only.
    pub document: DocumentData,
}

impl BulkWriteRowProcessed {
    /// Builds a processed row.
    pub fn new(previous: Option<DocumentData>, document: DocumentData) -> Self {
        BulkWriteRowProcessed {
            previous,
            document,
        }
    }
}

impl From<BulkWriteRowProcessed> for BulkWriteRow {
    fn from(row: BulkWriteRowProcessed) -> Self {
        BulkWriteRow {
            previous: row.previous,
            document: DocumentWriteData::new(row.document),
        }
    }
}

/// One row's failure.
///
/// The three variants match upstream's three `status` codes exactly, and the
/// reason each exists is different, which is why this is an enum rather than a
/// struct with a `status: u16`:
///
/// * a **conflict** carries `document_in_db` so the caller can resolve the fork
///   without a second read (upstream comment: "ensures that we can continue
///   resolving a conflict without having to pull the document out of the db
///   first"); it is absent only when the conflict happened on an insert against
///   a row that vanished;
/// * a **validation** failure carries the validator's errors and the schema, and
///   is produced by the validator wrapper, never by a plain storage;
/// * an **attachment** failure names the attachment whose payload was missing.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteError {
    /// `409`: `previous` did not match the stored revision.
    Conflict {
        /// Primary value of the row.
        document_id: String,
        /// The row that failed, so a caller can retry it verbatim.
        write_row: BulkWriteRow,
        /// The state actually in the database.
        document_in_db: Option<DocumentData>,
        /// The `context` of the `bulk_write` call.
        context: String,
    },
    /// `422`: the document did not validate.
    Validation {
        /// Primary value of the row.
        document_id: String,
        /// The row that failed.
        write_row: BulkWriteRow,
        /// Validator-produced errors, as plain JSON.
        validation_errors: Vec<serde_json::Value>,
        /// The schema that rejected it.
        schema: serde_json::Value,
        /// The `context` of the `bulk_write` call.
        context: String,
    },
    /// `510`: a new attachment arrived without its payload.
    Attachment {
        /// Primary value of the row.
        document_id: String,
        /// The row that failed.
        write_row: BulkWriteRow,
        /// Which attachment is missing data.
        attachment_id: String,
        /// The stored state, when the row was an update.
        document_in_db: Option<DocumentData>,
        /// The `context` of the `bulk_write` call.
        context: String,
    },
}

impl WriteError {
    /// The upstream status code.
    pub fn status(&self) -> u16 {
        match self {
            WriteError::Conflict { .. } => WRITE_ERROR_CONFLICT,
            WriteError::Validation { .. } => WRITE_ERROR_VALIDATION,
            WriteError::Attachment { .. } => WRITE_ERROR_ATTACHMENT,
        }
    }

    /// The failed row's primary value.
    pub fn document_id(&self) -> &str {
        match self {
            WriteError::Conflict { document_id, .. }
            | WriteError::Validation { document_id, .. }
            | WriteError::Attachment { document_id, .. } => document_id,
        }
    }

    /// The failed row.
    pub fn write_row(&self) -> &BulkWriteRow {
        match self {
            WriteError::Conflict { write_row, .. }
            | WriteError::Validation { write_row, .. }
            | WriteError::Attachment { write_row, .. } => write_row,
        }
    }

    /// True for a `409`, the only error a caller can normally resolve by
    /// re-reading and retrying.
    pub fn is_conflict(&self) -> bool {
        matches!(self, WriteError::Conflict { .. })
    }
}

/// The result of [`crate::RxStorageInstance::bulk_write`].
///
/// Empty `error` means every row was written. Non-empty means those rows were
/// not, and the rest were — there is no all-or-nothing mode, by design.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BulkWriteResponse {
    /// Per-row failures. Order follows write-row order.
    pub error: Vec<WriteError>,
}

impl BulkWriteResponse {
    /// Every row succeeded.
    pub fn ok() -> Self {
        BulkWriteResponse { error: Vec::new() }
    }

    /// Some rows failed.
    pub fn with_errors(error: Vec<WriteError>) -> Self {
        BulkWriteResponse { error }
    }

    /// True when no row failed.
    pub fn is_ok(&self) -> bool {
        self.error.is_empty()
    }

    /// The ids that failed, for a caller that wants to report them.
    pub fn failed_ids(&self) -> Vec<&str> {
        self.error.iter().map(WriteError::document_id).collect()
    }

    /// Reconstructs the written documents from the rows, minus the failures.
    ///
    /// This is what upstream's `getWrittenDocumentsFromBulkWriteResponse` does:
    /// the responsive thing is to send back only errors and rebuild the success
    /// set here. When a document id appears twice in `rows` (two writes to the
    /// same document in one bulk), the *last* successful row wins, matching the
    /// write order a storage applies.
    pub fn written_documents<'a>(
        &self,
        primary_path: &str,
        rows: &'a [BulkWriteRow],
    ) -> Vec<(String, &'a DocumentData)> {
        let failed: HashMap<&str, ()> = self
            .error
            .iter()
            .map(|error| (error.document_id(), ()))
            .collect();
        let mut written: Vec<(String, &'a DocumentData)> = Vec::new();
        for row in rows {
            let Ok(id) = row.document.id(primary_path) else {
                // A row with no usable id could not have been written either.
                continue;
            };
            if failed.contains_key(id) {
                continue;
            }
            written.push((id.to_string(), row.document.document()));
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentData;
    use serde_json::json;

    fn document(id: &str) -> DocumentData {
        DocumentData::from_value(json!({
            "id": id,
            "_rev": "1-a",
            "_deleted": false,
            "_meta": { "lwt": 1.01 },
            "_attachments": {}
        }))
        .expect("valid")
    }

    #[test]
    fn written_documents_excludes_the_failed_rows() {
        let rows = vec![
            BulkWriteRow::insert(DocumentWriteData::new(document("a"))),
            BulkWriteRow::insert(DocumentWriteData::new(document("b"))),
            BulkWriteRow::insert(DocumentWriteData::new(document("c"))),
        ];
        let response = BulkWriteResponse::with_errors(vec![WriteError::Conflict {
            document_id: "b".to_string(),
            write_row: rows[1].clone(),
            document_in_db: Some(document("b")),
            context: "test".to_string(),
        }]);
        let written = response.written_documents("id", &rows);
        assert_eq!(
            written.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"]
        );
    }

    #[test]
    fn error_statuses_match_upstream() {
        let row = BulkWriteRow::insert(DocumentWriteData::new(document("a")));
        assert_eq!(
            WriteError::Conflict {
                document_id: "a".into(),
                write_row: row.clone(),
                document_in_db: None,
                context: String::new(),
            }
            .status(),
            409
        );
        assert_eq!(
            WriteError::Attachment {
                document_id: "a".into(),
                write_row: row.clone(),
                attachment_id: "att".into(),
                document_in_db: None,
                context: String::new(),
            }
            .status(),
            510
        );
        assert_eq!(
            WriteError::Validation {
                document_id: "a".into(),
                write_row: row,
                validation_errors: vec![],
                schema: json!({}),
                context: String::new(),
            }
            .status(),
            422
        );
    }
}
