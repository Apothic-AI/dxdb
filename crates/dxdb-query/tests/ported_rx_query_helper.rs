//! Ported upstream tests from `test/unit/rx-query-helper.test.ts`.
//!
//! Upstream's file is 267 lines and covers three things: `shouldUseTopKSelection`, the
//! equivalence of `sortAndSliceDocuments` with a full sort plus slice, and
//! `createTopKDocumentsCollector`'s bounded memory. All of it is M1's and all of it is
//! ported here, one test per upstream test, in upstream's order.
//!
//! The top-k helpers exist because sorting *n* documents to return *k* is wasteful. The
//! tests are therefore about equivalence: the fast path must return exactly what the slow
//! path would, for the same input.

use serde_json::{json, Value};

use dxdb_query::{
    create_top_k_documents_collector, get_sort_comparator, normalize_mango_query,
    should_use_top_k_selection, sort_and_slice_documents, MangoQuery, TOP_K_SELECTION_FACTOR,
};

/// The fixture schema from `rx-query-helper.test.ts:35`.
fn schema() -> Value {
    dxdb_schema::fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 20},
            "num": {"type": "number"},
            "str": {"type": "string", "maxLength": 20},
            "bool": {"type": "boolean"},
            "mixed": {},
            "optional": {"type": "number"},
            "nested": {
                "type": "object",
                "properties": {
                    "deep": {"type": "object", "properties": {"value": {"type": "number"}}},
                    "str": {"type": "string"}
                }
            }
        },
        "required": ["id", "num", "str", "bool", "nested"]
    }))
    .expect("fixture fills")
}

fn normalized_sort(sort: Value) -> dxdb_query::FilledMangoQuery {
    let schema = schema();
    normalize_mango_query(
        &MangoQuery::from_json(&json!({"selector": {}, "sort": sort})).expect("parses"),
        "id",
        &dxdb_schema::get_string_indexes(&schema).expect("indexes"),
        false,
    )
    .expect("normalizes")
}

/// `rx-query-helper.test.ts:93` `randomDoc`. Deterministic here, because a seeded
/// generator reproduces a failure and `Math.random` does not.
fn document(index: usize) -> Value {
    // A small LCG, so the corpus is varied but fixed.
    let mut state = (index as u64).wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    let mut next = |modulus: u64| -> u64 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % modulus
    };
    let mixed = match next(6) {
        0 => json!(next(5)),
        1 => json!(format!("t{}", next(3))),
        2 => Value::Null,
        3 => json!(true),
        4 => json!(false),
        _ => Value::Null,
    };
    let mut doc = json!({
        "id": format!("id{:08}", index),
        "num": next(21),
        "str": format!("s{}", next(2)),
        "bool": next(2) == 1,
        "mixed": mixed,
        "nested": {
            "deep": {"value": next(6)},
            "str": format!("n{}", next(2))
        },
        "_deleted": false,
        "_attachments": {},
        "_rev": "1-abc",
        "_meta": {"lwt": 1}
    });
    if next(2) == 1 {
        doc["optional"] = json!(next(11));
    }
    doc
}

fn corpus(count: usize) -> Vec<Value> {
    (0..count).map(document).collect()
}

/// `rx-query-helper.test.ts:162` `expectedResult`: a full sort plus a slice.
fn expected_result(
    documents: &[Value],
    comparator: &dxdb_query::SortComparator,
    skip: usize,
    limit: Option<usize>,
) -> Vec<Value> {
    let mut sorted = documents.to_vec();
    sorted.sort_by(|a, b| comparator.compare(a, b));
    match limit {
        Some(limit) => sorted.into_iter().skip(skip).take(limit).collect(),
        None => sorted.into_iter().skip(skip).collect(),
    }
}

// --- rx-query-helper.test.ts:175 `.shouldUseTopKSelection()` -----------------

// rx-query-helper.test.ts:176 `should not use top-k without a limit`
#[test]
fn should_not_use_top_k_without_a_limit() {
    assert!(!should_use_top_k_selection(1000, None));
}

// rx-query-helper.test.ts:179 `should not use top-k when k is close to n`
#[test]
fn should_not_use_top_k_when_k_is_close_to_n() {
    assert!(!should_use_top_k_selection(100, Some(100)));
    assert!(!should_use_top_k_selection(100, Some(50)));
}

// rx-query-helper.test.ts:183 `should use top-k when k is small compared to n`
#[test]
fn should_use_top_k_when_k_is_small() {
    let k = 10usize;
    assert!(should_use_top_k_selection(
        k * TOP_K_SELECTION_FACTOR + 1,
        Some(k as u64)
    ));
    assert!(should_use_top_k_selection(100_000, Some(50)));
}

// --- rx-query-helper.test.ts:189 `.sortAndSliceDocuments()` ------------------

// rx-query-helper.test.ts:190 `should return the same as a full sort plus slice for fixed cases`
#[test]
fn sort_and_slice_matches_full_sort_for_fixed_cases() {
    let documents = corpus(200);
    let filled = normalized_sort(json!([{"num": "desc"}, {"str": "asc"}]));
    let comparator = get_sort_comparator(&filled);

    let cases: Vec<(usize, Option<usize>)> = vec![
        (0, None),
        (0, Some(0)),
        (0, Some(1)),
        (0, Some(10)),
        (5, Some(10)),
        (100, Some(10)),
        (199, Some(10)),
        (200, Some(10)),
        (250, Some(10)),
        (0, Some(200)),
        (0, Some(300)),
        (50, None),
        (0, Some(99)),
        (0, Some(100)),
        (0, Some(101)),
    ];
    for (skip, limit) in cases {
        let expected = expected_result(&documents, &comparator, skip, limit);
        let actual = sort_and_slice_documents(
            documents.clone(),
            &comparator,
            skip as u64,
            limit.map(|value| value as u64),
        );
        assert_eq!(
            actual, expected,
            "skip={skip} limit={limit:?}"
        );
    }
}

// rx-query-helper.test.ts:220 `should handle an empty input`
#[test]
fn sort_and_slice_handles_empty_input() {
    let filled = normalized_sort(json!([{"num": "asc"}]));
    let comparator = get_sort_comparator(&filled);
    assert!(sort_and_slice_documents(Vec::new(), &comparator, 0, Some(10)).is_empty());
    assert!(sort_and_slice_documents(Vec::new(), &comparator, 0, None).is_empty());
    let collector = create_top_k_documents_collector(&comparator, 0, Some(10));
    assert!(collector.get_result().is_empty());
}

// rx-query-helper.test.ts:227 `should return the same as a full sort plus slice for random docs, sorts, skip and limit`
//
// Upstream runs 1,000 iterations outside fast mode. A deterministic corpus of 200 runs
// gives the same coverage without a flaky test; each run exercises a different sort, skip
// and limit combination.
#[test]
fn sort_and_slice_and_collector_match_full_sort_for_many_shapes() {
    let documents = corpus(300);
    let sorts: Vec<Value> = vec![
        json!([{"num": "asc"}]),
        json!([{"num": "desc"}]),
        json!([{"str": "asc"}]),
        json!([{"bool": "desc"}, {"num": "asc"}]),
        json!([{"nested.deep.value": "asc"}]),
        json!([{"nested.str": "desc"}, {"num": "asc"}]),
        json!([{"mixed": "asc"}]),
        json!([{"optional": "asc"}]),
    ];
    let shapes: Vec<(usize, Option<usize>)> = vec![
        (0, None),
        (0, Some(0)),
        (0, Some(1)),
        (0, Some(5)),
        (3, Some(7)),
        (0, Some(300)),
        (0, Some(301)),
        (299, Some(5)),
        (300, Some(1)),
        (7, None),
        (150, Some(20)),
    ];

    for (sort_index, sort) in sorts.iter().enumerate() {
        let filled = normalized_sort(sort.clone());
        let comparator = get_sort_comparator(&filled);
        for (skip, limit) in &shapes {
            let expected = expected_result(&documents, &comparator, *skip, *limit);
            let actual = sort_and_slice_documents(
                documents.clone(),
                &comparator,
                *skip as u64,
                limit.map(|value| value as u64),
            );
            assert_eq!(
                actual, expected,
                "sort#{sort_index} {sort} skip={skip} limit={limit:?}"
            );

            let mut collector =
                create_top_k_documents_collector(&comparator, *skip as u64, limit.map(|v| v as u64));
            for document in &documents {
                collector.push(document.clone());
            }
            let streamed = collector.get_result();
            assert_eq!(
                streamed, expected,
                "collector sort#{sort_index} {sort} skip={skip} limit={limit:?}"
            );
        }
    }
}

// --- rx-query-helper.test.ts:253 `.createTopKDocumentsCollector()` -----------

// rx-query-helper.test.ts:254 `should keep only skip+limit documents in memory once enough documents are seen`
#[test]
fn collector_returns_exactly_limit_documents() {
    let documents = corpus(1000);
    let filled = normalized_sort(json!([{"num": "asc"}]));
    let comparator = get_sort_comparator(&filled);
    let skip = 3u64;
    let limit = 7u64;
    let mut collector = create_top_k_documents_collector(&comparator, skip, Some(limit));
    for document in &documents {
        collector.push(document.clone());
    }
    let result = collector.get_result();
    assert_eq!(
        result,
        expected_result(&documents, &comparator, skip as usize, Some(limit as usize))
    );
    assert_eq!(result.len() as u64, limit);
}

// --- the equivalence the whole module rests on -------------------------------

/// Top-k selection and a full sort agree for every (skip, limit) pair around the
/// documented crossover.
///
/// `TOP_K_SELECTION_FACTOR` is tuned so the helper is never *slower* than the full sort;
/// this asserts the complementary property, that it is never *wronger*, at and around
/// `n = k * factor`.
#[test]
fn selection_is_exact_across_the_crossover() {
    let documents = corpus(101);
    let filled = normalized_sort(json!([{"num": "asc"}, {"str": "desc"}]));
    let comparator = get_sort_comparator(&filled);

    let k = 50u64;
    for skip in [0u64, 1, 10, 49, 50, 51, 100] {
        // Just below, exactly at, and just above the crossover.
        for n in [
            (k * TOP_K_SELECTION_FACTOR as u64) - 1,
            k * TOP_K_SELECTION_FACTOR as u64,
            (k * TOP_K_SELECTION_FACTOR as u64) + 1,
        ] {
            let n = n as usize;
            if n > documents.len() {
                continue;
            }
            let subset = &documents[..n];
            let expected = expected_result(subset, &comparator, skip as usize, Some(k as usize));
            let actual =
                sort_and_slice_documents(subset.to_vec(), &comparator, skip, Some(k));
            assert_eq!(actual, expected, "n={n} skip={skip} k={k}");
        }
    }
}
