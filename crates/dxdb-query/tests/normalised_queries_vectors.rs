//! Oracle tests: normalisation and query plans, read through `dxdb-vectors`.
//!
//! `testdata/vectors/normalised-queries.json` was produced by upstream RxDB 17.6.0 in Node.
//! Each case carries the input query, the normalised query, the plan, the plan's serialised
//! JSON, and a flag saying whether the plan survives a JSON round trip.
//!
//! This is the largest semantic surface in the crate — sort derivation, primary-key appending,
//! the planner's `startKeys`/`endKeys`/`inclusive*` and both `*SatisfiedByIndex` flags — so it
//! is asserted field by field rather than by comparing whole objects.
//!
//! # Why the round-trip flag is not redundant with the plan
//!
//! `planRoundTripEqual` records `JSON.parse(JSON.stringify(plan)) === plan`. That is exactly
//! the property `INDEX_MIN = Number.MIN_SAFE_INTEGER` exists for (`src/query-planner.ts:20`):
//! a plan is serialised to a storage worker, and `-Infinity` would come back as `null` and
//! break the scan. So the byte-exact `planJson` and the round trip are separate assertions,
//! and a `Value` comparison would hide the difference (`20` versus `20.0`).

use serde_json::Value;

use dxdb_query::{
    get_query_plan, get_sort_comparator, normalize_mango_query, MangoQuery, Matcher, PlanKey,
    QueryPlan,
};
use dxdb_vectors::{normalised_queries, schema_hashes, VectorFamily, Vectors};

fn load() -> normalised_queries::Vectors {
    let vectors = Vectors::discover();
    vectors
        .load(VectorFamily::NormalisedQueries)
        .expect("normalised-queries.json loads through dxdb-vectors")
}

/// The fixture schemas, from the `schema-hashes` family, which records each input once.
///
/// `averageSchema` is the exception: upstream builds it in a function whose `title` includes
/// `randomString(5)` (`src/plugins/test-utils/schemas.ts:950`), so no two runs produce the same
/// schema and it cannot be recorded verbatim. Its structure is stable and is reproduced below
/// from that fixture; only `title` differs, and `title` affects neither normalisation nor
/// planning.
fn schema_lookup() -> std::collections::HashMap<String, Value> {
    let vectors = Vectors::discover();
    let hashes: schema_hashes::Vectors = vectors
        .load(VectorFamily::SchemaHashes)
        .expect("schema-hashes.json loads through dxdb-vectors");
    let mut lookup = std::collections::HashMap::new();
    for case in &hashes.cases {
        lookup
            .entry(case.name.clone())
            .or_insert_with(|| case.input.clone());
    }
    lookup
}

fn average_schema() -> Value {
    serde_json::json!({
        "title": "averageSchema_test",
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "keyCompression": false,
        "properties": {
            "id": {"description": "id", "type": "string", "maxLength": 12},
            "var1": {"description": "var1", "type": "string", "maxLength": 12},
            "var2": {"description": "var2", "type": "number", "minimum": 0, "maximum": 50000, "multipleOf": 1},
            "deep": {
                "type": "object",
                "properties": {
                    "deep1": {"type": "string", "maxLength": 10},
                    "deep2": {"type": "string", "maxLength": 10}
                }
            },
            "list": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "deep1": {"type": "string"},
                        "deep2": {"type": "string"}
                    }
                }
            }
        },
        "required": ["id", "var1", "var2"],
        "indexes": ["var1", "var2", "deep.deep1", ["var2", "var1"]],
        "sharding": {"shards": 6, "mode": "collection"}
    })
}

fn filled(name: &str, lookup: &std::collections::HashMap<String, Value>) -> Value {
    let schema = match lookup.get(name) {
        Some(schema) => schema.clone(),
        None if name == "averageSchema" => average_schema(),
        None => panic!("normalised-queries names an unknown schema: {name}"),
    };
    dxdb_schema::fill_with_default_settings(&schema)
        .unwrap_or_else(|error| panic!("fixture {name} does not fill: {error}"))
}

/// Maps the typed plan key onto the crate's own, so the comparison is on values.
fn plan_key(key: &normalised_queries::PlanKey) -> PlanKey {
    match key {
        normalised_queries::PlanKey::Text(text) if text == dxdb_query::INDEX_MAX_STR => {
            PlanKey::IndexMax
        }
        normalised_queries::PlanKey::Text(text) => PlanKey::String(text.clone()),
        normalised_queries::PlanKey::Bool(flag) => PlanKey::Bool(*flag),
        normalised_queries::PlanKey::Number(number)
            if *number == dxdb_query::INDEX_MIN as f64 =>
        {
            PlanKey::IndexMin
        }
        normalised_queries::PlanKey::Number(number) => PlanKey::Number(*number),
        normalised_queries::PlanKey::Null => PlanKey::Null,
    }
}

fn plan_of(plan: &normalised_queries::Plan) -> QueryPlan {
    QueryPlan {
        index: plan.index.clone(),
        start_keys: plan.start_keys.iter().map(plan_key).collect(),
        end_keys: plan.end_keys.iter().map(plan_key).collect(),
        inclusive_start: plan.inclusive_start,
        inclusive_end: plan.inclusive_end,
        sort_satisfied_by_index: plan.sort_satisfied_by_index,
        selector_satisfied_by_index: plan.selector_satisfied_by_index,
    }
}

/// The family is the size the vectors crate expects.
#[test]
fn the_family_has_its_documented_case_count() {
    let family = load();
    assert_eq!(family.cases.len(), family.case_count);
    let expected = dxdb_vectors::EXPECTED_CASE_COUNTS
        .iter()
        .find(|(candidate, _)| *candidate == VectorFamily::NormalisedQueries)
        .map(|(_, count)| *count)
        .expect("NormalisedQueries is in EXPECTED_CASE_COUNTS");
    assert_eq!(family.cases.len(), expected);
    // The sentinel block is the family's own statement of `INDEX_MIN`/`INDEX_MAX`; assert it
    // against the crate so a change to either is caught here as well as in
    // `oracle_sentinels.rs`.
    assert_eq!(
        family.sentinels.index_min as i64,
        dxdb_query::INDEX_MIN,
        "the family records a different INDEX_MIN than the crate defines"
    );
    assert_eq!(family.sentinels.index_max, dxdb_query::INDEX_MAX_STR);
}

/// Every plan case: normalised query, plan, byte-exact `planJson`, and the round trip.
#[test]
fn normalised_queries_and_plans_match_upstream_oracle() {
    let family = load();
    let lookup = schema_lookup();
    let mut checked = 0;
    let mut rejected = 0;
    let mut failures = Vec::new();

    for case in &family.cases {
        let (schema_name, why, query_json, expected_normalized, expected_plan, plan_json, round_trip) =
            match case {
                normalised_queries::Case::Planned(case) => (
                    &case.schema,
                    &case.why,
                    &case.query,
                    &case.normalized_query,
                    &case.plan,
                    &case.plan_json,
                    case.plan_round_trip_equal,
                ),
                // A case upstream failed to normalise is asserted by
                // `schemas_upstream_cannot_plan_are_rejected_here`.
                normalised_queries::Case::Rejected(_) => {
                    rejected += 1;
                    continue;
                }
            };

        let schema = filled(schema_name, &lookup);
        let primary_key = dxdb_schema::primary_field_of_primary_key(&schema).expect("primary key");
        let indexes = dxdb_schema::get_string_indexes(&schema).unwrap_or_default();

        let query = match MangoQuery::from_json(query_json) {
            Ok(query) => query,
            Err(error) => {
                failures.push(format!("{schema_name} {why}: query does not parse: {error}"));
                continue;
            }
        };
        let normalized = match normalize_mango_query(&query, &primary_key, &indexes, false) {
            Ok(normalized) => normalized,
            Err(error) => {
                failures.push(format!("{schema_name} {why}: normalisation failed: {error}"));
                continue;
            }
        };

        // 1. The normalised query, as a value with key order.
        if normalized.to_json() != *expected_normalized {
            failures.push(format!(
                "{schema_name} {why}: normalized query differs.\n  ours:     {}\n  upstream: {}",
                dxdb_json::to_js_json(&normalized.to_json()),
                dxdb_json::to_js_json(expected_normalized)
            ));
            continue;
        }

        // 2. The plan, as a value.
        let expected_plan = plan_of(expected_plan);
        let actual_plan = match get_query_plan(&schema, &normalized) {
            Ok(plan) => plan,
            Err(error) => {
                failures.push(format!("{schema_name} {why}: planning failed: {error}"));
                continue;
            }
        };
        if actual_plan != expected_plan {
            failures.push(format!(
                "{schema_name} {why}: plan differs.\n  ours:     {}\n  upstream: {}",
                dxdb_json::to_js_json(&actual_plan.to_json()),
                dxdb_json::to_js_json(&expected_plan.to_json())
            ));
            continue;
        }

        // 3. The plan's serialised form, exactly as upstream records it. This is where a
        //    `20.0` from serde's float writer would show up as a divergence.
        let our_json = actual_plan.to_json_string();
        if our_json != *plan_json {
            failures.push(format!(
                "{schema_name} {why}: planJson differs.\n  ours:     {our_json}\n  upstream: {plan_json}"
            ));
            continue;
        }

        // 4. And the round trip the family asserts, compared as text because a `Value`
        //    comparison cannot see the difference between `20` and `20.0`.
        if round_trip {
            let parsed: Value = serde_json::from_str(&our_json).expect("plan parses back");
            let re_serialised = plan_json_from_parsed(&parsed, &actual_plan);
            if re_serialised != our_json {
                failures.push(format!(
                    "{schema_name} {why}: plan does not survive a JSON round trip.\n  before: \
                     {our_json}\n  after:  {re_serialised}"
                ));
                continue;
            }
        }
        checked += 1;
    }

    assert!(
        failures.is_empty(),
        "{} of {} normalised-query cases diverged:\n{}",
        failures.len(),
        checked + failures.len(),
        failures.join("\n")
    );
    assert_eq!(
        checked + rejected,
        family.cases.len(),
        "every case must be either checked ({checked}) or a recorded upstream failure ({rejected})"
    );
    assert_eq!(rejected, family.failures.len(), "failure count");
    assert!(checked >= 700, "expected at least 700 comparable cases, got {checked}");
}

/// Rebuilds a plan from parsed JSON text so it can be serialised again, which is the round-trip
/// comparison: text before must equal text after.
fn plan_json_from_parsed(parsed: &Value, original: &QueryPlan) -> String {
    let read_keys = |value: &Value| -> Vec<PlanKey> {
        value
            .as_array()
            .expect("keys array")
            .iter()
            .map(|key| match key {
                Value::Null => PlanKey::Null,
                Value::Bool(flag) => PlanKey::Bool(*flag),
                Value::String(text) if text == dxdb_query::INDEX_MAX_STR => PlanKey::IndexMax,
                Value::String(text) => PlanKey::String(text.clone()),
                Value::Number(number) => match number.as_f64() {
                    Some(float) if float == dxdb_query::INDEX_MIN as f64 => PlanKey::IndexMin,
                    Some(float) => PlanKey::Number(float),
                    None => PlanKey::Undefined,
                },
                Value::Array(_) | Value::Object(_) => PlanKey::Undefined,
            })
            .collect()
    };
    QueryPlan {
        index: original.index.clone(),
        start_keys: read_keys(&parsed["startKeys"]),
        end_keys: read_keys(&parsed["endKeys"]),
        inclusive_start: parsed["inclusiveStart"].as_bool().expect("inclusiveStart"),
        inclusive_end: parsed["inclusiveEnd"].as_bool().expect("inclusiveEnd"),
        sort_satisfied_by_index: parsed["sortSatisfiedByIndex"]
            .as_bool()
            .expect("sortSatisfiedByIndex"),
        selector_satisfied_by_index: parsed["selectorSatisfiedByIndex"]
            .as_bool()
            .expect("selectorSatisfiedByIndex"),
    }
    .to_json_string()
}

/// Schemas upstream cannot plan must not be planned here either.
#[test]
fn schemas_upstream_cannot_plan_are_rejected_here() {
    let family = load();
    let lookup = schema_lookup();
    let mut checked = 0;
    let mut failures = Vec::new();

    assert!(
        !family.failures.is_empty(),
        "the family records upstream failures; none are present"
    );

    for case in &family.cases {
        let normalised_queries::Case::Rejected(case) = case else {
            continue;
        };
        if !case.expected_upstream_failure {
            failures.push(format!(
                "{} {}: recorded as rejected but not marked an expected upstream failure",
                case.schema, case.why
            ));
            continue;
        }
        let Some(schema_input) = lookup.get(&case.schema) else {
            continue;
        };
        // Filling is permissive by design; the refusal must appear at plan time.
        let Ok(schema) = dxdb_schema::fill_with_default_settings(schema_input) else {
            checked += 1;
            continue;
        };
        let Ok(primary_key) = dxdb_schema::primary_field_of_primary_key(&schema) else {
            checked += 1;
            continue;
        };
        let indexes = dxdb_schema::get_string_indexes(&schema).unwrap_or_default();
        let query = match MangoQuery::from_json(&case.query) {
            Ok(query) => query,
            Err(_) => {
                checked += 1;
                continue;
            }
        };
        let Ok(normalized) = normalize_mango_query(&query, &primary_key, &indexes, false) else {
            checked += 1;
            continue;
        };
        if get_query_plan(&schema, &normalized).is_ok() {
            failures.push(format!(
                "{} {}: upstream cannot plan this ({}) but dxdb produced a plan",
                case.schema, case.why, case.rejected
            ));
        } else {
            checked += 1;
        }
    }

    assert!(
        failures.is_empty(),
        "{} rejection cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(checked, family.failures.len(), "every failure case asserted");
}

/// `orderings`: the sort comparator against the family's fixed document set.
#[test]
fn sort_orderings_match_upstream_oracle() {
    let family = load();
    let lookup = schema_lookup();
    let schema = filled("human", &lookup);
    let primary_key = dxdb_schema::primary_field_of_primary_key(&schema).expect("primary key");
    let indexes = dxdb_schema::get_string_indexes(&schema).unwrap_or_default();
    let documents = fixture_documents();
    let mut checked = 0;

    assert!(!family.orderings.is_empty(), "orderings must be present");
    for ordering in &family.orderings {
        // Upstream built the comparator from the recorded normalised sort and sorted this
        // document set with it; feeding the same sort back through normalisation must be a
        // no-op, or the recorded ids would have been produced from a different order.
        let sort: Vec<Value> = ordering
            .normalized_sort
            .iter()
            .map(|part| {
                let map = &part.0;
                Value::Object(
                    map.iter()
                        .map(|(field, direction)| {
                            (field.clone(), Value::String(direction.clone()))
                        })
                        .collect(),
                )
            })
            .collect();
        let query = serde_json::json!({"selector": {}, "sort": sort});
        let parsed = MangoQuery::from_json(&query).expect("query parses");
        let normalized = normalize_mango_query(&parsed, &primary_key, &indexes, false)
            .expect("normalizes");
        assert_eq!(
            normalized.to_json()["sort"],
            Value::Array(sort.clone()),
            "{}: re-normalising the recorded sort must be a no-op",
            ordering.why
        );
        let comparator = get_sort_comparator(&normalized);
        let mut sorted = documents.clone();
        sorted.sort_by(|a, b| comparator.compare(a, b));
        let actual: Vec<String> = sorted
            .iter()
            .map(|document| document["passportId"].as_str().expect("id").to_string())
            .collect();
        assert_eq!(actual, ordering.sorted_ids, "{}: sort order", ordering.why);
        checked += 1;
    }
    assert_eq!(checked, family.orderings.len());
}

/// `matchers`: the selector matcher against the same document set, both directions.
#[test]
fn matchers_match_upstream_oracle() {
    let family = load();
    let documents = fixture_documents();
    let mut checked = 0;

    assert!(!family.matchers.is_empty(), "matchers must be present");
    for case in &family.matchers {
        let matcher = Matcher::new(&case.normalized_selector)
            .unwrap_or_else(|error| panic!("{}: selector does not compile: {error}", case.why));
        let expected: std::collections::BTreeSet<&str> =
            case.matching_ids.iter().map(String::as_str).collect();
        for document in &documents {
            let id = document["passportId"].as_str().expect("id");
            let should_match = expected.contains(id);
            assert_eq!(
                matcher.matches(document),
                should_match,
                "{}: document {id} of {} for selector {}",
                case.why,
                document,
                case.normalized_selector
            );
        }
        checked += 1;
    }
    assert_eq!(checked, family.matchers.len());
}

/// The document set the ordering and matcher sections were generated from
/// (`xtask/vectors/families/normalised-queries.mjs:262`).
fn fixture_documents() -> Vec<Value> {
    vec![
        serde_json::json!({"passportId": "a", "firstName": "bob", "age": 30, "_deleted": false}),
        serde_json::json!({"passportId": "b", "firstName": "alice", "age": 30, "_deleted": false}),
        serde_json::json!({"passportId": "c", "firstName": "alice", "age": 20, "_deleted": true}),
        serde_json::json!({"passportId": "d", "firstName": "", "age": 0, "_deleted": false}),
    ]
}
