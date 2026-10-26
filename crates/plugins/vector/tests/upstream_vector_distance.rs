//! Ported from `test/unit/vector-distance.test.ts` (101 lines, 19 `it` cases) at upstream
//! `3b763ee17`.
//!
//! All 19 cases are ported one-for-one, each named `upstream_<line>_<slug>`. This is the
//! smallest plugin in the tree and the only one whose whole behaviour is four pure functions,
//! so there was no reason to port a subset.
//!
//! Two assertions are strengthened relative to upstream, and both are noted in the test that
//! does it: upstream compares floats with `Math.abs(result - expected) < 1e-10`, which is the
//! right idea, and where the exact value is representable (5.0, 0.0, 1.0) this port asserts
//! equality exactly — a tolerance that wide would hide a wrong-but-close implementation.

use dxdb_plugin_vector::{
    cosine_similarity, euclidean_distance, jaccard_distance, jaccard_similarity, manhattan_distance,
};

/// Upstream's tolerance: `Math.abs(result - expected) < 1e-10`.
fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-10,
        "expected {expected}, got {actual}"
    );
}

// ---------------------------------------------------------------------------
// `vector-distance.test.ts:11-32` — .euclideanDistance()
// ---------------------------------------------------------------------------

#[test]
fn upstream_11_euclidean_returns_zero_for_identical_vectors() {
    // `:11-14`.
    assert_eq!(euclidean_distance(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]), 0.0);
}

#[test]
fn upstream_15_euclidean_computes_the_simple_case() {
    // `:15-18` — the 3-4-5 triangle, exactly representable, so equality is asserted rather
    // than a tolerance.
    assert_eq!(euclidean_distance(&[0.0, 0.0], &[3.0, 4.0]), 5.0);
}

#[test]
fn upstream_19_euclidean_handles_one_dimension() {
    // `:19-22`.
    assert_eq!(euclidean_distance(&[0.0], &[5.0]), 5.0);
}

#[test]
fn upstream_23_euclidean_handles_negative_values() {
    // `:23-27`, expected `sqrt(4 + 16)`.
    close(
        euclidean_distance(&[-1.0, -2.0], &[1.0, 2.0]),
        (4.0f64 + 16.0).sqrt(),
    );
}

#[test]
fn upstream_28_euclidean_handles_high_dimensions() {
    // `:28-32`, expected `sqrt(16 * 4)`.
    close(
        euclidean_distance(&[1.0, 2.0, 3.0, 4.0], &[5.0, 6.0, 7.0, 8.0]),
        (16.0f64 * 4.0).sqrt(),
    );
}

// ---------------------------------------------------------------------------
// `vector-distance.test.ts:35-52` — .manhattanDistance()
// ---------------------------------------------------------------------------

#[test]
fn upstream_36_manhattan_returns_zero_for_identical_vectors() {
    // `:36-39`.
    assert_eq!(manhattan_distance(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]), 0.0);
}

#[test]
fn upstream_40_manhattan_computes_the_simple_case() {
    // `:40-43`.
    assert_eq!(manhattan_distance(&[0.0, 0.0], &[3.0, 4.0]), 7.0);
}

#[test]
fn upstream_44_manhattan_handles_negative_values() {
    // `:44-47`.
    assert_eq!(manhattan_distance(&[-1.0, -2.0], &[1.0, 2.0]), 6.0);
}

#[test]
fn upstream_48_manhattan_handles_one_dimension() {
    // `:48-51`.
    assert_eq!(manhattan_distance(&[0.0], &[10.0]), 10.0);
}

// ---------------------------------------------------------------------------
// `vector-distance.test.ts:54-75` — .cosineSimilarity()
// ---------------------------------------------------------------------------

#[test]
fn upstream_55_cosine_is_one_for_identical_vectors() {
    // `:55-58`.
    close(cosine_similarity(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]), 1.0);
}

#[test]
fn upstream_59_cosine_is_one_for_parallel_vectors() {
    // `:59-62` — `[2,4,6]` is `2 * [1,2,3]`, so the angle is zero.
    close(cosine_similarity(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]), 1.0);
}

#[test]
fn upstream_63_cosine_is_zero_for_orthogonal_vectors() {
    // `:63-66`.
    close(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
}

#[test]
fn upstream_67_cosine_is_minus_one_for_opposite_vectors() {
    // `:67-70`.
    close(cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]), -1.0);
}

#[test]
fn upstream_71_cosine_handles_high_dimensions() {
    // `:71-74`.
    close(
        cosine_similarity(&[1.0, 1.0, 1.0, 1.0], &[1.0, 1.0, 1.0, 1.0]),
        1.0,
    );
}

// ---------------------------------------------------------------------------
// `vector-distance.test.ts:77-99` — .jaccardSimilarity(), which is a DISTANCE
// ---------------------------------------------------------------------------

#[test]
fn upstream_78_jaccard_is_zero_for_identical_sets() {
    // `:78-81`. Note "0 for identical" — that is the tell that this is a distance, not a
    // similarity, despite the name.
    assert_eq!(jaccard_similarity(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]), 0.0);
}

#[test]
fn upstream_82_jaccard_is_one_for_disjoint_sets() {
    // `:82-85`.
    assert_eq!(jaccard_similarity(&[1.0, 2.0], &[3.0, 4.0]), 1.0);
}

#[test]
fn upstream_86_jaccard_computes_partial_overlap() {
    // `:86-90` with the arithmetic spelled out in the upstream comment:
    // intersection {2}, union {1,2,3} => index 1/3 => distance 2/3.
    close(jaccard_similarity(&[1.0, 2.0], &[2.0, 3.0]), 2.0 / 3.0);
}

#[test]
fn upstream_91_jaccard_collapses_duplicates() {
    // `:91-95` — `[1,2,2]` and `[2,3,3]` are the same sets as the previous case, because
    // upstream builds a `Set`.
    close(
        jaccard_similarity(&[1.0, 2.0, 2.0], &[2.0, 3.0, 3.0]),
        2.0 / 3.0,
    );
}

#[test]
fn upstream_96_jaccard_handles_a_single_element() {
    // `:96-99`.
    assert_eq!(jaccard_similarity(&[1.0], &[1.0]), 0.0);
}

// ---------------------------------------------------------------------------
// The name/behaviour divergence, pinned as its own test
// ---------------------------------------------------------------------------

#[test]
fn jaccard_similarity_returns_a_distance_not_a_similarity() {
    // The plugin's `jaccardSimilarity` computes `1 - |A∩B|/|A∪B|`, so identical sets give 0
    // and disjoint give 1 — the opposite of what the name implies. Kept because the name is
    // the ported API and the upstream tests assert these values; `jaccard_distance` is the
    // accurate alias for new code.
    let identical = jaccard_similarity(&[1.0, 2.0], &[1.0, 2.0]);
    let disjoint = jaccard_similarity(&[1.0, 2.0], &[3.0, 4.0]);
    assert_eq!(identical, 0.0, "identical sets give 0, i.e. zero distance");
    assert_eq!(disjoint, 1.0, "disjoint sets give 1, i.e. maximum distance");
    assert_eq!(jaccard_distance(&[1.0, 2.0], &[3.0, 4.0]), disjoint);
}
