//! Indexable strings: the encoding every storage sorts by.
//!
//! Ported from `src/custom-index.ts` (`getIndexableStringMonad` at `:164`,
//! `getNumberIndexString` at `:274`, `getStartIndexStringFromLowerBound` at
//! `:337`, `getStartIndexStringFromUpperBound` at `:401`, and
//! `getStringLengthOfIndexNumber` at `:231`).
//!
//! # Why a storage needs this locally, and why it is temporary
//!
//! This module duplicates `dxdb-query`'s index-string code. That is deliberate and
//! provisional: `dxdb-query` is lane B's crate and was unpublished when
//! `dxdb-memory` was written, and `dxdb-memory` is on the conformance testkit's
//! critical path. Duplicating a codec is only defensible if the duplicate cannot
//! silently diverge, so:
//!
//! * this module is tested **directly against upstream's own output** — the 900-case
//!   `testdata/vectors/index-strings.json`, which records the exact string upstream
//!   produces for every index of every fixture schema, including the boundary
//!   probes at and beyond the schema's declared min/max (`tests/index_strings_oracle.rs`);
//! * the schemas it is tested on come from `testdata/vectors/schema-hashes.json`,
//!   which records upstream's *defaulted* schemas, so the tests feed this code the
//!   same input upstream saw, not a hand-written approximation.
//!
//! When lane B publishes `dxdb-query`, `dxdb-memory` should switch to it and this
//! module should be deleted. Until then, the oracle is what keeps the two honest.
//!
//! # The encoding
//!
//! An index string is the concatenation, in index-field order, of one part per
//! field:
//!
//! | field type | part |
//! | --- | --- |
//! | `string` | the value padded **right** with spaces to the schema's `maxLength`; a missing value is `""` |
//! | `boolean` | `"1"` or `"0"` |
//! | `number` / `integer` | the value, shifted and zero-padded (below) |
//!
//! Numbers are the interesting part. The schema declares a `minimum` and
//! `maximum`, which give a fixed *digit budget*: `(max - min)` determines how many
//! integer digits the part occupies, and `multipleOf`'s decimal places determine
//! how many fractional digits. The value is first **clamped** into
//! `[minimum, maximum]` and then encoded as
//! `floor(value) - floor(minimum)` padded left with zeros, followed by the
//! fractional part scaled by `10^decimals` and clamped to `decimals` digits.
//!
//! Clamping rather than erroring is load-bearing: a query for
//! `{age: {$gt: -1000}}` against a schema with `minimum: 0` would otherwise
//! produce a bound string shorter than a real one and silently exclude
//! documents. `testdata/vectors/index-strings.json`'s `kind: "bounds"` cases are
//! exactly this probe.
//!
//! Everything here compares and pads by **code units**, never bytes: the padding
//! width is a JavaScript `String.prototype.padEnd` width, so a string containing
//! an emoji occupies two columns (PLAN.md 3.2, `dxdb_jsstr`).
//!
//! # Failure modes that must stay failures
//!
//! Upstream *throws* for three schema shapes, and the vectors record them under
//! `skipped`:
//!
//! * an index containing a literal number instead of a field path (`CI1`);
//! * a numeric indexed field with no `minimum`/`maximum` (no digit budget exists);
//! * an index naming a field that is not in `properties`.
//!
//! A port that silently accepts these produces index strings that sort wrongly,
//! which is worse than refusing to open the schema, so all three are errors here.

use dxdb_storage::StorageError;
use serde_json::Value;

/// Largest finite value the planner uses as an open upper bound.
///
/// `INDEX_MAX` from `query-planner.ts:18` is `String.fromCharCode(65535)`, a single
/// U+FFFF. Named here because bounds code and index-string code must agree on it.
pub const INDEX_MAX: char = '\u{FFFF}';

/// `INDEX_MIN` from `query-planner.ts:25`: `Number.MIN_SAFE_INTEGER`, chosen so a
/// plan survives `JSON.stringify` (see the comment at `:20`).
pub const INDEX_MIN: f64 = -9_007_199_254_740_991.0;

/// The digit budget derived from a numeric field's declared bounds.
///
/// Mirrors upstream's `ParsedLengths`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParsedLengths {
    /// `Math.floor(minimum)`.
    pub minimum: f64,
    /// `Math.ceil(maximum)`.
    pub maximum: f64,
    /// Number of integer digits: `(max - min).toString().length`.
    pub non_decimals: usize,
    /// Fractional digits, taken from `multipleOf`'s textual form.
    pub decimals: usize,
    /// The offset subtracted from a floored value. Upstream stores `minimum` here.
    pub rounded_minimum: f64,
    /// `10^decimals`, precomputed.
    pub multiplier: f64,
}

/// Derives the digit budget for a numeric schema part.
///
/// `minimum` and `maximum` are required: without them there is no digit budget,
/// and upstream fails with `Cannot read properties of undefined (reading
/// 'minimum')` (recorded in the vectors as a skipped `noStringIndex` case). Taking
/// `Option`s and returning an error makes that a diagnosable refusal instead.
///
/// `multipleOf` defaults to `1`, matching the schema contract that a number without
/// `multipleOf` is an integer multiple of 1 — which gives `decimals = 0`.
pub fn get_string_length_of_index_number(
    schema_part: &Value,
) -> Result<ParsedLengths, StorageError> {
    let minimum = schema_part
        .get("minimum")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            StorageError::InvalidSchema(
                "an indexed numeric field must declare minimum and maximum".to_string(),
            )
        })?;
    let maximum = schema_part
        .get("maximum")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            StorageError::InvalidSchema(
                "an indexed numeric field must declare minimum and maximum".to_string(),
            )
        })?;
    let multiple_of = schema_part
        .get("multipleOf")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);

    let minimum = minimum.floor();
    let maximum = maximum.ceil();
    let value_span = maximum - minimum;
    // `(valueSpan).toString().length`: for a span of 0 this is "0" (one digit), and
    // for 999 it is three. Upstream uses the decimal length of the span.
    let non_decimals = dxdb_jsnum::to_js_string(value_span).len();
    // `multipleOf.toString().split('.')[1].length`: fractional digits of the
    // multiple, and 0 when it is an integer.
    let multiple_text = dxdb_jsnum::to_js_string(multiple_of);
    let decimals = match multiple_text.split_once('.') {
        Some((_, fraction)) => fraction.len(),
        None => 0,
    };
    Ok(ParsedLengths {
        minimum,
        maximum,
        non_decimals,
        decimals,
        rounded_minimum: minimum,
        multiplier: 10f64.powi(decimals as i32),
    })
}

/// Encodes one numeric field value, clamping into the declared bounds.
///
/// Upstream's `getNumberIndexString` (`:274`).
pub fn get_number_index_string(parsed: &ParsedLengths, field_value: f64) -> String {
    // `typeof fieldValue === 'undefined'` -> 0. A non-finite value cannot be
    // clamped meaningfully; NaN compares false against both bounds so it survives
    // upstream's clamps and produces "NaN" in the index, which sorts wrongly but
    // never crashes. Normalising it to the minimum instead keeps the index sorted
    // and cannot make a document unreachable that a query would match, because a
    // NaN value cannot match any bound either.
    let mut field_value = if field_value.is_nan() { 0.0 } else { field_value };
    if field_value < parsed.minimum {
        field_value = parsed.minimum;
    }
    if field_value > parsed.maximum {
        field_value = parsed.maximum;
    }

    let integer_part = (field_value.floor() - parsed.rounded_minimum)
        .to_string()
        // A clamped value can still produce a negative if the schema's minimum is
        // negative and the value floors below it; the clamp above prevents that,
        // so this only guards against float noise.
        .trim_start_matches('-')
        .to_string();
    let mut output = pad_start(&integer_part, parsed.non_decimals, '0');

    if parsed.decimals > 0 {
        let floored = field_value.floor();
        let shifted = ((field_value - floored) * parsed.multiplier)
            .round()
            .min(parsed.multiplier - 1.0);
        let decimal_part = dxdb_jsnum::to_js_string(shifted);
        output.push_str(&pad_start(&decimal_part, parsed.decimals, '0'));
    }
    output
}

/// One index field's encoder: a field path plus the schema part it reads.
#[derive(Clone, Debug)]
pub struct IndexMetaField {
    /// The dot path this field reads.
    pub field_name: String,
    /// The schema fragment for that field.
    pub schema_part: Value,
    /// Numeric digit budget, when the field is numeric.
    pub parsed_lengths: Option<ParsedLengths>,
}

impl IndexMetaField {
    /// Encodes this field's part of the index string for a document.
    pub fn index_string_part(&self, document: &Value) -> Result<String, StorageError> {
        let value = get_object_path(document, &self.field_name);
        let field_type = self
            .schema_part
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("string");
        match field_type {
            "string" => {
                let max_length = self
                    .schema_part
                    .get("maxLength")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        StorageError::InvalidSchema(format!(
                            "indexed string field '{}' has no maxLength",
                            self.field_name
                        ))
                    })? as usize;
                let text = match value {
                    // A falsy value is `""` upstream, so `null` and `""` encode the
                    // same way. That is observable: two documents differing only in
                    // a null-vs-empty string field share an index prefix.
                    Some(Value::String(text)) if !text.is_empty() => text.clone(),
                    _ => String::new(),
                };
                Ok(pad_end(&text, max_length, ' '))
            }
            "boolean" => Ok(match value {
                Some(Value::Bool(true)) => "1".to_string(),
                _ => "0".to_string(),
            }),
            "number" | "integer" => {
                let parsed = self.parsed_lengths.as_ref().ok_or_else(|| {
                    StorageError::InvalidSchema(format!(
                        "indexed numeric field '{}' has no digit budget",
                        self.field_name
                    ))
                })?;
                let number = match value {
                    Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
                    // `typeof fieldValue === 'undefined'` -> 0 upstream.
                    _ => 0.0,
                };
                Ok(get_number_index_string(parsed, number))
            }
            other => Err(StorageError::InvalidSchema(format!(
                "indexed field '{}' has an unsupported type '{other}'",
                self.field_name
            ))),
        }
    }
}

/// Builds the per-field encoders for one index.
///
/// Resolving the schema parts once (rather than per document) is what upstream's
/// "prepare everything outside the returned function" comment is about.
pub fn index_meta(
    schema: &Value,
    index: &[String],
) -> Result<Vec<IndexMetaField>, StorageError> {
    index
        .iter()
        .map(|field_name| {
            let schema_part = get_schema_by_object_path(schema, field_name).ok_or_else(|| {
                StorageError::InvalidSchema(format!(
                    "index names field '{field_name}' which is not in properties"
                ))
            })?;
            let field_type = schema_part
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("string");
            let parsed_lengths = if field_type == "number" || field_type == "integer" {
                Some(get_string_length_of_index_number(&schema_part)?)
            } else {
                None
            };
            Ok(IndexMetaField {
                field_name: field_name.clone(),
                schema_part,
                parsed_lengths,
            })
        })
        .collect()
}

/// The total encoded length of one index, in code units.
///
/// Upstream's `getIndexStringLength` (`:231`). Used by storages that allocate a
/// fixed-width key buffer.
pub fn get_index_string_length(schema: &Value, index: &[String]) -> Result<usize, StorageError> {
    let mut length = 0usize;
    for field in index_meta(schema, index)? {
        let field_type = field
            .schema_part
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("string");
        match field_type {
            "string" => {
                length += field
                    .schema_part
                    .get("maxLength")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
            }
            "boolean" => length += 1,
            _ => {
                let parsed = field.parsed_lengths.as_ref().ok_or_else(|| {
                    StorageError::InvalidSchema(format!(
                        "indexed numeric field '{}' has no digit budget",
                        field.field_name
                    ))
                })?;
                length += parsed.non_decimals + parsed.decimals;
            }
        }
    }
    Ok(length)
}

/// Encodes a whole document's index key for one index.
///
/// The returned closure is what a storage caches per index; this convenience wraps
/// the per-call allocation so tests can call it directly.
pub fn get_indexable_string(
    schema: &Value,
    index: &[String],
    document: &Value,
) -> Result<String, StorageError> {
    let mut output = String::new();
    for field in index_meta(schema, index)? {
        output.push_str(&field.index_string_part(document)?);
    }
    Ok(output)
}

/// A bound key for [`get_start_index_string_from_lower_bound`] and friends.
///
/// Upstream passes `string | boolean | number | null | undefined` and relies on
/// `===` against the `INDEX_MIN`/`INDEX_MAX` sentinels. Modelling the sentinels as
/// variants removes the possibility of a caller passing a *literal*
/// `Number.MIN_SAFE_INTEGER` and accidentally meaning "unbounded", which is exactly
/// the confusion `PlanKey` exists to prevent on the storage side.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundValue {
    /// A concrete value from the query.
    Value(Value),
    /// `INDEX_MIN`: the lowest possible bound.
    IndexMin,
    /// `INDEX_MAX`: the highest possible bound.
    IndexMax,
    /// `null`. Distinct from `IndexMin` for strings and booleans upstream, where it
    /// has its own branch.
    Null,
    /// `undefined`: no bound at this position.
    Undefined,
}

impl BoundValue {
    /// Builds a bound from a JSON value, mapping the sentinels by their JSON form.
    pub fn from_json(value: &Value) -> Self {
        match value {
            Value::Null => BoundValue::Null,
            other => BoundValue::Value(other.clone()),
        }
    }
}

/// Builds the lower-bound string for an index range.
///
/// Upstream `getStartIndexStringFromLowerBound` (`:337`). Differences from the
/// index-string encoder are the point: a bound must bracket an *open* range, so an
/// absent string bound becomes `\x00`-padding (the lowest possible code unit) and an
/// absent numeric bound becomes all-zeroes.
pub fn get_start_index_string_from_lower_bound(
    schema: &Value,
    index: &[String],
    lower_bound: &[BoundValue],
) -> Result<String, StorageError> {
    let mut output = String::new();
    for (position, field) in index_meta(schema, index)?.iter().enumerate() {
        let bound = lower_bound.get(position).unwrap_or(&BoundValue::Undefined);
        output.push_str(&bound_part(schema, field, bound, RangeSide::Lower)?);
    }
    Ok(output)
}

/// Builds the upper-bound string for an index range.
///
/// Upstream `getStartIndexStringFromUpperBound` (`:401`).
pub fn get_start_index_string_from_upper_bound(
    schema: &Value,
    index: &[String],
    upper_bound: &[BoundValue],
) -> Result<String, StorageError> {
    let mut output = String::new();
    for (position, field) in index_meta(schema, index)?.iter().enumerate() {
        let bound = upper_bound.get(position).unwrap_or(&BoundValue::Undefined);
        output.push_str(&bound_part(schema, field, bound, RangeSide::Upper)?);
    }
    Ok(output)
}

#[derive(Clone, Copy)]
enum RangeSide {
    Lower,
    Upper,
}

/// Encodes one bound position.
///
/// Upstream dispatches on the **schema field's type**, then on the *JavaScript type
/// and value* of the bound, comparing with `===` against the `INDEX_MIN` and
/// `INDEX_MAX` values. Reproducing that dispatch literally is the only way to get
/// the edge cases right, and they are not symmetric:
///
/// * `INDEX_MAX` is the **string** `"\u{FFFF}"`, so it matches only on a string
///   field, and on a string field it is also "a string", which changes which branch
///   it takes.
/// * `INDEX_MIN` is the **number** `Number.MIN_SAFE_INTEGER`, so on a *string* field
///   it is "not a string" and lands in the `\x00` arm, while on a *boolean* field
///   upstream tests it explicitly.
/// * **`undefined` is not a sentinel at all.** It matches none of the `===` tests, so
///   it falls into the final `else` of every chain. For a string bound that is
///   `INDEX_MAX`-padding; for a boolean it is JS falsiness (`0`); for a number it is
///   `getNumberIndexString(undefined)`, which is `0` clamped into the schema range.
///
/// That last point is reachable on every plan: a plan whose `start_keys`/`end_keys`
/// is shorter than the index has absent bounds for the trailing positions. Reading
/// `undefined` as "unbounded high" is the natural mistake and it is wrong for
/// booleans and numbers, so [`BoundValue::Undefined`] is threaded through the same
/// branches as any other non-matching value.
fn bound_part(
    _schema: &Value,
    field: &IndexMetaField,
    bound: &BoundValue,
    side: RangeSide,
) -> Result<String, StorageError> {
    let field_type = field
        .schema_part
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string");
    match field_type {
        "string" => {
            let max_length = field
                .schema_part
                .get("maxLength")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    StorageError::InvalidSchema(format!(
                        "indexed string field '{}' has no maxLength",
                        field.field_name
                    ))
                })? as usize;
            // `typeof bound === 'string'`: true for a string value and for the
            // INDEX_MAX sentinel (which *is* that string).
            let bound_text = as_js_string(bound);
            let is_max = is_index_max(bound);
            match side {
                RangeSide::Lower => {
                    if let Some(text) = bound_text {
                        if is_max {
                            Ok(fill(max_length, INDEX_MAX))
                        } else {
                            Ok(pad_end(text, max_length, ' '))
                        }
                    } else {
                        // Not a string: nil, a number, undefined, a boolean. Upstream
                        // uses `\x00` here rather than space, because a string may
                        // begin with a control character below space (tab is 9) and
                        // space-padding would silently exclude those documents from a
                        // range query.
                        Ok(fill(max_length, '\0'))
                    }
                }
                RangeSide::Upper => {
                    if let Some(text) = bound_text {
                        if is_max {
                            Ok(fill(max_length, INDEX_MAX))
                        } else {
                            Ok(pad_end(text, max_length, ' '))
                        }
                    } else if is_index_min(bound) {
                        Ok(fill(max_length, '\0'))
                    } else {
                        // `undefined`, `null`, a boolean, a non-sentinel number.
                        Ok(fill(max_length, INDEX_MAX))
                    }
                }
            }
        }
        "boolean" => Ok(match side {
            // `if (bound === null || bound === INDEX_MIN) '0'`
            // `else if (bound === INDEX_MAX) '1'`
            // `else bound ? '1' : '0'`
            RangeSide::Lower => {
                if is_null(bound) || is_index_min(bound) {
                    "0".to_string()
                } else if is_index_max(bound) {
                    "1".to_string()
                } else {
                    bool_char(js_truthy_bound(bound))
                }
            }
            // `if (bound === null || bound === INDEX_MAX) '1'`
            // `else if (bound === INDEX_MIN) '0'`
            // `else bound ? '1' : '0'`
            //
            // Note `undefined` lands in the last arm and is falsy -> "0", *not* the
            // "unbounded high" that the `null` arm gives.
            RangeSide::Upper => {
                if is_null(bound) || is_index_max(bound) {
                    "1".to_string()
                } else if is_index_min(bound) {
                    "0".to_string()
                } else {
                    bool_char(js_truthy_bound(bound))
                }
            }
        }),
        "number" | "integer" => {
            let parsed = field.parsed_lengths.as_ref().ok_or_else(|| {
                StorageError::InvalidSchema(format!(
                    "indexed numeric field '{}' has no digit budget",
                    field.field_name
                ))
            })?;
            let width = parsed.non_decimals + parsed.decimals;
            let numeric_bound = numeric_value(bound);
            match side {
                // `if (bound === null || bound === INDEX_MIN) zeroes`
                // `else if (bound === INDEX_MAX) maximum`
                // `else getNumberIndexString(bound)`  <- undefined lands here as 0
                RangeSide::Lower => {
                    if is_null(bound) || is_index_min(bound) {
                        Ok("0".repeat(width))
                    } else if is_index_max(bound) {
                        Ok(get_number_index_string(parsed, parsed.maximum))
                    } else {
                        Ok(get_number_index_string(
                            parsed,
                            numeric_bound.unwrap_or(parsed.minimum),
                        ))
                    }
                }
                // `if (bound === null || bound === INDEX_MAX) nines`
                // `else if (bound === INDEX_MIN) zeroes`
                // `else getNumberIndexString(bound)`  <- undefined lands here as 0
                RangeSide::Upper => {
                    if is_null(bound) || is_index_max(bound) {
                        Ok("9".repeat(width))
                    } else if is_index_min(bound) {
                        Ok("0".repeat(width))
                    } else {
                        Ok(get_number_index_string(
                            parsed,
                            numeric_bound.unwrap_or(parsed.minimum),
                        ))
                    }
                }
            }
        }
        other => Err(StorageError::InvalidSchema(format!(
            "indexed field '{}' has an unsupported type '{other}'",
            field.field_name
        ))),
    }
}

/// `typeof bound === 'string' ? bound : undefined`, with the sentinel folded in.
fn as_js_string(bound: &BoundValue) -> Option<&str> {
    match bound {
        BoundValue::Value(Value::String(text)) => Some(text.as_str()),
        // The sentinel *is* the string, so a caller passing it literally is the same
        // case upstream.
        BoundValue::IndexMax => Some(INDEX_MAX_STR),
        _ => None,
    }
}

/// `bound === null`.
fn is_null(bound: &BoundValue) -> bool {
    matches!(bound, BoundValue::Null | BoundValue::Value(Value::Null))
}

/// `bound === INDEX_MIN`, where the sentinel is the number `MIN_SAFE_INTEGER`.
fn is_index_min(bound: &BoundValue) -> bool {
    match bound {
        BoundValue::IndexMin => true,
        BoundValue::Value(Value::Number(number)) => {
            // Exact equality, not a tolerance: the sentinel is an integer that is
            // representable exactly in an f64, so a caller passing the literal must
            // be the same case.
            number.as_f64() == Some(INDEX_MIN)
        }
        _ => false,
    }
}

/// `bound === INDEX_MAX`, where the sentinel is the string `"\u{FFFF}"`.
fn is_index_max(bound: &BoundValue) -> bool {
    match bound {
        BoundValue::IndexMax => true,
        BoundValue::Value(Value::String(text)) => text.as_str() == INDEX_MAX_STR,
        _ => false,
    }
}

/// The bound as a number, for the numeric branch's `else getNumberIndexString(bound)`.
fn numeric_value(bound: &BoundValue) -> Option<f64> {
    match bound {
        BoundValue::Value(Value::Number(number)) => number.as_f64(),
        // `Math.floor(undefined)` is NaN upstream, which survives the clamps and
        // would put "NaN" into an index string. Treating an absent bound as the
        // schema minimum keeps the bound valid and sortable; a query bound of
        // `undefined` cannot match anything a real document holds anyway.
        _ => None,
    }
}

/// `INDEX_MAX` as a `&str`.
const INDEX_MAX_STR: &str = "\u{FFFF}";

fn fill(width: usize, fill_char: char) -> String {
    std::iter::repeat_n(fill_char, width).collect()
}

fn bool_char(value: bool) -> String {
    if value {
        "1".to_string()
    } else {
        "0".to_string()
    }
}

/// JavaScript truthiness for a JSON value: everything but `false`, `0`, `""`,
/// `null` and a missing value is truthy. `{}` and `[]` are **truthy** in JavaScript,
/// unlike in most languages, and upstream's `bound ? '1' : '0'` relies on it.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().map(|n| n != 0.0 && !n.is_nan()).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Truthiness of a bound, where `IndexMin`/`IndexMax` are truthy values and
/// `Undefined`/`Null` are falsy — matching what JavaScript would see.
fn js_truthy_bound(bound: &BoundValue) -> bool {
    match bound {
        BoundValue::Value(value) => js_truthy(value),
        BoundValue::IndexMin => true,
        BoundValue::IndexMax => true,
        BoundValue::Null => false,
        BoundValue::Undefined => false,
    }
}

/// Reads a sub-schema by dot path.
///
/// Upstream `getSchemaByObjectPath` (`rx-schema-helper.ts:65`). Walking
/// `properties` segment by segment is the whole rule; a segment that is not in
/// `properties` makes the field unknown, which is the `notExistingIndex` case the
/// vectors record as refused.
pub fn get_schema_by_object_path(schema: &Value, path: &str) -> Option<Value> {
    // Upstream does not walk `properties` as it descends; it rewrites the dotted path
    // into a flat property path and then does one plain property lookup:
    //
    //   usePath = path.replace(/REGEX_ALL_DOTS/g, '.properties.')
    //   usePath = 'properties.' + trimDots(usePath)
    //   return getProperty(schema, usePath)
    //
    // That is equivalent to interleaving `.properties.` between every segment, which
    // is what this loop does. Writing it as "descend into `properties` then take the
    // field" is *not* equivalent and fails on `_meta.lwt`: the segment `lwt` lives
    // under `_meta.properties.lwt`, not under `_meta.lwt`.
    // The first segment is looked up under `properties` too, because upstream
    // prefixes the whole rewritten path with `properties.`.
    let mut current = schema.get("properties")?;
    for (position, segment) in path.split('.').enumerate() {
        if position > 0 {
            current = current.get("properties")?;
        }
        current = current.get(segment)?;
    }
    Some(current.clone())
}

/// `String.prototype.padEnd`, by code units.
fn pad_end(text: &str, width: usize, fill: char) -> String {
    let current = dxdb_jsstr::js_length(text);
    if current >= width {
        return text.to_string();
    }
    let mut output = text.to_string();
    for _ in 0..(width - current) {
        output.push(fill);
    }
    output
}

/// `String.prototype.padStart`, by code units.
fn pad_start(text: &str, width: usize, fill: char) -> String {
    let current = dxdb_jsstr::js_length(text);
    if current >= width {
        return text.to_string();
    }
    let mut output = String::with_capacity(width);
    for _ in 0..(width - current) {
        output.push(fill);
    }
    output.push_str(text);
    output
}

/// Reads a dot path out of a JSON document, with numeric segments indexing arrays.
fn get_object_path<'a>(document: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = document;
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => {
                if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                items.get(segment.parse::<usize>().ok()?)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// The `String.fromCharCode(65535)` sentinel, as a `&str` for comparisons.
pub fn index_max_str() -> String {
    INDEX_MAX.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(properties: Value, indexes: Value) -> Value {
        json!({ "version": 0, "primaryKey": "id", "properties": properties, "indexes": indexes })
    }

    #[test]
    fn strings_pad_to_max_length() {
        let schema = schema(json!({ "id": { "type": "string", "maxLength": 5 } }), json!([]));
        assert_eq!(
            get_indexable_string(&schema, &["id".to_string()], &json!({ "id": "ab" })).expect("ok"),
            "ab   "
        );
        // Longer than maxLength is not truncated: upstream's padEnd leaves it alone.
        assert_eq!(
            get_indexable_string(&schema, &["id".to_string()], &json!({ "id": "abcdefgh" }))
                .expect("ok"),
            "abcdefgh"
        );
    }

    #[test]
    fn numbers_clamp_into_the_declared_bounds() {
        let schema = schema(
            json!({
                "id": { "type": "string", "maxLength": 1 },
                "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 }
            }),
            json!([]),
        );
        let index = vec!["age".to_string()];
        // Three digits of budget for a span of 150.
        assert_eq!(
            get_indexable_string(&schema, &index, &json!({ "age": 42 })).expect("ok"),
            "042"
        );
        // Below the minimum clamps to the minimum, not to a negative or short string.
        assert_eq!(
            get_indexable_string(&schema, &index, &json!({ "age": -1000 })).expect("ok"),
            "000"
        );
        // Above the maximum clamps to the maximum.
        assert_eq!(
            get_indexable_string(&schema, &index, &json!({ "age": 9999 })).expect("ok"),
            "150"
        );
        // A missing value is 0.
        assert_eq!(
            get_indexable_string(&schema, &index, &json!({})).expect("ok"),
            "000"
        );
    }

    #[test]
    fn decimal_budget_comes_from_multiple_of() {
        let schema = schema(
            json!({
                "id": { "type": "string", "maxLength": 1 },
                "score": { "type": "number", "minimum": 0, "maximum": 10, "multipleOf": 0.01 }
            }),
            json!([]),
        );
        let lengths =
            get_string_length_of_index_number(&schema["properties"]["score"]).expect("ok");
        assert_eq!(lengths.non_decimals, 2); // span 10 -> "10" -> 2 digits
        assert_eq!(lengths.decimals, 2);
        assert_eq!(
            get_indexable_string(&schema, &["score".to_string()], &json!({ "score": 1.5 }))
                .expect("ok"),
            "0150"
        );
    }

    #[test]
    fn a_numeric_field_without_bounds_is_refused() {
        // Upstream throws here; the vectors record it under `skipped`.
        let schema = schema(
            json!({ "id": { "type": "string", "maxLength": 1 }, "n": { "type": "number" } }),
            json!([]),
        );
        assert!(get_string_length_of_index_number(&schema["properties"]["n"]).is_err());
        assert!(get_indexable_string(&schema, &["n".to_string()], &json!({ "n": 1 })).is_err());
    }

    #[test]
    fn an_index_naming_an_unknown_field_is_refused() {
        let schema = schema(json!({ "id": { "type": "string", "maxLength": 1 } }), json!([]));
        assert!(get_indexable_string(&schema, &["nope".to_string()], &json!({})).is_err());
    }

    #[test]
    fn booleans_are_one_character() {
        let schema = schema(
            json!({ "id": { "type": "string", "maxLength": 1 }, "b": { "type": "boolean" } }),
            json!([]),
        );
        assert_eq!(
            get_indexable_string(&schema, &["b".to_string()], &json!({ "b": true })).expect("ok"),
            "1"
        );
        assert_eq!(
            get_indexable_string(&schema, &["b".to_string()], &json!({ "b": false })).expect("ok"),
            "0"
        );
    }

    #[test]
    fn lower_bound_uses_null_bytes_for_a_non_string_bound() {
        let schema = schema(json!({ "id": { "type": "string", "maxLength": 3 } }), json!([]));
        // `undefined`, `null`, a number — all "not a string", so all `\x00`.
        // Not spaces: a string can start with a control character below space.
        for bound in [
            BoundValue::Undefined,
            BoundValue::Null,
            BoundValue::IndexMin,
            BoundValue::Value(json!(42)),
        ] {
            assert_eq!(
                get_start_index_string_from_lower_bound(&schema, &["id".to_string()], &[bound])
                    .expect("ok"),
                "\0\0\0"
            );
        }
        // INDEX_MAX is *the string* U+FFFF, so it pads with itself.
        assert_eq!(
            get_start_index_string_from_lower_bound(
                &schema,
                &["id".to_string()],
                &[BoundValue::IndexMax]
            )
            .expect("ok"),
            "\u{FFFF}\u{FFFF}\u{FFFF}"
        );
    }

    #[test]
    fn upper_bound_treats_undefined_as_a_non_matching_value_not_as_unbounded() {
        let schema = schema(json!({ "id": { "type": "string", "maxLength": 3 } }), json!([]));
        // `undefined` matches none of upstream's `===` tests, so it takes the final
        // `else`, which pads with INDEX_MAX. This is the same output as INDEX_MAX,
        // but for a *different reason*, and the distinction matters for booleans and
        // numbers below.
        assert_eq!(
            get_start_index_string_from_upper_bound(
                &schema,
                &["id".to_string()],
                &[BoundValue::Undefined]
            )
            .expect("ok"),
            "\u{FFFF}\u{FFFF}\u{FFFF}"
        );
        // INDEX_MIN is the number MIN_SAFE_INTEGER, which for a *string* field is the
        // explicit null-byte arm.
        assert_eq!(
            get_start_index_string_from_upper_bound(
                &schema,
                &["id".to_string()],
                &[BoundValue::IndexMin]
            )
            .expect("ok"),
            "\0\0\0"
        );
    }

    #[test]
    fn undefined_is_falsy_for_boolean_bounds_but_null_is_not() {
        // The trap the vector corpus cannot see: no fixture probes `undefined` on a
        // non-boolean field, so only this test guards the rule.
        let schema = schema(
            json!({ "id": { "type": "string", "maxLength": 1 }, "b": { "type": "boolean" } }),
            json!([]),
        );
        let index = vec!["b".to_string()];
        let upper = |bound: BoundValue| {
            get_start_index_string_from_upper_bound(&schema, &index, &[bound]).expect("ok")
        };
        let lower = |bound: BoundValue| {
            get_start_index_string_from_lower_bound(&schema, &index, &[bound]).expect("ok")
        };
        // `null` -> unbounded high for an upper bound, unbounded low for a lower one.
        assert_eq!(upper(BoundValue::Null), "1");
        assert_eq!(lower(BoundValue::Null), "0");
        // `undefined` -> falsy in the final `else`, so "0" on *both* sides.
        assert_eq!(upper(BoundValue::Undefined), "0");
        assert_eq!(lower(BoundValue::Undefined), "0");
        // The sentinels keep their explicit branches.
        assert_eq!(upper(BoundValue::IndexMax), "1");
        assert_eq!(upper(BoundValue::IndexMin), "0");
        assert_eq!(lower(BoundValue::IndexMax), "1");
        assert_eq!(lower(BoundValue::IndexMin), "0");
    }

    #[test]
    fn numeric_bounds_use_zeroes_and_nines_for_null_but_clamp_undefined() {
        let schema = schema(
            json!({ "id": { "type": "string", "maxLength": 1 }, "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 } }),
            json!([]),
        );
        let index = vec!["age".to_string()];
        // `null` is the "no bound" value upstream recognises.
        assert_eq!(
            get_start_index_string_from_lower_bound(&schema, &index, &[BoundValue::Null])
                .expect("ok"),
            "000"
        );
        assert_eq!(
            get_start_index_string_from_upper_bound(&schema, &index, &[BoundValue::Null])
                .expect("ok"),
            "999"
        );
        // `undefined` is *not* recognised by upstream's `===` tests, so it goes to
        // `getNumberIndexString(undefined)`, which upstream turns into 0 and then
        // clamps into the schema range — the minimum here, not the maximum.
        assert_eq!(
            get_start_index_string_from_lower_bound(&schema, &index, &[BoundValue::Undefined])
                .expect("ok"),
            "000"
        );
        assert_eq!(
            get_start_index_string_from_upper_bound(&schema, &index, &[BoundValue::Undefined])
                .expect("ok"),
            "000"
        );
    }

    #[test]
    fn index_length_matches_the_encoded_length() {
        let schema = schema(
            json!({
                "id": { "type": "string", "maxLength": 4 },
                "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 },
                "b": { "type": "boolean" }
            }),
            json!([]),
        );
        let index = vec!["id".to_string(), "age".to_string(), "b".to_string()];
        assert_eq!(get_index_string_length(&schema, &index).expect("ok"), 4 + 3 + 1);
    }

    #[test]
    fn padding_is_measured_in_code_units_not_bytes() {
        // An emoji is two UTF-16 code units, so it occupies two columns of a
        // maxLength budget, exactly as `padEnd` sees it (PLAN.md 3.2).
        let schema = schema(json!({ "id": { "type": "string", "maxLength": 4 } }), json!([]));
        let encoded = get_indexable_string(&schema, &["id".to_string()], &json!({ "id": "😀" }))
            .expect("ok");
        assert_eq!(dxdb_jsstr::js_length(&encoded), 4);
        assert_eq!(encoded, "😀  ");
    }

    #[test]
    fn nested_field_paths_resolve() {
        let schema = schema(
            json!({
                "id": { "type": "string", "maxLength": 2 },
                "nes": { "type": "object", "properties": { "ted": { "type": "string", "maxLength": 3 } } }
            }),
            json!([]),
        );
        assert_eq!(
            get_indexable_string(
                &schema,
                &["nes.ted".to_string()],
                &json!({ "nes": { "ted": "a" } })
            )
            .expect("ok"),
            "a  "
        );
    }

    #[test]
    fn js_truthiness_matches_the_language_for_bound_conversion() {
        // `{}` and `[]` are truthy in JavaScript, unlike in most languages.
        assert!(js_truthy(&json!({})));
        assert!(js_truthy(&json!([])));
        assert!(!js_truthy(&json!("")));
        assert!(!js_truthy(&json!(0)));
        assert!(!js_truthy(&Value::Null));
    }
}
