//! The sort comparator: `getSortComparator` (`src/rx-query-helper.ts:186`).
//!
//! Used wherever documents must be ordered in memory rather than by a storage
//! index: after a `desc` sort forces a re-sort, after an index walk that only
//! partially satisfies the order, and by the top-k selection.
//!
//! # Two comparisons in one function
//!
//! Upstream has a fast path and a slow path, and the fast path is not an
//! optimisation detail — it defines the behaviour:
//!
//! - When both values are strings, or both numbers, compare with `<` and stop.
//!   For strings `<` is UTF-16 code-unit order, so this crate uses
//!   [`dxdb_jsstr::compare_js`] (PLAN.md 3.2). Rust's `str` comparison would order
//!   `"\u{1F600}"` *after* `"\u{FFFF}"`, which is backwards.
//! - Otherwise use mingo's cross-type `compare`, which orders by type first
//!   (`undefined < null < number < string < object < array < boolean`) and then
//!   within the type.
//!
//! The consequence of the second rule is that `{age: 1}` sorts *before*
//! `{age: 'x'}`, and a document missing the field sorts before one with `null`.
//!
//! # `undefined` results
//!
//! When every sort part ties, upstream's function falls off the end and returns
//! `undefined`. `Array.prototype.sort` treats that as `0`, so the array is left in
//! whatever order it had. Rust cannot express that, so a total tie returns
//! [`Ordering::Equal`], which has the same effect for a stable sort — and the sort
//! is stable (`sort_by` is, `sort_unstable_by` is not, and the latter is not used).
//! This is the one place where the Rust port has to interpret a JS quirk rather than
//! copy it, and the interpretation is the one the JS engine applies.

use std::cmp::Ordering;

use serde_json::Value;

use crate::matcher::mingo_compare;
use crate::normalize::{FilledMangoQuery, SortDirection};

/// A comparator produced by [`get_sort_comparator`].
///
/// A named type rather than a bare `fn` so a caller can store it, and so the
/// signature documents that documents are compared by reference rather than moved.
pub struct SortComparator {
    parts: Vec<SortPartCompiled>,
}

struct SortPartCompiled {
    field: String,
    direction: SortDirection,
}

impl SortComparator {
    /// Compares two documents. Returns [`Ordering::Equal`] when every sort part
    /// ties, which a stable sort leaves in place.
    pub fn compare(&self, a: &Value, b: &Value) -> Ordering {
        for part in &self.parts {
            let value_a = resolve_field(a, &part.field);
            let value_b = resolve_field(b, &part.field);
            if value_a == value_b {
                // `if (valueA !== valueB)`: a tie moves on to the next part.
                continue;
            }
            let direction = part.direction;
            let fast = match (&value_a, &value_b) {
                (Some(Value::String(x)), Some(Value::String(y))) => {
                    Some(dxdb_jsstr::compare_js(x, y))
                }
                (Some(Value::Number(_)), Some(Value::Number(_))) => value_a
                    .as_ref()
                    .and_then(Value::as_f64)
                    .and_then(|x| value_b.as_ref().and_then(Value::as_f64).map(|y| (x, y)))
                    .and_then(|(x, y)| x.partial_cmp(&y)),
                _ => None,
            };
            if let Some(ordering) = fast {
                return match direction {
                    SortDirection::Asc => ordering,
                    // `(valueA < valueB ? -1 : 1) * dirMultiplier` — so a `desc`
                    // part inverts the fast path.
                    SortDirection::Desc => ordering.reverse(),
                };
            }
            // The slow path: `direction === 'asc' ? mingoCmp(a, b) : mingoCmp(b, a)`.
            let (left, right) = match (&value_a, &value_b) {
                (Some(left), Some(right)) => (left, right),
                // One side missing: mingo's `compare` treats `undefined` as
                // sorting before everything except nothing at all, so a missing
                // field is modelled as JSON `null` here only for the place where a
                // value is needed. `None` vs `Some` is decided explicitly below.
                (None, Some(right)) => return compare_missing(direction, true, right),
                (Some(left), None) => return compare_missing(direction, false, left),
                (None, None) => continue,
            };
            let ordering = match direction {
                SortDirection::Asc => mingo_compare(left, right),
                SortDirection::Desc => mingo_compare(right, left),
            };
            return ordering;
        }
        Ordering::Equal
    }

    /// The sort parts, for diagnostics and for tests that assert the shape.
    pub fn parts(&self) -> Vec<(String, SortDirection)> {
        self.parts
            .iter()
            .map(|part| (part.field.clone(), part.direction))
            .collect()
    }
}

/// Compares a missing value against a present one.
///
/// In mingo `resolve` yields `undefined` for a missing path, and `mingoCmp` maps
/// `MISSING`/`undefined` to the lowest sort rank: `if (typeA == "undefined") return
/// -1; if (typeB == "undefined") return 1;`. So the missing side always sorts first,
/// before `null`.
fn compare_missing(direction: SortDirection, missing_is_left: bool, _other: &Value) -> Ordering {
    // Ascending: missing sorts first. Descending compares `(b, a)`, so the missing
    // side ends up last.
    match (missing_is_left, direction) {
        (true, SortDirection::Asc) => Ordering::Less,
        (true, SortDirection::Desc) => Ordering::Greater,
        (false, SortDirection::Asc) => Ordering::Greater,
        (false, SortDirection::Desc) => Ordering::Less,
    }
}

/// Resolves one sort field, returning `None` when the path is absent.
///
/// A mapped result (a path through an array of objects) yields an owned array; the
/// comparator is the one place that has to keep it alive, which is why this returns
/// an `Option<Value>` rather than a reference.
fn resolve_field(document: &Value, path: &str) -> Option<Value> {
    match crate::matcher::resolve(document, path) {
        crate::matcher::Resolved::Absent => None,
        crate::matcher::Resolved::Value(value) => Some(value.into_owned()),
        crate::matcher::Resolved::Mapped(items) => Some(Value::Array(items)),
    }
}

/// `getSortComparator` (`src/rx-query-helper.ts:186`).
///
/// The `_schema` argument upstream takes is unused there too — the sort parts carry
/// everything — so it is not part of this signature.
pub fn get_sort_comparator(query: &FilledMangoQuery) -> SortComparator {
    SortComparator {
        parts: query
            .sort
            .iter()
            .map(|part| SortPartCompiled {
                field: part.field.clone(),
                direction: part.direction,
            })
            .collect(),
    }
}
