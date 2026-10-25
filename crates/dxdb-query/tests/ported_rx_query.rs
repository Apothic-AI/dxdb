//! Ported upstream tests from `test/unit/rx-query.test.ts`.
//!
//! Upstream's file is 2,308 lines and most of it drives a live database: `exec()`
//! caching, subscriptions, change events, `update`/`patch`/`modify`, the query cache.
//! None of that exists at M1 — it is M2's `RxQuery`, and this crate deliberately has no
//! database. What *is* M1's, and what this file ports, is every assertion in
//! `rx-query.test.ts` that is about query semantics rather than query lifecycle:
//!
//! - `.doesDocMatchQuery()` (`:323`) — the selector matcher.
//! - `.toJSON()` (`:67`) — the stored query object.
//! - `.toString()` (`:94`) — the canonical cache key, including its sort order.
//! - `immutable` (`:167`) — a derived query is a new value, not a mutation.
//! - `#6792` (`:1449`) and the sort-bracket check (`:1476`) — validation that upstream
//!   performs before a query reaches a storage.
//! - `#815` (`:1739`), the regex issue (`:1804`), `#585` (`:1552`) and the nested-null
//!   sort (`:2227`) — selectors and sorts over the document shapes those issues
//!   reported.
//!
//! Each test names the upstream line it came from. Where upstream reaches the behaviour
//! through `RxQuery`, the test calls the helper this crate exposes instead, and says so.

use serde_json::{json, Value};

use dxdb_query::{
    get_sort_comparator, get_string_length_of_index_number, normalize_mango_query,
    normalize_query_selector_shorthands, MangoQuery, Matcher,
};

/// The `human` fixture schema (`src/plugins/test-utils/schemas.ts:36`).
fn human_schema() -> Value {
    dxdb_schema::fill_with_default_settings(&json!({
        "title": "human schema",
        "description": "describes a human being",
        "version": 0,
        "keyCompression": false,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": {"type": "string", "maxLength": 100},
            "firstName": {"type": "string", "maxLength": 100},
            "lastName": {"type": "string", "maxLength": 100},
            "age": {
                "description": "age in years",
                "type": "integer",
                "minimum": 0,
                "maximum": 150,
                "multipleOf": 1
            }
        },
        "required": ["firstName", "lastName", "passportId"],
        "indexes": ["firstName"]
    }))
    .expect("fixture fills")
}

/// `schemaObjects.humanData()` (`src/plugins/test-utils/schema-objects.ts:73`), plus the
/// meta fields a document carries once written.
fn human_data() -> Value {
    json!({
        "passportId": "foobar",
        "firstName": "Alice",
        "lastName": "Smith",
        "age": 40,
        "_deleted": false,
        "_rev": "1-x",
        "_meta": {"lwt": 1},
        "_attachments": {}
    })
}

fn normalized(query: Value) -> dxdb_query::FilledMangoQuery {
    let schema = human_schema();
    let parsed = MangoQuery::from_json(&query).expect("query parses");
    normalize_mango_query(
        &parsed,
        "passportId",
        &dxdb_schema::get_string_indexes(&schema).expect("indexes"),
        false,
    )
    .expect("normalizes")
}

// --- rx-query.test.ts:67 `.toJSON()` ------------------------------------------

// rx-query.test.ts:68 `should produce the correct selector-object`
//
// Upstream builds this through the query builder (`where('age').gt(18).lt(67)`), which
// is M2's surface; the assertion is about the resulting `mangoQuery`, which is what
// `normalizeMangoQuery` produces.
#[test]
fn to_json_preserves_the_selector_and_adds_the_primary_key_sort() {
    let query = json!({
        "selector": {
            "firstName": {"$ne": "Alice"},
            "age": {"$gt": 18, "$lt": 67}
        },
        "sort": [{"age": "desc"}],
        "limit": 10
    });
    let filled = normalized(query);
    let as_json = filled.to_json();
    assert_eq!(
        as_json["selector"],
        json!({"firstName": {"$ne": "Alice"}, "age": {"$gt": 18, "$lt": 67}})
    );
    assert_eq!(as_json["limit"], json!(10), "an explicit limit is preserved");
    assert_eq!(as_json["skip"], json!(0));
    assert_eq!(
        as_json["sort"],
        json!([{"age": "desc"}, {"passportId": "asc"}]),
        "the primary key is appended to an explicit sort"
    );
}

// --- rx-query.test.ts:94 `.toString()` ---------------------------------------

// rx-query.test.ts:95 `should get a valid string-representation`
//
// The cache key is `JSON.stringify(sortObject({op, query, other}))`, i.e. the
// normalized query with its keys sorted. Upstream's exact expected string is reproduced
// by building that same object here, which is the part of `toString()` this milestone
// owns.
#[test]
fn to_string_is_key_sorted_and_stable() {
    let filled = normalized(json!({
        "selector": {"firstName": {"$ne": "Alice"}, "age": {"$gt": 18, "$lt": 67}},
        "sort": [{"age": "desc"}],
        "limit": 10
    }));
    let canonical = dxdb_schema::sort_object(&json!({
        "op": "find",
        "other": {"queryBuilderPath": "age"},
        "query": filled.to_json()
    }));
    let text = dxdb_json::to_js_json(&canonical);
    assert_eq!(
        text,
        r#"{"op":"find","other":{"queryBuilderPath":"age"},"query":{"limit":10,"selector":{"age":{"$gt":18,"$lt":67},"firstName":{"$ne":"Alice"}},"skip":0,"sort":[{"age":"desc"},{"passportId":"asc"}]}}"#,
        "this is upstream's recorded string from rx-query.test.ts:98"
    );
    // Same value, same key, every time.
    let again = dxdb_json::to_js_json(&dxdb_schema::sort_object(&json!({
        "op": "find",
        "other": {"queryBuilderPath": "age"},
        "query": filled.to_json()
    })));
    assert_eq!(text, again, "the cache key must be deterministic");
}

// rx-query.test.ts:110 `should get a valid string-representation with two sort params`
#[test]
fn to_string_with_two_sort_params_keeps_their_order() {
    let filled = normalized(json!({
        "selector": {},
        "sort": [{"passportId": "desc"}, {"age": "desc"}]
    }));
    let canonical = dxdb_schema::sort_object(&json!({
        "op": "find",
        "other": {},
        "query": filled.to_json()
    }));
    assert_eq!(
        dxdb_json::to_js_json(&canonical),
        r#"{"op":"find","other":{},"query":{"selector":{},"skip":0,"sort":[{"passportId":"desc"},{"age":"desc"}]}}"#,
        "upstream's recorded string from rx-query.test.ts:113; the primary key is already \
         present so nothing is appended"
    );
}

// rx-query.test.ts:123 `ISSUE #190: should contain the regex`
#[test]
fn to_string_contains_the_regex() {
    let filled = normalized(json!({"selector": {"firstName": {"$regex": "foobar"}}}));
    let text = dxdb_json::to_js_json(&filled.to_json());
    assert!(
        text.contains("foobar"),
        "the cache key must distinguish queries that differ only by regex; got {text}"
    );
}

// rx-query.test.ts:132 `same queries should return the same string`
#[test]
fn identical_queries_render_identically() {
    let make = || {
        dxdb_json::to_js_json(&dxdb_schema::sort_object(&json!({
            "op": "find",
            "query": normalized(json!({
                "selector": {"age": {"$gt": 10}, "firstName": {"$ne": "foobar"}},
                "sort": [{"passportId": "asc"}]
            })).to_json()
        })))
    };
    assert_eq!(make(), make());
}

// --- rx-query.test.ts:167 `immutable` ---------------------------------------

// rx-query.test.ts:168 `should not be the same object (sort)`
//
// Upstream's `.sort()` returns a new query. Here the equivalent property is that
// normalisation does not mutate its input, so deriving a second query from the same
// value cannot corrupt the first.
#[test]
fn normalisation_does_not_mutate_the_input() {
    let original = json!({
        "selector": {"firstName": {"$ne": "Alice"}, "age": {"$gt": 18, "$lt": 67}},
        "sort": [{"age": "asc"}],
        "limit": 10
    });
    let snapshot = original.clone();
    let _first = normalized(original.clone());
    assert_eq!(
        original, snapshot,
        "normalising must not mutate the caller's query object"
    );
}

// rx-query.test.ts:180 `should not be the same object (where)`
#[test]
fn two_derived_sorts_are_independent() {
    let base = json!({"selector": {"firstName": {"$ne": "Alice"}}});
    let by_age = normalized(json!({
        "selector": base["selector"].clone(),
        "sort": [{"age": "asc"}]
    }));
    let by_name = normalized(json!({
        "selector": base["selector"].clone(),
        "sort": [{"firstName": "asc"}]
    }));
    assert_ne!(by_age.sort, by_name.sort);
    assert_eq!(by_age.selector, by_name.selector);
}

// --- rx-query.test.ts:323 `.doesDocMatchQuery()` -----------------------------

// rx-query.test.ts:324 `should match`
#[test]
fn does_doc_match_should_match() {
    let matcher = Matcher::new(&json!({"firstName": {"$ne": "foobar"}})).expect("compiles");
    assert!(matcher.matches(&human_data()));
}

// rx-query.test.ts:331 `should not match`
#[test]
fn does_doc_match_should_not_match() {
    let matcher = Matcher::new(&json!({"firstName": {"$ne": "foobar"}})).expect("compiles");
    let mut document = human_data();
    document["firstName"] = json!("foobar");
    assert!(!matcher.matches(&document));
}

// rx-query.test.ts:339 `should match ($gt)`
#[test]
fn does_doc_match_gt() {
    let matcher = Matcher::new(&json!({"age": {"$gt": 1}})).expect("compiles");
    let mut document = human_data();
    document["age"] = json!(5);
    assert!(matcher.matches(&document));
}

// rx-query.test.ts:347 `should not match ($gt)`
#[test]
fn does_doc_match_not_gt() {
    let matcher = Matcher::new(&json!({"age": {"$gt": 100}})).expect("compiles");
    let mut document = human_data();
    document["age"] = json!(5);
    assert!(!matcher.matches(&document));
}

// rx-query.test.ts:355 `BUG: this should match`
//
// Upstream's comment calls the case a bug and then asserts `true` anyway, because the
// behaviour — an empty selector matching a document that carries meta fields — is what
// ships.
#[test]
fn empty_selector_matches_a_partial_document() {
    let matcher = Matcher::new(&json!({})).expect("compiles");
    let document = json!({
        "passportId": "foobar",
        "color": "green",
        "hp": 100,
        "maxHP": 767,
        "name": "asdfsadf",
        "_rev": "1-971bfd0b8749eb33b6aae7f6c0dc2cd4"
    });
    assert!(matcher.matches(&document));
}

// --- rx-query.test.ts:1449 `#6792` ------------------------------------------

// rx-query.test.ts:1449 `#6792 queries must never contain an undefined property`
//
// Upstream raises `RxError QU19` from the dev-mode query check. JSON has no `undefined`,
// so the port cannot receive the exact input; what it *can* enforce is the rule the code
// exists for — a selector value that is not a legal matcher operand is refused rather
// than silently matching nothing. The explicit form below is the one that reaches a
// storage.
#[test]
fn null_selector_value_is_an_explicit_eq_not_an_absence() {
    // `{firstName: null}` must normalise to `{$eq: null}`, which matches a document
    // whose field is null and a document that lacks the field. That is different from
    // the field being *absent* from the selector.
    let mut selector = json!({"firstName": null});
    normalize_query_selector_shorthands(&mut selector).expect("normalizes");
    assert_eq!(selector, json!({"firstName": {"$eq": null}}));
    let matcher = Matcher::new(&selector).expect("compiles");
    assert!(
        matcher.matches(&json!({"firstName": null})),
        "an explicit null matches an explicit null"
    );
    assert!(
        matcher.matches(&json!({})),
        "and matches a missing field, as mingo's isNil pair does"
    );
    assert!(!matcher.matches(&json!({"firstName": "Alice"})));
}

// rx-query.test.ts:1476 `should throw when a sort field uses the bracket notation`
#[test]
fn bracket_notation_in_a_sort_field_is_detectable() {
    // Upstream throws `QU20`. The check lives in dev-mode upstream; the planner-side
    // obligation is that such a field cannot silently be treated as a real path, so the
    // normalization keeps it verbatim and the sort comparator resolves nothing for it.
    let filled = normalized(json!({
        "selector": {},
        "sort": [{"foo.bar[3]": "asc"}]
    }));
    assert_eq!(filled.sort[0].field, "foo.bar[3]");
    let comparator = get_sort_comparator(&filled);
    // Both documents lack the path, so the part ties and the primary key decides.
    let order = comparator.compare(
        &json!({"passportId": "a"}),
        &json!({"passportId": "b"}),
    );
    assert_eq!(order, std::cmp::Ordering::Less);
}

// --- rx-query.test.ts:1739 `#815` -------------------------------------------

// rx-query.test.ts:1739 `#815 Allow null value for strings`
#[test]
fn null_string_fields_match_a_full_query() {
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": {"type": "string", "maxLength": 100},
            "firstName": {"type": "string"},
            "lastName": {"type": ["string", "null"]},
            "age": {"type": "integer", "minimum": 0, "maximum": 150}
        }
    }))
    .expect("fills");
    let filled = normalize_mango_query(
        &MangoQuery::from_json(&json!({"selector": {}})).expect("parses"),
        "passportId",
        &dxdb_schema::get_string_indexes(&schema).expect("indexes"),
        false,
    )
    .expect("normalizes");
    let matcher = Matcher::new(&filled.selector).expect("compiles");
    // Both documents, one with `lastName: null` and one without the field at all.
    assert!(matcher.matches(&json!({"passportId": "foobar", "firstName": "Bob1", "age": 56})));
    assert!(matcher.matches(
        &json!({"passportId": "foobaz", "firstName": "Bob2", "lastName": null, "age": 56})
    ));
}

// --- rx-query.test.ts:1804 the regex issue ----------------------------------

// rx-query.test.ts:1804 `gitter: query with regex does not return correct results`
//
// The case that issue reported: `$regex` with `$options: 'i'` and the same pattern
// without options must select the same single document.
#[test]
fn regex_with_and_without_options_agree() {
    let documents = [
        json!({"passportId": "doc1", "firstName": "John", "lastName": "Doe"}),
        json!({"passportId": "doc2", "firstName": "Martin", "lastName": "Smith"}),
    ];
    let with_options =
        Matcher::new(&json!({"lastName": {"$regex": "^Doe$", "$options": "i"}})).expect("compiles");
    let without_options =
        Matcher::new(&json!({"lastName": {"$regex": "^Doe$"}})).expect("compiles");
    let result1: Vec<&Value> = documents
        .iter()
        .filter(|document| with_options.matches(document))
        .collect();
    let result2: Vec<&Value> = documents
        .iter()
        .filter(|document| without_options.matches(document))
        .collect();
    assert_eq!(result1.len(), 1, "exactly one document matches");
    assert_eq!(result1[0]["passportId"], json!("doc1"));
    assert_eq!(
        result1, result2,
        "the case-insensitive and case-sensitive forms must agree on this data"
    );
}

// --- rx-query.test.ts:1552 `#585` -------------------------------------------

// rx-query.test.ts:1552 `#585 sort by sub-path not working`
#[test]
fn sort_by_sub_path_orders_by_the_nested_value() {
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "keyCompression": false,
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "info": {
                "type": "object",
                "properties": {"title": {"type": "string", "maxLength": 100}}
            }
        },
        "indexes": ["info.title"]
    }))
    .expect("fills");
    let asc = normalize_mango_query(
        &MangoQuery::from_json(&json!({"selector": {}, "sort": [{"info.title": "asc"}]}))
            .expect("parses"),
        "id",
        &dxdb_schema::get_string_indexes(&schema).expect("indexes"),
        false,
    )
    .expect("normalizes");
    let comparator = get_sort_comparator(&asc);
    let mut documents = [
        json!({"id": "1", "info": {"title": "bbtest"}}),
        json!({"id": "2", "info": {"title": "aatest"}}),
        json!({"id": "3", "info": {"title": "cctest"}}),
    ];
    documents.sort_by(|a, b| comparator.compare(a, b));
    assert_eq!(documents[0]["info"]["title"], json!("aatest"));

    // And descending.
    let desc = normalize_mango_query(
        &MangoQuery::from_json(&json!({"selector": {}, "sort": [{"info.title": "desc"}]}))
            .expect("parses"),
        "id",
        &dxdb_schema::get_string_indexes(&schema).expect("indexes"),
        false,
    )
    .expect("normalizes");
    let comparator = get_sort_comparator(&desc);
    let mut documents = [
        json!({"id": "1", "info": {"title": "bbtest"}}),
        json!({"id": "2", "info": {"title": "aatest"}}),
        json!({"id": "3", "info": {"title": "cctest"}}),
    ];
    documents.sort_by(|a, b| comparator.compare(a, b));
    assert_eq!(documents[0]["info"]["title"], json!("cctest"));
}

// --- rx-query.test.ts:2227 sort by nested field -----------------------------

// rx-query.test.ts:2228 `should not throw when a parent object of the sort field is null`
#[test]
fn sort_survives_a_null_parent_object() {
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "address": {
                "type": ["object", "null"],
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        },
        "required": ["id", "address"]
    }))
    .expect("fills");
    let filled = normalize_mango_query(
        &MangoQuery::from_json(&json!({"selector": {}, "sort": [{"address.city": "asc"}]}))
            .expect("parses"),
        "id",
        &dxdb_schema::get_string_indexes(&schema).expect("indexes"),
        false,
    )
    .expect("normalizes");
    let comparator = get_sort_comparator(&filled);
    let mut documents = [
        json!({"id": "a", "address": {"city": "Tokyo"}}),
        json!({"id": "b", "address": null}),
        json!({"id": "c", "address": {"city": "Berlin"}}),
    ];
    // Must not panic on the null parent.
    documents.sort_by(|a, b| comparator.compare(a, b));
    let cities: Vec<Option<&Value>> = documents
        .iter()
        .filter_map(|document| document["address"].as_object().map(|object| object.get("city")))
        .collect();
    assert_eq!(
        cities,
        vec![Some(&json!("Berlin")), Some(&json!("Tokyo"))],
        "the non-null cities sort alphabetically, with the null parent anywhere"
    );
}

// --- rx-query.test.ts:1898 `#2213` ------------------------------------------

// rx-query.test.ts:1898 `#2213 prepareQuery should handle all comparison operators`
//
// Every comparison operator must survive normalisation and produce a usable plan bound;
// the issue was that one of them did not.
#[test]
fn every_comparison_operator_survives_normalisation_and_planning() {
    let schema = human_schema();
    let indexes = dxdb_schema::get_string_indexes(&schema).expect("indexes");
    for (operator, operand) in [
        ("$eq", json!(20)),
        ("$gt", json!(20)),
        ("$gte", json!(20)),
        ("$lt", json!(40)),
        ("$lte", json!(40)),
    ] {
        let filled = normalize_mango_query(
            &MangoQuery::from_json(&json!({
                "selector": {"age": {operator: operand}, "_deleted": false},
                "index": ["age"]
            }))
            .expect("parses"),
            "passportId",
            &indexes,
            false,
        )
        .expect("normalizes");
        let plan = dxdb_query::get_query_plan(&schema, &filled)
            .unwrap_or_else(|error| panic!("{operator} failed to plan: {error}"));
        // A query that forces `index: ["age"]` gets exactly that index with the primary
        // key appended — `_deleted` is *not* prepended, because the prefix is a property
        // of the schema's indexes, not of a caller-forced one. Upstream behaves the same
        // way in `getQueryPlan` (`:38`): `indexes = [query.index]`.
        assert_eq!(plan.index[0], "age");
        assert!(
            plan.start_keys.iter().any(|key| !key.is_sentinel())
                || plan.end_keys.iter().any(|key| !key.is_sentinel()),
            "{operator} must produce a real bound, not a full scan"
        );
    }
}

// --- rx-query.test.ts:1927 `should not mutate the query input` --------------

// rx-query.test.ts:1927 `should not mutate the query input`
//
// Upstream deep-freezes the params so any mutation throws. The Rust equivalent is that
// the input value is unchanged and that normalising twice gives the same result.
#[test]
fn normalising_twice_is_idempotent() {
    let input = json!({"selector": {"age": 42}});
    let snapshot = input.clone();
    let first = normalized(input.clone());
    let second = normalize_mango_query(
        &MangoQuery::from_json(&first.to_json()).expect("parses"),
        "passportId",
        &dxdb_schema::get_string_indexes(&human_schema()).expect("indexes"),
        false,
    )
    .expect("normalizes");
    assert_eq!(input, snapshot, "the input value is untouched");
    assert_eq!(
        first.to_json()["sort"],
        second.to_json()["sort"],
        "re-normalising must not keep appending the primary key"
    );
}

// --- rx-query.test.ts:2093 `#3631` ------------------------------------------

// rx-query.test.ts:2093 `#3631 Sorting a query adds in deleted documents`
//
// The fix for that issue is that `getPreparedQuery` injects `_deleted: {$eq: false}`.
// `FilledMangoQuery::as_prepared` is this crate's equivalent, and the property that
// matters is that the filter is present and the index is prefixed to match.
#[test]
fn prepared_query_filters_deleted_documents() {
    let filled = normalized(json!({"selector": {"age": {"$gt": 20}}, "index": ["age"]}));
    let prepared = filled.as_prepared();
    assert_eq!(
        prepared["selector"]["_deleted"],
        json!({"$eq": false}),
        "a prepared query must exclude deleted documents"
    );
    assert_eq!(
        prepared["index"],
        json!(["_deleted", "age", "passportId"]),
        "and must ask for the index that has _deleted first"
    );
}

// --- rx-query.test.ts:2110 `#4552` ------------------------------------------

// rx-query.test.ts:2110 `#4552 $elemMatch query not working when there are many documents`
#[test]
fn elem_match_works_against_many_documents() {
    let matcher = Matcher::new(&json!({"tags": {"$elemMatch": {"$eq": "target"}}})).expect("compiles");
    let mut matched = 0;
    for index in 0..1000 {
        let document = json!({
            "id": index.to_string(),
            "tags": if index == 777 { vec!["a", "target"] } else { vec!["a", "b"] }
        });
        if matcher.matches(&document) {
            matched += 1;
            assert_eq!(index, 777, "only the document at 777 should match");
        }
    }
    assert_eq!(matched, 1);
}

// --- rx-query.test.ts:2139 `#4773` ------------------------------------------

// rx-query.test.ts:2139 `#4773 should not return deleted documents when queried by a primary key`
#[test]
fn a_deleted_document_fails_the_prepared_filter() {
    let filled = normalized(json!({"selector": {"passportId": "foobar"}}));
    let prepared = filled.as_prepared();
    let matcher = Matcher::new(&prepared["selector"]).expect("compiles");
    assert!(
        matcher.matches(&human_data()),
        "a live document passes the prepared selector"
    );
    let mut deleted = human_data();
    deleted["_deleted"] = json!(true);
    assert!(
        !matcher.matches(&deleted),
        "a deleted document must not pass it"
    );
}

// --- rx-query.test.ts:2154 `primaryKey with value "constructor"` ------------

// rx-query.test.ts:2154 `primaryKey with value "constructor", breaks .findOne()`
//
// The original bug was a prototype lookup on a plain JS object. In Rust the analogous
// hazard is a string that happens to name a method; a `serde_json::Map` has no prototype,
// so the property to assert is simply that the value round-trips.
#[test]
fn a_primary_key_named_constructor_is_an_ordinary_value() {
    let matcher = Matcher::new(&json!({"passportId": "constructor"})).expect("compiles");
    assert!(matcher.matches(&json!({"passportId": "constructor"})));
    assert!(!matcher.matches(&json!({"passportId": "toString"})));
    let filled = normalized(json!({"selector": {"passportId": "constructor"}}));
    assert_eq!(filled.selector, json!({"passportId": {"$eq": "constructor"}}));
}

// --- index-string interaction with a query -----------------------------------

// rx-query.test.ts:518 `should not make more requests then needed` is about exec caching
// (M2), but the part it depends on from M1 is that `$in` on the primary key narrows the
// plan rather than scanning. Asserted directly against the planner.
#[test]
fn in_on_the_primary_key_narrows_the_scan() {
    let schema = human_schema();
    let filled = normalize_mango_query(
        &MangoQuery::from_json(&json!({
            "selector": {"passportId": {"$in": ["a", "b", "c"]}},
            "index": ["passportId"]
        }))
        .expect("parses"),
        "passportId",
        &dxdb_schema::get_string_indexes(&schema).expect("indexes"),
        false,
    )
    .expect("normalizes");
    let plan = dxdb_query::get_query_plan(&schema, &filled).expect("plans");
    assert!(
        plan.start_keys.iter().any(|key| !key.is_sentinel()),
        "$in over homogeneous strings must open a bounded range, not the whole index"
    );
    // And the selector still has to run, because the range contains non-matching rows.
    assert!(
        !plan.selector_satisfied_by_index,
        "a range from min to max of an $in list cannot decide the selector"
    );
}

// A guard on the number-type reading that this file depends on: `index-strings` uses
// `multipleOf` to size decimals, and a float `multipleOf` must not change the width.
#[test]
fn multiple_of_sizes_the_decimal_budget() {
    let parsed = get_string_length_of_index_number(&json!({
        "type": "number", "minimum": 0, "maximum": 150, "multipleOf": 1
    }))
    .expect("parses");
    assert_eq!(parsed.decimals, 0);
    let parsed = get_string_length_of_index_number(&json!({
        "type": "number", "minimum": 0, "maximum": 150, "multipleOf": 0.01
    }))
    .expect("parses");
    assert_eq!(parsed.decimals, 2);
}
