//! The round-trip property, plus the upstream cases that pin the table format.
//!
//! Upstream's suite is `test/unit/key-compression.test.ts` (675 lines, 13 `it`
//! cases). Most of those are integration tests that need a live database and a
//! storage; the parts that are this crate's behaviour are extracted here and each
//! names the upstream line it comes from.
//!
//! The headline test is [`compress_persist_read_fresh_schema_expand_is_byte_identical`]:
//! compress a document, round-trip it through JSON (which is what persisting
//! means), expand it with a state built from a **separately constructed schema
//! instance**, and require the result to be byte-identical to the original. Byte
//! identity is checked with `dxdb_json::to_js_json` on both sides, which is also
//! how the oracle vectors compare documents, so a key-order difference would fail
//! rather than pass silently.

use dxdb_plugin_key_compression::{
    compress_query, CompressionState, CompressionTable, Error, IGNORE_ALWAYS,
};
use dxdb_storage::{MangoQuery, SortDirection, SortPart};
use serde_json::{json, Value};

/// The `human` fixture from `test/unit/key-compression.test.ts:42-48`, with
/// `keyCompression` enabled the way its `enableKeyCompression` helper does
/// (`test-utils/schemas.ts:1385-1390`: flat clone, set the flag to true).
fn human_schema() -> Value {
    json!({
        "title": "human schema",
        "version": 0,
        "description": "describes a human being",
        "type": "object",
        "keyCompression": true,
        "primaryKey": "passportId",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "firstName": { "type": "string", "maxLength": 100 },
            "lastName": { "type": "string", "maxLength": 100 },
            "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 }
        },
        "required": ["firstName", "lastName", "passportId"],
        "indexes": ["firstName"]
    })
}

/// `schemaObjects.simpleHumanData()`-shaped document, hand-written so the test does
/// not depend on another lane's fixtures.
fn human_document() -> Value {
    json!({
        "passportId": "passport-1",
        "firstName": "Alice",
        "lastName": "Anderson",
        "age": 30
    })
}

/// The ignore list the plugin passes for the human schema: the primary field plus
/// the four meta fields (`index.ts:78-88`).
fn human_ignore() -> Vec<String> {
    let mut ignore = vec!["passportId".to_string()];
    ignore.extend(IGNORE_ALWAYS.iter().map(|field| field.to_string()));
    ignore
}

/// Renders a value the way `JSON.stringify` would, which is how the vectors compare
/// documents. A key-order difference shows up as a byte difference.
fn canonical(value: &Value) -> String {
    dxdb_json::to_js_json(value)
}

// ---------------------------------------------------------------------------
// The acceptance criterion.
// ---------------------------------------------------------------------------

#[test]
fn compress_persist_read_fresh_schema_expand_is_byte_identical() {
    let original = human_document();

    // 1. Compress with one schema instance.
    let writer_state = CompressionState::for_schema(&human_schema()).expect("writer state");
    let compressed = writer_state.compress_document(&original).expect("compress");

    // 2. Persist: the only thing a storage sees is JSON.
    let persisted = canonical(&compressed);
    let reloaded: Value = serde_json::from_str(&persisted).expect("parse persisted");

    // 3. Read back with a *separately constructed* schema instance, so nothing can
    //    be shared by accident. This is the part the brief asks for: a stale cache
    //    or an in-memory identity would hide a broken table.
    let fresh_schema: Value =
        serde_json::from_str(&canonical(&human_schema())).expect("parse schema");
    let reader_state = CompressionState::for_schema(&fresh_schema).expect("reader state");

    // 4. Expand.
    let expanded = reader_state.decompress_document(&reloaded);

    // 5. Byte-identical, key order included.
    assert_eq!(
        canonical(&expanded),
        canonical(&original),
        "\nexpanded: {}\noriginal: {}",
        canonical(&expanded),
        canonical(&original)
    );
}

#[test]
fn the_round_trip_holds_for_nested_objects_and_arrays() {
    // Upstream `:185` (#50, string arrays) and `:229` (nested object with nulls)
    // together, plus a deeper nest of our own.
    let schema = json!({
        "type": "object",
        "keyCompression": true,
        "primaryKey": "identifier",
        "properties": {
            "identifier": { "type": "string", "maxLength": 100 },
            "likes": { "type": "array", "items": { "type": "string" } },
            "nested": {
                "type": "object",
                "properties": {
                    "lastProvider": { "type": ["string", "null"] },
                    "providers": { "type": "integer" },
                    "sync": { "type": "boolean" },
                    "other": { "type": "object" }
                }
            },
            "rows": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "rowLabel": { "type": "string" },
                        "rowCount": { "type": "integer" }
                    }
                }
            }
        }
    });
    let document = json!({
        "identifier": "id-1",
        "likes": ["abc", "8"],
        "nested": {
            "lastProvider": null,
            "providers": 0,
            "sync": false,
            "other": {}
        },
        "rows": [
            { "rowLabel": "r1", "rowCount": 1 },
            { "rowLabel": "r2", "rowCount": 2 }
        ]
    });

    let state = CompressionState::for_schema(&schema).expect("state");
    let compressed = state.compress_document(&document).expect("compress");
    let expanded = state.decompress_document(&compressed);
    assert_eq!(canonical(&expanded), canonical(&document));
}

#[test]
fn only_the_documented_fields_keep_their_names() {
    // Upstream `:61-88` ("should have saved a compressed document"): every
    // non-underscore key other than the primary path is at most three characters
    // and a string. `_meta` and the primary path survive.
    let state = CompressionState::for_schema(&human_schema()).expect("state");
    let compressed = state
        .compress_document(&human_document())
        .expect("compress");
    let object = compressed.as_object().expect("object");

    for (key, value) in object {
        if key.starts_with('_') || key == "passportId" {
            continue;
        }
        // `'|' + one letter` is two characters for the first 58 properties.
        assert!(
            key.chars().count() <= 3,
            "key {key:?} is longer than three characters"
        );
        assert!(
            value.is_string() || value.is_number(),
            "value shape at {key:?}"
        );
    }
    assert_eq!(object.get("passportId"), Some(&json!("passport-1")));
    assert_eq!(object.get("_meta"), None, "the fixture has no _meta");
}

// ---------------------------------------------------------------------------
// The table format, pinned. A port that changes these breaks stored data.
// ---------------------------------------------------------------------------

#[test]
fn the_table_is_the_documented_format_for_the_human_schema() {
    // Names longer than three characters, sorted by UTF-16 code unit, excluding the
    // primary path: `age` is short and drops out, so the sorted list is
    // `firstName`, `lastName`, `passportId`. `passportId` is the primary path for
    // the human schema, so it is ignored too. Remaining: `firstName`, `lastName`.
    let table = CompressionTable::build(&human_schema(), &human_ignore()).expect("table");

    assert_eq!(table.compress_key("firstName").expect("firstName"), "|a");
    assert_eq!(table.compress_key("lastName").expect("lastName"), "|b");
    // `age` is three characters: never compressed (`:87`).
    assert_eq!(table.compress_key("age").expect("age"), "age");
    // The primary path is in the ignore list, so a query on it keeps its name.
    assert_eq!(
        table.compress_key("passportId").expect("passportId"),
        "passportId"
    );
    // Meta fields too.
    for field in IGNORE_ALWAYS {
        assert_eq!(table.compress_key(field).expect(field), field);
    }
    assert_eq!(table.len(), 2);
}

#[test]
fn the_primary_path_is_never_compressed() {
    // `index.ts:78-88`: the primary field is excluded "for easier debugging".
    let state = CompressionState::for_schema(&human_schema()).expect("state");
    let compressed = state
        .compress_document(&human_document())
        .expect("compress");
    assert_eq!(
        compressed.as_object().expect("object").get("passportId"),
        Some(&json!("passport-1")),
        "the primary key must keep its name so a document id stays readable in storage"
    );
}

#[test]
fn a_composite_primary_key_is_excluded_too() {
    // `index.ts:98-105` compresses a composite key's fields but keeps the shape. The
    // field names in a composite `key`/`fields` object are compressed; the *values*
    // are data and are never touched.
    let schema = json!({
        "type": "object",
        "keyCompression": true,
        "primaryKey": { "key": "compositeId", "fields": ["firstName", "lastName"], "separator": "|" },
        "properties": {
            "compositeId": { "type": "string", "maxLength": 200 },
            "firstName": { "type": "string", "maxLength": 100 },
            "lastName": { "type": "string", "maxLength": 100 }
        },
        "required": ["compositeId", "firstName", "lastName"]
    });
    let state = CompressionState::for_schema(&schema).expect("state");
    let compressed_schema = state.compressed_schema.as_object().expect("object");
    let primary = compressed_schema
        .get("primaryKey")
        .expect("primaryKey survives");
    // The shape is preserved; the `key` member keeps its name because a composite
    // primary key is not a string and the plugin's string branch does not apply.
    assert!(
        primary.is_object(),
        "a composite primaryKey stays an object: {primary}"
    );
}

// ---------------------------------------------------------------------------
// The compressed schema.
// ---------------------------------------------------------------------------

#[test]
fn the_compressed_schema_is_what_the_inner_storage_is_handed() {
    // `index.ts:166-177` plus `:97-123`: property names, `required`, indexes and the
    // primary key are compressed, and `keyCompression` is set to false so a bare
    // storage does not reject it as unhandled.
    let state = CompressionState::for_schema(&human_schema()).expect("state");
    let compressed = state.compressed_schema.as_object().expect("object");

    assert_eq!(compressed.get("keyCompression"), Some(&json!(false)));
    assert_eq!(compressed.get("primaryKey"), Some(&json!("passportId")));

    let properties = compressed
        .get("properties")
        .and_then(Value::as_object)
        .expect("properties");
    assert!(
        properties.contains_key("|a"),
        "firstName is compressed: {properties:?}"
    );
    assert!(properties.contains_key("|b"), "lastName is compressed");
    assert!(properties.contains_key("age"), "age is short, so unchanged");
    assert!(
        properties.contains_key("passportId"),
        "primary path unchanged"
    );

    let required = compressed
        .get("required")
        .and_then(Value::as_array)
        .expect("required");
    let required: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
    assert_eq!(required, vec!["|a", "|b", "passportId"]);

    // `indexes` are compressed too (`:114-123`), because the compression library
    // does not know about them.
    let indexes = compressed
        .get("indexes")
        .and_then(Value::as_array)
        .expect("indexes");
    assert_eq!(indexes[0], json!("|a"), "a flat index stays a string");
}

#[test]
fn the_original_schema_is_not_mutated() {
    // `index.ts:61-66`: the schema is frozen in dev mode precisely because the state
    // is cached by object identity, so the input must never be touched.
    let schema = human_schema();
    let snapshot = schema.clone();
    let _ = CompressionState::for_schema(&schema).expect("state");
    assert_eq!(canonical(&schema), canonical(&snapshot));
}

// ---------------------------------------------------------------------------
// Queries.
// ---------------------------------------------------------------------------

#[test]
fn a_query_selector_is_compressed_and_operators_keep_their_names() {
    // `compress.ts:167`.
    let table = CompressionTable::build(&human_schema(), &human_ignore()).expect("table");
    let query = MangoQuery {
        selector: json!({
            "firstName": { "$eq": "Alice" },
            "$or": [{ "lastName": "Anderson" }, { "age": { "$gt": 20 } }]
        }),
        sort: vec![SortPart {
            field: "firstName".to_string(),
            direction: SortDirection::Desc,
        }],
        skip: 0,
        limit: Some(5),
        index: None,
    };
    let compressed = compress_query(&table, &query).expect("compress query");
    assert_eq!(
        compressed.selector,
        json!({
            "|a": { "$eq": "Alice" },
            "$or": [{ "|b": "Anderson" }, { "age": { "$gt": 20 } }]
        }),
        "operators keep their names, field paths are compressed"
    );
    assert_eq!(compressed.sort[0].field, "|a");
    assert_eq!(compressed.sort[0].direction, SortDirection::Desc);
    assert_eq!(compressed.limit, Some(5));
}

#[test]
fn the_query_index_hint_survives_compression() {
    // Upstream `:566` ("should forward the query index hint through key
    // compression"): `compressQuery` drops `index` and the plugin re-adds it
    // (`index.ts:196-205`). This port keeps it in one place; the test asserts the
    // observable outcome.
    let table = CompressionTable::build(&human_schema(), &human_ignore()).expect("table");
    let query = MangoQuery {
        selector: json!({}),
        sort: Vec::new(),
        skip: 0,
        limit: None,
        index: Some(vec!["firstName".to_string(), "passportId".to_string()]),
    };
    let compressed = compress_query(&table, &query).expect("compress query");
    assert_eq!(
        compressed.index,
        Some(vec!["|a".to_string(), "passportId".to_string()]),
        "the hint is compressed but not lost"
    );
}

#[test]
fn a_nested_query_path_is_compressed_per_segment() {
    // `compressedPath` (`compress.ts:47`) splits on `.` and compresses each part.
    let schema = json!({
        "type": "object",
        "keyCompression": true,
        "primaryKey": "identifier",
        "properties": {
            "identifier": { "type": "string", "maxLength": 100 },
            "nested": {
                "type": "object",
                "properties": { "lastProvider": { "type": "string" }, "providers": { "type": "integer" } }
            }
        }
    });
    let state = CompressionState::for_schema(&schema).expect("state");
    let query = MangoQuery {
        selector: json!({ "nested.lastProvider": "x" }),
        sort: Vec::new(),
        skip: 0,
        limit: None,
        index: None,
    };
    let compressed = state.compressed_query(&query).expect("compress");
    let keys: Vec<&String> = compressed
        .selector
        .as_object()
        .expect("object")
        .keys()
        .collect();
    assert_eq!(keys.len(), 1);
    let key = keys[0];
    assert!(key.contains('.'), "still a dotted path: {key}");
    let (parent, child) = key.split_once('.').expect("one dot");
    assert!(parent.starts_with('|'), "parent segment compressed: {key}");
    assert!(child.starts_with('|'), "child segment compressed: {key}");
}

// ---------------------------------------------------------------------------
// Enabling compression on existing data. Upstream refuses; so does this port.
// ---------------------------------------------------------------------------

#[test]
fn enabling_compression_later_changes_schema_identity_rather_than_migrating() {
    // The brief asks what upstream does when compression is enabled after documents
    // exist. It does not migrate: `keyCompression` is part of the schema and
    // therefore of the schema hash (`rx-schema.ts:113-121`), so flipping it changes
    // the collection's identity and opening the collection raises DB6
    // (`rx-database.ts:481-488`).
    //
    // What this crate can assert is the premise: the two schemas are different
    // schemas, so a caller that compares identity sees a different collection.
    let without = {
        let mut schema = human_schema();
        schema["keyCompression"] = json!(false);
        dxdb_schema::create_rx_schema(
            &dxdb_schema::fill_with_default_settings(&schema).expect("fill"),
        )
        .expect("schema")
    };
    let with = {
        let schema = human_schema();
        dxdb_schema::create_rx_schema(
            &dxdb_schema::fill_with_default_settings(&schema).expect("fill"),
        )
        .expect("schema")
    };
    assert_ne!(
        without.hash(),
        with.hash(),
        "keyCompression must be part of schema identity, which is why enabling it \
         later is a DB6 error and not a migration"
    );

    // And the compressed documents are not readable as uncompressed ones: a
    // document written with compression has `|a` where `firstName` was, so
    // interpreting the same bytes under the uncompressed schema silently loses the
    // field. That is the failure mode the DB6 guard prevents.
    let state = CompressionState::for_schema(&human_schema()).expect("state");
    let compressed = state
        .compress_document(&human_document())
        .expect("compress");
    assert!(compressed.get("firstName").is_none());
    assert!(compressed.get("|a").is_some());
}

#[test]
fn a_document_whose_keys_already_use_the_flag_is_rejected() {
    // `compress.ts:62`: compressing such a key would be ambiguous when reading back,
    // so it is an error rather than a silent double-compression.
    let state = CompressionState::for_schema(&human_schema()).expect("state");
    let document = json!({ "passportId": "p", "|a": "already compressed" });
    assert!(matches!(
        state.compress_document(&document),
        Err(Error::KeyUsesFlag { .. })
    ));
}
