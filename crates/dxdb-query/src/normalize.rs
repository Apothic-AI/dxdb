//! Query normalisation: `normalizeMangoQuery` (`src/rx-query-helper.ts:34`).
//!
//! Normalisation exists so two queries that mean the same thing are the same
//! *value*, because the query cache keys on `JSON.stringify` of the normalized
//! form (`src/rx-query.ts:454`). Three things happen, in this order:
//!
//! 1. `skip` defaults to `0`.
//! 2. Selector shorthand (`{foo: 'bar'}`) becomes explicit `{$eq: 'bar'}`, deeply
//!    through `$and`/`$or`/`$nor`, the `$not` object form, and `$elemMatch`.
//! 3. A primary-key sort part is appended if absent, and if no sort was given at
//!    all, one is derived from the index, the selector's logical operators, or the
//!    schema's first index — in that order of preference.
//!
//! # Why the derived sort matters
//!
//! Storages return rows in index order, and RxDB's event-reduce and change-event
//! comparison both assume a deterministic total order. Appending the primary key is
//! what makes the order total: without it two documents with equal indexed fields
//! would arrive in insertion order on one run and storage order on the next.
//!
//! # Two details that are easy to lose
//!
//! - When an `index` is given, the primary key is appended *to the index* as well
//!   as to the sort. A user-declared index that omits the primary key would
//!   otherwise break the total-order property that `fillWithDefaultSettings`
//!   guarantees for schema indexes.
//! - `skipSort` short-circuits to a primary-key sort. It exists for `count()`
//!   queries, which need no order; reproducing it keeps a count query's prepared
//!   form identical to upstream's, and therefore its plan identical too.

use serde_json::{json, Map, Value};

use crate::error::{QueryError, QueryErrorCode};
use crate::plan::LOGICAL_OPERATORS;

/// `'asc'` or `'desc'` (`src/types/rx-query.d.ts:85`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortDirection {
    /// `asc`
    Asc,
    /// `desc`
    Desc,
}

impl SortDirection {
    /// Parses a direction. Upstream's type is a union, so anything else is a
    /// caller error rather than a value.
    pub fn parse(text: &str) -> Result<Self, QueryError> {
        match text {
            "asc" => Ok(Self::Asc),
            "desc" => Ok(Self::Desc),
            other => Err(QueryError::new(
                QueryErrorCode::InvalidOperatorPayload,
                format!("sort direction must be 'asc' or 'desc', got {other:?}"),
            )),
        }
    }

    /// The direction as upstream spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }
}

/// One `{field: direction}` pair of a sort.
///
/// Upstream models a sort part as a single-key object, which allows a malformed
/// multi-key part. A pair is the only shape Mango defines, and
/// [`SortPart::from_json`] rejects anything else rather than silently using the
/// first key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortPart {
    /// The field path.
    pub field: String,
    /// The direction.
    pub direction: SortDirection,
}

impl SortPart {
    /// Builds a part.
    pub fn new(field: impl Into<String>, direction: SortDirection) -> Self {
        Self {
            field: field.into(),
            direction,
        }
    }

    /// Reads one sort part from JSON.
    pub fn from_json(value: &Value) -> Result<Self, QueryError> {
        let Some(object) = value.as_object() else {
            return Err(QueryError::new(
                QueryErrorCode::InvalidOperatorPayload,
                format!("sort part must be an object, got {value}"),
            ));
        };
        if object.len() != 1 {
            return Err(QueryError::new(
                QueryErrorCode::InvalidOperatorPayload,
                format!("sort part must have exactly one key, got {value}"),
            ));
        }
        let (field, direction) = object.iter().next().expect("exactly one key");
        let Some(direction) = direction.as_str() else {
            return Err(QueryError::new(
                QueryErrorCode::InvalidOperatorPayload,
                format!("sort direction must be a string, got {direction}"),
            ));
        };
        Ok(Self::new(field, SortDirection::parse(direction)?))
    }

    /// Renders the part the way upstream does, as a single-key object.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert(
            self.field.clone(),
            Value::String(self.direction.as_str().to_string()),
        );
        Value::Object(map)
    }
}

/// A user-supplied Mango query (`MangoQuery`, `src/types/rx-query.d.ts:110`).
///
/// Every field is optional because Mango treats them that way: `{}` is a valid
/// query matching every non-deleted document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MangoQuery {
    /// The selector. Absent means "match everything".
    pub selector: Option<Value>,
    /// A user-declared index, as one field or a list of fields.
    pub index: Option<Vec<String>>,
    /// The requested sort.
    pub sort: Option<Vec<SortPart>>,
    /// Documents to skip.
    pub skip: Option<u64>,
    /// Documents to return.
    pub limit: Option<u64>,
}

impl MangoQuery {
    /// Reads a query from JSON, the shape a JS caller sends.
    pub fn from_json(value: &Value) -> Result<Self, QueryError> {
        let Some(object) = value.as_object() else {
            return Err(QueryError::new(
                QueryErrorCode::InvalidOperatorPayload,
                format!("query must be an object, got {value}"),
            ));
        };
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "selector" | "index" | "sort" | "skip" | "limit"
            ) {
                return Err(QueryError::new(
                    QueryErrorCode::InvalidOperatorPayload,
                    format!("unknown query property {key:?}"),
                ));
            }
        }
        let selector = match object.get("selector") {
            None => None,
            Some(Value::Object(_)) => object.get("selector").cloned(),
            Some(other) => {
                return Err(QueryError::new(
                    QueryErrorCode::InvalidOperatorPayload,
                    format!("selector must be an object, got {other}"),
                ))
            }
        };
        let index = match object.get("index") {
            None => None,
            Some(Value::String(single)) => Some(vec![single.clone()]),
            Some(Value::Array(items)) => {
                let mut fields = Vec::with_capacity(items.len());
                for item in items {
                    let Some(field) = item.as_str() else {
                        return Err(QueryError::new(
                            QueryErrorCode::InvalidOperatorPayload,
                            "index entries must be strings",
                        ));
                    };
                    fields.push(field.to_string());
                }
                Some(fields)
            }
            Some(other) => {
                return Err(QueryError::new(
                    QueryErrorCode::InvalidOperatorPayload,
                    format!("index must be a string or array of strings, got {other}"),
                ))
            }
        };
        let sort = match object.get("sort") {
            None => None,
            Some(Value::Array(items)) => {
                let mut parts = Vec::with_capacity(items.len());
                for item in items {
                    parts.push(SortPart::from_json(item)?);
                }
                Some(parts)
            }
            Some(other) => {
                return Err(QueryError::new(
                    QueryErrorCode::InvalidOperatorPayload,
                    format!("sort must be an array, got {other}"),
                ))
            }
        };
        let numeric =
            |key: &str| -> Result<Option<u64>, QueryError> {
                match object.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::Number(n)) => Ok(Some(n.as_u64().unwrap_or_else(|| {
                        n.as_f64().map(|f| f.max(0.0) as u64).unwrap_or_default()
                    }))),
                    Some(other) => Err(QueryError::new(
                        QueryErrorCode::InvalidOperatorPayload,
                        format!("{key} must be a number, got {other}"),
                    )),
                }
            };
        Ok(Self {
            selector,
            index,
            sort,
            skip: numeric("skip")?,
            limit: numeric("limit")?,
        })
    }
}

/// A normalized query (`FilledMangoQuery`, `src/types/rx-query.d.ts`).
///
/// Produced only by [`normalize_mango_query`], which is why every field is
/// present and the sort is non-empty.
#[derive(Debug, Clone, PartialEq)]
pub struct FilledMangoQuery {
    /// The selector, with shorthands expanded. Never absent: an empty object
    /// matches everything.
    pub selector: Value,
    /// The sort, always ending in the primary key.
    pub sort: Vec<SortPart>,
    /// Documents skipped. Always present.
    pub skip: u64,
    /// Documents to return, when the caller asked for a limit.
    ///
    /// Present only if it was in the input: upstream's `normalizeMangoQuery` clones
    /// the query and fills in `skip`, a `selector` and a `sort`, and leaves every
    /// other key as it found it. So `limit: 0` stays `limit: 0` (it is not falsy-
    /// dropped), and an absent limit stays absent rather than becoming `Infinity`,
    /// which would not survive JSON.
    pub limit: Option<u64>,
    /// A user-declared index, if any, with the primary key appended.
    pub index: Option<Vec<String>>,
}

impl FilledMangoQuery {
    /// The selector as a JSON object, which is what a caller stores and compares.
    pub fn selector_json(&self) -> Value {
        self.selector.clone()
    }

    /// Renders the whole query as the JSON upstream keeps in `mangoQuery`.
    ///
    /// Key order is fixed (`selector`, `index` when set, `sort`, `skip`, `limit`).
    /// Upstream's order comes from the caller's input object, because it clones the
    /// query; the value is compared as a JSON object, so the difference is not
    /// observable. `toString()` sorts keys before hashing, so query identity is
    /// unaffected either way.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("selector".to_string(), self.selector.clone());
        if let Some(index) = &self.index {
            map.insert(
                "index".to_string(),
                Value::Array(index.iter().cloned().map(Value::String).collect()),
            );
        }
        map.insert(
            "sort".to_string(),
            Value::Array(self.sort.iter().map(SortPart::to_json).collect()),
        );
        map.insert("skip".to_string(), json!(self.skip));
        if let Some(limit) = self.limit {
            map.insert("limit".to_string(), json!(limit));
        }
        Value::Object(map)
    }

    /// Same value, with the `_deleted: {$eq: false}` filter and index prefix that
    /// `getPreparedQuery` adds (`src/rx-query.ts:481`).
    ///
    /// Kept separate from [`FilledMangoQuery::to_json`] because it is a *second*
    /// transformation with a different purpose: `to_json` is the query cache key,
    /// this is what a storage sees. Reproducing both, and keeping them distinct, is
    /// what makes a prepared query comparable against upstream.
    pub fn as_prepared(&self) -> Value {
        let mut map = match self.to_json() {
            Value::Object(map) => map,
            other => unreachable!("to_json always builds an object: {other}"),
        };
        let mut selector = match self.selector.clone() {
            Value::Object(map) => map,
            other => {
                let mut map = Map::new();
                map.insert("$eq".to_string(), other);
                map
            }
        };
        selector.insert("_deleted".to_string(), json!({"$eq": false}));
        map.insert("selector".to_string(), Value::Object(selector));
        if let Some(index) = &self.index {
            let mut fields = vec!["_deleted".to_string()];
            fields.extend(index.iter().cloned());
            map.insert(
                "index".to_string(),
                Value::Array(fields.into_iter().map(Value::String).collect()),
            );
        }
        Value::Object(map)
    }
}

/// `normalizeMangoQuery` (`src/rx-query-helper.ts:34`).
///
/// `primary_key` is the *field* of the primary key
/// (`getPrimaryFieldOfPrimaryKey`), and `schema_indexes` is the schema's
/// `indexes` after `fillWithDefaultSettings` — both resolved once by the caller
/// rather than walked per query.
pub fn normalize_mango_query(
    query: &MangoQuery,
    primary_key: &str,
    schema_indexes: &[Vec<String>],
    skip_sort: bool,
) -> Result<FilledMangoQuery, QueryError> {
    let mut selector = match &query.selector {
        Some(selector) => selector.clone(),
        None => Value::Object(Map::new()),
    };
    normalize_query_selector_shorthands(&mut selector)?;

    let mut index = query.index.clone();
    if let Some(index) = &mut index {
        if !index.iter().any(|field| field == primary_key) {
            index.push(primary_key.to_string());
        }
    }

    let sort = match (&query.sort, skip_sort) {
        // `skipSort` and no sort: a plain primary-key sort. Upstream does this for
        // `count()` queries, where order is irrelevant and the index-matching
        // computation below would be wasted work.
        (None, true) => vec![SortPart::new(primary_key, SortDirection::Asc)],
        (None, false) => {
            sort_from_index_or_selector(&selector, index.as_deref(), primary_key, schema_indexes)
        }
        (Some(parts), _) => {
            let mut parts = parts.clone();
            if !parts.iter().any(|part| part.field == primary_key) {
                parts.push(SortPart::new(primary_key, SortDirection::Asc));
            }
            parts
        }
    };

    Ok(FilledMangoQuery {
        selector,
        sort,
        skip: query.skip.unwrap_or(0),
        limit: query.limit,
        index,
    })
}

/// The sort derivation from `src/rx-query-helper.ts:105`, in upstream's order of
/// preference.
fn sort_from_index_or_selector(
    selector: &Value,
    index: Option<&[String]>,
    primary_key: &str,
    schema_indexes: &[Vec<String>],
) -> Vec<SortPart> {
    if let Some(index) = index {
        return index
            .iter()
            .map(|field| SortPart::new(field.clone(), SortDirection::Asc))
            .collect();
    }
    // The index whose leading fields all appear in the selector with a logical
    // operator, preferring the longest such run.
    let mut fields_with_logical_operator: Vec<&String> = Vec::new();
    if let Some(object) = selector.as_object() {
        for (field, matcher) in object {
            let has_logical = match matcher {
                Value::Object(map) => map
                    .keys()
                    .any(|op| LOGICAL_OPERATORS.contains(&op.as_str())),
                _ => true,
            };
            if has_logical {
                fields_with_logical_operator.push(field);
            }
        }
    }
    let mut best: Option<&Vec<String>> = None;
    let mut best_count: isize = -1;
    for candidate in schema_indexes {
        let first_wrong = candidate
            .iter()
            .position(|field| !fields_with_logical_operator.contains(&field));
        let matching = match first_wrong {
            Some(position) => position,
            None => candidate.len(),
        };
        if matching > 0 && (matching as isize) > best_count {
            best_count = matching as isize;
            best = Some(candidate);
        }
    }
    if let Some(best) = best {
        return best
            .iter()
            .map(|field| SortPart::new(field.clone(), SortDirection::Asc))
            .collect();
    }
    // Fall back to the schema's first index, and to the primary key if there is
    // none. `fillWithDefaultSettings` guarantees at least the `_meta.lwt` index
    // for a real schema, so the primary-key branch is the "no schema" case.
    match schema_indexes.first() {
        Some(first) => first
            .iter()
            .map(|field| SortPart::new(field.clone(), SortDirection::Asc))
            .collect(),
        None => vec![SortPart::new(primary_key, SortDirection::Asc)],
    }
}

/// `normalizeQuerySelectorShorthands` (`src/rx-query-helper.ts:314`).
///
/// Recursion rules, each of which is an upstream test in
/// `query-planner.test.ts:128`:
///
/// - `$and`/`$or`/`$nor`: recurse into each arm.
/// - `$not`: recurse into the object.
/// - any other `$`-key: leave alone. This is what keeps `{$regex: 'x', $options:
///   'i'}` and `{$eq: 'foo'}` intact.
/// - a field name whose value is an object holding `$elemMatch`: recurse into the
///   `$elemMatch` payload, but only when it is a nested selector rather than a
///   payload (see [`should_normalize_elem_match`]).
pub fn normalize_query_selector_shorthands(selector: &mut Value) -> Result<(), QueryError> {
    let Some(object) = selector.as_object_mut() else {
        // A non-object selector is not something a caller can build from JSON
        // through the public API; the check exists so the error is explicit.
        return Err(QueryError::new(
            QueryErrorCode::InvalidOperatorPayload,
            format!("selector must be an object, got {selector}"),
        ));
    };

    for (field, matcher) in object.iter_mut() {
        match field.as_str() {
            "$and" | "$or" | "$nor" => {
                let Some(arms) = matcher.as_array_mut() else {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidOperatorPayload,
                        format!("{field} expects an array"),
                    ));
                };
                for arm in arms.iter_mut() {
                    normalize_query_selector_shorthands(arm)?;
                }
            }
            "$not" => {
                if matcher.is_object() {
                    normalize_query_selector_shorthands(matcher)?;
                }
            }
            other if other.starts_with('$') => {
                // An operator payload is opaque here: `$regex`/`$options`/`$eq`
                // values are not selectors, and recursing into them would rewrite
                // the payload into `{$eq: ...}`.
            }
            _ => {
                if matcher.is_object() {
                    let should_recurse = matcher
                        .get("$elemMatch")
                        .map(should_normalize_elem_match)
                        .unwrap_or(false);
                    if should_recurse {
                        if let Some(elem_match) = matcher.get_mut("$elemMatch") {
                            normalize_query_selector_shorthands(elem_match)?;
                        }
                    }
                } else {
                    // The shorthand itself: `{field: value}` -> `{field: {$eq: value}}`.
                    let value = matcher.clone();
                    let mut wrapper = Map::new();
                    wrapper.insert("$eq".to_string(), value);
                    *matcher = Value::Object(wrapper);
                }
            }
        }
    }
    Ok(())
}

/// `shouldNormalizeElemMatchSelector` (`src/rx-query-helper.ts:294`).
///
/// An `$elemMatch` payload is a nested selector when at least one key is not an
/// operator. When every key is an operator it is an operator payload — unless one
/// of them is a logical operator, in which case the payload holds selectors again.
fn should_normalize_elem_match(elem_match: &Value) -> bool {
    let Some(object) = elem_match.as_object() else {
        return false;
    };
    if object.is_empty() {
        return false;
    }
    if object.keys().any(|key| !key.starts_with('$')) {
        return true;
    }
    object
        .keys()
        .any(|key| matches!(key.as_str(), "$and" | "$or" | "$nor" | "$not"))
}

/// A query and the plan for it, prepared together.
///
/// Mirrors upstream's `PreparedQuery` (`src/rx-query-helper.ts:364`): the object a
/// storage receives, so it does not have to normalise or plan anything itself. Upstream
/// stores both on one object because a storage always needs both and the plan depends on
/// the normalised form.
///
/// The types here are `dxdb-query`'s. `dxdb-storage` has its own `MangoQuery`/`QueryPlan`
/// for the trait boundary, and a storage adapter maps one to the other at that boundary —
/// deliberately, because the alternative is either this crate depending on
/// `dxdb-storage` (pulling the async trait surface, `futures` and `dynosaur` into a
/// synchronous planner) or `dxdb-storage` depending on this one (decision D8 rejected
/// that: the storage layer must stay at the bottom of the graph). The mapping is
/// mechanical and belongs on the side that owns the trait.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedQuery {
    /// The normalised query.
    pub query: FilledMangoQuery,
    /// The plan for it.
    pub query_plan: crate::plan::QueryPlan,
}

impl PreparedQuery {
    /// Pairs a query with its plan.
    pub fn new(query: FilledMangoQuery, query_plan: crate::plan::QueryPlan) -> Self {
        Self { query, query_plan }
    }
}

/// Normalises a query and plans it, in one step.
///
/// This is the entry point a storage adapter should call: `schema` is the filled schema
/// (`dxdb_schema::create_rx_schema(..).json_schema()`), `query` is the caller's query, and
/// the result carries both the normalised form and the plan. Every argument is derived
/// from the schema exactly as upstream derives it, so a caller cannot forget one:
///
/// ```ignore
/// let prepared = prepare_query(&schema, &MangoQuery::from_json(&query_json)?, false)?;
/// let plan = prepared.query_plan;
/// ```
///
/// `skip_sort` is upstream's `skipSort` (`src/rx-query-helper.ts:41`): `true` for a
/// `count()` query, where order is irrelevant and a plain primary-key sort is enough.
///
/// Doing this in one function is not merely convenience. Three separate call sites in
/// three lanes each performing "resolve the primary path, resolve the indexes, normalise,
/// plan" is three places to get the primary-key extraction wrong, and the symptom would be
/// a plan built against a different index than the one the query was normalised for.
pub fn prepare_query(
    schema: &serde_json::Value,
    query: &MangoQuery,
    skip_sort: bool,
) -> Result<PreparedQuery, QueryError> {
    let primary_key = dxdb_schema::primary_field_of_primary_key(schema).map_err(|error| {
        QueryError::new(
            QueryErrorCode::Qu13,
            format!("schema has no usable primaryKey: {error}"),
        )
    })?;
    let indexes = dxdb_schema::get_string_indexes(schema)?;
    let normalized = normalize_mango_query(query, &primary_key, &indexes, skip_sort)?;
    let plan = crate::plan::get_query_plan(schema, &normalized)?;
    Ok(PreparedQuery {
        query: normalized,
        query_plan: plan,
    })
}
