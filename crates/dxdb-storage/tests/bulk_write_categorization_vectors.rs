//! The committed `bulk-write-categorization` oracle, read through `dxdb-vectors`.
//!
//! # Two oracles, and what each is for
//!
//! This crate checks `categorize_bulk_write_rows` against **two** independent recordings of
//! upstream's behaviour, and they answer different questions. Neither is a stand-in for the
//! other, and neither is provisional:
//!
//! | | `testdata/vectors/bulk-write-categorization.json` (this file) | `testdata/cases-{1,2,3}.json` (`tests/categorize_oracle.rs`) |
//! | --- | --- | --- |
//! | Cases | 47, curated: one per rule, with a readable `why` | 600, randomised sequences |
//! | Regenerated | Every `make vectors` run, from upstream at the pinned commit | Never; a static recording |
//! | Answers | "Does the port still match upstream *today*?" — catches **upstream drift**, and a change in the generator | "Does the port handle the combinations nobody wrote a case for?" |
//! | Fails on | a rule whose behaviour upstream changed, or a generator regression | a bug in a multi-row / multi-error interaction that no curator imagined |
//! | Cannot fail on | a combination nobody wrote down | upstream drift, because it is frozen |
//!
//! Concretely: only this file can tell us that upstream changed a conflict rule between pins.
//! Only the 600-case corpus can tell us that a batch of four rows where two conflict and one
//! has a broken attachment behaves correctly. Both are cheap, so both run.
//!
//! # Reading the vector
//!
//! The file is decoded by `dxdb-vectors`, the workspace's single typed reader, so field names
//! and nullability live in one place instead of in a per-lane `serde_json` traversal. A field
//! the generator adds without the crate knowing is a load error, not a silently ignored value.

use std::collections::HashMap;

use dxdb_storage::{
    categorize_bulk_write_rows, AttachmentData, AttachmentWriteData, BulkWriteRow, DocumentData,
    DocumentWriteData, Operation,
};
use dxdb_vectors::{bulk_write_categorization as family, VectorFamily, Vectors};

fn load() -> family::Vectors {
    Vectors::discover()
        .load::<family::Vectors>(VectorFamily::BulkWriteCategorization)
        .expect("the committed bulk-write-categorization family decodes")
}

/// Reconstructs a write row from the vector.
///
/// The generator records an attachment payload as `data`, whose value is a stable marker
/// object rather than a real `Blob` (JSON cannot carry bytes). A stub without `data` is
/// "metadata only", which is the case upstream treats as a broken write.
fn to_write_data(document: &serde_json::Value) -> DocumentWriteData {
    let mut stubs = Vec::new();
    if let Some(attachments) = document
        .get("_attachments")
        .and_then(|value| value.as_object())
    {
        for (attachment_id, stub) in attachments {
            let Some(object) = stub.as_object() else {
                continue;
            };
            stubs.push((
                attachment_id.clone(),
                AttachmentData {
                    length: object
                        .get("length")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0),
                    content_type: object
                        .get("type")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    digest: object
                        .get("digest")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                object.get("data").cloned(),
            ));
        }
    }
    let mut data =
        DocumentData::from_value(document.clone()).expect("vector document is an object");
    // Rebuild through the typed path so the write looks like a real caller's.
    data.set(
        "_attachments",
        serde_json::Value::Object(Default::default()),
    );
    let mut write = DocumentWriteData::new(data);
    for (attachment_id, stub, payload) in stubs {
        let bytes = payload.map(|value| serde_json::to_vec(&value).expect("json"));
        write.insert_attachment(&attachment_id, AttachmentWriteData { stub, data: bytes });
    }
    write
}

fn to_row(row: &family::UnprocessedWriteRow) -> BulkWriteRow {
    BulkWriteRow {
        previous: row
            .previous
            .as_ref()
            .map(|value| DocumentData::from_value(value.clone()).expect("previous is an object")),
        document: to_write_data(&row.document),
    }
}

/// The primary value of a vector document.
fn document_id(document: &serde_json::Value, primary_path: &str) -> String {
    document
        .get(primary_path)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

/// One case's observable output, reduced from the port.
#[derive(Debug, PartialEq)]
struct Observed {
    insert_ids: Vec<String>,
    update_ids: Vec<String>,
    errors: Vec<(String, u16, Option<String>)>,
    events: Vec<(String, String, bool, bool)>,
    checkpoint: Option<(String, f64)>,
    attachments_add: Vec<(String, String, String)>,
    attachments_remove: Vec<(String, String, String)>,
    attachments_update: Vec<(String, String, String)>,
}

fn observed_of(case: &family::Case, primary_path: &str) -> Observed {
    let docs_in_db: HashMap<String, DocumentData> = case
        .docs_in_db
        .iter()
        .map(|(id, document)| {
            (
                id.clone(),
                DocumentData::from_value(document.clone()).expect("stored document"),
            )
        })
        .collect();
    let rows: Vec<BulkWriteRow> = case.write_rows.iter().map(to_row).collect();

    // The schema name says whether attachments are declared; the attachment branches only run
    // when the schema has them.
    let has_attachments = case.schema.contains("attachment");

    let output = categorize_bulk_write_rows(
        primary_path,
        &docs_in_db,
        &rows,
        &case.context,
        has_attachments,
    );

    Observed {
        insert_ids: output
            .bulk_insert_docs
            .iter()
            .map(|row| {
                row.document
                    .id(primary_path)
                    .map(str::to_string)
                    .unwrap_or_default()
            })
            .collect(),
        update_ids: output
            .bulk_update_docs
            .iter()
            .map(|row| {
                row.document
                    .id(primary_path)
                    .map(str::to_string)
                    .unwrap_or_default()
            })
            .collect(),
        errors: output
            .error
            .iter()
            .map(|error| {
                let attachment_id = match error {
                    dxdb_storage::WriteError::Attachment { attachment_id, .. } => {
                        Some(attachment_id.clone())
                    }
                    _ => None,
                };
                (
                    error.document_id().to_string(),
                    error.status(),
                    attachment_id,
                )
            })
            .collect(),
        events: output
            .events
            .iter()
            .map(|event| {
                (
                    event.document_id.clone(),
                    event.operation.as_str().to_string(),
                    event.previous_document_data.is_some(),
                    event.document_data.is_deleted(),
                )
            })
            .collect(),
        checkpoint: output
            .checkpoint
            .as_ref()
            .map(|checkpoint| (checkpoint.id.clone(), checkpoint.lwt)),
        attachments_add: triple(
            &output
                .attachments_add
                .iter()
                .map(|add| {
                    (
                        add.document_id.clone(),
                        add.attachment_id.clone(),
                        add.digest.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        ),
        attachments_remove: triple(
            &output
                .attachments_remove
                .iter()
                .map(|remove| {
                    (
                        remove.document_id.clone(),
                        remove.attachment_id.clone(),
                        remove.digest.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        ),
        attachments_update: triple(
            &output
                .attachments_update
                .iter()
                .map(|update| {
                    (
                        update.document_id.clone(),
                        update.attachment_id.clone(),
                        update.digest.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        ),
    }
}

fn triple(items: &[(String, String, String)]) -> Vec<(String, String, String)> {
    items.to_vec()
}

/// Builds what the vector says should have happened.
fn expected_of(case: &family::Case, primary_path: &str) -> Observed {
    Observed {
        insert_ids: case
            .bulk_insert_docs
            .iter()
            .map(|row| document_id(&row.document, primary_path))
            .collect(),
        update_ids: case
            .bulk_update_docs
            .iter()
            .map(|row| document_id(&row.document, primary_path))
            .collect(),
        errors: case
            .errors
            .iter()
            .map(|error| {
                (
                    error.document_id.clone(),
                    error.status,
                    error.attachment_id.clone(),
                )
            })
            .collect(),
        events: case
            .events
            .iter()
            .map(|event| {
                (
                    event.document_id.clone(),
                    event.operation.clone(),
                    event.previous_document_data.is_some(),
                    event
                        .document_data
                        .get("_deleted")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                )
            })
            .collect(),
        checkpoint: case.newest_row.as_ref().and_then(|row| {
            let id = row.get(primary_path).and_then(|value| value.as_str())?;
            let lwt = row
                .get("_meta")
                .and_then(|meta| meta.get("lwt"))
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0);
            Some((id.to_string(), lwt))
        }),
        attachments_add: case
            .attachments_add
            .iter()
            .map(|change| {
                (
                    change.document_id.clone(),
                    change.attachment_id.clone(),
                    change.digest.clone(),
                )
            })
            .collect(),
        attachments_remove: case
            .attachments_remove
            .iter()
            .map(|change| {
                (
                    change.document_id.clone(),
                    change.attachment_id.clone(),
                    change.digest.clone(),
                )
            })
            .collect(),
        attachments_update: case
            .attachments_update
            .iter()
            .map(|change| {
                (
                    change.document_id.clone(),
                    change.attachment_id.clone(),
                    change.digest.clone(),
                )
            })
            .collect(),
    }
}

/// The whole committed family, case by case.
#[test]
fn matches_the_committed_bulk_write_categorization_family() {
    let vectors = load();
    let provenance = vectors.provenance();
    assert!(
        provenance.matches_documented_pin(),
        "the family was generated from a different upstream pin than the one recorded"
    );

    let primary_path = vectors.primary_path.clone();
    let mut failures = Vec::new();
    for case in &vectors.cases {
        // The vector's own claim about its operations is checked first: a generator that
        // emitted an unknown operation would make every downstream comparison meaningless.
        for event in &case.events {
            assert!(
                event.is_known_operation(),
                "case '{}' has an unknown operation '{}'",
                case.why,
                event.operation
            );
        }

        let expected = expected_of(case, &primary_path);
        let observed = observed_of(case, &primary_path);
        if expected == observed {
            continue;
        }
        // Report field by field so a reviewer sees the rule that broke.
        let mut detail = Vec::new();
        macro_rules! compare {
            ($field:ident, $label:literal) => {
                if expected.$field != observed.$field {
                    detail.push(format!(
                        "{} expected {:?} got {:?}",
                        $label, expected.$field, observed.$field
                    ));
                }
            };
        }
        compare!(insert_ids, "insert_ids");
        compare!(update_ids, "update_ids");
        compare!(errors, "errors");
        compare!(events, "events");
        compare!(checkpoint, "checkpoint");
        compare!(attachments_add, "attachments_add");
        compare!(attachments_remove, "attachments_remove");
        compare!(attachments_update, "attachments_update");
        failures.push(format!("{}: {}", case.why, detail.join("; ")));
    }

    assert!(
        failures.is_empty(),
        "{} of {} committed cases disagree with upstream:\n{}",
        failures.len(),
        vectors.cases.len(),
        failures.join("\n")
    );
}

/// Every named rule in the family must still be represented, so a shrunken family is noticed
/// rather than silently passing.
#[test]
fn the_family_still_covers_every_rule() {
    let vectors = load();
    for rule in [
        "fresh-insert",
        "update-existing",
        "update-without-previous-is-conflict",
        "update-conflict-stale-previous",
        "delete-existing",
        "revive-deleted-document-is-insert-event",
        "insert-already-deleted-document",
        "tombstone-for-unknown-document-inserts",
        "insert-attachment-without-data-is-510",
        "insert-with-attachment-data",
        "update-adds-attachment",
        "update-attachment-changed-digest",
        "update-removes-attachment",
        "delete-document-with-attachments-removes-them",
        "duplicate-ids-in-one-batch",
        "context-is-echoed",
        "empty-batch",
    ] {
        assert!(
            vectors.case(rule).is_some(),
            "the committed family no longer has a '{rule}' case"
        );
    }
    let generated = vectors
        .cases
        .iter()
        .filter(|case| case.why.starts_with("generated-sequence-"))
        .count();
    assert!(
        generated >= 20,
        "expected the generated sequences to remain, found {generated}"
    );
    assert_eq!(
        vectors.cases.len(),
        47,
        "the family's case count changed; update EXPECTED_CASE_COUNTS in dxdb-vectors and this test together"
    );
}

/// The resurrection rule, stated on its own because it makes event-reduce work and is the case
/// most likely to be "fixed" back to an UPDATE.
#[test]
fn a_revived_document_reports_insert_with_no_previous() {
    let vectors = load();
    let case = vectors
        .case("revive-deleted-document-is-insert-event")
        .expect("the family must keep this case");
    let observed = observed_of(case, &vectors.primary_path);

    let event = observed
        .events
        .iter()
        .find(|(_, operation, _, _)| operation == "INSERT")
        .expect("a resurrection must emit an INSERT event");
    assert!(
        !event.2,
        "a resurrection event must not carry a previous document"
    );
    assert_eq!(Operation::Insert.as_str(), "INSERT");
    assert_eq!(operation_names(), vec!["INSERT", "UPDATE", "DELETE"]);
}

fn operation_names() -> Vec<&'static str> {
    [Operation::Insert, Operation::Update, Operation::Delete]
        .into_iter()
        .map(Operation::as_str)
        .collect()
}
