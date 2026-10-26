//! Revisions and the write path: where `_rev` and `_meta` are actually assigned.
//!
//! # The upstream shape, because it is not obvious
//!
//! `_rev` and `_meta.lwt` are **not** assigned by the storage and **not** by
//! `bulkInsert`. They are assigned by `getWrappedStorageInstance`'s `bulkWrite`
//! wrapper (`src/rx-storage-helper.ts:612`), which sits between the collection and the
//! real storage. The details that wrapper pins:
//!
//! * **One timestamp per batch.** `const time = now()` is called once for the whole
//!   `bulkWrite`, then every row's `_meta` gets that same `time`. Not one call per
//!   document. This is observable: the fixture corpus has batches where every `lwt` is
//!   identical, and a per-row clock would change checkpoint ordering.
//! * **Inserts get `'1-' + databaseInstanceToken`.** Every insert in a batch shares
//!   that exact string, not just the height.
//! * **Updates get `createRevision(token, previous)`**, so the height is
//!   `previous.height + 1`. A caller cannot skip a height through the collection API,
//!   even though the storage itself would accept it.
//!
//! A port that put revision creation in the storage would make every storage
//! responsible for it; a port that put it in `bulk_insert` would be wrong about
//! `_meta.lwt` sharing.

use dxdb_storage::{create_revision, now, DocumentData, DocumentWriteData};

use crate::error::{CoreError, CoreResult};

/// The primary value of a document, without the field path.
pub fn primary_of(document: &DocumentData, primary_path: &str) -> CoreResult<String> {
    document
        .id(primary_path)
        .map(str::to_string)
        .map_err(|error| CoreError::InvalidDocument(error.to_string()))
}

/// One row on its way to a storage, after revisions and `_meta` are assigned.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedRow {
    /// The state the caller believes is stored. `None` for an insert.
    pub previous: Option<DocumentData>,
    /// The state to write, with `_rev` and `_meta` already set.
    pub document: DocumentWriteData,
}

/// Assigns `_rev` and `_meta.lwt` to every row of one batch.
///
/// Mirrors `getWrappedStorageInstance`'s wrapper (`rx-storage-helper.ts:612`):
///
/// * `now()` is called **once** for the batch, so all rows share an `lwt`;
/// * an insert gets `1-{database_instance_token}`;
/// * an update gets `create_revision(token, previous)`, i.e. `previous_height + 1`.
///
/// `_deleted` and `_attachments` are not touched here: `bulk_insert` already
/// initialised them, and the storage check depends on them being present before this
/// point.
pub fn assign_revisions(
    rows: Vec<PreparedRow>,
    database_instance_token: &str,
) -> CoreResult<Vec<PreparedRow>> {
    // One timestamp for the whole batch. This is the upstream decision that is easiest
    // to get subtly wrong.
    let time = now();

    rows.into_iter()
        .map(|row| {
            let mut document = row.document;
            match &row.previous {
                Some(previous) => {
                    let previous_revision = previous.rev().ok_or_else(|| {
                        CoreError::InvalidDocument(
                            "the previous state has no `_rev`, so a new revision cannot be derived"
                                .to_string(),
                        )
                    })?;
                    document.document_mut().set(
                        "_rev",
                        serde_json::json!(create_revision(
                            database_instance_token,
                            Some(previous_revision)
                        )),
                    );
                }
                None => {
                    document.document_mut().set(
                        "_rev",
                        serde_json::json!(create_revision(database_instance_token, None)),
                    );
                }
            }
            // `_meta` keeps any plugin-owned keys and only replaces `lwt`.
            document
                .document_mut()
                .set_lwt(time)
                .map_err(|error| CoreError::InvalidDocument(error.to_string()))?;
            Ok(PreparedRow {
                previous: row.previous,
                document,
            })
        })
        .collect()
}

/// Fills a document the way `fillObjectDataBeforeInsert` does
/// (`src/rx-collection-helper.ts:34`).
///
/// Three things happen, in this order:
///
/// 1. **schema defaults** are applied (`fillObjectWithDefaults`), so a document gets
///    its declared `default` values;
/// 2. **the composite primary key is computed**, when the schema declares one;
/// 3. **`_deleted` and `_attachments` are initialised** when absent.
///
/// `_meta` and `_rev` are deliberately *not* set here — see [`assign_revisions`] for
/// why, and note that upstream's comment says the same thing.
pub fn fill_object_data_before_insert(
    schema: &dxdb_schema::Schema,
    data: &serde_json::Value,
    skip_primary_key_fill: bool,
) -> CoreResult<DocumentData> {
    let mut object = data.clone();
    dxdb_schema::fill_object_with_defaults(schema.json_schema(), &mut object);

    if !skip_primary_key_fill {
        let is_composite = schema
            .json_schema()
            .get("primaryKey")
            .map(|primary| !primary.is_string())
            .unwrap_or(false);
        if is_composite {
            let composed =
                dxdb_schema::composed_primary_key_of_document_data(schema.json_schema(), &object)
                    .map_err(|error| CoreError::Schema(error.to_string()))?;
            if let Some(map) = object.as_object_mut() {
                map.insert(
                    schema.primary_path().to_string(),
                    serde_json::Value::String(composed),
                );
            }
        }
    }

    if let Some(map) = object.as_object_mut() {
        if !map.contains_key("_deleted") {
            map.insert("_deleted".to_string(), serde_json::Value::Bool(false));
        }
        if !map.contains_key("_attachments") {
            map.insert(
                "_attachments".to_string(),
                serde_json::Value::Object(Default::default()),
            );
        }
    }

    DocumentData::from_value(object).map_err(|error| CoreError::InvalidDocument(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(id: &str, revision: &str) -> DocumentData {
        DocumentData::from_value(json!({
            "id": id,
            "_rev": revision,
            "_deleted": false,
            "_meta": { "lwt": 1.0 },
            "_attachments": {}
        }))
        .expect("valid")
    }

    /// `rx-storage-helper.ts:612`: one `now()` per batch, so every row shares an `lwt`.
    #[test]
    fn every_row_of_a_batch_shares_one_timestamp() {
        let rows = vec![
            PreparedRow {
                previous: None,
                document: DocumentWriteData::new(document("a", "1-x")),
            },
            PreparedRow {
                previous: None,
                document: DocumentWriteData::new(document("b", "1-x")),
            },
            PreparedRow {
                previous: None,
                document: DocumentWriteData::new(document("c", "1-x")),
            },
        ];
        let prepared = assign_revisions(rows, "token").expect("assign");
        let lwts: Vec<f64> = prepared
            .iter()
            .map(|row| row.document.document().lwt().expect("lwt"))
            .collect();
        assert_eq!(
            lwts[0], lwts[1],
            "one bulkWrite must share one timestamp, not call now() per row"
        );
        assert_eq!(lwts[1], lwts[2]);
    }

    /// An insert gets `1-token`; an update gets `previous_height + 1`.
    #[test]
    fn inserts_start_at_height_one_and_updates_increment() {
        let insert = assign_revisions(
            vec![PreparedRow {
                previous: None,
                document: DocumentWriteData::new(document("a", "999-ignored")),
            }],
            "token",
        )
        .expect("insert");
        assert_eq!(
            insert[0].document.document().rev(),
            Some("1-token"),
            "an insert ignores any _rev the caller supplied"
        );

        let update = assign_revisions(
            vec![PreparedRow {
                previous: Some(document("a", "4-old")),
                document: DocumentWriteData::new(document("a", "4-old")),
            }],
            "token",
        )
        .expect("update");
        assert_eq!(update[0].document.document().rev(), Some("5-token"));
    }

    /// A previous state with no `_rev` cannot yield a revision, and silently inventing
    /// one would corrupt the history.
    #[test]
    fn a_previous_state_without_a_revision_is_an_error() {
        let previous = DocumentData::from_value(json!({
            "id": "a", "_deleted": false, "_meta": { "lwt": 1.0 }, "_attachments": {}
        }))
        .expect("valid");
        let error = assign_revisions(
            vec![PreparedRow {
                previous: Some(previous),
                document: DocumentWriteData::new(document("a", "2-x")),
            }],
            "token",
        )
        .expect_err("must reject");
        assert!(
            matches!(error, CoreError::InvalidDocument(_)),
            "got {error:?}"
        );
    }

    /// `fillObjectDataBeforeInsert` applies schema defaults and initialises the two
    /// flags, and leaves `_meta`/`_rev` alone.
    #[test]
    fn filling_an_insert_document_applies_defaults_but_not_revisions() {
        let schema = dxdb_schema::create_rx_schema(&json!({
            "version": 0,
            "primaryKey": "id",
            "type": "object",
            "properties": {
                "id": { "type": "string", "maxLength": 10 },
                "age": { "type": "integer", "minimum": 0, "maximum": 150, "default": 20 }
            },
            "required": ["id"],
            "indexes": []
        }))
        .expect("schema");

        let filled =
            fill_object_data_before_insert(&schema, &json!({ "id": "a" }), false).expect("fill");
        assert_eq!(filled.get("age"), Some(&json!(20)), "default applied");
        assert_eq!(filled.get("_deleted"), Some(&json!(false)));
        assert_eq!(filled.get("_attachments"), Some(&json!({})));
        assert!(filled.rev().is_none(), "_rev is assigned later, not here");
        assert!(filled.lwt().is_none(), "_meta is assigned later, not here");
    }
}
