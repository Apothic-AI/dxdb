//! Oracle tests: schema hashes.
//!
//! `testdata/vectors/schema-hashes.json` was produced by running upstream RxDB
//! 17.6.0 (commit `3b763ee17`) in Node — `JSON.stringify(defaultedSchema)` hashed
//! with SHA-256, which is what `RxSchema.hash` does with
//! `defaultHashSha256`. It is an external oracle, not a recording of this
//! implementation, so a mismatch here is a real divergence and the vector is never
//! adjusted to match the code.
//!
//! The vector carries three independent assertions per case, and all three are
//! checked:
//!
//! - `defaultedSchema`: the exact filled-and-normalized object, key order included.
//!   This localises a hash mismatch to injection, index building or sorting instead
//!   of leaving "the hash is wrong" as the diagnosis.
//! - `hashInputLength`: the byte length of `JSON.stringify(defaultedSchema)`, which
//!   catches a formatting difference that happens not to change the hash
//!   (impossible for SHA-256, but it names the failure precisely).
//! - `hash`: the digest itself.
//!
//! The vector also carries `relations` (which schema changes must and must not move
//! the hash) and `hashStringCases` (SHA-256 of known byte strings), so a broken hash
//! primitive is caught separately from broken canonicalisation.

use serde_json::Value;

use dxdb_schema::{create_rx_schema, fill_with_default_settings, normalize_rx_json_schema, sha256_hex};

fn load() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/vectors/schema-hashes.json"
    );
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("cannot read {path}: {error}. The oracle vectors are committed; do not generate them here.")
    });
    serde_json::from_str(&text).expect("schema-hashes.json is valid JSON")
}

/// The oracle must cover the fixtures, not a handful of synthetic cases.
#[test]
fn vector_file_covers_the_fixture_schemas() {
    let vector = load();
    let cases = vector["cases"].as_array().expect("cases array");
    assert!(
        cases.len() >= 50,
        "schema-hash oracle lost cases: {}",
        cases.len()
    );
    assert_eq!(
        vector["algorithm"].as_str(),
        Some("sha256 hex of JSON.stringify(defaultedSchema)"),
        "the algorithm string documents what is being asserted"
    );
    // Every fixture schema in `src/plugins/test-utils/schemas.ts` must appear, not
    // just the ones that happened to hash cleanly.
    let names: Vec<&str> = cases
        .iter()
        .filter_map(|case| case["name"].as_str())
        .collect();
    for required in ["human", "bigHuman", "noIndexHuman", "empty", "point"] {
        assert!(
            names.contains(&required),
            "fixture {required} missing from the oracle; got {names:?}"
        );
    }
}

/// Every fixture and synthetic schema: fill, normalize, hash, compare.
#[test]
fn hashes_match_upstream_oracle() {
    let vector = load();
    let cases = vector["cases"].as_array().expect("cases array");
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in cases {
        let name = case["name"].as_str().expect("name");
        let input = &case["input"];
        let expected_schema = &case["defaultedSchema"];
        let expected_hash = case["hash"].as_str().expect("hash");
        let expected_length = case["hashInputLength"].as_u64().expect("length");

        // 1. The defaulted schema itself.
        let filled = match fill_with_default_settings(input) {
            Ok(filled) => filled,
            Err(error) => {
                failures.push(format!("{name}: fill failed: {error}"));
                continue;
            }
        };
        let normalized = normalize_rx_json_schema(&filled);
        if &normalized != expected_schema {
            failures.push(format!(
                "{name}: defaulted schema differs.\n  ours:     {}\n  upstream: {}",
                dxdb_json::to_js_json(&normalized),
                dxdb_json::to_js_json(expected_schema),
            ));
            continue;
        }

        // 2. The canonical byte length.
        let bytes = dxdb_json::canonical_bytes(&normalized);
        if bytes.len() as u64 != expected_length {
            failures.push(format!(
                "{name}: hash input is {} bytes, upstream's is {expected_length}",
                bytes.len()
            ));
            continue;
        }

        // 3. The digest.
        let actual_hash = sha256_hex(&bytes);
        if actual_hash != expected_hash {
            failures.push(format!(
                "{name}: hash {actual_hash} != upstream {expected_hash}"
            ));
            continue;
        }

        // An unencodable schema still hashes upstream, because hashing only
        // serialises the object — `compoundIndexNoString` is the case, and the family
        // carries its hash. `Schema::new` is the *encodable*-schema type and rejects
        // it, which is asserted here rather than left implicit: the two behaviours are
        // different on purpose and both are observable.
        if let Ok(schema) = create_rx_schema(input) {
            if schema.hash() != expected_hash {
                failures.push(format!(
                    "{name}: Schema::hash {} != upstream {expected_hash}",
                    schema.hash()
                ));
                continue;
            }
        } else {
            // Only an index entry that is not a field name may be rejected after a
            // successful fill; any other rejection is a bug.
            let filled_indexes = dxdb_schema::get_string_indexes(&normalized);
            if filled_indexes.is_ok() {
                failures.push(format!(
                    "{name}: Schema::new rejected a schema whose indexes are all field \
                     names, which upstream accepts"
                ));
                continue;
            }
        }
        checked += 1;
    }

    assert!(
        failures.is_empty(),
        "{} of {} schema-hash cases diverged:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    assert_eq!(checked, cases.len());
}

/// `relations`: which edits must and must not move the hash.
///
/// This is the part of schema identity that matters in practice — a client that
/// re-encodes a schema with its keys in a different order, or spells out the index the
/// defaults would have produced, must land on the same hash or it cannot talk to the
/// same store. The counterpart matters just as much: a real semantic change must move
/// the hash, or two different schemas would share one storage.
///
/// The relation schemas are literals inside the generator
/// (`xtask/vectors/families/schema-hashes.mjs:206`), not separate cases, so they are
/// reproduced here rather than looked up. Each one is a one-line edit of the `plain`
/// base, which is what makes the relation meaningful.
#[test]
fn hash_relations_hold() {
    let vector = load();
    let relations = vector["relations"].as_array().expect("relations array");
    assert!(!relations.is_empty(), "relations must be present");

    let plain = base_schema();
    let variants = |name: &str| -> Value {
        match name {
            "plain" => plain.clone(),
            // Same schema, keys in a different order.
            "reordered" => serde_json::json!({
                "required": ["id"],
                "primaryKey": "id",
                "properties": { "id": { "maxLength": 100, "type": "string" } },
                "type": "object",
                "version": 0
            }),
            // The index the defaults would add, written out.
            "explicitIndex" => {
                let mut schema = plain.as_object().cloned().expect("object");
                schema.insert(
                    "indexes".into(),
                    serde_json::json!([["_deleted", "id"]]),
                );
                Value::Object(schema)
            }
            // A duplicate `required` entry, which de-duplication must absorb.
            "redundantRequired" => {
                let mut schema = plain.as_object().cloned().expect("object");
                schema.insert("required".into(), serde_json::json!(["id", "id"]));
                Value::Object(schema)
            }
            // A real semantic change: a shorter primary key.
            "differentMaxLength" => serde_json::json!({
                "version": 0,
                "type": "object",
                "primaryKey": "id",
                "properties": { "id": { "type": "string", "maxLength": 99 } },
                "required": ["id"]
            }),
            // Another real semantic change: a version bump.
            "versionBump" => {
                let mut schema = plain.as_object().cloned().expect("object");
                schema.insert("version".into(), serde_json::json!(1));
                Value::Object(schema)
            }
            other => panic!("relation names a variant this test does not model: {other}"),
        }
    };

    for relation in relations {
        let why = relation["why"].as_str().expect("why");
        let a_name = relation["a"].as_str().expect("a");
        let b_name = relation["b"].as_str().expect("b");
        let equal = relation["equal"].as_bool().expect("equal");

        let hash_of = |input: &Value| -> String {
            let schema = create_rx_schema(input)
                .unwrap_or_else(|error| panic!("{why}: schema does not build: {error}"));
            schema.hash().to_string()
        };
        let hash_a = hash_of(&variants(a_name));
        let hash_b = hash_of(&variants(b_name));
        let recorded = relation["hash"].as_str().expect("hash");

        if equal {
            assert_eq!(
                hash_a, hash_b,
                "{why}: {a_name} and {b_name} must hash the same"
            );
            assert_eq!(hash_a, recorded, "{why}: recorded shared hash");
        } else {
            assert_ne!(
                hash_a, hash_b,
                "{why}: {a_name} and {b_name} must hash differently, or two distinct \
                 schemas would share one storage"
            );
            // The recorded hash is the hash of the changed side, which is how the
            // generator wrote it.
            assert_eq!(
                hash_b, recorded,
                "{why}: the changed schema's hash ({b_name}) must be the recorded one"
            );
        }
    }
}

/// The `plain` base schema the relations are edits of
/// (`xtask/vectors/families/schema-hashes.mjs:206`).
fn base_schema() -> Value {
    serde_json::json!({
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": { "id": { "type": "string", "maxLength": 100 } },
        "required": ["id"]
    })
}

/// `hashStringCases`: the hash primitive itself, on known inputs.
#[test]
fn sha256_matches_known_vectors() {
    let vector = load();
    let cases = vector["hashStringCases"]
        .as_array()
        .expect("hashStringCases array");
    assert!(!cases.is_empty());
    for case in cases {
        let input = case["input"].as_str().expect("input");
        let expected = case["sha256"].as_str().expect("sha256");
        assert_eq!(
            sha256_hex(input.as_bytes()),
            expected,
            "SHA-256 of {input:?} ({} bytes)",
            input.len()
        );
    }
}

/// A schema is not a hash: the *input* is pinned too, so a change to injection
/// order is caught even where the digest happens to survive.
#[test]
fn defaulted_schema_matches_upstream_json() {
    let vector = load();
    let cases = vector["cases"].as_array().expect("cases array");
    let case = cases
        .iter()
        .find(|case| case["name"].as_str() == Some("human"))
        .expect("human case");
    let filled = fill_with_default_settings(&case["input"]).expect("fills");
    let normalized = normalize_rx_json_schema(&filled);
    // Compare the *serialised* form as well as the value: serde_json with
    // `preserve_order` compares object equality order-insensitively in `==`? It
    // does not — `Map` equality is order-insensitive — so the string comparison is
    // the one that pins key order.
    let ours = dxdb_json::to_js_json(&normalized);
    let upstream = dxdb_json::to_js_json(&case["defaultedSchema"]);
    assert_eq!(
        ours, upstream,
        "key order and formatting must match upstream's JSON.stringify exactly"
    );
}
