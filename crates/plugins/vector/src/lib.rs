//! Vector distance and similarity functions.
//!
//! Ported from `src/plugins/vector/` at upstream `3b763ee17` (37 lines: `index.ts` 2,
//! `types.ts` 1, `vector-distance.ts` 34, and an empty `helper.ts`).
//!
//! # Four functions, and one of them is misnamed upstream
//!
//! `euclideanDistance`, `manhattanDistance` and `cosineSimilarity` are what their names say.
//! **`jaccardSimilarity` is not**: it computes `1 - |A ∩ B| / |A ∪ B|`, which is the Jaccard
//! *distance*, not the Jaccard index (similarity). Returns 0 for identical sets and 1 for
//! disjoint ones — the opposite of what "similarity" implies.
//!
//! This port keeps the **name and the behaviour** (`jaccard_similarity` returning a distance),
//! because the name is the observable API a ported call site uses and the upstream test file
//! asserts the distance semantics at `vector-distance.test.ts:78-99`. Renaming it to
//! `jaccard_distance` here would be tidier and would silently change meaning for anyone
//! reading both. The doc comment carries the warning instead, and
//! [`jaccard_distance`] exists as an accurately-named alias for new code.
//!
//! # Number semantics
//!
//! These are JS numbers. The arithmetic is plain `f64` in the same order as upstream's
//! `reduce`, because floating-point addition is not associative and summing in a different
//! order would change the low bits — which matters when a similarity is a threshold.
//! `dxdb-jsnum` is a dependency so a caller needing JS-exact *formatting* of a result (e.g.
//! for a persisted index string) has it without another dependency.
//!
//! # Length mismatch
//!
//! Upstream indexes `B[i]` with `i` from `A`, so a shorter `B` yields `undefined`, `a -
//! undefined` is `NaN`, and the whole result is `NaN`. A longer `B` is silently ignored:
//! only `A`'s length drives the loop. Both are reproduced rather than "fixed", because a
//! caller comparing vectors of unequal length has a bug and `NaN` is how upstream surfaces
//! it; silently truncating or panicking would hide it. [`euclidean_distance`] etc. therefore
//! return `f64` (which can be `NaN`), not `Option`/`Result`.

#![forbid(unsafe_code)]

/// A vector, as upstream's `type Vector = number[]` (`types.ts:1`).
pub type Vector = [f64];

/// `euclideanDistance` (`vector-distance.ts:11-13`).
///
/// `sqrt(Σ (aᵢ - bᵢ)²)`, summed in index order. `NaN` when `b` is shorter than `a`, matching
/// JS `undefined` arithmetic; extra elements of `b` are ignored.
pub fn euclidean_distance(a: &Vector, b: &Vector) -> f64 {
    let mut sum = 0.0;
    for (index, a_value) in a.iter().enumerate() {
        let difference = a_value - element_at(b, index);
        sum += difference * difference;
    }
    sum.sqrt()
}

/// `manhattanDistance` (`:14-16`).
///
/// `Σ |aᵢ - bᵢ|`, summed in index order.
pub fn manhattan_distance(a: &Vector, b: &Vector) -> f64 {
    let mut sum = 0.0;
    for (index, a_value) in a.iter().enumerate() {
        sum += (a_value - element_at(b, index)).abs();
    }
    sum
}

/// `cosineSimilarity` (`:19-24`).
///
/// `(A·B) / (|A|·|B|)`. Note upstream computes the two magnitudes with **separate** `reduce`
/// passes (one over `a`, one over `b`) rather than accumulating all three in one loop, and
/// this preserves that: the summation order is part of the result's low bits.
///
/// Returns `NaN` for an empty or zero-magnitude vector, because the division is `0/0`.
pub fn cosine_similarity(a: &Vector, b: &Vector) -> f64 {
    let mut dot_product = 0.0;
    for (index, a_value) in a.iter().enumerate() {
        dot_product += a_value * element_at(b, index);
    }
    let mut magnitude_a = 0.0;
    for a_value in a {
        magnitude_a += a_value * a_value;
    }
    let mut magnitude_b = 0.0;
    for b_value in b {
        magnitude_b += b_value * b_value;
    }
    dot_product / (magnitude_a.sqrt() * magnitude_b.sqrt())
}

/// `jaccardSimilarity` (`:30-37`) — which returns the Jaccard **distance**.
///
/// `1 - |A ∩ B| / |A ∪ B|`, over the *sets* of the two vectors, so duplicates are collapsed
/// (`Set` semantics). 0 for identical sets, 1 for disjoint.
///
/// Named as upstream names it; see the module docs for why the name is kept and why
/// [`jaccard_distance`] also exists.
pub fn jaccard_similarity(a: &Vector, b: &Vector) -> f64 {
    jaccard_distance(a, b)
}

/// The accurately-named alias for [`jaccard_similarity`], for new code.
pub fn jaccard_distance(a: &Vector, b: &Vector) -> f64 {
    // `new Set(a)` / `new Set(b)`: duplicates collapse. Upstream uses JS `Set`, whose
    // membership test is SameValueZero — so `-0` and `0` are one element and `NaN` matches
    // itself. `f64::to_bits` gives exactly that equivalence, with the one adjustment that
    // `-0.0` and `0.0` must normalise to the same key.
    use std::collections::BTreeSet;
    let set_a: BTreeSet<u64> = a.iter().map(set_key).collect();
    let set_b: BTreeSet<u64> = b.iter().map(set_key).collect();

    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        // `0/0` is `NaN` in JS, so `1 - NaN` is `NaN`. An empty-vs-empty comparison is the
        // only way here, and upstream returns NaN for it.
        return f64::NAN;
    }
    // `1 - (intersection / union)`. Integer division would be wrong; JS divides as f64.
    1.0 - (intersection as f64 / union as f64)
}

/// A `Set`-equivalent key: `-0.0` and `0.0` are one element, `NaN` equals itself.
fn set_key(value: &f64) -> u64 {
    if *value == 0.0 {
        // Both `-0.0` and `0.0`.
        0
    } else {
        value.to_bits()
    }
}

/// Reads `b[index]` the way JS does: a missing element is `undefined`, which makes any
/// arithmetic on it `NaN`.
fn element_at(b: &Vector, index: usize) -> f64 {
    match b.get(index) {
        Some(value) => *value,
        // `a - undefined` is NaN in JS. Returning NaN makes every accumulator NaN, which is
        // exactly upstream's result.
        None => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shorter_b_yields_nan_as_javascript_does() {
        // `[1,2,3]` against `[1]`: `2 - undefined` is NaN, so the sum is NaN.
        assert!(euclidean_distance(&[1.0, 2.0, 3.0], &[1.0]).is_nan());
        assert!(manhattan_distance(&[1.0, 2.0], &[1.0]).is_nan());
    }

    #[test]
    fn a_longer_b_is_ignored_as_javascript_does() {
        // Only `a`'s length drives the loop, so extra elements of `b` do not contribute.
        assert_eq!(euclidean_distance(&[1.0], &[1.0, 999.0]), 0.0);
        assert_eq!(manhattan_distance(&[1.0], &[1.0, 999.0]), 0.0);
    }

    #[test]
    fn an_empty_pair_is_nan_for_jaccard() {
        // JS: union size 0, `1 - (0/0)` is NaN.
        assert!(jaccard_similarity(&[], &[]).is_nan());
    }

    #[test]
    fn negative_zero_and_zero_are_one_set_element() {
        // JS `Set` uses SameValueZero, so `-0` and `0` must not count as two elements.
        assert_eq!(jaccard_similarity(&[-0.0, 0.0], &[0.0]), 0.0);
    }

    #[test]
    fn the_alias_is_the_same_function() {
        assert_eq!(
            jaccard_similarity(&[1.0, 2.0], &[2.0, 3.0]),
            jaccard_distance(&[1.0, 2.0], &[2.0, 3.0])
        );
    }
}
