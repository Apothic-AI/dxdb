//! `schema-hashes.json` — `RxSchema.hash` and the defaulted schema it covers.
//!
//! The hash is of `JSON.stringify(defaultedSchema)`, so the file records both. That
//! pairing is the point: a bare hash tells a port only "wrong", while the defaulted
//! schema localises the mistake to schema defaulting (injected meta properties,
//! forced `additionalProperties: false`, rewritten indexes) or to hashing.
//!
//! `defaultedSchema` and `input` are kept as [`serde_json::Value`] on purpose. Key
//! order is part of what is being pinned — `normalizeRxJsonSchema` deep-sorts keys,
//! and the vectors assert that two schemas differing only in key order hash
//! identically — so re-serialising into a typed model would destroy the evidence.
//! Note that `serde_json` is configured with `preserve_order` in this workspace, so
//! value round-tripping keeps the original key order.

use serde::Deserialize;

use crate::FamilyFile;

/// The decoded `schema-hashes.json`.
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
    /// The recorded product, e.g. `schema->{defaultedSchema, sha256}`.
    pub format: String,
    /// The determinism stubs in force when the file was generated.
    pub determinism: Determinism,
    /// The hash construction, in prose, e.g. `sha256 hex of JSON.stringify(...)`.
    pub algorithm: String,
    /// Number of entries in [`Vectors::cases`].
    #[serde(rename = "caseCount")]
    pub case_count: usize,
    /// Fixture schemas that upstream refused to create. Expected empty.
    #[serde(rename = "failedSchemas")]
    pub failed_schemas: Vec<String>,
    /// One entry per schema that was hashed.
    pub cases: Vec<Case>,
    /// Pairs asserting which schema changes do and do not change the hash.
    pub relations: Vec<Relation>,
    /// Raw SHA-256 vectors for string inputs, independent of any schema.
    #[serde(rename = "hashStringCases")]
    pub hash_string_cases: Vec<HashStringCase>,
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
    /// Look up a case by its schema name.
    pub fn case(&self, name: &str) -> Option<&Case> {
        self.cases.iter().find(|c| c.name == name)
    }

    /// The input schema of the case a relation endpoint names.
    ///
    /// Relations resolve against [`Vectors::cases`], so the schema each endpoint refers
    /// to is already in this file. Exposing it here is what lets a consumer verify a
    /// relation without keeping its own copy of the schema literals: before this
    /// existed, a consumer had to reproduce the generator's literals by hand, and the
    /// two drifted the moment the relation endpoints changed.
    pub fn relation_input(&self, endpoint: &str) -> Option<&serde_json::Value> {
        self.case(endpoint).map(|c| &c.input)
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

/// One schema and its hash.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// The export name of the fixture schema, or a `synthetic:` label.
    pub name: String,
    /// Where the schema came from: `fixture` or `synthetic`.
    ///
    /// The synthetic ones each isolate a single defaulting step, so a failing port
    /// can bisect across them.
    pub origin: String,
    /// The schema as it was handed to `createRxSchema`. Key order preserved.
    pub input: serde_json::Value,
    /// `RxSchema.hash` — lowercase hex SHA-256.
    pub hash: String,
    /// `JSON.stringify(defaultedSchema).length`. Cheap cross-check for a port that
    /// produces the right hash bytes but the wrong serialisation.
    #[serde(rename = "hashInputLength")]
    pub hash_input_length: usize,
    /// The object that was stringified and hashed: the schema after
    /// `fillWithDefaultSettings` and `normalizeRxJsonSchema`. Key order preserved.
    #[serde(rename = "defaultedSchema")]
    pub defaulted_schema: serde_json::Value,
}

impl Case {
    /// Whether this schema was taken from upstream's fixtures rather than synthesised.
    pub fn is_fixture(&self) -> bool {
        self.origin == "fixture"
    }
}

/// A pair of schemas with the assertion about whether they hash the same.
///
/// Both endpoints name a case in [`Vectors::cases`], so a reader can always resolve
/// them. The negative pairs (`equal: false`) matter as much as the positive ones:
/// without them, a port that hashed everything identically would pass.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relation {
    /// What the relation asserts, e.g. `key-order-does-not-change-hash`.
    pub why: String,
    /// The `name` of the first case. Always present in [`Vectors::cases`].
    pub a: String,
    /// The `name` of the second case. Always present in [`Vectors::cases`].
    pub b: String,
    /// Whether the two cases hashed to the same value.
    pub equal: bool,
    /// The hash of case `a`, repeated here so the relation is readable on its own.
    #[serde(rename = "hashA")]
    pub hash_a: String,
    /// The hash of case `b`.
    #[serde(rename = "hashB")]
    pub hash_b: String,
}

impl Relation {
    /// Resolve both endpoints against a set of cases.
    ///
    /// Returns `None` if either name is missing, which the generator now refuses to
    /// emit; a `None` here means the file was edited by hand.
    pub fn resolve<'a>(&self, vectors: &'a Vectors) -> Option<(&'a Case, &'a Case)> {
        Some((vectors.case(&self.a)?, vectors.case(&self.b)?))
    }

    /// The two input schemas this relation compares, resolved from the cases.
    ///
    /// This is the accessor a hash-verification test wants: feed these two values to
    /// the implementation under test and assert the result matches [`Relation::equal`].
    /// Everything needed is in the file, so the test does not restate any schema.
    pub fn inputs<'a>(
        &self,
        vectors: &'a Vectors,
    ) -> Option<(&'a serde_json::Value, &'a serde_json::Value)> {
        Some((
            vectors.relation_input(&self.a)?,
            vectors.relation_input(&self.b)?,
        ))
    }

    /// Whether the recorded hashes agree with [`Relation::equal`].
    ///
    /// Recorded as a method rather than left to each consumer because this is the
    /// property the relation block exists to assert.
    pub fn is_self_consistent(&self) -> bool {
        self.equal == (self.hash_a == self.hash_b)
    }
}

/// A raw SHA-256 of a string input, with no schema involved.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HashStringCase {
    /// The exact input string.
    pub input: String,
    /// Lowercase hex SHA-256 of its UTF-8 bytes.
    pub sha256: String,
}
