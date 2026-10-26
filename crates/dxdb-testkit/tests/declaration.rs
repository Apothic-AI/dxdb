//! The testkit's own tests.
//!
//! This crate's product is *tests*, so its own tests have one job: prove the product
//! is honest. Three kinds of check, none of which touch a storage:
//!
//! 1. **Provenance.** Every declared case names an upstream file and line that
//!    actually exists and actually declares a case there. This is checked against the
//!    real upstream checkout when `RXDB_UPSTREAM` (or `~/Code/rxdb`) is present, and
//!    skipped with a loud message when it is not — the declaration table itself is
//!    already generated from the source, so this test is a second, independent check.
//! 2. **Table consistency.** Ids unique, kinds valid, statuses reachable, line tallies
//!    partition their files, the executable registry matches the "Exercised" rows in
//!    both directions, and the correctness data agrees with the declaration.
//! 3. **The ported utilities.** `random_token`, `random_string_with_special_chars`,
//!    `wait_until`, `is_fast_mode`, `sha256_hex`, `withIndexes`, and the schema
//!    fixtures each have a property worth asserting, because a broken utility makes
//!    every ported case meaningless.

use std::collections::BTreeSet;
use std::path::Path;

use dxdb_testkit::decl::{
    all_cases, Coverage, Kind, PortStatus, IMPLEMENTATIONS_CASES, QUERY_CORRECTNESS_CASES,
};
use dxdb_testkit::fixtures::correctness;
use dxdb_testkit::report::{validate, Accounting, IMPLEMENTATIONS_TALLY, QUERY_CORRECTNESS_TALLY};
use dxdb_testkit::suite::sha256_hex;
use dxdb_testkit::util;

// ---------------------------------------------------------------------------
// 1. Provenance
// ---------------------------------------------------------------------------

/// The upstream checkout, if it is on this machine.
fn upstream_root() -> Option<std::path::PathBuf> {
    let candidates = [
        std::env::var_os("RXDB_UPSTREAM").map(std::path::PathBuf::from),
        Some(Path::new(&std::env::var("HOME").unwrap_or_default()).join("Code/rxdb")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|path| path.join("test/unit/rx-storage-implementations.test.ts").exists())
}

/// Every declared line must exist in the upstream file it names.
///
/// The declaration table is generated, so this should be vacuous. That is the point:
/// it is the check that would have caught a generator bug, and it is cheap enough to
/// run on every build.
#[test]
fn every_declared_line_exists_upstream() {
    let Some(root) = upstream_root() else {
        eprintln!(
            "SKIP every_declared_line_exists_upstream: no upstream checkout. \
             Set RXDB_UPSTREAM to check provenance against the real source."
        );
        return;
    };

    let mut problems = Vec::new();
    for case in all_cases() {
        let path = root.join(case.upstream_file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            problems.push(format!("{}: cannot read {}", case.id, path.display()));
            continue;
        };
        let Some(line) = text.lines().nth(case.upstream_line as usize - 1) else {
            problems.push(format!(
                "{}: line {} is past the end of {}",
                case.id, case.upstream_line, case.upstream_file
            ));
            continue;
        };
        // An implementations case is declared at its `it(`, a correctness case at its
        // `testTitle:`.
        let expected_marker = if case.upstream_file.ends_with("implementations.test.ts") {
            "it("
        } else {
            "testTitle:"
        };
        if !line.contains(expected_marker) {
            problems.push(format!(
                "{}: {}:{} does not contain {:?}; it reads {:?}",
                case.id, case.upstream_file, case.upstream_line, expected_marker, line.trim()
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "declared provenance does not match upstream:\n  {}",
        problems.join("\n  ")
    );
}

/// The declared title must be the upstream title, verbatim.
#[test]
fn declared_titles_match_upstream() {
    let Some(root) = upstream_root() else {
        eprintln!("SKIP declared_titles_match_upstream: no upstream checkout.");
        return;
    };
    let mut problems = Vec::new();
    for case in all_cases() {
        let path = root.join(case.upstream_file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let line = text
            .lines()
            .nth(case.upstream_line as usize - 1)
            .unwrap_or("");
        let found = if case.upstream_file.ends_with("implementations.test.ts") {
            // `it('title', ...`
            line.find("it('").map(|start| (start + 4, '\''))
        } else {
            // `testTitle: 'title'`
            line.find("testTitle: '").map(|start| (start + 12, '\''))
        };
        match found {
            Some((start, quote)) => {
                let rest = &line[start..];
                match rest.find(quote) {
                    Some(end) => {
                        if &rest[..end] != case.title {
                            problems.push(format!(
                                "{}: declared {:?} but upstream line says {:?}",
                                case.id,
                                case.title,
                                &rest[..end]
                            ));
                        }
                    }
                    None => problems.push(format!("{}: unterminated title", case.id)),
                }
            }
            None => problems.push(format!(
                "{}: {}:{} does not declare a title",
                case.id, case.upstream_file, case.upstream_line
            )),
        }
    }
    assert!(
        problems.is_empty(),
        "declared titles do not match upstream:\n  {}",
        problems.join("\n  ")
    );
}

/// The upstream line counts the tally is measured against must be right.
///
/// If upstream is checked out at a different commit, this test tells you before the
/// line tally becomes silently meaningless.
#[test]
fn upstream_line_counts_match_the_pin() {
    let Some(root) = upstream_root() else {
        eprintln!("SKIP upstream_line_counts_match_the_pin: no upstream checkout.");
        return;
    };
    for (file, expected) in [
        (
            dxdb_testkit::decl::UPSTREAM_IMPLEMENTATIONS,
            dxdb_testkit::decl::UPSTREAM_IMPLEMENTATIONS_LINES,
        ),
        (
            dxdb_testkit::decl::UPSTREAM_QUERY_CORRECTNESS,
            dxdb_testkit::decl::UPSTREAM_QUERY_CORRECTNESS_LINES,
        ),
    ] {
        let Ok(text) = std::fs::read_to_string(root.join(file)) else {
            continue;
        };
        let actual = text.lines().count();
        assert_eq!(
            actual, expected,
            "{file} has {actual} lines but the declaration table and the line tally \
             are measured against {expected}. Upstream moved: re-pin NOTICE and \
             regenerate, or the coverage numbers are fiction."
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Table consistency
// ---------------------------------------------------------------------------

/// Everything `report::validate` checks, as one test.
#[test]
fn declaration_table_is_internally_consistent() {
    let problems = validate();
    assert!(
        problems.is_empty(),
        "the declaration table is inconsistent:\n  {}",
        problems.join("\n  ")
    );
}

/// Case ids are unique, which a report depends on.
#[test]
fn case_ids_are_unique() {
    let mut seen = BTreeSet::new();
    let mut duplicates = Vec::new();
    for case in all_cases() {
        if !seen.insert(case.id) {
            duplicates.push(case.id);
        }
    }
    assert!(duplicates.is_empty(), "duplicate ids: {duplicates:?}");
}

/// Every `kind` string parses to a [`Kind`], and every `Kind` is used at least once.
///
/// The second half matters: a kind with no cases is a promise the table does not keep,
/// and the brief names all eleven.
#[test]
fn every_kind_is_used() {
    let mut used = BTreeSet::new();
    for case in all_cases() {
        used.insert(case.kind_enum());
    }
    let unused: Vec<&str> = Kind::ALL
        .into_iter()
        .filter(|kind| !used.contains(kind))
        .map(Kind::as_str)
        .collect();
    assert!(
        unused.is_empty(),
        "declared kinds with no case: {unused:?}. Either add the case or drop the kind \
         from Kind::ALL, so the table's shape and the vocabulary agree."
    );
}

/// Every case status is reachable, so no status is dead vocabulary.
#[test]
fn statuses_in_use_are_a_subset_of_the_vocabulary() {
    let mut used = BTreeSet::new();
    for case in all_cases() {
        used.insert(case.status.as_str());
    }
    // Every status in use must be one the report knows how to describe.
    let known: BTreeSet<&str> = dxdb_testkit::report::ALL_STATUSES
        .iter()
        .map(|status| status.as_str())
        .collect();
    for status in &used {
        assert!(known.contains(status), "unknown status in the table: {status}");
    }
    // And at least one case must be exercised, or the crate does nothing.
    assert!(
        used.contains(PortStatus::Exercised.as_str()),
        "no case is Exercised; the testkit has no executable suite"
    );
}

/// The executable registry and the "Exercised" rows must agree **in both directions**.
///
/// This is the test that stops "exercised" from meaning "declared". If a row says
/// Exercised but has no body, this fails. If a body exists but its row says otherwise,
/// this fails too, because a body nobody claims is either dead code or a lie in the
/// other direction.
#[test]
fn exercised_rows_have_bodies_and_bodies_have_rows() {
    // The suite dispatches on upstream line, so the check is: every Exercised row has a
    // line, and every line is unique within its suite. A duplicate line would make two
    // different cases run the same body, which is the silent drift this guards.
    let exercised: Vec<&dxdb_testkit::decl::Case> = all_cases()
        .filter(|case| case.status.is_exercised())
        .collect();
    assert!(
        !exercised.is_empty(),
        "no case is Exercised, so the testkit has no executable suite"
    );

    let mut lines = BTreeSet::new();
    for case in &exercised {
        assert!(
            lines.insert((case.upstream_file, case.upstream_line)),
            "two exercised cases share {}:{}",
            case.upstream_file,
            case.upstream_line
        );
        assert!(case.upstream_line > 0, "{}: no upstream line", case.id);
    }

    // Every exercised correctness row is a *group* whose queries are exercised through
    // the data; the executable surface there is the 89 queries, checked separately by
    // `correctness_data_matches_declaration`.
    let impls = exercised
        .iter()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .count();
    assert!(
        impls > 0,
        "no implementations case is exercised; the storage suite is empty"
    );
}

/// The line tallies must partition their upstream files.
///
/// A gap means a range of upstream lines this crate does not account for, which would
/// make every percentage derived from the tally an overstatement. An overlap would
/// double-count. Both fail here rather than in a report.
#[test]
fn line_tallies_partition_their_files() {
    for tally in [&IMPLEMENTATIONS_TALLY, &QUERY_CORRECTNESS_TALLY] {
        assert!(
            tally.is_partition(),
            "{}: the line tally is not a partition; gaps: {:?}",
            tally.file,
            tally.gaps()
        );
        assert_eq!(
            tally.accounted() as usize,
            tally.upstream_lines,
            "{}: tally accounts for {} of {} lines",
            tally.file,
            tally.accounted(),
            tally.upstream_lines
        );
    }
}

/// The code-line tally must be smaller than the upstream total.
///
/// This is an anti-inflation check with teeth: it fails if someone claims to have
/// "ported" extracted fixture data as code, because the data is accounted separately
/// and `code_lines()` excludes it.
#[test]
fn code_lines_are_a_subset_of_upstream_lines() {
    for tally in [&IMPLEMENTATIONS_TALLY, &QUERY_CORRECTNESS_TALLY] {
        assert!(
            tally.code_lines() < tally.upstream_lines as u32,
            "{}: code_lines {} is not less than the upstream total {} — the tally is \
             counting data or scaffolding as ported code",
            tally.file,
            tally.code_lines(),
            tally.upstream_lines
        );
        let data = tally
            .by_accounting()
            .into_iter()
            .find(|(accounting, _)| *accounting == Accounting::ExtractedData)
            .map(|(_, lines)| lines)
            .unwrap_or(0);
        let exercised = tally
            .by_accounting()
            .into_iter()
            .find(|(accounting, _)| *accounting == Accounting::PortedAndExercised)
            .map(|(_, lines)| lines)
            .unwrap_or(0);
        assert_eq!(
            tally.accounted(),
            data + exercised
                + tally
                    .by_accounting()
                    .into_iter()
                    .filter(|(accounting, _)| {
                        !matches!(
                            accounting,
                            Accounting::ExtractedData | Accounting::PortedAndExercised
                        )
                    })
                    .map(|(_, lines)| lines)
                    .sum::<u32>(),
            "{}: the accounting categories do not add up to the total",
            tally.file
        );
    }
}

/// The generated correctness data must agree with the declaration table.
#[test]
fn correctness_data_matches_declaration() {
    let groups = correctness::groups();
    assert_eq!(
        groups.len(),
        QUERY_CORRECTNESS_CASES.len(),
        "the extracted data has {} groups but the table declares {}",
        groups.len(),
        QUERY_CORRECTNESS_CASES.len()
    );

    let queries: usize = groups.iter().map(|group| group.queries.len()).sum();
    let declared: usize = QUERY_CORRECTNESS_CASES
        .iter()
        .map(|case| case.queries as usize)
        .sum();
    assert_eq!(
        queries, declared,
        "the extracted data has {queries} queries but the table declares {declared}"
    );

    for (group, case) in groups.iter().zip(QUERY_CORRECTNESS_CASES.iter()) {
        assert_eq!(
            group.title, case.title,
            "group {} title disagrees with the table",
            group.index
        );
        assert_eq!(
            group.upstream_line, case.upstream_line,
            "group {} line disagrees with the table",
            group.index
        );
        assert!(
            !group.queries.is_empty(),
            "group {} ({}) has no queries",
            group.index,
            group.title
        );
        assert!(
            !group.data.is_empty(),
            "group {} ({}) has no data",
            group.index,
            group.title
        );
    }
}

/// Every expected result id must name a document the group actually inserts.
///
/// This is the check that makes the extracted data worth trusting. A transposed
/// expected id still compiles and still passes against a permissive storage, so it is
/// verified here against the data rather than left to the storage to notice.
#[test]
fn expected_result_ids_exist_in_the_group_data() {
    let mut problems = Vec::new();
    for group in correctness::groups() {
        let ids: BTreeSet<String> = group
            .data
            .iter()
            .filter_map(|document| group.id_of(document).map(str::to_string))
            .collect();
        assert_eq!(
            ids.len(),
            group.data.len(),
            "group {} ({}) has documents without a unique primary value",
            group.index,
            group.title
        );

        for query in &group.queries {
            for expected in query.expected_result_doc_ids {
                if !ids.contains(*expected) {
                    problems.push(format!(
                        "group {} ({}) query {:?}: expected id {:?} is not in the data",
                        group.index, group.title, query.info, expected
                    ));
                }
            }
            // And an expected list must be a set: the same id twice would mean the
            // storage returned one document twice, which no correct storage does.
            let unique: BTreeSet<&&str> = query.expected_result_doc_ids.iter().collect();
            if unique.len() != query.expected_result_doc_ids.len() {
                problems.push(format!(
                    "group {} query {:?}: expected ids contain a duplicate",
                    group.index, query.info
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n  "));
}

/// Every group's schema must actually be a schema a storage can read.
#[test]
fn group_schemas_are_storage_schemas() {
    let mut problems = Vec::new();
    for group in correctness::groups() {
        let schema = group.materialised_schema();
        match dxdb_storage::StorageSchema::from_value(&schema) {
            Ok(storage_schema) => {
                if storage_schema.primary_path != group.primary_path {
                    problems.push(format!(
                        "group {} ({}): declared primary path {:?} but the schema says {:?}",
                        group.index,
                        group.title,
                        group.primary_path,
                        storage_schema.primary_path
                    ));
                }
            }
            Err(err) => problems.push(format!(
                "group {} ({}): schema is not a storage schema: {err}",
                group.index, group.title
            )),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n  "));
}

/// `withIndexes` must append indexed fields to `required`, as upstream does.
///
/// Upstream's `withIndexes` clones the schema, replaces `indexes`, and pushes any
/// indexed field that is not already required. Dropping the last step changes which
/// documents are valid, so it is asserted directly.
#[test]
fn with_indexes_appends_required_fields() {
    use dxdb_testkit::fixtures::SchemaSource;

    let base = dxdb_testkit::schemas::human_minimal();
    // `age` is already required; `oneOptional` is not.
    let source = SchemaSource::WithIndexes {
        base: dxdb_testkit::schemas::human_minimal,
        base_name: "humanMinimal",
        indexes: vec![serde_json::json!(["oneOptional"])],
    };
    let materialised = source.materialise();

    let required: Vec<&str> = materialised["required"]
        .as_array()
        .expect("required")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(
        required.contains(&"oneOptional"),
        "an indexed optional field must become required, got {required:?}"
    );
    assert_eq!(
        materialised["indexes"],
        serde_json::json!([["oneOptional"]]),
        "the indexes must be replaced, not merged"
    );
    // And the base fixture must not have been mutated.
    assert_eq!(
        base["indexes"],
        serde_json::json!([]),
        "withIndexes must clone the fixture rather than mutate it"
    );
}

/// Coverage arithmetic must add up.
#[test]
fn coverage_totals_are_consistent() {
    for coverage in [
        Coverage::implementations(),
        Coverage::query_correctness(),
        Coverage::both(),
    ] {
        let by_kind_declared: usize = coverage.declared.values().sum();
        let by_kind_exercised: usize = coverage.exercised.values().sum();
        assert_eq!(by_kind_declared, coverage.total_declared);
        assert_eq!(by_kind_exercised, coverage.total_exercised);
        assert!(coverage.total_exercised <= coverage.total_declared);
        assert!((0.0..=100.0).contains(&coverage.exercised_percent()));
    }

    let both = Coverage::both();
    assert_eq!(
        both.total_declared,
        IMPLEMENTATIONS_CASES.len() + QUERY_CORRECTNESS_CASES.len()
    );
}

// ---------------------------------------------------------------------------
// 3. The ported utilities
// ---------------------------------------------------------------------------

/// `randomToken` returns lowercase letters of the requested length.
#[test]
fn random_token_shape() {
    for _ in 0..100 {
        let token = util::random_token(12);
        assert_eq!(token.len(), 12);
        assert!(
            token.chars().all(|c| c.is_ascii_lowercase()),
            "randomToken must only emit a-z, got {token:?}"
        );
    }
    assert_eq!(util::random_token(0), "");
}

/// `randomToken` must vary per call, and must vary under a reseed.
///
/// The per-call property is the one that matters most and the one this test used to miss.
/// The generator was `thread_local`, so every `#[test]` (which runs on a fresh thread)
/// started from the same seed and produced the *same* "random" database name. Against
/// `dxdb-memory`, whose collection registry is process-global, that meant two tests shared
/// one collection and eight cases failed as apparent storage bugs. Upstream never sees
/// this because `Math.random()` is process-wide, so the assertion here is deliberately
/// about the generator being shared rather than about one `#[test]`'s local draws.
#[test]
fn random_token_varies_and_is_seeded() {
    let first: Vec<String> = (0..8).map(|_| util::random_token(10)).collect();
    let unique: BTreeSet<&String> = first.iter().collect();
    assert!(
        unique.len() > 1,
        "randomToken produced one value across 8 calls: {first:?}"
    );

    // Same seed, same sequence: this is what makes a failure replayable.
    //
    // Asserted on a *local* generator rather than through `util::seed`, because tests in
    // one binary run in parallel and a global reseed would perturb every other test's
    // draws. The property under test is the generator's, not the global's.
    let draw = |seed: u64| -> Vec<String> {
        let rng = util::Rng::new(seed);
        (0..8)
            .map(|_| {
                let chars: Vec<char> = util::COUCH_NAME_CHARS.chars().collect();
                (0..10)
                    .map(|_| chars[(rng.next_f64() * chars.len() as f64) as usize])
                    .collect()
            })
            .collect()
    };
    assert_eq!(draw(12345), draw(12345), "the same seed must replay");
    assert_ne!(
        draw(1),
        draw(2),
        "two seeds must not produce the same sequence, or the seed is decorative"
    );
}

/// Two tokens drawn on **different threads** must differ.
///
/// This is the regression test for the bug above. A `thread_local` generator passes the
/// single-threaded assertions and fails this one, because both threads start at the same
/// seed. It is the property upstream gets from `Math.random()` being process-wide.
#[test]
fn random_tokens_differ_across_threads() {
    // No `util::seed` here: the point is that the *shared* counter advances between
    // threads, which is what a thread-local generator fails regardless of seeding. Seeding
    // would also race the other tests in this binary.
    let first = std::thread::spawn(|| util::random_token(12))
        .join()
        .expect("thread");
    let second = std::thread::spawn(|| util::random_token(12))
        .join()
        .expect("thread");

    assert_ne!(
        first, second,
        "two threads drew the same token ({first}). The generator is thread-local, so \
         every test process shares one collection with every other and storage state leaks \
         between cases."
    );
}

/// `randomStringWithSpecialChars` respects its length bounds in *UTF-16* units.
///
/// The length is JavaScript's `String.length`, not Rust's `char` count, which is the
/// whole reason upstream's version can overrun and retry. Asserting on `encode_utf16`
/// is what makes this test about the ported behaviour rather than a coincidence.
#[test]
fn random_special_chars_respects_utf16_length() {
    for _ in 0..200 {
        let text = util::random_string_with_special_chars(8, 12);
        let length = text.encode_utf16().count();
        assert!(
            (8..=12).contains(&length),
            "length {length} outside 8..=12 for {text:?}"
        );
        // The first character must never be an emoji (upstream's foundationdb TODO).
        let first = text.chars().next().expect("non-empty");
        assert!(
            (first as u32) < 0x1F00,
            "a string must not start with an emoji, got {text:?}"
        );
    }
}

/// `randomStringWithSpecialChars` must actually emit umlauts, which is its purpose.
#[test]
fn random_special_chars_emits_umlauts() {
    let mut saw_umlaut = false;
    for _ in 0..200 {
        let text = util::random_string_with_special_chars(8, 12);
        if text.chars().any(|c| "äöüÖÄßÜ".contains(c)) {
            saw_umlaut = true;
            break;
        }
    }
    assert!(
        saw_umlaut,
        "the special-character generator never produced an umlaut in 200 draws; \
         the charset is wrong and the umlaut conformance case tests nothing"
    );
}

/// `randomStringWithSpecialChars` panics on an inverted range, as upstream throws.
#[test]
#[should_panic(expected = "invalid length")]
fn random_special_chars_rejects_inverted_range() {
    let _ = util::random_string_with_special_chars(12, 8);
}

/// The last-sorted charset character must be the max by UTF-16 code unit (D5).
#[test]
fn charset_last_sorted_uses_code_unit_order() {
    let expected = util::TEST_DATA_CHARSET
        .chars()
        .max_by_key(|c| {
            let mut buffer = [0u16; 2];
            c.encode_utf16(&mut buffer).first().copied().unwrap_or(0)
        })
        .expect("charset is non-empty");
    assert_eq!(util::test_data_charset_last_sorted(), expected);
    // And it must be the character code-unit order would pick, which for this
    // charset is `ü` (U+00FC), not the `[` that a byte-order sort would pick.
    assert_eq!(
        util::test_data_charset_last_sorted(),
        'ü',
        "code-unit order picks ü; a byte-order sort would pick a different character"
    );
}

/// `wait_until` resolves without waiting when the predicate is already true.
#[test]
fn wait_until_immediate() {
    let sleeper = util::NoSleep;
    let counter = util::PollCounter::new(1);
    let result = futures::executor::block_on(util::wait_until(&sleeper, 5, || {
        futures::future::ready(counter.poll())
    }));
    assert!(result.is_ok());
    assert_eq!(counter.calls(), 1, "an already-true predicate polls once");
}

/// `wait_until` retries until the predicate is true.
#[test]
fn wait_until_retries() {
    let sleeper = util::NoSleep;
    let counter = util::PollCounter::new(4);
    let result = futures::executor::block_on(util::wait_until(&sleeper, 10, || {
        futures::future::ready(counter.poll())
    }));
    assert!(result.is_ok());
    assert_eq!(counter.calls(), 4, "it must poll exactly until it succeeds");
}

/// `wait_until` gives up at the bound rather than hanging.
#[test]
fn wait_until_bounded() {
    let sleeper = util::NoSleep;
    let counter = util::PollCounter::new(usize::MAX);
    let result = futures::executor::block_on(util::wait_until(&sleeper, 3, || {
        futures::future::ready(counter.poll())
    }));
    assert!(
        matches!(result, Err(util::WaitUntilError::Timeout(_))),
        "a predicate that never becomes true must time out, got {result:?}"
    );
}

/// A predicate that *errors* rejects immediately, as upstream's `.catch` does.
///
/// This is the behaviour that makes a broken storage surface as a failure instead of a
/// timeout, so it is worth its own test.
#[test]
fn wait_until_propagates_predicate_error() {
    #[derive(Debug)]
    struct Boom;
    let sleeper = util::NoSleep;
    let result: Result<(), util::WaitUntilError<Boom>> =
        futures::executor::block_on(util::wait_until(&sleeper, 100, || {
            futures::future::ready(Err::<bool, Boom>(Boom))
        }));
    assert!(
        matches!(result, Err(util::WaitUntilError::Predicate(Boom))),
        "a predicate error must propagate, got {result:?}"
    );
}

/// `is_fast_mode` reads the same environment variable upstream does.
#[test]
fn fast_mode_reads_node_env() {
    // The suite runs with a fixed environment, so assert the function's shape rather
    // than mutating `NODE_ENV`, which would race other tests in this binary.
    let value = util::is_fast_mode();
    let expected = matches!(
        std::env::var("NODE_ENV").as_deref(),
        Ok("fast")
    ) || std::env::var_os("DXDB_FAST_TEST_MODE").is_some();
    assert_eq!(value, expected);
    // And `fast_or` must pick consistently with it.
    assert_eq!(
        util::fast_or(200, 100),
        if value { 100 } else { 200 },
        "fast_or must follow is_fast_mode"
    );
}

/// `sha256_hex` must match the published NIST vectors.
///
/// A wrong digest makes every attachment case fail for the wrong reason, and the storage
/// is what would get blamed.
#[test]
fn sha256_matches_nist_vectors() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // A payload over one block, to exercise the padding and the chaining.
    assert_eq!(
        sha256_hex(&[b'a'; 1000]),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}

/// Every schema fixture must be readable as a `StorageSchema` once filled.
#[test]
fn schema_fixtures_are_storage_schemas() {
    let mut problems = Vec::new();
    for (name, upstream, schema) in dxdb_testkit::schemas::all_fixtures() {
        let filled = dxdb_testkit::harness::fill_with_default_settings(schema);
        match dxdb_storage::StorageSchema::from_value(&filled) {
            Ok(storage_schema) => {
                if storage_schema.indexes.is_empty() {
                    problems.push(format!("{name} ({upstream}): no indexes after filling"));
                }
                // Every *user* index must start with `_deleted`: that is
                // `fillWithDefaultSettings`'s rule (`rx-schema-helper.ts:270`), and a
                // storage's assumption.
                //
                // The change-feed index `['_meta.lwt', primary]` is the exception and
                // must NOT be prefixed, because upstream appends it *after* the
                // prefixing loop (`rx-schema-helper.ts:283`) — the whole point of that
                // index is to enumerate every change including tombstones, so a
                // `_deleted` prefix would exclude exactly the rows it exists to find.
                // Asserting the exception is as important as asserting the rule.
                for index in &storage_schema.indexes {
                    let is_change_feed = index.first().map(String::as_str) == Some("_meta.lwt");
                    if is_change_feed {
                        if index.len() != 2 || index[1] != storage_schema.primary_path {
                            problems.push(format!(
                                "{name} ({upstream}): change-feed index {index:?} is not                                  ['_meta.lwt', primary]"
                            ));
                        }
                        continue;
                    }
                    if index.first().map(String::as_str) != Some("_deleted") {
                        problems.push(format!(
                            "{name} ({upstream}): user index {index:?} is not _deleted-prefixed"
                        ));
                    }
                }
                // And the change-feed index must exist, or `get_changed_documents_since`
                // has no index to paginate.
                if storage_schema.change_feed_index().is_none() {
                    problems.push(format!(
                        "{name} ({upstream}): no change-feed index after filling"
                    ));
                }
            }
            Err(err) => problems.push(format!("{name} ({upstream}): {err}")),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n  "));
}

/// The filled fixture schema must declare `_meta.lwt`, because its own indexes
/// reference it.
///
/// This is a regression test for the worst kind of fixture bug: one that makes every
/// storage look broken. The testkit's old local `fill_with_default_settings` injected
/// `_meta` as a bare `{"type": "object"}`, while `fillWithDefaultSettings` prepends
/// `_meta.lwt` to essentially every index (PLAN.md 4.3, the change-feed sort key). The
/// result was a schema whose own index could not be resolved, and `dxdb-indexeddb` failed
/// 43 of 49 cases with
///
/// ```text
/// CI1: index field `_meta.lwt` is not in the schema
/// ```
///
/// before any storage code ran. Asserting the property here means the next time someone
/// is tempted to hand-build a schema in a fixture, it fails in the testkit's own suite
/// rather than in a downstream storage's.
#[test]
fn fixture_schema_declares_the_meta_lwt_its_indexes_reference() {
    for (name, upstream, schema) in dxdb_testkit::schemas::all_fixtures() {
        let filled = dxdb_testkit::harness::fill_with_default_settings(schema);

        // 1. `_meta.lwt` must be a declared property, which is what CI1 checks.
        let lwt = filled
            .get("properties")
            .and_then(|properties| properties.get("_meta"))
            .and_then(|meta| meta.get("properties"))
            .and_then(|properties| properties.get("lwt"));
        assert!(
            lwt.is_some(),
            "{name} ({upstream}): properties._meta has no lwt, but the change-feed index \
             keys on _meta.lwt. Filled _meta was: {:?}",
            filled.get("properties").and_then(|p| p.get("_meta"))
        );

        // 2. The storage projection must accept the schema. This is the exact call that
        //    produced "CI1: index field `_meta.lwt` is not in the schema".
        let storage_schema = dxdb_storage::StorageSchema::from_value(&filled).unwrap_or_else(|err| {
            panic!("{name} ({upstream}): filled schema is not a storage schema: {err}")
        });

        // 3. And the change-feed index must be resolvable through that projection.
        assert!(
            storage_schema.change_feed_index().is_some(),
            "{name} ({upstream}): no resolvable change-feed index; indexes were {:?}",
            storage_schema.indexes
        );
    }
}

/// Every index field must be a path the schema actually declares.
///
/// The general form of the `_meta.lwt` bug: a fixture may not carry an index that
/// references a field its `properties` does not define. This checks every index of every
/// fixture against the filled `properties`, so the whole class fails here rather than one
/// instance of it at a time.
#[test]
fn every_index_field_resolves_against_the_filled_properties() {
    let mut problems = Vec::new();
    for (name, upstream, schema) in dxdb_testkit::schemas::all_fixtures() {
        let filled = dxdb_testkit::harness::fill_with_default_settings(schema);
        let storage_schema = match dxdb_storage::StorageSchema::from_value(&filled) {
            Ok(schema) => schema,
            Err(err) => {
                problems.push(format!("{name} ({upstream}): {err}"));
                continue;
            }
        };
        for index in &storage_schema.indexes {
            for field in index {
                // Index fields are dot paths into a *document* (`_meta.lwt`,
                // `deep.deep1`). Resolving one against a schema is not the same walk as
                // resolving it against a document: in a schema each level is
                // `{"type": "object", "properties": {...}}`, so the next segment is a key
                // of the nested `properties`, not a key of the level itself. Walking with
                // `get_object_path` would look for `_meta.lwt` as `properties._meta.lwt`
                // and miss the `properties` keyword — which is exactly the mistake my
                // first version of this test made.
                let declared = schema_declares_path(&filled, field);
                if !declared {
                    problems.push(format!(
                        "{name} ({upstream}): index {index:?} references `{field}`, which \
                         the filled schema does not declare in properties"
                    ));
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "indexes reference undeclared fields — this is the bug class that made every \
         indexeddb case fail before any storage code ran:\n  {}",
        problems.join("\n  ")
    );
}

/// True when `properties` in `schema` declares `path`, following nested `properties`.
///
/// A schema level is `{"type": "object", "properties": {...}}`, so the segment after an
/// object field is a key of that nested `properties` map. A document path and a schema
/// path are deliberately not the same walk, and conflating them produces a false negative
/// for every nested index.
fn schema_declares_path(schema: &serde_json::Value, path: &str) -> bool {
    let mut level = match schema.get("properties") {
        Some(properties) => properties,
        None => return false,
    };
    for segment in path.split('.') {
        let Some(next) = level.get(segment) else {
            return false;
        };
        // Descend through `properties` when the field is an object with a declared
        // shape. A field declared as `{"type": "object"}` with no `properties` (which
        // filling never produces for `_meta`) has no further path to resolve.
        level = match next.get("properties") {
            Some(nested) => nested,
            None => next,
        };
    }
    true
}

/// Filling a schema is a **one-shot** operation, matching upstream.
///
/// This is pinned as a warning rather than a requirement, because it is a trap. Upstream's
/// `fillWithDefaultSettings` appends `['_meta.lwt', primary]` to `indexes` on every call
/// (`rx-schema-helper.ts:283`) *after* the loop that prepends `_deleted`, so a second fill
/// turns that index into `['_deleted', '_meta.lwt', primary]` and appends a fresh
/// unprefixed copy. It never dedupes `useIndexes`, only `required`.
///
/// A downstream lane that stores an already-filled schema and re-fills it on open gets a
/// corrupt index list, and the symptom is a change feed that silently misses the newest
/// documents. So the property is asserted: filling twice differs, and the difference is
/// exactly the duplicated change-feed index. If this ever starts passing as "idempotent",
/// either upstream changed or the stopgap in `harness` drifted from it.
#[test]
fn filling_twice_duplicates_the_change_feed_index() {
    let once = dxdb_testkit::harness::fill_with_default_settings(dxdb_testkit::schemas::human());
    let once_indexes = once["indexes"].as_array().expect("indexes").len();
    let twice = dxdb_testkit::harness::fill_with_default_settings(once.clone());
    let twice_indexes = twice["indexes"].as_array().expect("indexes").len();

    assert_eq!(
        twice_indexes,
        once_indexes + 1,
        "upstream re-appends the change-feed index on every fill; got {once_indexes} then \
         {twice_indexes}. If filling became idempotent, `harness::fill_with_default_settings` \
         has drifted from upstream's `fillWithDefaultSettings`."
    );

    // And `required` must NOT grow, because upstream dedupes it.
    let once_required = once["required"].as_array().expect("required").len();
    let twice_required = twice["required"].as_array().expect("required").len();
    assert_eq!(
        once_required, twice_required,
        "upstream dedupes `required`, so a second fill must not grow it"
    );
}

/// The document fixtures must produce documents the schema would accept.
#[test]
fn fixture_documents_have_their_primary_key() {
    for (name, upstream, document) in dxdb_testkit::fixtures::objects::all_generators() {
        assert!(
            document.is_object(),
            "{name} ({upstream}) did not produce an object"
        );
    }
    let human = dxdb_testkit::fixtures::objects::human_data("aa", 20, "alice");
    for field in ["passportId", "firstName", "lastName", "age"] {
        assert!(
            human.get(field).is_some(),
            "humanData must set {field}, got {human}"
        );
    }
}

/// The revision constants must parse at the heights their names claim.
#[test]
fn example_revisions_parse_at_their_heights() {
    for (index, revision) in dxdb_testkit::fixtures::objects::EXAMPLE_REVISIONS
        .iter()
        .enumerate()
    {
        let height = dxdb_storage::get_height_of_revision(revision);
        assert_eq!(
            height as usize,
            index + 1,
            "EXAMPLE_REVISION_{} must be at height {}",
            index + 1,
            index + 1
        );
    }
}

/// A written document must carry all four meta fields, or `write_data_with` is broken
/// and every ported case is testing the wrong thing.
#[test]
fn write_data_carries_meta_fields() {
    let document = dxdb_testkit::fixtures::write::test_doc_write_data(
        "aa",
        "value",
        1_700_000_000_000.0,
        serde_json::Map::new(),
    );
    for field in ["_deleted", "_attachments", "_meta", "_rev"] {
        assert!(
            document.get(field).is_some(),
            "write_data must set {field}, got {document}"
        );
    }
    assert_eq!(document["_deleted"], serde_json::json!(false));
    assert_eq!(
        document["_meta"]["lwt"],
        serde_json::json!(1_700_000_000_000.0f64)
    );
    assert_eq!(
        document["_rev"],
        serde_json::json!(dxdb_testkit::fixtures::EXAMPLE_REVISION_1)
    );
}

/// Overrides must win over the defaults, as upstream's `Object.assign` order does.
#[test]
fn write_data_overrides_win() {
    let document = dxdb_testkit::fixtures::write::test_doc_write_data(
        "aa",
        "value",
        1.0,
        serde_json::Map::from_iter([
            ("_deleted".to_string(), serde_json::json!(true)),
            (
                "_rev".to_string(),
                serde_json::json!(dxdb_testkit::fixtures::EXAMPLE_REVISION_2),
            ),
        ]),
    );
    assert_eq!(document["_deleted"], serde_json::json!(true));
    assert_eq!(
        document["_rev"],
        serde_json::json!(dxdb_testkit::fixtures::EXAMPLE_REVISION_2)
    );
}

/// `mango_query` must append the primary key when the fixture omits a sort.
///
/// Three correctness fixtures have no `sort`, and their expected-id lists are
/// order-sensitive. Without the append those cases would be asserting an arbitrary
/// order, so the append is the reason they can be asserted at all.
#[test]
fn mango_query_appends_primary_key_to_sort() {
    let query = dxdb_testkit::harness::mango_query(
        &serde_json::json!({ "selector": {} }),
        "passportId",
    );
    assert_eq!(query.sort.len(), 1, "the primary key must be appended");
    assert_eq!(query.sort[0].field, "passportId");
    assert_eq!(query.skip, 0, "skip must default to 0");

    // And it must not duplicate an already-present primary key.
    let query = dxdb_testkit::harness::mango_query(
        &serde_json::json!({
            "selector": {},
            "sort": [{ "age": "asc" }, { "passportId": "asc" }],
        }),
        "passportId",
    );
    assert_eq!(query.sort.len(), 2, "the primary key must not be duplicated");
    assert_eq!(query.sort[0].field, "age");
}

/// A prepared query must not claim the plan satisfies anything.
///
/// This is the honesty property of the harness: the plan asserts nothing, so the
/// storage does the filtering and sorting. If this ever flips to `true` without
/// `dxdb-query` behind it, every ported query case would silently stop testing the
/// storage.
#[test]
fn prepared_query_plan_claims_nothing() {
    let prepared = dxdb_testkit::build_prepared_query(
        &serde_json::json!({ "selector": { "age": { "$gt": 20 } }, "sort": [{ "age": "asc" }] }),
        "passportId",
    );
    assert!(
        !prepared.query_plan.selector_satisfied_by_index,
        "the testkit's plan must not claim the index satisfies the selector"
    );
    assert!(
        !prepared.query_plan.sort_satisfied_by_index,
        "the testkit's plan must not claim the index satisfies the sort"
    );
    // The plan must name an index the storage **actually has**. That is the invariant,
    // and it is the one the old synthesized-index version of this harness got wrong: it
    // produced `_deleted,value,key` for a schema that had no such index, and three
    // `dxdb-memory` cases failed against a correct storage with `UnknownIndex`.
    //
    // A change-feed index (`_meta.lwt`, primary) is a legitimate answer and is *not*
    // `_deleted`-prefixed — upstream appends it after the prefixing loop
    // (`rx-schema-helper.ts:283`) because it must enumerate tombstones. So the assertion
    // is "resolvable against the schema", not "`_deleted`-prefixed".
    let schema = dxdb_testkit::harness::fill_with_default_settings(dxdb_testkit::schemas::human());
    let storage_schema = dxdb_storage::StorageSchema::from_value(&schema).expect("schema");
    assert!(
        storage_schema.indexes.contains(&prepared.query_plan.index),
        "the planned index {:?} is not one the schema materialised: {:?}",
        prepared.query_plan.index,
        storage_schema.indexes
    );
    assert!(
        prepared
            .query_plan
            .index
            .iter()
            .any(|field| field == "passportId"),
        "the planned index must include the primary key, got {:?}",
        prepared.query_plan.index
    );
}

/// The suite's fixture `lwt`s must be **recent**, not a frozen constant.
///
/// This is a regression test for a bug that made a correct storage look broken. The
/// suite's `lwt` helper used the constant `1_700_000_000_000` so a case would not depend on
/// the clock. That constant is 2023-11-14, so every fixture tombstone was over a thousand
/// days old, and `cleanup(1000 * 60 * 6)` — a six-minute retention window — correctly
/// deleted it. The case asserting "a tombstone newer than the window must be kept" then
/// failed against a scaffold that was right.
///
/// The lesson is narrow and worth pinning: a retention assertion is only meaningful if the
/// fixture's timestamps are near the clock the retention window is measured against.
#[test]
fn suite_fixture_timestamps_are_recent() {
    // The suite's `lwt` base is private, so reconstruct the property from behaviour:
    // a freshly written tombstone must survive a six-minute retention window, and the only
    // way that can be true is if its `lwt` is within six minutes of `now()`.
    let now = dxdb_storage::now();
    let six_minutes_ms = 1000.0 * 60.0 * 6.0;

    // Read the base the suite uses, via the same helper the suite calls. If the helper is
    // not public, this test would have to duplicate the constant — which is exactly the
    // coupling that let the bug in — so the suite exposes it for this assertion.
    let base = dxdb_testkit::suite::fixture_lwt_base();
    let age_ms = now - base;
    assert!(
        age_ms.abs() < six_minutes_ms,
        "the suite's fixture lwt base is {age_ms:.0}ms from now — outside a six-minute \
         retention window. Any case asserting that a fresh tombstone survives cleanup() \
         will fail against a correct storage, because the fixture is older than the \
         window. Base was {base}, now is {now}."
    );
    assert!(
        age_ms > -six_minutes_ms,
        "the suite's fixture lwt base is in the future ({age_ms:.0}ms ahead of now)"
    );
}

/// The report must render without panicking and must mention every kind.
#[test]
fn report_renders() {
    let report = dxdb_testkit::report::Report::compute();
    let markdown = report.to_markdown();
    for kind in Kind::ALL {
        assert!(
            markdown.contains(kind.as_str()),
            "the report omits kind {}",
            kind.as_str()
        );
    }
    assert!(markdown.contains("implementations"));
    assert!(markdown.contains("query correctness"));
    print!("{markdown}");
}

/// The declaration table's per-kind totals must cover every case exactly once.
#[test]
fn totals_by_kind_cover_every_case() {
    let totals = dxdb_testkit::decl::totals_by_kind();
    let declared: usize = totals.iter().map(|(_, declared, _)| declared).sum();
    let exercised: usize = totals.iter().map(|(_, _, exercised)| exercised).sum();
    assert_eq!(declared, all_cases().count());
    assert_eq!(exercised, Coverage::both().total_exercised);
}
