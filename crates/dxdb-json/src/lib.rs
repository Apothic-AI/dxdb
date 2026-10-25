//! Ordered-JSON document encoding.
//!
//! PLAN.md section 3.1: RxDB sorts on index strings built from a deterministic
//! encoding of field values, and it compares whole index strings with JavaScript
//! `<`. Two consequences shape this crate:
//!
//! - **Key order matters** to anything that hashes a document (schema hash,
//!   schema identity, `_meta`, encryption AAD), so `serde_json` must be built
//!   with `preserve_order` and a document must never round-trip through a
//!   `BTreeMap`.
//! - **Number formatting matters** to anything that hashes or serialises a
//!   document, which is why numbers here are formatted by [`dxdb_jsnum`] and not
//!   by `serde_json`'s own writer.
//!
//! `serde_json` with `preserve_order` gets the ordering right but writes numbers
//! with Rust's `ryu` float formatting, which emits `1.0` where JavaScript emits
//! `1`. So this crate provides a writer that serialises a `serde_json::Value`
//! with JavaScript number formatting, and a canonicalisation function that
//! produces the exact bytes to hash.

#![forbid(unsafe_code)]

// Re-exported rather than only imported, because every consumer of this crate needs
// the value type to name a document and should not have to reach into serde_json to
// find it. A `use` here made `dxdb_json::Value` a private import and produced a
// confusing E0603 for the first crate that tried it.
pub use serde_json::{Map, Value};

pub use dxdb_jsnum;

/// Serialises a JSON value with JavaScript number formatting, preserving object
/// key order exactly as the value holds it.
///
/// Differences from `serde_json::to_string`:
///
/// - `Value::Number` holding a float renders via [`dxdb_jsnum::to_json_string`],
///   so `1.0` becomes `1` and `1e21` becomes `1e+21`, matching
///   `JSON.stringify`.
/// - Non-finite floats cannot appear in a `serde_json::Number` at all, so this
///   writer never has to emit `null` for them; the conversion is handled at the
///   boundary where a document is built.
/// - `Value::Number` holding an integer renders as the integer, unchanged.
///
/// # Examples
///
/// ```
/// # use dxdb_json::to_js_json;
/// use serde_json::json;
/// assert_eq!(to_js_json(&json!({"a": 1.0, "b": [1e21, true]})), r#"{"a":1,"b":[1e+21,true]}"#);
/// // serde_json's own writer disagrees on the float:
/// assert_eq!(serde_json::to_string(&json!({"a": 1.0})).unwrap(), r#"{"a":1.0}"#);
/// ```
pub fn to_js_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(f) = n.as_f64() {
                // `as_f64` always succeeds for a float-backed Number.
                out.push_str(&dxdb_jsnum::to_json_string(f));
            } else {
                // An integer too large for u64/i64 and not f64-backed cannot be
                // constructed through the public API, but falling back to the
                // standard writer is better than emitting nothing.
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => write_object(map, out),
    }
}

fn write_object(map: &Map<String, Value>, out: &mut String) {
    out.push('{');
    for (i, (key, value)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(key, out);
        out.push(':');
        write_value(value, out);
    }
    out.push('}');
}

/// Writes a JSON string literal using JavaScript's `JSON.stringify` escaping.
///
/// The escaping rules are the same as `serde_json`'s: control characters and
/// `"` and `\` are escaped, everything else is emitted as UTF-8. Keeping this
/// local means the writer above has no hidden dependency on serde's formatting.
fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Produces the canonical bytes to hash for a document: ordered-keys JSON with
/// JavaScript number formatting.
///
/// This is the function schema hashing and AAD binding must use. Two documents
/// that a JS client considers identical must produce identical bytes here, or
/// schema identity breaks across the boundary.
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    to_js_json(value).into_bytes()
}

/// Parses JSON while preserving object key order.
///
/// A thin wrapper that exists so call sites do not accidentally use a parser
/// with different semantics: key order is load-bearing (PLAN.md 3.1).
pub fn parse(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_format_like_javascript() {
        assert_eq!(to_js_json(&json!(1.0)), "1");
        assert_eq!(to_js_json(&json!(1.5)), "1.5");
        assert_eq!(to_js_json(&json!(1e21)), "1e+21");
        assert_eq!(to_js_json(&json!(1e20)), "100000000000000000000");
        assert_eq!(to_js_json(&json!(1e-7)), "1e-7");
        assert_eq!(to_js_json(&json!(0.0)), "0");
        // Integers are untouched.
        assert_eq!(to_js_json(&json!(42)), "42");
        assert_eq!(to_js_json(&json!(-42)), "-42");
        assert_eq!(to_js_json(&json!(u64::MAX)), u64::MAX.to_string());
    }

    #[test]
    fn key_order_is_preserved_through_parse_and_write() {
        let text = r#"{"z":1,"a":2,"m":3}"#;
        let value = parse(text).unwrap();
        assert_eq!(
            to_js_json(&value),
            text,
            "key order must survive a round trip"
        );

        // Nested objects too.
        let text = r#"{"outer":{"z":1,"a":{"y":2,"b":3}}}"#;
        let value = parse(text).unwrap();
        assert_eq!(to_js_json(&value), text);
    }

    #[test]
    fn strings_escape_like_javascript() {
        assert_eq!(to_js_json(&json!("a\"b")), r#""a\"b""#);
        assert_eq!(to_js_json(&json!("a\\b")), r#""a\\b""#);
        assert_eq!(to_js_json(&json!("\n\t\r")), r#""\n\t\r""#);
        assert_eq!(to_js_json(&json!("\u{1}")), r#""\u0001""#);
        // Non-ASCII is emitted literally, as JSON.stringify does.
        assert_eq!(to_js_json(&json!("héllo")), "\"héllo\"");
        assert_eq!(to_js_json(&json!("😀")), "\"😀\"");
    }

    #[test]
    fn round_trips_through_parse() {
        let original = json!({"a": [1, 2.5, "x", null, true], "b": {"n": 1e21}});
        let text = to_js_json(&original);
        let reparsed = parse(&text).unwrap();
        assert_eq!(reparsed, original, "text was {text}");
    }

    #[test]
    fn canonical_bytes_are_stable_and_order_sensitive() {
        let a = parse(r#"{"x":1,"y":2}"#).unwrap();
        let b = parse(r#"{"y":2,"x":1}"#).unwrap();
        // Same content, different order: different bytes. That is the point —
        // schema identity and AAD depend on order.
        assert_ne!(canonical_bytes(&a), canonical_bytes(&b));
        // Same input twice: identical bytes.
        assert_eq!(canonical_bytes(&a), canonical_bytes(&a.clone()));
    }
}
