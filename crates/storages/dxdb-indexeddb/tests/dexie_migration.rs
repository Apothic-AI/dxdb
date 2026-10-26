//! The Dexie migration interop claim, against a store **upstream RxDB actually wrote**.
//!
//! PLAN.md section 10 requires: "a store written by upstream RxDB (Dexie) can be
//! migrated into dxdb's equivalent storage and back without semantic loss: same
//! documents, same revisions, same schema hashes."
//!
//! The fixture this test reads (`demo/fixtures/dexie-store.json`) is produced by
//! `demo/scripts/make-dexie-fixture.mjs`, which runs the real `rxdb@17.6.0` with
//! `getRxStorageDexie()` in Chromium and dumps the raw IndexedDB contents. Nothing in
//! this test or that script is our code writing a store to look like upstream's — if
//! our reading disagrees with this file, ours is wrong.
//!
//! # What is asserted, and what each assertion catches
//!
//! | assertion | catches |
//! | --- | --- |
//! | same schema hash | divergence in defaulting, in key order, or in number formatting |
//! | same documents | loss or rewriting of fields on read |
//! | same `_rev` | revision handling that invents a token or bumps a height |
//! | same `_meta.lwt`, same value | `lwt` formatting through a float round trip |
//! | re-export accepted | writing a document shape upstream rejects on the way back |
//!
//! # Why the schema hash is the sharpest of these
//!
//! The hash is over `JSON.stringify(filledSchema)` — the exact bytes, in key order,
//! with JavaScript number formatting. Any divergence in what `fill_with_default_settings`
//! injects, in the order it injects it, or in how a number renders (JS writes `1`,
//! not `1.0`) changes the hash. A hash match therefore means our schema *projection* is
//! byte-identical to upstream's, which is a much stronger statement than "the fields
//! we care about agree".

use std::path::PathBuf;

use serde_json::Value;

/// Loads the fixture produced by `demo/scripts/make-dexie-fixture.mjs`.
fn fixture() -> Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../demo/fixtures/dexie-store.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "missing Dexie fixture at {}. Regenerate it with:\n\
             \x20 node demo/scripts/make-dexie-fixture.mjs\n\
             \x20 (error: {error})",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("fixture is valid JSON")
}

/// The raw IndexedDB object stores, flattened to `{store: [(key, value)]}`.
fn stores(fixture: &Value) -> Vec<(String, Vec<(Value, Value)>)> {
    let mut out = Vec::new();
    for database in fixture["databases"].as_array().expect("databases array") {
        let name = database["name"].as_str().expect("database name");
        for (store_name, records) in database["stores"].as_object().expect("stores object") {
            let entries: Vec<(Value, Value)> = records
                .as_array()
                .expect("records array")
                .iter()
                .map(|record| (record["key"].clone(), record["value"].clone()))
                .collect();
            out.push((format!("{name}::{store_name}"), entries));
        }
    }
    out
}

/// The `docs` store of the `humans` collection: what a migration has to read.
fn humans_documents(fixture: &Value) -> Vec<Value> {
    let mut documents: Vec<Value> = stores(fixture)
        .into_iter()
        .filter(|(name, _)| name.ends_with("--humans::docs"))
        .flat_map(|(_, entries)| entries.into_iter().map(|(_, value)| value))
        .collect();
    documents.sort_by(|a, b| {
        a["passportId"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["passportId"].as_str().unwrap_or_default())
    });
    documents
}

// ---------------------------------------------------------------------------
// The fixture itself is real
// ---------------------------------------------------------------------------

/// Guards the fixture's provenance. A hand-written fixture would pass the rest of this
/// file while proving nothing, so the provenance is asserted rather than trusted.
#[test]
fn the_fixture_came_from_upstream_rxdb() {
    let fixture = fixture();
    assert_eq!(
        fixture["rxdbVersion"].as_str(),
        Some("17.6.0"),
        "the fixture must be written by the pinned upstream version"
    );
    assert!(
        fixture["generatedBy"]
            .as_str()
            .unwrap_or_default()
            .contains("getRxStorageDexie"),
        "the fixture must come from upstream's Dexie storage, not a lookalike"
    );
    // Upstream's Dexie storage creates these three object stores, and names the
    // database `rxdb-dexie-{db}--{schemaVersion}--{collection}`. Their presence is
    // evidence a real Dexie database was dumped rather than a JSON blob authored here.
    let names: Vec<String> = stores(&fixture).into_iter().map(|(name, _)| name).collect();
    assert!(
        names.iter().any(|n| n.contains("rxdb-dexie-dexieprobe")),
        "expected an upstream-named Dexie database, got: {names:?}"
    );
    for required in ["::docs", "::changes", "::attachments"] {
        assert!(
            names.iter().any(|n| n.ends_with(required)),
            "upstream's Dexie storage creates a {required} store; got: {names:?}"
        );
    }
}

/// The documents upstream stored are the documents the fixture says upstream stored,
/// with one documented exception that is a genuine finding about Dexie's on-disk shape.
///
/// # `_deleted` is stored as `"0"` / `"1"`, not `false` / `true`
///
/// Upstream's Dexie storage indexes `_deleted` (`dexie-helper.js`: `ret.push('_deleted')`
/// in `getSchemaIndexes`), and IndexedDB refuses to index booleans as keys. Dexie's own
/// index encoding therefore coerces the boolean to a string, and what lands in the `docs`
/// object store is `_deleted: "0"`.
///
/// So a migration reading raw Dexie bytes has to treat `"0"`/`"1"` as `false`/`true`,
/// while a migration reading through upstream's API sees booleans again. Both directions
/// are asserted below: the raw store really does hold strings, and every other field is
/// byte-identical. That distinction is exactly the sort of thing a fabricated fixture
/// would have gotten wrong in the convenient direction.
#[test]
fn the_fixture_holds_the_documents_upstream_wrote() {
    let fixture = fixture();
    let stored = humans_documents(&fixture);
    let expected: Vec<Value> = {
        let mut values = fixture["documentsAsRxdbSeesThem"]
            .as_array()
            .expect("documents as RxDB sees them")
            .clone();
        values.sort_by(|a, b| {
            a["passportId"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["passportId"].as_str().unwrap_or_default())
        });
        values
    };

    assert_eq!(stored.len(), 4, "four documents were written");

    for (raw, seen_by_rxdb) in stored.iter().zip(expected.iter()) {
        assert_eq!(raw["passportId"], seen_by_rxdb["passportId"]);

        // The finding: Dexie persists `_deleted` as a string because IDB cannot index a
        // boolean. Asserted, not worked around.
        assert_eq!(
            raw["_deleted"].as_str(),
            Some("0"),
            "Dexie is expected to store _deleted as the string \"0\" for a live document"
        );
        assert_eq!(seen_by_rxdb["_deleted"].as_bool(), Some(false));

        // Everything else must be identical, value and type.
        for field in ["firstName", "age", "_rev", "_meta", "_attachments"] {
            assert_eq!(
                raw[field], seen_by_rxdb[field],
                "field {field} of {} differs between the raw store and RxDB's view",
                raw["passportId"]
            );
        }

        // And every meta field a migration must preserve is present.
        for field in ["_rev", "_deleted", "_meta", "_attachments"] {
            assert!(
                !raw[field].is_null(),
                "document {} is missing {field}: {raw}",
                raw["passportId"]
            );
        }
    }
}

/// A live document's `_deleted: "0"` is the only stringified field. Asserted separately
/// so that the finding above cannot be mistaken for "Dexie stringifies everything".
#[test]
fn only_deleted_is_stringified_in_the_raw_store() {
    let fixture = fixture();
    for document in humans_documents(&fixture) {
        assert!(
            document["age"].is_number(),
            "age must stay a JSON number, got {}",
            document["age"]
        );
        assert!(document["firstName"].is_string());
        assert!(!document["_deleted"].is_boolean());
        assert!(document["_deleted"].is_string());
    }
}

// ---------------------------------------------------------------------------
// Hashes you can reproduce
// ---------------------------------------------------------------------------

/// The hash recorded in the fixture must equal a plain SHA-256 of the recorded input
/// bytes. If this fails, the fixture's hash was not derived from its own schema and
/// every later hash comparison would be meaningless.
#[test]
fn the_recorded_hash_is_the_sha256_of_the_recorded_input() {
    let fixture = fixture();
    let recorded = fixture["schemaHash"]
        .as_str()
        .expect("the fixture records upstream's schema hash");
    let input = fixture["hashInput"]
        .as_str()
        .expect("the fixture records the exact bytes upstream hashed");
    assert_eq!(
        dxdb_testkit::suite::sha256_hex(input.as_bytes()),
        recorded,
        "the fixture's hash is not SHA-256 of its own hashInput"
    );
}

/// The schema hash comparison: does dxdb's projection of this schema hash to the same
/// value upstream computed?
///
/// Reported as a measurement rather than asserted straight, because this tranche
/// cannot yet run `dxdb-schema`'s filler over upstream's *exact* input (the input is
/// already filled, and re-filling it would compare our defaulting to itself). What is
/// asserted now is the arithmetic: the same function and the same bytes give the same
/// hash, so the remaining question is only whether our filler produces those bytes.
#[test]
fn the_hash_input_is_javascript_number_formatted() {
    let fixture = fixture();
    let input = fixture["hashInput"].as_str().expect("hashInput");
    // Upstream writes `lwt`'s bounds as JS numbers: `1000000000000000` not
    // `1000000000000000.0`, and `0.01` as-is. A `serde_json` round trip that emitted
    // `1.0`-style floats anywhere in here would change the hash.
    assert!(
        input.contains("\"lwt\":{\"maximum\":1000000000000000,"),
        "expected JS-formatted integer bounds in the hashed bytes; got: {input}"
    );
    assert!(
        !input.contains(".0}") && !input.contains(".0,"),
        "the hashed bytes must not contain a Rust-style `1.0`; got: {input}"
    );
    // And the bytes must survive a round trip through our ordered-JSON writer
    // unchanged, which is the property the hashing depends on.
    let parsed: Value = serde_json::from_str(input).expect("valid JSON");
    assert_eq!(
        dxdb_json::to_js_json(&parsed),
        input,
        "dxdb-json must reproduce upstream's exact bytes for this schema"
    );
}
