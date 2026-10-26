//! `selector-matching.json` — which documents match which selectors.
//!
//! The differential oracle for the M1 matcher, and the input to its property test.
//! The corpus is embedded in the file so a Rust test runs over exactly the documents
//! Node ran over, instead of generating its own and hoping they match.
//!
//! Reading notes:
//!
//! - **`selector` and `normalizedSelector` differ for shorthand.** `{firstName:
//!   "alice"}` reaches mingo as `{firstName: {$eq: "alice"}}`. Both are recorded; the
//!   normalised one is what the matcher actually saw.
//! - **Matching cases and refusals share the array.** Refusals carry `kind:
//!   "rejection"`, so [`Case`] is an untagged enum discriminated on the presence of
//!   the rejection marker rather than on `kind` alone (matching cases have no `kind`).
//! - **Zero matches is meaningful.** Cases such as `range-inverted`, `in-empty` and
//!   `and-with-impossible-arm` legitimately match nothing; do not treat an empty
//!   `matchingIds` as a skipped case.

use serde::Deserialize;

use crate::FamilyFile;

/// The decoded `selector-matching.json`.
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
    /// The recorded product, e.g. `documents+selector->matchingIds`.
    pub format: String,
    /// The determinism stubs in force when the file was generated.
    pub determinism: Determinism,
    /// The closed operator set upstream registers in `getMingoQuery`.
    ///
    /// Anything outside this list is *not* supported by upstream's matcher, which is
    /// why `$all` and `$where` appear as refusals below rather than as working cases.
    #[serde(rename = "registeredOperators")]
    pub registered_operators: Vec<String>,
    /// The document corpus the selectors were evaluated against.
    pub corpus: Vec<serde_json::Value>,
    /// Total number of entries in [`Vectors::cases`], including refusals.
    #[serde(rename = "caseCount")]
    pub case_count: usize,
    /// Cases that produced a match list.
    #[serde(rename = "matchingCaseCount")]
    pub matching_case_count: usize,
    /// Cases where upstream refused the selector.
    #[serde(rename = "rejectionCaseCount")]
    pub rejection_case_count: usize,
    /// Expected to be empty; non-empty means an unexpected refusal.
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
    /// The documents, keyed by their `id` field.
    ///
    /// The corpus carries `id` as a plain field, so this is a convenience rather
    /// than something the JSON structure provides.
    pub fn corpus_by_id(&self) -> std::collections::BTreeMap<&str, &serde_json::Value> {
        self.corpus
            .iter()
            .filter_map(|doc| doc.get("id").and_then(|id| id.as_str()).map(|id| (id, doc)))
            .collect()
    }

    /// Cases that produced a match list.
    pub fn matching(&self) -> impl Iterator<Item = &MatchingCase> {
        self.cases.iter().filter_map(|c| match c {
            Case::Matching(c) => Some(c),
            Case::Rejection(_) => None,
        })
    }

    /// Cases upstream refused.
    pub fn refused(&self) -> impl Iterator<Item = &RejectionCase> {
        self.cases.iter().filter_map(|c| match c {
            Case::Rejection(c) => Some(c),
            Case::Matching(_) => None,
        })
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
    /// How the corpus was generated.
    pub corpus: String,
}

/// One recorded case: a selector that matched, or one upstream refused.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Case {
    /// Upstream refused the selector. Listed first so the `rejected` marker decides
    /// the variant, since matching cases carry no tag field at all.
    Rejection(RejectionCase),
    /// A selector that was evaluated.
    Matching(MatchingCase),
}

/// A selector and the ids it matched.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchingCase {
    /// The selector shape label, e.g. `eq-null`, `elemMatch`, `regex-ci`.
    pub why: String,
    /// The selector as written. Shorthand equality is still shorthand here.
    pub selector: serde_json::Value,
    /// The selector as mingo received it, after `normalizeMangoQuery`.
    #[serde(rename = "normalizedSelector")]
    pub normalized_selector: serde_json::Value,
    /// Ids of matching documents, in corpus order.
    #[serde(rename = "matchingIds")]
    pub matching_ids: Vec<String>,
    /// `matching_ids.len()`, recorded so a reader can assert without counting.
    #[serde(rename = "matchCount")]
    pub match_count: usize,
}

/// A selector upstream refused, with its observable error.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectionCase {
    /// The selector shape label.
    pub why: String,
    /// The selector that was refused.
    pub selector: serde_json::Value,
    /// Always `"rejection"`; the discriminator for the untagged enum.
    pub kind: String,
    /// Always `true`.
    pub rejected: bool,
    /// The thrown constructor name, e.g. `MingoError2`.
    #[serde(rename = "errorName")]
    pub error_name: String,
    /// The error code, when one exists. `null` for mingo's own errors.
    #[serde(rename = "errorCode")]
    pub error_code: Option<String>,
    /// Whether the error carried the `rxdb` marker. `false` for mingo errors.
    #[serde(rename = "rxdbError")]
    pub rxdb_error: bool,
    /// The raw message, e.g. `unknown query operator $all`.
    #[serde(rename = "rawMessage")]
    pub raw_message: String,
    /// The generator's classification of the refusal.
    #[serde(rename = "upstreamBehaviour")]
    pub upstream_behaviour: UpstreamBehaviour,
    /// What the Rust port is expected to do with this selector.
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
    /// What the Rust port is expected to do with this selector.
    ///
    /// Duplicated at this level *and* on the enclosing case in the generator's output;
    /// both are modelled because both are in the file, and dropping either would mean
    /// the reader no longer matches what Node wrote.
    #[serde(rename = "rustExpectation")]
    pub rust_expectation: String,
}
