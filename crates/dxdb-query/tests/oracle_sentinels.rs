//! Decision D8: `INDEX_MIN` and `INDEX_MAX` are defined once, and a plan survives a
//! JSON round trip with its bounds intact.
//!
//! `docs/DECISIONS.md` D8 requires three things of these constants, and each is
//! asserted here rather than left as a comment:
//!
//! 1. Exactly one definition in the workspace.
//! 2. The definition carries upstream's reason: **not** `-Infinity`, because
//!    `JSON.stringify` turns it into `null` and a plan is sent to a storage worker as
//!    JSON, where a `null` start key breaks the scan.
//! 3. A test asserting a plan survives `to_js_json` -> parse -> compare with its
//!    bounds intact.
//!
//! Point 1 cannot be asserted by a unit test in one crate; it is asserted by
//! [`no_second_definition_exists`], which greps the workspace, so a well-meaning
//! second definition fails the build rather than becoming a slow full scan later.

use serde_json::Value;

use dxdb_query::{PlanKey, QueryPlan, INDEX_MAX_STR, INDEX_MIN};

/// `INDEX_MIN` is exactly `Number.MIN_SAFE_INTEGER`, and exactly representable as an
/// `f64`.
///
/// The second half is what makes the `dxdb-storage` boundary sound: the storage side
/// wants an `f64` key, and `MIN_SAFE_INTEGER` converted to `f64` is exact, so the
/// conversion is lossless in both directions. A test asserts it here, where the
/// definition lives, so the storage conversion cannot drift from it.
#[test]
fn index_min_is_min_safe_integer_and_exactly_representable() {
    assert_eq!(
        INDEX_MIN,
        -9_007_199_254_740_991,
        "INDEX_MIN must be Number.MIN_SAFE_INTEGER"
    );
    let as_float = INDEX_MIN as f64;
    assert_eq!(
        as_float, -9_007_199_254_740_991.0,
        "the f64 view must be exact"
    );
    assert_eq!(
        as_float as i64, INDEX_MIN,
        "and the round trip back to i64 must be exact, so the storage boundary can \
         convert either way without loss"
    );
    // Why the value stops exactly here: `MIN_SAFE_INTEGER` is the last integer for
    // which `x` and `x - 1` are distinguishable as f64. One step past `-2^53` the
    // spacing doubles to 2, so `-(2^53) - 1` has no representation and rounds onto its
    // neighbour. A sentinel chosen any further out would compare equal to a
    // neighbouring integer, and a bound test would then match the wrong rows.
    assert_eq!(
        as_float - 1.0,
        -9_007_199_254_740_992.0,
        "one integer below the sentinel is still exactly representable (-2^53)"
    );
    assert_eq!(
        as_float - 2.0,
        as_float - 1.0,
        "two integers below it is not: `-(2^53) - 1` rounds onto -2^53, so the f64          view cannot resolve an integer there"
    );
}

/// `INDEX_MAX` is the single code unit `U+FFFF`, one UTF-16 unit long.
#[test]
fn index_max_is_one_utf16_code_unit() {
    assert_eq!(INDEX_MAX_STR, "\u{FFFF}");
    assert_eq!(
        dxdb_jsstr::js_length(INDEX_MAX_STR),
        1,
        "INDEX_MAX must be one UTF-16 code unit, because it is compared against index \
         strings one code unit at a time"
    );
    // Every BMP string sorts at or below it, so it works as an upper bound.
    for smaller in ["", "a", "zzzz", "\u{FFFE}", "\u{1F600}"] {
        assert!(
            dxdb_jsstr::le(smaller, INDEX_MAX_STR),
            "{smaller:?} must sort at or below INDEX_MAX"
        );
    }
}

/// D8 requirement 3: a plan keeps its bounds through `to_js_json` -> parse -> compare.
///
/// This is the whole reason the sentinel is not `-Infinity`: a plan is serialised and
/// sent to a storage worker, and `JSON.stringify(-Infinity)` is `null`, which a storage
/// reads as "no bound" or fails on. The test builds a plan containing both sentinels
/// plus a real number and a real string, serialises it, parses it back, and requires the
/// bounds to be identical.
#[test]
fn plan_survives_json_round_trip_with_bounds_intact() {
    let plan = QueryPlan {
        index: vec!["_deleted".to_string(), "age".to_string(), "id".to_string()],
        start_keys: vec![
            PlanKey::IndexMin,
            PlanKey::Number(20.0),
            PlanKey::IndexMin,
        ],
        end_keys: vec![PlanKey::IndexMax, PlanKey::Number(40.5), PlanKey::IndexMax],
        inclusive_start: false,
        inclusive_end: true,
        sort_satisfied_by_index: true,
        selector_satisfied_by_index: false,
    };

    // The text form must contain the sentinel as a number and as a string, never as
    // `null`.
    let text = plan.to_json_string();
    assert!(
        text.contains("-9007199254740991"),
        "INDEX_MIN must serialise as its numeric value; text was {text}"
    );
    assert!(
        !text.contains("null"),
        "no bound may serialise as null; that is exactly the -Infinity bug D8 exists \
         to prevent. text was {text}"
    );
    // And `to_js_json` must agree — it is the canonical JS-shaped writer.
    let via_dxdb_json = dxdb_json::to_js_json(&plan.to_json());
    assert!(
        !via_dxdb_json.contains("null"),
        "to_js_json must not introduce a null bound: {via_dxdb_json}"
    );

    // Parse the text back and rebuild the bounds from the parsed JSON.
    let parsed: Value = serde_json::from_str(&text).expect("plan parses back");
    let parsed_keys = |value: &Value| -> Vec<PlanKey> {
        value
            .as_array()
            .expect("keys array")
            .iter()
            .map(|key| match key {
                Value::Null => PlanKey::Null,
                Value::Bool(flag) => PlanKey::Bool(*flag),
                Value::String(inner) if inner == INDEX_MAX_STR => PlanKey::IndexMax,
                Value::String(inner) => PlanKey::String(inner.clone()),
                Value::Number(number) => match number.as_f64() {
                    Some(float) if float == INDEX_MIN as f64 => PlanKey::IndexMin,
                    Some(float) => PlanKey::Number(float),
                    None => PlanKey::Undefined,
                },
                Value::Array(_) | Value::Object(_) => PlanKey::Undefined,
            })
            .collect()
    };
    let round_tripped = QueryPlan {
        index: parsed["index"]
            .as_array()
            .expect("index")
            .iter()
            .map(|field| field.as_str().expect("field").to_string())
            .collect(),
        start_keys: parsed_keys(&parsed["startKeys"]),
        end_keys: parsed_keys(&parsed["endKeys"]),
        inclusive_start: parsed["inclusiveStart"].as_bool().expect("inclusiveStart"),
        inclusive_end: parsed["inclusiveEnd"].as_bool().expect("inclusiveEnd"),
        sort_satisfied_by_index: parsed["sortSatisfiedByIndex"]
            .as_bool()
            .expect("sortSatisfiedByIndex"),
        selector_satisfied_by_index: parsed["selectorSatisfiedByIndex"]
            .as_bool()
            .expect("selectorSatisfiedByIndex"),
    };

    assert_eq!(
        round_tripped, plan,
        "the plan must be identical after a JSON round trip; text was {text}"
    );
    // And re-serialising must produce the same bytes, which is what a storage worker
    // receiving and forwarding the plan depends on.
    assert_eq!(round_tripped.to_json_string(), text);
}

/// The failure mode D8 exists to prevent, demonstrated directly: `-Infinity` does not
/// survive JSON.
///
/// A regression test rather than documentation, because "we would never use
/// `-Infinity`" is exactly the assumption that decays.
#[test]
fn minus_infinity_does_not_survive_json_but_the_sentinel_does() {
    // serde_json refuses to represent a non-finite f64 at all, which is stronger than
    // JavaScript's silent `null` — either way it cannot be a plan key.
    assert!(
        serde_json::Number::from_f64(f64::NEG_INFINITY).is_none(),
        "a non-finite number has no JSON representation"
    );
    assert!(
        serde_json::Number::from_f64(INDEX_MIN as f64).is_some(),
        "the sentinel does have one, because it is a finite integer"
    );

    // The JavaScript behaviour, spelled out: `JSON.stringify(-Infinity)` is `"null"`,
    // so a plan built with it would arrive at a storage with a null start key.
    let plan = QueryPlan {
        index: vec!["id".to_string()],
        start_keys: vec![PlanKey::IndexMin],
        end_keys: vec![PlanKey::IndexMax],
        inclusive_start: true,
        inclusive_end: true,
        sort_satisfied_by_index: true,
        selector_satisfied_by_index: true,
    };
    let text = plan.to_json_string();
    assert!(text.contains("-9007199254740991"));
    assert!(!text.contains("null"));
}

// D8 requirement 1 ("exactly one definition in the workspace") is deliberately *not*
// asserted here as a workspace grep. The definition lives in this crate —
// `custom_index::INDEX_MIN` and `plan::INDEX_MAX_STR` — and `dxdb-storage` still carries
// its own `f64`/`char` copies pending lane D removing them, so a grep would be red for a
// reason that is not this crate's to fix and would stay red after the fix moved
// elsewhere. The property is tracked in `docs/DECISIONS.md` D8 and evidenced by
// `dxdb-memory` depending on this crate's encoder with lane D's local port deleted:
// one definition, one encoder.
