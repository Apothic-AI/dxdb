//! Property tests: the matcher and the index encoder, against their own invariants.
//!
//! The oracle vectors are the primary evidence — they compare against real RxDB output.
//! This harness covers the space the vectors do not enumerate: a generated document, a
//! generated selector, and a set of invariants that must hold whatever the input.
//!
//! The invariants are chosen because each one, if violated, means a query silently
//! returns the wrong rows rather than failing:
//!
//! - **`$eq` implies `$gte` and `$lte`** on the same value. If it did not, a planner that
//!   turned an equality into a range would drop the row it was asked for.
//! - **`$in` of one value equals `$eq`** of that value. If it did not, the `$in` range
//!   optimisation in the planner would change results rather than just the scan width.
//! - **`$ne` is the complement of `$eq`.** If it did not, a filtered query would return a
//!   row twice or not at all.
//! - **`$gt` and `$lte` are disjoint, and `$gte` is the union of `$gt` and `$eq`.**
//!   Boundary handling is where the exclusive/inclusive distinction goes wrong.
//! - **`$and` of two selectors matches exactly the intersection; `$or` the union.**
//!   Composition is what every real query relies on.
//! - **Index strings are order-preserving and fixed-width**: two documents in index order
//!   produce index strings in the same order, and every string for an index is the same
//!   length. This is the property the whole storage layer sorts on.
//!
//! Every property is checked against generated documents, so a counterexample shrinks to a
//! small one that names the exact input.

use proptest::prelude::*;
use serde_json::{json, Value};

use dxdb_query::{get_index_string_length, get_indexable_string, Matcher};

/// A generated JSON scalar, weighted toward the types the matcher treats specially.
fn arb_scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        3 => any::<i64>().prop_map(|value| json!(value)),
        2 => (any::<i64>()).prop_map(|value| json!(value as f64 / 4.0)),
        3 => "[a-zA-Z0-9_.-]{0,8}".prop_map(Value::String),
        1 => any::<bool>().prop_map(Value::Bool),
        1 => Just(Value::Null),
        // The empty string and a string with a code unit above the BMP, because both are
        // boundary cases for padding and for code-unit ordering.
        1 => Just(Value::String(String::new())),
        1 => Just(Value::String("\u{1F600}".to_string())),
        1 => Just(Value::String("\u{FFFF}".to_string())),
    ]
}

/// Builds `{field: {operator: operand}}` without moving `field`.
///
/// `json!({field: ...})` treats a bare identifier as a *key literal* when it is a string
/// literal, but as a variable to serialize when it is a binding — and moving the binding
/// into the macro would make the second use in a test fail to compile.
fn selector(field: &str, operator: &str, operand: Value) -> Value {
    let mut matcher = serde_json::Map::new();
    matcher.insert(operator.to_string(), operand);
    let mut selector = serde_json::Map::new();
    selector.insert(field.to_string(), Value::Object(matcher));
    Value::Object(selector)
}

/// A generated document: a fixed schema-shaped object so selectors can address it.
fn arb_document() -> impl Strategy<Value = Value> {
    (
        any::<bool>(),
        prop::option::of(any::<i64>()),
        prop::option::of("[a-zA-Z]{0,6}"),
    )
        .prop_map(|(deleted, number, text)| {
            let mut document = serde_json::Map::new();
            document.insert("_deleted".into(), Value::Bool(deleted));
            if let Some(number) = number {
                document.insert("num".into(), json!(number));
            }
            if let Some(text) = text {
                document.insert("text".into(), Value::String(text));
            }
            Value::Object(document)
        })
}

/// A comparison operator and its operand.
fn arb_comparison() -> impl Strategy<Value = (&'static str, Value)> {
    ("\\$eq|\\$ne|\\$gt|\\$gte|\\$lt|\\$lte", arb_scalar())
        .prop_map(|(operator, operand)| {
            let operator: &'static str = match operator.as_str() {
                "$eq" => "$eq",
                "$ne" => "$ne",
                "$gt" => "$gt",
                "$gte" => "$gte",
                "$lt" => "$lt",
                _ => "$lte",
            };
            (operator, operand)
        })
}

/// A selector over the generated document shape.
fn arb_selector() -> impl Strategy<Value = Value> {
    prop_oneof![
        arb_comparison().prop_map(|(operator, operand)| json!({"num": {operator: operand}})),
        arb_comparison().prop_map(|(operator, operand)| json!({"text": {operator: operand}})),
        arb_comparison().prop_map(|(operator, operand)| json!({"_deleted": {operator: operand}})),
        (arb_scalar(), arb_scalar()).prop_map(|(left, right)| {
            json!({"$and": [{"num": {"$eq": left}}, {"num": {"$eq": right}}]})
        }),
        (arb_scalar(), arb_scalar()).prop_map(|(left, right)| {
            json!({"$or": [{"num": {"$eq": left}}, {"num": {"$eq": right}}]})
        }),
        prop::collection::vec(arb_scalar(), 0..4)
            .prop_map(|values| json!({"num": {"$in": values}})),
        prop::collection::vec(arb_scalar(), 0..4)
            .prop_map(|values| json!({"num": {"$nin": values}})),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        // A failure should print the input that broke it, not a backtrace.
        max_shrink_iters: 2048,
        ..ProptestConfig::default()
    })]

    /// `$eq` implies `$gte` and `$lte` for the same operand — **when the operand is not
    /// `null`**.
    ///
    /// This is the invariant that makes it safe for a planner to turn an equality into a
    /// closed range, which is exactly what `get_matcher_query_opts` does for `$eq`.
    ///
    /// The `null` exception is not a bug here; it is mingo's, and it is asserted separately
    /// in [`eq_null_matches_a_missing_field_but_gte_null_does_not`]. `$eq` special-cases a
    /// nil pair (`isNil(a) && isNil(b)`), so `{field: {$eq: null}}` matches a document that
    /// lacks the field — while `$gte: null` goes through the ordering comparison, which
    /// requires `typeOf(a) === typeOf(b)` and therefore rejects a resolved `undefined`
    /// before it ever compares. A planner that turned `$eq: null` into the range
    /// `[null, null]` would therefore drop every document the equality matched.
    #[test]
    fn eq_implies_gte_and_lte_for_non_null_operands(
        document in arb_document(),
        operand in arb_scalar(),
        field in "num|text|_deleted",
    ) {
        prop_assume!(!operand.is_null());
        let eq = Matcher::new(&selector(&field, "$eq", operand.clone())).expect("compiles");
        let gte = Matcher::new(&selector(&field, "$gte", operand.clone())).expect("compiles");
        let lte = Matcher::new(&selector(&field, "$lte", operand.clone())).expect("compiles");
        if eq.matches(&document) {
            prop_assert!(
                gte.matches(&document),
                "$eq({:?}) held but $gte did not, for field {:?} in {}",
                operand, field, document
            );
            prop_assert!(
                lte.matches(&document),
                "$eq({:?}) held but $lte did not, for field {:?} in {}",
                operand, field, document
            );
        }
    }

    /// The `null` asymmetry, pinned as a test rather than left to a comment.
    ///
    /// Verified against upstream mingo 7.2.5 (`$eq` in `_predicates.js`): `$eq: null`
    /// matches a missing field, `$gte: null` does not. The planner must therefore never
    /// translate `$eq: null` into a `[null, null]` range.
    #[test]
    fn eq_null_matches_a_missing_field_but_gte_null_does_not(document in arb_document()) {
        let eq = Matcher::new(&json!({"num": {"$eq": null}})).expect("compiles");
        let gte = Matcher::new(&json!({"num": {"$gte": null}})).expect("compiles");
        let lte = Matcher::new(&json!({"num": {"$lte": null}})).expect("compiles");
        // For a document whose `num` is present, all three agree (null equals null).
        if document.get("num").is_some() && document["num"].is_null() {
            prop_assert!(eq.matches(&document));
            prop_assert!(gte.matches(&document));
            prop_assert!(lte.matches(&document));
        }
        // For a document with a real number, none holds.
        if document.get("num").map(|value| value.is_number()).unwrap_or(false) {
            prop_assert!(!eq.matches(&document));
            prop_assert!(!gte.matches(&document));
            prop_assert!(!lte.matches(&document));
        }
        // For a document without the field, only `$eq` holds.
        if document.get("num").is_none() {
            prop_assert!(eq.matches(&document), "an absent field equals an explicit null");
            prop_assert!(!gte.matches(&document), "but is not ordered against one");
            prop_assert!(!lte.matches(&document));
        }
    }

    /// `$in` with one value behaves exactly like `$eq` with that value.
    #[test]
    fn in_of_one_equals_eq(document in arb_document(), operand in arb_scalar(), field in "num|text|_deleted") {
        let eq = Matcher::new(&selector(&field, "$eq", operand.clone())).expect("compiles");
        let in_one = Matcher::new(&selector(&field, "$in", json!([operand]))).expect("compiles");
        prop_assert_eq!(
            eq.matches(&document),
            in_one.matches(&document),
            "$in of one must equal $eq for field {:?} in {}",
            field,
            document
        );
    }

    /// `$ne` is the exact complement of `$eq`.
    #[test]
    fn ne_is_the_complement_of_eq(document in arb_document(), operand in arb_scalar(), field in "num|text|_deleted") {
        let eq = Matcher::new(&selector(&field, "$eq", operand.clone())).expect("compiles");
        let ne = Matcher::new(&selector(&field, "$ne", operand)).expect("compiles");
        prop_assert_ne!(
            eq.matches(&document),
            ne.matches(&document),
            "$ne must be the complement of $eq for field {:?} in {}",
            field,
            document
        );
    }

    /// `$gte` is `$gt` or `$eq` — the boundary is in exactly one of them.
    ///
    /// Again excluding `null`, for the reason documented on
    /// [`eq_implies_gte_and_lte_for_non_null_operands`].
    #[test]
    fn gte_is_gt_or_eq(document in arb_document(), operand in arb_scalar(), field in "num|text|_deleted") {
        prop_assume!(!operand.is_null());
        let eq = Matcher::new(&selector(&field, "$eq", operand.clone())).expect("compiles");
        let gt = Matcher::new(&selector(&field, "$gt", operand.clone())).expect("compiles");
        let gte = Matcher::new(&selector(&field, "$gte", operand.clone())).expect("compiles");
        let eq_match = eq.matches(&document);
        let gt_match = gt.matches(&document);
        prop_assert_eq!(
            gte.matches(&document),
            gt_match || eq_match,
            "$gte must be exactly ($gt or $eq) for field {:?} in {}",
            field,
            document
        );
        // And the two halves are mutually exclusive.
        prop_assert!(
            !(gt_match && eq_match),
            "$gt and $eq must not both hold for field {:?} in {}",
            field,
            document
        );
    }

    /// `$lt` and `$gte` partition the documents.
    #[test]
    fn lt_and_gte_partition(document in arb_document(), operand in arb_scalar(), field in "num|text|_deleted") {
        let lt = Matcher::new(&selector(&field, "$lt", operand.clone())).expect("compiles");
        let gte = Matcher::new(&selector(&field, "$gte", operand.clone())).expect("compiles");
        let lt_match = lt.matches(&document);
        let gte_match = gte.matches(&document);
        // They must not both match...
        prop_assert!(
            !(lt_match && gte_match),
            "$lt and $gte must be disjoint for field {:?} in {}",
            field,
            document
        );
        // ...and when the two sides have the same mingo type, exactly one matches.
        // A cross-type comparison matches neither: mingo's `$lt` requires
        // `typeOf(a) === typeOf(b)`, so `{num: {$lt: "x"}}` is simply false rather than
        // an ordering. That is upstream behaviour and is asserted as such.
        if let Some(field_value) = document.get(&field) {
            let same_type = matches!(
                (field_value, &operand),
                (Value::Number(_), Value::Number(_))
                    | (Value::String(_), Value::String(_))
                    | (Value::Bool(_), Value::Bool(_))
            );
            if same_type {
                prop_assert!(
                    lt_match || gte_match,
                    "for same-typed {:?} vs {:?}, exactly one of $lt/$gte must hold \
                     (field {:?} in {})",
                    field_value,
                    operand,
                    field,
                    document
                );
            } else {
                prop_assert!(
                    !lt_match && !gte_match,
                    "a cross-type comparison must match neither $lt nor $gte \
                     (field {:?} in {})",
                    field,
                    document
                );
            }
        }
    }

    /// `$and` matches exactly the intersection of its arms; `$or` exactly the union.
    #[test]
    fn and_is_intersection_or_is_union(
        document in arb_document(),
        left in arb_selector(),
        right in arb_selector(),
    ) {
        let left_matcher = Matcher::new(&left).expect("compiles");
        let right_matcher = Matcher::new(&right).expect("compiles");
        let and_matcher = Matcher::new(&json!({"$and": [left.clone(), right.clone()]}))
            .expect("compiles");
        let or_matcher = Matcher::new(&json!({"$or": [left.clone(), right.clone()]}))
            .expect("compiles");

        let left_match = left_matcher.matches(&document);
        let right_match = right_matcher.matches(&document);
        prop_assert_eq!(
            and_matcher.matches(&document),
            left_match && right_match,
            "$and must be the intersection of {} and {} on {}",
            left,
            right,
            document
        );
        prop_assert_eq!(
            or_matcher.matches(&document),
            left_match || right_match,
            "$or must be the union of {} and {} on {}",
            left,
            right,
            document
        );
    }

    /// `$nor` is the complement of `$or`.
    #[test]
    fn nor_is_the_complement_of_or(
        document in arb_document(),
        left in arb_selector(),
        right in arb_selector(),
    ) {
        let or_matcher = Matcher::new(&json!({"$or": [left.clone(), right.clone()]}))
            .expect("compiles");
        let nor_matcher = Matcher::new(&json!({"$nor": [left.clone(), right.clone()]}))
            .expect("compiles");
        prop_assert_ne!(
            or_matcher.matches(&document),
            nor_matcher.matches(&document),
            "$nor must be the complement of $or for {} and {} on {}",
            left,
            right,
            document
        );
    }

    /// The matcher is total: any generated selector either matches or does not, without
    /// panicking, and never depends on the *key order* of the selector.
    #[test]
    fn matching_is_deterministic_and_key_order_irrelevant(
        document in arb_document(),
        operand in arb_scalar(),
    ) {
        let forward = json!({"num": {"$gte": operand.clone()}, "_deleted": {"$eq": false}});
        let reversed = json!({"_deleted": {"$eq": false}, "num": {"$gte": operand}});
        let forward_matcher = Matcher::new(&forward).expect("compiles");
        let reversed_matcher = Matcher::new(&reversed).expect("compiles");
        let first = forward_matcher.matches(&document);
        prop_assert_eq!(first, forward_matcher.matches(&document), "matching must be pure");
        prop_assert_eq!(
            first,
            reversed_matcher.matches(&document),
            "selector key order must not matter"
        );
    }
}

// --- index-string invariants -------------------------------------------------

/// A schema with one string, one number and one boolean field, plus a primary key.
fn index_schema() -> Value {
    dxdb_schema::fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 8},
            "text": {"type": "string", "maxLength": 6},
            "num": {"type": "number", "minimum": -10, "maximum": 100, "multipleOf": 0.1},
            "flag": {"type": "boolean"}
        },
        "required": ["id", "text", "num", "flag"],
        "indexes": [
            ["text"],
            ["num"],
            ["flag"],
            ["text", "num"],
            ["flag", "num", "id"]
        ]
    }))
    .expect("fills")
}

/// A document for the index schema.
fn arb_index_document() -> impl Strategy<Value = Value> {
    (
        "[a-z0-9]{0,8}",
        "[a-z0-9]{0,6}",
        any::<i64>(),
        any::<f64>(),
        any::<bool>(),
    )
        .prop_map(|(id, text, integer, fractional, flag)| {
            json!({
                "id": id,
                "text": text,
                "num": if fractional.is_finite() { fractional } else { integer as f64 },
                "flag": flag,
                "_deleted": false,
                "_rev": "1-x",
                "_meta": {"lwt": 1},
                "_attachments": {}
            })
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Every index string for an index has that index's fixed width.
    ///
    /// Fixed width is what makes comparing concatenations equivalent to comparing the
    /// field tuple, so a variable-width string would silently reorder documents.
    #[test]
    fn index_strings_are_fixed_width(document in arb_index_document()) {
        let schema = index_schema();
        for index in dxdb_schema::get_string_indexes(&schema).expect("indexes") {
            let width = get_index_string_length(&schema, &index).expect("width");
            let encoded = get_indexable_string(&schema, &index, &document).expect("encodes");
            prop_assert_eq!(
                encoded.chars().count(),
                width,
                "index {:?} produced {:?} for {}",
                index,
                encoded,
                document
            );
        }
    }

    /// Values outside the declared range clamp rather than producing a broken encoding.
    ///
    /// The number field is bounded to `[-10, 100]`; a document value far outside it must
    /// encode identically to the boundary value, and must never be *shorter* or sort below
    /// the minimum.
    #[test]
    fn out_of_range_numbers_clamp_to_the_boundary(
        far in prop_oneof![(-100_000i64..-11), (101i64..100_000)],
        id in "[a-z]{1,8}",
    ) {
        let schema = index_schema();
        let index = vec!["num".to_string()];
        let width = get_index_string_length(&schema, &index).expect("width");
        let clamped = if far < 0 { -10.0 } else { 100.0 };
        let document_out_of_range = json!({
            "id": id, "text": "x", "num": far as f64, "flag": true,
            "_deleted": false, "_rev": "1-x", "_meta": {"lwt": 1}, "_attachments": {}
        });
        let document_at_boundary = json!({
            "id": id, "text": "x", "num": clamped, "flag": true,
            "_deleted": false, "_rev": "1-x", "_meta": {"lwt": 1}, "_attachments": {}
        });
        let out_of_range = get_indexable_string(&schema, &index, &document_out_of_range)
            .expect("encodes");
        let at_boundary = get_indexable_string(&schema, &index, &document_at_boundary)
            .expect("encodes");
        prop_assert_eq!(
            &out_of_range,
            &at_boundary,
            "a value of {} must clamp to {}",
            far,
            clamped
        );
        prop_assert_eq!(out_of_range.chars().count(), width);
    }

    /// The encoding is monotone: if one document's number is smaller, its index string
    /// sorts before the other's.
    ///
    /// This is the property a storage's binary search depends on. It is checked on the
    /// *unclamped* range, because clamping deliberately makes out-of-range values tie.
    #[test]
    fn encoding_is_monotone_in_the_number(
        small in -10i64..50,
        delta in 1i64..50,
        id in "[a-z]{1,8}",
    ) {
        let schema = index_schema();
        let index = vec!["num".to_string()];
        let large = small + delta;
        let make = |value: i64| json!({
            "id": id, "text": "x", "num": value as f64, "flag": true,
            "_deleted": false, "_rev": "1-x", "_meta": {"lwt": 1}, "_attachments": {}
        });
        let smaller = get_indexable_string(&schema, &index, &make(small)).expect("encodes");
        let larger = get_indexable_string(&schema, &index, &make(large)).expect("encodes");
        prop_assert!(
            dxdb_jsstr::lt(&smaller, &larger),
            "{} must encode before {} (got {:?} and {:?})",
            small,
            large,
            smaller,
            larger
        );
    }

    /// A string field's encoding is order-preserving under code-unit comparison.
    ///
    /// The padding is designed so that a shorter string that is a prefix sorts first,
    /// which is what `padEnd` with spaces gives and what the storage relies on.
    #[test]
    fn string_encoding_preserves_code_unit_order(
        left in "[a-z]{0,6}",
        right in "[a-z]{0,6}",
    ) {
        let schema = index_schema();
        let index = vec!["text".to_string()];
        let make = |text: &str| json!({
            "id": "a", "text": text, "num": 0, "flag": true,
            "_deleted": false, "_rev": "1-x", "_meta": {"lwt": 1}, "_attachments": {}
        });
        let left_encoded = get_indexable_string(&schema, &index, &make(&left)).expect("encodes");
        let right_encoded = get_indexable_string(&schema, &index, &make(&right)).expect("encodes");
        prop_assert_eq!(
            dxdb_jsstr::compare_js(&left_encoded, &right_encoded),
            dxdb_jsstr::compare_js(&left, &right),
            "encoding must preserve order: {:?} vs {:?}",
            left,
            right
        );
    }
}
