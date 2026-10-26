//! `$options` / `$regex` flag parity, against the upstream oracle.
//!
//! JavaScript regex flags are a [`RegexFlags`] struct in the port rather than inline `(?i)`
//! syntax, and `$options` is where a caller supplies them. Whether `$options: 'i'` behaves like
//! upstream cannot be answered by reading the source — it needs JavaScript's answers — so the
//! expectations in [`support/regex-flags.json`] were produced by running mingo (which calls
//! `new RegExp(pattern, options)`) over a document corpus.
//!
//! # The two failure modes this exists for
//!
//! 1. **A silently ignored flag.** If `i` were dropped, `{f: {$regex: '^alice$', $options: 'i'}}`
//!    would stop matching `ALICE`. The query returns *fewer documents* and it looks like missing
//!    data rather than a bug. Every accepted flag therefore gets an assertion where flipping it
//!    changes the result.
//! 2. **A flag string upstream rejects and we accept.** The worse direction: a caller learns
//!    their query works here and it fails in a browser. Every invalid string must be an **error**,
//!    never a silent no-match.
//!
//! # The flag grammar, measured
//!
//! ```text
//! accepted     ""  d  g  i  m  s  u  v  y  and combinations (gimsuy, dv, du, ims, ...)
//! error        a repeated flag        ii  uu  vv  gg  dd  mm  ss  yy
//! error        u and v together       uv  vu   (mutually exclusive)
//! error        x  X  I  z             (there is no `x` flag in ECMAScript)
//! ```
//!
//! `x` is worth calling out because it is a common assumption: PCRE and Python have an
//! extended/verbose `x` flag, ECMAScript does not, and `new RegExp('a', 'x')` throws. Accepting
//! it — even as a no-op — would make dxdb accept a query RxDB rejects.

use std::collections::BTreeSet;

use serde_json::Value;

use dxdb_query::{Matcher, RegexFlags};

const CORPUS: &str = include_str!("support/regex-flags.json");

fn load() -> Value {
    serde_json::from_str(CORPUS).expect("regex-flags.json is valid JSON")
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
    assert_eq!(
        corpus["generator"].as_str(),
        Some("crates/dxdb-query/tests/support/regex-flags.mjs")
    );
    assert!(
        cases.len() >= 50,
        "expected 53 cases, got {}",
        cases.len()
    );
    // Every group present, so dropping one is a failure rather than a silent gap.
    let declared: BTreeSet<&str> = corpus["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for required in ["none", "i", "m", "s", "u", "d", "g-y", "combined", "invalid"] {
        assert!(declared.contains(required), "group {required} missing");
    }
}

/// Every accepted `$options` string matches exactly what upstream matches.
#[test]
fn accepted_flag_strings_match_the_upstream_oracle() {
    let corpus = load();
    let documents = corpus["documents"].as_array().expect("documents").clone();
    let mut compared = 0u32;
    let mut positive = 0u64;
    let mut negative = 0u64;
    let mut divergences = Vec::new();

    for case in corpus["cases"].as_array().expect("cases") {
        if case["kind"].as_str() == Some("rejection") {
            continue;
        }
        let why = case["why"].as_str().unwrap_or("?");
        let pattern = case["pattern"].as_str().expect("pattern");
        let options = case["options"].as_str().expect("options");
        let expected: BTreeSet<&str> = case["matchingIds"]
            .as_array()
            .expect("matchingIds")
            .iter()
            .filter_map(Value::as_str)
            .collect();

        // Build the selector the same way a caller would.
        let selector = serde_json::json!({
            "f": { "$regex": pattern, "$options": options }
        });
        let matcher = match Matcher::new(&selector) {
            Ok(matcher) => matcher,
            Err(error) => {
                divergences.push(format!(
                    "{why}: upstream accepts this options string but dxdb refused it: {error}"
                ));
                continue;
            }
        };

        for document in &documents {
            let id = document["id"].as_str().expect("id");
            let should_match = expected.contains(id);
            if matcher.matches(document) == should_match {
                if should_match {
                    positive += 1;
                } else {
                    negative += 1;
                }
            } else {
                divergences.push(format!(
                    "{why}: document {id} {} but upstream says it {}",
                    if matcher.matches(document) {
                        "matched"
                    } else {
                        "did not match"
                    },
                    if should_match { "does" } else { "does not" }
                ));
            }
        }
        compared += 1;
    }

    assert!(positive > 0, "no positive assertions");
    assert!(negative > 0, "no negative assertions");
    assert!(
        divergences.is_empty(),
        "{} divergences across {compared} cases ({positive} positive, {negative} negative \
         assertions):\n{}",
        divergences.len(),
        divergences.join("\n")
    );
    assert_eq!(
        compared,
        corpus["comparisonCount"].as_u64().unwrap_or_default() as u32
    );
}

/// Every `$options` string upstream rejects is an **error**, never a silent no-match.
///
/// This is the direction that matters most. A matcher that ignored an unknown flag would
/// evaluate `$options: 'x'` as "regex `a` with no flags" — plausible-looking results for a query
/// that fails in a browser.
#[test]
fn every_invalid_flag_string_is_an_error() {
    let corpus = load();
    let mut checked = 0u32;
    let mut divergences = Vec::new();

    for case in corpus["cases"].as_array().expect("cases") {
        if case["kind"].as_str() != Some("rejection") {
            continue;
        }
        let options = case["options"].as_str().expect("options");
        let pattern = case["pattern"].as_str().expect("pattern");

        // Parsing the flags directly.
        match RegexFlags::parse(options) {
            Err(_) => {}
            Ok(flags) => divergences.push(format!(
                "$options {options:?} must be rejected (upstream: {}) but parsed to {flags:?}",
                case["rejected"].as_str().unwrap_or("")
            )),
        }
        // And through a whole selector, which is the path a query takes.
        let selector = serde_json::json!({
            "f": { "$regex": pattern, "$options": options }
        });
        match Matcher::new(&selector) {
            Err(_) => {}
            Ok(matcher) => {
                // Show what the silent-acceptance failure actually looks like.
                let matched = corpus["documents"]
                    .as_array()
                    .expect("documents")
                    .iter()
                    .filter(|document| matcher.matches(document))
                    .count();
                divergences.push(format!(
                    "$options {options:?} must be rejected but the matcher accepted it and \
                     evaluated to {matched} matching documents"
                ));
            }
        }
        checked += 1;
    }

    assert!(
        divergences.is_empty(),
        "{} of {checked} invalid options strings diverged:\n{}",
        divergences.len(),
        divergences.join("\n")
    );
    assert_eq!(
        checked,
        corpus["rejectionCount"].as_u64().unwrap_or_default() as u32
    );
    assert_eq!(checked, 15, "the corpus records 15 invalid strings");
}

/// Each accepted flag is observable: turning it off changes the match set.
///
/// Asserted independently of the corpus so that a corpus which lost a case still fails. Paired
/// cases from `regex-flags.json`: the same pattern with and without the flag.
#[test]
fn each_accepted_flag_changes_the_result() {
    // (pattern, options that change behaviour, options that do not, document that distinguishes)
    let pairs: Vec<(&str, &str, &str, Value)> = vec![
        // `i` folds case.
        ("^alice$", "i", "", serde_json::json!({"f": "ALICE"})),
        // `m` makes `^`/`$` match at line boundaries.
        ("^b$", "m", "", serde_json::json!({"f": "a\nb"})),
        // `s` lets `.` cross a newline.
        ("a.c", "s", "", serde_json::json!({"f": "a\nc"})),
        // `u` enables property escapes.
        (
            "\\p{Letter}+",
            "u",
            "",
            serde_json::json!({"f": "abc"}),
        ),
    ];
    let mut checked = 0;
    for (pattern, on, off, document) in pairs {
        let with = Matcher::new(&serde_json::json!({
            "f": { "$regex": pattern, "$options": on }
        }))
        .unwrap_or_else(|error| panic!("{pattern} with {on:?}: {error}"));
        let without = Matcher::new(&serde_json::json!({
            "f": { "$regex": pattern, "$options": off }
        }))
        .unwrap_or_else(|error| panic!("{pattern} without {off:?}: {error}"));
        assert!(
            with.matches(&document),
            "{pattern:?} with $options {on:?} must match {document}"
        );
        assert!(
            !without.matches(&document),
            "{pattern:?} without {on:?} must not match {document}, or the flag has no \
             observable effect in this test"
        );
        checked += 1;
    }
    assert_eq!(checked, 4, "i, m, s and u must each be exercised");
}

/// `d`, `g` and `y` are accepted and do **not** change matching.
///
/// `d` only adds capture-group indices to the match object, `g` makes a `RegExp` stateful through
/// `lastIndex`, and `y` anchors at `lastIndex` — none of which Mango reads. They must therefore
/// be accepted (upstream accepts them) and be inert.
#[test]
fn d_g_and_y_are_accepted_and_inert() {
    let documents = [
        serde_json::json!({"f": "alice"}),
        serde_json::json!({"f": "ALICE"}),
        serde_json::json!({"f": "bob"}),
        serde_json::json!({"f": ""}),
        serde_json::json!({"f": "a\nb"}),
    ];
    for (pattern, options) in [
        ("a", "d"),
        ("a", "g"),
        ("a", "y"),
        ("a", "gy"),
        ("a", "dg"),
        ("^alice$", "di"),
    ] {
        let flags = RegexFlags::parse(options)
            .unwrap_or_else(|error| panic!("{options:?} must be accepted: {error}"));
        let with = Matcher::new(&serde_json::json!({
            "f": { "$regex": pattern, "$options": options }
        }))
        .expect("compiles");
        // The same pattern with the flag removed: for `d`/`g`/`y` alone the results must be
        // identical, and for `di` they must equal plain `i`.
        let baseline_options = options.replace(['d', 'g', 'y'], "");
        let baseline = Matcher::new(&serde_json::json!({
            "f": { "$regex": pattern, "$options": baseline_options }
        }))
        .expect("compiles");
        for document in &documents {
            assert_eq!(
                with.matches(document),
                baseline.matches(document),
                "{pattern:?} with {options:?} (flags {flags:?}) must match identically to \
                 {baseline_options:?} on {document}"
            );
        }
    }
}

/// Flag parsing is a *set* check: order does not matter, repetition is an error.
#[test]
fn flag_order_is_irrelevant_and_repetition_is_not() {
    // Every ordering of `ims` parses and yields the same flags.
    let mut orders = Vec::new();
    for a in ['i', 'm', 's'] {
        for b in ['i', 'm', 's'] {
            for c in ['i', 'm', 's'] {
                let candidate: String = [a, b, c].into_iter().collect();
                if candidate.chars().collect::<BTreeSet<_>>().len() == 3 {
                    orders.push(candidate);
                }
            }
        }
    }
    assert_eq!(orders.len(), 6, "there are six orderings of three flags");
    let mut parsed = Vec::new();
    for order in &orders {
        parsed.push(
            RegexFlags::parse(order)
                .unwrap_or_else(|error| panic!("{order:?} must parse: {error}")),
        );
    }
    for pair in parsed.windows(2) {
        assert_eq!(pair[0], pair[1], "flag order must not change the flags");
    }
    // And every repetition is an error, for every flag.
    for flag in ['d', 'g', 'i', 'm', 's', 'u', 'v', 'y'] {
        let repeated: String = [flag, flag].into_iter().collect();
        assert!(
            RegexFlags::parse(&repeated).is_err(),
            "{repeated:?} is an invalid flags string upstream and must be an error"
        );
    }
}

/// `u` and `v` are mutually exclusive, in either order.
#[test]
fn unicode_and_unicode_sets_are_mutually_exclusive() {
    for options in ["uv", "vu", "duv", "ivu"] {
        let error = RegexFlags::parse(options)
            .expect_err(&format!("{options:?} must be rejected"));
        assert!(
            error.message.contains("mutually exclusive"),
            "{options:?}: the error should name the rule; got {error}"
        );
    }
    // Each alone is fine.
    assert!(RegexFlags::parse("u").is_ok());
    assert!(RegexFlags::parse("v").is_ok());
    assert!(RegexFlags::parse("du").is_ok());
    assert!(RegexFlags::parse("dv").is_ok());
}

/// The `Flags` struct round-trips through its ECMAScript string form.
///
/// `RegExp.prototype.flags` emits in the order `dgimsuvy`; `as_str` must agree, because it is
/// what the matcher and any diagnostic report.
#[test]
fn flags_render_in_ecmascript_order() {
    let flags = RegexFlags::parse("yusmigd").expect("all eight flags");
    assert_eq!(flags.as_str(), "dgimsuy", "the canonical ECMAScript order");
    let flags = RegexFlags::parse("i").expect("parses");
    assert_eq!(flags.as_str(), "i");
    let flags = RegexFlags::parse("").expect("parses");
    assert_eq!(flags.as_str(), "", "no flags renders as the empty string");
    // Round trip: parsing the rendered form gives the same flags back.
    for options in ["dgimsuy", "ims", "dv", "di"] {
        let once = RegexFlags::parse(options).expect("parses");
        let twice = RegexFlags::parse(&once.as_str())
            .unwrap_or_else(|error| panic!("re-parsing {:?}: {error}", once.as_str()));
        assert_eq!(once, twice, "{options:?} must round-trip");
    }
}
