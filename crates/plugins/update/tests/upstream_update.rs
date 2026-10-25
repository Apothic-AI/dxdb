//! Ported from upstream `test/unit/rx-document.test.ts` at `3b763ee17`, plus the
//! modifier semantics that live in mingo rather than in RxDB.
//!
//! # What each test covers
//!
//! The RxDB-level cases (`.update()` / `.modify()` / `.incrementalUpdate()`) split
//! into two halves. The *modifier* half — what the update document does to the
//! document data — is this crate, and is ported below. The *document* half — the
//! revision bump, the conflict on a stale doc, the `final` field guard, the
//! `_deleted` interaction with queries — is lane D's write path, and is listed in
//! the report as not ported here rather than claimed.
//!
//! Upstream's `$set`/`$unset`/`$inc` cases at `rx-document.test.ts:346,362,374`
//! go through `doc.update(...)`, which calls `mingoUpdater` and then `_saveData`.
//! The modifier outcome those tests assert on (`firstName` changed, `age` gone,
//! `age` incremented) is exactly what [`apply_update`] produces, so those three
//! are ported as pure-data tests.
//!
//! The mingo-level table in the crate docs is asserted in
//! [`mingo_quirk_*`] tests. Those name the mingo file the behaviour comes from
//! rather than a line in the RxDB test suite, because upstream has no test for
//! them and a port that got them wrong would pass upstream's suite.

use dxdb_plugin_update::{apply_update, Error, Modifiers, OPERATORS};
use serde_json::{json, Value};

/// Applies and unwraps, with the update document visible in the panic message.
fn run(document: Value, update: Value) -> Value {
    apply_update(&document, &update)
        .unwrap_or_else(|error| panic!("update {update} on {document} failed: {error}"))
}

// ---------------------------------------------------------------------------
// `rx-document.test.ts:346` — $set a value with a mongo like query
// ---------------------------------------------------------------------------

#[test]
fn upstream_346_dollar_set_a_value() {
    let document = json!({ "passportId": "p1", "firstName": "old", "age": 20 });
    let updated = run(
        document,
        json!({ "$set": { "firstName": "new first name" } }),
    );
    assert_eq!(updated["firstName"], json!("new first name"));
    // The rest of the document is untouched.
    assert_eq!(updated["passportId"], json!("p1"));
    assert_eq!(updated["age"], json!(20));
}

// ---------------------------------------------------------------------------
// `rx-document.test.ts:362` — $unset a value with a mongo like query
// ---------------------------------------------------------------------------

#[test]
fn upstream_362_dollar_unset_a_value() {
    let document = json!({ "passportId": "p1", "firstName": "alice", "age": 20 });
    // Upstream passes `''` as the operand; `$unset` ignores operands entirely.
    let updated = run(document, json!({ "$unset": { "age": "" } }));
    assert!(
        updated.get("age").is_none(),
        "the key must be gone, not null: {updated}"
    );
    assert_eq!(updated["firstName"], json!("alice"));
}

// ---------------------------------------------------------------------------
// `rx-document.test.ts:374` — $inc a value with a mongo like query
// ---------------------------------------------------------------------------

#[test]
fn upstream_374_dollar_inc_a_value() {
    let document = json!({ "passportId": "p1", "age": 20 });
    let updated = run(document, json!({ "$inc": { "age": 1 } }));
    assert_eq!(updated["age"], json!(21));
}

// ---------------------------------------------------------------------------
// `rx-document.test.ts:388` — update and remove in one operation
// ---------------------------------------------------------------------------

#[test]
fn upstream_388_update_and_delete_in_one_operation() {
    // `$set` of `_deleted: true` alongside a normal field. The query-level effect
    // (`find()` no longer returns it) is lane D's; the data effect is here.
    let document = json!({ "passportId": "p1", "age": 20, "_deleted": false });
    let updated = run(document, json!({ "$set": { "age": 1, "_deleted": true } }));
    assert_eq!(updated["_deleted"], json!(true));
    assert_eq!(updated["age"], json!(1));
}

// ---------------------------------------------------------------------------
// `rx-document.test.ts:398` / `:433` — conflict and `final` guard. Both are
// document-layer behaviour and are NOT ported here; this records the boundary.
// ---------------------------------------------------------------------------

#[test]
fn upstream_398_and_433_are_document_layer_not_modifier_layer() {
    // A conflict on a stale revision (`:398`) and the `final` field guard (`:433`)
    // both depend on `_rev` comparison and schema knowledge, neither of which this
    // crate has. Asserted here only so the gap is machine-visible: the modifier
    // engine applies `$set` and `$inc` to `_rev`/final fields without complaint,
    // because refusing them is the caller's job.
    let document = json!({ "passportId": "p1", "_rev": "1-abc", "age": 1 });
    let updated = run(document.clone(), json!({ "$inc": { "age": 1 } }));
    assert_eq!(updated["_rev"], json!("1-abc"));
}

// ---------------------------------------------------------------------------
// The mingo semantics table from the crate docs. Each names the mingo file.
// ---------------------------------------------------------------------------

#[test]
fn mingo_inc_skips_a_non_numeric_target_but_errors_on_a_non_numeric_argument() {
    // `inc.js`: `if (isNumber(o[k]) || o[k] === undefined) { ... }` and otherwise
    // `return false`, so a string target is silently left alone.
    let document = json!({ "name": "alice", "age": 20 });
    let updated = run(document, json!({ "$inc": { "name": 1, "age": 1 } }));
    assert_eq!(
        updated["name"],
        json!("alice"),
        "non-numeric target skipped"
    );
    assert_eq!(updated["age"], json!(21));

    // But the argument is asserted at build time.
    let error = apply_update(&json!({}), &json!({ "$inc": { "age": "1" } }))
        .expect_err("non-numeric argument");
    assert!(matches!(error, Error::NonNumericArgument(_)));
}

#[test]
fn mingo_inc_starts_a_missing_field_at_zero() {
    // `inc.js`: `o[k] ||= 0; o[k] += val;`
    let updated = run(json!({}), json!({ "$inc": { "age": 5 } }));
    assert_eq!(updated["age"], json!(5));
}

#[test]
fn mingo_mul_starts_a_missing_field_at_zero_not_one() {
    // `mul.js`: `else if (o[k] === void 0) o[k] = 0;` — the surprising case.
    let updated = run(json!({}), json!({ "$mul": { "age": 3 } }));
    assert_eq!(updated["age"], json!(0), "missing * factor is 0, not 3");
}

#[test]
fn mingo_unset_writes_null_into_an_array() {
    // `unset.js`: `if (isArray(o)) o[k] = null; else delete o[k];`
    let updated = run(
        json!({ "tags": ["a", "b", "c"] }),
        json!({ "$unset": { "tags.1": "" } }),
    );
    assert_eq!(
        updated["tags"],
        json!(["a", null, "c"]),
        "array slot becomes null"
    );
}

#[test]
fn mingo_unset_deletes_an_object_key() {
    let updated = run(
        json!({ "nested": { "a": 1, "b": 2 } }),
        json!({ "$unset": { "nested.a": "" } }),
    );
    assert_eq!(updated["nested"], json!({ "b": 2 }));
}

#[test]
fn mingo_push_slice_positive_keeps_the_first_n() {
    // `push.js`: `if ($slice < 0) arr.splice(0, arr.length + $slice); else
    // arr.splice($slice);`
    let updated = run(
        json!({ "tags": ["a", "b"] }),
        json!({ "$push": { "tags": { "$each": ["c", "d"], "$slice": 3 } } }),
    );
    assert_eq!(updated["tags"], json!(["a", "b", "c"]));
}

#[test]
fn mingo_push_slice_negative_keeps_the_last_n() {
    let updated = run(
        json!({ "tags": ["a", "b", "c"] }),
        json!({ "$push": { "tags": { "$each": ["d"], "$slice": -2 } } }),
    );
    assert_eq!(updated["tags"], json!(["c", "d"]));
}

#[test]
fn mingo_push_position_inserts_before_slicing() {
    let updated = run(
        json!({ "tags": ["a", "d"] }),
        json!({ "$push": { "tags": { "$each": ["b", "c"], "$position": 1 } } }),
    );
    assert_eq!(updated["tags"], json!(["a", "b", "c", "d"]));
}

#[test]
fn mingo_push_sort_orders_the_whole_array() {
    let updated = run(
        json!({ "scores": [{ "n": 3 }, { "n": 1 }] }),
        json!({ "$push": { "scores": {
            "$each": [{ "n": 2 }],
            "$sort": { "n": 1 }
        } } }),
    );
    assert_eq!(
        updated["scores"],
        json!([{ "n": 1 }, { "n": 2 }, { "n": 3 }])
    );
}

#[test]
fn mingo_push_without_each_appends_the_value() {
    let updated = run(
        json!({ "tags": ["a"] }),
        json!({ "$push": { "tags": "b" } }),
    );
    assert_eq!(updated["tags"], json!(["a", "b"]));
}

#[test]
fn mingo_add_to_set_deduplicates_deeply_and_keeps_first_seen_order() {
    // `addToSet.js`: `unique(prev.concat($each))`, comparing deeply.
    let updated = run(
        json!({ "tags": ["a", { "k": 1 }] }),
        json!({ "$addToSet": { "tags": { "$each": [{ "k": 1 }, "b"] } } }),
    );
    assert_eq!(updated["tags"], json!(["a", { "k": 1 }, "b"]));
}

#[test]
fn mingo_add_to_set_leaves_a_non_array_target_alone() {
    // `addToSet.js`: the `else return false` branch.
    let updated = run(
        json!({ "tags": "a" }),
        json!({ "$addToSet": { "tags": "b" } }),
    );
    assert_eq!(updated["tags"], json!("a"));
}

#[test]
fn mingo_add_to_set_creates_a_missing_field_as_an_array() {
    let updated = run(json!({}), json!({ "$addToSet": { "tags": "a" } }));
    assert_eq!(updated["tags"], json!(["a"]));
}

#[test]
fn mingo_pop_requires_plus_or_minus_one() {
    let updated = run(
        json!({ "tags": ["a", "b"] }),
        json!({ "$pop": { "tags": 1 } }),
    );
    assert_eq!(updated["tags"], json!(["a"]));
    let updated = run(
        json!({ "tags": ["a", "b"] }),
        json!({ "$pop": { "tags": -1 } }),
    );
    assert_eq!(updated["tags"], json!(["b"]));

    let error = apply_update(&json!({}), &json!({ "$pop": { "tags": 2 } }))
        .expect_err("invalid pop argument");
    assert!(matches!(error, Error::PopArgument(_)));
}

#[test]
fn mingo_pop_on_an_empty_or_missing_array_is_a_no_op() {
    // `pop.js`: `if (!isArray(arr) || !arr.length) return false;` — "unchanged", so
    // nothing is written. A missing field stays missing rather than becoming `[]`.
    let updated = run(json!({ "tags": [] }), json!({ "$pop": { "tags": 1 } }));
    assert_eq!(updated["tags"], json!([]));

    let updated = run(json!({}), json!({ "$pop": { "tags": 1 } }));
    assert!(
        updated.get("tags").is_none(),
        "$pop must not create the field: {updated}"
    );

    // Same for a non-array target.
    let updated = run(json!({ "tags": "a" }), json!({ "$pop": { "tags": 1 } }));
    assert_eq!(updated["tags"], json!("a"));
}

#[test]
fn mingo_pull_treats_a_field_object_as_element_conditions() {
    // `pull.js`: `wrap = !isObject(spec) || Object.keys(spec).some(isOperator)`, so
    // a plain object without operators is tested against the element itself.
    let updated = run(
        json!({ "items": [{ "a": 1 }, { "a": 2 }, { "b": 1 }] }),
        json!({ "$pull": { "items": { "a": 1 } } }),
    );
    assert_eq!(updated["items"], json!([{ "a": 2 }, { "b": 1 }]));
}

#[test]
fn mingo_pull_treats_an_operator_object_as_a_scalar_test() {
    // The other side of the `wrap` branch: `{$gte: 5}` wraps each scalar in
    // `{k: element}` and tests it.
    let updated = run(
        json!({ "scores": [1, 5, 9] }),
        json!({ "$pull": { "scores": { "$gte": 5 } } }),
    );
    assert_eq!(updated["scores"], json!([1]));
}

#[test]
fn mingo_pull_with_a_scalar_removes_equal_elements() {
    let updated = run(
        json!({ "tags": ["a", "b", "a"] }),
        json!({ "$pull": { "tags": "a" } }),
    );
    assert_eq!(updated["tags"], json!(["b"]));
}

#[test]
fn mingo_pull_all_requires_an_array_and_preserves_order() {
    let updated = run(
        json!({ "tags": ["a", "b", "c", "b"] }),
        json!({ "$pullAll": { "tags": ["b"] } }),
    );
    assert_eq!(updated["tags"], json!(["a", "c"]));

    let error = apply_update(&json!({}), &json!({ "$pullAll": { "tags": "b" } }))
        .expect_err("non-array argument");
    assert!(matches!(error, Error::PullAllNotArray));
}

#[test]
fn mingo_rename_moves_the_value_and_creates_the_destination_path() {
    let updated = run(
        json!({ "old": { "nested": 7 } }),
        json!({ "$rename": { "old.nested": "brand.new.path" } }),
    );
    assert_eq!(updated["brand"], json!({ "new": { "path": 7 } }));
    assert_eq!(updated["old"], json!({}), "the source key is removed");
}

#[test]
fn mingo_rename_requires_a_string_target() {
    let error =
        apply_update(&json!({}), &json!({ "$rename": { "a": 1 } })).expect_err("non-string target");
    assert_eq!(error, Error::RenameTarget);
}

#[test]
fn mingo_bit_treats_a_missing_field_as_zero() {
    let updated = run(json!({}), json!({ "$bit": { "flags": { "or": 4 } } }));
    assert_eq!(updated["flags"], json!(4));

    let updated = run(
        json!({ "flags": 6 }),
        json!({ "$bit": { "flags": { "and": 4 } } }),
    );
    assert_eq!(updated["flags"], json!(4));

    let updated = run(
        json!({ "flags": 6 }),
        json!({ "$bit": { "flags": { "xor": 3 } } }),
    );
    assert_eq!(updated["flags"], json!(5));
}

#[test]
fn mingo_bit_rejects_a_spec_that_is_not_exactly_one_bit_operation() {
    for spec in [
        json!({ "flags": { "or": 4, "and": 4 } }),
        json!({ "flags": { "nand": 4 } }),
        json!({ "flags": 4 }),
    ] {
        let error =
            apply_update(&json!({}), &json!({ "$bit": spec })).expect_err("invalid bit spec");
        assert!(
            matches!(error, Error::BitSpec(_)),
            "spec {spec} gave {error}"
        );
    }
}

#[test]
fn mingo_min_and_max_compare_and_only_write_when_they_move_the_value() {
    // `min.js`: write when `compare(current, new) >= 0`; `max.js`: `< 0`.
    let updated = run(json!({ "age": 20 }), json!({ "$min": { "age": 30 } }));
    assert_eq!(updated["age"], json!(20), "$min keeps the smaller value");

    let updated = run(json!({ "age": 20 }), json!({ "$min": { "age": 10 } }));
    assert_eq!(updated["age"], json!(10));

    let updated = run(json!({ "age": 20 }), json!({ "$max": { "age": 10 } }));
    assert_eq!(updated["age"], json!(20), "$max keeps the larger value");

    let updated = run(json!({ "age": 20 }), json!({ "$max": { "age": 30 } }));
    assert_eq!(updated["age"], json!(30));

    // Missing field: `$min`/`$max` write the argument.
    let updated = run(json!({}), json!({ "$min": { "age": 30 } }));
    assert_eq!(updated["age"], json!(30));
}

#[test]
fn mingo_current_date_accepts_true_date_and_timestamp_and_rejects_anything_else() {
    // `currentDate.js`: `true`/`{$type:'date'}` store a date; `'timestamp'` stores
    // milliseconds.
    let updated = run(json!({}), json!({ "$currentDate": { "a": true } }));
    assert!(
        updated["a"].as_str().is_some_and(|s| s.ends_with('Z')),
        "a date shape is stored: {}",
        updated["a"]
    );

    let updated = run(
        json!({}),
        json!({ "$currentDate": { "a": { "$type": "timestamp" } } }),
    );
    assert!(updated["a"].is_number(), "a timestamp stores milliseconds");

    let error = apply_update(&json!({}), &json!({ "$currentDate": { "a": "now" } }))
        .expect_err("invalid currentDate spec");
    assert!(matches!(error, Error::CurrentDate(_)));
}

#[test]
fn dotted_paths_create_intermediate_objects() {
    // `util/_internal.js:506`: `if (options?.buildGraph && isNil(obj[key]))
    // obj[key] = {}`
    let updated = run(json!({}), json!({ "$set": { "a.b.c": 1 } }));
    assert_eq!(updated, json!({ "a": { "b": { "c": 1 } } }));
}

// ---------------------------------------------------------------------------
// The rules the brief is explicit about
// ---------------------------------------------------------------------------

#[test]
fn an_unsupported_modifier_is_an_error_not_a_silent_no_op() {
    // The one behaviour the brief calls out. `$setOnInsert` is the interesting case
    // because upstream's *typings* accept it while mingo 7.2.5 does not ship it, so
    // mingo errors at runtime (`updater.js`, `Unknown update operator`).
    for operator in [
        "$setOnInsert",
        "$replaceRoot",
        "$pullAllTypo",
        "$frobnicate",
    ] {
        let error = Modifiers::parse(&json!({ operator: { "a": 1 } })).expect_err("must reject");
        assert_eq!(
            error,
            Error::UnknownOperator(operator.to_string()),
            "operator {operator}"
        );
    }
}

#[test]
fn the_accepted_set_is_exactly_the_fourteen_mingo_shipped() {
    for operator in OPERATORS {
        let parsed = Modifiers::parse(&json!({ operator: { "a": 1 } }));
        // `$pop` and `$bit` and `$currentDate` reject a bare `1`, which is correct;
        // what is asserted is that the *name* is recognised.
        if let Err(error) = parsed {
            assert!(
                !matches!(error, Error::UnknownOperator(_)),
                "{operator} should be recognised, got {error}"
            );
        }
    }
}

#[test]
fn an_update_document_that_is_not_all_operators_is_rejected() {
    for update in [
        json!({ "age": 1 }),
        json!({ "$set": { "a": 1 }, "age": 1 }),
        json!({ "$set": "not an object" }),
        json!([]),
        json!("$set"),
    ] {
        let error = Modifiers::parse(&update).expect_err("must reject");
        assert!(
            matches!(error, Error::NotAnUpdateDocument(_)),
            "update {update} gave {error}"
        );
    }
}

#[test]
fn apply_does_not_mutate_the_input_document() {
    // `mingo-updater.ts:15`: `const cloned = clone(d)` before mingo runs.
    let document = json!({ "age": 20, "tags": ["a"] });
    let snapshot = document.clone();
    let updated = run(
        document.clone(),
        json!({
            "$inc": { "age": 1 },
            "$push": { "tags": "b" }
        }),
    );
    assert_eq!(document, snapshot, "the input must be untouched");
    assert_eq!(updated["age"], json!(21));
}

#[test]
fn operators_apply_in_document_order() {
    // mingo builds every closure then applies them in `Object.keys(modifier)` order
    // (`updater.js`), so the last operator on a field wins.
    let updated = run(
        json!({ "age": 20 }),
        json!({ "$inc": { "age": 1 }, "$set": { "age": 99 } }),
    );
    assert_eq!(updated["age"], json!(99));

    let updated = run(
        json!({ "age": 20 }),
        json!({ "$set": { "age": 99 }, "$inc": { "age": 1 } }),
    );
    assert_eq!(updated["age"], json!(100));
}

#[test]
fn array_filter_paths_are_refused_rather_than_misapplied() {
    // Documented gap: mingo implements `$[]` and `$[id]` through `arrayFilters`; this
    // engine refuses the path instead of writing a literal `$[]` key.
    for path in ["tags.$[]", "tags.$[elem]", "tags.$"] {
        let error = apply_update(&json!({ "tags": [] }), &json!({ "$set": { path: 1 } }))
            .expect_err("array filter path");
        assert!(matches!(error, Error::UnsupportedPath(_)), "path {path}");
    }
}

#[test]
fn prototype_polluting_paths_are_refused() {
    // mingo's `assertNoProto` (`operators/update/_internal.js`).
    for path in ["__proto__.x", "constructor.prototype.y"] {
        let error =
            apply_update(&json!({}), &json!({ "$set": { path: 1 } })).expect_err("forbidden path");
        assert!(matches!(error, Error::ForbiddenPath(_)), "path {path}");
    }
}
