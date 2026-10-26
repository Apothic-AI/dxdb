//! Ported from the export/import plugin's behaviour plus the dump format's own invariants.
//!
//! **There is no `test/unit/json-dump.test.ts` upstream.** The plugin ships without a unit
//! test file; its behaviour is exercised indirectly by the backup plugin
//! (`test/unit/backup.test.ts`) and by the conformance suites. So every test here is
//! labelled with the *source line* it pins rather than with an upstream `it(...)` line, and
//! the crate docs say plainly that the corpus is inferred from source rather than ported.
//! Claiming a ported count for this plugin would be false.

use dxdb_plugin_json_dump::{
    check_schema_hash, export_collection, export_database, export_document, import_collection,
    import_document, CollectionDump, DatabaseDump, DumpAttachment, DumpOptions, DumpSink,
    DumpSource, Error,
};
use serde_json::json;
use std::collections::BTreeMap;

/// A collection with a fixed document set.
struct Source {
    documents: Vec<serde_json::Value>,
    hash: String,
    name: String,
    attachments: BTreeMap<(String, String), (String, Vec<u8>)>,
}

impl Source {
    fn new(name: &str, hash: &str, count: usize) -> Self {
        Source {
            documents: (0..count)
                .map(|index| {
                    json!({
                        "id": format!("doc-{index}"),
                        "_rev": format!("1-token{index}"),
                        "_meta": { "lwt": index as f64 + 1.0 },
                        "_deleted": false,
                        "_attachments": {},
                        "value": index,
                    })
                })
                .collect(),
            hash: hash.to_string(),
            name: name.to_string(),
            attachments: BTreeMap::new(),
        }
    }
}

impl DumpSource for Source {
    fn all_documents(&mut self) -> Result<Vec<serde_json::Value>, Error> {
        Ok(self.documents.clone())
    }
    fn schema_hash(&self) -> String {
        self.hash.clone()
    }
    fn collection_name(&self) -> String {
        self.name.clone()
    }
    fn attachment_bytes(
        &mut self,
        document_id: &str,
        attachment_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>, Error> {
        Ok(self
            .attachments
            .get(&(document_id.to_string(), attachment_id.to_string()))
            .cloned())
    }
}

#[derive(Default)]
struct Sink {
    written: Vec<serde_json::Value>,
}

impl DumpSink for Sink {
    fn bulk_write(&mut self, documents: Vec<serde_json::Value>) -> Result<(), Error> {
        self.written.extend(documents);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// `index.ts:109-147` — dumpRxCollection
// ---------------------------------------------------------------------------

#[test]
fn source_109_a_collection_dump_carries_name_hash_and_docs() {
    // `{name, schemaHash, docs}` (`:115-119`).
    let mut source = Source::new("humans", "hash-abc", 3);
    let dump = export_collection(&mut source, DumpOptions::default()).expect("export");
    assert_eq!(dump.name, "humans");
    assert_eq!(dump.schema_hash, "hash-abc");
    assert_eq!(dump.docs.len(), 3);
}

#[test]
fn source_126_export_drops_every_revision() {
    // `delete docData._rev; delete docData._attachments;` (`:126-128`). A revision belongs
    // to the database that issued it.
    let mut source = Source::new("humans", "h", 3);
    let dump = export_collection(&mut source, DumpOptions::default()).expect("export");
    for document in &dump.docs {
        assert!(
            document.get("_rev").is_none(),
            "no revision survives a dump: {document}"
        );
        assert!(document.get("_attachments").is_none());
    }
}

#[test]
fn source_113_attachments_are_base64_inlined_only_when_requested() {
    // `const withAttachments = !!(options && options.attachments)` (`:113`).
    let mut source = Source::new("humans", "h", 1);
    source.documents[0]["_attachments"] =
        json!({ "file.txt": { "digest": "d", "type": "text/plain" } });
    source.attachments.insert(
        ("doc-0".to_string(), "file.txt".to_string()),
        ("text/plain".to_string(), b"foobar".to_vec()),
    );

    let without = export_collection(&mut source, DumpOptions::default()).expect("export");
    assert!(
        without.docs[0].get("_attachments").is_none(),
        "not requested"
    );

    let with = export_collection(&mut source, DumpOptions { attachments: true }).expect("export");
    let attachments = with.docs[0]["_attachments"]
        .as_object()
        .expect("attachments");
    let file = attachments.get("file.txt").expect("file");
    assert_eq!(file["type"], json!("text/plain"));
    assert_eq!(file["length"], json!(6));
    assert_eq!(file["data"], json!("Zm9vYmFy"), "base64 of 'foobar'");
}

// ---------------------------------------------------------------------------
// `index.ts:185-219` — importDumpRxCollection
// ---------------------------------------------------------------------------

#[test]
fn source_187_import_rejects_a_different_schema_hash() {
    // `JD2` (`:187-192`).
    let dump = CollectionDump {
        name: "humans".to_string(),
        schema_hash: "hash-abc".to_string(),
        docs: vec![json!({ "id": "a" })],
    };
    let error = check_schema_hash(&dump, "hash-different").expect_err("must reject");
    assert_eq!(
        error,
        Error::SchemaHashMismatch {
            dump: "hash-abc".to_string(),
            own: "hash-different".to_string()
        }
    );
    // And the same hash passes.
    check_schema_hash(&dump, "hash-abc").expect("matching hash");
}

#[test]
fn source_187_a_rejected_import_writes_nothing() {
    let dump = CollectionDump {
        name: "humans".to_string(),
        schema_hash: "a".to_string(),
        docs: vec![json!({ "id": "x" })],
    };
    let mut sink = Sink::default();
    assert!(import_collection(&dump, "b", "1-tok", 1.0, &mut sink).is_err());
    assert!(sink.written.is_empty(), "no partial import");
}

#[test]
fn source_199_import_assigns_fresh_bookkeeping() {
    // `Object.assign({}, docData, {_meta: {lwt: now()}, _rev: getDefaultRevision(), ...})`
    // (`:199-210`). A restored document is a new revision chain.
    let dumped = json!({ "id": "a", "value": 1 });
    let imported = import_document(&dumped, "1-fresh", 1234.0).expect("import");
    assert_eq!(imported["_rev"], json!("1-fresh"));
    assert_eq!(imported["_meta"]["lwt"].as_f64(), Some(1234.0));
    assert_eq!(imported["_deleted"], json!(false));
    assert_eq!(imported["value"], json!(1), "the payload survives");
}

#[test]
fn source_185_a_full_round_trip_preserves_the_payload() {
    let mut source = Source::new("humans", "hash-abc", 4);
    let dump = export_collection(&mut source, DumpOptions::default()).expect("export");
    let mut sink = Sink::default();
    import_collection(&dump, "hash-abc", "1-tok", 9.0, &mut sink).expect("import");

    assert_eq!(sink.written.len(), 4);
    for (index, document) in sink.written.iter().enumerate() {
        assert_eq!(document["id"], json!(format!("doc-{index}")));
        assert_eq!(document["value"], json!(index));
        assert_eq!(
            document["_rev"],
            json!("1-tok"),
            "every row gets the new revision"
        );
        assert_eq!(document["_meta"]["lwt"].as_f64(), Some(9.0));
    }
}

// ---------------------------------------------------------------------------
// `index.ts:30-79` — database-level dump and import
// ---------------------------------------------------------------------------

#[test]
fn source_66_import_requires_every_collection_to_exist() {
    // `JD1` (`:66-73`).
    let dump = DatabaseDump {
        name: "db".to_string(),
        instance_token: "tok".to_string(),
        collections: vec![
            CollectionDump {
                name: "humans".to_string(),
                schema_hash: "h".to_string(),
                docs: Vec::new(),
            },
            CollectionDump {
                name: "pets".to_string(),
                schema_hash: "p".to_string(),
                docs: Vec::new(),
            },
        ],
    };
    let error = dump
        .check_collections_exist(&["humans"])
        .expect_err("pets is missing");
    assert_eq!(error, Error::MissingCollections(vec!["pets".to_string()]));
    dump.check_collections_exist(&["humans", "pets"])
        .expect("both exist");
}

#[test]
fn source_38_a_database_dump_carries_name_token_and_collections() {
    let collection = CollectionDump {
        name: "humans".to_string(),
        schema_hash: "h".to_string(),
        docs: vec![json!({ "id": "a" })],
    };
    let dump = export_database("db", "instance-token", vec![collection]);
    assert_eq!(dump.name, "db");
    assert_eq!(dump.instance_token, "instance-token");
    assert_eq!(dump.collections.len(), 1);
}

#[test]
fn a_database_dump_round_trips_through_json() {
    // The dump is a file format, so the wire shape has to survive serialisation.
    let dump = export_database(
        "db",
        "tok",
        vec![CollectionDump {
            name: "humans".to_string(),
            schema_hash: "h".to_string(),
            docs: vec![json!({ "id": "a", "n": 1 })],
        }],
    );
    let json = dump.to_json();
    let parsed = DatabaseDump::from_json(&json).expect("parse");
    assert_eq!(parsed, dump);
    // And the serialised form is plain JSON, which is the point of base64 for attachments.
    assert!(dxdb_json::to_js_json(&json).contains("\"schemaHash\":\"h\""));
}

#[test]
fn a_malformed_dump_is_rejected_with_a_message() {
    for bad in [
        json!({}),
        json!({ "name": "db" }),
        json!({ "name": "db", "collections": [{}] }),
        json!({ "name": "db", "collections": [{ "name": "c" }] }),
    ] {
        assert!(
            DatabaseDump::from_json(&bad).is_err(),
            "should reject {bad}"
        );
    }
}

// ---------------------------------------------------------------------------
// Attachment encoding
// ---------------------------------------------------------------------------

#[test]
fn source_99_an_attachment_round_trips_through_base64() {
    // `{type, length, data}` (`:99-103`) with the digest recomputed on import (`:178-184`)
    // — which is why the digest is *not* in the dump.
    let original = b"\x00\x01binary\xffdata";
    let attachment = DumpAttachment::from_bytes("application/octet-stream", original);
    assert_eq!(attachment.length, original.len());
    let json = attachment.to_json();
    assert!(
        json.get("digest").is_none(),
        "a digest is a function of the hashFunction, so it is not stored"
    );
    let parsed = DumpAttachment::from_json(&json).expect("parse");
    assert_eq!(parsed, attachment);
    assert_eq!(parsed.to_bytes().expect("decode"), original);
}

#[test]
fn invalid_base64_is_an_error_not_a_silent_empty_attachment() {
    let attachment = DumpAttachment {
        mime_type: "text/plain".to_string(),
        length: 3,
        base64: "not base64!!".to_string(),
    };
    assert!(matches!(
        attachment.to_bytes(),
        Err(Error::InvalidBase64(_))
    ));
}

#[test]
fn export_without_attachments_never_emits_the_field() {
    let document = json!({ "id": "a", "_attachments": { "x": { "digest": "d" } } });
    let dumped = export_document(&document, None).expect("export");
    assert!(dumped.get("_attachments").is_none());
}

#[test]
fn export_with_an_empty_attachment_map_does_not_emit_the_field() {
    // Upstream only inlines when `Object.keys(attachments).length > 0` (`:131-134`), so an
    // empty map must not produce `_attachments: {}` — which would import as "has
    // attachments" for nothing.
    let document = json!({ "id": "a", "_attachments": {} });
    let dumped = export_document(&document, Some(&BTreeMap::new())).expect("export");
    assert!(dumped.get("_attachments").is_none());
}
