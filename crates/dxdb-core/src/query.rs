//! Query preparation: the one place a collection meets `dxdb-query`.
//!
//! Upstream `prepareQuery` (`src/rx-query-helper.ts:345`) does two things: normalise, then
//! plan. This module adds the third thing a *collection* does and a storage must not:
//! **add `_deleted: false` to the selector**. A collection never returns tombstones from
//! `find()`, while a storage returns whatever the selector matches — the conformance suite
//! has a test asserting exactly that split ("Notice that the RxStorage itself runs whatever
//! query you give it, filtering out deleted documents is done by RxDB, not by the
//! storage").
//!
//! The output is the storage ABI's [`PreparedQuery`] rather than the query crate's types,
//! so a storage implementor does not need `dxdb-query` to consume a plan.

use dxdb_query::normalize::{
    normalize_mango_query, MangoQuery as QueryMangoQuery, SortDirection as QueryDirection,
};
use dxdb_query::plan::{get_query_plan, PlanKey as QueryPlanKey};
use dxdb_storage::{MangoQuery, PlanKey, PreparedQuery, QueryPlan, SortDirection, SortPart};
use serde_json::{json, Value};

use crate::error::{CoreError, CoreResult};

/// Normalises `query` for a collection on `schema`, adding the tombstone filter, then
/// plans it.
///
/// Accepts the loose shape a user writes (`{}`, `{ selector }`, no sort) and produces the
/// filled shape a storage requires: `skip` always set, `sort` always ending in the primary
/// key, `selector` always an object with operator-form values.
pub fn prepare(schema: &Value, query: &Value, primary_path: &str) -> CoreResult<PreparedQuery> {
    let query_object = query
        .as_object()
        .ok_or_else(|| CoreError::Query("a query must be a JSON object".to_string()))?;

    // `selector` may be absent entirely; core fills an empty object and then adds the
    // tombstone filter.
    let mut selector = query_object
        .get("selector")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !selector.is_object() {
        return Err(CoreError::Query("selector must be an object".to_string()));
    }
    // A user-supplied `_deleted` wins: `findOne({selector: {_deleted: true}})` after
    // `withDeleted` must be able to ask for tombstones deliberately.
    if let Some(map) = selector.as_object_mut() {
        if !map.contains_key("_deleted") {
            map.insert("_deleted".to_string(), json!(false));
        }
    }

    let sort = query_object
        .get("sort")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    let object = part.as_object()?;
                    let (field, direction) = object.iter().next()?;
                    Some(dxdb_query::normalize::SortPart {
                        field: field.clone(),
                        direction: if direction == "desc" {
                            QueryDirection::Desc
                        } else {
                            QueryDirection::Asc
                        },
                    })
                })
                .collect::<Vec<_>>()
        });

    let indexes = dxdb_schema::get_string_indexes(schema)
        .map_err(|error| CoreError::Schema(error.to_string()))?;

    let filled = normalize_mango_query(
        &QueryMangoQuery {
            selector: Some(selector),
            sort,
            skip: query_object.get("skip").and_then(Value::as_u64),
            limit: query_object.get("limit").and_then(Value::as_u64),
            index: query_object.get("index").and_then(|index| match index {
                Value::String(field) => Some(vec![field.clone()]),
                Value::Array(fields) => Some(
                    fields
                        .iter()
                        .filter_map(|field| field.as_str().map(str::to_string))
                        .collect(),
                ),
                _ => None,
            }),
        },
        primary_path,
        &indexes,
        false,
    )
    .map_err(|error| CoreError::Query(error.to_string()))?;

    let plan =
        get_query_plan(schema, &filled).map_err(|error| CoreError::Query(error.to_string()))?;

    Ok(PreparedQuery {
        query: MangoQuery {
            selector: filled.selector.clone(),
            sort: filled
                .sort
                .iter()
                .map(|part| SortPart {
                    field: part.field.clone(),
                    direction: match part.direction {
                        QueryDirection::Asc => SortDirection::Asc,
                        QueryDirection::Desc => SortDirection::Desc,
                    },
                })
                .collect(),
            skip: filled.skip as usize,
            limit: filled.limit.map(|limit| limit as usize),
            index: filled.index.clone(),
        },
        query_plan: QueryPlan {
            index: plan.index.clone(),
            sort_satisfied_by_index: plan.sort_satisfied_by_index,
            selector_satisfied_by_index: plan.selector_satisfied_by_index,
            start_keys: plan.start_keys.iter().map(lift_key).collect(),
            end_keys: plan.end_keys.iter().map(lift_key).collect(),
            inclusive_start: plan.inclusive_start,
            inclusive_end: plan.inclusive_end,
        },
    })
}

/// Lifts one query-crate plan key into the storage ABI's key.
///
/// The sentinels are recognised by *value* through the query crate's own accessors, so a
/// plan whose bound happens to equal `MIN_SAFE_INTEGER` is treated as the sentinel exactly
/// as upstream's `===` comparison would (D8).
fn lift_key(key: &QueryPlanKey) -> PlanKey {
    match key {
        QueryPlanKey::IndexMin => PlanKey::Number(dxdb_query::custom_index::INDEX_MIN as f64),
        QueryPlanKey::IndexMax => PlanKey::String(dxdb_query::custom_index::INDEX_MAX.to_string()),
        QueryPlanKey::String(text) => PlanKey::String(text.clone()),
        QueryPlanKey::Number(number) => PlanKey::Number(*number),
        QueryPlanKey::Bool(flag) => PlanKey::Boolean(*flag),
        // `Null` and `Undefined` are both "no bound" on the storage side; upstream
        // distinguishes them only for `$eq: null`, which the planner has already turned
        // into an explicit bound by the time a storage sees it.
        QueryPlanKey::Null | QueryPlanKey::Undefined => PlanKey::Unbounded,
    }
}
