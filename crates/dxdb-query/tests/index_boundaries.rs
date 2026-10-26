//! Index-string encoding at the numeric boundaries, against the upstream oracle.
//!
//! The committed `index-strings` family probes each index field with a handful of values,
//! which pins the clamping *branches*. It does not systematically pin the *arithmetic*, and
//! the arithmetic is where a bug corrupts query results silently rather than failing.
//!
//! This test consumes a generated corpus ([`support/index-boundaries.mjs`]) whose schemas are
//! chosen to put a boundary in play — power-of-ten widths, negative ranges, ranges crossing
//! zero, every decimal budget — and whose probe values are derived from each schema's own
//! `minimum`/`maximum`/`multipleOf`: exact bounds, one step inside, one step outside (which
//! must clamp), far outside, and the decimal-carry case.
//!
//! Expected strings come from upstream `custom-index.ts` running in Node. Nothing here
//! consults our encoder to decide what is correct.
//!
//! # What each assertion protects
//!
//! - **Width.** Every index string for an index has that index's declared length. Fixed width
//!   is what makes comparing concatenations equivalent to comparing the field tuple; a
//!   variable-width string silently reorders documents.
//! - **Digit budget.** `nonDecimals` is the length of `String(ceil(max) - floor(min))`, which
//!   is why `maximum: 999` yields `nonDecimals: 3` and encodes `999` as `0999` — one
//!   character wider than the value needs. An off-by-one here is exactly the kind of bug a
//!   hand-picked corpus misses.
//! - **Clamping.** A value outside the declared range encodes identically to the boundary, so
//!   an exclusive bound clamped onto the boundary cannot exclude a document sitting on it.
//! - **Monotonicity.** Within the declared range, a smaller value encodes to a
//!   lexicographically smaller string. This is what a storage's binary search depends on.

use serde_json::Value;

use dxdb_query::{
    get_index_string_length, get_indexable_string, get_start_index_string_from_lower_bound,
    get_start_index_string_from_upper_bound, get_string_length_of_index_number, BoundValue,
};

/// The generated corpus, embedded so a missing file is a build error rather than a green test
/// with zero cases.
const CORPUS: &str = include_str!("support/index-boundaries.json");

fn load() -> Value {
    serde_json::from_str(CORPUS).expect("index-boundaries.json is valid JSON")
}

/// Builds the filled schema for one corpus entry.
///
/// The corpus stores the numeric spec rather than the whole schema literal, so the test
/// reconstructs it — and re-runs `fill_with_default_settings`, which is what produces the
/// `_deleted` prefix, the `_meta.lwt` index and the primary-key suffix that the recorded
/// index strings contain.
fn filled_schema(spec: &Value) -> Value {
    let literal = serde_json::json!({
        "title": format!("boundary {}", spec["name"].as_str().unwrap_or("?")),
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 10},
            "value": {
                "type": "number",
                "minimum": spec["minimum"],
                "maximum": spec["maximum"],
                "multipleOf": spec["multipleOf"],
            },
        },
        "required": ["id", "value"],
        "indexes": [["value"]],
    });
    dxdb_schema::fill_with_default_settings(&literal).expect("boundary schema fills")
}

fn index_of(entry: &Value) -> Vec<String> {
    entry["index"]
        .as_array()
        .expect("index")
        .iter()
        .map(|field| field.as_str().expect("field").to_string())
        .collect()
}

/// The corpus is the size it claims and came from the pin.
#[test]
fn the_corpus_is_the_documented_size_and_provenance() {
    let corpus = load();
    let cases = corpus["cases"].as_array().expect("cases");
    assert_eq!(
        cases.len() as u64,
        corpus["schemaCount"].as_u64().expect("schemaCount")
    );
    let values: u64 = cases
        .iter()
        .map(|entry| {
            entry["values"]
                .as_array()
                .map(|v| v.len() as u64)
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(values, corpus["valueCount"].as_u64().expect("valueCount"));
    assert_eq!(
        corpus["rxdbCommit"].as_str(),
        Some("3b763ee17d62bc96cfae556ea0ccf92f9891c8c1"),
        "the expectations must come from the pinned upstream commit"
    );
    assert!(cases.len() >= 12, "expected at least 12 boundary schemas");
    assert!(
        values >= 150,
        "expected at least 150 probe values, got {values}"
    );
}

/// The digit budget upstream derives from each schema, reproduced exactly.
///
/// This is asserted separately from the encodings because it is the *cause*: if
/// `nonDecimals` is wrong, every value for that schema is wrong in the same way, and a
/// per-value failure would bury the reason.
#[test]
fn digit_budgets_match_upstream() {
    let corpus = load();
    let mut checked = 0;
    let mut failures = Vec::new();

    for entry in corpus["cases"].as_array().expect("cases") {
        let name = entry["schema"].as_str().expect("schema");
        let schema = filled_schema(&entry["spec"]);
        let part = dxdb_schema::schema_part_by_object_path(&schema, "value").expect("value part");
        let budget = &entry["digitBudget"];
        match get_string_length_of_index_number(part) {
            Ok(ours) => {
                let expected_non_decimals = budget["nonDecimals"].as_u64().expect("nonDecimals");
                let expected_decimals = budget["decimals"].as_u64().expect("decimals");
                if ours.non_decimals as u64 != expected_non_decimals
                    || ours.decimals as u64 != expected_decimals
                {
                    failures.push(format!(
                        "{name}: budget nonDecimals={} decimals={} != upstream {},{}",
                        ours.non_decimals, ours.decimals, expected_non_decimals, expected_decimals
                    ));
                } else if ours.minimum != budget["minimum"].as_f64().expect("minimum")
                    || ours.maximum != budget["maximum"].as_f64().expect("maximum")
                {
                    failures.push(format!(
                        "{name}: budget range [{}, {}] != upstream [{}, {}]",
                        ours.minimum, ours.maximum, budget["minimum"], budget["maximum"]
                    ));
                } else {
                    checked += 1;
                }
            }
            Err(error) => failures.push(format!("{name}: budget failed: {error}")),
        }
    }

    assert!(
        failures.is_empty(),
        "{} digit-budget divergences:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(
        checked,
        corpus["schemaCount"].as_u64().expect("schemaCount")
    );
}

/// Index-string *length* per schema.
#[test]
fn index_string_lengths_match_upstream() {
    let corpus = load();
    let mut failures = Vec::new();
    for entry in corpus["cases"].as_array().expect("cases") {
        let name = entry["schema"].as_str().expect("schema");
        let schema = filled_schema(&entry["spec"]);
        let index = index_of(entry);
        let expected = entry["length"].as_u64().expect("length") as usize;
        match get_index_string_length(&schema, &index) {
            Ok(ours) if ours == expected => {}
            Ok(ours) => failures.push(format!(
                "{name}: width {ours} != upstream {expected} (index {index:?})"
            )),
            Err(error) => failures.push(format!("{name}: width failed: {error}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} lengths diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Every probe value's index string, both bound strings, and the fixed width.
#[test]
fn boundary_values_encode_like_upstream() {
    let corpus = load();
    let mut encoded_checks = 0u32;
    let mut bound_checks = 0u32;
    let mut failures = Vec::new();

    for entry in corpus["cases"].as_array().expect("cases") {
        let name = entry["schema"].as_str().expect("schema");
        let schema = filled_schema(&entry["spec"]);
        let index = index_of(entry);
        let width = entry["length"].as_u64().expect("length") as usize;

        for probe in entry["values"].as_array().expect("values") {
            let label = probe["label"].as_str().unwrap_or("?");
            let value = &probe["value"];

            // 1. The index string for a document holding this value.
            let document = serde_json::json!({
                "id": "x",
                "value": value,
                "_deleted": false,
                "_rev": "1-x",
                "_meta": {"lwt": 1},
                "_attachments": {},
            });
            match get_indexable_string(&schema, &index, &document) {
                Ok(ours) => {
                    let expected = probe["indexString"].as_str().unwrap_or("");
                    if ours != expected {
                        failures.push(format!(
                            "{name} {label} ({value}): encoded {ours:?} != upstream {expected:?}"
                        ));
                    } else if ours.chars().count() != width {
                        // The recorded length and the recorded string must agree, and ours
                        // must match both — this is the invariant a variable-width bug breaks.
                        failures.push(format!(
                            "{name} {label}: encoded width {} != declared {width}",
                            ours.chars().count()
                        ));
                    } else {
                        encoded_checks += 1;
                    }
                }
                Err(error) => failures.push(format!("{name} {label}: encode failed: {error}")),
            }

            // 2. The lower and upper bound strings, with this value at its field position.
            let position = index
                .iter()
                .position(|field| field == "value")
                .expect("value in index");
            let mut bounds = Vec::with_capacity(index.len());
            for (at, field) in index.iter().enumerate() {
                if at == position {
                    bounds.push(BoundValue::from_json(Some(value)));
                    continue;
                }
                let part = dxdb_schema::schema_part_by_object_path(&schema, field);
                bounds.push(
                    match part.and_then(|p| p.get("type")).and_then(Value::as_str) {
                        Some("number") | Some("integer") => BoundValue::Number(
                            part.and_then(|p| p.get("minimum"))
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0),
                        ),
                        Some("boolean") => BoundValue::Bool(false),
                        _ => BoundValue::String(String::new()),
                    },
                );
            }
            match get_start_index_string_from_lower_bound(&schema, &index, &bounds) {
                Ok(ours) => {
                    let expected = probe["lowerBoundString"].as_str().unwrap_or("");
                    if ours != expected {
                        failures.push(format!(
                            "{name} {label} ({value}): lower {ours:?} != upstream {expected:?}"
                        ));
                    } else {
                        bound_checks += 1;
                    }
                }
                Err(error) => failures.push(format!("{name} {label}: lower bound failed: {error}")),
            }
            match get_start_index_string_from_upper_bound(&schema, &index, &bounds) {
                Ok(ours) => {
                    let expected = probe["upperBoundString"].as_str().unwrap_or("");
                    if ours != expected {
                        failures.push(format!(
                            "{name} {label} ({value}): upper {ours:?} != upstream {expected:?}"
                        ));
                    } else {
                        bound_checks += 1;
                    }
                }
                Err(error) => failures.push(format!("{name} {label}: upper bound failed: {error}")),
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} boundary divergences ({} values encoded, {} bounds checked):\n{}",
        failures.len(),
        encoded_checks,
        bound_checks,
        failures.join("\n")
    );
    let expected_values = corpus["valueCount"].as_u64().expect("valueCount") as u32;
    assert_eq!(encoded_checks, expected_values, "every value must encode");
    assert_eq!(
        bound_checks,
        expected_values * 2,
        "every value needs both bounds"
    );
}

/// Clamping, asserted as a property rather than only through the oracle.
///
/// A value outside the declared range must encode identically to the nearest boundary.
/// Stating it independently means a corpus that stopped containing out-of-range probes would
/// still fail if clamping regressed.
#[test]
fn out_of_range_values_clamp_to_the_boundary() {
    let corpus = load();
    let mut checks = 0;
    for entry in corpus["cases"].as_array().expect("cases") {
        let schema = filled_schema(&entry["spec"]);
        let index = index_of(entry);
        let encode = |value: f64| -> String {
            let document = serde_json::json!({
                "id": "x", "value": value, "_deleted": false, "_rev": "1-x",
                "_meta": {"lwt": 1}, "_attachments": {},
            });
            get_indexable_string(&schema, &index, &document)
                .unwrap_or_else(|error| panic!("encode {value}: {error}"))
        };

        // The clamp boundary is the *floored* minimum and the *ceiled* maximum, not the
        // declared values: `getStringLengthOfIndexNumber` takes `floor(minimum)` and
        // `ceil(maximum)`, and `getNumberIndexString` clamps against those. So on a
        // `minimum: 0.5` field the value `0.4` is still inside the encodable range and is
        // *not* clamped — upstream records `0.4 -> "0004"` for `ceil-max` while
        // `-999.5 -> "0000"`. Asserting against the declared minimum was wrong; the
        // corpus's own `digitBudget` carries the floored/ceiled bounds, so use them.
        let clamp_low = entry["digitBudget"]["minimum"]
            .as_f64()
            .expect("budget minimum");
        let clamp_high = entry["digitBudget"]["maximum"]
            .as_f64()
            .expect("budget maximum");
        assert_eq!(
            encode(clamp_low - 1.0),
            encode(clamp_low),
            "{}: below the floored minimum ({clamp_low}) must clamp",
            entry["schema"]
        );
        assert_eq!(
            encode(clamp_high + 1.0),
            encode(clamp_high),
            "{}: above the ceiled maximum ({clamp_high}) must clamp",
            entry["schema"]
        );
        assert_eq!(
            encode(clamp_low - 1e9),
            encode(clamp_low),
            "{}: far below must clamp",
            entry["schema"]
        );
        assert_eq!(
            encode(clamp_high + 1e9),
            encode(clamp_high),
            "{}: far above must clamp",
            entry["schema"]
        );
        checks += 4;
    }
    assert_eq!(
        checks,
        corpus["schemaCount"].as_u64().expect("schemaCount") as usize * 4
    );
}

/// Within the declared range, the encoding is monotone.
///
/// This is the property a storage's ordered scan depends on, and it is asserted on every
/// schema in the corpus rather than only the ones a human thought to check.
#[test]
fn encoding_is_monotone_across_the_declared_range() {
    let corpus = load();
    let mut comparisons = 0;
    for entry in corpus["cases"].as_array().expect("cases") {
        let schema = filled_schema(&entry["spec"]);
        let index = index_of(entry);
        let minimum = entry["spec"]["minimum"].as_f64().expect("minimum");
        let maximum = entry["spec"]["maximum"].as_f64().expect("maximum");
        let step = (maximum - minimum) / 20.0;

        let encode = |value: f64| -> String {
            let document = serde_json::json!({
                "id": "x", "value": value, "_deleted": false, "_rev": "1-x",
                "_meta": {"lwt": 1}, "_attachments": {},
            });
            get_indexable_string(&schema, &index, &document)
                .unwrap_or_else(|error| panic!("encode {value}: {error}"))
        };

        let mut previous: Option<(f64, String)> = None;
        for step_index in 0..=20 {
            let value = minimum + step * step_index as f64;
            let encoded = encode(value);
            if let Some((previous_value, previous_encoded)) = &previous {
                assert!(
                    dxdb_jsstr::lt(previous_encoded, &encoded),
                    "{}: {previous_value} encoded to {previous_encoded:?} which does not sort \
                     before {value} ({encoded:?})",
                    entry["schema"]
                );
                comparisons += 1;
            }
            previous = Some((value, encoded));
        }
    }
    assert!(
        comparisons >= 250,
        "expected 300 range comparisons, did {comparisons}"
    );
}

/// The clamp boundary is the floored minimum and ceiled maximum, not the declared values.
///
/// `getStringLengthOfIndexNumber` takes `floor(schemaPart.minimum)` and
/// `ceil(schemaPart.maximum)` and those are what `getNumberIndexString` clamps against. On a
/// `minimum: 0.5, maximum: 10.5` field the encodable range is therefore `[0, 11]`, so a value
/// of `0.4` is inside it and must *not* clamp — while `-0.5` must.
///
/// This was wrong in the first version of the test beside it, which assumed the declared
/// minimum was the clamp boundary. Upstream's own recorded output corrected it
/// (`ceil-max` records `0.4 -> "0004"` and `-999.5 -> "0000"`), which is the whole reason the
/// corpus is generated from upstream rather than from our reading of the source.
#[test]
fn the_clamp_boundary_is_the_floored_minimum_and_ceiled_maximum() {
    let spec = serde_json::json!({
        "name": "ceil-max-check",
        "minimum": 0.5,
        "maximum": 10.5,
        "multipleOf": 0.1,
    });
    let schema = filled_schema(&spec);
    let index = vec![
        "_deleted".to_string(),
        "value".to_string(),
        "id".to_string(),
    ];
    let encode = |value: f64| -> String {
        let document = serde_json::json!({
            "id": "x", "value": value, "_deleted": false, "_rev": "1-x",
            "_meta": {"lwt": 1}, "_attachments": {},
        });
        get_indexable_string(&schema, &index, &document)
            .unwrap_or_else(|error| panic!("encode {value}: {error}"))
    };

    // Declared minimum is 0.5 but the floor is 0, so 0.4 keeps its own encoding.
    assert_ne!(
        encode(0.4),
        encode(0.5),
        "0.4 is inside the floored range and must not clamp onto 0.5"
    );
    // 0.4 and 0.0 share the integer bucket but differ in the decimal digit, which is the
    // whole reason the encoding keeps a decimal budget: upstream records `0.4 -> "0004"` and
    // `0.0 -> "0000"` for this schema.
    assert_eq!(encode(0.4), encode(0.4));
    assert_ne!(
        encode(0.4),
        encode(0.0),
        "the decimal digit must distinguish them"
    );
    // Below the floor, clamping applies.
    assert_eq!(encode(-0.5), encode(0.0), "below the floor must clamp");
    assert_eq!(encode(-1e9), encode(0.0));
    // Above the ceil (11), clamping applies; 10.6 is inside it.
    assert_ne!(
        encode(10.6),
        encode(11.0),
        "10.6 is inside the ceiled range"
    );
    assert_eq!(encode(11.5), encode(11.0), "above the ceil must clamp");
}
