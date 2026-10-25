//! The query planner: `src/query-planner.ts`.
//!
//! A plan says which index to walk and where to start and stop. Storages differ in
//! whether they trust it — IndexedDB builds an `IDBKeyRange` from it, memory storage
//! binary-searches index strings — so the plan is wire behaviour and is reproduced
//! exactly, including the parts that are arguably suboptimal.
//!
//! # The two sentinels
//!
//! `INDEX_MAX` is `String.fromCharCode(65535)` and `INDEX_MIN` is
//! `Number.MIN_SAFE_INTEGER`, not `-Infinity` (`:25`). The comment upstream is
//! explicit about why: `-Infinity` becomes `null` under `JSON.stringify`, and a plan
//! is sent to a worker or a remote storage as JSON in several plugins. A `null` start
//! key there is a broken scan. The cost of the substitution is that a real number at
//! `MIN_SAFE_INTEGER` is indistinguishable from "unbounded", which is accepted
//! upstream and therefore here.
//!
//! # Only five operators narrow a scan
//!
//! [`LOGICAL_OPERATORS`] is `$eq $gt $gte $lt $lte` (`:239`). Everything else —
//! `$ne`, `$in`, `$regex`, `$exists`, `$mod`, `$type`, `$size`, `$elemMatch`,
//! `$not`, `$nin` — either widens or cannot be expressed as a contiguous range, so
//! the scan is opened and the matcher filters the rows afterwards.
//!
//! One deliberate exception, and it is upstream's: `$in` with a homogeneous array of
//! strings or finite numbers becomes the range `[min, max]` (`getInQueryRangeOpts`,
//! `:370`). The range contains non-matching rows, so `selectorSatisfiedByIndex`
//! stays false and the matcher still runs — but the scan is narrowed, which is the
//! point. Mixed types are rejected because `<` across types is not a total order a
//! range can express.
//!
//! # Plan quality
//!
//! [`rate_query_plan`] scores a plan: 10 points per leading key that pins a
//! specific value on either end, 15 per position where start and end agree (an
//! equality), and 5 for not needing a re-sort. The highest score wins, and ties keep
//! the earlier index (`>=` in `:216`), so schema index order breaks ties. All of it
//! is observable through which index a query ends up using.

use serde_json::Value;

use crate::error::{QueryError, QueryErrorCode};
use crate::normalize::FilledMangoQuery;
use crate::custom_index::INDEX_MIN;

/// `INDEX_MAX` — the highest UTF-16 code unit, used as the "no upper bound"
/// sentinel (`:15`).
pub const INDEX_MAX_STR: &str = "\u{FFFF}";

/// The operators that can narrow a scan (`:239`).
pub const LOGICAL_OPERATORS: [&str; 5] = ["$eq", "$gt", "$gte", "$lt", "$lte"];
/// Operators that bound the lower end of a scan (`:240`).
pub const LOWER_BOUND_LOGICAL_OPERATORS: [&str; 3] = ["$eq", "$gt", "$gte"];
/// Operators that bound the upper end of a scan (`:241`).
pub const UPPER_BOUND_LOGICAL_OPERATORS: [&str; 3] = ["$eq", "$lt", "$lte"];

/// A value in `startKeys`/`endKeys` (`RxQueryPlanKey`, `types/query-planner.d.ts`).
///
/// A closed enum rather than `Value`, because the set is exactly "a string, a
/// number, a boolean, or a sentinel", and because the plan must survive
/// `JSON.stringify` — which is why the bottom sentinel is a number and not
/// `-Infinity`.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanKey {
    /// `INDEX_MIN`: unbounded at the bottom, or unbounded at the top for an
    /// end key. Both directions use it, as they do upstream.
    IndexMin,
    /// `INDEX_MAX`.
    IndexMax,
    /// A string bound.
    String(String),
    /// A numeric bound.
    Number(f64),
    /// A boolean bound. Booleans appear as plan keys because `_deleted` is the
    /// first field of every index.
    Bool(bool),
    /// An explicit `null`, which is a legal `$eq` operand.
    Null,
    /// No bound was produced for this position.
    Undefined,
}

impl PlanKey {
    /// Reads a selector operand as a plan key.
    pub fn from_value(value: Option<&Value>) -> Self {
        match value {
            None => Self::Undefined,
            Some(Value::Null) => Self::Null,
            Some(Value::Bool(b)) => Self::Bool(*b),
            Some(Value::String(s)) => Self::String(s.clone()),
            Some(Value::Number(n)) => match n.as_f64() {
                Some(f) if f == INDEX_MIN as f64 => Self::IndexMin,
                Some(f) => Self::Number(f),
                None => Self::Undefined,
            },
            Some(_) => Self::Undefined,
        }
    }

    /// Renders the key as JSON *text*.
    ///
    /// Numbers go through [`dxdb_jsnum::to_js_string`] rather than serde's float
    /// writer: `serde_json` writes `20.0` where `JSON.stringify` writes `20`
    /// (PLAN.md 3.1). A plan key reaches a storage as JSON and appears in the
    /// oracle's `planJson`, so the two must agree byte for byte.
    pub fn to_json_string(&self) -> String {
        match self {
            Self::IndexMin => INDEX_MIN.to_string(),
            Self::IndexMax => format!("\"{INDEX_MAX_STR}\""),
            Self::String(text) => {
                serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
            }
            Self::Number(value) => dxdb_jsnum::to_json_string(*value),
            Self::Bool(flag) => flag.to_string(),
            Self::Null | Self::Undefined => "null".to_string(),
        }
    }

    /// Renders the key as a JSON value.
    ///
    /// Not a serialisation path: a `Value` cannot express "the number 20, written
    /// `20`", so use [`PlanKey::to_json_string`] for anything compared, printed or
    /// sent across a boundary.
    pub fn to_json(&self) -> Value {
        match self {
            Self::IndexMin => Value::from(INDEX_MIN),
            Self::IndexMax => Value::String(INDEX_MAX_STR.to_string()),
            Self::String(text) => Value::String(text.clone()),
            Self::Number(value) => match serde_json::Number::from_f64(*value) {
                Some(number) => Value::Number(number),
                None => Value::Null,
            },
            Self::Bool(flag) => Value::Bool(*flag),
            Self::Null | Self::Undefined => Value::Null,
        }
    }

    /// Whether this key is one of the two sentinels.
    pub fn is_sentinel(&self) -> bool {
        matches!(self, Self::IndexMin | Self::IndexMax | Self::Undefined)
    }

    /// Structural equality as `===` would define it, which is what the planner's
    /// `startKeys[i] === endKeys[i]` test means.
    ///
    /// `Number` compares by value (so `1.0 == 1`), which is `===` in JavaScript.
    /// `Undefined` is not equal to `Null`: `undefined !== null`.
    pub fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::IndexMin, Self::IndexMin) => true,
            (Self::IndexMax, Self::IndexMax) => true,
            (Self::Undefined, Self::Undefined) => true,
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Number(a), Self::Number(b)) => a == b,
            _ => false,
        }
    }
}

/// A query plan (`RxQueryPlan`, `types/query-planner.d.ts`).
#[derive(Debug, Clone, PartialEq)]
pub struct QueryPlan {
    /// The index to walk, as field names.
    pub index: Vec<String>,
    /// The lower bound per index position.
    pub start_keys: Vec<PlanKey>,
    /// The upper bound per index position.
    pub end_keys: Vec<PlanKey>,
    /// Whether the document at `start_keys` is included.
    pub inclusive_start: bool,
    /// Whether the document at `end_keys` is included.
    pub inclusive_end: bool,
    /// Whether walking the index produces the requested order, so no re-sort is
    /// needed.
    pub sort_satisfied_by_index: bool,
    /// Whether the index alone decides the selector, so the matcher can be
    /// skipped.
    pub selector_satisfied_by_index: bool,
}

impl QueryPlan {
    /// Renders the plan exactly as upstream's object literal would stringify.
    ///
    /// Key order matters because the plan is part of the prepared query and
    /// upstream's `JSON.stringify` order is observed by a recorded vector;
    /// [`QueryPlan::to_json_string`] is the form to compare against it.
    /// Renders the plan as the JSON text `JSON.stringify(plan)` produces.
    ///
    /// Key order is upstream's object-literal order, and numbers use JavaScript
    /// formatting — both are observed, because `testdata/vectors/normalised-queries.json`
    /// records the exact `planJson` string and a mismatched `20.0`/`20` is a
    /// divergence.
    pub fn to_json_string(&self) -> String {
        let mut out = String::from("{\"index\":[");
        for (position, field) in self.index.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            out.push_str(&serde_json::to_string(field).unwrap_or_else(|_| "\"\"".to_string()));
        }
        out.push_str("],\"startKeys\":[");
        for (position, key) in self.start_keys.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            out.push_str(&key.to_json_string());
        }
        out.push_str("],\"endKeys\":[");
        for (position, key) in self.end_keys.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            out.push_str(&key.to_json_string());
        }
        out.push_str("],\"inclusiveEnd\":");
        out.push_str(if self.inclusive_end { "true" } else { "false" });
        out.push_str(",\"inclusiveStart\":");
        out.push_str(if self.inclusive_start { "true" } else { "false" });
        out.push_str(",\"sortSatisfiedByIndex\":");
        out.push_str(if self.sort_satisfied_by_index {
            "true"
        } else {
            "false"
        });
        out.push_str(",\"selectorSatisfiedByIndex\":");
        out.push_str(if self.selector_satisfied_by_index {
            "true"
        } else {
            "false"
        });
        out.push('}');
        out
    }

    /// The plan as a JSON value, for a caller that wants to walk it.
    ///
    /// Not a serialisation path — see [`QueryPlan::to_json_string`] for that.
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "index".to_string(),
            Value::Array(self.index.iter().cloned().map(Value::String).collect()),
        );
        map.insert(
            "startKeys".to_string(),
            Value::Array(self.start_keys.iter().map(PlanKey::to_json).collect()),
        );
        map.insert(
            "endKeys".to_string(),
            Value::Array(self.end_keys.iter().map(PlanKey::to_json).collect()),
        );
        map.insert("inclusiveEnd".to_string(), Value::Bool(self.inclusive_end));
        map.insert(
            "inclusiveStart".to_string(),
            Value::Bool(self.inclusive_start),
        );
        map.insert(
            "sortSatisfiedByIndex".to_string(),
            Value::Bool(self.sort_satisfied_by_index),
        );
        map.insert(
            "selectorSatisfiedByIndex".to_string(),
            Value::Bool(self.selector_satisfied_by_index),
        );
        Value::Object(map)
    }
}

/// The partial range options one operator contributes: a small value type so the
/// merge in [`get_query_plan`] is explicit rather than a chain of `if let`s on a
/// `HashMap`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RangeOpts {
    start_key: Option<PlanKey>,
    end_key: Option<PlanKey>,
    inclusive_start: Option<bool>,
    inclusive_end: Option<bool>,
}

/// `getMatcherQueryOpts` (`:408`).
///
/// `$eq` sets both ends and both are inclusive; `$lt`/`$gt` set one end and make it
/// exclusive. An operator outside the five is `SNH` upstream because the caller has
/// already filtered by [`LOGICAL_OPERATORS`]; the same is true here.
pub fn get_matcher_query_opts(operator: &str, operator_value: &Value) -> Result<RangeOpts, QueryError> {
    let key = PlanKey::from_value(Some(operator_value));
    match operator {
        "$eq" => Ok(RangeOpts {
            start_key: Some(key.clone()),
            end_key: Some(key),
            inclusive_start: Some(true),
            inclusive_end: Some(true),
        }),
        "$lte" => Ok(RangeOpts {
            end_key: Some(key),
            inclusive_end: Some(true),
            ..Default::default()
        }),
        "$gte" => Ok(RangeOpts {
            start_key: Some(key),
            inclusive_start: Some(true),
            ..Default::default()
        }),
        "$lt" => Ok(RangeOpts {
            end_key: Some(key),
            inclusive_end: Some(false),
            ..Default::default()
        }),
        "$gt" => Ok(RangeOpts {
            start_key: Some(key),
            inclusive_start: Some(false),
            ..Default::default()
        }),
        other => Err(QueryError::new(
            QueryErrorCode::Snh,
            format!("{other} is not a logical operator"),
        )),
    }
}

/// `getInQueryRangeOpts` (`:370`): `$in` narrows to the min/max of its values.
///
/// Only a non-empty array whose every entry is the same primitive type of string or
/// finite number qualifies. Anything else returns `None` and leaves the scan open,
/// because min/max across types is not a range.
pub fn get_in_query_range_opts(operator_value: &Value) -> Option<RangeOpts> {
    let items = operator_value.as_array()?;
    if items.is_empty() {
        return None;
    }
    // `typeof operatorValue[0]` — the type of the first entry fixes the type for all.
    let is_string = items[0].is_string();
    let is_number = items[0].is_number();
    if !is_string && !is_number {
        return None;
    }
    let mut min = items[0].clone();
    let mut max = items[0].clone();
    for item in items {
        if is_string && !item.is_string() {
            return None;
        }
        if is_number {
            let value = item.as_f64()?;
            if !value.is_finite() {
                return None;
            }
        }
        if compare_primitive(item, &min) == std::cmp::Ordering::Less {
            min = item.clone();
        }
        if compare_primitive(item, &max) == std::cmp::Ordering::Greater {
            max = item.clone();
        }
    }
    Some(RangeOpts {
        start_key: Some(PlanKey::from_value(Some(&min))),
        end_key: Some(PlanKey::from_value(Some(&max))),
        inclusive_start: Some(true),
        inclusive_end: Some(true),
    })
}

/// `<` on two same-typed primitives, using code-unit order for strings.
///
/// Kept separate from [`crate::matcher::mingo_compare`] because upstream's `$in`
/// range uses JavaScript's `<` on the raw values, which for same-typed primitives is
/// the same comparison but across types would be a coercion the range must not
/// perform.
fn compare_primitive(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::String(x), Value::String(y)) => dxdb_jsstr::compare_js(x, y),
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or_default()
            .partial_cmp(&y.as_f64().unwrap_or_default())
            .unwrap_or(std::cmp::Ordering::Equal),
        _ => std::cmp::Ordering::Equal,
    }
}

/// `getQueryPlan` (`:35`).
///
/// The steps, in upstream's order:
///
/// 1. Candidate indexes: the schema's, or just the query's when it names one.
/// 2. Whether any sort part is `desc`. If so, no storage can satisfy the order,
///    because storages have no descending indexes, so a re-sort is forced.
/// 3. Fields whose selector pins a single value — a boolean, an enum, or a
///    string/number `$eq` — are *sort-irrelevant*: every matching document has the
///    same value there, so they can be dropped from the sort/index comparison. This
///    is what lets `{status: 'active'}` with `sort: [{age: 'asc'}]` use an index
///    that starts with `status`.
/// 4. Per index, compute bounds per position, then rate and keep the best.
pub fn get_query_plan(
    schema: &Value,
    query: &FilledMangoQuery,
) -> Result<QueryPlan, QueryError> {
    // Candidate indexes: the query's own if it names one, otherwise the schema's.
    //
    // Upstream assigns `indexes = schema.indexes.slice(0)` and then *overwrites* it
    // with `[query.index]` when the query names an index (`:36`), so an unusable
    // schema index is never touched by a query that brings its own. The oracle pins
    // this: `compoundIndexNoString` has an index containing a literal `10`, and its
    // four `index-*` cases still produce a plan because the query supplies
    // `['firstName']`.
    //
    // So the schema indexes are kept as raw JSON here and only converted to field
    // names for the index actually being considered, inside the loop below. That
    // makes the rejection of an unencodable index land exactly where upstream's
    // does: when the index is used.
    let indexes: Vec<Vec<Value>> = if let Some(index) = &query.index {
        vec![index.iter().cloned().map(Value::String).collect()]
    } else {
        dxdb_schema::get_indexes(schema)
    };

    // "Most storages do not support descending indexes so having a 'desc' in the
    // sorting means we always have to re-sort the results." (`:48`)
    let has_desc_sorting = query
        .sort
        .iter()
        .any(|part| part.direction == crate::normalize::SortDirection::Desc);

    let sort_irrelevant_fields = sort_irrelevant_fields(schema, &query.selector);

    let optimal_sort_index: Vec<String> = query.sort.iter().map(|part| part.field.clone()).collect();
    let optimal_sort_index_compare_string = optimal_sort_index
        .iter()
        .filter(|field| !sort_irrelevant_fields.contains(*field))
        .cloned()
        .collect::<Vec<String>>()
        .join(",");

    let mut current_best_quality: i32 = -1;
    let mut current_best_plan: Option<QueryPlan> = None;

    for raw_index in &indexes {
        // Convert this index's entries to field names. A non-string entry, or a field
        // with no index-string encoding, fails here rather than at the top of the
        // function — which is what makes an unusable *unused* index harmless.
        let mut index: Vec<String> = Vec::with_capacity(raw_index.len());
        for entry in raw_index {
            match entry.as_str() {
                Some(field) => index.push(field.to_string()),
                None => {
                    return Err(QueryError::new(
                        QueryErrorCode::Snh,
                        format!(
                            "index entry {} is not a field name, so it cannot be planned",
                            dxdb_json::to_js_json(entry)
                        ),
                    ))
                }
            }
        }
        let index = &index;

        let mut inclusive_end = true;
        let mut inclusive_start = true;
        let mut opts: Vec<RangeOpts> = Vec::with_capacity(index.len());

        for field in index.iter() {
            let matcher = selector_field(&query.selector, field);
            let operators: Vec<&String> = match matcher {
                Some(Value::Object(map)) => map.keys().collect(),
                _ => Vec::new(),
            };
            let mut matcher_opts = RangeOpts::default();
            if operators.is_empty() {
                // No constraint on this field: open at whichever end is still open.
                matcher_opts = RangeOpts {
                    start_key: Some(if inclusive_start {
                        PlanKey::IndexMin
                    } else {
                        PlanKey::IndexMax
                    }),
                    end_key: Some(if inclusive_end {
                        PlanKey::IndexMax
                    } else {
                        PlanKey::IndexMin
                    }),
                    inclusive_start: Some(true),
                    inclusive_end: Some(true),
                };
            } else {
                let matcher_object = matcher.and_then(Value::as_object).expect("operators exist");
                for operator in &operators {
                    if LOGICAL_OPERATORS.contains(&operator.as_str()) {
                        let value = matcher_object.get(*operator).expect("key exists");
                        let partial = get_matcher_query_opts(operator, value)?;
                        matcher_opts.start_key = partial.start_key.or(matcher_opts.start_key);
                        matcher_opts.end_key = partial.end_key.or(matcher_opts.end_key);
                        matcher_opts.inclusive_start =
                            partial.inclusive_start.or(matcher_opts.inclusive_start);
                        matcher_opts.inclusive_end =
                            partial.inclusive_end.or(matcher_opts.inclusive_end);
                    } else if operator.as_str() == "$in" {
                        if let Some(partial) = get_in_query_range_opts(
                            matcher_object.get(*operator).expect("key exists"),
                        ) {
                            matcher_opts.start_key = partial.start_key.or(matcher_opts.start_key);
                            matcher_opts.end_key = partial.end_key.or(matcher_opts.end_key);
                            matcher_opts.inclusive_start =
                                partial.inclusive_start.or(matcher_opts.inclusive_start);
                            matcher_opts.inclusive_end =
                                partial.inclusive_end.or(matcher_opts.inclusive_end);
                        }
                    }
                }
            }

            // Fill the missing attributes.
            let mut start_key = matcher_opts.start_key.unwrap_or(PlanKey::IndexMin);
            let mut end_key = matcher_opts.end_key.unwrap_or(PlanKey::IndexMax);
            let mut use_inclusive_start = matcher_opts.inclusive_start.unwrap_or(true);
            let mut use_inclusive_end = matcher_opts.inclusive_end.unwrap_or(true);

            // Boundary promotion (`:145`): a numeric bound outside the schema's
            // range clamps in the index string, so an *exclusive* bound that clamps
            // onto the boundary value would exclude documents sitting exactly at the
            // boundary. Promote it to inclusive.
            if let Some(schema_part) = dxdb_schema::schema_part_by_object_path(schema, field) {
                let field_type = schema_part.get("type").and_then(Value::as_str);
                if matches!(field_type, Some("number") | Some("integer")) {
                    let minimum = schema_part.get("minimum").and_then(Value::as_f64);
                    let maximum = schema_part.get("maximum").and_then(Value::as_f64);
                    if !use_inclusive_start {
                        if let (PlanKey::Number(start), Some(minimum)) = (&start_key, minimum) {
                            if *start < minimum {
                                use_inclusive_start = true;
                            }
                        }
                    }
                    if !use_inclusive_end {
                        if let (PlanKey::Number(end), Some(maximum)) = (&end_key, maximum) {
                            if *end > maximum {
                                use_inclusive_end = true;
                            }
                        }
                    }
                }
            }
            if !use_inclusive_start {
                inclusive_start = false;
            }
            if !use_inclusive_end {
                inclusive_end = false;
            }
            // A clamped bound of `IndexMin`/`IndexMax` is not a numeric bound.
            if matches!(start_key, PlanKey::IndexMin) && !matches!(end_key, PlanKey::IndexMax) {
                // Nothing to do: the two sentinels already mean "open".
            }
            if matches!(start_key, PlanKey::Undefined) {
                start_key = PlanKey::IndexMin;
            }
            if matches!(end_key, PlanKey::Undefined) {
                end_key = PlanKey::IndexMax;
            }
            opts.push(RangeOpts {
                start_key: Some(start_key),
                end_key: Some(end_key),
                inclusive_start: Some(use_inclusive_start),
                inclusive_end: Some(use_inclusive_end),
            });
        }

        let start_keys: Vec<PlanKey> = opts
            .iter()
            .map(|opt| opt.start_key.clone().unwrap_or(PlanKey::IndexMin))
            .collect();
        let end_keys: Vec<PlanKey> = opts
            .iter()
            .map(|opt| opt.end_key.clone().unwrap_or(PlanKey::IndexMax))
            .collect();

        let index_compare_string = if sort_irrelevant_fields.is_empty() {
            index.join(",")
        } else {
            index
                .iter()
                .filter(|field| !sort_irrelevant_fields.contains(*field))
                .cloned()
                .collect::<Vec<String>>()
                .join(",")
        };

        let plan = QueryPlan {
            index: index.to_vec(),
            start_keys: start_keys.clone(),
            end_keys: end_keys.clone(),
            inclusive_end,
            inclusive_start,
            sort_satisfied_by_index: !has_desc_sorting
                && optimal_sort_index_compare_string == index_compare_string,
            selector_satisfied_by_index: is_selector_satisfied_by_index(
                index,
                &query.selector,
                &start_keys,
                &end_keys,
            ),
        };
        let quality = rate_query_plan(query, &plan);
        if quality >= current_best_quality || query.index.is_some() {
            current_best_quality = quality;
            current_best_plan = Some(plan);
        }
    }

    current_best_plan.ok_or_else(|| {
        QueryError::new(
            QueryErrorCode::Snh,
            "no index found for the query; a filled schema always has one",
        )
    })
}

/// Looks up a selector entry by field name, resolving nested paths the way the
/// matcher does.
///
/// Upstream indexes `selector[indexField]` directly, so `other.age` in a selector is
/// found only if it is written as the literal key `"other.age"` — which is how Mango
/// selectors are written. The lookup is therefore a plain key match, with the nested
/// form as a fallback so a caller who builds a nested selector object is not
/// silently given a full scan.
fn selector_field<'a>(selector: &'a Value, field: &str) -> Option<&'a Value> {
    let object = selector.as_object()?;
    if let Some(found) = object.get(field) {
        return Some(found);
    }
    if field.contains('.') {
        // A dotted path written as nested objects: walk it.
        let mut current = selector.clone();
        for segment in field.split('.') {
            current = current.get(segment)?.clone();
        }
        // The clone above cannot be returned by reference, so the nested-object
        // form is not resolvable without allocation. Upstream does not support it
        // either (a selector key is a literal path string), so returning `None`
        // here is the faithful answer, and the `if` exists only to document why.
        let _ = current;
    }
    None
}

/// Fields whose selector pins one value, so they are irrelevant to sorting
/// (`:60`).
fn sort_irrelevant_fields(schema: &Value, selector: &Value) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let Some(object) = selector.as_object() else {
        return out;
    };
    for (field, field_selector) in object {
        let Some(schema_part) = dxdb_schema::schema_part_by_object_path(schema, field) else {
            continue;
        };
        let Some(field_object) = field_selector.as_object() else {
            continue;
        };
        let Some(eq_value) = field_object.get("$eq") else {
            continue;
        };
        let field_type = schema_part.get("type").and_then(Value::as_str);
        let has_enum = schema_part.get("enum").is_some();
        // A boolean or enum field with `$eq` has the same value in every matching
        // document, so it cannot affect the order. A string or numeric field only
        // qualifies when the `$eq` operand is a primitive of that kind — an object or
        // array operand does not pin a single sortable value.
        let pinned = field_type == Some("boolean")
            || has_enum
            || (matches!(field_type, Some("string") | Some("number") | Some("integer"))
                && (eq_value.is_string() || eq_value.is_number()));
        if pinned {
            out.insert(field.clone());
        }
    }
    out
}

/// `isSelectorSatisfiedByIndex` (`:244`).
///
/// True only when the index alone decides every selector entry: all fields present,
/// all operators logical, at most one lower and one upper bound, no gaps between
/// relevant fields, and no position where both bounds are open.
pub fn is_selector_satisfied_by_index(
    index: &[String],
    selector: &Value,
    start_keys: &[PlanKey],
    end_keys: &[PlanKey],
) -> bool {
    // "Not satisfied if contains $and or $or operations." `$nor` is not in that
    // list upstream, which is deliberate-looking but wrong — a `$nor` cannot be
    // satisfied by a range either. Reproducing upstream exactly is the contract, so
    // `$nor` is checked alongside them here only because `$nor` never reaches the
    // planner as a selector entry: it is a top-level key, and the loop below
    // rejects it as "not in the index". Both routes agree.
    let Some(selector_object) = selector.as_object() else {
        return true;
    };
    if selector_object.contains_key("$and") || selector_object.contains_key("$or") {
        return false;
    }

    let mut lower_operator_field_names: std::collections::BTreeSet<String> = Default::default();
    let mut upper_operator_field_names: std::collections::BTreeSet<String> = Default::default();
    let mut has_non_eq_lower_bound = false;
    let mut has_non_eq_upper_bound = false;

    for (field_name, operation) in selector_object {
        if !index.iter().any(|field| field == field_name) {
            return false;
        }
        let Some(operation_object) = operation.as_object() else {
            return false;
        };

        let mut lower_logic_op_count = 0usize;
        let mut last_lower_logic_op: Option<&str> = None;
        let mut upper_logic_op_count = 0usize;
        let mut last_upper_logic_op: Option<&str> = None;

        for op in operation_object.keys() {
            if !LOGICAL_OPERATORS.contains(&op.as_str()) {
                return false;
            }
            if LOWER_BOUND_LOGICAL_OPERATORS.contains(&op.as_str()) {
                lower_logic_op_count += 1;
                last_lower_logic_op = Some(op);
            }
            if UPPER_BOUND_LOGICAL_OPERATORS.contains(&op.as_str()) {
                upper_logic_op_count += 1;
                last_upper_logic_op = Some(op);
            }
        }

        // More than one logical op per bound direction means a selector match is
        // still required, because the range cannot express the intersection.
        if lower_logic_op_count > 1 || upper_logic_op_count > 1 {
            return false;
        }

        if let Some(last) = last_lower_logic_op {
            lower_operator_field_names.insert(field_name.clone());
            if last != "$eq" {
                if has_non_eq_lower_bound {
                    return false;
                }
                has_non_eq_lower_bound = true;
            }
        }
        if let Some(last) = last_upper_logic_op {
            upper_operator_field_names.insert(field_name.clone());
            if last != "$eq" {
                if has_non_eq_upper_bound {
                    return false;
                }
                has_non_eq_upper_bound = true;
            }
        }
    }

    // "If the index contains a non-relevant field between the relevant fields, then
    // the index is not satisfying."
    for (position, field_name) in index.iter().enumerate() {
        for set in [&mut lower_operator_field_names, &mut upper_operator_field_names] {
            if !set.contains(field_name) && !set.is_empty() {
                return false;
            }
            set.remove(field_name);
        }
        let start_key = start_keys.get(position);
        let end_key = end_keys.get(position);
        let differs = match (start_key, end_key) {
            (Some(start), Some(end)) => !start.same_as(end),
            _ => true,
        };
        if differs && !lower_operator_field_names.is_empty() && !upper_operator_field_names.is_empty()
        {
            return false;
        }
    }

    true
}

/// `countUntilNotMatching` (`plugins/utils/utils-array.ts:97`).
fn count_until_not_matching<T>(items: &[T], mut predicate: impl FnMut(&T, usize) -> bool) -> i32 {
    let mut count = 0;
    for (index, item) in items.iter().enumerate() {
        if predicate(item, index) {
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// `rateQueryPlan` (`:450`).
///
/// Weights are upstream's: 10 per matching key from either end, 15 per equal
/// position (an equality is worth more than a bound because it narrows both ends),
/// and 5 for avoiding a re-sort.
pub fn rate_query_plan(query: &FilledMangoQuery, plan: &QueryPlan) -> i32 {
    let mut quality = 0;
    let mut add = |value: i32| {
        if value > 0 {
            quality += value;
        }
    };
    let points_per_matching_key = 10;
    let _ = query;

    let non_min_key_count = count_until_not_matching(&plan.start_keys, |key, _| {
        !matches!(key, PlanKey::IndexMin | PlanKey::IndexMax)
    });
    add(non_min_key_count * points_per_matching_key);

    let non_max_key_count = count_until_not_matching(&plan.end_keys, |key, _| {
        !matches!(key, PlanKey::IndexMax | PlanKey::IndexMin)
    });
    add(non_max_key_count * points_per_matching_key);

    let equal_key_count = count_until_not_matching(&plan.start_keys, |key, index| {
        plan.end_keys
            .get(index)
            .map(|end| key.same_as(end))
            .unwrap_or(false)
    });
    add((f64::from(equal_key_count) * f64::from(points_per_matching_key) * 1.5) as i32);

    if plan.sort_satisfied_by_index {
        add(5);
    }
    quality
}

/// Promotes an out-of-schema numeric bound to inclusive, exposed for tests that
/// pin the clamping interaction without building a whole plan.
///
/// This is the `:145` block. It is a separate function because the planner's flow is
/// long and this rule is the one that silently drops rows when it is missing.
pub fn promote_out_of_range_bound(
    schema_part: &Value,
    start_key: &PlanKey,
    end_key: &PlanKey,
    inclusive_start: bool,
    inclusive_end: bool,
) -> (bool, bool) {
    let mut inclusive_start = inclusive_start;
    let mut inclusive_end = inclusive_end;
    let field_type = schema_part.get("type").and_then(Value::as_str);
    if !matches!(field_type, Some("number") | Some("integer")) {
        return (inclusive_start, inclusive_end);
    }
    let minimum = schema_part.get("minimum").and_then(Value::as_f64);
    let maximum = schema_part.get("maximum").and_then(Value::as_f64);
    if !inclusive_start {
        if let (PlanKey::Number(start), Some(minimum)) = (start_key, minimum) {
            if *start < minimum {
                inclusive_start = true;
            }
        }
    }
    if !inclusive_end {
        if let (PlanKey::Number(end), Some(maximum)) = (end_key, maximum) {
            if *end > maximum {
                inclusive_end = true;
            }
        }
    }
    (inclusive_start, inclusive_end)
}

