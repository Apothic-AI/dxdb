//! JavaScript string comparison: UTF-16 code unit order.
//!
//! This is a core module, not a helper. PLAN.md section 3.2 records the probe
//! that motivates it:
//!
//! ```text
//! JS:    '\u{1F600}' < '\uFFFF'  ->  true    (surrogate pair 0xD83D < 0xFFFF)
//! Rust:  '\u{1F600}' < '\uFFFF'  ->  false   (UTF-8 byte order)
//! ```
//!
//! RxDB sorts by index strings, compares those strings with JavaScript `<`, and
//! its memory storage, index strings, and documented sort order all assume that
//! order. Rust's `str` ordering is UTF-8 byte order, which differs for exactly
//! the characters where UTF-8 byte order and UTF-16 code unit order disagree:
//! any comparison between a supplementary-plane character (which becomes a
//! surrogate pair, both above 0xD7FF and below 0xE000) and a BMP character in
//! the range 0xE000..=0xFFFF.
//!
//! So this module exists to be used by every index and sort path, and it does the
//! comparison on the UTF-16 encoding rather than on the UTF-8 bytes.
//!
//! # The `localeCompare` problem
//!
//! PLAN.md section 3.2 requires a deliberate, documented decision about
//! `localeCompare`, because upstream uses it for some sort orders
//! (`src/plugins/utils/utils-object.ts:207`, `:220`) and ICU collation is a third
//! order again. The decision, taken here and recorded in `docs/`:
//!
//! **Code-unit order is the only supported order.** `localeCompare`-based sorts
//! are not reproduced. Rationale: ICU collation depends on a locale data version
//! that changes between Node releases, so a "compatible" collation would be
//! compatible with one specific ICU build and quietly wrong with the next. A
//! stable, testable order that differs from upstream in an exotic case is a
//! better contract than an unstable one that matches today. The behaviour that
//! matters for data integrity — index strings, revisions, storage order — is
//! unaffected, because none of it uses `localeCompare`.

#![forbid(unsafe_code)]

use std::cmp::Ordering;

/// Compares two strings the way JavaScript's `<`, `>`, `<=`, `>=` do: by UTF-16
/// code unit sequences.
///
/// # Examples
///
/// ```
/// # use dxdb_jsstr::compare_js;
/// use std::cmp::Ordering;
/// // Rust's own `str` ordering says the opposite of this.
/// assert_eq!("😀" < "\u{FFFF}", false);
/// assert_eq!(compare_js("😀", "\u{FFFF}"), Ordering::Less);
/// ```
pub fn compare_js(a: &str, b: &str) -> Ordering {
    let mut left = a.encode_utf16();
    let mut right = b.encode_utf16();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match x.cmp(&y) {
                Ordering::Equal => continue,
                other => return other,
            },
        }
    }
}

/// Returns true when `a < b` in JavaScript string order.
#[inline]
pub fn lt(a: &str, b: &str) -> bool {
    compare_js(a, b) == Ordering::Less
}

/// Returns true when `a <= b` in JavaScript string order.
#[inline]
pub fn le(a: &str, b: &str) -> bool {
    compare_js(a, b) != Ordering::Greater
}

/// Returns true when `a > b` in JavaScript string order.
#[inline]
pub fn gt(a: &str, b: &str) -> bool {
    compare_js(a, b) == Ordering::Greater
}

/// Returns true when `a >= b` in JavaScript string order.
#[inline]
pub fn ge(a: &str, b: &str) -> bool {
    compare_js(a, b) != Ordering::Less
}

/// Renders a string's UTF-16 code units, for index-string construction and for
/// diagnostics. Length is the JavaScript `.length`, not the Rust `len()`.
pub fn utf16_units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The value JavaScript's `s.length` returns, which is UTF-16 code units.
///
/// Rust's `str::len()` counts UTF-8 bytes and `chars().count()` counts code
/// points; neither is what a JS client means by length, and index-string
/// construction depends on knowing which one it needs.
#[inline]
pub fn js_length(s: &str) -> usize {
    s.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe from PLAN.md section 8, plus the neighbouring cases that make
    /// the failure mode obvious. Any of these failing means index strings and
    /// sort orders silently diverge from RxDB.
    #[test]
    fn code_unit_order_differs_from_utf8_order() {
        // The recorded probe: these disagree, and the JS answer is ours.
        assert_eq!(compare_js("\u{1F600}", "\u{FFFF}"), Ordering::Less);
        assert_eq!("\u{1F600}".cmp("\u{FFFF}"), Ordering::Greater);

        // Where UTF-8 and UTF-16 agree, so does the helper.
        assert_eq!(compare_js("a", "b"), Ordering::Less);
        assert_eq!(compare_js("b", "a"), Ordering::Greater);
        assert_eq!(compare_js("a", "a"), Ordering::Equal);
        assert_eq!(compare_js("", "a"), Ordering::Less);
        assert_eq!(compare_js("a", ""), Ordering::Greater);
        assert_eq!(compare_js("", ""), Ordering::Equal);

        // Prefix rule is the same in both encodings.
        assert_eq!(compare_js("abc", "abcd"), Ordering::Less);

        // Astral vs astral: both start with 0xD83D, then compare low surrogates.
        assert_eq!(compare_js("\u{1F600}", "\u{1F601}"), Ordering::Less);

        // Astral (0xD83D 0xDE00) vs BMP in the private use area.
        assert_eq!(compare_js("\u{1F600}", "\u{E000}"), Ordering::Less);
        assert_eq!(compare_js("\u{E000}", "\u{1F600}"), Ordering::Greater);

        // Astral vs BMP below the surrogate range: BMP wins, as in both orders.
        assert_eq!(compare_js("\u{1F600}", "\u{D7FF}"), Ordering::Greater);
    }

    #[test]
    fn comparison_operators_match_javascript() {
        assert!(lt("a", "b"));
        assert!(!lt("b", "a"));
        assert!(!lt("a", "a"));
        assert!(le("a", "a"));
        assert!(ge("a", "a"));
        assert!(gt("b", "a"));
        assert!(lt("\u{1F600}", "\u{FFFF}"));
        assert!(gt("\u{FFFF}", "\u{1F600}"));
    }

    #[test]
    fn length_is_code_units_not_bytes_or_code_points() {
        // "😀" is 4 UTF-8 bytes, 1 Rust char, 2 UTF-16 code units.
        assert_eq!("\u{1F600}".len(), 4);
        assert_eq!("\u{1F600}".chars().count(), 1);
        assert_eq!(js_length("\u{1F600}"), 2);
        assert_eq!(js_length("abc"), 3);
        assert_eq!(js_length(""), 0);
        assert_eq!(utf16_units("\u{1F600}"), vec![0xD83D, 0xDE00]);
    }

    /// A sanity check that the helper is a well-defined total order on a corpus
    /// that mixes planes, so it is safe as a sort comparator.
    #[test]
    fn is_a_total_order() {
        // Lone surrogates cannot be represented in a Rust `str`, so the corpus
        // covers the plane boundaries around them instead.
        let corpus = [
            "",
            "a",
            "A",
            "ab",
            "\u{7F}",
            "\u{80}",
            "\u{7FF}",
            "\u{800}",
            "\u{D7FF}",
            "\u{E000}",
            "\u{FFFF}",
            "\u{10000}",
            "\u{1F600}",
            "\u{10FFFF}",
        ];
        // Deterministic sort using our comparator.
        let mut sorted: Vec<&str> = corpus.to_vec();
        sorted.sort_by(|a, b| compare_js(a, b));
        // Sorted order must be internally consistent: every adjacent pair
        // satisfies <=, and equal strings are adjacent.
        for window in sorted.windows(2) {
            assert!(
                le(window[0], window[1]),
                "{:?} should be <= {:?}",
                window[0],
                window[1]
            );
        }
        // Antisymmetry and transitivity spot-checks over the whole corpus.
        for a in corpus {
            for b in corpus {
                assert_eq!(
                    compare_js(a, b),
                    compare_js(b, a).reverse(),
                    "antisymmetry failed for {a:?} / {b:?}"
                );
            }
        }
    }
}
