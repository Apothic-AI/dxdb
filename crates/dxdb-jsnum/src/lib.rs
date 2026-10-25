//! ECMAScript `Number::toString` semantics.
//!
//! This is a core module, not a helper. PLAN.md section 3.1 is the reason: any
//! number that becomes a string can reach a stored document, an index string, or
//! a revision, and JavaScript's formatting differs from Rust's in ways that
//! silently corrupt data across a JS/Rust boundary:
//!
//! ```text
//! JS   String(1e21)          -> "1e+21"     Rust Display -> "1000000000000000000000"
//! JS   JSON.stringify(1.0)   -> "1"         Rust serde_json -> "1.0"
//! ```
//!
//! The rules implemented here are from ECMA-262 `Number::toString(x, 10)`:
//!
//! 1. NaN renders as `"NaN"`, and the infinities as `"Infinity"`/`"-Infinity"`.
//! 2. Negative zero renders as `"0"`.
//! 3. Otherwise the shortest digit string that round-trips is computed.
//! 4. Placement depends on `k` (digit count) and `n` (decimal exponent):
//!    - if `k <= n <= 21`: digits followed by `n - k` zeros, no exponent,
//!    - if `0 < n <= 21`: digits with a decimal point inserted after position `n`,
//!    - if `-6 < n <= 0`: `"0."` then `-n` zeros then the digits,
//!    - otherwise exponential form with an explicit sign.
//!
//! The threshold 21 and the threshold -6 are exactly where the two languages
//! disagree, so both are tested directly rather than trusted.

#![forbid(unsafe_code)]

/// Formats an `f64` the way `String(value)` does in JavaScript.
///
/// This is the function to use anywhere a number becomes bytes: index strings,
/// document serialization, schema hashing, revision formatting.
///
/// # Examples
///
/// ```
/// # use dxdb_jsnum::to_js_string;
/// assert_eq!(to_js_string(1.0), "1");
/// assert_eq!(to_js_string(-0.0), "0");
/// assert_eq!(to_js_string(1e21), "1e+21");
/// assert_eq!(to_js_string(1e20), "100000000000000000000");
/// ```
pub fn to_js_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        // Both +0 and -0 render as "0" in JS: String(-0) === "0".
        return "0".to_string();
    }

    let negative = value < 0.0;
    let mut out = String::new();
    if negative {
        out.push('-');
    }

    // `{:e}` gives the shortest round-tripping digits in the form "d.ddddde<exp>",
    // which is the representation ECMAScript specifies before placement.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("{:e} always emits an exponent");
    let exponent: i32 = exponent.parse().expect("{:e} emits an integer exponent");

    // `digits` is the shortest round-tripping digit string with no leading or
    // trailing zeros, `k` its length, and `n` the position of the decimal point
    // relative to its start (ECMA-262's k and n).
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exponent + 1;

    if k <= n && n <= 21 {
        // Integer with trailing zeros, no exponent.
        out.push_str(&digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        // Decimal point inside the digit string.
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        // Leading "0." then zeros.
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        // Exponential form with an explicit sign on the exponent.
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        let e = n - 1;
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&e.abs().to_string());
    }

    out
}

/// Formats a number the way `JSON.stringify` does when the number is the whole
/// document value: identical to [`to_js_string`] for every finite value.
///
/// It exists as a separate name because the call sites are different and a
/// reader should not have to reason about whether the two differ. They do not:
/// `JSON.stringify` delegates to `Number::toString` for numbers.
pub fn to_json_string(value: f64) -> String {
    if value.is_finite() {
        to_js_string(value)
    } else {
        // JSON has no NaN or Infinity; `JSON.stringify` emits null for them.
        "null".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases PLAN.md section 8 recorded from a real probe. These are also
    /// documentation of the divergence, so they stay as literals even though the
    /// full coverage lives in the generated oracle below.
    #[test]
    fn matches_the_documented_divergences() {
        let cases: &[(f64, &str)] = &[
            (1.0, "1"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1e-7, "1e-7"),
            (1e-6, "0.000001"),
            (-0.0, "0"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            (5e-324, "5e-324"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for (input, expected) in cases {
            assert_eq!(&to_js_string(*input), expected, "to_js_string({input:e})");
        }
    }

    /// The real specification: 300+ cases whose expected output was produced by
    /// running Node. Generated by `xtask/vectors/jsnum.mjs` and committed, so a
    /// wrong expectation cannot be laundered into a passing test.
    #[test]
    fn matches_generated_javascript_oracle() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../testdata/vectors/jsnum.json"
        );
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
            panic!(
                "cannot read {path}: {e}\n\
                 The jsnum oracle vectors are committed; if they are missing, run `make vectors`."
            )
        });
        let doc: serde_json::Value = serde_json::from_str(&text).expect("vectors are valid JSON");
        let cases = doc["cases"].as_array().expect("vectors have a cases array");
        assert!(cases.len() >= 200, "oracle lost cases: {}", cases.len());

        for case in cases {
            let input_text = case["input"].as_str().expect("input is a string");
            let expected_string = case["jsString"].as_str().expect("jsString is a string");
            let expected_json = case["jsJson"].as_str().expect("jsJson is a string");

            let value: f64 = match input_text {
                "NaN" => f64::NAN,
                "Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                other => other
                    .parse()
                    .unwrap_or_else(|e| panic!("input {other:?}: {e}")),
            };

            assert_eq!(
                to_js_string(value),
                expected_string,
                "String({input_text}) mismatch"
            );
            assert_eq!(
                to_json_string(value),
                expected_json,
                "JSON.stringify({input_text}) mismatch"
            );
        }
    }

    #[test]
    fn json_stringify_emits_null_for_non_finite() {
        assert_eq!(to_json_string(f64::NAN), "null");
        assert_eq!(to_json_string(f64::INFINITY), "null");
        assert_eq!(to_json_string(1.0), "1");
        assert_eq!(to_json_string(1e21), "1e+21");
    }

    /// Every finite f64 must round-trip through the formatted string, because
    /// the formatter is used to persist values that are read back as numbers.
    #[test]
    fn formatted_values_round_trip() {
        for bits in [
            1u64,
            2,
            3,
            0x3ff0_0000_0000_0001,
            0x7fef_ffff_ffff_ffff,
            0x0010_0000_0000_0000,
            0xc000_0000_0000_0000,
            0x4340_0000_0000_0001,
        ] {
            let value = f64::from_bits(bits);
            if !value.is_finite() {
                continue;
            }
            let text = to_js_string(value);
            let parsed: f64 = text
                .parse()
                .unwrap_or_else(|e| panic!("{text:?} (from {value:e}) does not parse back: {e}"));
            assert_eq!(parsed, value, "{text:?} did not round-trip {value:e}");
        }
    }

    /// The 21 and -6 branches share an index slice; guard the arithmetic with
    /// values right at each edge so an off-by-one cannot hide.
    #[test]
    fn boundary_arithmetic_does_not_panic() {
        for exp in -320..=308 {
            let value = 10f64.powi(exp);
            if value.is_finite() && value != 0.0 {
                let _ = to_js_string(value);
            }
        }
    }
}
