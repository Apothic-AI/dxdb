//! `index-strings.json` — index strings from upstream `src/custom-index.ts`.
//!
//! Three things about this family are easy to get wrong when reading it by hand, and
//! each is encoded here rather than left to the consumer:
//!
//! 1. **`index` is not `Vec<String>`.** It is a list of field paths, and upstream's
//!    fixtures include one index whose entry is a literal number (`10`), which is
//!    exactly the case upstream refuses. Hence [`IndexEntry`], an untagged enum.
//! 2. **Cases are heterogeneous.** It is an internally tagged enum on `kind`, with a
//!    distinct variant per shape. Four shapes exist: an index string produced from a
//!    document, a declared index-string length, a pair of bound strings, and a
//!    recorded upstream rejection.
//! 3. **Rejections are cases, not gaps.** `caseCount` includes them. A family that
//!    silently dropped them would still show 896/900 and look complete.

use serde::Deserialize;

use crate::FamilyFile;

/// The decoded `index-strings.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vectors {
    /// The documentation comment the generator wrote into the file.
    #[serde(rename = "$comment")]
    pub comment: String,
    /// The generator that wrote the file, e.g. `xtask/vectors/generate.mjs`.
    pub generator: String,
    /// Informational Node version; `--check` ignores it, so it may differ locally.
    pub node: String,
    /// Upstream RxDB version the vectors came from. Absent for `jsnum`, which only
    /// needs Node.
    #[serde(default, rename = "rxdbVersion")]
    pub rxdb_version: Option<String>,
    /// Upstream RxDB commit the vectors came from. Same condition as `rxdb_version`.
    #[serde(default, rename = "rxdbCommit")]
    pub rxdb_commit: Option<String>,
    /// The recorded product, e.g. `schema+index+document->indexString`.
    pub format: String,
    /// The determinism stubs in force when the file was generated.
    pub determinism: Determinism,
    /// Every case, including rejections.
    #[serde(rename = "caseCount")]
    pub case_count: usize,
    /// Cases that produced an index string.
    #[serde(rename = "producedCaseCount")]
    pub produced_case_count: usize,
    /// Cases that recorded an upstream refusal.
    #[serde(rename = "rejectionCaseCount")]
    pub rejection_case_count: usize,
    /// Cases computed but omitted to keep the committed file small.
    ///
    /// Non-zero by design: the generator caps the file and strata-samples so no
    /// schema and no case shape disappears. A consumer must not read this as
    /// "coverage is incomplete for what I care about" without checking
    /// [`Vectors::cases`] first.
    #[serde(rename = "droppedCaseCount")]
    pub dropped_case_count: usize,
    /// Roll-up of the rejection cases, by refusal class.
    pub rejections: Vec<RejectionSummary>,
    /// Expected to be empty. A non-empty list means the generator met a refusal it
    /// did not anticipate, which is worth failing a test over.
    #[serde(rename = "unexpectedRejections")]
    pub unexpected_rejections: Vec<serde_json::Value>,
    /// The cases themselves.
    pub cases: Vec<Case>,
}

impl FamilyFile for Vectors {
    fn provenance(&self) -> crate::Provenance {
        crate::Provenance {
            comment: self.comment.clone(),
            generator: self.generator.clone(),
            node: self.node.clone(),
            rxdb_version: self.rxdb_version.clone(),
            rxdb_commit: self.rxdb_commit.clone(),
        }
    }
}

impl Vectors {
    /// The provenance header of this file.
    ///
    /// Inherent rather than trait-only so a consumer can call it without importing
    /// [`FamilyFile`]; the trait exists for the loader's generic bound.
    pub fn provenance(&self) -> crate::Provenance {
        FamilyFile::provenance(self)
    }
}

impl Vectors {
    /// Cases whose `kind` is `"index-string"`: a document in, an index string out.
    ///
    /// Named for the case kind, not for "everything that succeeded". The three
    /// non-rejection kinds are `index-string`, `index-string-length` and `bounds`, so
    /// this is a subset of the 896 cases that produced a value; see
    /// [`Vectors::non_rejection`] for that set.
    pub fn index_strings(&self) -> impl Iterator<Item = &IndexStringCase> {
        self.cases.iter().filter_map(|c| match c {
            Case::IndexString(c) => Some(c),
            _ => None,
        })
    }

    /// Cases of the `"index-string-length"` kind.
    pub fn index_string_lengths(&self) -> impl Iterator<Item = &IndexStringLengthCase> {
        self.cases.iter().filter_map(|c| match c {
            Case::IndexStringLength(c) => Some(c),
            _ => None,
        })
    }

    /// Cases of the `"bounds"` kind.
    pub fn bounds(&self) -> impl Iterator<Item = &BoundsCase> {
        self.cases.iter().filter_map(|c| match c {
            Case::Bounds(c) => Some(c),
            _ => None,
        })
    }

    /// Every case that produced a value, i.e. all three non-rejection kinds.
    pub fn non_rejection(&self) -> impl Iterator<Item = &Case> {
        self.cases
            .iter()
            .filter(|c| !matches!(c, Case::Rejection(_)))
    }

    /// Cases that recorded an upstream refusal.
    pub fn refused(&self) -> impl Iterator<Item = &RejectionCase> {
        self.cases.iter().filter_map(|c| match c {
            Case::Rejection(c) => Some(c),
            _ => None,
        })
    }

    /// Every distinct schema name appearing in any case.
    pub fn schema_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.cases.iter().map(Case::schema).collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// The determinism stubs the generator installed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Determinism {
    /// How `Math.random` was replaced.
    #[serde(rename = "mathRandom")]
    pub math_random: String,
    /// How `Date.now` was replaced.
    #[serde(rename = "dateNow")]
    pub date_now: String,
}

/// One entry of an index array: a field path, or (in the one broken fixture) a
/// literal number.
///
/// Untagged because the JSON has no tag: the array is heterogeneous by value. Do not
/// collapse this to `String` — the numeric entry is the entire reason upstream
/// refuses that schema, and stringifying it would erase the case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum IndexEntry {
    /// A field path such as `_deleted`, `firstName`, or `_meta.lwt`.
    Field(String),
    /// A non-string entry. Upstream crashes on these; this is how the case is
    /// recorded rather than lost.
    NonString(serde_json::Number),
}

impl IndexEntry {
    /// The field path when this entry is a string.
    pub fn as_field(&self) -> Option<&str> {
        match self {
            IndexEntry::Field(field) => Some(field),
            IndexEntry::NonString(_) => None,
        }
    }

    /// True when this entry is not a field path, i.e. the index is one upstream
    /// will reject.
    pub fn is_non_string(&self) -> bool {
        matches!(self, IndexEntry::NonString(_))
    }
}

impl std::fmt::Display for IndexEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexEntry::Field(field) => f.write_str(field),
            IndexEntry::NonString(number) => write!(f, "{number}"),
        }
    }
}

/// One recorded case, tagged by its `kind`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Case {
    /// An index string produced by running the monad over a document.
    #[serde(rename = "index-string")]
    IndexString(IndexStringCase),
    /// The declared length of an index, from `getIndexStringLength`.
    #[serde(rename = "index-string-length")]
    IndexStringLength(IndexStringLengthCase),
    /// A pair of bound strings from the lower/upper bound helpers.
    Bounds(BoundsCase),
    /// Upstream refused this index. Recorded behaviour, not a gap.
    Rejection(RejectionCase),
}

impl Case {
    /// The fixture schema this case belongs to.
    pub fn schema(&self) -> &str {
        match self {
            Case::IndexString(c) => &c.schema,
            Case::IndexStringLength(c) => &c.schema,
            Case::Bounds(c) => &c.schema,
            Case::Rejection(c) => &c.schema,
        }
    }

    /// The defaulted index this case was computed against.
    pub fn index(&self) -> &[IndexEntry] {
        match self {
            Case::IndexString(c) => &c.index,
            Case::IndexStringLength(c) => &c.index,
            Case::Bounds(c) => &c.index,
            Case::Rejection(c) => &c.index,
        }
    }
}

/// A value at a position of an index. The concrete JS type varies with the field's
/// declared schema type, so this is untagged by necessity.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum BoundValue {
    /// A string or boolean field's bound value.
    Text(String),
    /// A boolean field's bound value.
    Bool(bool),
    /// A numeric field's bound value.
    Number(f64),
    /// `null` or an absent bound.
    Null,
}

/// `kind: "index-string"`: one document in, one index string out.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexStringCase {
    /// The fixture schema name.
    pub schema: String,
    /// The defaulted index (`_deleted` prepended, primary key appended).
    pub index: Vec<IndexEntry>,
    /// The index field that was varied, or `None` for whole-index cases.
    #[serde(default)]
    pub field: Option<String>,
    /// Why this value was chosen, e.g. `at-minimum`, `above-maximum-clamped`.
    ///
    /// The clamping probes are labelled; their outputs must equal the corresponding
    /// `at-minimum` / `at-maximum` case, which is the invariant a port most often
    /// breaks.
    pub why: String,
    /// The document the monad was called with. Arbitrary JSON produced by the
    /// generator; `undefined` inputs appear as absent keys.
    pub document: serde_json::Value,
    /// The resulting index string.
    #[serde(rename = "indexString")]
    pub index_string: String,
}

/// `kind: "index-string-length"`: the declared length of an index.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexStringLengthCase {
    /// The fixture schema name.
    pub schema: String,
    /// The index whose length was computed.
    pub index: Vec<IndexEntry>,
    /// The length in code units, from `getIndexStringLength`.
    pub length: usize,
}

/// `kind: "bounds"`: the lower and upper bound strings for one index position.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundsCase {
    /// The fixture schema name.
    pub schema: String,
    /// The index the bounds were built for.
    pub index: Vec<IndexEntry>,
    /// The index field whose bound was varied.
    pub field: String,
    /// Which bound was probed, e.g. `lower-INDEX_MIN`, `lower-null`.
    pub why: String,
    /// The bound value used, if it is representable in JSON. Absent for the
    /// `lower-undefined` probe, which is precisely the case where the value is not.
    #[serde(default)]
    pub bound: Option<serde_json::Value>,
    /// `getStartIndexStringFromLowerBound` output.
    #[serde(rename = "lowerBoundString")]
    pub lower_bound_string: String,
    /// `getStartIndexStringFromUpperBound` output.
    #[serde(rename = "upperBoundString")]
    pub upper_bound_string: String,
}

/// `kind: "rejection"`: upstream refused this index, with the observable error.
///
/// Two refusal shapes exist and both are recorded: a typed RxDB error (code `CI1`)
/// and a bare `TypeError` from an unguarded property access. A port must reproduce
/// both, as a typed error rather than a panic, which is why
/// [`RejectionCase::rust_expectation`] is part of the data.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectionCase {
    /// The fixture schema name.
    pub schema: String,
    /// The index that was refused.
    pub index: Vec<IndexEntry>,
    /// The refusal class, e.g. `upstream-rejects-missing-indexed-field`.
    pub why: String,
    /// Which upstream entry points were attempted.
    #[serde(rename = "entryPointsTried")]
    pub entry_points_tried: Vec<String>,
    /// Always `true`; present so a reader can filter without matching on `kind`.
    pub rejected: bool,
    /// The thrown constructor name, e.g. `TypeError` or `RxError`.
    #[serde(rename = "errorName")]
    pub error_name: String,
    /// The RxDB error code when upstream raised a typed error, else `null`.
    #[serde(rename = "errorCode")]
    pub error_code: Option<String>,
    /// Whether the error carried the `rxdb` marker.
    #[serde(rename = "rxdbError")]
    pub rxdb_error: bool,
    /// The documentation URL upstream attached, when there was one.
    #[serde(rename = "errorUrl")]
    pub error_url: Option<String>,
    /// Structured error parameters, e.g. `{"fieldName": "address.apartment"}`.
    pub parameters: Option<serde_json::Value>,
    /// The raw message, flattened. For the untyped cases this is the only hint about
    /// which property access failed, so it is kept verbatim.
    #[serde(rename = "rawMessage")]
    pub raw_message: String,
    /// Which entry point was entered when the refusal happened.
    #[serde(rename = "atConstructor")]
    pub at_constructor: String,
    /// The generator's description of the refusal class.
    #[serde(rename = "upstreamBehaviour")]
    pub upstream_behaviour: UpstreamBehaviour,
    /// What the Rust port is expected to do with this input.
    #[serde(rename = "rustExpectation")]
    pub rust_expectation: String,
}

/// The generator's classification of an upstream refusal.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamBehaviour {
    /// The refusal class name.
    pub class: String,
    /// Why upstream refuses, in prose.
    pub detail: String,
}

/// One line of the rejection roll-up.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectionSummary {
    /// The fixture schema name.
    pub schema: String,
    /// The refused index.
    pub index: Vec<IndexEntry>,
    /// The refusal class.
    pub class: String,
    /// RxDB error code where there was one.
    #[serde(rename = "errorCode")]
    pub error_code: Option<String>,
    /// Thrown constructor name.
    #[serde(rename = "errorName")]
    pub error_name: String,
}
