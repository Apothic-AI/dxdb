//! `bulk-write-categorization.json` — `categorizeBulkWriteRows` output.
//!
//! This is the function every RxStorage implementation funnels its writes through,
//! so its output shape is the write contract: which rows become inserts, which
//! become updates, which become conflicts, and exactly which change events come out.
//!
//! Reading notes:
//!
//! - **`previous` is optional in the input and required in the output.** A caller may
//!   omit it; upstream's processed rows always carry a key, sometimes `null`. Both
//!   forms are modelled, because "omitted" and "present but null" are different
//!   inputs upstream treats differently in the conflict check.
//! - **Documents are arbitrary JSON.** They are stored documents with meta fields, so
//!   they are [`serde_json::Value`]; the family's own envelope is what is typed.
//! - **`newestRow` is nullable.** An entirely-conflicting batch has no successful row,
//!   and the event bulk's checkpoint is derived from it.
//! - **The `eventBulk.id` is a `randomToken()`.** Deterministic only because the
//!   generator seeds `Math.random`; do not assert a literal value, use it to check
//!   that ids are stable across documents.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::FamilyFile;

/// The decoded `bulk-write-categorization.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vectors {
    /// The documentation comment the generator wrote into the file.
    #[serde(rename = "$comment")]
    pub comment: String,
    /// The generator that wrote the file, e.g. `xtask/vectors/generate.mjs`.
    pub generator: String,
    /// Informational Node version; `--check` ignores it, so it may differ locally.
    pub node: String,
    /// Upstream RxDB version the vectors came from. Absent for `jsnum`, which only
    /// needs Node.
    #[serde(default, rename = "rxdbVersion")]
    pub rxdb_version: Option<String>,
    /// Upstream RxDB commit the vectors came from. Same condition as `rxdb_version`.
    #[serde(default, rename = "rxdbCommit")]
    pub rxdb_commit: Option<String>,
    /// The recorded product, e.g. `docsInDb+writeRows->{...}`.
    pub format: String,
    /// The determinism stubs in force when the file was generated.
    pub determinism: Determinism,
    /// The primary key field name of every document in this family.
    #[serde(rename = "primaryPath")]
    pub primary_path: String,
    /// Number of entries in [`Vectors::cases`].
    #[serde(rename = "caseCount")]
    pub case_count: usize,
    /// The cases themselves.
    pub cases: Vec<Case>,
}

impl FamilyFile for Vectors {
    fn provenance(&self) -> crate::Provenance {
        crate::Provenance {
            comment: self.comment.clone(),
            generator: self.generator.clone(),
            node: self.node.clone(),
            rxdb_version: self.rxdb_version.clone(),
            rxdb_commit: self.rxdb_commit.clone(),
        }
    }
}

impl Vectors {
    /// The provenance header of this file.
    ///
    /// Inherent rather than trait-only so a consumer can call it without importing
    /// [`FamilyFile`]; the trait exists for the loader's generic bound.
    pub fn provenance(&self) -> crate::Provenance {
        FamilyFile::provenance(self)
    }
}

impl Vectors {
    /// Look up a case by its label, e.g. `fresh-insert`.
    pub fn case(&self, why: &str) -> Option<&Case> {
        self.cases.iter().find(|c| c.why == why)
    }
}

/// The determinism stubs the generator installed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Determinism {
    /// How `Math.random` was replaced.
    #[serde(rename = "mathRandom")]
    pub math_random: String,
    /// How `Date.now` was replaced.
    #[serde(rename = "dateNow")]
    pub date_now: String,
    /// Why `eventBulk.id` is stable despite being randomly generated.
    #[serde(rename = "eventBulkId")]
    pub event_bulk_id: String,
    /// Why `context` is stable: it is the caller's label, echoed back.
    pub context: String,
}

/// One categorisation run: a storage state in, a categorised batch out.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Which schema the storage instance had, e.g. `human` or
    /// `human+attachments`. The attachment branches only run when the schema
    /// declares `attachments`.
    pub schema: String,
    /// The scenario label, e.g. `revive-deleted-document-is-insert-event`.
    pub why: String,
    /// The bulk-write context, echoed into every error and into the event bulk.
    pub context: String,
    /// The documents already in the storage, keyed by id, as the `Map` upstream
    /// receives.
    #[serde(rename = "docsInDb")]
    pub docs_in_db: BTreeMap<String, serde_json::Value>,
    /// The rows passed to `bulkWrite`.
    #[serde(rename = "writeRows")]
    pub write_rows: Vec<UnprocessedWriteRow>,
    /// Rows to insert, in input order.
    #[serde(rename = "bulkInsertDocs")]
    pub bulk_insert_docs: Vec<ProcessedWriteRow>,
    /// Rows to update, in input order.
    #[serde(rename = "bulkUpdateDocs")]
    pub bulk_update_docs: Vec<ProcessedWriteRow>,
    /// The last successfully written row, or `null` when nothing was written. The
    /// event bulk's checkpoint is derived from it.
    #[serde(rename = "newestRow")]
    pub newest_row: Option<serde_json::Value>,
    /// Write errors: `409` conflicts and `510` attachment errors.
    pub errors: Vec<WriteError>,
    /// Change events to emit, in input order.
    pub events: Vec<Event>,
    /// The event bulk envelope the events were emitted in.
    #[serde(rename = "eventBulk")]
    pub event_bulk: EventBulk,
    /// Attachments to store.
    #[serde(rename = "attachmentsAdd")]
    pub attachments_add: Vec<AttachmentChange>,
    /// Attachments to drop.
    #[serde(rename = "attachmentsRemove")]
    pub attachments_remove: Vec<AttachmentChange>,
    /// Attachments whose digest changed.
    #[serde(rename = "attachmentsUpdate")]
    pub attachments_update: Vec<AttachmentChange>,
}

/// A write row as the caller supplied it. `previous` may be absent entirely.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnprocessedWriteRow {
    /// The stored document to write.
    pub document: serde_json::Value,
    /// The revision the caller believes is in the storage. Absent means "no
    /// assumption", which upstream treats as a conflict for an existing document.
    #[serde(default)]
    pub previous: Option<serde_json::Value>,
}

/// A write row after upstream processed it. Both keys are always present here.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessedWriteRow {
    /// The stored document, with attachment write data stripped when applicable.
    pub document: serde_json::Value,
    /// The previous revision, or `null` for an insert.
    pub previous: Option<serde_json::Value>,
}

/// A write error returned to the caller.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteError {
    /// Always `true`; present so a reader can distinguish errors from rows.
    #[serde(rename = "isError")]
    pub is_error: bool,
    /// `409` for a revision conflict, `510` for a broken attachment write.
    pub status: u16,
    /// The document id that failed.
    #[serde(rename = "documentId")]
    pub document_id: String,
    /// The attachment id for `510` errors, else `null`.
    #[serde(rename = "attachmentId")]
    pub attachment_id: Option<String>,
    /// The document currently in the storage, for `409` errors.
    #[serde(rename = "documentInDb")]
    pub document_in_db: Option<serde_json::Value>,
    /// The document the caller tried to write. Recorded by value rather than as a
    /// row index, because upstream hands back the caller's row object.
    #[serde(rename = "writeRowDocument")]
    pub write_row_document: serde_json::Value,
}

/// A change event to emit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// The document id the event concerns.
    #[serde(rename = "documentId")]
    pub document_id: String,
    /// `INSERT`, `UPDATE` or `DELETE`.
    ///
    /// A write that un-deletes a tombstoned document is an `INSERT`, not an
    /// `UPDATE`; that distinction is what downstream replications branch on, so it
    /// is kept as the raw string rather than an enum that might tempt a reader into
    /// inferring it from the row.
    pub operation: String,
    /// The document state after the write.
    #[serde(rename = "documentData")]
    pub document_data: serde_json::Value,
    /// The document state before the write, or `null` for an insert.
    #[serde(rename = "previousDocumentData")]
    pub previous_document_data: Option<serde_json::Value>,
}

impl Event {
    /// True for the insert/update/delete operations upstream defines.
    pub fn is_known_operation(&self) -> bool {
        matches!(self.operation.as_str(), "INSERT" | "UPDATE" | "DELETE")
    }
}

/// The event-bulk envelope.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventBulk {
    /// A `randomToken(10)` value. Stable only because the generator seeds
    /// `Math.random`; assert stability, not a literal.
    pub id: String,
    /// The context passed to `bulkWrite`, echoed back.
    pub context: String,
    /// The checkpoint for this bulk. `null` from `categorizeBulkWriteRows` itself;
    /// the storage stamps it before emitting.
    pub checkpoint: Option<serde_json::Value>,
}

/// One attachment change.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentChange {
    /// The document the attachment belongs to.
    #[serde(rename = "documentId")]
    pub document_id: String,
    /// The attachment id, e.g. `file.txt`.
    #[serde(rename = "attachmentId")]
    pub attachment_id: String,
    /// The attachment digest.
    pub digest: String,
}
