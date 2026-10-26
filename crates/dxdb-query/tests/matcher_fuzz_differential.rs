//! Differential fuzz: our matcher against upstream's, on generated input.
//!
//! The committed `selector-matching` family is a hand-designed corpus, which by
//! construction cannot cover the cases nobody thought to write down. This test consumes a
//! corpus that was *generated* — 160 random documents and 240 random selectors composed from
//! the eighteen registered operators — and whose expected answers came from upstream RxDB
//! running in Node (PLAN.md 7.3: "proptest for the selector matcher: generate documents and
//! selectors, assert our matcher agrees with the JS oracle").
//!
//! # This is a differential, not a self-check
//!
//! Every `matchingIds` list was produced by `getQueryMatcher(...)(document)` in upstream's
//! own code. Nothing in the generator consulted this crate. A corpus generated from our
//! matcher would launder a bug into a test, so the provenance matters more than the
//! coverage: `rxdbCommit` in the file is asserted against the pin.
//!
//! # Regenerating
//!
//! ```text
//! node crates/dxdb-query/tests/support/matcher-fuzz.mjs           # regenerate
//! node crates/dxdb-query/tests/support/matcher-fuzz.mjs --check   # verify committed copy
//! ```
//!
//! The generator imports lane C's `xtask/vectors/vendor.mjs`, so the oracle is the same
//! bundled upstream commit every other family uses. It writes inside this crate because
//! `xtask/vectors/` and `testdata/vectors/` are lane C's paths.
//!
//! # Why absence is recorded separately
//!
//! Each document carries `$present`, the list of fields actually present. JSON cannot express
//! "this key is absent" apart from "this key is null" once the key is gone, and
//! absent-versus-null is the classic Mango divergence — `$eq: null` matches both, `$gte: null`
//! matches only the explicit null. Recording presence lets the test assert that the document
//! the matcher saw had the same shape the generator built.

use std::collections::BTreeSet;

use serde_json::Value;

use dxdb_query::{Matcher, QueryError, QueryErrorCode};

/// The generated corpus, embedded at compile time.
///
/// Embedded rather than read from disk so the test cannot silently skip when the path is
/// wrong: a missing file is then a build error, not a green test with zero cases.
const CORPUS: &str = include_str!("support/matcher-fuzz.json");

fn load() -> Value {
    serde_json::from_str(CORPUS).expect("matcher-fuzz.json is valid JSON")
}

/// The corpus is the size it claims, and its provenance is the pin.
///
/// A truncated or hand-edited corpus is the failure mode that matters here: a fuzz family
/// that quietly shrinks still passes, and a family whose expectations were edited to match
/// our output is worse than none.
#[test]
fn the_corpus_is_the_documented_size_and_provenance() {
    let corpus = load();
    let documents = corpus["documents"].as_array().expect("documents");
    let cases = corpus["cases"].as_array().expect("cases");

    assert_eq!(
        documents.len() as u64,
        corpus["documentCount"].as_u64().expect("documentCount"),
        "documentCount disagrees with the array"
    );
    assert_eq!(
        cases.len() as u64,
        corpus["caseCount"].as_u64().expect("caseCount"),
        "caseCount disagrees with the array"
    );
    assert!(
        documents.len() >= 100,
        "the corpus must be large enough to be a fuzz corpus; got {}",
        documents.len()
    );
    assert!(
        cases.len() >= 200,
        "selector count too small: {}",
        cases.len()
    );

    // Provenance: the expectations came from the pinned upstream commit.
    assert_eq!(
        corpus["rxdbCommit"].as_str(),
        Some("3b763ee17d62bc96cfae556ea0ccf92f9891c8c1"),
        "the corpus must record the upstream commit its answers came from"
    );
    assert_eq!(corpus["rxdbVersion"].as_str(), Some("17.6.0"));

    // The generator is OURS and the answers are upstream's; assert the marker that
    // distinguishes this family from the committed vector families.
    assert_eq!(
        corpus["generator"].as_str(),
        Some("crates/dxdb-query/tests/support/matcher-fuzz.mjs")
    );
}

/// Every generated document carries the presence record the divergence cases depend on.
#[test]
fn every_document_records_which_fields_are_present() {
    let corpus = load();
    for document in corpus["documents"].as_array().expect("documents") {
        let present = document["$present"]
            .as_array()
            .unwrap_or_else(|| panic!("document has no $present: {document}"));
        let present: BTreeSet<&str> = present.iter().filter_map(Value::as_str).collect();
        for field in ["num", "text", "flag", "tags", "nested", "mixed"] {
            let actually_present = document.get(field).is_some();
            assert_eq!(
                actually_present,
                present.contains(field),
                "document {} says {:?} is {} present, but $present says otherwise",
                document["id"],
                field,
                if actually_present { "" } else { "not" }
            );
        }
    }
}

/// The corpus exercises all three match outcomes, or it is not testing much.
///
/// A fuzz corpus where every selector matches nothing (or everything) would pass against a
/// matcher that always answers the same way, so the shape of the distribution is asserted
/// before the agreement itself.
#[test]
fn the_corpus_exercises_zero_all_and_partial_matches() {
    let corpus = load();
    let total_documents = corpus["documents"].as_array().expect("documents").len();
    let mut zero = 0;
    let mut all = 0;
    let mut partial = 0;
    for case in corpus["cases"].as_array().expect("cases") {
        let Some(ids) = case["matchingIds"].as_array() else {
            continue;
        };
        match ids.len() {
            0 => zero += 1,
            count if count == total_documents => all += 1,
            _ => partial += 1,
        }
    }
    assert!(zero > 0, "no selector matches nothing");
    assert!(all > 0, "no selector matches everything");
    assert!(
        partial > 0,
        "no selector matches a proper subset, so a matcher cannot be distinguished from \
         a constant"
    );
    assert!(
        partial >= 20,
        "only {partial} partial-match selectors; the corpus is too easy to discriminate"
    );
}

/// The agreement test: every generated selector, both directions, every document.
///
/// This is the whole point of the file. Failures print the selector and the document so a
/// counterexample is actionable rather than a number.
#[test]
fn generated_selectors_agree_with_the_upstream_oracle() {
    let corpus = load();
    let documents = corpus["documents"].as_array().expect("documents").clone();
    let cases = corpus["cases"].as_array().expect("cases");

    let mut comparable = 0u32;
    let mut refusals = 0u32;
    let mut positive_assertions = 0u64;
    let mut negative_assertions = 0u64;
    let mut divergences: Vec<String> = Vec::new();

    for case in cases {
        let why = case["why"].as_str().unwrap_or("?");

        // A generated selector upstream refuses. The port must refuse it too: silently
        // answering "no matches" would turn a caller's bug into apparent empty data.
        if case["kind"].as_str() == Some("rejection") {
            refusals += 1;
            match Matcher::new(&case["selector"]) {
                Err(_) => {}
                Ok(_) => divergences.push(format!(
                    "{why}: upstream refuses this selector ({}) but dxdb accepted it: {}",
                    case["rejected"].as_str().unwrap_or(""),
                    case["selector"]
                )),
            }
            continue;
        }

        let Some(expected_ids) = case["matchingIds"].as_array() else {
            continue;
        };
        let expected: BTreeSet<&str> = expected_ids.iter().filter_map(Value::as_str).collect();
        // The recorded count must agree with the ids, or the file is internally inconsistent
        // and no answer could satisfy it.
        let recorded = case["matchCount"].as_u64().unwrap_or(u64::MAX);
        if recorded != expected.len() as u64 {
            divergences.push(format!(
                "{why}: matchCount {recorded} != {} ids",
                expected.len()
            ));
            continue;
        }

        // Built from the *normalised* selector, which is what upstream's matcher saw.
        let matcher = match Matcher::new(&case["normalizedSelector"]) {
            Ok(matcher) => matcher,
            Err(QueryError { code, message }) => {
                // A refusal here is a divergence too, unless the selector is one of the
                // recorded refusals (handled above).
                let _ = QueryErrorCode::UnknownOperator;
                divergences.push(format!(
                    "{why}: selector does not compile ({code:?}: {message}): {}",
                    case["selector"]
                ));
                continue;
            }
        };

        for document in &documents {
            let id = document["id"].as_str().expect("id");
            let should_match = expected.contains(id);
            // The matcher sees the document as generated, minus the bookkeeping key. `$present`
            // is metadata for this test, not part of the document a storage would hold.
            let mut for_matcher = document.clone();
            if let Some(object) = for_matcher.as_object_mut() {
                object.remove("$present");
            }
            let did_match = matcher.matches(&for_matcher);
            if did_match == should_match {
                if should_match {
                    positive_assertions += 1;
                } else {
                    negative_assertions += 1;
                }
            } else {
                let mut shape = for_matcher.clone();
                if let Some(object) = shape.as_object_mut() {
                    object.remove("_rev");
                }
                divergences.push(format!(
                    "{why}: document {id} {} but upstream says it {}\n  selector: {}\n  \
                     document: {}",
                    if did_match {
                        "matched"
                    } else {
                        "did not match"
                    },
                    if should_match { "does" } else { "does not" },
                    case["selector"],
                    shape
                ));
            }
        }
        comparable += 1;
    }

    // Both directions must have been exercised, or the run proves little.
    assert!(
        negative_assertions > 0,
        "no negative assertions: a matcher that returns true for everything would pass"
    );
    assert!(
        positive_assertions > 0,
        "no positive assertions: a matcher that returns false for everything would pass"
    );

    assert!(
        divergences.is_empty(),
        "{} divergences from the upstream oracle across {comparable} generated selectors \
         ({} positive and {} negative assertions):\n{}",
        divergences.len(),
        positive_assertions,
        negative_assertions,
        divergences.join("\n")
    );

    assert!(
        comparable >= 200,
        "expected 240 selectors, compared {comparable}"
    );
    assert_eq!(
        comparable + refusals,
        corpus["caseCount"].as_u64().unwrap_or_default() as u32,
        "every case must be either compared or asserted refused"
    );
}
