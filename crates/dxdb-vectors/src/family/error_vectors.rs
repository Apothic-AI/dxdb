//! `error-vectors.json` — how upstream refuses, rather than what it produces.
//!
//! Two layers, and the distinction is the whole point:
//!
//! - **`raisedCases`** — inputs driven through the bundled upstream, with each error's
//!   observable identity. This is the layer a port can be diffed against directly.
//! - **`declaredCodes`** — upstream's full `ERROR_MESSAGES` vocabulary (219 entries at
//!   this pin), which is the authority on *which codes exist*.
//!
//! A code in the declared layer is not evidence that upstream raises it from anything
//! reachable here: half the codes belong to the dev-mode plugin, a live database, or a
//! browser storage. Each declared code therefore carries `reachable`, and only the ones
//! this generator actually raised are `true`. Treating the whole 219 as verified
//! behaviour would be exactly the "looks like evidence" failure the relations block in
//! `schema-hashes` had.
//!
//! Note that three raised cases carry **no** RxDB code at all (`errorCode: null`).
//! Those are mingo and `RegExp` failures: upstream surfaces them untyped. A port that
//! returns a tidy typed error there is not more correct, it is divergent, and a caller
//! that checks for a code would find one where RxDB gives none.

use serde::Deserialize;

use crate::loader::{FamilyFile, Provenance};

/// The decoded `error-vectors.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vectors {
    /// The documentation comment the generator wrote into the file.
    #[serde(rename = "$comment")]
    pub comment: String,
    /// The generator that wrote the file.
    pub generator: String,
    /// Informational Node version; `--check` ignores it.
    pub node: String,
    /// Upstream RxDB version the vectors came from.
    #[serde(default, rename = "rxdbVersion")]
    pub rxdb_version: Option<String>,
    /// Upstream RxDB commit the vectors came from.
    #[serde(default, rename = "rxdbCommit")]
    pub rxdb_commit: Option<String>,
    /// The recorded product.
    pub format: String,
    /// The determinism stubs in force when the file was generated.
    pub determinism: Determinism,
    /// Number of entries in [`Vectors::raised_cases`].
    #[serde(rename = "raisedCaseCount")]
    pub raised_case_count: usize,
    /// How many raised cases carry a real RxDB code.
    #[serde(rename = "typedCaseCount")]
    pub typed_case_count: usize,
    /// How many raised cases threw an untyped error (no RxDB code exists for them).
    #[serde(rename = "untypedCaseCount")]
    pub untyped_case_count: usize,
    /// Number of entries in [`Vectors::declared_codes`].
    #[serde(rename = "declaredCodeCount")]
    pub declared_code_count: usize,
    /// The codes the task named explicitly, with an honest reachability note for each.
    ///
    /// A map rather than an array because the keys are stable labels (`SC20`, `DXE1`,
    /// `DB family`) that a reader looks up directly.
    #[serde(rename = "namedCodes")]
    pub named_codes: std::collections::BTreeMap<String, NamedCode>,
    /// Codes this generator actually raised, with their error identity.
    #[serde(rename = "raisedCases")]
    pub raised_cases: Vec<RaisedCase>,
    /// The whole declared vocabulary, alphabetically.
    #[serde(rename = "declaredCodes")]
    pub declared_codes: Vec<DeclaredCode>,
}

impl FamilyFile for Vectors {
    fn provenance(&self) -> Provenance {
        Provenance {
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

    /// Look up a raised case by its label, e.g. `index-names-a-field-not-in-schema`.
    pub fn raised(&self, why: &str) -> Option<&RaisedCase> {
        self.raised_cases.iter().find(|c| c.why == why)
    }

    /// Look up a declared code, e.g. `SC20`.
    pub fn declared(&self, code: &str) -> Option<&DeclaredCode> {
        self.declared_codes.iter().find(|c| c.code == code)
    }

    /// The named-code note for a code, e.g. `DXE1`.
    pub fn named_code(&self, code: &str) -> Option<&NamedCode> {
        self.named_codes.get(code)
    }

    /// Whether upstream has a declared code with this name.
    ///
    /// The check a port needs before returning a code of its own: a code that is not in
    /// this vocabulary did not come from upstream.
    pub fn is_declared(&self, code: &str) -> bool {
        self.declared(code).is_some()
    }

    /// Raised cases that carry a real RxDB code.
    pub fn typed(&self) -> impl Iterator<Item = &RaisedCase> {
        self.raised_cases.iter().filter(|c| c.error_code.is_some())
    }

    /// Raised cases where upstream throws an untyped error.
    pub fn untyped(&self) -> impl Iterator<Item = &RaisedCase> {
        self.raised_cases.iter().filter(|c| c.error_code.is_none())
    }

    /// Declared codes this generator actually raised.
    pub fn reachable_codes(&self) -> impl Iterator<Item = &DeclaredCode> {
        self.declared_codes.iter().filter(|c| c.reachable)
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

/// A code the task named, and whether this generator could raise it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedCode {
    /// Whether the generator raised this code from a reachable entry point.
    pub raised: bool,
    /// For `raised: false`, exactly why not. For `raised: true` this is absent.
    #[serde(default)]
    pub why: Option<String>,
}

/// One input that makes upstream raise, with what it raised.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RaisedCase {
    /// The input's label, e.g. `primary-key-without-maxLength`.
    pub why: String,
    /// The code the generator asserted. `None` when upstream throws untyped.
    #[serde(rename = "expectedCode")]
    pub expected_code: Option<String>,
    /// Why upstream refuses this input.
    pub detail: String,
    /// What the Rust port is expected to do.
    #[serde(rename = "rustExpectation")]
    pub rust_expectation: String,
    /// The thrown constructor, e.g. `RxError`, `MingoError2`, `SyntaxError`.
    #[serde(rename = "errorName")]
    pub error_name: String,
    /// The RxDB code, or `None` when upstream throws an untyped error.
    #[serde(rename = "errorCode")]
    pub error_code: Option<String>,
    /// Whether the error carried the `rxdb` marker.
    #[serde(rename = "rxdbError")]
    pub rxdb_error: bool,
    /// The documentation URL upstream attached.
    #[serde(rename = "errorUrl")]
    pub error_url: Option<String>,
    /// Upstream's structured payload, verbatim.
    pub parameters: Option<serde_json::Value>,
    /// The keys of [`RaisedCase::parameters`], sorted. A port that returns the right
    /// code without the payload cannot be acted on, so the keys are asserted directly.
    #[serde(rename = "parameterKeys")]
    pub parameter_keys: Vec<String>,
    /// The raw message, truncated. Kept because for untyped failures it is the only
    /// description of what went wrong.
    #[serde(rename = "rawMessage")]
    pub raw_message: String,
}

/// One entry of upstream's declared error vocabulary.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredCode {
    /// The code, e.g. `SC20`.
    pub code: String,
    /// Upstream's human-readable message, or `None` if the entry has none.
    pub message: Option<String>,
    /// The documentation URL for this code, where upstream provides one.
    pub docs: Option<String>,
    /// Whether this generator actually raised this code.
    ///
    /// `false` means "declared upstream, not verified here": the raiser needs the
    /// dev-mode plugin, a live database, or a browser storage.
    pub reachable: bool,
}
