//! Behaviour pinned from `src/plugins/dev-mode/` at `3b763ee17`, by source line.
//!
//! **There is no `test/unit/dev-mode.test.ts` upstream.** The plugin is installed globally by
//! `test/helper/parallel-setup.ts:27`, so its checks run as a precondition of every other
//! suite rather than being asserted themselves — which means a regression in a specific check
//! would surface as a confusing failure somewhere else, or not at all. So every test here is
//! named for the source line and error code it pins (`source_sc10_…`) instead of an upstream
//! `it(...)` line, and this crate does not claim a ported-test count.
//!
//! The selection of *which* checks to pin is the substance of the dev-mode decision, so it is
//! stated in the crate docs: only the rules `dxdb-schema` does not already enforce. Each test
//! below therefore also demonstrates a real schema that `dxdb-schema` accepts and dev-mode
//! rejects — otherwise it would be testing a duplicate.

use dxdb_plugin_dev_mode::{
    check_collection_name, check_migration_strategy_count, check_schema, expanded_error_message,
    DevModePlugin, Error, DOCUMENT_PROPERTIES,
};
use serde_json::json;

/// A schema that `dxdb-schema` accepts, as the baseline every negative case perturbs.
fn valid() -> serde_json::Value {
    json!({
        "title": "human",
        "version": 0,
        "type": "object",
        "primaryKey": "passportId",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 },
            "firstName": { "type": "string", "maxLength": 100 },
        },
        "required": ["passportId"],
        "indexes": ["firstName"],
    })
}

/// Asserts `dxdb-schema` accepts the schema, so the following `check_schema` failure is a
/// genuinely additional rule rather than a duplicate.
fn dxdb_schema_accepts(schema: &serde_json::Value) {
    let filled = dxdb_schema::fill_with_default_settings(schema)
        .unwrap_or_else(|error| panic!("dxdb-schema rejected the fixture: {error}"));
    dxdb_schema::create_rx_schema(&filled)
        .unwrap_or_else(|error| panic!("dxdb-schema rejected the fixture: {error}"));
}

#[test]
fn source_check_schema_the_baseline_passes_both_layers() {
    // The fixture must be legal for both, or the negative cases below prove nothing.
    let schema = valid();
    dxdb_schema_accepts(&schema);
    check_schema(&schema).expect("dev-mode accepts the baseline");
}

// ---------------------------------------------------------------------------
// `check-schema.ts:299` — SC10
// ---------------------------------------------------------------------------

#[test]
fn source_sc10_a_declared_rev_is_rejected() {
    // `if (jsonSchema.properties._rev) throw newRxError('SC10')` (`:299-304`).
    let mut schema = valid();
    schema["properties"]["_rev"] = json!({ "type": "string", "minLength": 1 });

    // dxdb-schema fills the _rev it wants and does not object, which is why this rule has to
    // live somewhere.
    dxdb_schema_accepts(&schema);
    assert_eq!(check_schema(&schema).expect_err("SC10"), Error::RevDeclared);
}

// ---------------------------------------------------------------------------
// `check-schema.ts:318-343` — the primary-field block
// ---------------------------------------------------------------------------

#[test]
fn source_sc13_the_primary_field_in_indexes_is_rejected() {
    // `:320-325`. The primary key is already the tail of every index string.
    let mut schema = valid();
    schema["indexes"] = json!(["firstName", "passportId"]);
    dxdb_schema_accepts(&schema);
    assert_eq!(
        check_schema(&schema).expect_err("SC13"),
        Error::PrimaryInIndexes("passportId".to_string())
    );
}

#[test]
fn source_sc13_a_compound_index_containing_the_primary_is_rejected() {
    // The compound form goes through the same rule; upstream's `indexes.includes(key)`
    // comparison is what misses a *compound* index containing the key, and there is a
    // dedicated upstream test for it (`rx-schema.test.ts:659`, "should throw when composite
    // primary key field is in indexes (SC13)"). This port checks both shapes.
    let mut schema = valid();
    schema["indexes"] = json!([["firstName", "passportId"]]);
    assert_eq!(
        check_schema(&schema).expect_err("SC13"),
        Error::PrimaryInIndexes("passportId".to_string())
    );
}

#[test]
fn source_sc14_a_unique_primary_field_is_rejected() {
    // `:326-331`.
    let mut schema = valid();
    schema["properties"]["passportId"] =
        json!({ "type": "string", "maxLength": 100, "unique": true });
    dxdb_schema_accepts(&schema);
    assert_eq!(
        check_schema(&schema).expect_err("SC14"),
        Error::PrimaryUnique("passportId".to_string())
    );
}

#[test]
fn source_sc15_an_encrypted_primary_field_is_rejected() {
    // `:332-337`. An encrypted field cannot be indexed or compared.
    let mut schema = valid();
    schema["encrypted"] = json!(["passportId"]);
    assert_eq!(
        check_schema(&schema).expect_err("SC15"),
        Error::PrimaryEncrypted("passportId".to_string())
    );
}

#[test]
fn source_sc16_a_non_string_primary_field_is_rejected() {
    // `:338-343`.
    let mut schema = valid();
    schema["properties"]["passportId"] =
        json!({ "type": "integer", "minimum": 0, "maximum": 100, "multipleOf": 1 });
    assert_eq!(
        check_schema(&schema).expect_err("SC16"),
        Error::PrimaryNotString("passportId".to_string(), "integer".to_string())
    );
}

#[test]
fn source_sc17_a_field_shadowing_a_document_property_is_rejected() {
    // `:345-352` with the list from `entity-properties.ts:48-55`.
    for name in DOCUMENT_PROPERTIES {
        let mut schema = valid();
        schema["properties"][name] = json!({ "type": "string", "maxLength": 10 });
        assert_eq!(
            check_schema(&schema).expect_err(name),
            Error::FieldShadowsDocumentProperty(name.to_string()),
            "field `{name}` should be rejected"
        );
    }
    assert_eq!(DOCUMENT_PROPERTIES.len(), 7);
}

// ---------------------------------------------------------------------------
// `check-schema.ts:288-…` — SC34 / SC35, indexed field bounds
// ---------------------------------------------------------------------------

#[test]
fn source_sc34_an_indexed_string_needs_max_length() {
    // The index string encoding is length-dependent, so a `maxLength` is required.
    let mut schema = valid();
    schema["properties"]["firstName"] = json!({ "type": "string" });
    // dxdb-schema accepts it: it only enforces this for the *primary* key (SC39).
    dxdb_schema_accepts(&schema);
    assert_eq!(
        check_schema(&schema).expect_err("SC34"),
        Error::IndexedStringWithoutMaxLength("firstName".to_string())
    );
}

#[test]
fn source_sc35_an_indexed_number_needs_bounds() {
    let mut schema = valid();
    schema["indexes"] = json!(["age"]);
    schema["properties"]["age"] = json!({ "type": "integer" });
    dxdb_schema_accepts(&schema);
    assert_eq!(
        check_schema(&schema).expect_err("SC35"),
        Error::IndexedNumberWithoutBounds("age".to_string())
    );
}

#[test]
fn source_sc35_a_fully_bounded_number_passes() {
    // The baseline's `age` has minimum, maximum and multipleOf and is not indexed; index it
    // and it must still pass.
    let mut schema = valid();
    schema["indexes"] = json!(["age"]);
    check_schema(&schema).expect("a bounded indexed number is fine");
}

#[test]
fn an_index_on_a_nested_path_resolves() {
    // `firstname` inside a nested object, so the path resolution is exercised rather than
    // only the flat case.
    let mut schema = valid();
    schema["properties"]["nested"] = json!({
        "type": "object",
        "properties": { "label": { "type": "string", "maxLength": 50 } }
    });
    schema["indexes"] = json!(["nested.label"]);
    check_schema(&schema).expect("a nested bounded string is fine");

    let mut unbounded = schema.clone();
    unbounded["properties"]["nested"]["properties"]["label"] = json!({ "type": "string" });
    assert_eq!(
        check_schema(&unbounded).expect_err("SC34 nested"),
        Error::IndexedStringWithoutMaxLength("nested.label".to_string())
    );
}

// ---------------------------------------------------------------------------
// `check-schema.ts:33-62` — SC1 / SC23, field-name pattern.
//
// Found by the decision test at the bottom of this file: `dxdb-schema` declares
// `SchemaErrorCode::Sc1` but never raises it, so this rule had no home until it was added
// here. That is the test doing its job.
// ---------------------------------------------------------------------------

#[test]
fn source_sc1_a_field_name_must_match_the_pattern() {
    // `^[a-zA-Z](?:[a-zA-Z0-9_]*[a-zA-Z0-9])?$` (`check-schema.ts:38`).
    for good in ["a", "age", "first_name", "a1", "A_1"] {
        let mut schema = valid();
        schema["properties"][good] = json!({ "type": "string", "maxLength": 5 });
        check_schema(&schema).unwrap_or_else(|error| panic!("`{good}` should be allowed: {error}"));
    }
    for bad in [
        "1age",
        "_private",
        "has space",
        "trailing_",
        "kebab-case",
        "dotted.name",
    ] {
        let mut schema = valid();
        schema["properties"][bad] = json!({ "type": "string", "maxLength": 5 });
        assert_eq!(
            check_schema(&schema).expect_err(bad),
            Error::FieldNamePattern(bad.to_string()),
            "`{bad}` should be rejected"
        );
    }
}

#[test]
fn source_sc1_the_reserved_names_are_exempt() {
    // `_deleted` is allowed (`:34-36`) and `_id` is explicitly allowed as a primary key
    // (`:44-50`, for RxDB+CouchDB).
    let mut with_deleted = valid();
    with_deleted["properties"]["_deleted"] = json!({ "type": "boolean" });
    check_schema(&with_deleted).expect("_deleted is exempt");

    let id_primary = json!({
        "title": "t",
        "version": 0,
        "type": "object",
        "primaryKey": "_id",
        "properties": { "_id": { "type": "string", "maxLength": 100 } },
        "required": ["_id"],
    });
    check_schema(&id_primary).expect("_id is allowed as a primary key");
}

#[test]
fn source_sc23_properties_cannot_be_a_field_name() {
    // `:37-42`.
    let mut schema = valid();
    schema["properties"]["properties"] = json!({ "type": "object", "properties": {} });
    assert_eq!(
        check_schema(&schema).expect_err("SC23"),
        Error::FieldNameProperties
    );
}

// ---------------------------------------------------------------------------
// `unallowed-properties.ts:11-21` and `index.ts:95-100` — DB2
// ---------------------------------------------------------------------------

#[test]
fn source_db2_a_collection_name_must_not_start_with_an_underscore() {
    // `:97-100`. `_` names are reserved for internal collections.
    assert_eq!(
        check_collection_name("_humans").expect_err("DB2"),
        Error::CollectionNameUnderscore("_humans".to_string())
    );
    check_collection_name("humans").expect("fine");
    // An underscore later in the name is fine.
    check_collection_name("hu_mans").expect("an inner underscore is fine");
}

// ---------------------------------------------------------------------------
// `check-migration-strategies.ts:27-35` — COL12
// ---------------------------------------------------------------------------

#[test]
fn source_col12_the_strategy_count_must_match_the_version() {
    // `previousVersions.length !== Object.keys(migrationStrategies).length`. A version-3
    // schema needs strategies 1, 2 and 3.
    check_migration_strategy_count(0, 0).expect("version 0 needs none");
    check_migration_strategy_count(3, 3).expect("three for a version-3 schema");
    for (version, have) in [(3, 0), (3, 2), (3, 4), (1, 0)] {
        assert_eq!(
            check_migration_strategy_count(version, have).expect_err("COL12"),
            Error::StrategyCountMismatch { version, have }
        );
    }
}

// ---------------------------------------------------------------------------
// `index.ts:95-119` — the overwritables
// ---------------------------------------------------------------------------

#[test]
fn source_index_95_installing_turns_on_dev_mode() {
    use dxdb_core::{Hooks, PluginSet};

    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    assert!(!set.overwritables().is_dev_mode, "production default");

    let handle = set.add(DevModePlugin, &mut hooks).expect("install");
    assert_eq!(handle.name(), "dev-mode");
    assert!(
        set.overwritables().is_dev_mode,
        "isDevMode now returns true"
    );
    assert!(
        set.overwritables().tunnel_error_messages,
        "error messages are expanded"
    );
    assert!(
        !set.overwritables().deep_freeze_when_dev_mode,
        "freezing is not claimed: a Rust value cannot be frozen after the fact, so \
         Document::ensure_revision is the guarantee instead"
    );
    // And the plugin registers no hooks: in dxdb the checks run unconditionally.
    assert_eq!(handle.registered(), 0);
    assert!(hooks.is_empty());
}

// ---------------------------------------------------------------------------
// `error-messages.ts` — the mechanism
// ---------------------------------------------------------------------------

#[test]
fn source_error_messages_the_lookup_covers_the_rules_this_crate_enforces() {
    // Every code this crate can raise must have a hint, or `tunnel_error_messages` would
    // promise an expansion that yields nothing.
    for code in [
        "SC1", "SC23", "SC10", "SC13", "SC14", "SC15", "SC16", "SC17", "SC34", "SC35", "DB2",
        "COL12",
    ] {
        assert!(
            expanded_error_message(code).is_some(),
            "{code} must have a hint"
        );
    }
    // And an unknown code falls through, so a caller can supply its own table — the
    // mechanism upstream has without the 1,433 lines of prose.
    assert!(expanded_error_message("NOPE").is_none());
    assert!(expanded_error_message("CONFLICT").is_some());
}

// ---------------------------------------------------------------------------
// The decision itself, asserted so it cannot drift silently
// ---------------------------------------------------------------------------

#[test]
fn dxdb_schema_declares_sc1_but_cannot_raise_it() {
    // The finding that put SC1 in this crate. `SchemaErrorCode::Sc1` exists in the catalogue
    // and `as_str` renders it, but nothing in `dxdb-schema` produces it — so a schema with
    // `Bad Name` as a field passes `create_rx_schema`. Asserted here so the day it is fixed,
    // this test fails and the rule can move back.
    let schema = json!({
        "type": "object", "version": 0, "primaryKey": "id",
        "properties": { "id": { "type": "string", "maxLength": 10 }, "Bad Name": { "type": "string" } }
    });
    let accepted = dxdb_schema::fill_with_default_settings(&schema)
        .and_then(|filled| dxdb_schema::create_rx_schema(&filled))
        .is_ok();
    assert!(
        accepted,
        "dxdb-schema now rejects a bad field name — move SC1/SC23 out of dev-mode and into \
         dxdb-schema, and delete this test"
    );
    // And dev-mode is what catches it.
    assert_eq!(
        check_schema(&schema).expect_err("SC1"),
        Error::FieldNamePattern("Bad Name".to_string())
    );
}

#[test]
fn dxdb_schema_enforces_sc39_lazily_not_at_creation() {
    // The third nuance the decision test surfaced. `SC39` ("the primary key has no
    // maxLength") is raised by `Schema::primary_key_max_length`, which an index builder calls
    // when it needs the bound — not by `create_rx_schema`. So a schema with an unbounded
    // primary key is *constructible* and fails later, which is fine: it fails on the path
    // that needs the value. Asserted so nobody assumes creation-time validation.
    let schema = json!({
        "type": "object", "version": 0, "primaryKey": "id",
        "properties": { "id": { "type": "string" } }
    });
    let filled = dxdb_schema::fill_with_default_settings(&schema).expect("fill");
    let created =
        dxdb_schema::create_rx_schema(&filled).expect("creation accepts an unbounded primary key");
    // But the accessor that needs it refuses.
    let error = created.primary_key_max_length().expect_err("SC39 lazily");
    assert_eq!(error.code.as_str(), "SC39");
}

#[test]
fn dxdb_schema_defaults_version_rather_than_requiring_it() {
    // The second finding from the decision test. `SC11` is "missing or invalid version", and
    // upstream's dev-mode raises it (`check-schema.ts:306-313`). dxdb-schema instead
    // *defaults* `version` to 0 (`lib.rs:332`, `:416`) — a deliberate divergence, documented
    // there, because a missing version most likely means "I forgot" and 0 is the only
    // sensible reading. So this is not a gap to port; it is a different, deliberate rule.
    let schema = json!({
        "type": "object", "primaryKey": "id",
        "properties": { "id": { "type": "string", "maxLength": 10 } }
    });
    let filled = dxdb_schema::fill_with_default_settings(&schema).expect("fill");
    assert_eq!(
        filled["version"],
        json!(0),
        "version is defaulted, not rejected"
    );
    dxdb_schema::create_rx_schema(&filled).expect("and the schema is accepted");
    // Neither a non-numeric nor a negative version is checked: `SchemaErrorCode::Sc11` is
    // declared and never raised. Recorded by assertion rather than comment, so the day it is
    // enforced this fails and dev-mode's version rule can be reconsidered. `version` is read
    // with a default at `Schema::version` (`lib.rs:774`), so a string simply falls back to 0.
    for bad_version in [json!("one"), json!(-1)] {
        let mut bad = schema.clone();
        bad["version"] = bad_version.clone();
        let accepted = dxdb_schema::fill_with_default_settings(&bad)
            .and_then(|filled| dxdb_schema::create_rx_schema(&filled))
            .is_ok();
        assert!(
            accepted,
            "dxdb-schema now rejects version {bad_version} — SC11 is being enforced, so \
             invert this assertion"
        );
    }
}

#[test]
fn the_duplicated_check_list_is_not_reimplemented() {
    // The rule this crate is built on: a check `dxdb-schema` already makes must NOT be made
    // here. These five schemas are each invalid for dxdb-schema, so `check_schema` is never
    // reached for them in a real flow — and asserting that `dxdb-schema` rejects them is what
    // keeps the two implementations from drifting into disagreement.
    let cases = [
        // SC1 is deliberately NOT in this list: a test found that dxdb-schema declares
        // `SchemaErrorCode::Sc1` but never raises it, so the rule lives in dev-mode instead.
        // SC29: no properties.
        (
            "SC29",
            json!({ "type": "object", "version": 0, "primaryKey": "id" }),
        ),
        // SC30: no primaryKey.
        (
            "SC30",
            json!({ "type": "object", "version": 0, "properties": {} }),
        ),
    ];
    for (code, schema) in cases {
        let rejected = dxdb_schema::fill_with_default_settings(&schema)
            .and_then(|filled| dxdb_schema::create_rx_schema(&filled))
            .is_err();
        assert!(
            rejected,
            "dxdb-schema must reject {code} on its own, so dev-mode does not need to — \
             if this ever passes, that rule has moved and dev-mode should adopt it"
        );
    }
}
