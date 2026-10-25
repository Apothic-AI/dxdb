//! Oracle tests: index strings and index-string bounds.
//!
//! `testdata/vectors/index-strings.json` was produced by upstream RxDB 17.6.0 in
//! Node: for every index of every fixture schema, it records the index string of a
//! document, the index-string length, and — for a large set of probe values — the
//! lower and upper bound strings the storage layer would build.
//!
//! This is the family that would catch the most damaging class of bug, because a
//! wrong index string does not throw: it silently orders or filters documents
//! differently, and the resulting data still looks valid. Three specific things are
//! only pinned here:
//!
//! - **Clamping.** A probe far below a field's declared `minimum` must encode as the
//!   minimum, not as a negative number in a zero-padded field.
//! - **The `\x00` string bound.** A lower bound for a string field is NUL-padded, not
//!   space-padded, so a value containing `\t` is not excluded.
//! - **Sentinel handling.** `INDEX_MIN` and `INDEX_MAX` produce all-zeros and
//!   all-nines number bounds.
//!
//! The vector's `skipped` array records schemas upstream *rejects* (an index that
//! names a literal number, a numeric indexed field with no `minimum`/`maximum`, an
//! index naming a field that does not exist). Those are asserted to be rejected here
//! too, in [`rejected_schemas_are_rejected_here`] — a port that silently accepts them
//! would produce index strings upstream cannot produce.

use serde_json::Value;

use dxdb_query::{
    get_index_string_length, get_indexable_string, get_start_index_string_from_lower_bound,
    get_start_index_string_from_upper_bound, BoundValue,
};

fn load() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/vectors/index-strings.json"
    );
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("cannot read {path}: {error}. The oracle vectors are committed.")
    });
    serde_json::from_str(&text).expect("index-strings.json is valid JSON")
}

/// The schemas the vector refers to by name.
///
/// The vector stores a schema *name* per case rather than repeating the schema,
/// because the same fixture appears in hundreds of cases. The names come from
/// `src/plugins/test-utils/schemas.ts`; each is reconstructed here from the
/// `schema-hashes` family, which carries the input schema for every fixture under
/// the same name. That keeps one copy of each fixture in the repo instead of two.
fn schema_lookup() -> std::collections::HashMap<String, Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/vectors/schema-hashes.json"
    );
    let text = std::fs::read_to_string(path).expect("schema-hashes.json is committed");
    let vector: Value = serde_json::from_str(&text).expect("valid JSON");
    let mut out = std::collections::HashMap::new();
    for case in vector["cases"].as_array().expect("cases array") {
        let name = case["name"].as_str().expect("name").to_string();
        // The index-strings family names the plain fixtures, so a synthetic case
        // (`synthetic:...`) can never collide with one.
        out.entry(name).or_insert_with(|| case["input"].clone());
    }
    out
}

fn filled(name: &str, lookup: &std::collections::HashMap<String, Value>) -> Value {
    let schema = lookup
        .get(name)
        .unwrap_or_else(|| panic!("index-strings names a schema the hashes family lacks: {name}"));
    dxdb_schema::fill_with_default_settings(schema).unwrap_or_else(|error| {
        panic!("fixture {name} does not fill, but upstream produced index strings for it: {error}")
    })
}

fn index_fields(index: &Value) -> Vec<String> {
    index
        .as_array()
        .expect("index array")
        .iter()
        .map(|field| field.as_str().expect("field name").to_string())
        .collect()
}

/// `kind: "index-string"` — the encoding of one document under one index.
#[test]
fn index_strings_match_upstream_oracle() {
    let vector = load();
    let lookup = schema_lookup();
    let cases = vector["cases"].as_array().expect("cases array");
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in cases {
        if case["kind"].as_str() != Some("index-string") {
            continue;
        }
        let schema_name = case["schema"].as_str().expect("schema");
        let schema = filled(schema_name, &lookup);
        let index = index_fields(&case["index"]);
        let document = &case["document"];
        let expected = case["indexString"].as_str().expect("indexString");
        let why = case["why"].as_str().unwrap_or_default();
        let field = case["field"].as_str().unwrap_or_default();

        match get_indexable_string(&schema, &index, document) {
            Ok(actual) => {
                if actual != expected {
                    failures.push(format!(
                        "{schema_name} [{index:?}] {why}: encoded {actual:?} != upstream {expected:?} \
                         (field {field})"
                    ));
                } else {
                    checked += 1;
                }
            }
            Err(error) => failures.push(format!(
                "{schema_name} [{index:?}] {why}: encoder failed: {error}"
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} index-string cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        checked >= 400,
        "expected at least 400 index-string cases, checked {checked}"
    );
}

/// `kind: "index-string-length"` — the fixed width of an index.
#[test]
fn index_string_lengths_match_upstream_oracle() {
    let vector = load();
    let lookup = schema_lookup();
    let cases = vector["cases"].as_array().expect("cases array");
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in cases {
        if case["kind"].as_str() != Some("index-string-length") {
            continue;
        }
        let schema_name = case["schema"].as_str().expect("schema");
        let schema = filled(schema_name, &lookup);
        let index = index_fields(&case["index"]);
        let expected = case["length"].as_u64().expect("length") as usize;
        match get_index_string_length(&schema, &index) {
            Ok(actual) if actual == expected => checked += 1,
            Ok(actual) => failures.push(format!(
                "{schema_name} [{index:?}]: width {actual} != upstream {expected}"
            )),
            Err(error) => failures.push(format!(
                "{schema_name} [{index:?}]: width failed: {error}"
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
/// The probe value is the interesting part: `lower-null`, `lower-undefined`,
/// `lower-INDEX_MIN`, `lower-INDEX_MAX`, `lower-out-of-range-low`,
/// `lower-out-of-range-high`, `lower-typed` and `lower-empty` each exercise a
/// different branch of the bound builders.
#[test]
fn bounds_match_upstream_oracle() {
    let vector = load();
    let lookup = schema_lookup();
    let cases = vector["cases"].as_array().expect("cases array");
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in cases {
        if case["kind"].as_str() != Some("bounds") {
            continue;
        }
        let schema_name = case["schema"].as_str().expect("schema");
        let schema = filled(schema_name, &lookup);
        let index = index_fields(&case["index"]);
        let why = case["why"].as_str().unwrap_or_default();
        let field = case["field"].as_str().unwrap_or_default();

        // The probe value sits at the position of `field` in the index; every other
        // position is held at a *neutral* value, not left absent. That is how the
        // generator built the case (`xtask/vectors/families/index-strings.mjs:225`):
        // the neutral filler is the field's schema minimum for a number, `false` for
        // a boolean, and the empty string otherwise. Using `Missing` instead would
        // exercise a different branch (`\0` string padding, all-zero numbers) and
        // make every case disagree for the wrong reason.
        let field_position = index
            .iter()
            .position(|candidate| candidate == field)
            .unwrap_or_else(|| panic!("{schema_name} {why}: field {field} is not in {index:?}"));
        let mut bound_values: Vec<BoundValue> = Vec::with_capacity(index.len());
        for candidate in &index {
            // A non-string index entry cannot be neutral, and such a schema is
            // already covered by `rejected_schemas_are_rejected_here`.
            let Some(part) = dxdb_schema::schema_part_by_object_path(&schema, candidate) else {
                bound_values.push(BoundValue::Missing);
                continue;
            };
            bound_values.push(match part.get("type").and_then(Value::as_str) {
                Some("number") | Some("integer") => match part.get("minimum").and_then(Value::as_f64) {
                    Some(minimum) => BoundValue::Number(minimum),
                    None => BoundValue::Number(0.0),
                },
                Some("boolean") => BoundValue::Bool(false),
                _ => BoundValue::String(String::new()),
            });
        }
        bound_values[field_position] = probe_bound(case);

        let expected_lower = case["lowerBoundString"].as_str().expect("lowerBoundString");
        let expected_upper = case["upperBoundString"].as_str().expect("upperBoundString");

        match get_start_index_string_from_lower_bound(&schema, &index, &bound_values) {
            Ok(actual) if actual == expected_lower => {}
            Ok(actual) => failures.push(format!(
                "{schema_name} [{index:?}] {why}: lower {actual:?} != upstream {expected_lower:?}"
            )),
            Err(error) => {
                failures.push(format!("{schema_name} [{index:?}] {why}: lower bound failed: {error}"))
            }
        }
        match get_start_index_string_from_upper_bound(&schema, &index, &bound_values) {
            Ok(actual) if actual == expected_upper => checked += 1,
            Ok(actual) => failures.push(format!(
                "{schema_name} [{index:?}] {why}: upper {actual:?} != upstream {expected_upper:?}"
            )),
            Err(error) => {
                failures.push(format!("{schema_name} [{index:?}] {why}: upper bound failed: {error}"))
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} bound cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        checked >= 300,
        "expected at least 300 bound cases, checked {checked}"
    );
}

/// Maps one bounds case's probe value onto a [`BoundValue`], sentinels included.
///
/// Two ambiguities have to be resolved, and both are upstream's:
///
/// - `INDEX_MIN` is a number and `INDEX_MAX` is the string `"\u{FFFF}"`, so neither
///   is distinguishable from a real bound by inspection. `BoundValue::from_json`
///   resolves them by value, the way upstream's `bound === INDEX_MIN` does.
/// - The `lower-undefined` probe has *no* `bound` key at all, because JSON cannot
///   hold `undefined`. A missing key must become [`BoundValue::Missing`], not
///   `Null` — `Value::Index` on a missing key returns `Null` in serde, and confusing
///   the two changes the upper bound for a boolean field from `'0'` to `'1'`.
fn probe_bound(case: &Value) -> BoundValue {
    match case.get("bound") {
        None => BoundValue::Missing,
        Some(value) => BoundValue::from_json(Some(value)),
    }
}

/// `kind: "rejection"` — the schemas where upstream itself throws.
///
/// The family records four of these, each carrying the *class* of failure in
/// `why`/`upstreamBehaviour.class`:
///
/// - `upstream-rejects-non-string-index-entry` — the index array holds a literal number,
///   so upstream calls a string method on it and throws `TypeError` before any schema
///   lookup (`compoundIndexNoString`).
/// - `upstream-requires-numeric-index-bounds` — a numeric or object-typed field is indexed
///   without `minimum`/`maximum`. Those bounds size the fixed-width digit budget, so there
///   is no valid index string (`noStringIndex`, twice).
/// - `upstream-rejects-missing-indexed-field` — the index names a path the schema does not
///   declare, which upstream raises as error `CI1` (`notExistingIndex`).
///
/// A port that *accepts* any of these would produce index strings upstream cannot, so a
/// store written by dxdb would be unreadable by RxDB. Both the encoder and the width
/// query must reject.
///
/// The rejection must appear at the encoder, not at fill time: upstream fills and *hashes*
/// every one of these schemas, and the `schema-hashes` family contains a hash for each
/// (see `dxdb_schema`'s `fill_keeps_a_non_string_index_entry`).
///
/// `caseCount` counts these rejection cases, so the total of
/// produced + rejected must equal it.
#[test]
fn rejected_schemas_are_rejected_here() {
    let vector = load();
    let lookup = schema_lookup();
    let cases = vector["cases"].as_array().expect("cases array");

    let rejection_cases: Vec<&Value> = cases
        .iter()
        .filter(|case| case["kind"].as_str() == Some("rejection"))
        .collect();
    assert!(
        !rejection_cases.is_empty(),
        "the family must record the schemas upstream rejects; rejectionCaseCount is {}",
        vector["rejectionCaseCount"]
    );
    assert_eq!(
        rejection_cases.len() as u64,
        vector["rejectionCaseCount"].as_u64().unwrap_or_default(),
        "rejectionCaseCount must agree with the number of rejection cases"
    );
    // The generator also checks that every rejection it saw was expected.
    let unexpected = vector["unexpectedRejections"]
        .as_array()
        .expect("unexpectedRejections array");
    assert!(
        unexpected.is_empty(),
        "the generator recorded rejections it did not expect, which means a fixture changed \
         shape: {unexpected:?}"
    );

    let expected_count = rejection_cases.len();
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in &rejection_cases {
        let schema_name = case["schema"].as_str().expect("schema");
        let class = case["upstreamBehaviour"]["class"]
            .as_str()
            .or_else(|| case["why"].as_str())
            .unwrap_or_default();
        let index: Vec<Value> = case["index"].as_array().expect("index array").to_vec();

        let Some(schema_input) = lookup.get(schema_name) else {
            continue;
        };
        // Filling is permissive by design: upstream fills and hashes these too.
        let filled = match dxdb_schema::fill_with_default_settings(schema_input) {
            Ok(filled) => filled,
            Err(error) => {
                failures.push(format!(
                    "{schema_name} {class}: filling failed ({error}), but upstream fills and \
                     hashes this schema"
                ));
                continue;
            }
        };

        // An index entry that is not a field name has no field-name form at all, and
        // that absence *is* the rejection: there is no `Vec<String>` to hand the encoder.
        // Upstream reaches the same conclusion by calling `.replace` on the number.
        let field_names: Option<Vec<String>> = index
            .iter()
            .map(|entry| entry.as_str().map(str::to_owned))
            .collect();
        let Some(declared) = field_names else {
            let whole_schema = dxdb_schema::get_string_indexes(&filled);
            if whole_schema.is_ok() {
                failures.push(format!(
                    "{schema_name} {class}: an index containing a non-string entry must not \
                     yield field names, but get_string_indexes returned {whole_schema:?}"
                ));
            } else {
                checked += 1;
            }
            continue;
        };

        // The declared index may be pre-fill; find the filled form (with `_deleted`
        // prepended and the primary key appended) that contains those fields.
        let filled_indexes = dxdb_schema::get_string_indexes(&filled).unwrap_or_default();
        let matching = filled_indexes.iter().find(|candidate| {
            declared
                .iter()
                .all(|field| candidate.iter().any(|entry| entry == field))
        });
        let Some(candidate) = matching else {
            // No filled form exists, so there is nothing to encode: also a rejection.
            checked += 1;
            continue;
        };

        let document = case["document"].clone();
        let encoded = get_indexable_string(&filled, candidate, &document);
        let width = get_index_string_length(&filled, candidate);
        if encoded.is_err() && width.is_err() {
            checked += 1;
        } else if encoded.is_ok() {
            failures.push(format!(
                "{schema_name} {class}: index {candidate:?} must be rejected, but it encoded to \
                 {:?}",
                encoded.ok()
            ));
        } else {
            failures.push(format!(
                "{schema_name} {class}: index {candidate:?} has no valid encoding, but its width \
                 was reported as {:?}",
                width.ok()
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} rejection cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(checked, expected_count, "every rejection case must be asserted");
}
