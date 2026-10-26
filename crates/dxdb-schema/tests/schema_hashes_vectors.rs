//! Oracle tests: schema hashes, read through `dxdb-vectors`.
//!
//! `testdata/vectors/schema-hashes.json` was produced by running upstream RxDB 17.6.0 in
//! Node — `JSON.stringify(defaultedSchema)` hashed with SHA-256, which is what `RxSchema.hash`
//! does with `defaultHashSha256`. It is an external oracle, not a recording of this
//! implementation, so a mismatch is a real divergence and the vector is never adjusted to
//! match the code.
//!
//! The file is read through [`dxdb_vectors`], which owns path discovery, the case-count table
//! and the `deny_unknown_fields` structs.
//!
//! Each case carries three independent assertions and all three are checked:
//!
//! - `defaultedSchema`: the exact filled-and-normalized object, key order included. This
//!   localises a hash mismatch to injection, index building or key sorting instead of leaving
//!   "the hash is wrong" as the diagnosis.
//! - `hashInputLength`: the byte length of the serialised schema, which names a formatting
//!   difference precisely.
//! - `hash`: the digest itself.
//!
//! The family also carries `relations` (which edits must and must not move the hash) and
//! `hashStringCases` (SHA-256 of known byte strings), so a broken hash primitive is caught
//! separately from broken canonicalisation.

use serde_json::Value;

use dxdb_schema::{create_rx_schema, fill_with_default_settings, normalize_rx_json_schema, sha256_hex};
use dxdb_vectors::{schema_hashes, VectorFamily, Vectors};

fn load() -> schema_hashes::Vectors {
    let vectors = Vectors::discover();
    vectors
        .load(VectorFamily::SchemaHashes)
        .expect("schema-hashes.json loads through dxdb-vectors")
}

/// The oracle covers the fixtures, not a handful of synthetic cases.
#[test]
fn the_family_has_its_documented_case_count() {
    let family = load();
    assert_eq!(family.cases.len(), family.case_count);
    let expected = dxdb_vectors::EXPECTED_CASE_COUNTS
        .iter()
        .find(|(candidate, _)| *candidate == VectorFamily::SchemaHashes)
        .map(|(_, count)| *count)
        .expect("SchemaHashes is in EXPECTED_CASE_COUNTS");
    assert_eq!(family.cases.len(), expected);
    assert_eq!(
        family.algorithm, "sha256 hex of JSON.stringify(defaultedSchema)",
        "the algorithm string documents what is being asserted"
    );
    assert!(
        family.failed_schemas.is_empty(),
        "the generator recorded schemas it could not hash: {:?}",
        family.failed_schemas
    );

    // Every fixture the port must cover, plus at least the synthetic cases that isolate one
    // defaulting step each.
    for required in ["human", "bigHuman", "noIndexHuman", "empty", "point"] {
        assert!(
            family.cases.iter().any(|case| case.name == required),
            "fixture {required} missing from the oracle"
        );
    }
    assert!(
        family.cases.iter().filter(|case| !case.is_fixture()).count() >= 8,
        "the synthetic cases are what let a failing port bisect across defaulting steps"
    );
}

/// Every fixture and synthetic schema: fill, normalize, hash, compare.
#[test]
fn hashes_match_upstream_oracle() {
    let family = load();
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in &family.cases {
        let name = &case.name;

        // 1. The defaulted schema itself.
        let filled = match fill_with_default_settings(&case.input) {
            Ok(filled) => filled,
            Err(error) => {
                failures.push(format!("{name}: fill failed: {error}"));
                continue;
            }
        };
        let normalized = normalize_rx_json_schema(&filled);
        if normalized != case.defaulted_schema {
            failures.push(format!(
                "{name}: defaulted schema differs.\n  ours:     {}\n  upstream: {}",
                dxdb_json::to_js_json(&normalized),
                dxdb_json::to_js_json(&case.defaulted_schema)
            ));
            continue;
        }
        // Key order and formatting, which a value comparison above cannot see.
        let ours = dxdb_json::to_js_json(&normalized);
        let upstream = dxdb_json::to_js_json(&case.defaulted_schema);
        if ours != upstream {
            failures.push(format!(
                "{name}: serialised defaulted schema differs.\n  ours:     {ours}\n  upstream: \
                 {upstream}"
            ));
            continue;
        }

        // 2. The canonical byte length.
        let bytes = dxdb_json::canonical_bytes(&normalized);
        if bytes.len() != case.hash_input_length {
            failures.push(format!(
                "{name}: hash input is {} bytes, upstream's is {}",
                bytes.len(),
                case.hash_input_length
            ));
            continue;
        }

        // 3. The digest.
        let actual_hash = sha256_hex(&bytes);
        if actual_hash != case.hash {
            failures.push(format!(
                "{name}: hash {actual_hash} != upstream {}",
                case.hash
            ));
            continue;
        }

        // And the public entry point must agree. An unencodable schema still hashes
        // upstream — hashing only serialises the object — while `Schema` additionally
        // requires the indexes to be usable, so a `Schema` that exists must report the same
        // hash.
        if let Ok(schema) = create_rx_schema(&case.input) {
            if schema.hash() != case.hash {
                failures.push(format!(
                    "{name}: Schema::hash {} != upstream {}",
                    schema.hash(),
                    case.hash
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
        family.cases.len(),
        failures.join("\n")
    );
    assert_eq!(checked, family.cases.len());
}

/// `relations`: which edits must and must not move the hash.
///
/// This is the part of schema identity that matters in practice — a client that re-encodes a
/// schema with its keys in a different order, or spells out the index the defaults would have
/// produced, must land on the same hash or it cannot talk to the same store. The negative
/// pairs matter just as much: without them, a port that hashed everything identically would
/// pass the positive ones.
#[test]
fn hash_relations_hold() {
    let family = load();
    assert!(!family.relations.is_empty(), "relations must be present");
    let mut checked = 0;

    for relation in &family.relations {
        // The relations now name real cases, so both endpoints and both hashes are
        // resolvable from the file itself rather than from generator internals.
        let (a, b) = relation.resolve(&family).unwrap_or_else(|| {
            panic!(
                "{}: relation names a case that is not in the family (a={:?}, b={:?})",
                relation.why, relation.a, relation.b
            )
        });

        // The recorded per-endpoint hashes must match what the cases say.
        assert_eq!(a.hash, relation.hash_a, "{}: hashA", relation.why);
        assert_eq!(b.hash, relation.hash_b, "{}: hashB", relation.why);

        // And our implementation must reproduce the hashes it is claiming a relation about.
        let hash_of = |input: &Value| -> String {
            create_rx_schema(input)
                .unwrap_or_else(|error| panic!("{}: schema does not build: {error}", relation.why))
                .hash()
                .to_string()
        };
        let ours_a = hash_of(&a.input);
        let ours_b = hash_of(&b.input);
        assert_eq!(ours_a, a.hash, "{}: our hash of {:?}", relation.why, a.name);
        assert_eq!(ours_b, b.hash, "{}: our hash of {:?}", relation.why, b.name);

        if relation.equal {
            assert_eq!(
                ours_a, ours_b,
                "{}: {:?} and {:?} must hash the same",
                relation.why, a.name, b.name
            );
        } else {
            assert_ne!(
                ours_a, ours_b,
                "{}: {:?} and {:?} must hash differently, or two distinct schemas would share \
                 one storage",
                relation.why, a.name, b.name
            );
        }
        checked += 1;
    }
    assert_eq!(checked, family.relations.len());
}

/// `hashStringCases`: the hash primitive itself, on known inputs.
#[test]
fn sha256_matches_known_vectors() {
    let family = load();
    assert!(!family.hash_string_cases.is_empty());
    for case in &family.hash_string_cases {
        assert_eq!(
            sha256_hex(case.input.as_bytes()),
            case.sha256,
            "SHA-256 of {:?} ({} bytes)",
            case.input,
            case.input.len()
        );
    }
}
