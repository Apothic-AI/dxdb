//! Oracle tests: normalisation and query plans.
//!
//! `testdata/vectors/normalised-queries.json` was produced by upstream RxDB 17.6.0
//! in Node. Each case is `{schema, why, query, normalizedQuery, plan}` plus the
//! `planJson` string and a `planRoundTripEqual` flag.
//!
//! # Why the round-trip flag matters
//!
//! `planRoundTripEqual` records that `JSON.parse(JSON.stringify(plan))` is equal to
//! `plan`. That is exactly the property `INDEX_MIN = Number.MIN_SAFE_INTEGER`
//! exists for (`src/query-planner.ts:20`): a plan is serialised to a storage worker
//! in several plugins, and `-Infinity` would come back as `null` and break the scan.
//! So this file asserts three things per case — the normalized query, the plan, and
//! that the plan survives a JSON round trip — and a fourth globally, that the
//! sentinel really is `MIN_SAFE_INTEGER`.
//!
//! The family also carries `orderings` (a sort applied to a fixed document set) and
//! `matchers` (a selector applied to the same set), which are asserted directly
//! against [`dxdb_query::get_sort_comparator`] and [`dxdb_query::Matcher`] — they
//! are the only oracle coverage of sorting and matching anywhere in the repo, so they
//! are checked here rather than left to the ported unit tests.

use serde_json::Value;

use dxdb_query::{
    get_query_plan, get_sort_comparator, normalize_mango_query, MangoQuery, Matcher, PlanKey,
    QueryPlan,
};

fn load() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/vectors/normalised-queries.json"
    );
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("cannot read {path}: {error}. The oracle vectors are committed.")
    });
    serde_json::from_str(&text).expect("normalised-queries.json is valid JSON")
}

fn schema_lookup() -> std::collections::HashMap<String, Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/vectors/schema-hashes.json"
    );
    let text = std::fs::read_to_string(path).expect("schema-hashes.json is committed");
    let vector: Value = serde_json::from_str(&text).expect("valid JSON");
    let mut out = std::collections::HashMap::new();
    for case in vector["cases"].as_array().expect("cases array") {
        let name = case["name"].as_str().expect("name").to_string();
        out.entry(name).or_insert_with(|| case["input"].clone());
    }
    out
}

/// The filled schema for a fixture name.
///
/// Most fixtures come from the `schema-hashes` family, which records each input
/// schema once. `averageSchema` is the exception: upstream builds it in a function
/// whose `title` includes `randomString(5)` (`src/plugins/test-utils/schemas.ts:950`,
/// with the comment "randomisation used so hash differs"), so no two runs produce the
/// same schema and it cannot be recorded verbatim. Its structure is stable, and it is
/// reproduced below from that fixture — the copied parts are the fields, `maxLength`s,
/// numeric bounds, `required` list and `indexes`, all of which are what the plan
/// depends on; only `title` differs, and `title` affects neither normalization nor
/// planning.
fn filled(name: &str, lookup: &std::collections::HashMap<String, Value>) -> Value {
    let schema = match lookup.get(name) {
        Some(schema) => schema.clone(),
        None if name == "averageSchema" => average_schema(),
        None => panic!("normalised-queries names an unknown schema: {name}"),
    };
    dxdb_schema::fill_with_default_settings(&schema)
        .unwrap_or_else(|error| panic!("fixture {name} does not fill: {error}"))
}

/// `averageSchema()` from `src/plugins/test-utils/schemas.ts:948`, minus the
/// random title.
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

/// Recovers a plan from the oracle's JSON, so our plan can be compared as a value.
fn plan_from_json(plan: &Value, index: &[String]) -> QueryPlan {
    let keys = |value: &Value| -> Vec<PlanKey> {
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
                    Some(f) if f == dxdb_query::INDEX_MIN as f64 => PlanKey::IndexMin,
                    Some(f) => PlanKey::Number(f),
                    None => PlanKey::Undefined,
                },
                Value::Array(_) | Value::Object(_) => PlanKey::Undefined,
            })
            .collect()
    };
    QueryPlan {
        index: index.to_vec(),
        start_keys: keys(&plan["startKeys"]),
        end_keys: keys(&plan["endKeys"]),
        inclusive_start: plan["inclusiveStart"].as_bool().expect("inclusiveStart"),
        inclusive_end: plan["inclusiveEnd"].as_bool().expect("inclusiveEnd"),
        sort_satisfied_by_index: plan["sortSatisfiedByIndex"]
            .as_bool()
            .expect("sortSatisfiedByIndex"),
        selector_satisfied_by_index: plan["selectorSatisfiedByIndex"]
            .as_bool()
            .expect("selectorSatisfiedByIndex"),
    }
}

/// The sentinel is `Number.MIN_SAFE_INTEGER` and survives `JSON.stringify`.
///
/// This is asserted separately from the cases because it is the one constant whose
/// value is chosen for a reason that has nothing to do with query semantics, and
/// getting it wrong is invisible until a plan crosses a process boundary.
#[test]
fn index_min_is_min_safe_integer_and_round_trips() {
    let vector = load();
    let sentinels = &vector["sentinels"];

    assert_eq!(
        dxdb_query::INDEX_MIN,
        -9_007_199_254_740_991_i64,
        "INDEX_MIN must be Number.MIN_SAFE_INTEGER"
    );
    assert_eq!(
        sentinels["INDEX_MIN"].as_i64(),
        Some(dxdb_query::INDEX_MIN),
        "the oracle records the same sentinel"
    );
    assert_eq!(
        sentinels["isMinSafeInteger"].as_bool(),
        Some(true),
        "the oracle asserts MIN_SAFE_INTEGER"
    );
    // `-Infinity` under JSON.stringify is `null`, which is why it is not the
    // sentinel. The oracle records that too.
    assert_eq!(sentinels["minusInfinityJson"].as_str(), Some("null"));
    assert_eq!(sentinels["minusInfinityRoundTrips"].as_bool(), Some(false));
    assert_eq!(sentinels["indexMinRoundTrips"].as_bool(), Some(true));

    // And the code must agree: a plan containing both sentinels must survive a
    // JSON round trip unchanged.
    let plan = QueryPlan {
        index: vec!["_deleted".to_string(), "id".to_string()],
        start_keys: vec![PlanKey::IndexMin, PlanKey::IndexMin],
        end_keys: vec![PlanKey::IndexMax, PlanKey::IndexMax],
        inclusive_start: true,
        inclusive_end: true,
        sort_satisfied_by_index: true,
        selector_satisfied_by_index: true,
    };
    let json = plan.to_json();
    let text = plan.to_json_string();
    let round_tripped: Value = serde_json::from_str(&text).expect("plan parses");
    assert_eq!(
        round_tripped, json,
        "the plan must survive JSON.stringify/parse unchanged; text was {text}"
    );
    assert_eq!(
        text,
        "{\"index\":[\"_deleted\",\"id\"],\"startKeys\":[-9007199254740991,-9007199254740991],\
         \"endKeys\":[\"\u{FFFF}\",\"\u{FFFF}\"],\"inclusiveEnd\":true,\"inclusiveStart\":true,\
         \"sortSatisfiedByIndex\":true,\"selectorSatisfiedByIndex\":true}"
            .replace(char::is_whitespace, ""),
        "the serialised plan must be byte-identical to what upstream records as planJson"
    );
}

/// Every case: normalized query, plan, and the plan's JSON round trip.
#[test]
fn normalized_queries_and_plans_match_upstream_oracle() {
    let vector = load();
    let lookup = schema_lookup();
    let cases = vector["cases"].as_array().expect("cases array");
    let mut checked = 0;
    let mut rejected = 0;
    let mut failures = Vec::new();

    for case in cases {
        let schema_name = case["schema"].as_str().expect("schema");
        let why = case["why"].as_str().unwrap_or_default();
        let schema = filled(schema_name, &lookup);
        let primary_key = dxdb_schema::primary_field_of_primary_key(&schema).expect("primary key");
        let schema_indexes = dxdb_schema::get_string_indexes(&schema).unwrap_or_default();

        // Some cases record that upstream *failed* rather than recording a result:
        // `normalizedQuery` is `null` and `expectedUpstreamFailure` is true, with a
        // `rejected` reason. They are asserted by
        // `schemas_upstream_cannot_plan_are_rejected_here`, so they are counted and
        // skipped here rather than compared against `null`.
        if case
            .get("normalizedQuery")
            .map(Value::is_null)
            .unwrap_or(true)
        {
            rejected += 1;
            continue;
        }

        let query = match MangoQuery::from_json(&case["query"]) {
            Ok(query) => query,
            Err(error) => {
                failures.push(format!(
                    "{schema_name} {why}: query does not parse: {error}"
                ));
                continue;
            }
        };
        let normalized = match normalize_mango_query(&query, &primary_key, &schema_indexes, false) {
            Ok(normalized) => normalized,
            Err(error) => {
                failures.push(format!(
                    "{schema_name} {why}: normalisation failed: {error}"
                ));
                continue;
            }
        };

        // 1. The normalized query, as a value with key order.
        let expected_normalized = &case["normalizedQuery"];
        if normalized.to_json() != *expected_normalized {
            failures.push(format!(
                "{schema_name} {why}: normalized query differs.\n  ours:     {}\n  upstream: {}",
                dxdb_json::to_js_json(&normalized.to_json()),
                dxdb_json::to_js_json(expected_normalized)
            ));
            continue;
        }

        // 2. The plan.
        let expected_plan_json = &case["plan"];
        let expected_index: Vec<String> = expected_plan_json["index"]
            .as_array()
            .expect("plan index")
            .iter()
            .map(|field| field.as_str().expect("field").to_string())
            .collect();
        let expected_plan = plan_from_json(expected_plan_json, &expected_index);
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
                dxdb_json::to_js_json(expected_plan_json)
            ));
            continue;
        }

        // 3. The plan's serialised form, exactly as upstream records it. This is
        // where a `20.0` from serde's float writer would show up as a divergence.
        let our_json = actual_plan.to_json_string();
        let expected_json = case["planJson"].as_str().expect("planJson");
        if our_json != expected_json {
            failures.push(format!(
                "{schema_name} {why}: planJson differs.\n  ours:     {our_json}\n  upstream: {expected_json}"
            ));
            continue;
        }

        // 4. And the round trip the vector asserts: `JSON.parse(JSON.stringify(plan))`
        // must equal `plan`. Comparing the *text* before and after a parse is the
        // right test, because a `Value` comparison would hide the difference between
        // `20` and `20.0` — which is the whole point of the assertion, since
        // `INDEX_MIN` was chosen as a number precisely so that serialising does not
        // turn it into `null`.
        if case["planRoundTripEqual"].as_bool() == Some(true) {
            let parsed: Value = serde_json::from_str(&our_json).expect("plan parses back");
            let re_serialised = plan_from_json(&parsed, &actual_plan.index).to_json_string();
            if re_serialised != our_json {
                failures.push(format!(
                    "{schema_name} {why}: plan does not survive a JSON round trip.\n  before: {our_json}\n  after:  {re_serialised}"
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
        cases.len(),
        failures.join("\n")
    );
    assert!(
        checked + rejected == cases.len(),
        "every case must be either checked ({checked}) or a recorded upstream failure ({rejected}); \
         total {}",
        cases.len()
    );
    assert!(
        checked >= 700,
        "expected at least 700 comparable cases, got {checked}"
    );
    assert!(
        rejected >= 30,
        "expected the recorded upstream failures to be present, got {rejected}"
    );
}

/// The schemas upstream cannot normalise must not be normalised here either.
///
/// Every recorded failure is `compoundIndexNoString`, whose declared index contains
/// the literal number `10` instead of a field path — so the very first thing
/// upstream does with the schema throws (`usePath.replace is not a function`).
/// Producing a plan for it would mean dxdb can run queries RxDB cannot express.
#[test]
fn schemas_upstream_cannot_plan_are_rejected_here() {
    let vector = load();
    let failures = vector["failures"].as_array().expect("failures array");
    assert!(
        !failures.is_empty(),
        "the family records 38 cases upstream failed; none are present"
    );
    let lookup = schema_lookup();
    let cases = vector["cases"].as_array().expect("cases array");

    for failure in failures {
        let schema_name = failure["schema"].as_str().expect("schema");
        let why = failure["why"].as_str().unwrap_or_default();
        let Some(schema_input) = lookup.get(schema_name) else {
            continue;
        };
        // The rejection can happen at fill time (the index names a non-field) or at
        // plan time (the field cannot be encoded). Either is a rejection; what must
        // not happen is a plan.
        let Ok(schema) = dxdb_schema::fill_with_default_settings(schema_input) else {
            continue;
        };
        let primary_key = dxdb_schema::primary_field_of_primary_key(&schema).expect("primary key");
        let schema_indexes = dxdb_schema::get_string_indexes(&schema).unwrap_or_default();
        // The same query shapes the failing cases used.
        let sample_query = cases
            .iter()
            .find(|case| case["schema"].as_str() == Some(schema_name))
            .map(|case| case["query"].clone())
            .unwrap_or_else(|| serde_json::json!({}));
        let query = MangoQuery::from_json(&sample_query).expect("query parses");
        let normalized = normalize_mango_query(&query, &primary_key, &schema_indexes, false);
        let normalized = match normalized {
            Ok(normalized) => normalized,
            Err(_) => continue,
        };
        assert!(
            get_query_plan(&schema, &normalized).is_err(),
            "{schema_name} {why}: upstream cannot plan this schema ({}) but dxdb produced a plan",
            failure["reason"].as_str().unwrap_or_default()
        );
    }
}

/// `orderings`: the sort comparator against a fixed document set.
#[test]
fn sort_orderings_match_upstream_oracle() {
    let vector = load();
    let orderings = vector["orderings"].as_array().expect("orderings array");
    assert!(!orderings.is_empty());

    // The document set the generator sorted. It is in the generator
    // (`xtask/vectors/families/normalised-queries.mjs:262`) and recorded here
    // because the vector stores only the resulting ids.
    let documents = fixture_documents();
    let lookup = schema_lookup();
    let schema = filled("human", &lookup);
    let primary_key = dxdb_schema::primary_field_of_primary_key(&schema).expect("primary key");
    let schema_indexes = dxdb_schema::get_string_indexes(&schema).unwrap_or_default();

    for ordering in orderings {
        let why = ordering["why"].as_str().expect("why");
        let sort = ordering["normalizedSort"].clone();
        let expected_ids: Vec<String> = ordering["sortedIds"]
            .as_array()
            .expect("sortedIds")
            .iter()
            .map(|id| id.as_str().expect("id").to_string())
            .collect();

        // The recorded `normalizedSort` is what the comparator must be built from;
        // feeding it back through a query keeps the path identical to upstream's
        // (`normalizeMangoQuery` then `getSortComparator`).
        let query = serde_json::json!({ "selector": {}, "sort": sort });
        let parsed = MangoQuery::from_json(&query).expect("query parses");
        let normalized = normalize_mango_query(&parsed, &primary_key, &schema_indexes, false)
            .expect("normalizes");
        // Upstream built the comparator from the *recorded* normalized sort, and
        // that is what the ids were produced with.
        assert_eq!(
            normalized.to_json()["sort"],
            sort,
            "{why}: re-normalising the recorded sort must be a no-op"
        );
        let comparator = get_sort_comparator(&normalized);
        let mut sorted = documents.clone();
        sorted.sort_by(|a, b| comparator.compare(a, b));
        let actual_ids: Vec<String> = sorted
            .iter()
            .map(|doc| doc["passportId"].as_str().expect("id").to_string())
            .collect();
        assert_eq!(actual_ids, expected_ids, "{why}: sort order");
    }
}

/// `matchers`: the selector matcher against the same document set.
#[test]
fn matchers_match_upstream_oracle() {
    let vector = load();
    let matchers = vector["matchers"].as_array().expect("matchers array");
    assert!(!matchers.is_empty());
    let documents = fixture_documents();

    for case in matchers {
        let why = case["why"].as_str().expect("why");
        let selector = &case["normalizedSelector"];
        let expected: Vec<String> = case["matchingIds"]
            .as_array()
            .expect("matchingIds")
            .iter()
            .map(|id| id.as_str().expect("id").to_string())
            .collect();
        let matcher = Matcher::new(selector)
            .unwrap_or_else(|error| panic!("{why}: selector does not compile: {error}"));
        let actual: Vec<String> = documents
            .iter()
            .filter(|doc| matcher.matches(doc))
            .map(|doc| doc["passportId"].as_str().expect("id").to_string())
            .collect();
        assert_eq!(actual, expected, "{why}: matching ids for {selector}");
    }
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
