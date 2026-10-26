//! Operator interactions: compound selectors on one field, against the upstream oracle.
//!
//! This is the second axis on top of `matcher_fuzz_differential.rs`. That family generates
//! *one* operator (or one nested clause) per field over random documents, so it covers the
//! operator *vocabulary*. It cannot systematically cover operators **composed on the same
//! field and the same document**, which is where the semantics interact rather than merely
//! intersect — and where a matcher that evaluates each operator independently against a
//! slightly different resolved value goes wrong.
//!
//! The corpus ([`support/operator-interactions.mjs`]) has 72 compound selectors in seven
//! groups:
//!
//! | group | what it pins |
//! | --- | --- |
//! | `interval` | `$gt` + `$lte` as a half-open interval, both ends, plus an inverted interval |
//! | `in-with-range` | `$in` intersected with a range, and `$in` + `$nin` on one field |
//! | `elemMatch-with-size` | `$elemMatch` on an element with `$size` on the array |
//! | `not-compound` | `$not` wrapping a *conjunction*, so it negates the whole thing |
//! | `three-way-state` | absent vs null vs a real value, per operator, and combined |
//! | `regex-with-range` | `$regex` + `$options` alongside a range on the same field |
//! | `cross-type-ordering` | ordering needs the same type, so a mixed corpus must not coerce |
//!
//! # The absent / null / undefined three-way
//!
//! This is the group the corpus exists for. Mango has three states for a field and every
//! operator treats them differently; the table below is what upstream does, and this test
//! asserts all of it rather than trusting a reading of the source:
//!
//! | selector | absent | null |
//! | --- | --- | --- |
//! | `$exists: true` | no | yes |
//! | `$exists: false` | yes | no |
//! | `$eq: null` | **yes** | yes |
//! | `$ne: null` | no | no |
//! | `$type: 'null'` | **no** | yes |
//! | `$type: 'undefined'` | yes | yes |
//! | `$gte: null` | **no** | yes |
//! | `$in: [null]` | **yes** | yes |
//! | `$not: {$exists: true}` | yes | no |
//!
//! The bolded cells are the asymmetries: `$eq`/`$in` treat absent as equal to null, while
//! `$type`/`$gte` do not. Rust's `Option<&Value>` collapses both into `None`, so a port that
//! models "resolved value" as an `Option` loses exactly the information these four operators
//! need. The corpus records `$state` per document so the test can assert the shape it built.
//!
//! `undefined` itself has no JSON representation, so it cannot appear in the corpus. The
//! `$type: 'undefined'` cases document what upstream does for it (it is `isNil`, so it
//! accepts both absent and null) and the port reproduces that without ever needing a third
//! runtime state.

use std::collections::BTreeSet;

use serde_json::Value;

use dxdb_query::Matcher;

const CORPUS: &str = include_str!("support/operator-interactions.json");

fn load() -> Value {
    serde_json::from_str(CORPUS).expect("operator-interactions.json is valid JSON")
}

/// The corpus is the size it claims and came from the pin.
#[test]
fn the_corpus_is_the_documented_size_and_provenance() {
    let corpus = load();
    let cases = corpus["cases"].as_array().expect("cases");
    let documents = corpus["documents"].as_array().expect("documents");

    assert_eq!(
        cases.len() as u64,
        corpus["caseCount"].as_u64().expect("caseCount")
    );
    assert_eq!(
        documents.len() as u64,
        corpus["documentCount"].as_u64().expect("documentCount")
    );
    assert_eq!(
        corpus["rxdbCommit"].as_str(),
        Some("3b763ee17d62bc96cfae556ea0ccf92f9891c8c1"),
        "the expectations must come from the pinned upstream commit"
    );
    assert_eq!(
        corpus["generator"].as_str(),
        Some("crates/dxdb-query/tests/support/operator-interactions.mjs")
    );

    // All seven groups must be present and non-empty; a family that lost a group would
    // silently stop covering that interaction.
    let declared: BTreeSet<&str> = corpus["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for required in [
        "interval",
        "in-with-range",
        "elemMatch-with-size",
        "not-compound",
        "three-way-state",
        "regex-with-range",
        "cross-type-ordering",
    ] {
        assert!(declared.contains(required), "group {required} missing");
        assert!(
            cases
                .iter()
                .any(|case| case["group"].as_str() == Some(required)),
            "group {required} is declared but has no cases"
        );
    }
    assert!(cases.len() >= 60, "expected 72 cases, got {}", cases.len());
}

/// Every document carries the state the three-way cases depend on.
#[test]
fn every_document_declares_its_state() {
    let corpus = load();
    for document in corpus["documents"].as_array().expect("documents") {
        let state = document["$state"]
            .as_str()
            .unwrap_or_else(|| panic!("document has no $state: {document}"));
        match state {
            "absent" => assert!(
                document.get("f").is_none(),
                "{}: state absent but `f` is present",
                document["id"]
            ),
            "null" => assert!(
                document.get("f").map(Value::is_null).unwrap_or(false),
                "{}: state null but `f` is not null",
                document["id"]
            ),
            "value" => assert!(
                document.get("f").is_some(),
                "{}: state value but `f` is absent",
                document["id"]
            ),
            other => panic!("unexpected $state {other:?}"),
        }
    }
    // All three states must actually be represented, or the three-way group is vacuous.
    let states: BTreeSet<&str> = corpus["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .filter_map(|document| document["$state"].as_str())
        .collect();
    assert_eq!(
        states,
        ["absent", "null", "value"]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "the corpus must contain all three field states"
    );
}

/// The agreement test: every compound selector, both directions, every document.
#[test]
fn compound_selectors_agree_with_the_upstream_oracle() {
    let corpus = load();
    let documents = corpus["documents"].as_array().expect("documents").clone();
    let cases = corpus["cases"].as_array().expect("cases");

    let mut compared = 0u32;
    let mut refusals = 0u32;
    let mut positive = 0u64;
    let mut negative = 0u64;
    let mut divergences: Vec<String> = Vec::new();

    for case in cases {
        let group = case["group"].as_str().unwrap_or("?");
        let why = case["why"].as_str().unwrap_or("");

        if case["kind"].as_str() == Some("rejection") {
            refusals += 1;
            if Matcher::new(&case["selector"]).is_ok() {
                divergences.push(format!(
                    "{group}: upstream refuses this selector ({}) but dxdb accepted it: {}",
                    case["rejected"].as_str().unwrap_or(""),
                    case["selector"]
                ));
            }
            continue;
        }

        let Some(expected_ids) = case["matchingIds"].as_array() else {
            continue;
        };
        let expected: BTreeSet<&str> = expected_ids.iter().filter_map(Value::as_str).collect();
        if case["matchCount"].as_u64().unwrap_or(u64::MAX) != expected.len() as u64 {
            divergences.push(format!("{group}: matchCount disagrees with matchingIds"));
            continue;
        }

        let matcher = match Matcher::new(&case["normalizedSelector"]) {
            Ok(matcher) => matcher,
            Err(error) => {
                divergences.push(format!(
                    "{group}: selector does not compile: {error}\n  selector: {}",
                    case["selector"]
                ));
                continue;
            }
        };

        for document in &documents {
            let id = document["id"].as_str().expect("id");
            let should_match = expected.contains(id);
            // `$state` is metadata for this test, not part of a stored document.
            let mut for_matcher = document.clone();
            if let Some(object) = for_matcher.as_object_mut() {
                object.remove("$state");
            }
            let did_match = matcher.matches(&for_matcher);
            if did_match == should_match {
                if should_match {
                    positive += 1;
                } else {
                    negative += 1;
                }
            } else {
                divergences.push(format!(
                    "{group} [{why}]: document {id} {} but upstream says it {}\n  selector: {}",
                    if did_match {
                        "matched"
                    } else {
                        "did not match"
                    },
                    if should_match { "does" } else { "does not" },
                    case["selector"]
                ));
            }
        }
        compared += 1;
    }

    assert!(positive > 0, "no positive assertions");
    assert!(negative > 0, "no negative assertions");
    assert!(
        divergences.is_empty(),
        "{} divergences across {compared} compound selectors ({positive} positive, {negative} \
         negative assertions):\n{}",
        divergences.len(),
        divergences.join("\n")
    );
    assert_eq!(
        compared + refusals,
        corpus["caseCount"].as_u64().unwrap_or_default() as u32,
        "every case must be compared or asserted refused"
    );
    assert_eq!(
        compared,
        corpus["comparisonCount"].as_u64().unwrap_or_default() as u32
    );
}

/// The three-way matrix, asserted cell by cell as an explicit table.
///
/// The agreement test would catch a wrong cell, but it would report it as "document X of
/// selector Y" — which does not say *which semantic* broke. This test states the matrix
/// directly, so a failure names the rule: "`$type: 'null'` must not match an absent field".
#[test]
fn the_absent_null_matrix_is_what_upstream_does() {
    let corpus = load();
    let documents: Vec<Value> = corpus["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .filter(|document| matches!(document["id"].as_str(), Some("absent" | "null")))
        .cloned()
        .collect();
    assert_eq!(documents.len(), 2, "need the absent and null documents");

    // (selector, absent matches?, null matches?). The comments are the rule being pinned.
    let matrix: Vec<(Value, bool, bool)> = vec![
        // Presence: the only pair of operators that partitions the two states.
        (serde_json::json!({"f": {"$exists": true}}), false, true),
        (serde_json::json!({"f": {"$exists": false}}), true, false),
        // `$eq`/`$in` treat an absent field as equal to null (mingo's isNil pair).
        (serde_json::json!({"f": {"$eq": null}}), true, true),
        (serde_json::json!({"f": {"$in": [null]}}), true, true),
        // `$ne`/`$nin` are their complements, so neither state matches.
        (serde_json::json!({"f": {"$ne": null}}), false, false),
        (serde_json::json!({"f": {"$nin": [null]}}), false, false),
        // `$type: 'null'` is `=== null`, so an absent field is *not* of type null —
        // unlike `$eq: null`. This asymmetry is the trap.
        (serde_json::json!({"f": {"$type": "null"}}), false, true),
        // `$type: 'undefined'` is `isNil`, so it accepts both.
        (serde_json::json!({"f": {"$type": "undefined"}}), true, true),
        // Ordering against null: both states resolve differently, and only the explicit
        // null is comparable to null.
        (serde_json::json!({"f": {"$gte": null}}), false, true),
        (serde_json::json!({"f": {"$lte": null}}), false, true),
        (serde_json::json!({"f": {"$gt": null}}), false, false),
        (serde_json::json!({"f": {"$lt": null}}), false, false),
        // `$not` negates, so it flips each cell.
        (
            serde_json::json!({"f": {"$not": {"$eq": null}}}),
            false,
            false,
        ),
        (
            serde_json::json!({"f": {"$not": {"$exists": true}}}),
            true,
            false,
        ),
        // Combining presence with equality narrows to the explicit null.
        (
            serde_json::json!({"f": {"$exists": true, "$eq": null}}),
            false,
            true,
        ),
    ];

    let mut checked = 0;
    for (selector, absent_expected, null_expected) in matrix {
        let matcher = Matcher::new(&selector)
            .unwrap_or_else(|error| panic!("{selector} does not compile: {error}"));
        for (document, expected) in documents.iter().zip([absent_expected, null_expected]) {
            let mut for_matcher = document.clone();
            if let Some(object) = for_matcher.as_object_mut() {
                object.remove("$state");
            }
            assert_eq!(
                matcher.matches(&for_matcher),
                expected,
                "{selector} on the {:?} document must be {expected}",
                document["$state"]
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 30, "the matrix must assert every cell");
}

/// `$not` wrapping a conjunction negates the whole thing, not one arm.
///
/// The case that distinguishes the two readings: `{$not: {$gt: 1, $lt: 3}}` must match a
/// value of `4` (the conjunction is false, so its negation is true) *and* a value of `1`
/// (also outside). A matcher that turned `$not` into per-operator negation would produce
/// `$lte: 1 OR $gte: 3`, which happens to agree here — so the discriminating input is a
/// value that satisfies exactly one arm: `2` satisfies both (inside), `1` satisfies neither.
/// The real discriminator is a value that satisfies one arm and not the other, which cannot
/// happen for a well-formed interval; so instead this asserts the interval directly against
/// `$not` on a union-shaped payload, where the readings do differ.
#[test]
fn not_negates_the_conjunction_not_each_arm() {
    // `{$not: {$in: [1,2]}}` is already covered by the corpus. The reading-sensitive shape
    // is a conjunction inside `$not` on a field whose values straddle it.
    let matcher = Matcher::new(&serde_json::json!({"f": {"$not": {"$gte": 1, "$lte": 2}}}))
        .expect("compiles");
    for (value, expected) in [
        (serde_json::json!({"f": 0}), true), // below the interval: conjunction false
        (serde_json::json!({"f": 1}), false), // lower bound: conjunction true
        (serde_json::json!({"f": 2}), false), // upper bound: conjunction true
        (serde_json::json!({"f": 3}), true), // above: conjunction false
        (serde_json::json!({"f": null}), true), // null fails both arms
        (serde_json::json!({}), true),       // absent fails both arms
    ] {
        assert_eq!(
            matcher.matches(&value),
            expected,
            "$not over [$gte 1, $lte 2] on {value}"
        );
    }
}

/// A `$regex` and a range on the same field must both hold.
///
/// `{f: {$regex: '^a', $options: 'i', $gt: 'a'}}` is a conjunction, so a value must both
/// match the pattern and be ordered above `'a'` — which is where `$options` interacting with
/// the ordering comparison (which is case-sensitive) becomes observable.
#[test]
fn regex_and_range_on_one_field_are_a_conjunction() {
    let matcher = Matcher::new(&serde_json::json!({
        "f": {"$regex": "^a", "$options": "i", "$gt": "a"}
    }))
    .expect("compiles");
    // 'alice' matches the regex and is > 'a'.
    assert!(matcher.matches(&serde_json::json!({"f": "alice"})));
    // 'ALICE' matches case-insensitively but is < 'a' in code-unit order ('A' is 0x41).
    assert!(
        !matcher.matches(&serde_json::json!({"f": "ALICE"})),
        "the case-insensitive regex must not make the case-sensitive ordering match"
    );
    // 'a' matches the regex but is not > 'a'.
    assert!(!matcher.matches(&serde_json::json!({"f": "a"})));
    // 'bob' fails the regex.
    assert!(!matcher.matches(&serde_json::json!({"f": "bob"})));
}

/// `undefined` is indistinguishable from absent, for every operator.
///
/// Mango's three states are absent, null and undefined, but only **two** are observable, and
/// this test pins why. Measured against upstream mingo 7.2.5 with a real JavaScript
/// `{f: undefined}` in the document:
///
/// ```text
/// selector              absent  null  undefined
/// $exists: true           no     yes      no
/// $exists: false         yes      no     yes
/// $eq: null              yes     yes     yes
/// $type: 'null'           no     yes      no
/// $type: 'undefined'     yes     yes     yes
/// $gte: null              no     yes      no
/// $in: [null]            yes     yes     yes
/// ```
///
/// `undefined` agrees with absent in every row. That is not an accident of mingo's
/// implementation: `resolve` cannot distinguish "key absent" from "key present-valued
/// undefined" for the same reason `obj.missing` and `obj.definedAsUndefined` both return
/// `undefined` in JavaScript, and an absent key also survives `'f' in obj` only as a
/// presence check that no Mango operator consults except `$exists` — where `undefined` reads
/// as absent because `resolve(...) !== undefined` is false for both.
///
/// The consequence for the port is that there is **no third state to model**. A document is
/// either present-at-a-path or not, and `Option<&Value>` is a complete representation. That is
/// worth stating explicitly because the opposite is widely assumed, and a port that invented
/// a third state would be modelling something Mango cannot express.
#[test]
fn undefined_is_indistinguishable_from_absent() {
    // What the port models: absent (None) and present (Some). The two rows of the matrix
    // above that matter are therefore "absent or undefined" and "explicit null".
    let absent = serde_json::json!({});
    let explicit_null = serde_json::json!({"f": null});

    // For each operator, the absent document behaves the same whether the absence came from a
    // missing key or from `undefined` — so a JSON document with the key omitted is a complete
    // stand-in for the undefined case, and no third state needs representing.
    for (selector, absent_expected, null_expected) in [
        (serde_json::json!({"f": {"$exists": true}}), false, true),
        (serde_json::json!({"f": {"$exists": false}}), true, false),
        (serde_json::json!({"f": {"$eq": null}}), true, true),
        (serde_json::json!({"f": {"$type": "null"}}), false, true),
        (serde_json::json!({"f": {"$type": "undefined"}}), true, true),
        (serde_json::json!({"f": {"$gte": null}}), false, true),
        (serde_json::json!({"f": {"$in": [null]}}), true, true),
    ] {
        let matcher = Matcher::new(&selector).expect("compiles");
        assert_eq!(
            matcher.matches(&absent),
            absent_expected,
            "{selector} on an absent field"
        );
        assert_eq!(
            matcher.matches(&explicit_null),
            null_expected,
            "{selector} on an explicit null"
        );
    }
}
