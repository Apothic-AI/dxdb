//! Locating the vectors directory, provenance, and the per-family entry point.
//!
//! Path resolution has one job and one trap. The job: a consumer crate may live at
//! any nesting depth under `crates/`, and tests run with the manifest dir of the crate
//! under test, not this one. The trap: `env!("CARGO_MANIFEST_DIR")` inside this crate
//! expands to *this crate's* directory, which is always `crates/dxdb-vectors`, so a
//! single fixed relative path is correct for every consumer. That is why this lives
//! here and not in each consumer's test.
//!
//! A second candidate is tried after the first, so the crate also works when it is
//! vendored into a tree whose layout differs slightly. If neither exists the error
//! lists both paths rather than guessing.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::VectorError;

/// Which vector family to load.
///
/// The `Display` output is the file stem, which is also the name the generator prints
/// and the name used in `make vectors-check` output, so a failure message can be
/// pasted straight into a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VectorFamily {
    /// `jsnum.json` — `String(x)` / `JSON.stringify(x)` from Node.
    Jsnum,
    /// `index-strings.json` — `getIndexableStringMonad` output and bound helpers.
    IndexStrings,
    /// `schema-hashes.json` — `RxSchema.hash` and the defaulted schema it covers.
    SchemaHashes,
    /// `normalised-queries.json` — `normalizeMangoQuery` output and query plans.
    NormalisedQueries,
    /// `bulk-write-categorization.json` — `categorizeBulkWriteRows` output.
    BulkWriteCategorization,
    /// `selector-matching.json` — which documents match which selectors.
    SelectorMatching,
    /// `replication-transcripts.json` — replicated state and checkpoints.
    ReplicationTranscripts,
    /// `error-vectors.json` — how upstream refuses, with each error's code.
    ErrorVectors,
}

impl VectorFamily {
    /// The file stem, matching the generator's output name.
    pub fn file_stem(self) -> &'static str {
        match self {
            VectorFamily::Jsnum => "jsnum",
            VectorFamily::IndexStrings => "index-strings",
            VectorFamily::SchemaHashes => "schema-hashes",
            VectorFamily::NormalisedQueries => "normalised-queries",
            VectorFamily::BulkWriteCategorization => "bulk-write-categorization",
            VectorFamily::SelectorMatching => "selector-matching",
            VectorFamily::ReplicationTranscripts => "replication-transcripts",
            VectorFamily::ErrorVectors => "error-vectors",
        }
    }

    /// The file name including the `.json` extension.
    pub fn file_name(self) -> String {
        format!("{}.json", self.file_stem())
    }
}

impl std::fmt::Display for VectorFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.file_stem())
    }
}

/// Where the vectors were generated from, copied out of each file's header.
///
/// The same value appears in every family, because one `make vectors` run writes them
/// all. It exists so a test can assert which oracle it ran against instead of assuming
/// whatever is checked out.
///
/// Owned rather than borrowed so a consumer can hold it after the document is dropped,
/// which is what an assertion against the documented pin wants to do. Five short
/// strings is not worth a lifetime parameter at every call site.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// The documentation comment the generator wrote into the file.
    #[serde(rename = "$comment")]
    pub comment: String,
    /// The generator that wrote the file, e.g. `xtask/vectors/generate.mjs`.
    pub generator: String,
    /// Informational Node version. Excluded from `make vectors-check` drift
    /// comparison, so it may legitimately differ between a local run and CI.
    pub node: String,
    /// Upstream RxDB version the vectors came from. `None` for `jsnum`, which only
    /// needs Node.
    #[serde(default, rename = "rxdbVersion")]
    pub rxdb_version: Option<String>,
    /// Upstream RxDB commit the vectors came from. Same condition as `rxdb_version`.
    #[serde(default, rename = "rxdbCommit")]
    pub rxdb_commit: Option<String>,
}

impl Provenance {
    /// The documented upstream pin this crate was written against.
    ///
    /// Mirrors `NOTICE.md`. A test that cares about behavioural parity should assert
    /// `provenance.rxdb_commit.as_deref() == Some(Provenance::EXPECTED_RXDB_COMMIT)`,
    /// so that a silently swapped oracle fails instead of quietly testing something
    /// else.
    pub const EXPECTED_RXDB_COMMIT: &'static str = "3b763ee17d62bc96cfae556ea0ccf92f9891c8c1";

    /// The documented upstream version, mirroring `NOTICE.md`.
    pub const EXPECTED_RXDB_VERSION: &'static str = "17.6.0";

    /// True when this family was generated from the documented upstream pin.
    ///
    /// `jsnum` legitimately has no upstream fields, so it reports `false`; use
    /// [`Provenance::is_upstream`] to distinguish "wrong pin" from "no pin".
    pub fn matches_documented_pin(&self) -> bool {
        self.rxdb_commit.as_deref() == Some(Self::EXPECTED_RXDB_COMMIT)
            && self.rxdb_version.as_deref() == Some(Self::EXPECTED_RXDB_VERSION)
    }

    /// Whether this family records upstream provenance at all.
    pub fn is_upstream(&self) -> bool {
        self.rxdb_commit.is_some()
    }

    /// The upstream commit, or a panic naming the generator that forgot it.
    pub fn expect_rxdb_commit(&self) -> &str {
        self.rxdb_commit.as_deref().unwrap_or_else(|| {
            panic!(
                "family generated by {} has no rxdbCommit; only jsnum is expected to omit it",
                self.generator
            )
        })
    }
}

/// Implemented by each family's top-level document type.
///
/// The bound exists so [`Vectors::load`] has one signature for seven types. Consumers
/// do not need to import this trait: every family struct also has an inherent
/// `provenance()` method, so `family.provenance()` works on its own. That is
/// deliberate — a reader reaching for provenance should not have to discover a trait
/// to get it.
pub trait FamilyFile: for<'de> Deserialize<'de> {
    /// The provenance header of this family.
    fn provenance(&self) -> Provenance;
}

/// A resolved vectors directory.
///
/// Construct with [`Vectors::discover`] for the normal case, or [`Vectors::at`] to
/// point at a different directory (a regenerated copy, a CI artifact). Loading is per
/// family and not cached: these files are read in tests, where re-reading a few
/// hundred kilobytes costs nothing and a stale cache would be a bug.
#[derive(Debug, Clone)]
pub struct Vectors {
    dir: PathBuf,
}

impl Vectors {
    /// The directory name every family file lives in.
    pub const DIR_NAME: &'static str = "vectors";

    /// Resolve the committed vectors directory from this crate's manifest dir.
    ///
    /// Checks, in order:
    /// 1. `<this crate>/../../testdata/vectors` (the repository layout)
    /// 2. `<this crate>/testdata/vectors` (a vendored layout)
    ///
    /// # Panics
    ///
    /// Panics with every path it tried. This is a test-time convenience: a test that
    /// cannot find its own committed data has nothing useful to assert, and a silent
    /// empty corpus would be worse. Use [`Vectors::try_discover`] where a recoverable
    /// error is wanted.
    pub fn discover() -> Self {
        Self::try_discover().unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible [`Vectors::discover`].
    pub fn try_discover() -> Result<Self, VectorError> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let candidates = [
            root.join("..")
                .join("..")
                .join("testdata")
                .join(Self::DIR_NAME),
            root.join("testdata").join(Self::DIR_NAME),
        ];
        for candidate in &candidates {
            if candidate.is_dir() {
                return Ok(Self {
                    dir: candidate.clone(),
                });
            }
        }
        Err(VectorError::DirectoryNotFound {
            tried: candidates.to_vec(),
        })
    }

    /// Use an explicit directory. Useful for testing a regenerated copy.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory in use.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The path for one family, whether or not it exists.
    pub fn path(&self, family: VectorFamily) -> PathBuf {
        self.dir.join(family.file_name())
    }

    /// Read and decode one family.
    pub fn load<T: FamilyFile>(&self, family: VectorFamily) -> Result<T, VectorError> {
        let path = self.path(family);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(VectorError::FileMissing { family, path });
            }
            Err(source) => return Err(VectorError::Read { path, source }),
        };
        serde_json::from_str(&text).map_err(|source| VectorError::Decode {
            family,
            path,
            source,
        })
    }

    /// Number of cases in a family, without naming the family's struct type.
    ///
    /// For `replication-transcripts`, which counts snapshots rather than cases, this
    /// counts recorded steps. Kept as its own entry point so the case-count guard does
    /// not have to know each family's shape.
    pub fn case_count(&self, family: VectorFamily) -> Result<usize, VectorError> {
        match family {
            VectorFamily::Jsnum => Ok(self.load::<crate::jsnum::Vectors>(family)?.cases.len()),
            VectorFamily::IndexStrings => Ok(self
                .load::<crate::index_strings::Vectors>(family)?
                .cases
                .len()),
            VectorFamily::SchemaHashes => Ok(self
                .load::<crate::schema_hashes::Vectors>(family)?
                .cases
                .len()),
            VectorFamily::NormalisedQueries => Ok(self
                .load::<crate::normalised_queries::Vectors>(family)?
                .cases
                .len()),
            VectorFamily::BulkWriteCategorization => Ok(self
                .load::<crate::bulk_write_categorization::Vectors>(family)?
                .cases
                .len()),
            VectorFamily::SelectorMatching => Ok(self
                .load::<crate::selector_matching::Vectors>(family)?
                .cases
                .len()),
            VectorFamily::ReplicationTranscripts => Ok(self
                .load::<crate::replication_transcripts::Vectors>(family)?
                .case_count()),
            VectorFamily::ErrorVectors => Ok(self
                .load::<crate::error_vectors::Vectors>(family)?
                .raised_cases
                .len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stem_and_name_agree() {
        for family in crate::ALL_FAMILIES {
            assert_eq!(family.file_name(), format!("{}.json", family.file_stem()));
            assert_eq!(family.to_string(), family.file_stem());
            assert!(!family.file_stem().ends_with(".json"));
        }
    }

    #[test]
    fn discover_finds_the_committed_directory() {
        let vectors = Vectors::try_discover().expect("vectors directory should be discoverable");
        assert!(
            vectors.dir().ends_with("testdata/vectors"),
            "unexpected vectors dir: {}",
            vectors.dir().display()
        );
        for family in crate::ALL_FAMILIES {
            assert!(
                vectors.path(*family).is_file(),
                "{family} file missing at {}",
                vectors.path(*family).display()
            );
        }
    }

    #[test]
    fn at_overrides_the_directory() {
        let vectors = Vectors::at("/tmp/definitely-not-vectors");
        assert_eq!(vectors.dir(), Path::new("/tmp/definitely-not-vectors"));
        let err = vectors
            .load::<crate::jsnum::Vectors>(VectorFamily::Jsnum)
            .expect_err("loading from a bogus directory must fail");
        assert!(
            matches!(err, VectorError::FileMissing { .. }),
            "expected FileMissing, got {err:?}"
        );
    }
}
