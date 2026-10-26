//! Ported from `test/unit/validate.test.ts` (1,020 lines) at upstream `3b763ee17`.
//!
//! # How this file relates to upstream's
//!
//! Upstream runs its whole suite once **per validator backend** — it is a
//! `validationImplementation => describe(...)` loop (`validate.test.ts:65`), invoked for ajv,
//! ajv-standalone, is-my-json-valid and z-schema. Every case in it is therefore a statement
//! about *the shared wrapper*, executed four times with a different engine bolted underneath.
//! That is the strongest evidence for the collapse decision in the crate docs: the upstream
//! suite's own shape says the differences are not behavioural.
//!
//! So the cases are ported **once**, and each names the upstream line. The `describe` blocks
//! covered are: `RxStorageInstance` positive (`:137`) and negative (`:268`), the
//! `bulkWrite`/event interaction (`:511`), the preInsert-hook case (`:684`), and the format
//! cases (`:715`).
//!
//! # Two cases are deliberately not ported, with reasons
//!
//! * **`:951` and `:985`, "should be able to register a custom format"** — these call into the
//!   backend's format registry (`ajv.addFormat`, `z-schema.registerFormat`). With one engine
//!   there is one registry, so the behaviour is "does `jsonschema` support a format" — a
//!   question about a dependency, not about this crate. If a format is missing, it is added
//!   where `dxdb-schema` configures the validator, once, not per backend.
//! * **`:190` "should allow this complex regex pattern"** — a `$regex` inside a *document
//!   selector*, i.e. a query-engine concern reached through the validator. `dxdb-query` covers
//!   `$regex` with `regress`; re-testing it here would be testing a dependency twice.

use dxdb_plugin_validate::{
    partition_rows, wrapped_storage_name, CandidateRow, SchemaValidator, ValidateStorage,
    Validator, ValidatorCache, VALIDATION_STATUS,
};
use serde_json::{json, Value};

/// The `human` fixture, filled the way a collection's schema is.
fn human_schema() -> Value {
    dxdb_schema::fill_with_default_settings(&json!({
        "title": "human",
        "version": 0,
        "description": "describes a human being",
        "type": "object",
        "primaryKey": "passportId",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "firstName": { "type": "string", "maxLength": 100 },
            "lastName": { "type": "string", "maxLength": 100 },
            "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 },
        },
        "required": ["passportId"],
        "indexes": ["firstName"],
    }))
    .expect("fill")
}

fn human(passport_id: &str) -> Value {
    json!({
        "passportId": passport_id,
        "firstName": "Alice",
        "lastName": "Anderson",
        "age": 30,
        "_deleted": false,
        "_rev": "1-abc",
        "_meta": { "lwt": 1 },
        "_attachments": {},
    })
}

fn row(passport_id: &str, document: Value) -> CandidateRow {
    CandidateRow {
        document_id: passport_id.to_string(),
        document,
        context: "validate-test".to_string(),
        previous: None,
    }
}

/// `assertBulkWriteNoError` (`validate.test.ts:67-74`): the batch reports no errors.
fn assert_no_error(storage: &mut ValidateStorage, schema: &Value, rows: Vec<CandidateRow>) {
    let partition = storage.partition(schema, rows).expect("partition");
    assert!(
        partition.rejected.is_empty(),
        "expected no errors, got {:?}",
        partition.rejected
    );
}

/// `assertBulkWriteValidationError` (`validate.test.ts:75-97`): every error is a `422` and
/// carries the schema, optionally containing a substring.
fn assert_validation_error(
    storage: &mut ValidateStorage,
    schema: &Value,
    rows: Vec<CandidateRow>,
    must_contain: Option<&str>,
) {
    let partition = storage.partition(schema, rows).expect("partition");
    assert!(
        !partition.rejected.is_empty(),
        "expected a validation error"
    );
    for rejected in &partition.rejected {
        let error = rejected.to_write_error_json();
        assert_eq!(
            error["status"],
            json!(VALIDATION_STATUS),
            "every validation error is a 422"
        );
        // "422 validation errors must include the schema for easier debugging"
        // (`validate.test.ts:84-85`).
        assert_eq!(
            error["schema"]["type"],
            json!("object"),
            "the schema must be included"
        );
        if let Some(needle) = must_contain {
            assert!(
                error.to_string().contains(needle),
                "error does not include {needle:?}: {error}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// `validate.test.ts:100` — create storage
// ---------------------------------------------------------------------------

#[test]
fn upstream_100_the_wrapped_storage_name_identifies_the_validator() {
    // `:100-110`. Upstream asserts the storage was created and its name carries the validator,
    // which is what makes a misconfigured storage diagnosable.
    let storage = ValidateStorage::new("memory");
    assert_eq!(storage.name(), "validate-schema-memory");
    assert!(storage.name().starts_with("validate-"));
}

// ---------------------------------------------------------------------------
// `validate.test.ts:138` — validate one human
// ---------------------------------------------------------------------------

#[test]
fn upstream_138_a_valid_human_inserts_without_error() {
    // `:138-145`.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    assert_no_error(&mut storage, &schema, vec![row("foobar", human("foobar"))]);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:146` — validate one point
// ---------------------------------------------------------------------------

#[test]
fn upstream_146_a_schema_with_numeric_bounds_accepts_in_range_values() {
    // `:146-152`. A different schema shape (a "point" with coordinates) through the same
    // wrapper — which is the point of having one wrapper.
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "point",
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "x": { "type": "number", "minimum": -180, "maximum": 180, "multipleOf": 0.01 },
            "y": { "type": "number", "minimum": -90, "maximum": 90, "multipleOf": 0.01 },
        },
        "required": ["id", "x", "y"],
    }))
    .expect("fill");
    let mut storage = ValidateStorage::new("memory");
    let document = json!({
        "id": "p1", "x": 12.5, "y": -3.25,
        "_deleted": false, "_rev": "1-abc", "_meta": { "lwt": 1 }, "_attachments": {}
    });
    assert_no_error(&mut storage, &schema, vec![row("p1", document)]);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:153` — validate without non-required
// ---------------------------------------------------------------------------

#[test]
fn upstream_153_an_absent_optional_field_is_valid() {
    // `:153-162`: a document omitting every non-required property still validates.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let mut document = human("minimal");
    for field in ["firstName", "lastName", "age"] {
        document.as_object_mut().expect("object").remove(field);
    }
    assert_no_error(&mut storage, &schema, vec![row("minimal", document)]);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:163` — validate nested
// ---------------------------------------------------------------------------

#[test]
fn upstream_163_a_nested_schema_is_validated() {
    // `:163-170`. Nested objects must be checked, not just the top level — a wrapper that
    // validated only required top-level keys would pass `:153` and fail this.
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "nested",
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "nested": {
                "type": "object",
                "properties": { "value": { "type": "integer", "minimum": 0, "maximum": 10, "multipleOf": 1 } },
                "required": ["value"],
                "additionalProperties": false,
            },
        },
        "required": ["id", "nested"],
    }))
    .expect("fill");
    let mut storage = ValidateStorage::new("memory");
    let base = json!({ "id": "n", "_deleted": false, "_rev": "1-a", "_meta": { "lwt": 1 }, "_attachments": {} });

    let mut good = base.clone();
    good["nested"] = json!({ "value": 5 });
    assert_no_error(&mut storage, &schema, vec![row("n", good)]);

    let mut bad = base;
    bad["nested"] = json!({ "value": 50 });
    assert_validation_error(&mut storage, &schema, vec![row("n", bad)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:171` — validate with decimal _meta.lwt times
// ---------------------------------------------------------------------------

#[test]
fn upstream_171_a_decimal_lwt_is_valid() {
    // `:171-189`. `_meta.lwt` is `multipleOf: 0.01` in the injected meta schema, so a decimal
    // timestamp must pass — this is about the *injected* meta fields being validated too,
    // which is why the wrapper validates the document as the storage receives it.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let mut document = human("decimal");
    document["_meta"] = json!({ "lwt": 1700000000000.42 });
    assert_no_error(&mut storage, &schema, vec![row("decimal", document)]);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:227` — nested additionalProperties
// ---------------------------------------------------------------------------

#[test]
fn upstream_227_a_schema_as_nested_additional_properties_is_honoured() {
    // `:227-267`. `additionalProperties: <schema>` means the extra keys are themselves
    // validated, not just allowed.
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "dict",
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "bag": {
                "type": "object",
                "additionalProperties": { "type": "integer", "minimum": 0, "maximum": 9, "multipleOf": 1 },
            },
        },
        "required": ["id", "bag"],
    }))
    .expect("fill");
    let mut storage = ValidateStorage::new("memory");
    let base = json!({ "id": "d", "_deleted": false, "_rev": "1-a", "_meta": { "lwt": 1 }, "_attachments": {} });

    let mut good = base.clone();
    good["bag"] = json!({ "a": 1, "b": 9 });
    assert_no_error(&mut storage, &schema, vec![row("d", good)]);

    let mut bad = base;
    bad["bag"] = json!({ "a": 1, "b": 99 });
    assert_validation_error(&mut storage, &schema, vec![row("d", bad)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:269` — not validate other object
// ---------------------------------------------------------------------------

#[test]
fn upstream_269_a_document_missing_required_fields_is_rejected() {
    // `:269-280`.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    assert_validation_error(
        &mut storage,
        &schema,
        vec![row("x", json!({ "firstName": "Alice" }))],
        None,
    );
}

// ---------------------------------------------------------------------------
// `validate.test.ts:281` — required field not given
// ---------------------------------------------------------------------------

#[test]
fn upstream_281_a_missing_required_field_is_named_in_the_error() {
    // `:281-294`. The error must be specific enough to act on, which is what the
    // `must_contain` assertion upstream uses is for.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let mut document = human("x");
    document
        .as_object_mut()
        .expect("object")
        .remove("passportId");
    assert_validation_error(
        &mut storage,
        &schema,
        vec![row("x", document)],
        Some("passportId"),
    );
}

// ---------------------------------------------------------------------------
// `validate.test.ts:295` — overflow maximum int
// ---------------------------------------------------------------------------

#[test]
fn upstream_295_a_number_above_maximum_is_rejected() {
    // `:295-308`.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let mut document = human("x");
    document["age"] = json!(999);
    assert_validation_error(&mut storage, &schema, vec![row("x", document)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:309` — additional property
// ---------------------------------------------------------------------------

#[test]
fn upstream_309_an_additional_property_is_rejected() {
    // `:309-321`. `additionalProperties: false` is injected by defaulting, so an unknown field
    // is an error rather than being stored.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let mut document = human("x");
    document["surprise"] = json!("hello");
    assert_validation_error(&mut storage, &schema, vec![row("x", document)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:322` — nested additionalProperties: false
// ---------------------------------------------------------------------------

#[test]
fn upstream_322_nested_additional_properties_false_is_respected() {
    // `:322-345`.
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "strict-nested",
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "inner": {
                "type": "object",
                "properties": { "known": { "type": "string", "maxLength": 10 } },
                "additionalProperties": false,
            },
        },
        "required": ["id", "inner"],
    }))
    .expect("fill");
    let mut storage = ValidateStorage::new("memory");
    let body = json!({
        "id": "s", "inner": { "known": "yes", "unknown": "no" },
        "_deleted": false, "_rev": "1-a", "_meta": { "lwt": 1 }, "_attachments": {}
    });
    assert_validation_error(&mut storage, &schema, vec![row("s", body)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:346` — do not allow primary == null
// ---------------------------------------------------------------------------

#[test]
fn upstream_346_a_null_primary_key_is_rejected() {
    // `:346-358`.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let mut document = human("x");
    document["passportId"] = json!(null);
    assert_validation_error(&mut storage, &schema, vec![row("x", document)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:359` — enum does not match
// ---------------------------------------------------------------------------

#[test]
fn upstream_359_a_value_outside_the_enum_is_rejected() {
    // `:359-...`.
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "enumerated",
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "status": { "type": "string", "maxLength": 10, "enum": ["open", "closed"] },
        },
        "required": ["id", "status"],
    }))
    .expect("fill");
    let mut storage = ValidateStorage::new("memory");
    let base = json!({ "id": "e", "_deleted": false, "_rev": "1-a", "_meta": { "lwt": 1 }, "_attachments": {} });

    let mut good = base.clone();
    good["status"] = json!("open");
    assert_no_error(&mut storage, &schema, vec![row("e", good)]);

    let mut bad = base;
    bad["status"] = json!("sideways");
    assert_validation_error(&mut storage, &schema, vec![row("e", bad)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:511` — no event on a non-success insert
// ---------------------------------------------------------------------------

#[test]
fn upstream_511_a_rejected_row_is_not_in_the_accepted_batch() {
    // `:511-537` asserts that a failed insert emits no change event. The event source is the
    // storage write, so the property this crate owns is that the rejected row is *absent* from
    // `accepted` — the thing that guarantees nothing below it runs.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let partition = storage
        .partition(
            &schema,
            vec![row("bad", json!({ "age": 1 })), row("good", human("good"))],
        )
        .expect("partition");
    assert_eq!(
        partition
            .accepted
            .iter()
            .map(|row| row.document_id.as_str())
            .collect::<Vec<_>>(),
        vec!["good"],
        "the invalid row must never reach the storage"
    );
}

// ---------------------------------------------------------------------------
// `validate.test.ts:538` / `:557` — additional prop, primary missing
// ---------------------------------------------------------------------------

#[test]
fn upstream_538_an_additional_property_alone_blocks_the_insert() {
    // `:538-556`.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let mut document = human("x");
    document["extra"] = json!(1);
    assert_validation_error(&mut storage, &schema, vec![row("x", document)], None);
}

#[test]
fn upstream_557_a_missing_primary_key_blocks_the_insert() {
    // `:557-579`.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let document = json!({
        "firstName": "Alice",
        "_deleted": false, "_rev": "1-a", "_meta": { "lwt": 1 }, "_attachments": {}
    });
    assert_validation_error(&mut storage, &schema, vec![row("unknown", document)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:580` / `:603` — schema not matching at creation / write
// ---------------------------------------------------------------------------

#[test]
fn upstream_580_an_uncompilable_schema_is_a_start_up_error() {
    // `:580-602`. A schema the engine cannot compile must fail when the validator is built,
    // not on the first write.
    //
    // Note what is NOT a failure: `properties: {}` is accepted, because `dxdb-schema`'s SC29
    // checks that `properties` is *present and an object*, not that it is non-empty. Asserted
    // rather than assumed, so the boundary is recorded.
    let empty_but_valid = json!({ "type": "object", "version": 0, "primaryKey": "id", "properties": {} });
    assert!(
        SchemaValidator::new(&empty_but_valid).is_ok(),
        "an empty properties object is a legal schema"
    );

    // A schema that is not valid draft-07 at all cannot be compiled.
    let broken = json!({ "type": "object", "properties": { "id": { "type": "notatype" } } });
    assert!(
        SchemaValidator::new(&broken).is_err(),
        "an invalid type keyword must not produce a validator"
    );
}

#[test]
fn upstream_603_a_schema_that_stops_matching_is_caught_per_write() {
    // `:603-632`. The counterpart: once a validator exists, a document that does not match it
    // fails at write time.
    let schema = human_schema();
    let validator = SchemaValidator::new(&schema).expect("validator");
    assert!(validator.validate(&human("ok")).expect("valid").is_empty());
    assert!(!validator
        .validate(&json!({ "passportId": "x", "age": "notanumber" }))
        .expect("invalid")
        .is_empty());
}

// ---------------------------------------------------------------------------
// `validate.test.ts:662` — crash on non document field
// ---------------------------------------------------------------------------

#[test]
fn upstream_662_a_non_object_document_is_rejected_not_a_panic() {
    // `:662-683`. A row whose `document` is not an object must be reported, not crash the
    // batch — one bad row cannot take the others with it.
    let schema = human_schema();
    let validator = SchemaValidator::new(&schema).expect("validator");
    assert!(validator.validate(&json!(42)).is_err());

    let mut storage = ValidateStorage::new("memory");
    let partition = storage
        .partition(
            &schema,
            vec![row("weird", json!(42)), row("fine", human("fine"))],
        )
        .expect("partition");
    assert_eq!(partition.rejected.len(), 1);
    assert_eq!(partition.accepted.len(), 1);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:684` — preInsert hook invalidates the schema
// ---------------------------------------------------------------------------

#[test]
fn upstream_684_the_validator_sees_post_insert_defaults() {
    // `:684-714`. Upstream's point is that validation happens on the document as the storage
    // will store it — after insert defaulting — so a field with a `default` is already filled
    // when the validator runs. That is why `_deleted`/`_rev`/`_meta`/`_attachments` are
    // validated rather than stripped, and it is worth pinning because "strip the meta fields
    // before validating" is the natural wrong assumption.
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "defaulted",
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "count": { "type": "integer", "minimum": 0, "maximum": 9, "multipleOf": 1, "default": 0 },
        },
        "required": ["id"],
    }))
    .expect("fill");

    // A document WITHOUT the defaulted field is valid (it is not required)...
    let mut storage = ValidateStorage::new("memory");
    let without = json!({
        "id": "d", "_deleted": false, "_rev": "1-a", "_meta": { "lwt": 1 }, "_attachments": {}
    });
    assert_no_error(&mut storage, &schema, vec![row("d", without.clone())]);

    // ...and a document WITH the meta fields present is what the storage actually validates.
    // If the wrapper stripped them, the schema (which requires them after defaulting) would
    // reject every document.
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(required.contains(&"_deleted"));
    assert!(required.contains(&"_meta"));
}

// ---------------------------------------------------------------------------
// `validate.test.ts:715` — date-time format known by default
// ---------------------------------------------------------------------------

#[test]
fn upstream_715_the_date_time_format_is_available_by_default() {
    // `:715-766` ("#7253 should know about the date-time format by default by all validation
    // plugins"). A `format: 'date-time'` field must be validated without registering anything.
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "timed",
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "at": { "type": "string", "maxLength": 30, "format": "date-time" },
        },
        "required": ["id", "at"],
    }))
    .expect("fill");
    let mut storage = ValidateStorage::new("memory");
    let base = json!({ "id": "t", "_deleted": false, "_rev": "1-a", "_meta": { "lwt": 1 }, "_attachments": {} });

    let mut good = base.clone();
    good["at"] = json!("2024-02-29T12:00:00.000Z");
    assert_no_error(&mut storage, &schema, vec![row("t", good)]);

    let mut bad = base;
    bad["at"] = json!("not a date");
    assert_validation_error(&mut storage, &schema, vec![row("t", bad)], None);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:767` — invalid value must not persist after a failed update
// ---------------------------------------------------------------------------

#[test]
fn upstream_767_an_invalid_update_does_not_reach_the_storage() {
    // `:767-853` ("#734 Invalid value persists in document after failed update"). The property
    // is that a failing row is excluded from `accepted`, so there is nothing for the storage to
    // persist.
    let schema = human_schema();
    let mut storage = ValidateStorage::new("memory");
    let mut bad_update = human("x");
    bad_update["age"] = json!(-1);
    bad_update["_rev"] = json!("2-abc");
    let partition = storage
        .partition(
            &schema,
            vec![CandidateRow {
                document_id: "x".to_string(),
                document: bad_update,
                context: "validate-test".to_string(),
                previous: Some(human("x")),
            }],
        )
        .expect("partition");
    assert!(partition.accepted.is_empty(), "nothing may be persisted");
    assert_eq!(partition.rejected.len(), 1);
}

// ---------------------------------------------------------------------------
// `validate.test.ts:854` — additionalProperties data access
// ---------------------------------------------------------------------------

#[test]
fn upstream_854_an_object_with_additional_properties_validates() {
    // `:854-950` ("#5197 can't get data for object field defined with additionalProperties").
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "freeform",
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "anything": { "type": "object", "additionalProperties": true },
        },
        "required": ["id", "anything"],
    }))
    .expect("fill");
    let mut storage = ValidateStorage::new("memory");
    let document = json!({
        "id": "f",
        "anything": { "a": 1, "b": [1, 2], "c": { "d": null } },
        "_deleted": false, "_rev": "1-a", "_meta": { "lwt": 1 }, "_attachments": {}
    });
    assert_no_error(&mut storage, &schema, vec![row("f", document)]);
}

// ---------------------------------------------------------------------------
// The wrapper's own contract, beyond the upstream cases
// ---------------------------------------------------------------------------

#[test]
fn one_schema_produces_one_validator_across_collections() {
    // The cache, `plugin-helpers.ts:49-53`. Two collections with one schema must not build two
    // validators — and must produce identical errors.
    let schema = human_schema();
    let mut cache = ValidatorCache::new();
    let first = cache
        .get(&schema)
        .expect("first")
        .validate(&json!({ "age": 1 }));
    let second = cache
        .get(&schema)
        .expect("second")
        .validate(&json!({ "age": 1 }));
    assert_eq!(cache.len(), 1);
    assert_eq!(first.expect("one"), second.expect("two"));
}

#[test]
fn partition_preserves_the_order_of_accepted_rows() {
    // The surviving rows keep their batch order, which a storage relies on for its own
    // per-row response ordering.
    let schema = human_schema();
    let validator = SchemaValidator::new(&schema).expect("validator");
    let rows = vec![
        row("a", human("a")),
        row("bad", json!({})),
        row("b", human("b")),
        row("c", human("c")),
    ];
    let partition = partition_rows(&validator, &schema, rows);
    assert_eq!(
        partition
            .accepted
            .iter()
            .map(|row| row.document_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
}

#[test]
fn the_storage_name_round_trips_through_the_helper() {
    assert_eq!(
        wrapped_storage_name("schema", "indexeddb"),
        "validate-schema-indexeddb"
    );
    assert_eq!(
        ValidateStorage::new("indexeddb").name(),
        wrapped_storage_name("schema", "indexeddb")
    );
}
