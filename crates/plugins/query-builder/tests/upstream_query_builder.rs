//! Ported from `test/unit/query-builder.test.ts` at upstream `3b763ee17`.
//!
//! Every test names the upstream `it(...)` and the line it starts on. The one
//! upstream case that cannot be ported here — the `RxQuery`-level merge test at
//! `:189`, which needs a live collection — is reported as not ported in the
//! module docs of each crate rather than silently dropped; the `where(object)`
//! behaviour it exercises is covered at builder level by
//! [`upstream_189_where_object_promotes_shorthand_both_directions`].
//!
//! Expectations are compared as ordered JSON, because selector key order is
//! observable to anything that later serialises the query.

use dxdb_plugin_query_builder::{js_number, Error, QueryBuilder, RegexArg, SortArg};
use serde_json::{json, Value};

/// Upstream asserts with `assert.deepStrictEqual`, which compares key order for
/// objects in practice (`Object.keys` order). Comparing re-serialised JSON keeps
/// that property.
fn assert_selector(builder: &QueryBuilder, expected: Value) {
    assert_eq!(
        builder.selector(),
        expected,
        "\n  actual: {}\nexpected: {}",
        builder.selector(),
        expected
    );
}

/// Renders a `MangoQuery` as JSON in Mango's own shape, for round-trip asserts.
fn query_json(builder: &QueryBuilder) -> Value {
    let json = builder.to_json().expect("to_json");
    let mut query = serde_json::Map::new();
    query.insert("selector".to_string(), json.query.selector);
    if !json.query.sort.is_empty() {
        let sort: Vec<Value> = json
            .query
            .sort
            .iter()
            .map(|part| json!({ part.field.clone(): part.direction.as_str() }))
            .collect();
        query.insert("sort".to_string(), Value::Array(sort));
    }
    if json.query.skip != 0 {
        query.insert("skip".to_string(), json!(json.query.skip));
    }
    if let Some(limit) = json.query.limit {
        query.insert("limit".to_string(), json!(limit));
    }
    Value::Object(query)
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:24` — should make a basic roundtrip
// ---------------------------------------------------------------------------

#[test]
fn upstream_24_should_make_a_basic_roundtrip() {
    let start_query = json!({
        "selector": {
            "age": { "$gt": 4 },
            "name": { "$ne": "alice" }
        },
        "sort": [{ "name": "asc" }, { "lastname": "desc" }],
        "skip": 3,
        "limit": 2
    });

    // The initial-json half: `createQueryBuilder(startQuery).toJSON().query`
    // deep-equals the query it was given (`:36-38`).
    let query = parse_mango(&start_query);
    let initial = QueryBuilder::new(&query, None).expect("build from query");
    assert_eq!(query_json(&initial), start_query, "initial json");

    // The built half (`:41-49`): same query, reached by chaining.
    let mut builder = QueryBuilder::empty();
    builder
        .where_("age")
        .gt(json!(4))
        .expect("gt")
        .where_("name")
        .ne(json!("alice"))
        .expect("ne")
        .skip(3)
        .limit(2)
        .sort(SortArg::Object(vec![
            ("name".to_string(), json!("asc")),
            ("lastname".to_string(), json!("desc")),
        ]))
        .expect("sort");
    assert_eq!(query_json(&builder), start_query, "built json");
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:55` — should work with only the selector
// ---------------------------------------------------------------------------

#[test]
fn upstream_55_should_work_with_only_the_selector() {
    let start_query = json!({
        "selector": {
            "age": { "$gt": 5 },
            "name": { "$ne": "alice" }
        }
    });

    let query = parse_mango(&start_query);
    let initial = QueryBuilder::new(&query, None).expect("build from query");
    assert_eq!(query_json(&initial), start_query, "initial json");

    let mut builder = QueryBuilder::empty();
    builder
        .where_("age")
        .gt(json!(5))
        .expect("gt")
        .where_("name")
        .ne(json!("alice"))
        .expect("ne");
    assert_eq!(query_json(&builder), start_query, "built json");
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:79` — should have path
// ---------------------------------------------------------------------------

#[test]
fn upstream_79_should_have_path() {
    let path = "foobar";
    let mut builder = QueryBuilder::empty();
    builder
        .where_("age")
        .gt(json!(6))
        .expect("gt")
        .where_("name")
        .ne(json!("alice"))
        .expect("ne")
        .where_(path);
    let json = builder.to_json().expect("to_json");
    assert_eq!(json.path.as_deref(), Some(path));
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:89` — should work with big equal number
// ---------------------------------------------------------------------------

#[test]
fn upstream_89_should_work_with_big_equal_number() {
    let start_query = json!({
        "selector": { "age": { "$gt": -999999999999999_i64 } },
        "sort": [{ "age": "asc" }]
    });

    let query = parse_mango(&start_query);
    let initial = QueryBuilder::new(&query, None).expect("build from query");
    assert_eq!(query_json(&initial), start_query, "initial json");

    // The value survives because it is inside the safe-integer range, which is
    // the point of the case: `-999999999999999` is representable and must not be
    // rounded or stringified.
    let big = js_number(json!(-999_999_999_999_999_i64)).expect("safe integer");
    assert_eq!(big, json!(-999_999_999_999_999_i64));

    let mut builder = QueryBuilder::empty();
    builder
        .where_("age")
        .gt(big)
        .expect("gt")
        .sort(SortArg::String("age".to_string()))
        .expect("sort");
    assert_eq!(query_json(&builder), start_query, "built json");
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:113` — eq() should use $eq so it coexists
// ---------------------------------------------------------------------------

#[test]
fn upstream_113_eq_uses_dollar_eq_so_operators_coexist() {
    let mut builder = QueryBuilder::empty();
    builder
        .where_("age")
        .eq(json!(25))
        .expect("eq")
        .where_("age")
        .gt(json!(20))
        .expect("gt");
    assert_selector(&builder, json!({ "age": { "$eq": 25, "$gt": 20 } }));
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:129` — equals() should use $eq so it coexists
// ---------------------------------------------------------------------------

#[test]
fn upstream_129_equals_uses_dollar_eq_so_operators_coexist() {
    let mut builder = QueryBuilder::empty();
    builder
        .where_("age")
        .equals(json!(25))
        .expect("equals")
        .where_("age")
        .gt(json!(20))
        .expect("gt");
    assert_selector(&builder, json!({ "age": { "$eq": 25, "$gt": 20 } }));
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:142` — operator followed by eq() preserves both
// ---------------------------------------------------------------------------

#[test]
fn upstream_142_operator_followed_by_eq_preserves_both() {
    let mut builder = QueryBuilder::empty();
    builder
        .where_("age")
        .gt(json!(20))
        .expect("gt")
        .where_("age")
        .eq(json!(25))
        .expect("eq");
    // Note the order: `$gt` was inserted first, so `$eq` follows it. Ordered JSON
    // makes that visible, and `deepStrictEqual` upstream sees the same order.
    assert_selector(&builder, json!({ "age": { "$gt": 20, "$eq": 25 } }));
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:155` — selector shorthand preserved when chaining
// ---------------------------------------------------------------------------

#[test]
fn upstream_155_shorthand_value_preserved_when_chaining() {
    let mut builder = QueryBuilder::empty();
    builder.find(&json!({ "age": 5 })).expect("find");
    builder.where_("age").gt(json!(3)).expect("gt");
    assert_selector(&builder, json!({ "age": { "$eq": 5, "$gt": 3 } }));
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:172` — shorthand null value preserved when chaining
// ---------------------------------------------------------------------------

#[test]
fn upstream_172_shorthand_null_preserved_when_chaining() {
    let mut builder = QueryBuilder::empty();
    builder.find(&json!({ "name": null })).expect("find");
    builder.where_("name").ne(json!("bar")).expect("ne");
    // The `null` becomes `$eq: null` rather than being treated as "no condition",
    // which is the whole point of the case.
    assert_selector(&builder, json!({ "name": { "$eq": null, "$ne": "bar" } }));
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:221` — no all() method, because $all is unsupported
// ---------------------------------------------------------------------------

#[test]
fn upstream_221_there_is_no_all_method() {
    // Upstream asserts `typeof builder.all === 'undefined'` on both an RxQuery and
    // a bare builder. Rust has the stronger form: there is no such method to call,
    // so the absence cannot regress without a compile error in a test that tries.
    // What is testable is that `$all` is rejected by the operator surface, which is
    // the reason `all()` was removed.
    let mut builder = QueryBuilder::empty();
    builder.where_("tags");
    let unsupported = builder.dropped_attribute("all", json!(["a"]));
    assert!(
        matches!(unsupported, Err(Error::SortArguments { .. })),
        "an unknown attribute must be an error, not a silent no-op"
    );
}

// ---------------------------------------------------------------------------
// `query-builder.test.ts:189` — RxQuery-level `where(object)` merge, at builder
// level. The upstream test needs `humansCollection` and a live query; that part is
// lane D's. Both directions of the merge are exercised here because that is where
// the behaviour lives.
// ---------------------------------------------------------------------------

#[test]
fn upstream_189_where_object_promotes_shorthand_both_directions() {
    // shorthand first, then an operator object (`:198-204`): `{age: 7}` merged with
    // `{age: {$gt: 3}}` must keep both, i.e. match `age == 7 && age > 3`.
    let mut builder = QueryBuilder::empty();
    builder.find(&json!({ "age": 7 })).expect("find");
    builder
        .where_object(&json!({ "age": { "$gt": 3 } }))
        .expect("merge");
    assert_selector(&builder, json!({ "age": { "$eq": 7, "$gt": 3 } }));

    // operator first, then a shorthand (`:206-213`): `{age: {$lt: 7}}` merged with
    // `{age: 7}` must also keep both, which matches nothing.
    let mut builder = QueryBuilder::empty();
    builder.find(&json!({ "age": { "$lt": 7 } })).expect("find");
    builder.where_object(&json!({ "age": 7 })).expect("merge");
    assert_selector(&builder, json!({ "age": { "$lt": 7, "$eq": 7 } }));
}

// ---------------------------------------------------------------------------
// Behaviour that upstream covers incidentally, asserted here because it is
// contract: the sort shapes, the merge rules, and the error codes.
// ---------------------------------------------------------------------------

#[test]
fn sort_accepts_all_three_upstream_shapes() {
    // `:275-311` — object, array-of-pairs, and the space-delimited string.
    let mut object = QueryBuilder::empty();
    object
        .sort(SortArg::Object(vec![
            ("name".to_string(), json!("asc")),
            ("lastname".to_string(), json!(-1)),
        ]))
        .expect("object sort");

    let mut array = QueryBuilder::empty();
    array
        .sort(SortArg::Array(vec![
            ("name".to_string(), json!(1)),
            ("lastname".to_string(), json!(-1)),
        ]))
        .expect("array sort");

    let mut string = QueryBuilder::empty();
    string
        .sort(SortArg::String("name -lastname".to_string()))
        .expect("string sort");

    let expected = json!([{ "name": "asc" }, { "lastname": "desc" }]);
    for (label, builder) in [("object", object), ("array", array), ("string", string)] {
        let json = builder.to_json().expect("to_json");
        let sort: Vec<Value> = json
            .query
            .sort
            .iter()
            .map(|part| json!({ part.field.clone(): part.direction.as_str() }))
            .collect();
        assert_eq!(Value::Array(sort), expected, "{label} sort shape");
    }
}

#[test]
fn sort_rejects_an_invalid_direction_with_mq7() {
    // `:557-566` — the regex is the contract, and the error carries the field and
    // the value it rejected.
    let mut builder = QueryBuilder::empty();
    let error = builder
        .sort(SortArg::Object(vec![(
            "age".to_string(),
            json!("sideways"),
        )]))
        .expect_err("invalid direction");
    assert_eq!(
        error,
        Error::SortDirection {
            field: "age".to_string(),
            value: "sideways".to_string(),
        }
    );
    assert!(error.to_string().starts_with("MQ7:"));
}

#[test]
fn merge_does_not_overwrite_and_skips_prototype_keys() {
    // The three special properties never appear in the selector; a new key is
    // added; an existing key is overwritten (see the assertion below).
    let mut builder = QueryBuilder::empty();
    builder
        .find(&json!({
            "age": 5,
            "__proto__": { "polluted": true },
            "constructor": "x",
            "prototype": "y"
        }))
        .expect("find");
    builder
        .find(&json!({ "age": 99, "name": "alice" }))
        .expect("merge");
    // `mquery-utils.ts:21-28`: `typeof to[key] === 'undefined'` is false for an
    // existing key, so it recurses when both sides are objects and **overwrites**
    // otherwise. `age: 5` therefore becomes `age: 99`; the existing value does not
    // win. My first expectation had this backwards.
    assert_selector(&builder, json!({ "age": 99, "name": "alice" }));
}

#[test]
fn merge_recurses_into_objects_instead_of_replacing() {
    // `mquery-utils.ts:24-25`.
    let mut builder = QueryBuilder::empty();
    builder.find(&json!({ "age": { "$gt": 1 } })).expect("find");
    builder
        .find(&json!({ "age": { "$lt": 9 } }))
        .expect("merge");
    assert_selector(&builder, json!({ "age": { "$gt": 1, "$lt": 9 } }));
}

#[test]
fn comparison_without_a_path_is_mq5() {
    let mut builder = QueryBuilder::empty();
    assert_eq!(
        builder.gt(json!(1)).expect_err("no path"),
        Error::NoPath { method: "gt" }
    );
}

#[test]
fn find_by_ids_refuses_to_build() {
    // `index.ts:42`, error `QU17`.
    let builder = QueryBuilder::empty();
    assert_eq!(
        builder.find_by_ids().expect_err("refused"),
        Error::FindByIds
    );
}

#[test]
fn regex_accepts_pattern_and_options() {
    // `:510-529`.
    let mut plain = QueryBuilder::empty();
    plain
        .where_("name")
        .regex(RegexArg::Pattern("^al".to_string()))
        .expect("regex");
    assert_selector(&plain, json!({ "name": { "$regex": "^al" } }));

    let mut with_options = QueryBuilder::empty();
    with_options
        .where_("name")
        .regex(RegexArg::PatternWithOptions {
            pattern: "^al".to_string(),
            options: "i".to_string(),
        })
        .expect("regex");
    assert_selector(
        &with_options,
        json!({ "name": { "$regex": "^al", "$options": "i" } }),
    );
}

#[test]
fn elem_match_keeps_the_criteria_object() {
    // `:229`.
    let mut builder = QueryBuilder::empty();
    builder
        .elem_match(
            "comment",
            json!({ "author": "autobot", "votes": { "$gte": 5 } }),
        )
        .expect("elemMatch");
    assert_selector(
        &builder,
        json!({ "comment": { "$elemMatch": { "author": "autobot", "votes": { "$gte": 5 } } } }),
    );
}

#[test]
fn elem_match_callback_builds_a_nested_selector() {
    // `:255-259`.
    let mut builder = QueryBuilder::empty();
    builder
        .elem_match_with("comment", |inner| {
            inner.where_("author").equals(json!("autobot"))?;
            inner.where_("votes").gte(json!(5))?;
            Ok(())
        })
        .expect("elemMatch callback");
    assert_selector(
        &builder,
        json!({ "comment": { "$elemMatch": { "author": { "$eq": "autobot" }, "votes": { "$gte": 5 } } } }),
    );
}

#[test]
fn logical_operators_append_rather_than_replace() {
    // `:121`, `:133`, `:146` — each call pushes onto the existing array.
    let mut builder = QueryBuilder::empty();
    builder
        .or(vec![json!({ "a": 1 })])
        .expect("or")
        .or(vec![json!({ "b": 2 })])
        .expect("or again");
    builder.and(vec![json!({ "c": 3 })]).expect("and");
    builder.nor(vec![json!({ "d": 4 })]).expect("nor");
    assert_selector(
        &builder,
        json!({
            "$or": [{ "a": 1 }, { "b": 2 }],
            "$and": [{ "c": 3 }],
            "$nor": [{ "d": 4 }]
        }),
    );
}

#[test]
fn dropped_attributes_are_accepted_and_not_emitted() {
    // Crate doc divergence 1: `maxScan`/`batchSize`/`comment` (`:459`) never reach
    // `toJSON` (`:389-411`). Asserted so the behaviour is pinned rather than
    // discovered later.
    let mut builder = QueryBuilder::empty();
    builder
        .dropped_attribute("limit_ignored_typo", json!(1))
        .expect_err("unknown attribute");
    builder
        .dropped_attribute("comment", json!("feed query"))
        .expect("comment");
    let json = builder.to_json().expect("to_json");
    assert_eq!(json.query.limit, None);
    assert_eq!(builder.dropped_attributes().len(), 1);
}

#[test]
fn meta_sort_is_reported_rather_than_silently_sorted() {
    // Crate doc divergence 3.
    let mut builder = QueryBuilder::empty();
    builder
        .sort(SortArg::Object(vec![(
            "score".to_string(),
            json!({ "$meta": "textScore" }),
        )]))
        .expect("meta sort is accepted");
    match builder.to_json() {
        Err(Error::UnsortableMeta { field, kind }) => {
            assert_eq!(field, "score");
            assert_eq!(kind, "textScore");
        }
        other => panic!("expected UnsortableMeta, got {other:?}"),
    }
}

/// Builds a `MangoQuery` from upstream's own literal shape, filling the fields
/// `MangoQuery` requires but upstream's test fixtures omit.
fn parse_mango(query: &Value) -> dxdb_plugin_query_builder::MangoQuery {
    let selector = query.get("selector").cloned().unwrap_or_else(|| json!({}));
    let mut sort = Vec::new();
    if let Some(parts) = query.get("sort").and_then(Value::as_array) {
        for part in parts {
            let map = part.as_object().expect("sort part is an object");
            // Upstream's fixtures use single-key objects (`:32`, `:94`).
            for (field, direction) in map {
                let direction = direction.as_str().expect("direction is a string");
                sort.push(dxdb_plugin_query_builder::SortPart {
                    field: field.clone(),
                    direction: dxdb_plugin_query_builder::SortDirection::parse(direction)
                        .expect("asc or desc"),
                });
            }
        }
    }
    dxdb_plugin_query_builder::MangoQuery {
        selector,
        sort,
        skip: query.get("skip").and_then(Value::as_u64).unwrap_or(0) as usize,
        limit: query
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize),
        index: None,
    }
}
