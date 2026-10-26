//! Documents as a storage sees them: payload plus `_deleted` / `_rev` / `_meta` /
//! `_attachments`.
//!
//! Upstream's implementations suite builds these inline in every case with
//! `Object.assign({...}, { _deleted: false, _attachments: {}, _meta: { lwt: now() },
//! _rev: EXAMPLE_REVISION_1 }, ownParams)`. The `Object.assign` order matters — the
//! caller's overrides win — so the helpers here take the overrides as a map and
//! apply them last, which reproduces that exactly.
//!
//! The meta fields are not optional in a stored document (`fillWithDefaultSettings`
//! injects them into `properties` and `required`), so a storage is entitled to
//! reject a write without them. Building documents through these helpers means a
//! ported case never accidentally tests "what happens without `_meta`".

use serde_json::{Map, Value};

use dxdb_storage::{
    DocumentData, DocumentWriteData, FIELD_ATTACHMENTS, FIELD_DELETED, FIELD_META, FIELD_REV,
    META_FIELD_LWT,
};

use crate::fixtures::objects::EXAMPLE_REVISION_1;

/// Builds a document payload with the four meta fields upstream's `getWriteData`
/// adds, applying `overrides` last.
///
/// `overrides` can replace any field including the meta ones, which is how the
/// suite produces a tombstone (`_deleted: true`), a bumped revision
/// (`_rev: EXAMPLE_REVISION_2`) or an `_meta`-only change.
pub fn write_data_with(
    mut document: Map<String, Value>,
    lwt: f64,
    overrides: Map<String, Value>,
) -> Value {
    document.insert(FIELD_DELETED.to_string(), Value::Bool(false));
    document.insert(
        FIELD_ATTACHMENTS.to_string(),
        Value::Object(Map::new()),
    );
    let mut meta = Map::new();
    meta.insert(
        META_FIELD_LWT.to_string(),
        serde_json::Number::from_f64(lwt)
            .map(Value::Number)
            .expect("lwt must be finite"),
    );
    document.insert(FIELD_META.to_string(), Value::Object(meta));
    document.insert(
        FIELD_REV.to_string(),
        Value::String(EXAMPLE_REVISION_1.to_string()),
    );
    for (key, value) in overrides {
        document.insert(key, value);
    }
    Value::Object(document)
}

/// [`write_data_with`] with `lwt` taken from the clock.
pub fn write_data(document: Map<String, Value>) -> Value {
    write_data_with(document, dxdb_storage::now(), Map::new())
}

/// [`write_data_with`] with an explicit `lwt`, no overrides.
pub fn write_data_lwt(document: Map<String, Value>, lwt: f64) -> Value {
    write_data_with(document, lwt, Map::new())
}

/// The `getWriteData(ownParams)` port: a `TestDocType` document with meta fields
/// and the caller's overrides applied.
pub fn test_doc_write_data(
    key: &str,
    value: &str,
    lwt: f64,
    overrides: Map<String, Value>,
) -> Value {
    write_data_with(
        Map::from_iter([
            ("key".to_string(), Value::String(key.to_string())),
            ("value".to_string(), Value::String(value.to_string())),
        ]),
        lwt,
        overrides,
    )
}

/// Wraps a JSON payload as [`DocumentData`], which is what the traits take.
///
/// # Panics
///
/// Panics on a non-object, because every ported case passes an object and a
/// non-object would be a bug in the test rather than in the storage.
pub fn doc(value: Value) -> DocumentData {
    DocumentData::from_value(value).expect("fixture document must be a JSON object")
}

/// Wraps a JSON payload as [`DocumentWriteData`].
pub fn write(value: Value) -> DocumentWriteData {
    DocumentWriteData::new(doc(value))
}

/// An insert row: no previous state.
pub fn insert_row(value: Value) -> dxdb_storage::BulkWriteRow {
    dxdb_storage::BulkWriteRow::insert(write(value))
}

/// An update row: the previous document plus the new one.
pub fn update_row(previous: Value, document: Value) -> dxdb_storage::BulkWriteRow {
    dxdb_storage::BulkWriteRow::update(doc(previous), write(document))
}

/// A delete row: previous state, new state with `_deleted: true`.
pub fn delete_row(previous: Value, mut tombstone: Value) -> dxdb_storage::BulkWriteRow {
    if let Some(object) = tombstone.as_object_mut() {
        object.insert(FIELD_DELETED.to_string(), Value::Bool(true));
    }
    dxdb_storage::BulkWriteRow::update(doc(previous), write(tombstone))
}

/// A single small attachment whose bytes and digest are fixed.
///
/// Upstream's attachment cases compute a digest over the payload
/// (`getAttachmentData` verifies it), so the digest has to be right. It is supplied
/// by the caller rather than computed here, because computing SHA-256 is not this
/// crate's job and a wrong digest would make a storage look broken. The correctness
/// of a digest is checked by the testkit's own `attachment` suite, which takes the
/// digest from the storage's own write path where upstream does.
pub fn attachment_stub(content_type: &str, digest: &str, length: usize) -> Value {
    serde_json::json!({
        "digest": digest,
        "length": length,
        "type": content_type,
    })
}

/// Builds an `_attachments` map with one entry.
pub fn attachments_with(id: &str, stub: Value) -> Value {
    let mut map = Map::new();
    map.insert(id.to_string(), stub);
    Value::Object(map)
}

/// Applies a field set to a document, returning a new value. Used where upstream
/// does `Object.assign({}, doc, {...})`.
pub fn with_fields(document: &Value, fields: Map<String, Value>) -> Value {
    let mut out = document
        .as_object()
        .cloned()
        .expect("with_fields() needs an object document");
    for (key, value) in fields {
        out.insert(key, value);
    }
    Value::Object(out)
}

/// Reads a field of a document for an assertion.
pub fn field<'a>(document: &'a Value, name: &str) -> Option<&'a Value> {
    document.get(name)
}

/// Reads a nested field by dot path, e.g. `_meta.lwt`.
pub fn field_path<'a>(document: &'a Value, path: &str) -> Option<&'a Value> {
    dxdb_storage::get_object_path(document, path)
}

/// The primary value of a document, as a `String`, for building an expected-id
/// list. Returns `None` when the field is absent, so a fixture mistake fails at the
/// assertion that needed it rather than producing an empty string.
pub fn primary_of(document: &Value, primary_path: &str) -> Option<String> {
    match dxdb_storage::get_object_path(document, primary_path)? {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}
