//! Index strings: the encoding RxDB sorts and scans by.
//!
//! Ported from `src/custom-index.ts`, with the constants from
//! `src/query-planner.ts`. Everything here is observable: an index string is
//! written into a storage row, compared with `<`, and used to open a scan range,
//! so a change to any of it is a change to the on-disk ordering.
//!
//! # The encoding
//!
//! An index string is the concatenation of one fixed-width part per index field,
//! in index order:
//!
//! | field type | encoding | width |
//! | --- | --- | --- |
//! | `string` | the value, right-padded with spaces to `maxLength` | `maxLength` |
//! | `boolean` | `"1"` for true, `"0"` for false | 1 |
//! | `number`, `integer` | zero-padded integer part, then zero-padded decimals | `nonDecimals + decimals` |
//!
//! Fixed width is what makes `a < b` on the concatenation equivalent to
//! lexicographic comparison of the field tuple. Every quirk below exists to keep
//! that property.
//!
//! # Numbers
//!
//! The width comes from the *schema*, not the value: `nonDecimals` is the length
//! of `String(ceil(max) - floor(min))` and `decimals` is the count of digits
//! after the decimal point in `multipleOf` (`getStringLengthOfIndexNumber`,
//! `src/custom-index.ts:214`).
//!
//! Three consequences that are easy to get wrong:
//!
//! 1. A value outside `[minimum, maximum]` **clamps** (`src/custom-index.ts:290`).
//!    A query like `{numField: {$gt: -1000}}` on a `minimum: 0` field would
//!    otherwise produce a negative number in a zero-padded field, which sorts
//!    before every real value and silently drops rows.
//! 2. The decimal part is `Math.round(frac * 10^decimals)`, clamped to
//!    `multiplier - 1`. Rounding *up* to the multiplier is therefore impossible:
//!    `0.99` with `decimals: 1` becomes `"9"`, i.e. the same string as `0.9`
//!    would be after rounding, not an extra carry digit that would widen the
//!    field (`src/custom-index.ts:308`).
//! 3. `Math.floor` is used for the integer part, so negatives truncate *down*:
//!    `-0.5` gives integer part `-1` and fraction `0.5`, which keeps the
//!    encoding monotonic across zero.
//!
//! # Bounds
//!
//! Range scans are opened with [`get_start_index_string_from_lower_bound`] and
//! [`get_start_index_string_from_upper_bound`], which fill the *unconstrained*
//! tail of the index with sentinels: `\0` for the string minimum and `' '` for a
//! present string, `"0"`/`"1"` for booleans, and all-zeros or all-nines for
//! numbers.
//!
//! The lower bound for strings uses `\x00`, not space. Upstream changed this
//! deliberately (`src/custom-index.ts:342`): strings may contain code points
//! below space (`\t`, `\n`), which sort *before* a space-padded bound and would
//! be silently excluded from results.

use serde_json::Value;

use crate::error::{QueryError, QueryErrorCode};

/// `INDEX_MAX` (`src/query-planner.ts:15`): the highest UTF-16 code unit.
///
/// Used as the "no upper bound" sentinel in query plans. It is a real string, not
/// a Rust `char::MAX`, because bounds are compared against index strings built
/// from UTF-16 code units.
pub const INDEX_MAX: &str = "\u{FFFF}";

/// `INDEX_MIN` (`src/query-planner.ts:25`): `Number.MIN_SAFE_INTEGER`, **not**
/// `-Infinity`.
///
/// Upstream's comment is the reason and it is worth keeping in view: `-Infinity`
/// becomes `null` under `JSON.stringify`, and a query plan is sent to a storage
/// worker as JSON in several plugins, where a `null` start key breaks the scan.
/// The value is deliberately a number, and deliberately outside any realistic
/// schema range.
pub const INDEX_MIN: i64 = -9_007_199_254_740_991;

/// The per-number-field encoding parameters, computed once per index.
///
/// Upstream calls this `ParsedLengths` (`src/custom-index.ts:202`) and precomputes
/// `multiplier` to avoid a `Math.pow` per document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParsedLengths {
    /// `Math.floor(minimum)`.
    pub minimum: f64,
    /// `Math.ceil(maximum)`.
    pub maximum: f64,
    /// Width of the integer part: `String(maximum - minimum).len()`.
    pub non_decimals: usize,
    /// Digits after the point in `multipleOf`.
    pub decimals: usize,
    /// Alias kept for parity with upstream's field name; equals `minimum`.
    pub rounded_minimum: f64,
    /// `10^decimals`, precomputed.
    pub multiplier: f64,
}

/// `getStringLengthOfIndexNumber` (`src/custom-index.ts:214`).
///
/// A schema part with no `minimum`/`maximum`/`multipleOf` cannot produce a
/// fixed-width encoding, so it is an error rather than a guess: an `Infinity`
/// bound upstream produced an index width of `Infinity` and every string of that
/// length (`rx-schema.test.ts:1515` documents the failure).
pub fn get_string_length_of_index_number(schema_part: &Value) -> Result<ParsedLengths, QueryError> {
    let minimum_raw = schema_part.get("minimum").and_then(Value::as_f64);
    let maximum_raw = schema_part.get("maximum").and_then(Value::as_f64);
    let multiple_of = schema_part.get("multipleOf").and_then(Value::as_f64);
    let (Some(minimum_raw), Some(maximum_raw), Some(multiple_of)) =
        (minimum_raw, maximum_raw, multiple_of)
    else {
        return Err(QueryError::new(
            QueryErrorCode::Ci2,
            "number index field needs minimum, maximum and multipleOf",
        ));
    };
    if !minimum_raw.is_finite() || !maximum_raw.is_finite() {
        return Err(QueryError::new(
            QueryErrorCode::Ci2,
            "number index field has a non-finite minimum or maximum",
        ));
    }
    let minimum = minimum_raw.floor();
    let maximum = maximum_raw.ceil();
    let value_span = maximum - minimum;
    // `valueSpan.toString().length` — JavaScript number formatting, so 1e21 is
    // an 5-character string in exponent form, not 22 digits (PLAN.md 3.1).
    let non_decimals = dxdb_jsnum::to_js_string(value_span).chars().count();
    let multiple_of_str = dxdb_jsnum::to_js_string(multiple_of);
    let decimals = match multiple_of_str.split_once('.') {
        Some((_, frac)) => frac.chars().count(),
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

/// `getNumberIndexString` (`src/custom-index.ts:274`).
///
/// See the module docs for the three clamping/rounding rules. `undefined` becomes
/// `0` before clamping, matching upstream's treatment of a missing field.
pub fn get_number_index_string(parsed_lengths: &ParsedLengths, field_value: Option<f64>) -> String {
    let mut value = field_value.unwrap_or(0.0);
    if value < parsed_lengths.minimum {
        value = parsed_lengths.minimum;
    }
    if value > parsed_lengths.maximum {
        value = parsed_lengths.maximum;
    }

    let integer_part = value.floor() - parsed_lengths.rounded_minimum;
    let non_decimals_value_as_string = dxdb_jsnum::to_js_string(integer_part);
    let mut out = pad_start(&non_decimals_value_as_string, parsed_lengths.non_decimals, '0');

    if parsed_lengths.decimals > 0 {
        let multiplier = parsed_lengths.multiplier;
        // `Math.round` on a negative fraction would round half away from zero,
        // but the fraction is always in [0, 1) here, so `f64::round` agrees.
        let shifted = ((value - value.floor()) * multiplier)
            .round()
            .min(multiplier - 1.0);
        let decimal_part = dxdb_jsnum::to_js_string(shifted);
        out.push_str(&pad_start(&decimal_part, parsed_lengths.decimals, '0'));
    }
    out
}

/// JavaScript's `String.prototype.padStart`.
///
/// Rust has no `pad_start`; `format!("{:0>width$}")` exists but does not handle
/// the "already longer than width" case the same way for sign characters, and
/// the width here is a runtime value from a schema.
fn pad_start(value: &str, width: usize, fill: char) -> String {
    let len = value.chars().count();
    if len >= width {
        return value.to_string();
    }
    let mut out = String::with_capacity(width);
    for _ in 0..(width - len) {
        out.push(fill);
    }
    out.push_str(value);
    out
}

/// JavaScript's `String.prototype.padEnd`.
fn pad_end(value: &str, width: usize, fill: char) -> String {
    let len = value.chars().count();
    if len >= width {
        return value.to_string();
    }
    let mut out = String::with_capacity(width);
    out.push_str(value);
    for _ in 0..(width - len) {
        out.push(fill);
    }
    out
}

/// One index field, pre-resolved against the schema.
///
/// Upstream's `IndexMetaField` (`src/custom-index.ts:38`) carries a closure per
/// field so building an index string is a chain of calls. Rust gets the same
/// shape without the closures: the schema part and parsed lengths are resolved
/// once, and [`IndexMetaField::index_string_part`] encodes one document's value.
#[derive(Debug, Clone)]
pub struct IndexMetaField {
    /// The field name as written in the index.
    pub field_name: String,
    /// The resolved sub-schema, used for `type`, `maxLength` and the bounds.
    pub schema_part: Value,
    /// Present only for `number` and `integer` fields.
    pub parsed_lengths: Option<ParsedLengths>,
    /// `type` from the schema part, as a string (`""` when absent).
    pub field_type: String,
    /// `maxLength` for string fields (`0` when absent).
    pub max_length: usize,
}

impl IndexMetaField {
    /// The fixed width this field contributes to an index string
    /// (`getIndexStringLength`, `src/custom-index.ts:239`).
    pub fn width(&self) -> usize {
        match self.field_type.as_str() {
            "string" => self.max_length,
            "boolean" => 1,
            _ => self
                .parsed_lengths
                .map(|p| p.non_decimals + p.decimals)
                .unwrap_or(0),
        }
    }

    /// Encodes one document's value for this field (`src/custom-index.ts:74`).
    ///
    /// A missing or empty string becomes all spaces, which is what makes an
    /// optional field sort *before* any present value (`custom-index.test.ts:268`
    /// is the upstream test for that).
    pub fn index_string_part(&self, document: &Value) -> String {
        let value = dxdb_schema::value_at_path(document, &self.field_name);
        match self.field_type.as_str() {
            "string" => {
                let text = value.and_then(Value::as_str).unwrap_or_default();
                pad_end(text, self.max_length, ' ')
            }
            "boolean" => {
                let flag = value.and_then(Value::as_bool).unwrap_or(false);
                if flag { "1" } else { "0" }.to_string()
            }
            _ => {
                let Some(parsed) = self.parsed_lengths else {
                    return String::new();
                };
                get_number_index_string(&parsed, value.and_then(Value::as_f64))
            }
        }
    }
}

/// `getIndexMeta` (`src/custom-index.ts:46`): resolves every field of an index.
///
/// An index field that is not in the schema is `CI1`, not a silently empty part:
/// an unrecognised field would otherwise produce a fixed-width run of spaces and
/// match every document.
pub fn index_meta(schema: &Value, index: &[String]) -> Result<Vec<IndexMetaField>, QueryError> {
    let mut fields = Vec::with_capacity(index.len());
    for field_name in index {
        let schema_part = dxdb_schema::schema_part_by_object_path(schema, field_name).ok_or_else(|| {
            QueryError::new(
                QueryErrorCode::Ci1,
                format!("index field `{field_name}` is not in the schema"),
            )
        })?;
        let field_type = schema_part
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Upstream's `getIndexMeta` resolves a per-field encoder, and its `else`
        // branch is the *number* encoder: `type === 'string'`, `type === 'boolean'`,
        // else assume number and read `minimum`/`maximum`/`multipleOf`. So a field of
        // any other type (`object`, `array`, an absent `type`, or a union like
        // `['string','null']`) fails while the monad is being built — which is
        // exactly what `testdata/vectors/index-strings.json::skipped` records for
        // `noStringIndex`:
        //
        //     monad failed: Cannot read properties of undefined (reading 'minimum')
        //
        // Encode the same rejection rather than guessing an encoding: an index over
        // a field type upstream cannot index must not silently produce a
        // fixed-width blank, because that would match every document.
        let parsed_lengths = match field_type.as_str() {
            "string" | "boolean" => None,
            "number" | "integer" => Some(get_string_length_of_index_number(schema_part)?),
            other => {
                return Err(QueryError::new(
                    QueryErrorCode::Ci2,
                    format!(
                        "index field `{field_name}` has type {other:?}, which has no index-string encoding"
                    ),
                ))
            }
        };
        let max_length = schema_part
            .get("maxLength")
            .and_then(Value::as_f64)
            .map(|v| v.max(0.0) as usize)
            .unwrap_or(0);
        fields.push(IndexMetaField {
            field_name: field_name.clone(),
            schema_part: schema_part.clone(),
            parsed_lengths,
            field_type,
            max_length,
        });
    }
    Ok(fields)
}

/// `getIndexableStringMonad` (`src/custom-index.ts:164`): the index string for
/// one document under one index.
///
/// Upstream returns a monad that closes over the resolved field metadata, and
/// specialises the 1-, 2- and 3-field cases to avoid loop overhead. The
/// specialisation is a performance detail with no observable effect, so it is not
/// reproduced; the resolved metadata is, because resolving it per call would put
/// a schema walk on the hot path of every insert.
pub fn get_indexable_string(
    schema: &Value,
    index: &[String],
    document: &Value,
) -> Result<String, QueryError> {
    let fields = index_meta(schema, index)?;
    Ok(fields
        .iter()
        .map(|field| field.index_string_part(document))
        .collect())
}

/// The width of a whole index string (`getIndexStringLength`,
/// `src/custom-index.ts:239`).
pub fn get_index_string_length(schema: &Value, index: &[String]) -> Result<usize, QueryError> {
    Ok(index_meta(schema, index)?
        .iter()
        .map(IndexMetaField::width)
        .sum())
}

/// `getPrimaryKeyFromIndexableString` (`src/custom-index.ts:263`).
///
/// The primary key is the last field of every index (that is what
/// `fillWithDefaultSettings` guarantees), so its padded form is the tail of the
/// string. Trimming the padding is safe because a primary key may not begin or end
/// with a space.
pub fn get_primary_key_from_indexable_string_with_length(
    indexable_string: &str,
    primary_key_length: usize,
) -> String {
    let units: Vec<char> = indexable_string.chars().collect();
    let start = units.len().saturating_sub(primary_key_length);
    units[start..].iter().collect::<String>().trim().to_string()
}

/// `getPrimaryKeyFromIndexableString` when the caller knows the schema.
pub fn get_primary_key_from_indexable_string(
    schema: &Value,
    primary_path: &str,
    indexable_string: &str,
) -> Result<String, QueryError> {
    let part = dxdb_schema::schema_part_by_object_path(schema, primary_path).ok_or_else(|| {
        QueryError::new(
            QueryErrorCode::Ci1,
            format!("primary key `{primary_path}` is not in the schema"),
        )
    })?;
    let max_length = part
        .get("maxLength")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            QueryError::new(
                QueryErrorCode::Ci2,
                format!("primary key `{primary_path}` has no maxLength"),
            )
        })? as usize;
    Ok(get_primary_key_from_indexable_string_with_length(
        indexable_string,
        max_length,
    ))
}

/// A value that may appear in a bound array: a real value, or one of the two
/// sentinels.
///
/// Upstream models this as the union `string | boolean | number | null |
/// undefined` and compares by identity against `INDEX_MIN`/`INDEX_MAX`. The
/// distinction between "the number `-9007199254740991`" and "the sentinel" is by
/// value upstream too, which is why this enum keeps them separate: a caller that
/// means the sentinel says so.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundValue {
    /// No value was supplied for this field (`undefined` upstream).
    Missing,
    /// An explicit `null`.
    Null,
    /// `INDEX_MIN`: open at the bottom.
    IndexMin,
    /// `INDEX_MAX`: open at the top.
    IndexMax,
    /// A string bound.
    String(String),
    /// A boolean bound.
    Bool(bool),
    /// A number bound.
    Number(f64),
}

impl BoundValue {
    /// Interprets a JSON value as a bound, recognising both sentinels.
    ///
    /// Upstream compares `bound === INDEX_MIN` and `bound === INDEX_MAX` by identity,
    /// which for a string is value equality and for a number is `===` — so a bound
    /// that happens to *equal* a sentinel is the sentinel, whichever field type it is
    /// being applied to. A string field bounded by `"\u{FFFF}"` therefore means
    /// "unbounded at the top", and a number field bounded by `"\u{FFFF}"` means the
    /// same rather than "no encoding". `testdata/vectors/index-strings.json` pins
    /// this: `lower-INDEX_MAX` expects the all-nines/maximum string even for a
    /// `_deleted` (boolean) first field.
    pub fn from_json(value: Option<&Value>) -> Self {
        match value {
            None => Self::Missing,
            Some(Value::Null) => Self::Null,
            Some(Value::String(s)) if s == INDEX_MAX => Self::IndexMax,
            Some(Value::String(s)) => Self::String(s.clone()),
            Some(Value::Bool(b)) => Self::Bool(*b),
            Some(Value::Number(n)) => match n.as_f64() {
                Some(f) if f == INDEX_MIN as f64 => Self::IndexMin,
                Some(f) => Self::Number(f),
                None => Self::Missing,
            },
            Some(_) => Self::Missing,
        }
    }
}

/// `getStartIndexStringFromLowerBound` (`src/custom-index.ts:318`).
pub fn get_start_index_string_from_lower_bound(
    schema: &Value,
    index: &[String],
    lower_bound: &[BoundValue],
) -> Result<String, QueryError> {
    let fields = index_meta(schema, index)?;
    let mut out = String::new();
    for (idx, field) in fields.iter().enumerate() {
        let bound = lower_bound.get(idx).cloned().unwrap_or(BoundValue::Missing);
        match field.field_type.as_str() {
            "string" => {
                let max_length = field.max_length;
                match bound {
                    BoundValue::String(ref s) if s == INDEX_MAX => {
                        out.push_str(&pad_end("", max_length, '\u{FFFF}'))
                    }
                    BoundValue::String(s) => out.push_str(&pad_end(&s, max_length, ' ')),
                    // The `\x00` decision from `src/custom-index.ts:342`.
                    _ => out.push_str(&pad_end("", max_length, '\u{0}')),
                }
            }
            "boolean" => match bound {
                BoundValue::Null | BoundValue::IndexMin | BoundValue::Missing => out.push('0'),
                BoundValue::IndexMax => out.push('1'),
                BoundValue::Bool(b) => out.push(if b { '1' } else { '0' }),
                // A non-boolean bound on a boolean field has no encoding; the
                // falsy branch is what upstream's `bound ? '1' : '0'` does.
                _ => out.push('0'),
            },
            _ => {
                let Some(parsed) = field.parsed_lengths else {
                    return Err(QueryError::new(
                        QueryErrorCode::Ci2,
                        format!("index field `{}` has no number encoding", field.field_name),
                    ));
                };
                let width = parsed.non_decimals + parsed.decimals;
                match bound {
                    BoundValue::Null | BoundValue::IndexMin => {
                        out.push_str(&"0".repeat(width))
                    }
                    BoundValue::IndexMax => {
                        out.push_str(&get_number_index_string(&parsed, Some(parsed.maximum)))
                    }
                    // A missing number bound goes through `getNumberIndexString`,
                    // which turns `undefined` into `0` and then *clamps it to the
                    // schema minimum* (`src/custom-index.ts:287`). It is therefore
                    // the minimum's encoding, not all zeros — the two differ when
                    // `minimum` is negative, and the oracle records both.
                    BoundValue::Missing => {
                        out.push_str(&get_number_index_string(&parsed, None))
                    }
                    BoundValue::Number(n) => {
                        out.push_str(&get_number_index_string(&parsed, Some(n)))
                    }
                    BoundValue::Bool(_) | BoundValue::String(_) => {
                        out.push_str(&"0".repeat(width))
                    }
                }
            }
        }
    }
    Ok(out)
}

/// `getStartIndexStringFromUpperBound` (`src/custom-index.ts:393`).
pub fn get_start_index_string_from_upper_bound(
    schema: &Value,
    index: &[String],
    upper_bound: &[BoundValue],
) -> Result<String, QueryError> {
    let fields = index_meta(schema, index)?;
    let mut out = String::new();
    for (idx, field) in fields.iter().enumerate() {
        let bound = upper_bound.get(idx).cloned().unwrap_or(BoundValue::Missing);
        match field.field_type.as_str() {
            "string" => {
                let max_length = field.max_length;
                match bound {
                    BoundValue::String(ref s) if s != INDEX_MAX => {
                        out.push_str(&pad_end(s, max_length, ' '))
                    }
                    // The `\x00` decision from `src/custom-index.ts:412`: a
                    // missing upper bound on a string field must not exclude
                    // strings starting below space.
                    BoundValue::IndexMin => out.push_str(&pad_end("", max_length, '\u{0}')),
                    _ => out.push_str(&pad_end("", max_length, '\u{FFFF}')),
                }
            }
            // `bound === null || bound === INDEX_MAX` is '1'; `bound === INDEX_MIN`
            // is '0'; everything else — including a missing bound — is
            // `bound ? '1' : '0'`, which is '0' (`src/custom-index.ts:426`).
            "boolean" => match bound {
                BoundValue::Null | BoundValue::IndexMax => out.push('1'),
                BoundValue::IndexMin | BoundValue::Missing => out.push('0'),
                BoundValue::Bool(b) => out.push(if b { '1' } else { '0' }),
                _ => out.push('0'),
            },
            _ => {
                let Some(parsed) = field.parsed_lengths else {
                    return Err(QueryError::new(
                        QueryErrorCode::Ci2,
                        format!("index field `{}` has no number encoding", field.field_name),
                    ));
                };
                let width = parsed.non_decimals + parsed.decimals;
                match bound {
                    BoundValue::Null | BoundValue::IndexMax => out.push_str(&"9".repeat(width)),
                    BoundValue::IndexMin => out.push_str(&"0".repeat(width)),
                    // A missing bound is not `null` and not a sentinel, so it takes
                    // the general path: `getNumberIndexString(parsed, undefined)`,
                    // i.e. zero clamped to the schema minimum
                    // (`src/custom-index.ts:447`). Where `minimum` is 0 that is
                    // indistinguishable from all zeros; where it is not, this is
                    // the difference between matching and dropping rows.
                    BoundValue::Missing => out.push_str(&get_number_index_string(&parsed, None)),
                    BoundValue::Number(n) => {
                        out.push_str(&get_number_index_string(&parsed, Some(n)))
                    }
                    BoundValue::Bool(_) | BoundValue::String(_) => out.push_str(&"9".repeat(width)),
                }
            }
        }
    }
    Ok(out)
}

/// `changeIndexableStringByOneQuantum` (`src/custom-index.ts:464`).
///
/// Used by storages that cannot express `inclusiveStart`/`inclusiveEnd` and have
/// to step a bound by one code unit instead.
pub fn change_indexable_string_by_one_quantum(input: &str, direction: i8) -> String {
    let mut chars: Vec<char> = input.chars().collect();
    if let Some(last) = chars.pop() {
        let code = last as u32 as i64 + direction as i64;
        if let Some(ch) = char::from_u32(code.clamp(0, 0x10FFFF) as u32) {
            chars.push(ch);
        }
    }
    chars.into_iter().collect()
}

