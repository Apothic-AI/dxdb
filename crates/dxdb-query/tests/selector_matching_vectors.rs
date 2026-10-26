//! Oracle tests: selector matching.
//!
//! `testdata/vectors/selector-matching.json` was produced by upstream RxDB 17.6.0 in Node:
//! a fixed corpus of 64 documents, 60 selectors with the ids each must match, and 3
//! selectors upstream *rejects*. It is the only oracle coverage of the matcher that is not
//! also entangled with planning, so it is the strongest evidence this crate has that the
//! Mango operator set behaves like upstream's.
//!
//! The family records the eighteen registered operators (`registeredOperators`), which is
//! the closed set from `src/types/rx-query.d.ts:43`. Everything else — `$all`, `$where`,
//! `$expr`, `$jsonSchema`, `$bitsAllSet`, `$geoWithin` — is *absent* upstream and must be
//! rejected here, not implemented: a port that accepted `$all` would run queries RxDB
//! cannot express and would diverge silently in the other direction too.
//!
//! The rejection cases also pin a subtlety worth its own assertion: `$not` is a
//! *field-level* operator in Mango. A top-level `$not` is rejected upstream, so it must be
//! rejected here, even though implementing it would be easy and would look like an
//! improvement.

use serde_json::Value;

use dxdb_query::{Matcher, QueryErrorCode};

fn load() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/vectors/selector-matching.json"
    );
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("cannot read {path}: {error}. The oracle vectors are committed.")
    });
    serde_json::from_str(&text).expect("selector-matching.json is valid JSON")
}

/// The document corpus every case is evaluated against.
fn corpus(vector: &Value) -> Vec<Value> {
    vector["corpus"].as_array().expect("corpus array").to_vec()
}

/// The generator's deterministic corpus must survive: a shrunken corpus would make every
/// case pass vacuously.
#[test]
fn the_corpus_is_the_documented_size() {
    let vector = load();
    let documents = corpus(&vector);
    assert_eq!(
        documents.len(),
        64,
        "the corpus is generated from a seeded xorshift of 64 documents; a different size \
         means the family changed shape"
    );
    // Every document must carry the id field the cases reference.
    for document in &documents {
        assert!(
            document.get("id").and_then(Value::as_str).is_some(),
            "every corpus document needs an id: {document}"
        );
    }
}

/// The registered operator set is the closed eighteen from `src/types/rx-query.d.ts:43`.
///
/// Asserted from the vector so a future family that quietly adds `$all` would be caught
/// here rather than becoming the port's behaviour.
#[test]
fn the_registered_operator_set_is_closed_and_small() {
    let vector = load();
    let registered: std::collections::BTreeSet<&str> = vector["registeredOperators"]
        .as_array()
        .expect("registeredOperators")
        .iter()
        .filter_map(Value::as_str)
        .collect();

    // The eighteen from `src/types/rx-query.d.ts:43`. `$options` is not in the list: it is
    // folded into `$regex` by mingo's `normalize` rather than registered as an operator.
    let documented: std::collections::BTreeSet<&str> = [
        "$and", "$elemMatch", "$eq", "$exists", "$gt", "$gte", "$in", "$lt", "$lte", "$mod",
        "$ne", "$nin", "$nor", "$not", "$or", "$regex", "$size", "$type",
    ]
    .into_iter()
    .collect();

    assert_eq!(
        registered, documented,
        "the registered operator set must be exactly the documented small set; extra \
         operators would be MongoDB parity RxDB does not have, missing ones would be a \
         selector dxdb cannot run"
    );
    assert_eq!(registered.len(), 18, "the set is eighteen operators, not a range");

    // And the deliberately absent operators, each of which exists in MongoDB. A port that
    // accepted one would run queries upstream rejects.
    for absent in [
        "$all", "$bitsAllClear", "$bitsAllSet", "$bitsAnyClear", "$bitsAnySet", "$comment",
        "$expr", "$geoIntersects", "$geoWithin", "$jsonSchema", "$near", "$nearSphere",
        "$text", "$where",
    ] {
        assert!(
            !registered.contains(absent),
            "{absent} is MongoDB's, not RxDB's; the set must stay closed"
        );
        let error = Matcher::new(&serde_json::json!({"field": {absent: [1]}}))
            .expect_err(&format!("{absent} must be rejected"));
        assert_eq!(
            error.code,
            QueryErrorCode::UnknownOperator,
            "{absent} must be rejected as an unknown operator"
        );
    }
}

/// The 60 match cases: every matching id, in corpus order.
#[test]
fn selectors_match_upstream_oracle() {
    let vector = load();
    let documents = corpus(&vector);
    let cases = vector["cases"].as_array().expect("cases array");
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in cases {
        // Rejection cases have no `matchingIds`; they are asserted separately.
        if case["kind"].as_str() == Some("rejection") || case.get("matchingIds").is_none() {
            continue;
        }
        let why = case["why"].as_str().expect("why");
        let selector = &case["selector"];
        // The vector records what normalisation produced; using it keeps the failure
        // attributable to the matcher rather than to normalisation, which the
        // normalised-queries family covers separately.
        let normalized = &case["normalizedSelector"];
        let expected: Vec<String> = case["matchingIds"]
            .as_array()
            .expect("matchingIds")
            .iter()
            .map(|id| id.as_str().expect("id").to_string())
            .collect();

        let matcher = match Matcher::new(normalized) {
            Ok(matcher) => matcher,
            Err(error) => {
                failures.push(format!(
                    "{why}: selector {selector} does not compile: {error}"
                ));
                continue;
            }
        };
        let actual: Vec<String> = documents
            .iter()
            .filter(|document| matcher.matches(document))
            .map(|document| document["id"].as_str().expect("id").to_string())
            .collect();

        if actual != expected {
            failures.push(format!(
                "{why}: selector {selector}\n  ours:     {actual:?}\n  upstream: {expected:?}"
            ));
            continue;
        }
        checked += 1;
    }

    assert!(
        failures.is_empty(),
        "{} selector-matching cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        checked >= 50,
        "expected at least 50 matching cases, checked {checked}"
    );
    assert_eq!(
        checked as u64,
        vector["matchingCaseCount"].as_u64().unwrap_or_default(),
        "matchingCaseCount must agree with the number of matching cases"
    );
}

/// The 3 rejection cases: upstream throws, so this crate must too.
///
/// - `not-outside-field` — a top-level `$not` is not a Mango operator; mingo rejects it as
///   an unknown top-level operator.
/// - `unregistered-$all` — `$all` exists in MongoDB but RxDB registers a closed set of
///   eighteen, so upstream throws rather than behaving like MongoDB.
/// - `unregistered-$where` — same, and `$where` additionally runs script.
#[test]
fn rejected_selectors_are_rejected_here() {
    let vector = load();
    let cases = vector["cases"].as_array().expect("cases array");
    let rejection_cases: Vec<&Value> = cases
        .iter()
        .filter(|case| case["kind"].as_str() == Some("rejection"))
        .collect();

    assert_eq!(
        rejection_cases.len() as u64,
        vector["rejectionCaseCount"].as_u64().unwrap_or_default(),
        "rejectionCaseCount must agree with the number of rejection cases"
    );
    assert!(
        vector["unexpectedRejections"]
            .as_array()
            .expect("unexpectedRejections")
            .is_empty(),
        "the generator recorded an unexpected rejection, which means a corpus document \
         changed shape"
    );

    let mut checked = 0;
    for case in &rejection_cases {
        let why = case["why"].as_str().expect("why");
        let selector = &case["selector"];
        let class = case["upstreamBehaviour"]["class"]
            .as_str()
            .unwrap_or_default();
        let matcher = Matcher::new(selector);
        let error = match matcher {
            Ok(_) => {
                panic!(
                    "{why} ({class}): upstream rejects this selector ({}) but dxdb accepted it: \
                     {selector}",
                    case["rawMessage"].as_str().unwrap_or_default()
                )
            }
            Err(error) => error,
        };
        // The kind of failure, and where it appears. For a top-level rejection the
        // operator is the selector's own key; for a field-level one it is the operator key
        // inside the field's object, not the field name.
        let object = selector.as_object().expect("selector object");
        let top_level_key = object.keys().next().map(String::as_str).unwrap_or_default();
        let nested_key = object
            .values()
            .find_map(|value| value.as_object())
            .and_then(|inner| inner.keys().next())
            .map(String::as_str)
            .unwrap_or_default();
        let operator = match class {
            "upstream-rejects-negative-logical-operator" => top_level_key,
            _ => {
                // `$where` sits at the top level, `$all` inside a field's object.
                if top_level_key.starts_with('$') && !top_level_key.is_empty() {
                    top_level_key
                } else {
                    nested_key
                }
            }
        };

        // The code must name the kind of failure, not just "invalid".
        match class {
            "upstream-rejects-negative-logical-operator" => {
                assert_eq!(
                    error.code,
                    QueryErrorCode::UnknownTopLevelOperator,
                    "{why}: a top-level logical operator must be rejected as a top-level \
                     operator, not as a field-level one"
                );
            }
            "upstream-does-not-register-operator" => {
                // The class says "not registered", not "not at this position", so the code
                // depends on where the operator sits. mingo rejects either way, with a
                // different message, and the port must reject with the code that names the
                // position: `$all` is a field-level operator RxDB never registers, `$where`
                // is a top-level one it never registers. Both are `Unknown*`, never a
                // silently-ignored key.
                let at_top_level = top_level_key == operator;
                let expected = if at_top_level {
                    QueryErrorCode::UnknownTopLevelOperator
                } else {
                    QueryErrorCode::UnknownOperator
                };
                assert_eq!(
                    error.code, expected,
                    "{why}: {operator} must be reported as unknown at the position it \
                     appears in"
                );
            }
            other => panic!("{why}: unmodelled rejection class {other:?}"),
        }
        // And the message names the offending operator, so a caller can act on it.
        assert!(
            error.message.contains(operator),
            "{why}: the error must name the operator {operator:?}; got {:?}",
            error.message
        );
        checked += 1;
    }
    assert_eq!(checked, rejection_cases.len());
}
