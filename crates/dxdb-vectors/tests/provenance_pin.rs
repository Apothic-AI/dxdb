//! The provenance invariant: the vectors claim a commit, and that commit is the one
//! the project has documented.
//!
//! `testdata/vectors/*.json` each carry `rxdbCommit` and `rxdbVersion`. Those fields
//! are what makes "these vectors came from upstream" checkable at all, but on their own
//! they are self-reported: a regeneration against a different upstream commit would
//! happily write the new commit into the header, every consumer would keep passing,
//! and nothing would say that the oracle underneath the whole port had moved.
//!
//! So the claim is anchored in three places that cannot drift silently:
//!
//! 1. `NOTICE.md` — the human-facing, licence-relevant statement of which upstream we
//!    derive from. This is the root of truth.
//! 2. [`Provenance::EXPECTED_RXDB_COMMIT`] — the constant this crate exposes to
//!    consumers, so a test can assert the pin it was written against.
//! 3. `xtask/vectors/vendor.mjs`'s `PIN` — what the generator actually fetches.
//!
//! This file checks 1 against 2 and against the committed vectors. The generator
//! checks 1 against 3 (`assert_pin_matches_notice` in `vendor.mjs`), so a lane that
//! bumps the pin without updating `NOTICE.md` fails at generation time rather than
//! producing plausible-looking vectors from a commit nobody wrote down.

use dxdb_vectors::{VectorFamily, Vectors, ALL_FAMILIES};

/// The repository root, resolved from this crate's manifest directory.
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repository root is reachable from the crate manifest dir")
}

/// Pull the commit and version out of `NOTICE.md`.
///
/// The file states them in one sentence:
///
/// ```text
/// Upstream source pin: pubkey/rxdb at commit
/// 3b763ee17d62bc96cfae556ea0ccf92f9891c8c1 (2026-10-25), version 17.6.0.
/// ```
///
/// Parsed rather than duplicated as another constant, because a second copy of the pin
/// in Rust would be one more thing that can drift, and it would defeat the point of the
/// test. Deliberately strict about the shape: if `NOTICE.md` is reworded so this no
/// longer matches, that is a signal to look, not something to paper over with a looser
/// regex.
fn notice_pin() -> (String, String) {
    let path = repo_root().join("NOTICE");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\n\
             NOTICE.md documents the upstream pin; the provenance tests are meaningless \
             without it.",
            path.display()
        )
    });

    let commit = text
        .split_whitespace()
        .find(|word| word.len() == 40 && word.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| {
            panic!(
                "could not find a 40-hex upstream commit in {}; the provenance tests \
                 parse this file and will need updating if its wording changed",
                path.display()
            )
        })
        .to_string();

    // "version 17.6.0." -> "17.6.0". The trailing sentence period must not become part
    // of the captured version: `17.6.0.` is not a version, and comparing against it
    // would fail in a way that reads like the pin drifted when the parser was at fault.
    let version = text
        .split("version ")
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect::<String>()
                .trim_end_matches('.')
                .to_string()
        })
        .filter(|v| v.contains('.') && !v.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "could not find `version X.Y.Z` in {}; the provenance tests parse this \
                 file and will need updating if its wording changed",
                path.display()
            )
        });

    (commit, version)
}

#[test]
fn crate_pin_equals_the_documented_notice_pin() {
    let (notice_commit, notice_version) = notice_pin();

    assert_eq!(
        dxdb_vectors::Provenance::EXPECTED_RXDB_COMMIT,
        notice_commit,
        "dxdb-vectors::Provenance::EXPECTED_RXDB_COMMIT disagrees with NOTICE.md.\n\
         Consumers assert against this constant, so it has to track the documented pin. \
         If upstream was moved on purpose, update NOTICE.md, this constant, and \
         `xtask/vectors/vendor.mjs`'s PIN together, then regenerate with `make vectors`."
    );
    assert_eq!(
        dxdb_vectors::Provenance::EXPECTED_RXDB_VERSION,
        notice_version,
        "dxdb-vectors::Provenance::EXPECTED_RXDB_VERSION disagrees with NOTICE.md"
    );
}

#[test]
fn every_committed_vector_file_records_the_documented_pin() {
    let (notice_commit, notice_version) = notice_pin();
    let vectors = Vectors::discover();

    for family in ALL_FAMILIES {
        let p = match *family {
            VectorFamily::Jsnum => vectors
                .load::<dxdb_vectors::jsnum::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::IndexStrings => vectors
                .load::<dxdb_vectors::index_strings::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::SchemaHashes => vectors
                .load::<dxdb_vectors::schema_hashes::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::NormalisedQueries => vectors
                .load::<dxdb_vectors::normalised_queries::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::BulkWriteCategorization => vectors
                .load::<dxdb_vectors::bulk_write_categorization::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::SelectorMatching => vectors
                .load::<dxdb_vectors::selector_matching::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::ReplicationTranscripts => vectors
                .load::<dxdb_vectors::replication_transcripts::Vectors>(*family)
                .unwrap()
                .provenance(),
            VectorFamily::ErrorVectors => vectors
                .load::<dxdb_vectors::error_vectors::Vectors>(*family)
                .unwrap()
                .provenance(),
        };

        if *family == VectorFamily::Jsnum {
            // jsnum is Node-only and has no upstream provenance to record.
            continue;
        }

        assert_eq!(
            p.rxdb_commit.as_deref(),
            Some(notice_commit.as_str()),
            "{family} was generated from a different upstream commit than NOTICE.md \
             documents; regenerate with `make vectors` against the pinned checkout"
        );
        assert_eq!(
            p.rxdb_version.as_deref(),
            Some(notice_version.as_str()),
            "{family} records a different upstream version than NOTICE.md"
        );
        // And the file's own claim agrees with the crate's constant, which is what a
        // consumer actually asserts against.
        assert!(
            p.matches_documented_pin(),
            "{family} does not match Provenance::EXPECTED_RXDB_COMMIT/VERSION"
        );
    }
}
