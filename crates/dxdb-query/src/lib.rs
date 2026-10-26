//! dxdb query: the Mango surface RxDB actually supports, the index strings its
//! storages sort by, and the planner that turns a query into a scan range.
//!
//! Ported from upstream `src/custom-index.ts`, `src/query-planner.ts`,
//! `src/rx-query-helper.ts` and the mingo configuration in
//! `src/rx-query-mingo.ts`. `src/rx-query.ts` is the collection-level wrapper —
//! query caching, subscriptions, update/remove — and belongs to M2; what this
//! crate takes from it is behaviour, not structure: see [`normalize`],
//! [`plan`], [`sort`] and [`matcher`].
//!
//! # The operator set is deliberately small
//!
//! `src/types/rx-query.d.ts:43` lists exactly what RxDB supports:
//!
//! ```text
//! $eq $gt $gte $lt $lte $ne $in $nin $regex $options $exists $type
//! $mod $not $size $elemMatch
//! ```
//!
//! plus the top-level `$and $or $nor` and the object form of `$not`. That is not
//! mingo's full surface — mingo also has `$all`, `$bitsAllSet`, `$expr`,
//! `$jsonSchema`, `$where`, `$text`, `$geoWithin` and more — and the limitation is
//! part of the contract, not an oversight to improve on:
//!
//! - An unknown operator **throws**, as it does upstream (`mingo/query.js`
//!   `processOperator`: "unknown query operator"). A document that would have
//!   matched is not silently returned, and a typo in a selector is a crash rather
//!   than a full scan.
//! - Selector pushdown is **not** improved beyond `$eq $gt $gte $lt $lte` plus the
//!   homogeneous-`$in` range optimisation, because a better plan than upstream's
//!   is a different plan, and query plans are wire behaviour (`docs/PLAN.md` 4.3).
//!
//! # Where the semantics live
//!
//! | module | upstream |
//! | --- | --- |
//! | [`custom_index`] | `src/custom-index.ts` — index strings and bounds |
//! | [`matcher`] | `src/rx-query-mingo.ts` + `mingo/operators/_predicates.js` |
//! | [`normalize`] | `src/rx-query-helper.ts:34` `normalizeMangoQuery` |
//! | [`plan`] | `src/query-planner.ts` |
//! | [`sort`] | `src/rx-query-helper.ts:186` `getSortComparator` |
//! | [`top_k`] | `src/rx-query-helper.ts:387` top-k selection |
//!
//! # Numbers and strings
//!
//! Two rules hold everywhere in this crate and are worth stating once:
//!
//! - A number that becomes text goes through [`dxdb_jsnum::to_js_string`], never
//!   `f64::to_string` (PLAN.md 3.1).
//! - Two strings are ordered by [`dxdb_jsstr::compare_js`], never by Rust's
//!   `str` ordering (PLAN.md 3.2).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod custom_index;
pub mod error;
pub mod matcher;
pub mod normalize;
pub mod plan;
pub mod sort;
pub mod top_k;

pub use custom_index::{
    change_indexable_string_by_one_quantum, get_index_string_length, get_indexable_string,
    get_number_index_string, get_primary_key_from_indexable_string,
    get_primary_key_from_indexable_string_with_length, get_start_index_string_from_lower_bound,
    get_start_index_string_from_upper_bound, get_string_length_of_index_number, index_meta,
    BoundValue, IndexMetaField, ParsedLengths, INDEX_MAX, INDEX_MIN,
};
pub use error::{QueryError, QueryErrorCode};
pub use matcher::{
    is_equal, mingo_compare, resolve, Matcher, RegexFlags, RegexMatcher, Resolved,
    MINGO_DEFAULT_DEPTH,
};
pub use normalize::{
    normalize_mango_query, normalize_query_selector_shorthands, prepare_query, FilledMangoQuery,
    MangoQuery, PreparedQuery, SortDirection, SortPart,
};
pub use plan::{
    get_in_query_range_opts, get_matcher_query_opts, get_query_plan,
    is_selector_satisfied_by_index, promote_out_of_range_bound, rate_query_plan, PlanKey,
    QueryPlan, INDEX_MAX_STR, LOGICAL_OPERATORS, LOWER_BOUND_LOGICAL_OPERATORS,
    UPPER_BOUND_LOGICAL_OPERATORS,
};
pub use sort::{get_sort_comparator, SortComparator};
pub use top_k::{
    create_top_k_documents_collector, get_skip_plus_limit, should_use_top_k_selection,
    sort_and_slice_documents, TopKDocumentsCollector, TOP_K_SELECTION_FACTOR,
};

#[cfg(test)]
mod tests;
