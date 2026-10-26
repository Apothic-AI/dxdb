//! Sort tie-breaking and stability, against the upstream oracle.
//!
//! Upstream sorts by index string and appends the primary key to every normalized sort, which
//! makes the order *total*: two documents with identical sort-field values still have a
//! deterministic relative order. RxDB's event-reduce and change-event comparison both depend
//! on that, so a tie broken differently is a divergence even when the *set* of results is the
//! same. This test asserts the full ordering, never just membership.
//!
//! Three tie shapes, each of which a Rust port gets wrong in a different way:
//!
//! 1. **Ties surviving a re-sort.** A `desc` anywhere forces an in-memory re-sort
//!    (`query-planner.ts:48`). Upstream appends the primary key as **`asc`** regardless of the
//!    other parts' directions, so a `desc` primary sort part does not invert the tie-break.
//! 2. **`1` versus `1.0`.** The same number in JavaScript, so the documents are tied and the
//!    primary key decides. `serde_json::Value` equality distinguishes `Number(1u64)` from
//!    `Number(1.0f64)`, so a comparator that decides ties with `Value ==` sees them as
//!    *different* and falls through to a comparison that can order them by representation.
//!    This corpus' `numberIdentity` block pins it directly.
//! 3. **`_meta.lwt` sharing a millisecond with differing fractions.** `1000`, `1000.0` and
//!    `1000.25` are three distinct values a storage must order, and the first two are the same
//!    value.
//!
//! Each case carries a pairwise comparison matrix as well as the total order, so a comparator
//! that is accidentally right for one corpus but has the wrong relation for a pair is caught.

use std::collections::BTreeMap;

use serde_json::Value;

use dxdb_query::{get_sort_comparator, normalize_mango_query, MangoQuery, SortDirection};

const CORPUS: &str = include_str!("support/sort-ties.json");

fn load() -> Value {
    serde_json::from_str(CORPUS).expect("sort-ties.json is valid JSON")
}

/// The schema the corpus was generated against.
fn schema() -> Value {
    dxdb_schema::fill_with_default_settings(&serde_json::json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": { "type": "string", "maxLength": 10 },
            "s": { "type": "string", "maxLength": 20 },
            "n": { "type": "number" },
            "lwt": { "type": "number" },
            "b": { "type": "boolean" },
            "m": { "type": "number" }
        },
        "required": ["id"],
        "indexes": ["s"]
    }))
    .expect("schema fills")
}

fn documents(corpus: &Value) -> BTreeMap<String, Vec<Value>> {
    corpus["corpora"]
        .as_object()
        .expect("corpora")
        .iter()
        .map(|(name, documents)| {
            (
                name.clone(),
                documents.as_array().expect("documents").clone(),
            )
        })
        .collect()
}

/// Builds the comparator for a recorded sort, via normalisation so the primary key is appended.
fn comparator_for(sort: &Value) -> dxdb_query::SortComparator {
    let schema = schema();
    let query = serde_json::json!({ "selector": {}, "sort": sort });
    let parsed = MangoQuery::from_json(&query).expect("query parses");
    let normalized = normalize_mango_query(
        &parsed,
        "id",
        &dxdb_schema::get_string_indexes(&schema).expect("indexes"),
        false,
    )
    .expect("normalizes");
    get_sort_comparator(&normalized)
}

/// The corpus is the size it claims and came from the pin.
#[test]
fn the_corpus_is_the_documented_size_and_provenance() {
    let corpus = load();
    let cases = corpus["cases"].as_array().expect("cases");
    assert_eq!(
        cases.len() as u64,
        corpus["caseCount"].as_u64().expect("caseCount")
    );
    assert_eq!(
        corpus["rxdbCommit"].as_str(),
        Some("3b763ee17d62bc96cfae556ea0ccf92f9891c8c1")
    );
    assert!(cases.len() >= 15, "expected 17 cases, got {}", cases.len());
    // Every corpus a case refers to must exist.
    let corpora = documents(&corpus);
    for case in cases {
        let name = case["corpus"].as_str().expect("corpus");
        assert!(
            corpora.contains_key(name),
            "case names corpus {name} which is not in `corpora`"
        );
    }
    assert!(corpora.len() >= 7, "expected 7 corpora, got {}", corpora.len());
}

/// Every sort ends with the primary key as an `asc` part.
///
/// Asserted separately because it is the mechanism: the tie-break exists only because
/// normalisation appends the primary key, and it is appended `asc` even when every other part
/// is `desc`. A port that inverted it would still produce a deterministic order — just the
/// wrong one.
#[test]
fn every_normalized_sort_appends_the_primary_key_as_ascending() {
    let corpus = load();
    let mut checked = 0;
    for case in corpus["cases"].as_array().expect("cases") {
        let sort = case["sort"].as_array().expect("sort");
        let normalized = case["normalizedSort"].as_array().expect("normalizedSort");
        let last = normalized.last().expect("normalized sort is non-empty");
        let (field, direction) = last
            .as_object()
            .and_then(|object| object.iter().next())
            .expect("last sort part");
        assert_eq!(field, "id", "{}: last part must be the primary key", case["why"]);

        // The rule, stated precisely: when the caller did NOT name the primary key, one
        // ascending part is appended. When the caller DID name it, nothing is appended and the
        // caller's direction survives — which is why `[{s:asc},{id:desc}]` stays `desc` here.
        let caller_has_pk = sort.iter().any(|part| {
            part.as_object()
                .map(|object| object.contains_key("id"))
                .unwrap_or(false)
        });
        if caller_has_pk {
            assert_eq!(
                normalized.len(),
                sort.len(),
                "{}: nothing is appended when the caller already sorts by the primary key",
                case["why"]
            );
            assert_eq!(
                direction.as_str(),
                sort.last()
                    .and_then(|part| part.as_object())
                    .and_then(|object| object.get("id"))
                    .and_then(Value::as_str),
                "{}: a caller-provided primary-key direction is kept",
                case["why"]
            );
        } else {
            assert_eq!(
                direction.as_str(),
                Some("asc"),
                "{}: the appended primary key is always ascending, even under desc",
                case["why"]
            );
            assert_eq!(
                normalized.len(),
                sort.len() + 1,
                "{}: exactly one part is appended",
                case["why"]
            );
        }
        checked += 1;
    }
    assert_eq!(checked, corpus["caseCount"].as_u64().unwrap_or_default());
}

/// The full ordering for every case.
#[test]
fn tie_orders_match_the_upstream_oracle() {
    let corpus = load();
    let corpora = documents(&corpus);
    let mut cases_checked = 0;
    let mut divergences = Vec::new();

    for case in corpus["cases"].as_array().expect("cases") {
        let why = case["why"].as_str().unwrap_or("?");
        let name = case["corpus"].as_str().expect("corpus");
        let documents = corpora.get(name).expect("corpus present");
        let expected: Vec<String> = case["sortedIds"]
            .as_array()
            .expect("sortedIds")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();

        let comparator = comparator_for(&case["sort"]);
        // Sort several times over differently-ordered inputs: a total order gives the same
        // answer regardless of the starting permutation, which a stable sort with a
        // non-total comparator does not.
        for (label, input) in [
            ("as-given", documents.clone()),
            ("reversed", documents.iter().rev().cloned().collect()),
            (
                "rotated",
                documents
                    .iter()
                    .cycle()
                    .skip(1)
                    .take(documents.len())
                    .cloned()
                    .collect(),
            ),
        ] {
            let mut sorted = input;
            sorted.sort_by(|a, b| comparator.compare(a, b));
            let actual: Vec<String> = sorted
                .iter()
                .map(|document| document["id"].as_str().expect("id").to_string())
                .collect();
            if actual != expected {
                divergences.push(format!(
                    "{why} [{label}]: ordering differs.\n  ours:     {actual:?}\n  upstream: {expected:?}"
                ));
            }
        }
        cases_checked += 1;
    }

    assert!(
        divergences.is_empty(),
        "{} divergences across {cases_checked} cases:\n{}",
        divergences.len(),
        divergences.join("\n")
    );
    assert_eq!(
        cases_checked,
        corpus["caseCount"].as_u64().unwrap_or_default() as usize
    );
}

/// The pairwise comparison matrix: the *relation* between two documents, not just the order.
///
/// A comparator can produce the right total order by accident while having the wrong relation
/// for a specific pair, and it is the relation that `event-reduce` and the top-k heap use.
#[test]
fn pairwise_comparisons_match_the_upstream_oracle() {
    let corpus = load();
    let corpora = documents(&corpus);
    let mut checked = 0u32;
    let mut divergences = Vec::new();

    for case in corpus["cases"].as_array().expect("cases") {
        let why = case["why"].as_str().unwrap_or("?");
        let name = case["corpus"].as_str().expect("corpus");
        let documents = corpora.get(name).expect("corpus present");
        let by_id: BTreeMap<&str, &Value> = documents
            .iter()
            .map(|document| (document["id"].as_str().expect("id"), document))
            .collect();
        let comparator = comparator_for(&case["sort"]);

        for pair in case["comparisons"].as_array().expect("comparisons") {
            let a = pair["a"].as_str().expect("a");
            let b = pair["b"].as_str().expect("b");
            let expected = pair["relation"].as_i64().expect("relation");
            let actual = match comparator.compare(by_id[a], by_id[b]) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            };
            if actual != expected {
                divergences.push(format!(
                    "{why}: compare({a}, {b}) = {actual}, upstream {expected}"
                ));
            }
            checked += 1;
        }
    }

    assert!(
        divergences.is_empty(),
        "{} of {checked} pairwise comparisons diverged:\n{}",
        divergences.len(),
        divergences.join("\n")
    );
    assert_eq!(
        checked as u64,
        corpus["comparisonCount"].as_u64().expect("comparisonCount")
    );
}

/// `1` and `1.0` are the same number, so documents holding them are tied.
///
/// This is the case a `serde_json::Value`-equality tie check gets wrong: `Number(1u64)` and
/// `Number(1.0f64)` are unequal as values, so the comparator would treat them as different
/// sort-field values rather than tied, and the primary key would never get to decide.
///
/// Measured upstream: comparing them on `n` alone returns `undefined` in both directions
/// (`numberIdentity.forwardRelation`), and under the normalized sort the primary key decides —
/// `float` before `int`.
#[test]
fn one_and_one_point_zero_are_tied() {
    let corpus = load();
    let identity = &corpus["numberIdentity"];

    // Upstream's answer, and the reason this test exists.
    assert_eq!(
        identity["tied"].as_bool(),
        Some(true),
        "the corpus records that upstream ties 1 and 1.0; if this changed, the generator changed shape"
    );
    assert_eq!(identity["forwardRelation"].as_str(), Some("undefined"));
    assert_eq!(identity["reverseRelation"].as_str(), Some("undefined"));

    // Rebuild the two documents from their raw JSON text, since only the source distinguishes
    // the representations.
    let pair = identity["pair"].as_array().expect("pair");
    let int_document: Value =
        serde_json::from_str(pair[0]["json"].as_str().expect("json")).expect("parses");
    let float_document: Value =
        serde_json::from_str(pair[1]["json"].as_str().expect("json")).expect("parses");
    // Assert the two really do have different representations, or the test proves nothing.
    assert_ne!(
        int_document["n"], float_document["n"],
        "serde_json must see two different representations for this test to be meaningful"
    );

    // On `n` alone they must tie: neither `Less` nor `Greater`.
    let n_only = comparator_for(&serde_json::json!([{"n": "asc"}]));
    assert_eq!(
        n_only.compare(&int_document, &float_document),
        std::cmp::Ordering::Equal,
        "1 and 1.0 hold the same JavaScript number, so the sort field ties"
    );
    assert_eq!(
        n_only.compare(&float_document, &int_document),
        std::cmp::Ordering::Equal,
        "and ties in both directions"
    );

    // Under the normalized sort the primary key decides, from either input order.
    let expected: Vec<String> = identity["sortedIds"]
        .as_array()
        .expect("sortedIds")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    assert_eq!(expected, vec!["float".to_string(), "int".to_string()]);
    let with_pk = comparator_for(&serde_json::json!([{"n": "asc"}]));
    for (label, mut input) in [
        ("int-first", vec![int_document.clone(), float_document.clone()]),
        ("float-first", vec![float_document.clone(), int_document.clone()]),
    ] {
        input.sort_by(|a, b| with_pk.compare(a, b));
        let actual: Vec<String> = input
            .iter()
            .map(|document| document["id"].as_str().expect("id").to_string())
            .collect();
        assert_eq!(
            actual, expected,
            "{label}: the primary key must break the tie identically either way"
        );
    }
}

/// A `desc` sort still breaks ties by ascending primary key.
///
/// The subtlety is that upstream appends `{id: 'asc'}` to a sort whatever the other parts say,
/// so `[{s: 'desc'}]` normalises to `[{s: 'desc'}, {id: 'asc'}]` — the tie-break is *not*
/// inverted. Asserted directly because a port that propagated the direction would be
/// plausibly right and observably wrong.
#[test]
fn desc_does_not_invert_the_primary_key_tie_break() {
    let corpus = load();
    let corpora = documents(&corpus);
    let string_ties = corpora.get("stringTies").expect("stringTies corpus");
    // Documents with s: 'same' are a, b, c, e; under `desc` they must come back a, b, c, e.
    let comparator = comparator_for(&serde_json::json!([{"s": "desc"}]));
    let mut tied: Vec<Value> = string_ties
        .iter()
        .filter(|document| document["s"].as_str() == Some("same"))
        .cloned()
        .collect();
    tied.sort_by(|a, b| comparator.compare(a, b));
    let ids: Vec<&str> = tied
        .iter()
        .map(|document| document["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ids,
        vec!["a", "b", "c", "e"],
        "under a desc sort the tied documents must still be in ascending primary-key order"
    );

    // Explicit `desc` on the primary key *does* invert, because the caller asked for it.
    let comparator = comparator_for(&serde_json::json!([{"s": "desc"}, {"id": "desc"}]));
    let mut tied: Vec<Value> = string_ties
        .iter()
        .filter(|document| document["s"].as_str() == Some("same"))
        .cloned()
        .collect();
    tied.sort_by(|a, b| comparator.compare(a, b));
    let ids: Vec<&str> = tied
        .iter()
        .map(|document| document["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec!["e", "c", "b", "a"]);
}

/// The comparator is a total order on every corpus: antisymmetric and transitive.
///
/// Asserted independently of the oracle, because a comparator can match every recorded pair
/// and still not be a total order (for instance if it returned `Equal` for incomparable
/// values). Sorting then depends on input order, which is exactly what event-reduce cannot
/// tolerate.
#[test]
fn the_comparator_is_antisymmetric_and_transitive() {
    let corpus = load();
    let corpora = documents(&corpus);
    let mut checked_pairs = 0u32;
    let mut checked_triples = 0u32;

    for case in corpus["cases"].as_array().expect("cases") {
        let name = case["corpus"].as_str().expect("corpus");
        let documents = corpora.get(name).expect("corpus present");
        let comparator = comparator_for(&case["sort"]);

        // Antisymmetry: sign(cmp(a,b)) == -sign(cmp(b,a)).
        for a in documents {
            for b in documents {
                let forward = comparator.compare(a, b);
                let backward = comparator.compare(b, a);
                assert_eq!(
                    forward,
                    backward.reverse(),
                    "{}: compare({}, {}) is not antisymmetric",
                    case["why"],
                    a["id"],
                    b["id"]
                );
                checked_pairs += 1;
            }
        }
        // Transitivity on triples, which is cheap enough for these corpus sizes.
        for a in documents {
            for b in documents {
                for c in documents {
                    let ab = comparator.compare(a, b);
                    let bc = comparator.compare(b, c);
                    let ac = comparator.compare(a, c);
                    if ab == std::cmp::Ordering::Less && bc == std::cmp::Ordering::Less {
                        assert_eq!(
                            ac,
                            std::cmp::Ordering::Less,
                            "{}: {} < {} < {} but not {} < {}",
                            case["why"],
                            a["id"],
                            b["id"],
                            c["id"],
                            a["id"],
                            c["id"]
                        );
                    }
                    checked_triples += 1;
                }
            }
        }
    }
    assert!(checked_pairs >= 300, "expected 382 pairs, got {checked_pairs}");
    assert!(checked_triples >= 2000, "transitivity triples: {checked_triples}");
}

/// A `desc` part anywhere is what forces a re-sort, and the plan says so.
///
/// `sortSatisfiedByIndex` must be false whenever any sort part is `desc`, because storages have
/// no descending index (`query-planner.ts:48`). This ties the sort work back to the planner flag
/// that decides whether the comparator runs at all.
///
/// Note the flag can also be false for an *ascending* sort when the index does not match it —
/// `fillWithDefaultSettings` puts `_deleted` first on every index, so `[{s: asc}]` normalises to
/// `[{s: asc}, {id: asc}]`, which the `[_deleted, s, id]` index does not satisfy. The assertion
/// is therefore one-directional: a `desc` anywhere forces false, never true.
#[test]
fn any_desc_part_forces_a_re_sort() {
    let schema = schema();
    let indexes = dxdb_schema::get_string_indexes(&schema).expect("indexes");
    let mut checked = 0;

    for sort in [
        serde_json::json!([{"s": "asc"}]),
        serde_json::json!([{"s": "desc"}]),
        serde_json::json!([{"s": "asc"}, {"n": "desc"}]),
        serde_json::json!([{"s": "desc"}, {"n": "asc"}]),
        serde_json::json!([{"_deleted": "asc"}, {"s": "asc"}]),
        serde_json::json!([{"_deleted": "desc"}, {"s": "asc"}]),
    ] {
        let query = serde_json::json!({"selector": {}, "sort": sort});
        let parsed = MangoQuery::from_json(&query).expect("parses");
        let normalized = normalize_mango_query(&parsed, "id", &indexes, false).expect("normalizes");
        let plan = dxdb_query::get_query_plan(&schema, &normalized).expect("plans");
        let has_desc = normalized
            .sort
            .iter()
            .any(|part| part.direction == SortDirection::Desc);
        if has_desc {
            assert!(
                !plan.sort_satisfied_by_index,
                "a desc part must force a re-sort, but the plan claims the index satisfies {sort}"
            );
        }
        // And the flag is never set when the sort does not lead with `_deleted`, since every
        // index does.
        let leads_with_deleted = normalized
            .sort
            .first()
            .map(|part| part.field == "_deleted")
            .unwrap_or(false);
        if !leads_with_deleted {
            assert!(
                !plan.sort_satisfied_by_index,
                "every index starts with _deleted, so a sort without it cannot be satisfied: {sort}"
            );
        }
        checked += 1;
    }
    assert_eq!(checked, 6);
}
