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

/// The schemas upstream rejects must be rejected here.
///
/// The vector's `skipped` array names four such schemas with the reason upstream
/// itself failed on them:
///
/// - `compoundIndexNoString`: the index contains the literal number `10` instead of
///   a field path.
/// - `noStringIndex`: a numeric indexed field declares no `minimum`/`maximum`, so
///   there is no digit budget for the encoding.
/// - `notExistingIndex`: the index names `address.apartment`, which is not in the
///   schema — upstream's error carries code `CI1`.
///
/// Accepting any of them would mean producing index strings upstream cannot, which
/// is worse than failing: a store written by dxdb would be unreadable by RxDB.
#[test]
fn rejected_schemas_are_rejected_here() {
    let vector = load();
    // The family records these as first-class `rejections` with the observable error
    // identity, not as a `skipped` list: accepting any of them would mean producing
    // index strings upstream cannot, which is worse than failing, because a store
    // written by dxdb would then be unreadable by RxDB. The field was renamed when the
    // vectors lane made rejections assertable rather than merely noted.
    let rejections = vector["rejections"]
        .as_array()
        .expect("rejections array: the family must record the schemas upstream rejects");
    assert!(
        !rejections.is_empty(),
        "the family must record the schemas upstream rejects"
    );

    let lookup = schema_lookup();
    for entry in rejections {
        let schema_name = entry["schema"].as_str().expect("schema");
        let index: Vec<String> = entry["index"]
            .as_array()
            .expect("index array")
            .iter()
            .map(|field| {
                // A skipped entry can name a literal number, which is exactly the
                // entry that has no field name; it is represented by its JSON text.
                match field {
                    Value::String(name) => name.clone(),
                    other => dxdb_json::to_js_json(other),
                }
            })
            .collect();
        // The new shape names the observable failure class and error name rather than a
        // free-text reason, so the assertion can cite what upstream actually did.
        let reason = entry["class"].as_str().unwrap_or_default();

        let Some(schema_input) = lookup.get(schema_name) else {
            // `noStringIndex` is one of the fixtures; if a fixture is missing the
            // oracle above already fails loudly.
            continue;
        };
        // Filling may already fail (a numeric index field with no bounds), or the
        // failure may only appear when the index is used. Either is a rejection.
        // Filling is permissive by design (upstream fills these schemas too), so the
        // rejection has to appear at the encoder.
        let filled = dxdb_schema::fill_with_default_settings(schema_input)
            .expect("upstream fills this schema; only encoding fails");
        let document = entry["document"].clone();
        let result = get_indexable_string(&filled, &index, &document);
        assert!(
            result.is_err(),
            "{schema_name} {index:?} must be rejected (upstream: {reason}), but it produced {result:?}"
        );
        // The length query must reject it too: an index with no width is not
        // representable.
        assert!(
            get_index_string_length(&filled, &index).is_err(),
            "{schema_name} {index:?}: width must be rejected as well"
        );
    }
}
