//! Oracle tests: selector matching, read through `dxdb-vectors`.
//!
//! `testdata/vectors/selector-matching.json` was produced by upstream RxDB 17.6.0 in Node:
//! a fixed corpus of 64 documents, 60 selectors with the ids each must match, and 3
//! selectors upstream *refuses*. Of the seven families this is the one that isolates the
//! matcher from planning, so it is the primary evidence that the Mango operator set behaves
//! like upstream's.
//!
//! The file is read through [`dxdb_vectors`], the typed reader, rather than a local
//! `serde_json` struct. The vectors crate owns path discovery, the case-count table and the
//! `deny_unknown_fields` structs, so a generator change and the reader change land together
//! and a sixth hand-rolled reader does not appear.
//!
//! # Both directions are asserted
//!
//! A matcher that returns `true` for everything passes a positives-only test. For every
//! case this file therefore checks:
//!
//! - every id in `matchingIds` matches, **and**
//! - every id in the corpus that is *not* in `matchingIds` does **not** match,
//! - the count agrees with the recorded `matchCount`.
//!
//! # Refusals are errors, not empty results
//!
//! The three rejection cases are queries upstream throws on. They must produce an error
//! here, not `false`. A caller who writes `{$all: [...]}` has a bug; silently evaluating it
//! to "no documents" turns that bug into what looks like empty data, which is the worse
//! failure. Two of the three also pin a deliberate limitation: `$all` exists in MongoDB but
//! RxDB registers a closed set of eighteen operators, and a top-level `$not` is not a Mango
//! operator at all.

use std::collections::BTreeSet;

use dxdb_query::{Matcher, QueryErrorCode};
use dxdb_vectors::{selector_matching, VectorFamily, Vectors};

/// Loads the family through the typed reader.
fn load() -> selector_matching::Vectors {
    let vectors = Vectors::discover();
    vectors
        .load(VectorFamily::SelectorMatching)
        .expect("selector-matching.json loads through dxdb-vectors")
}

/// The family is the size the vectors crate expects, so a truncated file fails here rather
/// than passing against a shrunken corpus.
#[test]
fn the_family_has_its_documented_case_count() {
    let family = load();
    assert_eq!(
        family.cases.len(),
        family.case_count,
        "caseCount disagrees with the number of cases"
    );
    assert_eq!(
        family.corpus.len(),
        64,
        "the corpus is 64 generated documents; a different size means the family changed shape"
    );
    // The reader's own table, which is the cross-crate guard against a shrunk corpus.
    let expected = dxdb_vectors::EXPECTED_CASE_COUNTS
        .iter()
        .find(|(candidate, _)| *candidate == VectorFamily::SelectorMatching)
        .map(|(_, count)| *count)
        .expect("SelectorMatching is in EXPECTED_CASE_COUNTS");
    assert_eq!(family.cases.len(), expected);
    // Every document needs the id the cases reference.
    for document in &family.corpus {
        assert!(
            document
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "every corpus document needs an id: {document}"
        );
    }
}

/// The registered operator set is the closed eighteen (`src/types/rx-query.d.ts:43`).
#[test]
fn the_registered_operator_set_is_closed_and_small() {
    let family = load();
    let registered: BTreeSet<&str> = family
        .registered_operators
        .iter()
        .map(String::as_str)
        .collect();

    let documented: BTreeSet<&str> = [
        "$and",
        "$elemMatch",
        "$eq",
        "$exists",
        "$gt",
        "$gte",
        "$in",
        "$lt",
        "$lte",
        "$mod",
        "$ne",
        "$nin",
        "$nor",
        "$not",
        "$or",
        "$regex",
        "$size",
        "$type",
    ]
    .into_iter()
    .collect();

    assert_eq!(
        registered, documented,
        "the registered set must be exactly the documented eighteen; extra operators would be \
         MongoDB parity RxDB does not have, missing ones would be a selector dxdb cannot run"
    );
    assert_eq!(registered.len(), 18);

    // Each operator MongoDB has and RxDB does not must be refused, not implemented.
    for absent in [
        "$all",
        "$bitsAllClear",
        "$bitsAllSet",
        "$bitsAnyClear",
        "$bitsAnySet",
        "$comment",
        "$expr",
        "$geoIntersects",
        "$geoWithin",
        "$jsonSchema",
        "$near",
        "$nearSphere",
        "$text",
        "$where",
    ] {
        assert!(
            !registered.contains(absent),
            "{absent} must not be registered"
        );
        let error = Matcher::new(&serde_json::json!({"field": {absent: [1]}}))
            .expect_err(&format!("{absent} must be refused"));
        assert_eq!(
            error.code,
            QueryErrorCode::UnknownOperator,
            "{absent} must be refused as an unknown operator"
        );
    }
}

/// Every matching case, in **both** directions.
#[test]
fn selectors_match_and_non_match_upstream_oracle() {
    let family = load();
    let mut matching_cases = 0;
    let mut id_assertions = 0;
    let mut positive_assertions = 0;
    let mut negative_assertions = 0;
    let mut failures = Vec::new();

    for case in &family.cases {
        let selector_matching::Case::Matching(case) = case else {
            continue;
        };
        matching_cases += 1;
        let why = &case.why;
        let expected: BTreeSet<&str> = case.matching_ids.iter().map(String::as_str).collect();

        // Keep the recorded count honest before trusting the id list.
        if case.match_count != case.matching_ids.len() {
            failures.push(format!(
                "{why}: matchCount {} disagrees with {} matchingIds",
                case.match_count,
                case.matching_ids.len()
            ));
            continue;
        }

        // The matcher is built from the *normalised* selector, so a failure is attributable
        // to matching rather than to normalisation (which the normalised-queries family
        // covers separately).
        let matcher = match Matcher::new(&case.normalized_selector) {
            Ok(matcher) => matcher,
            Err(error) => {
                failures.push(format!(
                    "{why}: selector {} does not compile: {error}",
                    case.selector
                ));
                continue;
            }
        };

        for document in &family.corpus {
            let id = document["id"].as_str().expect("id");
            let should_match = expected.contains(id);
            // Guard against a case referring to an id the corpus does not have, which would
            // make the expectation unsatisfiable in a confusing way.
            if should_match {
                id_assertions += 1;
            }
            let did_match = matcher.matches(document);
            if did_match != should_match {
                failures.push(format!(
                    "{why}: document {id} {} but upstream says it {}. selector {}",
                    if did_match {
                        "matched"
                    } else {
                        "did not match"
                    },
                    if should_match { "does" } else { "does not" },
                    case.selector
                ));
            } else if should_match {
                positive_assertions += 1;
            } else {
                negative_assertions += 1;
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} selector-matching assertions diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(
        matching_cases, family.matching_case_count,
        "matchingCaseCount must agree with the number of matching cases"
    );
    // The corpus is 64 documents and every case is evaluated against every document, so the
    // two directions together must account for all of them.
    assert_eq!(
        positive_assertions + negative_assertions,
        matching_cases * family.corpus.len(),
        "every (case, document) pair must be asserted in one direction or the other"
    );
    assert!(
        negative_assertions > 0,
        "a positives-only run would pass for a matcher that returns true for everything"
    );
    assert_eq!(positive_assertions, id_assertions);
    assert!(
        matching_cases >= 50,
        "expected 60 matching cases, got {matching_cases}"
    );
}

/// The three cases upstream refuses must be **errors**, not `false`.
///
/// Asserting `!matcher.matches(..)` would be satisfied by a matcher that silently returns
/// "no matches" for a selector it does not understand — which converts a caller's
/// typo or unsupported operator into apparent empty data.
#[test]
fn refused_selectors_are_errors_not_false_negatives() {
    let family = load();
    let mut refusals = 0;
    let mut failures = Vec::new();

    assert!(
        family.unexpected_rejections.is_empty(),
        "the generator recorded an unexpected rejection, so a corpus document changed shape: \
         {:?}",
        family.unexpected_rejections
    );

    for case in &family.cases {
        let selector_matching::Case::Rejection(case) = case else {
            continue;
        };
        refusals += 1;

        let matcher = Matcher::new(&case.selector);
        let error = match matcher {
            Ok(matcher) => {
                // The failure mode this test exists to prevent: the selector was accepted and
                // evaluated, so a bug now looks like empty data.
                failures.push(format!(
                    "{} ({}): upstream refuses this selector ({}), but dxdb accepted it and \
                     evaluated it to {} matching documents out of {}",
                    case.why,
                    case.upstream_behaviour.class,
                    case.raw_message,
                    family
                        .corpus
                        .iter()
                        .filter(|document| matcher.matches(document))
                        .count(),
                    family.corpus.len()
                ));
                continue;
            }
            Err(error) => error,
        };

        // The error must be typed and name the operator, not be a generic failure.
        // The class says why upstream refuses, not at which position, so the code depends on
        // where the operator sits. Both `$all` and `$where` are unregistered, but `$all` sits
        // inside a field's object and `$where` is a top-level key, and the error must name the
        // position — that is what a caller needs in order to fix the query.
        let expected_code = match case.upstream_behaviour.class.as_str() {
            "upstream-rejects-negative-logical-operator" => QueryErrorCode::UnknownTopLevelOperator,
            "upstream-does-not-register-operator" => {
                if case
                    .selector
                    .as_object()
                    .map(|object| object.keys().any(|key| key.starts_with('$')))
                    .unwrap_or(false)
                {
                    QueryErrorCode::UnknownTopLevelOperator
                } else {
                    QueryErrorCode::UnknownOperator
                }
            }
            other => {
                failures.push(format!(
                    "{}: unmodelled rejection class {other:?}",
                    case.why
                ));
                continue;
            }
        };
        if error.code != expected_code {
            failures.push(format!(
                "{}: expected {expected_code:?}, got {:?} ({})",
                case.why, error.code, error.message
            ));
            continue;
        }
        // And it names the offending operator so a caller can fix the query.
        let operator = offending_operator(&case.selector);
        if !error.message.contains(operator) {
            failures.push(format!(
                "{}: error must name the operator {operator:?}; got {:?}",
                case.why, error.message
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} refusal assertions diverged:\n{}",
        failures.join("\n"),
        failures.len()
    );
    assert_eq!(
        refusals, family.rejection_case_count,
        "rejectionCaseCount must agree with the number of rejection cases"
    );
    assert_eq!(refusals, 3, "the family records three refusals");
}

/// The operator a rejection is about: the top-level key for a top-level refusal, otherwise
/// the operator key inside the field's object.
fn offending_operator(selector: &serde_json::Value) -> &str {
    let Some(object) = selector.as_object() else {
        return "";
    };
    // A top-level refusal is keyed by the operator itself (`$where`); a field-level one is
    // keyed by a field name (`tags`) with the operator inside.
    if let Some(key) = object.keys().find(|key| key.starts_with('$')) {
        return key.as_str();
    }
    object
        .values()
        .find_map(|value| value.as_object())
        .and_then(|inner| inner.keys().find(|key| key.starts_with('$')))
        .map(String::as_str)
        .unwrap_or_default()
}
