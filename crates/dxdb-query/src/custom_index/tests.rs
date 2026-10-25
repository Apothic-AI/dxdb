//! Ported upstream index-string tests.
//!
//! Sources: `test/unit/custom-index.test.ts` (the bulk of this file) and the
//! numbers in `src/custom-index.ts`. Each test names its upstream line.
//!
//! Fixtures follow upstream's `schema` in `custom-index.test.ts:36`: a primary key
//! of `maxLength: 100`, a number field with a two-decimal `multipleOf` and a
//! boolean, with indexes chosen so that every index type appears in a compound.

use serde_json::{json, Value};

use super::*;
use dxdb_schema::fill_with_default_settings;

/// `custom-index.test.ts:36` — the fixture schema.
fn index_test_schema() -> Value {
    fill_with_default_settings(&json!({
        "primaryKey": "id",
        "version": 0,
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "num": {"type": "number", "minimum": 10, "maximum": 100, "multipleOf": 0.01},
            "bool": {"type": "boolean"}
        },
        "required": ["id", "num", "bool"],
        "indexes": [["id"], ["num"], ["bool"], ["bool", "num"]]
    }))
    .expect("fixture fills")
}

/// `custom-index.test.ts:71` — a document with fixed values, so the expected
/// strings below are literals rather than computed.
fn doc(id: &str, num: f64, flag: bool) -> Value {
    json!({
        "id": id,
        "num": num,
        "bool": flag,
        "_deleted": false,
        "_rev": "1-x",
        "_meta": {"lwt": 1},
        "_attachments": {}
    })
}

fn index_of(fields: &[&str]) -> Vec<String> {
    fields.iter().map(|f| f.to_string()).collect()
}

// --- custom-index.test.ts:86 `.getStringLengthOfIndexNumber()` -----------------

// custom-index.test.ts:87 `should calculate the correct length`
#[test]
fn string_length_of_index_number() {
    let parsed = get_string_length_of_index_number(&json!({
        "type": "number",
        "minimum": 0.1,
        "maximum": 110.5,
        "multipleOf": 0.01
    }))
    .expect("parses");
    assert_eq!(parsed.decimals, 2);
    // floor(0.1) = 0, ceil(110.5) = 111, span = 111 -> "111" is three characters.
    assert_eq!(parsed.non_decimals, 3);
    assert_eq!(parsed.minimum, 0.0);
    assert_eq!(parsed.maximum, 111.0);
    assert_eq!(parsed.multiplier, 100.0);
}

// custom-index.test.ts:196 `should produce monotonically increasing strings`
#[test]
fn number_index_strings_are_monotonic() {
    let parsed = get_string_length_of_index_number(&json!({
        "type": "number",
        "minimum": -10,
        "maximum": 10,
        "multipleOf": 0.1
    }))
    .expect("parses");
    let values = [-2.0, -1.5, -1.0, -0.5, 0.0, 0.5, 1.0, 1.5, 2.0];
    let strings: Vec<String> = values
        .iter()
        .map(|v| get_number_index_string(&parsed, Some(*v)))
        .collect();
    for window in strings.windows(2) {
        assert!(
            dxdb_jsstr::lt(&window[0], &window[1]),
            "expected {:?} < {:?}",
            window[0],
            window[1]
        );
    }
}

// custom-index.test.ts:99 `index-type: string`
#[test]
fn string_index_part_is_padded_to_max_length() {
    let schema = index_test_schema();
    let index = index_of(&["id"]);
    let a = get_indexable_string(&schema, &index, &doc("bb", 10.0, true)).expect("encodes");
    let b = get_indexable_string(&schema, &index, &doc("aa", 10.0, true)).expect("encodes");
    assert_eq!(dxdb_jsstr::js_length(&a), 100);
    assert_eq!(dxdb_jsstr::js_length(&b), 100);
    assert!(dxdb_jsstr::lt(&b, &a), "aa sorts before bb");
}

// custom-index.test.ts:214 `should work correctly on big numbers`
#[test]
fn big_numbers_keep_both_decimal_digits() {
    let schema = fill_with_default_settings(&json!({
        "primaryKey": "id",
        "version": 0,
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 10},
            "bigNum": {"type": "number", "minimum": 0, "maximum": 1000000000000000i64, "multipleOf": 0.01}
        },
        "additionalProperties": false,
        "required": ["bigNum"]
    }))
    .expect("fills");
    let index = index_of(&["bigNum", "id"]);
    let index_string = get_indexable_string(
        &schema,
        &index,
        &json!({
            "id": "foobar",
            "bigNum": 1661946016806.01f64,
            "_attachments": {},
            "_deleted": false,
            "_rev": "1-x",
            "_meta": {"lwt": 1}
        }),
    )
    .expect("encodes");
    assert!(
        index_string.starts_with("000166194601680601"),
        "got {index_string:?}"
    );
}

// custom-index.test.ts:268 `indexing a optional field must work`
#[test]
fn optional_field_encodes_as_spaces() {
    let schema = fill_with_default_settings(&json!({
        "primaryKey": "id",
        "version": 0,
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "optional": {"type": "string", "maxLength": 100}
        },
        "required": ["id"],
        "indexes": [["id"], ["optional"]]
    }))
    .expect("fills");
    let encoded = get_indexable_string(
        &schema,
        &index_of(&["optional"]),
        &json!({"id": "foo"}),
    )
    .expect("encodes");
    assert!(!encoded.is_empty());
    assert!(
        encoded.chars().all(|c| c == ' '),
        "an absent optional string is all padding, which sorts before any value; got {encoded:?}"
    );
}

// --- custom-index.test.ts:323 `.getIndexStringLength()` -----------------------

// custom-index.test.ts:324 `get the correct length`
#[test]
fn index_string_length_matches_encoded_length() {
    let schema = index_test_schema();
    for fields in [vec!["num", "id"], vec!["bool", "num", "id"]] {
        let index = index_of(&fields);
        let length = get_index_string_length(&schema, &index).expect("length");
        let encoded = get_indexable_string(&schema, &index, &doc("abc", 42.0, true)).expect("encodes");
        assert_eq!(encoded.chars().count(), length, "index {fields:?}");
    }
}

// custom-index.test.ts:340 `issue did not work with this numberValue`
#[test]
fn index_string_length_holds_for_negative_numbers() {
    let schema = fill_with_default_settings(&json!({
        "version": 0,
        "type": "object",
        "primaryKey": "key",
        "properties": {
            "key": {"type": "string", "maxLength": 50},
            "numberValue": {"type": "number", "minimum": -10, "maximum": 1000, "multipleOf": 1}
        },
        "required": ["key", "numberValue"],
        "indexes": [["numberValue", "key"]],
        "additionalProperties": false
    }))
    .expect("fills");
    let index = index_of(&["numberValue", "key"]);
    let length = get_index_string_length(&schema, &index).expect("length");
    for value in [17.0, -5.0] {
        let encoded = get_indexable_string(
            &schema,
            &index,
            &json!({
                "key": "abc",
                "numberValue": value,
                "_deleted": false,
                "_attachments": {},
                "_meta": {"lwt": 1},
                "_rev": "1-x"
            }),
        )
        .expect("encodes");
        assert_eq!(encoded.chars().count(), length, "value {value}");
    }
}

// --- custom-index.test.ts:402 `.getPrimaryKeyFromIndexableString()` -----------

// custom-index.test.ts:403 `get the correct id`
#[test]
fn primary_key_round_trips_out_of_an_index_string() {
    let schema = index_test_schema();
    for fields in [vec!["num", "id"], vec!["bool", "num", "id"]] {
        let index = index_of(&fields);
        let encoded =
            get_indexable_string(&schema, &index, &doc("foobar", 42.0, true)).expect("encodes");
        let id = get_primary_key_from_indexable_string(&schema, "id", &encoded).expect("extracts");
        assert_eq!(id, "foobar", "index {fields:?}");
    }
}

// --- custom-index.test.ts:420 lower bound -------------------------------------

// custom-index.test.ts:421 `should find the correct docs when comparing with the index`
#[test]
fn lower_bound_excludes_below() {
    let schema = index_test_schema();
    let index = index_of(&["bool", "num"]);
    let lower = get_start_index_string_from_lower_bound(
        &schema,
        &index,
        &[BoundValue::Bool(true), BoundValue::Number(30.0)],
    )
    .expect("builds");
    for num in [10.0, 29.99, 30.0, 30.01, 42.0, 99.99, 100.0] {
        for flag in [true, false] {
            let encoded =
                get_indexable_string(&schema, &index, &doc("x", num, flag)).expect("encodes");
            let matches = dxdb_jsstr::ge(&encoded, &lower);
            if flag && num >= 30.0 {
                assert!(matches, "num={num} flag={flag} should be in range");
            }
            if !flag || num < 30.0 {
                assert!(!matches, "num={num} flag={flag} should be out of range");
            }
        }
    }
}

// --- custom-index.test.ts:448 upper bound -------------------------------------

// custom-index.test.ts:449 `should match the correct docs`
#[test]
fn upper_bound_excludes_above() {
    let schema = index_test_schema();
    let index = index_of(&["bool", "num"]);
    let upper = get_start_index_string_from_upper_bound(
        &schema,
        &index,
        &[BoundValue::Bool(false), BoundValue::Number(30.0)],
    )
    .expect("builds");
    for num in [10.0, 29.99, 30.0, 30.01, 42.0] {
        for flag in [true, false] {
            let encoded =
                get_indexable_string(&schema, &index, &doc("x", num, flag)).expect("encodes");
            let matches = dxdb_jsstr::le(&encoded, &upper);
            if !flag && num <= 30.0 {
                assert!(matches, "num={num} flag={flag} should be in range");
            }
            if flag || num > 30.0 {
                assert!(!matches, "num={num} flag={flag} should be out of range");
            }
        }
    }
}

// custom-index.test.ts:474 `should match the correct docs if bound is undefined`
#[test]
fn undefined_upper_bound_matches_everything() {
    let schema = index_test_schema();
    let index = index_of(&["id"]);
    let upper =
        get_start_index_string_from_upper_bound(&schema, &index, &[BoundValue::Missing])
            .expect("builds");
    for id in ["", "a", "abc", "zzz", "\u{FFFF}"] {
        let encoded = get_indexable_string(&schema, &index, &doc(id, 10.0, true)).expect("encodes");
        assert!(
            dxdb_jsstr::le(&encoded, &upper),
            "an absent bound must not exclude id {id:?}"
        );
    }
}

// custom-index.test.ts:508 `_deleted+_meta.lwt index`
#[test]
fn deleted_lwt_index_bounds() {
    let schema = index_test_schema();
    let index = index_of(&["_deleted", "_meta.lwt"]);
    let lower = get_start_index_string_from_lower_bound(
        &schema,
        &index,
        &[BoundValue::Bool(true), BoundValue::Number(1.0)],
    )
    .expect("builds");
    let document = json!({
        "id": "x",
        "num": 10.0,
        "bool": true,
        "_deleted": true,
        "_rev": "1-x",
        "_attachments": {},
        "_meta": {"lwt": 1700000000000.0}
    });
    let encoded = get_indexable_string(&schema, &index, &document).expect("encodes");
    assert!(
        dxdb_jsstr::lt(&lower, &encoded),
        "lower {lower:?} must sort before {encoded:?}"
    );

    let upper = get_start_index_string_from_upper_bound(
        &schema,
        &index,
        &[BoundValue::Bool(true), BoundValue::Number(1700000001000.0)],
    )
    .expect("builds");
    assert!(upper.starts_with('1'));
    assert!(dxdb_jsstr::lt(&encoded, &upper));

    // Upstream steps by 1000 * 100 here (`custom-index.test.ts:542`), a whole
    // order of magnitude further out, so the bound must move further.
    let further = get_start_index_string_from_upper_bound(
        &schema,
        &index,
        &[BoundValue::Bool(true), BoundValue::Number(1700000000000.0 + 100_000.0)],
    )
    .expect("builds");
    assert!(dxdb_jsstr::gt(&further, &upper));
}

// --- behaviour that only shows up in the encoding ----------------------------

// src/custom-index.ts:290 — clamping, not a broken index.
#[test]
fn out_of_range_numbers_clamp() {
    let schema = index_test_schema();
    let index = index_of(&["num"]);
    let below = get_indexable_string(&schema, &index, &doc("x", -5.0, true)).expect("encodes");
    let at_min = get_indexable_string(&schema, &index, &doc("x", 10.0, true)).expect("encodes");
    assert_eq!(
        below, at_min,
        "a value below the schema minimum must encode as the minimum, not as a \
         negative number in a zero-padded field"
    );
    let above = get_indexable_string(&schema, &index, &doc("x", 5000.0, true)).expect("encodes");
    let at_max = get_indexable_string(&schema, &index, &doc("x", 100.0, true)).expect("encodes");
    assert_eq!(above, at_max);
}

// src/custom-index.ts:308 — rounding must not widen the field.
#[test]
fn decimal_rounding_does_not_overflow_the_field() {
    let parsed = get_string_length_of_index_number(&json!({
        "type": "number",
        "minimum": 0,
        "maximum": 10,
        "multipleOf": 0.1
    }))
    .expect("parses");
    let width = parsed.non_decimals + parsed.decimals;
    // 0.99 rounds to 1.0 at one decimal, which would be a two-digit fraction.
    let encoded = get_number_index_string(&parsed, Some(0.99));
    assert_eq!(encoded.chars().count(), width, "got {encoded:?}");
    assert!(
        encoded.ends_with('9'),
        "the shift is clamped to multiplier - 1; got {encoded:?}"
    );
}

// src/custom-index.ts:297 — `Math.floor`, so negatives truncate downward.
#[test]
fn negative_fractions_floor_toward_minus_infinity() {
    let parsed = get_string_length_of_index_number(&json!({
        "type": "number",
        "minimum": -10,
        "maximum": 10,
        "multipleOf": 1
    }))
    .expect("parses");
    // -0.5 -> integer part floor(-0.5) - (-10) = 9, i.e. one less than 0.
    assert_eq!(get_number_index_string(&parsed, Some(-0.5)), "09");
    assert_eq!(get_number_index_string(&parsed, Some(0.0)), "10");
    assert_eq!(get_number_index_string(&parsed, Some(-10.0)), "00");
}

// src/custom-index.ts:113 — a missing number is 0, then clamped.
#[test]
fn missing_number_encodes_as_the_minimum_when_clamped() {
    let schema = index_test_schema();
    let index = index_of(&["num"]);
    let missing = get_indexable_string(&schema, &index, &json!({})).expect("encodes");
    let zero_clamped = get_indexable_string(&schema, &index, &doc("x", 0.0, true)).expect("encodes");
    assert_eq!(
        missing, zero_clamped,
        "undefined becomes 0 and then clamps to the schema minimum"
    );
}

// src/custom-index.ts:342 — the `\x00` lower bound for strings.
#[test]
fn string_lower_bound_is_below_space_not_at_space() {
    let schema = fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 10},
            "name": {"type": "string", "maxLength": 10}
        },
        "indexes": [["name"]]
    }))
    .expect("fills");
    let index = index_of(&["name"]);
    let lower = get_start_index_string_from_lower_bound(&schema, &index, &[BoundValue::Missing])
        .expect("builds");
    assert!(
        lower.chars().all(|c| c == '\0'),
        "expected NUL padding, got {lower:?}"
    );
    // A value containing characters below space must sort at or after the bound.
    for name in ["\u{0}", "\t", "\n", " a", "a"] {
        let encoded = get_indexable_string(&schema, &index, &json!({"name": name})).expect("encodes");
        assert!(
            dxdb_jsstr::ge(&encoded, &lower),
            "name {name:?} must not be excluded by the lower bound"
        );
    }
}

// src/custom-index.ts:464 — stepping by one code unit, used by storages that
// cannot express inclusive ranges.
#[test]
fn change_by_one_quantum() {
    assert_eq!(change_indexable_string_by_one_quantum("abc", 1), "abd");
    assert_eq!(change_indexable_string_by_one_quantum("abc", -1), "abb");
    assert_eq!(change_indexable_string_by_one_quantum("", 1), "");
}

#[test]
fn index_string_length_of_index_number_matches_upstream_fixture() {
    // From `custom-index.test.ts:36`: minimum 10, maximum 100, multipleOf 0.01.
    let schema = index_test_schema();
    let length = get_index_string_length(&schema, &index_of(&["num"])).expect("length");
    // span = 90 -> "90" is two characters; decimals = 2.
    assert_eq!(length, 4);
    let encoded = get_indexable_string(&schema, &index_of(&["num"]), &doc("x", 42.0, true))
        .expect("encodes");
    assert_eq!(encoded, "3200");
}
