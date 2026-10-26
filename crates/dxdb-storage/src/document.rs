//! Ordered-JSON documents: the one representation every crate passes around.
//!
//! Upstream a document is a plain JavaScript object with four injected meta
//! fields (`_rev`, `_deleted`, `_meta`, `_attachments`), and the storage layer
//! treats it as opaque JSON plus those four keys. This module is that: a newtype
//! over `serde_json::Value` that is guaranteed to be an object, with typed
//! accessors for the injected fields and zero-copy dot-path access for
//! everything else.
//!
//! Two invariants are enforced by construction:
//!
//! * a [`DocumentData`] is always a JSON object (never `null`, never an array);
//! * key order is whatever the producer used (`preserve_order`), never sorted.

use serde_json::{Map, Value};

use crate::traits::StorageError;

/// `_deleted`: tombstones instead of physical deletes (upstream never removes
/// rows on delete).
pub const FIELD_DELETED: &str = "_deleted";
/// `_rev`: `{height}-{databaseInstanceToken}`; see [`crate::revision`].
pub const FIELD_REV: &str = "_rev";
/// `_meta`: last-write time plus plugin-owned flags.
pub const FIELD_META: &str = "_meta";
/// `_attachments`: attachment stubs only. Payloads never live in a stored
/// document; they travel in [`DocumentWriteData`].
pub const FIELD_ATTACHMENTS: &str = "_attachments";

/// `_meta.lwt`: the last-write time, in unix milliseconds with two decimals.
pub const META_FIELD_LWT: &str = "lwt";
/// `_meta.o`: the "owner" flag the ownership/local-documents plugins set.
/// Preserved like any other unknown `_meta` key; named here because lanes need
/// to read it by a literal that is not a magic string.
pub const META_FIELD_OWNER: &str = "o";

/// `_meta.lwt` lower bound from upstream's meta schema. Deliberately 1, not 0,
/// so a write time is never falsy in JavaScript.
pub const META_LWT_MINIMUM: f64 = 1.0;
/// `_meta.lwt` upper bound (`1000000000000000`), matching upstream.
pub const META_LWT_UNIX_TIME_MAX: f64 = 1_000_000_000_000_000.0;

/// Reads a dot path out of a JSON value the way upstream's `objectPathMonad`
/// does: object keys, with numeric segments indexing into arrays.
///
/// A `null` in the middle of a path yields `None`, exactly like upstream's
/// fast paths (`v === undefined || v === null ? undefined : v[key]`).
pub fn get_object_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => {
                // Only all-digit segments index an array; upstream's
                // `isDigits` check is the same rule.
                if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                items.get(segment.parse::<usize>().ok()?)?
            }
            // Scalars have no properties. Upstream would read `undefined`.
            _ => return None,
        };
    }
    Some(current)
}

/// A document as it is stored, read, indexed and replicated.
///
/// The four injected fields are accessible through typed accessors; everything
/// else through [`DocumentData::get`] / [`DocumentData::get_object_path`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocumentData(Value);

impl DocumentData {
    /// Wraps an ordered JSON object.
    pub fn new(object: Map<String, Value>) -> Self {
        DocumentData(Value::Object(object))
    }

    /// Wraps a JSON value, rejecting anything that is not an object.
    ///
    /// Rejecting here is deliberate: a non-object document would fail upstream's
    /// schema validation *later* and further from the cause.
    pub fn from_value(value: Value) -> Result<Self, StorageError> {
        match value {
            Value::Object(_) => Ok(DocumentData(value)),
            other => Err(StorageError::InvalidDocument(format!(
                "document must be a JSON object, got {}",
                json_type_name(&other)
            ))),
        }
    }

    /// The document as ordered JSON.
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Consumes the document, returning its ordered JSON.
    pub fn into_value(self) -> Value {
        self.0
    }

    /// The backing object. Always succeeds by the type's invariant.
    pub fn as_object(&self) -> &Map<String, Value> {
        match &self.0 {
            Value::Object(map) => map,
            // Unreachable: every constructor produces an object.
            _ => unreachable!("DocumentData is always an object"),
        }
    }

    /// The backing object, mutably.
    pub fn as_object_mut(&mut self) -> &mut Map<String, Value> {
        match &mut self.0 {
            Value::Object(map) => map,
            _ => unreachable!("DocumentData is always an object"),
        }
    }

    /// A top-level field.
    pub fn get(&self, field: &str) -> Option<&Value> {
        self.as_object().get(field)
    }

    /// A dot path such as `nes.ted` (upstream: `getSchemaByObjectPath` /
    /// `objectPathMonad`).
    pub fn get_object_path(&self, path: &str) -> Option<&Value> {
        get_object_path(&self.0, path)
    }

    /// Sets a top-level field.
    pub fn set(&mut self, field: impl Into<String>, value: Value) -> &mut Self {
        self.as_object_mut().insert(field.into(), value);
        self
    }

    /// The document's primary value as a string.
    ///
    /// Upstream allows a composite primary key, in which case the storage uses
    /// the composite key *field* as the id. Either way the value is a string; a
    /// document whose id is not a string cannot be stored.
    pub fn id(&self, primary_path: &str) -> Result<&str, StorageError> {
        match self.get_object_path(primary_path) {
            Some(Value::String(id)) => Ok(id),
            Some(other) => Err(StorageError::InvalidDocument(format!(
                "primary key '{primary_path}' must be a string, got {}",
                json_type_name(other)
            ))),
            None => Err(StorageError::InvalidDocument(format!(
                "document is missing its primary key '{primary_path}'"
            ))),
        }
    }

    /// `_rev`, if present.
    pub fn rev(&self) -> Option<&str> {
        match self.get(FIELD_REV) {
            Some(Value::String(rev)) => Some(rev),
            _ => None,
        }
    }

    /// `_deleted`. Missing means `false` (upstream treats a missing flag as
    /// "not deleted", and the schema makes it required anyway).
    pub fn is_deleted(&self) -> bool {
        matches!(self.get(FIELD_DELETED), Some(Value::Bool(true)))
    }

    /// `_meta.lwt`, if present and numeric.
    pub fn lwt(&self) -> Option<f64> {
        match self.get_object_path("_meta.lwt") {
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        }
    }

    /// `_meta` as an object, for plugins that own extra keys in it.
    pub fn meta(&self) -> Option<&Map<String, Value>> {
        match self.get(FIELD_META) {
            Some(Value::Object(map)) => Some(map),
            _ => None,
        }
    }

    /// `_attachments` as an object (stubs only).
    pub fn attachments(&self) -> Option<&Map<String, Value>> {
        match self.get(FIELD_ATTACHMENTS) {
            Some(Value::Object(map)) => Some(map),
            _ => None,
        }
    }

    /// A single attachment stub.
    pub fn attachment(&self, attachment_id: &str) -> Option<&Value> {
        self.attachments().and_then(|atts| atts.get(attachment_id))
    }

    /// Replaces `_meta.lwt`, leaving every other `_meta` key untouched.
    pub fn set_lwt(&mut self, lwt: f64) -> Result<(), StorageError> {
        let number = serde_json::Number::from_f64(lwt)
            .ok_or_else(|| StorageError::InvalidDocument(format!("lwt {lwt} is not finite")))?;
        match self.as_object_mut().get_mut(FIELD_META) {
            Some(Value::Object(meta)) => {
                meta.insert(META_FIELD_LWT.to_string(), Value::Number(number));
            }
            _ => {
                let mut meta = Map::new();
                meta.insert(META_FIELD_LWT.to_string(), Value::Number(number));
                self.set(FIELD_META, Value::Object(meta));
            }
        }
        Ok(())
    }

    /// Serialises with JavaScript number formatting (`dxdb-json`), which is the
    /// only serialisation allowed to reach a hash, a schema identity, or disk.
    pub fn to_js_json(&self) -> String {
        dxdb_json::to_js_json(&self.0)
    }

    /// Canonical bytes to hash (ordered keys, JS numbers).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        dxdb_json::canonical_bytes(&self.0)
    }
}

impl From<DocumentData> for Value {
    fn from(document: DocumentData) -> Self {
        document.0
    }
}

/// Attachment metadata as it is stored inside `_attachments`.
///
/// Upstream calls the MIME field `type`; it is `content_type` here because `type`
/// is a keyword in Rust, and the JSON key is kept as `type` on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentData {
    /// Size in bytes. A JS number upstream; `u64` here because it is a byte
    /// count and therefore always a non-negative integer well below 2^53.
    pub length: u64,
    /// MIME type, e.g. `text/plain`.
    pub content_type: String,
    /// Digest of the content. The only guarantee upstream makes is that it
    /// changes when the data changes; it is opaque here too.
    pub digest: String,
}

impl AttachmentData {
    /// The stored JSON shape: `{"length": n, "type": "...", "digest": "..."}`.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("length".to_string(), Value::from(self.length));
        map.insert("type".to_string(), Value::String(self.content_type.clone()));
        map.insert("digest".to_string(), Value::String(self.digest.clone()));
        Value::Object(map)
    }

    /// Parses the stored JSON shape. Returns `None` for a malformed stub so the
    /// caller decides whether that is an error or a "no data" case.
    pub fn from_value(value: &Value) -> Option<Self> {
        let map = value.as_object()?;
        let length = map.get("length").and_then(Value::as_u64)?;
        let content_type = map.get("type").and_then(Value::as_str)?.to_string();
        let digest = map.get("digest").and_then(Value::as_str)?.to_string();
        Some(AttachmentData {
            length,
            content_type,
            digest,
        })
    }
}

/// An attachment supplied with a write: metadata plus, optionally, its payload.
///
/// `data: None` means "metadata only" — the upstream case of an
/// `RxAttachmentData` stub with no `.data`. `categorize_bulk_write_rows` treats
/// a new attachment without payload as a `510` error, exactly as upstream does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentWriteData {
    /// The stub that will be stored in the document.
    pub stub: AttachmentData,
    /// Payload for a new or changed attachment. `None` leaves the stored payload
    /// untouched (the "send the stub again" case).
    pub data: Option<Vec<u8>>,
}

/// A document as it is written: the stored JSON plus any attachment payloads.
///
/// The payloads are kept out of the JSON tree because `serde_json` cannot hold
/// bytes and base64-in-a-string would be an invisible encoding at every storage
/// boundary. `document._attachments` therefore holds *stubs only*, and
/// [`DocumentWriteData::attachment_payloads`] maps attachment id to bytes for the
/// attachments this write creates or replaces.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentWriteData {
    document: DocumentData,
    attachment_payloads: std::collections::BTreeMap<String, Vec<u8>>,
}

impl DocumentWriteData {
    /// Wraps stored-shaped document data. Attachments carry no payload, which is
    /// the common case for a plain write.
    pub fn new(document: DocumentData) -> Self {
        DocumentWriteData {
            document,
            attachment_payloads: std::collections::BTreeMap::new(),
        }
    }

    /// Builds a write from stored data plus payloads keyed by attachment id.
    pub fn with_payloads(
        document: DocumentData,
        payloads: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Self {
        DocumentWriteData {
            document,
            attachment_payloads: payloads,
        }
    }

    /// The stored document shape (`_attachments` holds stubs).
    pub fn document(&self) -> &DocumentData {
        &self.document
    }

    /// The stored document shape, mutably.
    pub fn document_mut(&mut self) -> &mut DocumentData {
        &mut self.document
    }

    /// Payloads supplied by this write.
    pub fn attachment_payloads(&self) -> &std::collections::BTreeMap<String, Vec<u8>> {
        &self.attachment_payloads
    }

    /// Payload for one attachment, if this write supplies it.
    pub fn attachment_payload(&self, attachment_id: &str) -> Option<&[u8]> {
        self.attachment_payloads
            .get(attachment_id)
            .map(Vec::as_slice)
    }

    /// True when this write supplies the payload for `attachment_id`.
    pub fn has_attachment_payload(&self, attachment_id: &str) -> bool {
        self.attachment_payloads.contains_key(attachment_id)
    }

    /// Adds an attachment: writes its stub into `_attachments` and records the
    /// payload (if any) for the storage to persist separately.
    pub fn insert_attachment(&mut self, attachment_id: &str, attachment: AttachmentWriteData) {
        let stub = attachment.stub.to_value();
        let attachments = self
            .document
            .as_object_mut()
            .entry(FIELD_ATTACHMENTS.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(map) = attachments {
            map.insert(attachment_id.to_string(), stub);
        }
        match attachment.data {
            Some(data) => {
                self.attachment_payloads
                    .insert(attachment_id.to_string(), data);
            }
            None => {
                self.attachment_payloads.remove(attachment_id);
            }
        }
    }

    /// The write's primary value.
    pub fn id(&self, primary_path: &str) -> Result<&str, StorageError> {
        self.document.id(primary_path)
    }

    /// Drops payloads, yielding the stored document shape.
    pub fn into_document_data(self) -> DocumentData {
        self.document
    }
}

impl From<DocumentData> for DocumentWriteData {
    fn from(document: DocumentData) -> Self {
        DocumentWriteData::new(document)
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> DocumentData {
        DocumentData::from_value(json!({
            "id": "a",
            "_rev": "1-abc",
            "_deleted": false,
            "_meta": { "lwt": 1000.01, "o": "owner-1" },
            "_attachments": {},
            "nes": { "ted": 5, "list": [ { "x": 1 }, { "x": 2 } ] }
        }))
        .expect("valid document")
    }

    #[test]
    fn injected_fields_have_typed_accessors() {
        let d = doc();
        assert_eq!(d.id("id").expect("id"), "a");
        assert_eq!(d.rev(), Some("1-abc"));
        assert!(!d.is_deleted());
        assert_eq!(d.lwt(), Some(1000.01));
        // Plugin-owned `_meta` keys survive.
        assert_eq!(d.meta().and_then(|m| m.get("o")), Some(&json!("owner-1")));
    }

    #[test]
    fn dot_paths_index_into_arrays() {
        let d = doc();
        assert_eq!(d.get_object_path("nes.ted"), Some(&json!(5)));
        assert_eq!(d.get_object_path("nes.list.1.x"), Some(&json!(2)));
        // Out of range and non-numeric segments are absent, not errors.
        assert_eq!(d.get_object_path("nes.list.9.x"), None);
        assert_eq!(d.get_object_path("nes.list.x"), None);
        assert_eq!(d.get_object_path("nes.nope.ted"), None);
    }

    #[test]
    fn non_objects_are_rejected_at_construction() {
        assert!(DocumentData::from_value(json!([1, 2, 3])).is_err());
        assert!(DocumentData::from_value(json!(null)).is_err());
    }

    #[test]
    fn set_lwt_preserves_other_meta_keys() {
        let mut d = doc();
        d.set_lwt(2000.5).expect("finite");
        assert_eq!(d.lwt(), Some(2000.5));
        assert_eq!(d.meta().and_then(|m| m.get("o")), Some(&json!("owner-1")));
    }

    #[test]
    fn attachment_payloads_are_not_in_the_document_tree() {
        let mut write = DocumentWriteData::new(doc());
        write.insert_attachment(
            "att-1",
            AttachmentWriteData {
                stub: AttachmentData {
                    length: 3,
                    content_type: "text/plain".to_string(),
                    digest: "sha-1".to_string(),
                },
                data: Some(b"payload-bytes".to_vec()),
            },
        );
        // The stub is in the document; the bytes are not.
        assert_eq!(
            write.document().attachment("att-1"),
            Some(&json!({ "length": 3, "type": "text/plain", "digest": "sha-1" }))
        );
        assert_eq!(
            write.attachment_payload("att-1"),
            Some(&b"payload-bytes"[..])
        );
        assert!(!write.document().to_js_json().contains("payload-bytes"));
    }
}
