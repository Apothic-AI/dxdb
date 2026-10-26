//! `normalised-queries.json` — `normalizeMangoQuery` output and query plans.
//!
//! Each case records three levels: the query the caller wrote, the query after
//! normalisation, and the plan `getQueryPlan` derived from it. A port can therefore
//! be checked at whichever level it first disagrees.
//!
//! Two shapes here need care:
//!
//! - **`startKeys` / `endKeys` are heterogeneous.** They hold strings, booleans and
//!   finite numbers, plus `INDEX_MIN` (a number) and `INDEX_MAX` (a one-character
//!   string). [`PlanKey`] is untagged for that reason.
//! - **`sort` entries are single-key objects.** `{"age": "asc"}` has a field name in
//!   the key position, so it cannot be a struct field; [`SortPart`] models it as a
//!   one-entry map.

use serde::Deserialize;

use crate::FamilyFile;

/// The decoded `normalised-queries.json`.
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
    /// The recorded product, e.g. `schema+query->{normalizedQuery, plan}`.
    pub format: String,
    /// The determinism stubs in force when the file was generated.
    pub determinism: Determinism,
    /// The `INDEX_MIN` / `INDEX_MAX` boundary values and their JSON behaviour.
    pub sentinels: Sentinels,
    /// Number of entries in [`Vectors::cases`].
    #[serde(rename = "caseCount")]
    pub case_count: usize,
    /// Queries upstream rejected. Every entry carries the expected-failure marker;
    /// see [`Failure::expected`].
    pub failures: Vec<Failure>,
    /// The cases themselves.
    pub cases: Vec<Case>,
    /// Sort-comparator behaviour over a fixed document set.
    pub orderings: Vec<Ordering>,
    /// Selector-matcher behaviour over a fixed document set.
    pub matchers: Vec<Matcher>,
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
    /// Cases that produced a plan, in file order.
    pub fn planned(&self) -> impl Iterator<Item = &PlanCase> {
        self.cases.iter().filter_map(|c| match c {
            Case::Planned(c) => Some(c),
            Case::Rejected(_) => None,
        })
    }

    /// Cases upstream rejected.
    pub fn rejected(&self) -> impl Iterator<Item = &RejectedCase> {
        self.cases.iter().filter_map(|c| match c {
            Case::Rejected(c) => Some(c),
            Case::Planned(_) => None,
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
}

/// The sentinel values that define the ends of every index range.
///
/// `INDEX_MIN` is `Number.MIN_SAFE_INTEGER` rather than `-Infinity` specifically so
/// that a query plan survives being shipped to a remote storage as JSON. The file
/// records both facts, including the counter-example.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sentinels {
    /// The lower sentinel. Always [`f64`]-representable; compare with
    /// [`Sentinels::INDEX_MIN_VALUE`].
    #[serde(rename = "INDEX_MIN")]
    pub index_min: f64,
    /// The upper sentinel: the single character `U+FFFF`.
    #[serde(rename = "INDEX_MAX")]
    pub index_max: String,
    /// Code point(s) of [`Sentinels::index_max`].
    #[serde(rename = "INDEX_MAXCodePoints")]
    pub index_max_code_points: Vec<u32>,
    /// `JSON.stringify(INDEX_MIN)`, as a string.
    #[serde(rename = "indexMinJson")]
    pub index_min_json: String,
    /// Whether `INDEX_MIN` survives a JSON round trip unchanged.
    #[serde(rename = "indexMinRoundTrips")]
    pub index_min_round_trips: bool,
    /// `JSON.stringify(-Infinity)`, which is `"null"`: the reason `-Infinity` was
    /// not chosen.
    #[serde(rename = "minusInfinityJson")]
    pub minus_infinity_json: String,
    /// Whether `-Infinity` survives a JSON round trip. False, by construction.
    #[serde(rename = "minusInfinityRoundTrips")]
    pub minus_infinity_round_trips: bool,
    /// Whether `INDEX_MIN` equals `Number.MIN_SAFE_INTEGER`.
    #[serde(rename = "isMinSafeInteger")]
    pub is_min_safe_integer: bool,
}

impl Sentinels {
    /// The lower sentinel as an integer, for exact comparison.
    pub const INDEX_MIN_VALUE: i64 = -9_007_199_254_740_991;
}

/// A query upstream refused.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    /// The fixture schema name.
    pub schema: String,
    /// The query shape label.
    pub why: String,
    /// The error message.
    pub reason: String,
    /// The generator's explanation, or `null` when it was not expected.
    ///
    /// A `null` here means the generator met a failure it did not anticipate; the
    /// whole set currently comes from one fixture whose declared index contains a
    /// literal number.
    pub expected: Option<String>,
}

/// One recorded query.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Case {
    /// The query normalised and planned successfully.
    Planned(PlanCase),
    /// Upstream refused the query.
    Rejected(RejectedCase),
}

/// A query that normalised and planned.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCase {
    /// The fixture schema name.
    pub schema: String,
    /// The query shape label.
    pub why: String,
    /// The query as written, before normalisation.
    pub query: serde_json::Value,
    /// `normalizeMangoQuery` output. This is what a storage sees.
    #[serde(rename = "normalizedQuery")]
    pub normalized_query: serde_json::Value,
    /// `getQueryPlan` output for the normalised query.
    pub plan: Plan,
    /// The plan serialised with `JSON.stringify`, verbatim.
    #[serde(rename = "planJson")]
    pub plan_json: String,
    /// Whether parsing [`PlanCase::plan_json`] yields the same value. True for every
    /// recorded case; the field exists to make that an assertion.
    #[serde(rename = "planRoundTripEqual")]
    pub plan_round_trip_equal: bool,
}

/// A query upstream refused.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectedCase {
    /// The fixture schema name.
    pub schema: String,
    /// The query shape label.
    pub why: String,
    /// The query as written.
    pub query: serde_json::Value,
    /// The error message.
    pub rejected: String,
    /// Whether this failure was anticipated. Currently `true` for all cases, all
    /// from `compoundIndexNoString`.
    #[serde(rename = "expectedUpstreamFailure")]
    pub expected_upstream_failure: bool,
}

/// A single-key sort entry, e.g. `{"age": "asc"}`.
///
/// The field name is in the key position, so this cannot be a plain struct. Wrapped
/// rather than flattened to a tuple to keep the field name available.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct SortPart(pub std::collections::BTreeMap<String, String>);

impl SortPart {
    /// The field being sorted on.
    pub fn field(&self) -> &str {
        self.0.keys().next().map(String::as_str).unwrap_or_default()
    }

    /// The direction, `"asc"` or `"desc"`.
    pub fn direction(&self) -> &str {
        self.0
            .values()
            .next()
            .map(String::as_str)
            .unwrap_or_default()
    }
}

/// A value at a position of a plan's start or end keys.
///
/// Untagged because the concrete type depends on the indexed field's schema type:
/// strings for string fields, booleans for boolean fields, numbers for numeric
/// fields, and `INDEX_MIN` / `INDEX_MAX` at the open ends.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum PlanKey {
    /// A string bound, including `INDEX_MAX` (`"\u{ffff}"`).
    Text(String),
    /// A boolean bound.
    Bool(bool),
    /// A numeric bound, including `INDEX_MIN`.
    Number(f64),
    /// An absent bound.
    Null,
}

impl PlanKey {
    /// True when this key is the lower sentinel.
    pub fn is_index_min(&self) -> bool {
        matches!(self, PlanKey::Number(n) if n.trunc() == Sentinels::INDEX_MIN_VALUE as f64)
    }

    /// True when this key is the upper sentinel.
    pub fn is_index_max(&self) -> bool {
        matches!(self, PlanKey::Text(s) if s == "\u{ffff}")
    }
}

/// A query plan, from `getQueryPlan`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The index chosen.
    pub index: Vec<String>,
    /// Lower bound per index position.
    #[serde(rename = "startKeys")]
    pub start_keys: Vec<PlanKey>,
    /// Upper bound per index position.
    #[serde(rename = "endKeys")]
    pub end_keys: Vec<PlanKey>,
    /// Whether the lower bound is inclusive.
    #[serde(rename = "inclusiveStart")]
    pub inclusive_start: bool,
    /// Whether the upper bound is inclusive.
    #[serde(rename = "inclusiveEnd")]
    pub inclusive_end: bool,
    /// Whether the index order satisfies the requested sort. False whenever the
    /// query contains a `desc`, because most storages have no descending index.
    #[serde(rename = "sortSatisfiedByIndex")]
    pub sort_satisfied_by_index: bool,
    /// Whether the index alone decides the selector, so no matcher is needed.
    #[serde(rename = "selectorSatisfiedByIndex")]
    pub selector_satisfied_by_index: bool,
}

/// Sort-comparator behaviour for one sort specification.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ordering {
    /// Which sort was applied.
    pub why: String,
    /// The normalised sort, with the primary key appended when it was absent.
    #[serde(rename = "normalizedSort")]
    pub normalized_sort: Vec<SortPart>,
    /// Document ids in the order the comparator produced.
    #[serde(rename = "sortedIds")]
    pub sorted_ids: Vec<String>,
}

/// Selector-matcher behaviour for one selector.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    /// Which selector was applied.
    pub why: String,
    /// The selector after normalisation.
    #[serde(rename = "normalizedSelector")]
    pub normalized_selector: serde_json::Value,
    /// Ids of the documents that matched, in corpus order.
    #[serde(rename = "matchingIds")]
    pub matching_ids: Vec<String>,
}
