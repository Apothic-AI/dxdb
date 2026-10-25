//! The storage query ABI.
//!
//! This module is *only* the shape of what crosses the trait boundary:
//!
//! ```text
//! dxdb-query (lane B)                    dxdb-storage (this crate)
//!   normalize_mango_query  ─────────────▶ MangoQuery { selector, sort, skip, limit, index }
//!   get_query_plan         ─────────────▶ QueryPlan { index, start_keys, end_keys, inclusive*, *SatisfiedByIndex }
//!   prepare_query          ─────────────▶ PreparedQuery { query, query_plan }
//!                                                     │
//!                                        storage decides whether to trust the plan
//! ```
//!
//! Nothing here computes an index string, matches a selector, or plans a query.
//! But a storage *must* be able to consume a plan without depending on
//! `dxdb-query`, or the wrapper stack (which is generic over storage, not over
//! query) could not exist.
//!
//! Two upstream details are encoded in the types rather than left to convention:
//!
//! * [`PlanKey`] is a *closed* enum of the three things the planner can emit, not
//!   arbitrary JSON. Upstream `RxQueryPlanKey = string | number | undefined`, and
//!   the reason `undefined` exists is documented at `query-planner.ts:20`:
//!   `INDEX_MIN` is `Number.MIN_SAFE_INTEGER` rather than `-Infinity` so a plan
//!   survives `JSON.stringify`. A `f64` key that could be infinite would let that
//!   bug back in, so a numeric key must be finite here too.
//! * A sort part keeps insertion order and allows exactly one field per part,
//!   like upstream's `{ [field]: 'asc' }` object literal.

use serde_json::Value;

/// `INDEX_MIN` from `query-planner.ts:25`: `Number.MIN_SAFE_INTEGER`, not
/// `-Infinity`, so a query plan survives `JSON.stringify` (`:20`).
pub const INDEX_MIN: f64 = -9_007_199_254_740_991.0;

/// `INDEX_MAX` from `query-planner.ts:18`: `String.fromCharCode(65535)`.
pub const INDEX_MAX: char = '\u{FFFF}';

/// A bound key in a query plan.
///
/// Mirrors `RxQueryPlanKey = string | number | undefined` (`query-planner.d.ts`).
#[derive(Clone, Debug, PartialEq)]
pub enum PlanKey {
    /// A string bound; `'\u{FFFF}'` stands for `INDEX_MAX`.
    String(String),
    /// A finite numeric bound. Non-finite is unrepresentable on purpose.
    Number(f64),
    /// A boolean bound. Booleans appear as plan keys because `_deleted` is the first
    /// field of every index, so an equality on it becomes a `startKeys`/`endKeys`
    /// pair of booleans.
    Boolean(bool),
    /// The "no bound" case that upstream represents as `undefined`, which is how
    /// a plan survives a JSON round-trip.
    Unbounded,
}

impl PlanKey {
    /// `INDEX_MIN` as a key.
    pub fn index_min() -> Self {
        PlanKey::Number(INDEX_MIN)
    }

    /// `INDEX_MAX` as a key.
    pub fn index_max() -> Self {
        PlanKey::String(INDEX_MAX.to_string())
    }

    /// The key as JSON, with `Unbounded` becoming `null` the way a serialised
    /// `undefined` member would not appear at all. `null` is used rather than
    /// omission so array indexes stay aligned.
    pub fn to_json(&self) -> Value {
        match self {
            PlanKey::String(s) => Value::String(s.clone()),
            PlanKey::Number(n) => serde_json::Number::from_f64(*n)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            PlanKey::Boolean(flag) => Value::Bool(*flag),
            PlanKey::Unbounded => Value::Null,
        }
    }

    /// Builds a key from a JSON bound. Returns `None` for a non-finite number,
    /// which a plan must never contain.
    pub fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::String(s) => Some(PlanKey::String(s.clone())),
            Value::Number(n) => n.as_f64().filter(|n| n.is_finite()).map(PlanKey::Number),
            Value::Bool(flag) => Some(PlanKey::Boolean(*flag)),
            Value::Null => Some(PlanKey::Unbounded),
            _ => None,
        }
    }

    /// True when this key is `INDEX_MIN` or `INDEX_MAX`, i.e. carries no
    /// information about the selector. The planner's quality rating counts
    /// exactly these (`rateQueryPlan` in `query-planner.ts`).
    pub fn is_min_or_max(&self) -> bool {
        match self {
            PlanKey::Number(n) => *n == INDEX_MIN,
            PlanKey::String(s) => s.as_str() == INDEX_MAX.to_string().as_str(),
            PlanKey::Boolean(_) | PlanKey::Unbounded => false,
        }
    }
}

/// A sort direction. Descending sorts always force a manual re-sort because no
/// storage supports descending indexes (`query-planner.ts:48`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDirection {
    /// Ascending.
    Asc,
    /// Descending.
    Desc,
}

impl SortDirection {
    /// Parses `"asc"`/`"desc"`. Anything else is `None`; upstream would treat a
    /// typo as a JS truthy value and sort the wrong way.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "asc" => Some(SortDirection::Asc),
            "desc" => Some(SortDirection::Desc),
            _ => None,
        }
    }

    /// The upstream spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SortDirection::Asc => "asc",
            SortDirection::Desc => "desc",
        }
    }

    /// `-1.0` for ascending, `1.0` for descending: the multiplier a comparator
    /// applies.
    pub fn multiplier(self) -> f64 {
        match self {
            SortDirection::Asc => 1.0,
            SortDirection::Desc => -1.0,
        }
    }
}

/// One `{ field: direction }` sort entry.
///
/// Upstream type is `{ [k in StringKeys<RxDocType> | string]: MangoQuerySortDirection }`,
/// i.e. an object with exactly one key. Modelled as a pair so the
/// one-key-per-part rule cannot be violated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortPart {
    /// The dot path to sort on.
    pub field: String,
    /// Direction.
    pub direction: SortDirection,
}

impl SortPart {
    /// `{ field: 'asc' }`.
    pub fn asc(field: impl Into<String>) -> Self {
        SortPart {
            field: field.into(),
            direction: SortDirection::Asc,
        }
    }

    /// `{ field: 'desc' }`.
    pub fn desc(field: impl Into<String>) -> Self {
        SortPart {
            field: field.into(),
            direction: SortDirection::Desc,
        }
    }
}

/// A **normalised** Mango query: what a storage receives, never what a user wrote.
///
/// Guarantees this type carries (upstream `FilledMangoQuery`):
///
/// * `skip` is always a number (`0` when absent) — so it is not optional here;
/// * `sort` always contains the primary key, appended last if the user did not
///   mention it;
/// * `index`, when present, is a `Vec`, never a bare string.
///
/// Matching upstream's shape exactly matters because the planner's output and a
/// storage's index choice are both part of the wire behaviour.
#[derive(Clone, Debug, PartialEq)]
pub struct MangoQuery {
    /// `selector` as ordered JSON. Normalisation has already rewritten bare
    /// equality (`{a: 1}`) into `{a: {$eq: 1}}` and recursed into `$and`/`$or`/
    /// `$nor`/`$not`, so a storage may rely on operator form.
    pub selector: Value,
    /// Never empty: the primary key is appended if the caller omitted it.
    pub sort: Vec<SortPart>,
    /// `0` when the caller did not ask for a skip.
    pub skip: usize,
    /// `None` means "no limit". An explicit `0` is a real zero.
    pub limit: Option<usize>,
    /// A forced index. When set, the planner returns a plan for this index only.
    pub index: Option<Vec<String>>,
}

impl MangoQuery {
    /// The fields this query sorts on, in order. Used by the planner's
    /// `sortSatisfiedByIndex` check and by a storage that has to re-sort.
    pub fn sort_fields(&self) -> impl Iterator<Item = &str> {
        self.sort.iter().map(|part| part.field.as_str())
    }

    /// True when any sort part is descending, which forces a manual re-sort.
    pub fn has_desc_sort(&self) -> bool {
        self.sort
            .iter()
            .any(|part| part.direction == SortDirection::Desc)
    }

    /// `skip + limit`, or `usize::MAX` when unlimited.
    ///
    /// Storages use this to bound how many matching documents they must collect:
    /// with a limit, only the first `skip + limit` documents in sort order can
    /// matter.
    pub fn skip_plus_limit(&self) -> usize {
        match self.limit {
            // Saturating, not wrapping: `usize::MAX` already means "unlimited"
            // for every caller, and wrapping would bound the result at a small
            // number.
            Some(limit) => self.skip.saturating_add(limit),
            None => usize::MAX,
        }
    }

    /// Reads a field out of a document the way the selector would.
    pub fn document_field<'a>(
        &self,
        document: &'a crate::document::DocumentData,
        field: &str,
    ) -> Option<&'a Value> {
        document.get_object_path(field)
    }

    /// Sanity check used by testkit and by storage implementations in dev mode:
    /// a query with no sort would make result order non-deterministic, and every
    /// layer above assumes determinism (event-reduce depends on it).
    pub fn is_deterministic(&self) -> bool {
        !self.sort.is_empty()
    }
}

/// The planner's advice to a storage.
///
/// A storage may ignore any field of it — upstream's contract says the plan is a
/// hint (`rx-storage.interface.d.ts` on `PreparedQuery`) — but a storage that
/// respects `selectorSatisfiedByIndex` and `sortSatisfiedByIndex` gets the
/// performance the planner was tuned for.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryPlan {
    /// The index the plan chose, as field paths with `_deleted` first and the
    /// primary key last.
    pub index: Vec<String>,
    /// True when the index order already is the sort order, so the storage need
    /// not re-sort. Never true with a descending sort part.
    pub sort_satisfied_by_index: bool,
    /// True when the selector needs no per-document check, because the index
    /// range is exactly the result set.
    pub selector_satisfied_by_index: bool,
    /// Lower bounds, one per index field.
    pub start_keys: Vec<PlanKey>,
    /// Upper bounds, one per index field.
    pub end_keys: Vec<PlanKey>,
    /// Whether the document at `start_keys` is included.
    pub inclusive_start: bool,
    /// Whether the document at `end_keys` is included.
    pub inclusive_end: bool,
}

impl QueryPlan {
    /// The index fields joined the way a storage keys its index maps
    /// (`getMemoryIndexName`: `index.join(',')`).
    pub fn index_name(&self) -> String {
        self.index.join(",")
    }

    /// True when the plan has no bounds on the first index field, i.e. it scans
    /// the whole index. Useful for a storage's dev-mode warnings.
    pub fn is_full_scan(&self) -> bool {
        match (self.start_keys.first(), self.end_keys.first()) {
            (Some(start), Some(end)) => start.is_min_or_max() && end.is_min_or_max(),
            _ => true,
        }
    }
}

/// What a storage is handed to run a query.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedQuery {
    /// The normalised query.
    pub query: MangoQuery,
    /// The planner's hint.
    pub query_plan: QueryPlan,
}

impl PreparedQuery {
    /// Pairs a query with its plan.
    pub fn new(query: MangoQuery, query_plan: QueryPlan) -> Self {
        PreparedQuery { query, query_plan }
    }
}

/// Whether a count was answered from an index or by scanning.
///
/// Upstream returns `'fast' | 'slow'` and core throws on `'slow'` unless the
/// database was created with `allowSlowCount: true`. Keeping the distinction is
/// required: it is how a user finds out their count query has no index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountMode {
    /// Answered without materialising documents.
    Fast,
    /// Answered by scanning and filtering.
    Slow,
}

impl CountMode {
    /// The upstream spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CountMode::Fast => "fast",
            CountMode::Slow => "slow",
        }
    }
}
