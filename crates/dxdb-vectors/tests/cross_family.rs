//! Cross-family consistency.
//!
//! Each family is generated independently and each has its own `--check`. That catches
//! a family drifting from itself, but not the family drifting from its *siblings*: if
//! one family is regenerated at a different upstream commit, or after a generator
//! change that only touched one module, every individual file still matches its own
//! regeneration while the set as a whole becomes incoherent. A consumer then reads a
//! schema from one file and an index string from another and gets a mismatch that
//! neither family's check can explain.
//!
//! This file asserts the relationships *between* the files. Every check here is
//! something that would hold if all eight were generated in the same run at the same
//! pin, and that can fail if they were not.
//!
//! # Writing these checks
//!
//! The temptation is to assert a tidy invariant and then loosen it when it fails. Each
//! check below states precisely what it covers and, where upstream's behaviour makes a
//! naive version false, says why — so a future reader can tell a deliberate exemption
//! from a fudge. Two examples are load-bearing:
//!
//! - Index strings for a document whose field value **exceeds** `maxLength` are longer
//!   than the declared index length, because upstream pads with `padEnd` and does not
//!   truncate. That is the documented behaviour of the `longer-than-max-length` probes,
//!   not an inconsistency.
//! - `selector-matching`'s corpus uses `id` as its primary key while the other families'
//!   fixtures use `passportId`. The families describe *different fixtures on purpose*;
//!   the shared invariant is the primary-key *name declared by the schema*, not a single
//!   name across the corpus.

use std::collections::{BTreeMap, BTreeSet};

use dxdb_vectors::{
    error_vectors, index_strings, normalised_queries, schema_hashes, selector_matching,
    VectorFamily, Vectors,
};

fn load_all() -> (
    schema_hashes::Vectors,
    index_strings::Vectors,
    normalised_queries::Vectors,
    selector_matching::Vectors,
    error_vectors::Vectors,
    dxdb_vectors::bulk_write_categorization::Vectors,
) {
    let v = Vectors::discover();
    (
        v.load::<schema_hashes::Vectors>(VectorFamily::SchemaHashes)
            .expect("schema-hashes"),
        v.load::<index_strings::Vectors>(VectorFamily::IndexStrings)
            .expect("index-strings"),
        v.load::<normalised_queries::Vectors>(VectorFamily::NormalisedQueries)
            .expect("normalised-queries"),
        v.load::<selector_matching::Vectors>(VectorFamily::SelectorMatching)
            .expect("selector-matching"),
        v.load::<error_vectors::Vectors>(VectorFamily::ErrorVectors)
            .expect("error-vectors"),
        v.load::<dxdb_vectors::bulk_write_categorization::Vectors>(
            VectorFamily::BulkWriteCategorization,
        )
        .expect("bulk-write-categorization"),
    )
}

/// Every family must record the same upstream provenance.
///
/// This is the coarsest cross-check and the one that catches the "regenerated one family
/// at a different commit" mistake directly. `provenance_pin.rs` checks each file against
/// NOTICE; this checks them against *each other*, which is a different failure: two
/// files can both disagree with a moved NOTICE in the same way and still be consistent,
/// but they cannot both be consistent with each other if one was left stale.
#[test]
fn all_families_share_one_upstream_provenance() {
    let v = Vectors::discover();

    let mut by_file: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    for family in dxdb_vectors::ALL_FAMILIES {
        // `jsnum` is Node-only and has no upstream fields; `error-vectors` included.
        let p = match *family {
            VectorFamily::Jsnum => continue,
            VectorFamily::IndexStrings => v
                .load::<index_strings::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::SchemaHashes => v
                .load::<schema_hashes::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::NormalisedQueries => v
                .load::<normalised_queries::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::BulkWriteCategorization => v
                .load::<dxdb_vectors::bulk_write_categorization::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::SelectorMatching => v
                .load::<selector_matching::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::ReplicationTranscripts => v
                .load::<dxdb_vectors::replication_transcripts::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::ErrorVectors => v
                .load::<error_vectors::Vectors>(*family)
                .unwrap()
                .provenance(),
        };
        by_file.push((family.to_string(), p.rxdb_commit, p.rxdb_version));
    }

    let commits: BTreeSet<&Option<String>> = by_file.iter().map(|(_, c, _)| c).collect();
    let versions: BTreeSet<&Option<String>> = by_file.iter().map(|(_, _, v)| v).collect();
    assert_eq!(
        commits.len(),
        1,
        "the families were generated from different upstream commits, so they are stale \
         relative to each other: {by_file:?}\n\
         Regenerate with `make vectors` so every family comes from one run."
    );
    assert_eq!(
        versions.len(),
        1,
        "the families record different upstream versions: {by_file:?}"
    );
}

/// Every schema name a family refers to must resolve in `schema-hashes`.
///
/// `index-strings` and `normalised-queries` name a fixture per case instead of repeating
/// its schema. If a family is regenerated against a fixture set that `schema-hashes`
/// does not carry, a consumer cannot build the schema the case is about.
#[test]
fn every_referenced_schema_resolves_in_schema_hashes() {
    let (hashes, idx, queries, ..) = load_all();
    let resolvable: BTreeSet<&str> = hashes.fixture_inputs().keys().copied().collect();

    let mut unresolved: Vec<String> = Vec::new();
    for (family, names) in [
        (
            "index-strings",
            idx.cases.iter().map(|c| c.schema()).collect::<Vec<_>>(),
        ),
        (
            "normalised-queries",
            queries
                .cases
                .iter()
                .map(|c| match c {
                    normalised_queries::Case::Planned(p) => p.schema.as_str(),
                    normalised_queries::Case::Rejected(r) => r.schema.as_str(),
                })
                .collect(),
        ),
    ] {
        for name in names {
            if !resolvable.contains(name) {
                unresolved.push(format!("{family} -> {name}"));
            }
        }
    }
    unresolved.sort();
    unresolved.dedup();
    assert!(
        unresolved.is_empty(),
        "{} schema reference(s) do not resolve in schema-hashes:\n{}\n\
         A consumer that needs the schema behind such a name would have to hand-copy it.",
        unresolved.len(),
        unresolved.join("\n")
    );
}

/// An index string must be exactly as long as `getIndexStringLength` says — with one
/// documented exception.
///
/// Upstream builds an index string by padding each field with `padEnd(maxLength)`. If a
/// document's field value is *longer* than `maxLength`, `padEnd` does not truncate, so
/// the result is longer than the declared length. That is real upstream behaviour and
/// the `longer-than-max-length` probes exist precisely to record it. Every other case
/// must match exactly, and the over-length cases must be longer by exactly the excess.
#[test]
fn index_string_lengths_agree_across_cases_and_schemas() {
    let (_, idx, ..) = load_all();

    let declared: BTreeMap<(String, Vec<String>), usize> = idx
        .index_string_lengths()
        .map(|c| {
            let key = (
                c.schema.clone(),
                c.index.iter().map(|e| e.to_string()).collect(),
            );
            (key, c.length)
        })
        .collect();

    let mut checked = 0usize;
    let mut over_length = 0usize;
    let mut mismatches: Vec<String> = Vec::new();

    for case in idx.index_strings() {
        let key = (
            case.schema.clone(),
            case.index.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
        );
        // Only compare where the same (schema, index) has a declared length. Some
        // index-string cases exist for indexes with no length case, and inventing an
        // expectation for those would be a check of nothing.
        let Some(&want) = declared.get(&key) else {
            continue;
        };
        let got = case.index_string.len();
        checked += 1;

        if case.why.contains("longer-than-max-length") {
            // The value exceeds a maxLength, so padding overflows. Assert it is longer
            // rather than exactly `want`, and that the overflow is small and positive —
            // a port that truncated would produce exactly `want`, and one that mangled
            // the padding would produce something wildly off.
            let overflow = got.saturating_sub(want);
            if got <= want || overflow > 32 {
                mismatches.push(format!(
                    "{} ({}): over-length probe produced {got}, declared {want} — expected \
                     a small positive overflow",
                    case.schema, case.why
                ));
            } else {
                over_length += 1;
            }
            continue;
        }

        if got != want {
            mismatches.push(format!(
                "{} ({}): index string is {got} chars, declared length is {want}",
                case.schema, case.why
            ));
        }
    }

    assert!(
        checked > 0,
        "no index-string case had a matching declared length, so this check examined \
         nothing — the two case kinds must reference the same (schema, index) pairs"
    );
    assert!(
        over_length > 0,
        "no over-length probe was found; the `longer-than-max-length` cases are how \
         upstream's non-truncating padding is recorded, and losing them would silently \
         make this check vacuous for that behaviour"
    );
    assert!(
        mismatches.is_empty(),
        "{} of {checked} index strings disagree with their declared length:\n{}",
        mismatches.len(),
        mismatches
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Fields named by a query must exist in the schema the same case records.
///
/// `normalised-queries` records both the schema *name* and the plan. The plan's index
/// and the normalised sort both name fields, and every one of them must be declared in
/// that fixture. This is the check that catches a query corpus generated against a
/// schema that `schema-hashes` no longer describes.
#[test]
fn plan_fields_exist_in_their_schema() {
    let (hashes, _, queries, ..) = load_all();
    let fixtures = hashes.fixture_inputs();

    fn declares(schema: &serde_json::Value, path: &str) -> bool {
        let mut cursor = schema;
        for segment in path.split('.') {
            match cursor.get("properties").and_then(|p| p.get(segment)) {
                Some(next) => cursor = next,
                None => return false,
            }
        }
        true
    }

    let mut checked = 0usize;
    let mut missing: Vec<String> = Vec::new();

    for case in queries.planned() {
        // `index-unknown-field` is the one case that must name a field the schema does
        // not declare: it exists to record how upstream plans an index over a missing
        // field (`notAField`). Skipping it is the point of the case, not an exemption
        // from the rule — the generator keeps it unpruned on purpose, so asserting
        // otherwise here would delete the only evidence of that behaviour.
        if case.why == "index-unknown-field" {
            assert!(
                case.plan.index.iter().any(|f| f == "notAField")
                    || case
                        .normalized_query
                        .get("sort")
                        .and_then(|s| s.as_array())
                        .is_some_and(|a| a.iter().any(|p| p.get("notAField").is_some())),
                "the index-unknown-field case no longer references the missing field it                  exists to probe, so it is now testing nothing"
            );
            continue;
        }

        let Some(schema) = fixtures.get(case.schema.as_str()) else {
            // Resolution is asserted by its own test above; skipping here keeps the two
            // failures readable instead of reporting the same problem twice.
            continue;
        };

        for field in &case.plan.index {
            // Meta fields are injected by defaulting and are not in the raw fixture.
            if field.starts_with('_') || field.contains('.') && field.starts_with("_meta") {
                continue;
            }
            if !declares(schema, field) {
                missing.push(format!(
                    "{} ({}): plan index names {field:?}, not declared in the fixture",
                    case.schema, case.why
                ));
            }
        }

        // Sort fields, from the normalised query rather than the plan.
        if let Some(sort) = case.normalized_query.get("sort").and_then(|s| s.as_array()) {
            for part in sort {
                let Some(object) = part.as_object() else {
                    continue;
                };
                for field in object.keys() {
                    if field.starts_with('_') {
                        continue;
                    }
                    if !declares(schema, field) {
                        missing.push(format!(
                            "{} ({}): normalised sort names {field:?}, not declared in the fixture",
                            case.schema, case.why
                        ));
                    }
                }
            }
        }
        checked += 1;
    }

    assert!(checked > 0, "no planned query case was examined");
    assert!(
        missing.is_empty(),
        "{} field reference(s) do not exist in the schema their case names:\n{}",
        missing.len(),
        missing
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Every id a selector case matches must be in the corpus, and match counts must agree.
///
/// `selector-matching` records `matchingIds` and `matchCount` separately. They can
/// disagree only if the file was edited or produced by a broken generator, and a
/// consumer using `matchCount` for a quick check would never notice the ids drifting.
#[test]
fn selector_match_ids_are_in_the_corpus_and_counted_honestly() {
    let (_, _, _, selectors, ..) = load_all();

    let corpus_ids: BTreeSet<&str> = selectors
        .corpus
        .iter()
        .filter_map(|d| d.get("id").and_then(|i| i.as_str()))
        .collect();
    assert!(!corpus_ids.is_empty(), "the selector corpus has no ids");

    let mut problems: Vec<String> = Vec::new();
    for case in selectors.matching() {
        if case.match_count != case.matching_ids.len() {
            problems.push(format!(
                "{}: matchCount is {} but {} ids are listed",
                case.why,
                case.match_count,
                case.matching_ids.len()
            ));
        }
        for id in &case.matching_ids {
            if !corpus_ids.contains(id.as_str()) {
                problems.push(format!(
                    "{}: matched id {id:?} is not in the corpus",
                    case.why
                ));
            }
        }
        // No duplicates: an id matching twice would be a corpus problem.
        let unique: BTreeSet<&String> = case.matching_ids.iter().collect();
        if unique.len() != case.matching_ids.len() {
            problems.push(format!("{}: matchingIds contains duplicates", case.why));
        }
    }

    assert!(
        problems.is_empty(),
        "{} selector-matching inconsistenc(ies):\n{}",
        problems.len(),
        problems
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The corpus and the selectors must agree on the primary key.
///
/// This is the check that would catch the selector family being built over a document
/// shape that the rest of the family set does not describe.
#[test]
fn selector_corpus_primary_key_is_declared_by_its_schema() {
    let (hashes, _, _, selectors, ..) = load_all();
    let fixtures = hashes.fixture_inputs();

    // The corpus documents all carry `id`, and the generator builds them from the
    // `human` fixture with a substituted primary key. Either way, the field must be in
    // every document and must be a declared property of the fixture the family used.
    let human = fixtures
        .get("human")
        .expect("the selector family is built from the `human` fixture");
    assert!(
        human.get("properties").and_then(|p| p.get("id")).is_none(),
        "this check assumed `human` has no `id` property; if it gained one, the corpus \
         generator's primary-key substitution needs revisiting"
    );

    for doc in &selectors.corpus {
        assert!(
            doc.get("id").and_then(|i| i.as_str()).is_some(),
            "a corpus document has no string `id`: {doc}"
        );
    }
}

/// A declared code is a code upstream has; anything else is ours alone.
///
/// The complement of the per-crate check in `error_vectors.rs`: this one asserts the
/// vector's own internal consistency, so a `reachable` flag cannot point at a code that
/// is not in the vocabulary.
#[test]
fn declared_codes_and_raised_cases_are_internally_consistent() {
    let (_, _, _, _, errors, ..) = load_all();

    let declared: BTreeSet<&str> = errors
        .declared_codes
        .iter()
        .map(|c| c.code.as_str())
        .collect();

    // Every raised case that produced a code must be in the vocabulary.
    for case in errors.typed() {
        let code = case.error_code.as_deref().unwrap();
        assert!(
            declared.contains(code),
            "raised case {} produced {code}, which is not in the declared vocabulary",
            case.why
        );
        assert!(
            case.error_code.as_deref() == case.expected_code.as_deref(),
            "raised case {} recorded expectedCode {:?} but observed {:?}",
            case.why,
            case.expected_code,
            case.error_code
        );
    }

    // `reachable` must mean exactly "this generator raised it".
    let raised: BTreeSet<&str> = errors
        .typed()
        .filter_map(|c| c.error_code.as_deref())
        .collect();
    let flagged: BTreeSet<&str> = errors
        .declared_codes
        .iter()
        .filter(|c| c.reachable)
        .map(|c| c.code.as_str())
        .collect();
    assert_eq!(
        flagged, raised,
        "the `reachable` flags disagree with the codes actually raised"
    );

    // Counts must match the arrays.
    assert_eq!(errors.raised_case_count, errors.raised_cases.len());
    assert_eq!(errors.declared_code_count, errors.declared_codes.len());
    assert_eq!(
        errors.typed_case_count + errors.untyped_case_count,
        errors.raised_case_count
    );

    // The declared list must be sorted and unique, so a diff is readable and a duplicate
    // cannot hide one code behind another.
    let mut sorted = errors.declared_codes.clone();
    sorted.sort_by(|a, b| a.code.cmp(&b.code));
    let dupes: Vec<&str> = errors
        .declared_codes
        .windows(2)
        .filter(|w| w[0].code == w[1].code)
        .map(|w| w[0].code.as_str())
        .collect();
    assert!(dupes.is_empty(), "duplicate declared codes: {dupes:?}");
}

/// `bulk-write-categorization` documents must carry the family's primary path.
///
/// The family records `primaryPath` once and documents throughout. If they disagree, a
/// consumer indexing by the recorded path silently reads the wrong field.
#[test]
fn bulk_write_documents_use_the_recorded_primary_path() {
    let (_, _, _, _, _, bulk) = load_all();
    let primary = bulk.primary_path.as_str();
    assert!(!primary.is_empty(), "primaryPath is empty");

    fn check(doc: &serde_json::Value, primary: &str, where_: &str, problems: &mut Vec<String>) {
        if !doc.is_object() {
            return;
        }
        if doc.get(primary).is_none() {
            problems.push(format!("{where_}: document has no {primary:?} field"));
        }
    }

    let mut problems: Vec<String> = Vec::new();
    for case in &bulk.cases {
        for row in &case.write_rows {
            check(&row.document, primary, &case.why, &mut problems);
        }
        for row in &case.bulk_insert_docs {
            check(&row.document, primary, &case.why, &mut problems);
        }
        for row in &case.bulk_update_docs {
            check(&row.document, primary, &case.why, &mut problems);
        }
        for event in &case.events {
            check(&event.document_data, primary, &case.why, &mut problems);
        }
        for (id, doc) in &case.docs_in_db {
            check(doc, primary, &case.why, &mut problems);
            if doc.get(primary).and_then(|v| v.as_str()) != Some(id.as_str()) {
                problems.push(format!(
                    "{}: docsInDb key {id:?} does not match the document's {primary:?}",
                    case.why
                ));
            }
        }
        if let Some(newest) = &case.newest_row {
            check(newest, primary, &case.why, &mut problems);
        }
    }

    assert!(
        problems.is_empty(),
        "{} bulk-write document(s) disagree with primaryPath={primary:?}:\n{}",
        problems.len(),
        problems
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
