//! Oracle tests: index strings and index-string bounds, read through `dxdb-vectors`.
//!
//! `testdata/vectors/index-strings.json` was produced by upstream RxDB 17.6.0 in Node: for
//! every index of every fixture schema it records the index string of a document, the
//! index-string length, and — for a large set of probe values — the lower and upper bound
//! strings a storage would build.
//!
//! The file is read through [`dxdb_vectors`], the typed reader. `IndexEntry` distinguishes a
//! field path from a non-string entry, which is exactly the distinction upstream crashes on,
//! so the reader models the contract instead of leaving it to `serde_json::Value` indexing.
//!
//! This is the family that catches the most damaging class of bug, because a wrong index
//! string does not throw: it silently orders or filters documents differently and the data
//! still looks valid. Three things are only pinned here:
//!
//! - **Clamping.** A probe far below a field's declared `minimum` must encode as the
//!   minimum, not as a negative number in a zero-padded field.
//! - **The `\x00` string bound.** A lower bound for a string field is NUL-padded, not
//!   space-padded, so a value containing `\t` is not excluded.
//! - **Sentinel handling.** `INDEX_MIN` and `INDEX_MAX` produce all-zeros and all-nines.
//!
//! The family also records the indexes upstream *refuses*: a literal number in an index, a
//! numeric indexed field with no `minimum`/`maximum`, and an index naming a field that does
//! not exist. Those must be refused here too — a port that accepted them would produce index
//! strings upstream cannot.

use serde_json::Value;

use dxdb_query::{
    get_index_string_length, get_indexable_string, get_start_index_string_from_lower_bound,
    get_start_index_string_from_upper_bound, BoundValue,
};
use dxdb_vectors::{index_strings, schema_hashes, VectorFamily, Vectors};

/// Loads the family through the typed reader.
fn load() -> index_strings::Vectors {
    let vectors = Vectors::discover();
    vectors
        .load(VectorFamily::IndexStrings)
        .expect("index-strings.json loads through dxdb-vectors")
}

/// The fixture schemas, keyed by name, from the `schema-hashes` family.
///
/// The index-strings family names a schema per case rather than repeating it, because the
/// same fixture appears in hundreds of cases. The hashes family carries each input schema
/// once under the same name, so there is one copy in the repo rather than two.
fn schema_lookup() -> std::collections::HashMap<String, Value> {
    let vectors = Vectors::discover();
    let hashes: schema_hashes::Vectors = vectors
        .load(VectorFamily::SchemaHashes)
        .expect("schema-hashes.json loads through dxdb-vectors");
    let mut lookup = std::collections::HashMap::new();
    for case in &hashes.cases {
        lookup
            .entry(case.name.clone())
            .or_insert_with(|| case.input.clone());
    }
    lookup
}

fn filled(name: &str, lookup: &std::collections::HashMap<String, Value>) -> Value {
    let schema = lookup
        .get(name)
        .unwrap_or_else(|| panic!("index-strings names a schema the hashes family lacks: {name}"));
    dxdb_schema::fill_with_default_settings(schema).unwrap_or_else(|error| {
        panic!("fixture {name} does not fill, but upstream produced index strings for it: {error}")
    })
}

/// The index as field names, or `None` when an entry is not a field path.
fn index_fields(index: &[index_strings::IndexEntry]) -> Option<Vec<String>> {
    index
        .iter()
        .map(|entry| entry.as_field().map(str::to_owned))
        .collect()
}

/// The family is the size the vectors crate expects.
#[test]
fn the_family_has_its_documented_case_count() {
    let family = load();
    assert_eq!(family.cases.len(), family.case_count);
    let expected = dxdb_vectors::EXPECTED_CASE_COUNTS
        .iter()
        .find(|(candidate, _)| *candidate == VectorFamily::IndexStrings)
        .map(|(_, count)| *count)
        .expect("IndexStrings is in EXPECTED_CASE_COUNTS");
    assert_eq!(family.cases.len(), expected);
    // The three produced kinds plus the refusals must account for every case.
    let mut produced = 0;
    let mut refusals = 0;
    for case in &family.cases {
        match case {
            index_strings::Case::Rejection(_) => refusals += 1,
            _ => produced += 1,
        }
    }
    assert_eq!(produced, family.produced_case_count);
    assert_eq!(refusals, family.rejection_case_count);
}

/// `kind: "index-string"` — the encoding of one document under one index.
#[test]
fn index_strings_match_upstream_oracle() {
    let family = load();
    let lookup = schema_lookup();
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in &family.cases {
        let index_strings::Case::IndexString(case) = case else {
            continue;
        };
        let schema = filled(&case.schema, &lookup);
        // A non-string entry has no field-name form; those are the refusal cases, asserted
        // separately.
        let Some(index) = index_fields(&case.index) else {
            continue;
        };
        match get_indexable_string(&schema, &index, &case.document) {
            Ok(actual) if actual == case.index_string => checked += 1,
            Ok(actual) => failures.push(format!(
                "{} [{index:?}] {}: encoded {actual:?} != upstream {:?} (field {:?})",
                case.schema,
                case.why,
                case.index_string,
                case.field.as_deref().unwrap_or("")
            )),
            Err(error) => failures.push(format!(
                "{} [{index:?}] {}: encoder failed: {error}",
                case.schema, case.why
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} index-string cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked >= 400, "expected at least 400 cases, checked {checked}");
}

/// `kind: "index-string-length"` — the fixed width of an index.
#[test]
fn index_string_lengths_match_upstream_oracle() {
    let family = load();
    let lookup = schema_lookup();
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in &family.cases {
        let index_strings::Case::IndexStringLength(case) = case else {
            continue;
        };
        let schema = filled(&case.schema, &lookup);
        let Some(index) = index_fields(&case.index) else {
            continue;
        };
        match get_index_string_length(&schema, &index) {
            Ok(actual) if actual == case.length => checked += 1,
            Ok(actual) => failures.push(format!(
                "{} [{index:?}]: width {actual} != upstream {}",
                case.schema, case.length
            )),
            Err(error) => failures.push(format!(
                "{} [{index:?}]: width failed: {error}",
                case.schema
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} index-string-length cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked >= 80, "expected at least 80 length cases, got {checked}");
}

/// `kind: "bounds"` — the lower and upper bound strings for one probe value.
///
/// Each probe exercises a different branch: `lower-null`, `lower-undefined`, `lower-INDEX_MIN`,
/// `lower-INDEX_MAX`, `lower-out-of-range-low`, `lower-out-of-range-high`, `lower-typed` and
/// `lower-empty`.
#[test]
fn bounds_match_upstream_oracle() {
    let family = load();
    let lookup = schema_lookup();
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in &family.cases {
        let index_strings::Case::Bounds(case) = case else {
            continue;
        };
        let schema = filled(&case.schema, &lookup);
        let Some(index) = index_fields(&case.index) else {
            continue;
        };

        // The probe value sits at its field's position. Every other position is held at a
        // *neutral* value — the field's schema minimum for a number, `false` for a boolean,
        // the empty string otherwise — which is how the generator built the case. Using an
        // absent bound instead would exercise a different branch and make every case disagree
        // for the wrong reason.
        let field_position = index
            .iter()
            .position(|candidate| *candidate == case.field)
            .unwrap_or_else(|| {
                panic!(
                    "{} {}: field {:?} is not in {index:?}",
                    case.schema, case.why, case.field
                )
            });
        let mut bound_values: Vec<BoundValue> = Vec::with_capacity(index.len());
        for candidate in &index {
            let part = dxdb_schema::schema_part_by_object_path(&schema, candidate);
            bound_values.push(match part.and_then(|part| part.get("type")).and_then(Value::as_str) {
                Some("number") | Some("integer") => {
                    let minimum = part
                        .and_then(|part| part.get("minimum"))
                        .and_then(Value::as_f64);
                    BoundValue::Number(minimum.unwrap_or(0.0))
                }
                Some("boolean") => BoundValue::Bool(false),
                _ => BoundValue::String(String::new()),
            });
        }
        bound_values[field_position] = probe_bound(case);

        match get_start_index_string_from_lower_bound(&schema, &index, &bound_values) {
            Ok(actual) if actual == case.lower_bound_string => {}
            Ok(actual) => failures.push(format!(
                "{} [{index:?}] {}: lower {actual:?} != upstream {:?}",
                case.schema, case.why, case.lower_bound_string
            )),
            Err(error) => failures.push(format!(
                "{} [{index:?}] {}: lower bound failed: {error}",
                case.schema, case.why
            )),
        }
        match get_start_index_string_from_upper_bound(&schema, &index, &bound_values) {
            Ok(actual) if actual == case.upper_bound_string => checked += 1,
            Ok(actual) => failures.push(format!(
                "{} [{index:?}] {}: upper {actual:?} != upstream {:?}",
                case.schema, case.why, case.upper_bound_string
            )),
            Err(error) => failures.push(format!(
                "{} [{index:?}] {}: upper bound failed: {error}",
                case.schema, case.why
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} bound cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked >= 300, "expected at least 300 bound cases, checked {checked}");
    // The clamping branch must actually be exercised, or the most dangerous case is untested.
    assert!(
        family
            .cases
            .iter()
            .any(|case| matches!(case, index_strings::Case::Bounds(b) if b.why.contains("out-of-range"))),
        "the corpus must still probe the clamping boundary"
    );
}

/// Maps a bounds case's probe value onto a [`BoundValue`], sentinels included.
///
/// Two ambiguities are upstream's, and one is a limitation of the typed reader:
///
/// 1. `INDEX_MIN` is a number and `INDEX_MAX` the string `"\u{FFFF}"`, so neither is
///    distinguishable from a real bound by inspection. [`BoundValue::from_json`] resolves
///    them by value, the way upstream's `bound === INDEX_MIN` does.
/// 2. **`null` versus absent.** `lower-null` records `"bound": null` and `lower-undefined`
///    records *no* key at all, because JSON cannot hold `undefined`. But
///    `index_strings::BoundValue` deserialises into an `Option`, and serde maps both an
///    explicit `null` and a missing field to `None` — so the reader cannot tell the two
///    apart. They take different branches for a boolean field (`null` gives `'1'`, absent
///    gives `'0'`), so the case's own `why` label is what distinguishes them here.
///
///    This is a real gap in the typed reader rather than something to work around
///    silently: the family's `why` is stable and documented (`lower-null`,
///    `lower-undefined`), so using it is correct, but a reader with a `bound_present:
///    bool` or a tri-state `Bound<T>` would be the better fix. Reported rather than
///    papered over.
fn probe_bound(case: &index_strings::BoundsCase) -> BoundValue {
    let absent = case.why == "lower-undefined" || case.why == "lower-undefined-empty";
    match (&case.bound, absent) {
        (_, true) => BoundValue::Missing,
        (None, false) => BoundValue::Null,
        (Some(value), false) => BoundValue::from_json(Some(value)),
    }
}

/// The indexes upstream refuses must be refused here.
///
/// A port that accepted one would produce index strings upstream cannot, so a store written
/// by dxdb would be unreadable by RxDB. The rejection belongs at the encoder, not at fill
/// time: upstream fills and *hashes* all of these schemas, and the `schema-hashes` family
/// contains a hash for each.
#[test]
fn refused_indexes_are_refused_here() {
    let family = load();
    let lookup = schema_lookup();
    let mut checked = 0;
    let mut failures = Vec::new();

    assert!(
        family.unexpected_rejections.is_empty(),
        "the generator recorded an unexpected refusal: {:?}",
        family.unexpected_rejections
    );

    for case in &family.cases {
        let index_strings::Case::Rejection(case) = case else {
            continue;
        };
        let Some(schema_input) = lookup.get(&case.schema) else {
            continue;
        };
        let filled = match dxdb_schema::fill_with_default_settings(schema_input) {
            Ok(filled) => filled,
            Err(error) => {
                failures.push(format!(
                    "{}: filling failed ({error}), but upstream fills and hashes this schema",
                    case.schema
                ));
                continue;
            }
        };

        // An index containing a non-string entry has no field-name form at all, and that
        // absence *is* the refusal.
        let Some(declared) = index_fields(&case.index) else {
            match dxdb_schema::get_string_indexes(&filled) {
                Err(_) => checked += 1,
                Ok(indexes) => failures.push(format!(
                    "{}: an index containing a non-string entry must not yield field names, \
                     but get_string_indexes returned {indexes:?}",
                    case.schema
                )),
            }
            continue;
        };

        // Otherwise find the filled form containing the declared fields and require the
        // encoder to refuse it.
        let filled_indexes = dxdb_schema::get_string_indexes(&filled).unwrap_or_default();
        let candidate = filled_indexes.iter().find(|candidate| {
            declared
                .iter()
                .all(|field| candidate.iter().any(|entry| entry == field))
        });
        let Some(candidate) = candidate else {
            // No filled form exists, so there is nothing to encode: also a refusal.
            checked += 1;
            continue;
        };
        let encoded = get_indexable_string(&filled, candidate, &serde_json::json!({}));
        let width = get_index_string_length(&filled, candidate);
        if encoded.is_err() && width.is_err() {
            checked += 1;
        } else {
            failures.push(format!(
                "{} {candidate:?}: must be refused, but encoded={:?} width={:?}",
                case.schema,
                encoded.ok(),
                width.ok()
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} refusal cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(
        checked, family.rejection_case_count,
        "every refusal case must be asserted"
    );
    assert_eq!(checked, 4, "the family records four refusals");
}
