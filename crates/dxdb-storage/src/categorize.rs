//! `categorizeBulkWriteRows`: the heart of RxDB's consistency story.
//!
//! Ported from `src/rx-storage-helper.ts:205` as **one pure function** (PLAN.md
//! 4.3). It decides, for every row of a `bulk_write`:
//!
//! * is this an insert or an update?
//! * is it a **conflict**, and therefore not written?
//! * does it name an attachment whose payload is **missing** (status 510)?
//! * which attachments must be added, replaced or dropped?
//! * what is the event's operation — and specifically, is a write that brings a
//!   tombstone back to life an `INSERT` or an `UPDATE`?
//!
//! # Why one function
//!
//! Every storage — memory, Dexie, SQLite, MongoDB, IndexedDB — makes these same
//! decisions, every wrapper re-derives them, and the conformance suites assert
//! them by name. Upstream factored them once so an implementer cannot get
//! conflict-vs-insert wrong in a way that only surfaces under replication. This
//! port keeps that shape and adds the property the brief asks for: the function
//! is **pure**. It reads a snapshot of the stored documents and returns a
//! decision; it mutates no storage and reads no clock. That is what makes it
//! checkable against lane C's `bulk-write-categorization.json` vectors and
//! against the proptest harness.
//!
//! # The rules, verbatim
//!
//! For each row, given that row's document as stored (`None` = absent):
//!
//! | stored | `previous` | outcome |
//! | --- | --- | --- |
//! | absent | anything | **insert** — upstream explicitly allows inserting an already-deleted document, because replication does it |
//! | present | absent | **409** — you may not insert over a stored document, *including a tombstone* |
//! | present | `previous._rev != stored._rev` | **409**, carrying `document_in_db` |
//! | present | `previous._rev == stored._rev` | **update** |
//!
//! Then, only for a row not already rejected:
//!
//! * a **new** attachment with no payload, or an attachment whose digest changed
//!   with no payload, is a **510** and the row is not written;
//! * the event is `INSERT` when `previous._deleted && !document._deleted`
//!   (resurrection), `DELETE` when `document._deleted`, else `UPDATE`;
//! * an insert of an already-deleted document emits **no** event
//!   (upstream: `if (!documentDeleted)` around the insert branch).
//!
//! The update event always carries `previous`, and the DELETE event carries the
//! tombstone as its `document_data`; `rx-change-event.ts:19` reads both.
//!
//! # What this function deliberately does not do
//!
//! * It does not assign `lwt` or `_rev`. The caller (core) has already done that:
//!   upstream's `bulkWrite` receives them set, and deriving them here would make
//!   the function depend on a clock.
//! * It does not persist anything. [`CategorizeBulkWriteRowsOutput`] returns the
//!   decisions, the rows split by kind, the attachment side effects, the events,
//!   the error list and the checkpoint; applying them is the storage's job.
//! * It does not assume rows are distinct. Two rows for one document both see the
//!   *same* stored state, because the snapshot is taken before the loop. That is
//!   upstream's behaviour, and the memory storage's "should be able to jump more
//!   than 1 revision height in a single write operation" test depends on it.
//!
//! # One deliberate divergence
//!
//! An attachment whose digest changed **without** a payload is a `510` here.
//! Upstream only requires a payload for an attachment that is *new*; for an
//! existing id it writes the new digest into the stub and leaves whatever bytes
//! it had, so the stored data no longer matches its own digest. Silently
//! corrupting an attachment to match a quirk is not a behaviour worth porting,
//! and the divergence is only reachable by a caller that constructs that write by
//! hand — core never does, because it always sends the payload when the digest
//! moves.

use std::collections::BTreeMap;
use std::collections::HashMap;

use serde_json::Value;

use crate::changes::{ChangeEvent, Checkpoint, EventBulk, Operation};
use crate::document::{DocumentData, FIELD_DELETED, META_FIELD_LWT};
use crate::write::{
    BulkWriteResponse, BulkWriteRow, BulkWriteRowProcessed, WriteError, WRITE_ERROR_ATTACHMENT,
    WRITE_ERROR_CONFLICT,
};

/// JSON key of an attachment stub's digest, used often enough to name once.
const DIGEST: &str = "digest";

/// An attachment payload the storage must persist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentAdd {
    /// Owning document.
    pub document_id: String,
    /// Attachment id within that document.
    pub attachment_id: String,
    /// Bytes.
    pub data: Vec<u8>,
    /// The digest the row's stub declares for those bytes.
    pub digest: String,
}

/// An attachment the storage must delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentRemove {
    /// Owning document.
    pub document_id: String,
    /// Attachment id within that document.
    pub attachment_id: String,
    /// Digest of the content being dropped.
    pub digest: String,
}

/// An attachment whose bytes were replaced within an existing document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentUpdate {
    /// Owning document.
    pub document_id: String,
    /// Attachment id within that document.
    pub attachment_id: String,
    /// New bytes.
    pub data: Vec<u8>,
    /// New digest.
    pub digest: String,
}

/// What should happen to one row.
#[derive(Clone, Debug, PartialEq)]
pub enum RowDecision {
    /// Write as a new document.
    Insert(BulkWriteRowProcessed),
    /// Replace an existing document.
    Update(BulkWriteRowProcessed),
    /// Do not write. The row's failure, ready to return.
    Reject(WriteError),
}

impl RowDecision {
    /// True when the row will be written.
    pub fn is_write(&self) -> bool {
        !matches!(self, RowDecision::Reject(_))
    }

    /// The document that will be stored, if any.
    pub fn written(&self) -> Option<&BulkWriteRowProcessed> {
        match self {
            RowDecision::Insert(row) | RowDecision::Update(row) => Some(row),
            RowDecision::Reject(_) => None,
        }
    }
}

/// The full result of categorising one `bulk_write` call.
///
/// Field meanings match upstream's `CategorizeBulkWriteRowsOutput` so a port of a
/// storage's `bulkWrite` body is mechanical. Three additions, each earning its
/// place:
///
/// * `decisions` keeps the per-row outcome in **row order**, which upstream drops
///   after splitting into insert/update lists. A storage reporting per row and the
///   property harness checking per-document atomicity both need it.
/// * `checkpoint` is computed here, from the last written row, exactly as upstream
///   does inline (`categorized.newestRow.document`). Deriving it in the storage
///   would be a second place to get "newest" wrong.
/// * `error` is the same list the response carries, so "the storage returned the
///   errors this function produced" is one comparison.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CategorizeBulkWriteRowsOutput {
    /// One entry per input row, in input order.
    pub decisions: Vec<RowDecision>,
    /// Rows that will be inserted (stored shape, payloads stripped).
    pub bulk_insert_docs: Vec<BulkWriteRowProcessed>,
    /// Rows that will update an existing document.
    pub bulk_update_docs: Vec<BulkWriteRowProcessed>,
    /// Attachment payloads to add.
    pub attachments_add: Vec<AttachmentAdd>,
    /// Attachment ids to drop.
    pub attachments_remove: Vec<AttachmentRemove>,
    /// Attachment payloads to replace.
    pub attachments_update: Vec<AttachmentUpdate>,
    /// Events this write must emit, in row order. Empty when every row failed —
    /// and the storage must then emit **nothing at all**, per the conformance test
    /// "should not emit an empty eventBulk when the write had only errors".
    pub events: Vec<ChangeEvent>,
    /// The checkpoint for this bulk: the last successfully written row's
    /// `(primary value, _meta.lwt)`. `None` when nothing was written.
    pub checkpoint: Option<Checkpoint>,
    /// The error list to return to the caller.
    pub error: Vec<WriteError>,
}

impl CategorizeBulkWriteRowsOutput {
    /// The response a storage returns from `bulk_write`.
    pub fn response(&self) -> BulkWriteResponse {
        BulkWriteResponse::with_errors(self.error.clone())
    }

    /// True when at least one row was written.
    pub fn wrote_anything(&self) -> bool {
        !self.bulk_insert_docs.is_empty() || !self.bulk_update_docs.is_empty()
    }

    /// The event bulk to emit, or `None` when there is nothing to emit.
    ///
    /// Returning `None` rather than an empty bulk is what makes the "no empty
    /// eventBulk" rule impossible to break by forgetting a check.
    pub fn event_bulk(&self, id: impl Into<String>, context: impl Into<String>) -> Option<EventBulk> {
        if self.events.is_empty() {
            return None;
        }
        Some(EventBulk {
            id: id.into(),
            events: self.events.clone(),
            checkpoint: self.checkpoint.clone(),
            context: context.into(),
        })
    }
}

/// Categorises a bulk write against the current stored state.
///
/// * `primary_path` — the schema's primary key path.
/// * `docs_in_db` — stored state for every document the rows mention. A missing
///   key means "absent"; the caller need not pre-filter, and passing an empty map
///   makes every row an insert.
/// * `bulk_write_rows` — the rows, in the order they were given.
/// * `context` — the caller's opaque context, copied onto every error.
/// * `has_attachments` — `schema.attachments` truthiness. When false the payload
///   rules are skipped entirely, which is why a plain storage accepts attachment
///   stubs it cannot serve data for.
///
/// Pure and deterministic: same inputs, same output, no I/O, no clock.
///
/// # Examples
///
/// ```
/// # use dxdb_storage::{categorize_bulk_write_rows, BulkWriteRow, DocumentData, DocumentWriteData, Operation};
/// # use serde_json::json;
/// let document = DocumentData::from_value(json!({
///     "id": "a", "_rev": "1-t", "_deleted": false,
///     "_meta": { "lwt": 1.01 }, "_attachments": {}
/// })).expect("valid");
/// let rows = [BulkWriteRow::insert(DocumentWriteData::new(document))];
/// let out = categorize_bulk_write_rows("id", &Default::default(), &rows, "ctx", false);
/// assert_eq!(out.error.len(), 0);
/// assert_eq!(out.bulk_insert_docs.len(), 1);
/// assert_eq!(out.events[0].operation, Operation::Insert);
/// ```
pub fn categorize_bulk_write_rows(
    primary_path: &str,
    docs_in_db: &HashMap<String, DocumentData>,
    bulk_write_rows: &[BulkWriteRow],
    context: &str,
    has_attachments: bool,
) -> CategorizeBulkWriteRowsOutput {
    let rules = Rules {
        primary_path,
        context,
        has_attachments,
    };
    let mut out = CategorizeBulkWriteRowsOutput {
        decisions: Vec::with_capacity(bulk_write_rows.len()),
        ..Default::default()
    };

    for write_row in bulk_write_rows {
        let (decision, side_effects) = rules.categorize(write_row, docs_in_db);
        match &decision {
            RowDecision::Insert(row) => out.bulk_insert_docs.push(row.clone()),
            RowDecision::Update(row) => out.bulk_update_docs.push(row.clone()),
            RowDecision::Reject(error) => out.error.push(error.clone()),
        }
        // The checkpoint is the *last written* row, including an insert of a
        // tombstone (which writes but emits no event). Computing it from the
        // decision, not from the events, is what keeps those two cases apart.
        if let Some(row) = decision.written() {
            if let (Ok(id), Some(lwt)) = (row.document.id(primary_path), row.document.lwt()) {
                out.checkpoint = Some(Checkpoint::new(id, lwt));
            }
        }
        out.attachments_add.extend(side_effects.add);
        out.attachments_remove.extend(side_effects.remove);
        out.attachments_update.extend(side_effects.update);
        out.events.extend(side_effects.events);
        out.decisions.push(decision);
    }

    out
}

/// Attachment and event work produced by one row.
#[derive(Default)]
struct SideEffects {
    add: Vec<AttachmentAdd>,
    remove: Vec<AttachmentRemove>,
    update: Vec<AttachmentUpdate>,
    events: Vec<ChangeEvent>,
}

/// The invariant parts of a categorisation call.
struct Rules<'a> {
    primary_path: &'a str,
    context: &'a str,
    has_attachments: bool,
}

impl Rules<'_> {
    fn categorize(
        &self,
        write_row: &BulkWriteRow,
        docs_in_db: &HashMap<String, DocumentData>,
    ) -> (RowDecision, SideEffects) {
        let document = write_row.document.document();
        // A row with no usable primary value cannot be written or indexed: the
        // storage would have to invent a key, and every later read would miss the
        // document. Rejecting it produces a diagnosable failure instead.
        let Ok(doc_id) = document.id(self.primary_path) else {
            let reason = document
                .id(self.primary_path)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            return (
                RowDecision::Reject(WriteError::Conflict {
                    document_id: String::new(),
                    write_row: write_row.clone(),
                    document_in_db: None,
                    context: format!("{} ({reason})", self.context),
                }),
                SideEffects::default(),
            );
        };
        let doc_id = doc_id.to_string();
        let stored = docs_in_db.get(&doc_id);
        let previous = write_row.previous.as_ref();

        let decision = match stored {
            None => {
                if let Some(error) = self.insert_attachment_error(write_row, &doc_id) {
                    RowDecision::Reject(error)
                } else {
                    RowDecision::Insert(BulkWriteRowProcessed::new(
                        write_row.previous.clone(),
                        document.clone(),
                    ))
                }
            }
            Some(document_in_db) => {
                // Conflict unless `previous` provably is the stored revision.
                // A missing `previous` conflicts even against a tombstone: that
                // is the rule a naive port drops, and the conformance suite tests
                // it by name.
                let previous_is_stored = match previous {
                    Some(previous) => {
                        matches!(
                            (previous.rev(), document_in_db.rev()),
                            (Some(a), Some(b)) if a == b
                        )
                    }
                    None => false,
                };
                if !previous_is_stored {
                    RowDecision::Reject(WriteError::Conflict {
                        document_id: doc_id.clone(),
                        write_row: write_row.clone(),
                        document_in_db: Some(document_in_db.clone()),
                        context: self.context.to_string(),
                    })
                } else if let Some(error) =
                    self.update_attachment_error(write_row, document_in_db, &doc_id)
                {
                    RowDecision::Reject(error)
                } else {
                    RowDecision::Update(BulkWriteRowProcessed::new(
                        write_row.previous.clone(),
                        document.clone(),
                    ))
                }
            }
        };

        let side_effects = match &decision {
            RowDecision::Insert(_) => self.collect_insert(write_row, &doc_id),
            RowDecision::Update(_) => self.collect_update(write_row, &doc_id),
            RowDecision::Reject(_) => SideEffects::default(),
        };
        (decision, side_effects)
    }

    /// A new attachment with no payload is a 510.
    fn insert_attachment_error(&self, write_row: &BulkWriteRow, doc_id: &str) -> Option<WriteError> {
        if !self.has_attachments {
            return None;
        }
        let attachments = write_row.document.document().attachments()?;
        attachments.keys().find_map(|attachment_id| {
            (!write_row.document.has_attachment_payload(attachment_id)).then(|| {
                self.attachment_error(write_row, doc_id, attachment_id, None)
            })
        })
    }

    /// Update-side attachment rule: a *new* id, or a changed digest, needs bytes.
    fn update_attachment_error(
        &self,
        write_row: &BulkWriteRow,
        document_in_db: &DocumentData,
        doc_id: &str,
    ) -> Option<WriteError> {
        if !self.has_attachments {
            return None;
        }
        let previous = write_row.previous.as_ref()?;
        let document = write_row.document.document();
        // A delete clears every attachment, so no payload rule applies.
        if document.is_deleted() {
            return None;
        }
        let empty = serde_json::Map::new();
        let document_attachments = document.attachments().unwrap_or(&empty);
        let previous_attachments = previous.attachments().unwrap_or(&empty);

        document_attachments
            .iter()
            .find_map(|(attachment_id, stub)| {
                let new_digest = stub.get(DIGEST).and_then(Value::as_str);
                let needs_payload = match previous_attachments.get(attachment_id) {
                    None => true,
                    Some(previous_stub) => {
                        previous_stub.get(DIGEST).and_then(Value::as_str) != new_digest
                    }
                };
                (needs_payload && !write_row.document.has_attachment_payload(attachment_id)).then(
                    || {
                        self.attachment_error(
                            write_row,
                            doc_id,
                            attachment_id,
                            Some(document_in_db.clone()),
                        )
                    },
                )
            })
    }

    fn attachment_error(
        &self,
        write_row: &BulkWriteRow,
        doc_id: &str,
        attachment_id: &str,
        document_in_db: Option<DocumentData>,
    ) -> WriteError {
        WriteError::Attachment {
            document_id: doc_id.to_string(),
            write_row: write_row.clone(),
            attachment_id: attachment_id.to_string(),
            document_in_db,
            context: self.context.to_string(),
        }
    }

    fn collect_insert(&self, write_row: &BulkWriteRow, doc_id: &str) -> SideEffects {
        let document = write_row.document.document();
        let mut side_effects = SideEffects::default();

        if self.has_attachments {
            if let Some(attachments) = document.attachments() {
                for attachment_id in attachments.keys() {
                    if let Some(payload) = write_row.document.attachment_payload(attachment_id) {
                        side_effects.add.push(AttachmentAdd {
                            document_id: doc_id.to_string(),
                            attachment_id: attachment_id.clone(),
                            data: payload.to_vec(),
                            digest: digest_of(attachments.get(attachment_id)),
                        });
                    }
                }
            }
        }

        // Upstream emits no event for the insert of an already-deleted document.
        if !document.is_deleted() {
            side_effects.events.push(ChangeEvent {
                document_id: doc_id.to_string(),
                operation: Operation::Insert,
                document_data: document.clone(),
                previous_document_data: write_row.previous.clone(),
            });
        }
        side_effects
    }

    fn collect_update(&self, write_row: &BulkWriteRow, doc_id: &str) -> SideEffects {
        let document = write_row.document.document();
        let previous = write_row.previous.as_ref();
        let mut side_effects = SideEffects::default();

        if self.has_attachments {
            let empty = serde_json::Map::new();
            let previous_attachments = previous
                .and_then(DocumentData::attachments)
                .unwrap_or(&empty);
            if document.is_deleted() {
                // A tombstone drops every attachment it had.
                for attachment_id in previous_attachments.keys() {
                    side_effects.remove.push(AttachmentRemove {
                        document_id: doc_id.to_string(),
                        attachment_id: attachment_id.clone(),
                        digest: digest_of(previous_attachments.get(attachment_id)),
                    });
                }
            } else {
                let document_attachments = document.attachments().unwrap_or(&empty);
                for (attachment_id, stub) in document_attachments {
                    let new_digest = digest_of(Some(stub));
                    match previous_attachments.get(attachment_id) {
                        None => {
                            if let Some(payload) =
                                write_row.document.attachment_payload(attachment_id)
                            {
                                side_effects.add.push(AttachmentAdd {
                                    document_id: doc_id.to_string(),
                                    attachment_id: attachment_id.clone(),
                                    data: payload.to_vec(),
                                    digest: new_digest,
                                });
                            }
                        }
                        Some(previous_stub) => {
                            // Upstream's documented shortcut: unchanged digest
                            // means no write, even if a payload was supplied.
                            if digest_of(Some(previous_stub)) != new_digest {
                                if let Some(payload) =
                                    write_row.document.attachment_payload(attachment_id)
                                {
                                    side_effects.update.push(AttachmentUpdate {
                                        document_id: doc_id.to_string(),
                                        attachment_id: attachment_id.clone(),
                                        data: payload.to_vec(),
                                        digest: new_digest,
                                    });
                                }
                            }
                        }
                    }
                }
                for attachment_id in previous_attachments.keys() {
                    if !document_attachments.contains_key(attachment_id) {
                        side_effects.remove.push(AttachmentRemove {
                            document_id: doc_id.to_string(),
                            attachment_id: attachment_id.clone(),
                            digest: digest_of(previous_attachments.get(attachment_id)),
                        });
                    }
                }
            }
        }

        let previous_deleted = previous.map(DocumentData::is_deleted).unwrap_or(false);
        let document_deleted = document.is_deleted();
        // Resurrection is an INSERT: event-reduce above drops an UPDATE for a
        // document the live query does not currently hold.
        let operation = if previous_deleted && !document_deleted {
            Operation::Insert
        } else if document_deleted {
            Operation::Delete
        } else {
            Operation::Update
        };

        side_effects.events.push(ChangeEvent {
            document_id: doc_id.to_string(),
            operation,
            document_data: document.clone(),
            previous_document_data: previous.cloned(),
        });
        side_effects
    }
}

fn digest_of(stub: Option<&Value>) -> String {
    stub.and_then(|stub| stub.get(DIGEST))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Payloads grouped by document, for a storage that writes them in one pass.
pub fn payloads_by_document(
    output: &CategorizeBulkWriteRowsOutput,
) -> BTreeMap<String, Vec<AttachmentAdd>> {
    let mut grouped: BTreeMap<String, Vec<AttachmentAdd>> = BTreeMap::new();
    for add in &output.attachments_add {
        grouped
            .entry(add.document_id.clone())
            .or_default()
            .push(add.clone());
    }
    for update in &output.attachments_update {
        grouped
            .entry(update.document_id.clone())
            .or_default()
            .push(AttachmentAdd {
                document_id: update.document_id.clone(),
                attachment_id: update.attachment_id.clone(),
                data: update.data.clone(),
                digest: update.digest.clone(),
            });
    }
    grouped
}

/// Reads `_meta.lwt` from a document, defaulting to 0.
///
/// Every storage needs this when building a checkpoint, and the literal `"lwt"`
/// should appear once in the workspace.
pub fn document_lwt(document: &DocumentData) -> f64 {
    document.lwt().unwrap_or(0.0)
}

/// The status codes this module can produce, re-exported for storages.
pub const CONFLICT_STATUS: u16 = WRITE_ERROR_CONFLICT;
/// See [`CONFLICT_STATUS`].
pub const ATTACHMENT_STATUS: u16 = WRITE_ERROR_ATTACHMENT;

/// Re-exported so a caller can name the meta key without a literal.
pub const LWT_FIELD: &str = META_FIELD_LWT;
/// Re-exported so a caller can name the deleted flag without a literal.
pub const DELETED_FIELD: &str = FIELD_DELETED;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{AttachmentData, AttachmentWriteData, DocumentWriteData};
    use serde_json::json;

    const CONTEXT: &str = "categorize-test";

    fn document(id: &str, rev: &str, deleted: bool, lwt: f64) -> DocumentData {
        DocumentData::from_value(json!({
            "id": id,
            "_rev": rev,
            "_deleted": deleted,
            "_meta": { "lwt": lwt },
            "_attachments": {}
        }))
        .expect("valid document")
    }

    fn write_row(document: DocumentData) -> BulkWriteRow {
        BulkWriteRow::insert(DocumentWriteData::new(document))
    }

    fn update_row(previous: DocumentData, document: DocumentData) -> BulkWriteRow {
        BulkWriteRow::update(previous, DocumentWriteData::new(document))
    }

    fn db(entries: &[(&str, DocumentData)]) -> HashMap<String, DocumentData> {
        entries
            .iter()
            .map(|(id, doc)| ((*id).to_string(), doc.clone()))
            .collect()
    }

    #[test]
    fn absent_row_is_an_insert_with_an_insert_event() {
        let doc = document("a", "1-t", false, 1.01);
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[]),
            &[write_row(doc.clone())],
            CONTEXT,
            false,
        );
        assert_eq!(out.error, vec![]);
        assert_eq!(out.bulk_insert_docs.len(), 1);
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].operation, Operation::Insert);
        assert_eq!(out.events[0].document_id, "a");
        assert_eq!(out.events[0].previous_document_data, None);
        assert_eq!(out.checkpoint, Some(Checkpoint::new("a", 1.01)));
    }

    #[test]
    fn inserting_an_already_deleted_document_is_allowed_and_emits_no_event() {
        // Replication does this: a tombstone arrives for a document this node
        // never had. Upstream writes it and emits nothing.
        let doc = document("a", "2-t", true, 2.02);
        let out = categorize_bulk_write_rows("id", &db(&[]), &[write_row(doc)], CONTEXT, false);
        assert_eq!(out.error, vec![]);
        assert_eq!(out.bulk_insert_docs.len(), 1);
        assert!(out.events.is_empty(), "no event for a deleted insert");
        // The row was written, so a checkpoint exists even without events.
        assert_eq!(out.checkpoint, Some(Checkpoint::new("a", 2.02)));
        assert!(out.event_bulk("bulk-1", CONTEXT).is_none());
    }

    #[test]
    fn a_write_without_previous_conflicts_against_a_live_document() {
        let stored = document("a", "1-t", false, 1.01);
        let incoming = document("a", "1-t", false, 1.02);
        let out =
            categorize_bulk_write_rows("id", &db(&[("a", stored)]), &[write_row(incoming)], CONTEXT, false);
        assert_eq!(out.bulk_insert_docs.len(), 0);
        assert_eq!(out.bulk_update_docs.len(), 0);
        assert_eq!(out.error.len(), 1);
        assert_eq!(out.error[0].status(), CONFLICT_STATUS);
        match &out.error[0] {
            WriteError::Conflict {
                document_in_db, ..
            } => assert_eq!(document_in_db.as_ref(), Some(&document("a", "1-t", false, 1.01))),
            other => panic!("expected conflict, got {other:?}"),
        }
        assert!(out.events.is_empty());
        assert_eq!(out.checkpoint, None);
    }

    #[test]
    fn a_write_without_previous_conflicts_against_a_tombstone_too() {
        // The rule a naive port drops: an insert over a *deleted* document is
        // still a conflict without `previous`.
        let stored = document("a", "2-t", true, 2.02);
        let incoming = document("a", "3-t", false, 3.03);
        let out =
            categorize_bulk_write_rows("id", &db(&[("a", stored)]), &[write_row(incoming)], CONTEXT, false);
        assert_eq!(out.error.len(), 1);
        assert_eq!(out.error[0].status(), CONFLICT_STATUS);
    }

    #[test]
    fn a_stale_previous_revision_conflicts() {
        let stored = document("a", "3-t", false, 3.03);
        let stale = document("a", "1-t", false, 1.01);
        let incoming = document("a", "2-t", false, 2.02);
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored)]),
            &[update_row(stale, incoming)],
            CONTEXT,
            false,
        );
        assert_eq!(out.error.len(), 1);
        assert_eq!(out.error[0].status(), CONFLICT_STATUS);
    }

    #[test]
    fn a_matching_previous_revision_updates_and_carries_previous() {
        let stored = document("a", "1-t", false, 1.01);
        let incoming = document("a", "2-t", false, 2.02);
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored.clone())]),
            &[update_row(stored.clone(), incoming)],
            CONTEXT,
            false,
        );
        assert_eq!(out.error, vec![]);
        assert_eq!(out.bulk_update_docs.len(), 1);
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].operation, Operation::Update);
        assert_eq!(out.events[0].previous_document_data, Some(stored));
        assert_eq!(out.checkpoint, Some(Checkpoint::new("a", 2.02)));
    }

    #[test]
    fn resurrecting_a_tombstone_reports_insert_not_update() {
        let stored = document("a", "2-t", true, 2.02);
        let incoming = document("a", "3-t", false, 3.03);
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored.clone())]),
            &[update_row(stored, incoming)],
            CONTEXT,
            false,
        );
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].operation, Operation::Insert);
    }

    #[test]
    fn deleting_reports_delete_and_carries_the_tombstone() {
        let stored = document("a", "1-t", false, 1.01);
        let tombstone = document("a", "2-t", true, 2.02);
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored.clone())]),
            &[update_row(stored.clone(), tombstone.clone())],
            CONTEXT,
            false,
        );
        assert_eq!(out.events[0].operation, Operation::Delete);
        assert_eq!(out.events[0].document_data, tombstone);
        assert_eq!(out.events[0].previous_document_data, Some(stored));
    }

    #[test]
    fn a_bulk_of_mixed_outcomes_writes_the_good_rows_and_reports_the_rest() {
        let stored_a = document("a", "1-t", false, 1.01);
        let stored_b = document("b", "1-t", false, 1.02);
        let rows = vec![
            // insert (absent)
            write_row(document("c", "1-t", false, 1.03)),
            // conflict (previous missing)
            write_row(document("a", "2-t", false, 2.01)),
            // update (previous matches)
            update_row(
                stored_b.clone(),
                document("b", "2-t", false, 2.02),
            ),
        ];
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored_a), ("b", stored_b)]),
            &rows,
            CONTEXT,
            false,
        );
        assert_eq!(out.bulk_insert_docs.len(), 1);
        assert_eq!(out.bulk_update_docs.len(), 1);
        assert_eq!(out.error.len(), 1);
        assert_eq!(out.error[0].document_id(), "a");
        // Events follow row order, minus the rejected row.
        assert_eq!(
            out.events
                .iter()
                .map(|event| (event.document_id.as_str(), event.operation))
                .collect::<Vec<_>>(),
            vec![("c", Operation::Insert), ("b", Operation::Update)]
        );
        // The checkpoint is the last *written* row, not the last row.
        assert_eq!(out.checkpoint, Some(Checkpoint::new("b", 2.02)));
        assert_eq!(out.decisions.len(), 3);
        assert!(!out.decisions[1].is_write());
    }

    #[test]
    fn a_write_of_only_errors_produces_no_event_bulk() {
        let stored = document("a", "1-t", false, 1.01);
        let out =
            categorize_bulk_write_rows("id", &db(&[("a", stored)]), &[write_row(document("a", "2-t", false, 2.0))], CONTEXT, false);
        assert!(!out.wrote_anything());
        assert!(out.events.is_empty());
        assert!(out.event_bulk("bulk", CONTEXT).is_none());
    }

    #[test]
    fn two_rows_for_one_document_both_see_the_same_stored_state() {
        // Upstream reads the state once, so the second row must still match it.
        let stored = document("a", "1-t", false, 1.01);
        let rows = vec![
            update_row(stored.clone(), document("a", "2-t", false, 2.02)),
            update_row(document("a", "2-t", false, 2.02), document("a", "3-t", false, 3.03)),
        ];
        let out =
            categorize_bulk_write_rows("id", &db(&[("a", stored)]), &rows, CONTEXT, false);
        // The first row is a valid update; the second's `previous` (2-t) is not
        // what the snapshot holds (1-t), so it conflicts.
        assert_eq!(out.bulk_update_docs.len(), 1);
        assert_eq!(out.error.len(), 1);
        assert_eq!(out.error[0].status(), CONFLICT_STATUS);
    }

    #[test]
    fn a_missing_primary_key_is_rejected_rather_than_written_under_an_empty_id() {
        let orphan = DocumentData::from_value(json!({
            "_rev": "1-t", "_deleted": false, "_meta": { "lwt": 1.0 }, "_attachments": {}
        }))
        .expect("valid object");
        let out = categorize_bulk_write_rows("id", &db(&[]), &[write_row(orphan)], CONTEXT, false);
        assert!(out.bulk_insert_docs.is_empty());
        assert_eq!(out.error.len(), 1);
    }

    // ----- attachments -----

    fn with_attachment(
        mut document: DocumentData,
        attachment_id: &str,
        digest: &str,
        payload: Option<&[u8]>,
    ) -> DocumentWriteData {
        let mut write = DocumentWriteData::new({
            document.set("_attachments", json!({}));
            document
        });
        write.insert_attachment(
            attachment_id,
            AttachmentWriteData {
                stub: AttachmentData {
                    length: payload.map(|p| p.len() as u64).unwrap_or(0),
                    content_type: "text/plain".to_string(),
                    digest: digest.to_string(),
                },
                data: payload.map(<[u8]>::to_vec),
            },
        );
        write
    }

    #[test]
    fn a_new_attachment_without_a_payload_is_a_510() {
        let doc = document("a", "1-t", false, 1.01);
        let row = BulkWriteRow::insert(with_attachment(doc, "att", "sha-1", None));
        let out = categorize_bulk_write_rows("id", &db(&[]), &[row], CONTEXT, true);
        assert_eq!(out.error.len(), 1);
        assert_eq!(out.error[0].status(), ATTACHMENT_STATUS);
        match &out.error[0] {
            WriteError::Attachment { attachment_id, .. } => assert_eq!(attachment_id, "att"),
            other => panic!("expected attachment error, got {other:?}"),
        }
        assert!(out.bulk_insert_docs.is_empty());
    }

    #[test]
    fn a_new_attachment_with_a_payload_is_written_and_recorded() {
        let doc = document("a", "1-t", false, 1.01);
        let row = BulkWriteRow::insert(with_attachment(doc, "att", "sha-1", Some(b"hi")));
        let out = categorize_bulk_write_rows("id", &db(&[]), &[row], CONTEXT, true);
        assert_eq!(out.error, vec![]);
        assert_eq!(
            out.attachments_add,
            vec![AttachmentAdd {
                document_id: "a".to_string(),
                attachment_id: "att".to_string(),
                data: b"hi".to_vec(),
                digest: "sha-1".to_string(),
            }]
        );
    }

    #[test]
    fn deleting_a_document_removes_all_of_its_attachments() {
        let mut stored = document("a", "1-t", false, 1.01);
        stored.set(
            "_attachments",
            json!({ "att": { "length": 2, "type": "text/plain", "digest": "sha-1" } }),
        );
        let tombstone = document("a", "2-t", true, 2.02);
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored.clone())]),
            &[update_row(stored, tombstone)],
            CONTEXT,
            true,
        );
        assert_eq!(out.error, vec![]);
        assert_eq!(
            out.attachments_remove,
            vec![AttachmentRemove {
                document_id: "a".to_string(),
                attachment_id: "att".to_string(),
                digest: "sha-1".to_string(),
            }]
        );
    }

    #[test]
    fn an_unchanged_attachment_digest_does_not_rewrite_the_bytes() {
        let mut stored = document("a", "1-t", false, 1.01);
        stored.set(
            "_attachments",
            json!({ "att": { "length": 2, "type": "text/plain", "digest": "sha-1" } }),
        );
        // The caller sends the stub again, plus a payload (a wasteful caller).
        let mut incoming = DocumentWriteData::new(document("a", "2-t", false, 2.02));
        incoming.insert_attachment(
            "att",
            AttachmentWriteData {
                stub: AttachmentData {
                    length: 2,
                    content_type: "text/plain".to_string(),
                    digest: "sha-1".to_string(),
                },
                data: Some(b"hi".to_vec()),
            },
        );
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored.clone())]),
            &[BulkWriteRow::update(stored, incoming)],
            CONTEXT,
            true,
        );
        assert_eq!(out.error, vec![]);
        // Upstream's documented shortcut: no attachment write at all.
        assert!(out.attachments_add.is_empty());
        assert!(out.attachments_update.is_empty());
        assert!(out.attachments_remove.is_empty());
    }

    #[test]
    fn a_changed_digest_with_a_payload_is_an_attachment_update() {
        let mut stored = document("a", "1-t", false, 1.01);
        stored.set(
            "_attachments",
            json!({ "att": { "length": 2, "type": "text/plain", "digest": "sha-1" } }),
        );
        let mut incoming = DocumentWriteData::new(document("a", "2-t", false, 2.02));
        incoming.insert_attachment(
            "att",
            AttachmentWriteData {
                stub: AttachmentData {
                    length: 3,
                    content_type: "text/plain".to_string(),
                    digest: "sha-2".to_string(),
                },
                data: Some(b"new".to_vec()),
            },
        );
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored.clone())]),
            &[BulkWriteRow::update(stored, incoming)],
            CONTEXT,
            true,
        );
        assert_eq!(out.error, vec![]);
        assert_eq!(
            out.attachments_update,
            vec![AttachmentUpdate {
                document_id: "a".to_string(),
                attachment_id: "att".to_string(),
                data: b"new".to_vec(),
                digest: "sha-2".to_string(),
            }]
        );
    }

    #[test]
    fn a_dropped_attachment_is_removed() {
        let mut stored = document("a", "1-t", false, 1.01);
        stored.set(
            "_attachments",
            json!({
                "gone": { "length": 2, "type": "text/plain", "digest": "sha-1" },
                "kept": { "length": 2, "type": "text/plain", "digest": "sha-2" }
            }),
        );
        let mut incoming = DocumentWriteData::new(document("a", "2-t", false, 2.02));
        incoming.insert_attachment(
            "kept",
            AttachmentWriteData {
                stub: AttachmentData {
                    length: 2,
                    content_type: "text/plain".to_string(),
                    digest: "sha-2".to_string(),
                },
                data: None,
            },
        );
        let out = categorize_bulk_write_rows(
            "id",
            &db(&[("a", stored.clone())]),
            &[BulkWriteRow::update(stored, incoming)],
            CONTEXT,
            true,
        );
        assert_eq!(out.error, vec![]);
        assert_eq!(
            out.attachments_remove
                .iter()
                .map(|remove| remove.attachment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["gone"]
        );
    }

    #[test]
    fn attachment_rules_are_skipped_entirely_without_a_schema_declaring_them() {
        let doc = document("a", "1-t", false, 1.01);
        let row = BulkWriteRow::insert(with_attachment(doc, "att", "sha-1", None));
        let out = categorize_bulk_write_rows("id", &db(&[]), &[row], CONTEXT, false);
        // Upstream does the same: without `schema.attachments`, payloads are not
        // a storage concern.
        assert_eq!(out.error, vec![]);
        assert_eq!(out.bulk_insert_docs.len(), 1);
        assert!(out.attachments_add.is_empty());
    }

    #[test]
    fn is_pure_and_repeatable() {
        let stored = document("a", "1-t", false, 1.01);
        let rows = vec![
            write_row(document("b", "1-t", false, 1.02)),
            update_row(stored.clone(), document("a", "2-t", false, 2.0)),
        ];
        let state = db(&[("a", stored)]);
        let first = categorize_bulk_write_rows("id", &state, &rows, CONTEXT, false);
        let second = categorize_bulk_write_rows("id", &state, &rows, CONTEXT, false);
        assert_eq!(first, second);
    }
}
