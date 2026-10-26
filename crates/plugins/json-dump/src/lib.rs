//! JSON export and import of a database or a collection.
//!
//! Ported from `src/plugins/json-dump/index.ts` (236 lines) at upstream `3b763ee17`.
//!
//! # What the format is, exactly
//!
//! A collection dump (`dumpRxCollection`, `:109-147`):
//!
//! ```json
//! { "name": "humans", "schemaHash": "…", "docs": [ … ] }
//! ```
//!
//! and a database dump wraps a list of those plus the database's name and instance
//! token (`:38-42`). Both are stable enough to be a file format, so the field names here
//! are upstream's, not Rust-preferred.
//!
//! # The three rules that make export/import lossless
//!
//! 1. **`_rev` is dropped and `_attachments` is emptied on export** (`:126-128`). A
//!    revision is meaningful only to the database that issued it, and an attachment
//!    digest only to the `hashFunction` that computed it, so carrying either across a dump
//!    would be a lie. Import therefore assigns a **fresh** revision and a fresh `_meta.lwt`
//!    (`:199-210`), which is why a restored document is a new revision chain rather than a
//!    continuation of the old one.
//! 2. **A collection dump is pinned to a schema hash** and importing into a different
//!    schema is `JD2` (`:187-192`). This is the guard that stops a dump being restored into
//!    a collection whose shape has moved on.
//! 3. **Every collection in a dump must already exist** on import, else `JD1`
//!    (`:66-73`). The dump does not carry enough to create a collection: it has no
//!    migration strategies, no instance options, no storage choice.
//!
//! # Attachments are base64, and the digest is recomputed
//!
//! With `options.attachments`, an attachment's bytes are inlined as base64 with its `type`
//! and `length` (`:86-107`). On import the digest is **recomputed with the importing
//! database's `hashFunction`** (`:178-184`) — the comment says so explicitly — because a
//! digest is a function of the hash algorithm, and two databases may not share one.
//!
//! Base64 rather than raw bytes because a dump has to be plain JSON: `JSON.stringify` of a
//! byte array would be an array of numbers, ten times larger and not diffable.
//!
//! # Not ported
//!
//! The document-level plumbing: reading a collection's documents needs a storage
//! instance, and writing them back needs `bulkWrite`. Both are behind [`DumpSource`] and
//! [`DumpSink`] — the same two-trait idea as `migration-storage` and `pipeline` — so the
//! format, its validation and the import transformation are all testable without a
//! database. Attachment bytes are handled as base64 strings here; reading them from a
//! storage is the caller's.

#![forbid(unsafe_code)]

use serde_json::{json, Map, Value};
use thiserror::Error;

/// Errors, carrying upstream's codes.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// `JD1`: the dump names a collection the database does not have
    /// (`index.ts:70`).
    #[error("JD1: the dump contains collections that do not exist: {0:?}")]
    MissingCollections(Vec<String>),
    /// `JD2`: the dump's schema hash does not match the collection's
    /// (`index.ts:189`).
    #[error("JD2: dump schema hash is {dump}, but the collection's is {own}")]
    SchemaHashMismatch {
        /// The hash recorded in the dump.
        dump: String,
        /// The hash of the collection being imported into.
        own: String,
    },
    /// The dump is not shaped like a dump.
    #[error("malformed dump: {0}")]
    Malformed(String),
    /// Base64 payload is not valid base64.
    #[error("attachment data is not valid base64: {0}")]
    InvalidBase64(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// One attachment, in the dump's base64 form (`RxDumpAttachmentData`).
#[derive(Clone, Debug, PartialEq)]
pub struct DumpAttachment {
    /// The MIME type.
    pub mime_type: String,
    /// The length in bytes, before base64 encoding.
    pub length: usize,
    /// The bytes, base64-encoded.
    pub base64: String,
}

impl DumpAttachment {
    /// Wraps raw bytes as a dump attachment.
    pub fn from_bytes(mime_type: impl Into<String>, bytes: &[u8]) -> Self {
        DumpAttachment {
            mime_type: mime_type.into(),
            length: bytes.len(),
            base64: base64_encode(bytes),
        }
    }

    /// The decoded bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        base64_decode(&self.base64)
    }

    /// The wire shape: `{type, length, data}` (`index.ts:99-103`).
    pub fn to_json(&self) -> Value {
        json!({ "type": self.mime_type, "length": self.length, "data": self.base64 })
    }

    /// Reads the wire shape.
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::Malformed(value.to_string()))?;
        Ok(DumpAttachment {
            mime_type: map
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Malformed("attachment needs a type".to_string()))?
                .to_string(),
            length: map.get("length").and_then(Value::as_u64).unwrap_or(0) as usize,
            base64: map
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Malformed("attachment needs data".to_string()))?
                .to_string(),
        })
    }
}

/// One collection's dump (`:115-119`).
#[derive(Clone, Debug, PartialEq)]
pub struct CollectionDump {
    /// The collection name.
    pub name: String,
    /// The schema hash the dump was taken with.
    pub schema_hash: String,
    /// The documents.
    pub docs: Vec<Value>,
}

impl CollectionDump {
    /// The wire shape.
    pub fn to_json(&self) -> Value {
        json!({ "name": self.name, "schemaHash": self.schema_hash, "docs": self.docs })
    }

    /// Reads the wire shape.
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::Malformed(value.to_string()))?;
        let docs = map
            .get("docs")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Malformed("collection dump needs docs".to_string()))?
            .clone();
        Ok(CollectionDump {
            name: map
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Malformed("collection dump needs a name".to_string()))?
                .to_string(),
            schema_hash: map
                .get("schemaHash")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Malformed("collection dump needs schemaHash".to_string()))?
                .to_string(),
            docs,
        })
    }
}

/// A database dump (`:38-42`).
#[derive(Clone, Debug, PartialEq)]
pub struct DatabaseDump {
    /// The database name the dump came from.
    pub name: String,
    /// The instance token the dump came from. Informational: import does not use it,
    /// which is what makes a dump portable between instances.
    pub instance_token: String,
    /// The collections.
    pub collections: Vec<CollectionDump>,
}

impl DatabaseDump {
    /// The wire shape.
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "instanceToken": self.instance_token,
            "collections": self.collections.iter().map(CollectionDump::to_json).collect::<Vec<_>>(),
        })
    }

    /// Reads the wire shape.
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::Malformed(value.to_string()))?;
        let collections = map
            .get("collections")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Malformed("dump needs collections".to_string()))?
            .iter()
            .map(CollectionDump::from_json)
            .collect::<Result<Vec<_>>>()?;
        Ok(DatabaseDump {
            name: map
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Malformed("dump needs a name".to_string()))?
                .to_string(),
            instance_token: map
                .get("instanceToken")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            collections,
        })
    }

    /// `importDumpRxDatabase`'s precondition (`:66-73`): every collection in the dump must
    /// already exist.
    pub fn check_collections_exist(&self, existing: &[&str]) -> Result<()> {
        let missing: Vec<String> = self
            .collections
            .iter()
            .filter(|collection| !existing.contains(&collection.name.as_str()))
            .map(|collection| collection.name.clone())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error::MissingCollections(missing))
        }
    }
}

/// How a collection dump is taken (`RxDumpOptions`, `:113`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DumpOptions {
    /// Inline attachment bytes as base64. Off by default, because a dump with attachments
    /// can be enormous and most restores do not need them.
    pub attachments: bool,
}

/// Turns a stored document into its dumped form (`:126-145`).
///
/// `_rev` is dropped and `_attachments` is either dropped or replaced with base64
/// payloads. The caller supplies the payloads because reading them needs the storage
/// instance's `getAttachmentData`.
pub fn export_document(
    document: &Value,
    attachments: Option<&std::collections::BTreeMap<String, DumpAttachment>>,
) -> Result<Value> {
    let map = document
        .as_object()
        .ok_or_else(|| Error::Malformed(document.to_string()))?;
    let mut out = map.clone();
    // `delete docData._rev; delete docData._attachments;` (`:126-128`).
    out.remove("_rev");
    out.remove("_attachments");
    if let Some(payloads) = attachments {
        if !payloads.is_empty() {
            let inlined: Map<String, Value> = payloads
                .iter()
                .map(|(id, attachment)| (id.clone(), attachment.to_json()))
                .collect();
            out.insert("_attachments".to_string(), Value::Object(inlined));
        }
    }
    Ok(Value::Object(out))
}

/// Turns a dumped document back into a write row (`:193-214`).
///
/// Assigns a **fresh** revision and a fresh `_meta.lwt`, which is the rule that makes a
/// restored document a new revision chain: the dump has no valid `_rev` to continue from.
/// `new_revision` and `now_ms` are parameters rather than read from a clock so the
/// transformation is deterministic and testable.
pub fn import_document(dumped: &Value, revision: &str, now_ms: f64) -> Result<Value> {
    let map = dumped
        .as_object()
        .ok_or_else(|| Error::Malformed(dumped.to_string()))?;
    let mut document = map.clone();
    document.insert("_rev".to_string(), Value::String(revision.to_string()));
    document.insert("_meta".to_string(), json!({ "lwt": now_ms }));
    document.insert("_deleted".to_string(), Value::Bool(false));
    // `_attachments` in a dump is base64, not the write shape, so it must not be carried
    // through verbatim: the caller converts it and re-inserts it. Emitting an empty map
    // keeps the field present, which the schema requires.
    document.insert("_attachments".to_string(), Value::Object(Map::new()));
    Ok(Value::Object(document))
}

/// Checks a collection dump's schema hash against the collection's (`:187-192`).
pub fn check_schema_hash(dump: &CollectionDump, own_hash: &str) -> Result<()> {
    if dump.schema_hash != own_hash {
        return Err(Error::SchemaHashMismatch {
            dump: dump.schema_hash.clone(),
            own: own_hash.to_string(),
        });
    }
    Ok(())
}

/// Reads a collection's documents for export (`:120-122` queries with no filter).
pub trait DumpSource {
    /// Every document, tombstones excluded — `_getDefaultQuery` filters `_deleted`
    /// (`rx-query.ts`).
    fn all_documents(&mut self) -> Result<Vec<Value>>;

    /// The collection's schema hash (`:117`).
    fn schema_hash(&self) -> String;

    /// The collection's name.
    fn collection_name(&self) -> String;

    /// Attachment bytes for a document, when `options.attachments` is set
    /// (`:94-98`).
    fn attachment_bytes(
        &mut self,
        _document_id: &str,
        _attachment_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        Ok(None)
    }

    /// The primary key path, to find the document id for the attachment lookup.
    fn primary_path(&self) -> String {
        "id".to_string()
    }
}

/// Writes imported documents (`:212-218`).
pub trait DumpSink {
    /// Bulk-writes the rows.
    fn bulk_write(&mut self, documents: Vec<Value>) -> Result<()>;
}

/// `dumpRxCollection` (`:109-147`).
pub fn export_collection<S: DumpSource>(
    source: &mut S,
    options: DumpOptions,
) -> Result<CollectionDump> {
    let primary_path = source.primary_path();
    let documents = source.all_documents()?;
    let mut dumped = Vec::with_capacity(documents.len());
    for document in documents {
        let attachments = if options.attachments {
            let id = document
                .get(&primary_path)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let ids: Vec<String> = document
                .get("_attachments")
                .and_then(Value::as_object)
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();
            let mut payloads = std::collections::BTreeMap::new();
            for attachment_id in ids {
                if let Some((mime_type, bytes)) = source.attachment_bytes(&id, &attachment_id)? {
                    payloads.insert(attachment_id, DumpAttachment::from_bytes(mime_type, &bytes));
                }
            }
            Some(payloads)
        } else {
            None
        };
        dumped.push(export_document(&document, attachments.as_ref())?);
    }
    Ok(CollectionDump {
        name: source.collection_name(),
        schema_hash: source.schema_hash(),
        docs: dumped,
    })
}

/// `importDumpRxCollection` (`:185-219`).
///
/// `revision` is supplied by the caller because a revision contains the database's
/// instance token ([`dxdb_core`]'s write path builds it); the dump deliberately does not
/// carry one.
pub fn import_collection<T: DumpSink>(
    dump: &CollectionDump,
    own_hash: &str,
    revision: &str,
    now_ms: f64,
    sink: &mut T,
) -> Result<()> {
    check_schema_hash(dump, own_hash)?;
    let documents = dump
        .docs
        .iter()
        .map(|document| import_document(document, revision, now_ms))
        .collect::<Result<Vec<_>>>()?;
    sink.bulk_write(documents)
}

/// `dumpRxDatabase` (`:30-56`).
pub fn export_database(
    name: &str,
    instance_token: &str,
    collections: Vec<CollectionDump>,
) -> DatabaseDump {
    DatabaseDump {
        name: name.to_string(),
        instance_token: instance_token.to_string(),
        collections,
    }
}

/// Standard base64, which is what `blobToBase64String` produces
/// (`utils-base64.ts`).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3F] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3F] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3F] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3F] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut triple = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            let digit = value(*byte).ok_or_else(|| Error::InvalidBase64(text.to_string()))?;
            triple |= (digit as u32) << (18 - 6 * index);
        }
        out.push((triple >> 16) as u8);
        if chunk.len() > 2 {
            out.push((triple >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(triple as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for input in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let encoded = base64_encode(input);
            assert_eq!(base64_decode(&encoded).expect("decode"), input, "{input:?}");
        }
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        // RFC 4648 §10, which is what `blobToBase64String` produces.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn an_export_drops_the_revision() {
        let document = json!({ "id": "a", "_rev": "1-abc", "name": "x", "_attachments": {} });
        let dumped = export_document(&document, None).expect("export");
        assert!(dumped.get("_rev").is_none(), "a revision is database-local");
        assert!(dumped.get("_attachments").is_none());
        assert_eq!(dumped["name"], json!("x"));
    }

    #[test]
    fn an_import_assigns_a_fresh_revision_and_lwt() {
        let dumped = json!({ "id": "a", "name": "x" });
        let imported = import_document(&dumped, "1-newtoken", 42.0).expect("import");
        assert_eq!(imported["_rev"], json!("1-newtoken"));
        // Compare numerically: `json!(42.0)` carries an f64 and renders as `42.0` in a
        // `PartialEq` against `json!(42)`, but the JS-semantics rendering is `42`, which is
        // what `dxdb_json::to_js_json` emits and what the dump will contain.
        assert_eq!(imported["_meta"]["lwt"].as_f64(), Some(42.0));
        assert_eq!(dxdb_json::to_js_json(&imported["_meta"]), r#"{"lwt":42}"#);
        assert_eq!(imported["_deleted"], json!(false));
        assert_eq!(imported["_attachments"], json!({}));
    }
}
