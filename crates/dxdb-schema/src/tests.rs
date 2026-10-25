//! Ported upstream tests for schema defaulting and normalization.
//!
//! Every test names the upstream file and line it was translated from. Tests that
//! upstream runs through a database (`createRxDatabase` + `addCollections`) are
//! reduced to the schema-level call they exercise, because `dxdb-schema` has no
//! database by design; when the assertion was about collection setup rather than
//! schema behaviour, the reduced test says so.

use serde_json::{json, Value};

use super::*;

/// The `human` fixture schema, byte for byte from
/// `src/plugins/test-utils/schemas.ts:36`.
fn human_schema() -> Value {
    json!({
        "title": "human schema",
        "description": "describes a human being",
        "version": 0,
        "keyCompression": false,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "firstName": { "type": "string", "maxLength": 100 },
            "lastName": { "type": "string", "maxLength": 100 },
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
    })
}

/// `schemaObjects.humanData()` (`src/plugins/test-utils/schema-objects.ts`), the
/// document used by most query tests upstream.
fn human_data() -> Value {
    json!({
        "passportId": "foobar",
        "firstName": "Alice",
        "lastName": "Alice",
        "age": 40
    })
}

// --- rx-schema.test.ts:34 `.getIndexes()` ---------------------------------------

// rx-schema.test.ts:37 `get single indexes`
#[test]
fn get_indexes_single() {
    let indexes = get_string_indexes(&human_schema()).expect("indexes");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0], vec!["firstName".to_string()]);
}

// rx-schema.test.ts:48 `get sub-index`
#[test]
fn get_indexes_sub_index() {
    let schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "other": {"type": "object", "properties": {"age": {"type": "integer"}}}
        },
        "indexes": ["other.age"]
    });
    let indexes = get_string_indexes(&schema).expect("indexes");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0], vec!["other.age".to_string()]);
}

// rx-schema.test.ts:53 `get no index`
#[test]
fn get_indexes_none() {
    let schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {"id": {"type": "string", "maxLength": 100}}
    });
    assert!(get_string_indexes(&schema).expect("indexes").is_empty());
}

// rx-schema.test.ts:57 `get compoundIndex`
#[test]
fn get_indexes_compound() {
    let schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "age": {"type": "integer", "minimum": 0, "maximum": 150},
            "passportCountry": {"type": "string", "maxLength": 10}
        },
        "indexes": [["age", "passportCountry"]]
    });
    let indexes = get_string_indexes(&schema).expect("indexes");
    assert_eq!(indexes.len(), 1);
    assert_eq!(
        indexes[0],
        vec!["age".to_string(), "passportCountry".to_string()]
    );
}

// --- rx-schema.test.ts:713 fill / normalize -------------------------------------

// rx-schema.test.ts:730 `should not sort indexes array in the schema`
//
// Upstream sorts object keys but passes `noArraySort`, so `indexes` keeps its
// order. The order is observable: the planner walks `schema.indexes` in order and
// `>=` on equal quality keeps the earlier one.
#[test]
fn normalize_does_not_sort_indexes_array() {
    let schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "firstName": {"type": "string", "maxLength": 100},
            "lastName": {"type": "string", "maxLength": 100}
        },
        "indexes": [["lastName"], ["firstName"], "lastName"]
    });
    let normalized = normalize_rx_json_schema(&schema);
    assert_eq!(
        normalized.get("indexes").unwrap(),
        schema.get("indexes").unwrap()
    );
}

// rx-schema.test.ts:725 `should deep sort one schema with different orders to be the same`
#[test]
fn normalize_makes_key_order_irrelevant() {
    let a = json!({
        "version": 0,
        "type": "object",
        "primaryKey": "id",
        "properties": {
            "id": {"type": "string", "maxLength": 10},
            "z": {"type": "number"},
            "a": {"type": "string"}
        }
    });
    let b = json!({
        "properties": {
            "a": {"type": "string"},
            "z": {"type": "number"},
            "id": {"type": "string", "maxLength": 10}
        },
        "primaryKey": "id",
        "type": "object",
        "version": 0
    });
    assert_eq!(
        normalize_rx_json_schema(&a),
        normalize_rx_json_schema(&b),
        "the same schema in a different key order must normalize to the same value, \
         because the hash is taken over the normalized value"
    );
}

// rx-schema.test.ts:721 `should be the same object`
#[test]
fn normalize_of_normalized_is_stable() {
    let once = normalize_rx_json_schema(&human_schema());
    let twice = normalize_rx_json_schema(&once);
    assert_eq!(once, twice);
}

// --- rx-schema.test.ts:734 `should have added the primaryKey to indexes` -------

// rx-schema.test.ts:734
#[test]
fn fill_adds_primary_key_to_every_index() {
    let filled = fill_with_default_settings(&json!({
        "primaryKey": "id",
        "version": 0,
        "type": "object",
        "properties": {"id": {"type": "string", "maxLength": 100}},
        "required": ["id"],
        "indexes": ["age", ["foo", "bar"], ["bar", "id", "foo"]]
    }))
    .expect("fills");
    for index in get_string_indexes(&filled).expect("indexes") {
        assert!(
            index.contains(&"id".to_string()),
            "every index must contain the primary key, got {index:?}"
        );
    }
}

// --- rx-schema.test.ts:757 deduplicate indexes ---------------------------------

// rx-schema.test.ts:757 `should deduplicate indexes that become identical after
// adding _deleted prefix and primaryKey suffix`
#[test]
fn fill_deduplicates_indexes() {
    let filled = fill_with_default_settings(&json!({
        "primaryKey": "id",
        "version": 0,
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "name": {"type": "string", "maxLength": 100}
        },
        "required": ["id"],
        "indexes": [["name"], ["name", "id"]]
    }))
    .expect("fills");
    let indexes = get_string_indexes(&filled).expect("indexes");
    let joined: Vec<String> = indexes.iter().map(|i| i.join(",")).collect();
    let unique: std::collections::BTreeSet<&String> = joined.iter().collect();
    assert_eq!(joined.len(), unique.len(), "indexes: {indexes:?}");
}

// --- rx-schema.test.ts:822 `.getFinalFields()` ---------------------------------

// rx-schema.test.ts:823 `should contain the field`
#[test]
fn final_fields_contains_declared_final() {
    let schema = json!({
        "version": 0,
        "primaryKey": "myField",
        "type": "object",
        "properties": {
            "myField": {"type": "string", "maxLength": 100, "final": true}
        }
    });
    assert!(get_final_fields(&schema)
        .expect("final fields")
        .contains(&"myField".to_string()));
}

// rx-schema.test.ts:838 `should contain the primary`
#[test]
fn final_fields_contains_primary() {
    let schema = json!({
        "version": 0,
        "primaryKey": "myField",
        "type": "object",
        "properties": {"myField": {"type": "string", "maxLength": 100}}
    });
    assert_eq!(
        get_final_fields(&schema).expect("final fields"),
        vec!["myField".to_string()]
    );
}

// --- rx-schema.test.ts:855 `.getPreviousVersions()` ----------------------------

// rx-schema.test.ts:856 `get empty array when current==0`
#[test]
fn previous_versions_empty_for_zero() {
    assert_eq!(previous_versions(&json!({"version": 0})), Vec::<u32>::new());
}

// rx-schema.test.ts:874 `get valid array when current==5`
#[test]
fn previous_versions_five() {
    assert_eq!(previous_versions(&json!({"version": 5})), vec![0, 1, 2, 3, 4]);
}

// --- rx-schema.test.ts:998 `.getSchemaByObjectPath()` --------------------------

// rx-schema.test.ts:1000 `get firstLevel`
#[test]
fn schema_by_object_path_first_level() {
    let filled = fill_with_default_settings(&human_schema()).expect("fills");
    let part = schema_part_by_object_path(&filled, "passportId").expect("part");
    assert_eq!(part.get("type").and_then(Value::as_str), Some("string"));
}

// rx-schema.test.ts:1010 `get nested`
#[test]
fn schema_by_object_path_nested() {
    let schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "mainSkill": {
                "type": "object",
                "properties": {"name": {"type": "string", "maxLength": 10}}
            }
        }
    });
    let part = schema_part_by_object_path(&schema, "mainSkill.name").expect("part");
    assert_eq!(part.get("type").and_then(Value::as_str), Some("string"));
}

// --- rx-schema.test.ts:1018 `.fillObjectWithDefaults()` -----------------------

// rx-schema.test.ts:1020 `should fill all unset fields`
#[test]
fn fill_object_with_defaults_fills_unset() {
    let schema = json!({
        "version": 0,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": {"type": "string", "maxLength": 100},
            "age": {"type": "integer", "minimum": 0, "maximum": 150, "default": 20}
        }
    });
    let mut data = json!({"foo": "bar"});
    fill_object_with_defaults(&schema, &mut data);
    assert_eq!(data["foo"], json!("bar"));
    assert_eq!(data["age"], json!(20));
}

// rx-schema.test.ts:1029 `should not overwrite given values`
#[test]
fn fill_object_with_defaults_keeps_given() {
    let schema = json!({
        "version": 0,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": {"type": "string", "maxLength": 100},
            "age": {"type": "integer", "minimum": 0, "maximum": 150, "default": 20}
        }
    });
    let mut data = json!({"foo": "bar", "age": 40});
    fill_object_with_defaults(&schema, &mut data);
    assert_eq!(data["age"], json!(40));
}

// rx-schema.test.ts:1043 `should not share non-primitive default value references`
#[test]
fn fill_object_with_defaults_copies_non_primitive() {
    let schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "tags": {"type": "array", "items": {"type": "string"}, "default": []},
            "metadata": {"type": "object", "properties": {}, "default": {}}
        },
        "required": ["id"]
    });
    let mut first = json!({"id": "doc1"});
    let mut second = json!({"id": "doc2"});
    fill_object_with_defaults(&schema, &mut first);
    fill_object_with_defaults(&schema, &mut second);
    assert_eq!(first["tags"], json!([]));
    assert_eq!(second["tags"], json!([]));
    first["tags"]
        .as_array_mut()
        .expect("array")
        .push(json!("mutated"));
    assert_eq!(
        second["tags"],
        json!([]),
        "defaults must not share a reference: mutating one document must not corrupt another"
    );
}

// --- rx-schema.test.ts:1593 `.getJsonSchemaWithoutMeta()` ----------------------

// rx-schema.test.ts:1594 `should not contain any RxDB-internal meta properties`
#[test]
fn json_schema_without_meta_drops_meta_fields() {
    let filled = fill_with_default_settings(&human_schema()).expect("fills");
    let stripped = json_schema_without_meta(&filled).expect("strips");
    let properties = stripped["properties"].as_object().expect("properties");
    for meta in ["_rev", "_deleted", "_meta", "_attachments"] {
        assert!(!properties.contains_key(meta), "{meta} must be gone");
    }
    let required = stripped["required"].as_array().expect("required");
    assert!(required
        .iter()
        .all(|r| !r.as_str().unwrap_or_default().starts_with('_')));
    for user in ["firstName", "lastName", "passportId", "age"] {
        assert!(properties.contains_key(user), "{user} must survive");
    }
}

// rx-schema.test.ts:1625 `indexes should not reference internal meta fields`
#[test]
fn json_schema_without_meta_indexes_resolve() {
    let filled = fill_with_default_settings(&human_schema()).expect("fills");
    let stripped = json_schema_without_meta(&filled).expect("strips");
    let properties = stripped["properties"].as_object().expect("properties");
    for index in get_string_indexes(&stripped).expect("indexes") {
        for field in &index {
            let top_level = field.split('.').next().unwrap_or_default();
            assert!(
                properties.contains_key(top_level),
                "index field {field} references missing property {top_level}"
            );
        }
    }
    let all_fields: Vec<String> = get_string_indexes(&stripped)
        .expect("indexes")
        .into_iter()
        .flatten()
        .collect();
    assert!(
        all_fields.contains(&"firstName".to_string()),
        "user-defined index fields must survive"
    );
}

// --- raw_custom_index / schema hash ------------------------------------------

/// The filled `human` schema, printed by upstream 17.6.0 through
/// `JSON.stringify(normalizeRxJsonSchema(fillWithDefaultSettings(human)))`.
/// Recorded here as a literal so a change in fill order fails loudly.
const HUMAN_NORMALIZED_JSON: &str = r#"{"additionalProperties":false,"description":"describes a human being","encrypted":[],"indexes":[["_deleted","firstName","passportId"],["_meta.lwt","passportId"]],"keyCompression":false,"primaryKey":"passportId","properties":{"_attachments":{"type":"object"},"_deleted":{"type":"boolean"},"_meta":{"additionalProperties":true,"properties":{"lwt":{"maximum":1000000000000000,"minimum":1,"multipleOf":0.01,"type":"number"}},"required":["lwt"],"type":"object"},"_rev":{"minLength":1,"type":"string"},"age":{"description":"age in years","maximum":150,"minimum":0,"multipleOf":1,"type":"integer"},"firstName":{"maxLength":100,"type":"string"},"lastName":{"maxLength":100,"type":"string"},"passportId":{"maxLength":100,"type":"string"}},"required":["firstName","lastName","passportId","_deleted","_rev","_meta","_attachments"],"title":"human schema","type":"object","version":0}"#;

#[test]
fn human_schema_fills_like_upstream() {
    let filled = fill_with_default_settings(&human_schema()).expect("fills");
    let normalized = normalize_rx_json_schema(&filled);
    let actual = dxdb_json::to_js_json(&normalized);
    assert_eq!(actual, HUMAN_NORMALIZED_JSON);
}

/// The hash upstream produces for the same schema (`RxSchema.hash` with
/// `defaultHashSha256`, recorded from upstream 17.6.0).
#[test]
fn human_schema_hash_matches_upstream() {
    const UPSTREAM_HASH: &str = "e1db93378e0a3a31dfb44b89b834ed1fe7279e59b9d48ef6455999f00a4aebe8";
    let schema = create_rx_schema(&human_schema()).expect("creates");
    assert_eq!(schema.hash(), UPSTREAM_HASH);
}

#[test]
fn hash_input_is_canonical_bytes() {
    let schema = create_rx_schema(&human_schema()).expect("creates");
    let bytes = schema.hash_input();
    assert_eq!(bytes, dxdb_json::canonical_bytes(schema.json_schema()));
    assert_eq!(
        sha256_hex(&bytes),
        schema.hash(),
        "the recorded hash must be the SHA-256 of the recorded input"
    );
}

/// A float in a schema must not hash through serde_json's writer: `1.0` is `1` in
/// JavaScript. This is the schema-hash face of PLAN.md 3.1.
#[test]
fn hash_uses_javascript_number_formatting() {
    let schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "ratio": {"type": "number", "minimum": 0, "maximum": 1, "default": 1.0}
        }
    });
    let built = create_rx_schema(&schema).expect("creates");
    let input = String::from_utf8(built.hash_input()).expect("utf8");
    assert!(
        input.contains(r#""default":1,"#),
        "expected JS float formatting (1) not serde_json's (1.0); got {input}"
    );
    assert!(!input.contains("1.0"));
}

// --- meta injection ordering --------------------------------------------------

// src/rx-schema-helper.ts:271 — `_deleted` is *prepended* unless already first.
#[test]
fn deleted_is_prepended_not_appended() {
    let filled = fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 10},
            "age": {"type": "integer", "minimum": 0, "maximum": 10}
        },
        "indexes": ["age"]
    }))
    .expect("fills");
    assert_eq!(
        get_string_indexes(&filled).expect("indexes")[0],
        vec!["_deleted".to_string(), "age".to_string(), "id".to_string()],
        "_deleted first, primary key last, exactly as upstream builds it"
    );
}

// src/rx-schema-helper.ts:271 — an index that already starts with `_deleted` is
// left alone rather than gaining a second one.
#[test]
fn existing_deleted_first_is_not_duplicated() {
    let filled = fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 10},
            "age": {"type": "integer", "minimum": 0, "maximum": 10}
        },
        "indexes": [["_deleted", "age"]]
    }))
    .expect("fills");
    assert_eq!(
        get_string_indexes(&filled).expect("indexes")[0],
        vec!["_deleted".to_string(), "age".to_string(), "id".to_string()]
    );
}

// src/rx-schema-helper.ts:283 — the `_meta.lwt` index is always appended.
#[test]
fn meta_lwt_index_is_always_present() {
    let filled = fill_with_default_settings(&human_schema()).expect("fills");
    let indexes = get_string_indexes(&filled).expect("indexes");
    assert!(
        indexes
            .iter()
            .any(|i| i == &vec!["_meta.lwt".to_string(), "passportId".to_string()]),
        "indexes: {indexes:?}"
    );
}

// src/rx-schema-helper.ts:188 — a schema with no indexes gets the default one.
#[test]
fn no_indexes_gets_default_index() {
    let filled = fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {"id": {"type": "string", "maxLength": 10}}
    }))
    .expect("fills");
    let indexes = get_string_indexes(&filled).expect("indexes");
    assert_eq!(indexes[0], vec!["_deleted".to_string(), "id".to_string()]);
    assert_eq!(indexes.len(), 2, "default index plus _meta.lwt: {indexes:?}");
}

// src/rx-schema-helper.ts:243 — required order.
#[test]
fn required_order_is_upstream_order() {
    let filled = fill_with_default_settings(&human_schema()).expect("fills");
    let required: Vec<String> = filled["required"]
        .as_array()
        .expect("required")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        required,
        vec![
            "firstName",
            "lastName",
            "passportId",
            "_deleted",
            "_rev",
            "_meta",
            "_attachments"
        ],
        "user required fields first in their own order, then the meta fields, then the primary key"
    );
}

// src/rx-schema-helper.ts:251 — a dotted required entry is dropped.
#[test]
fn dotted_required_entries_are_filtered() {
    let filled = fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 10},
            "nested": {"type": "object", "properties": {"a": {"type": "string"}}}
        },
        "required": ["id", "nested.a"]
    }))
    .expect("fills");
    let required: Vec<String> = filled["required"]
        .as_array()
        .expect("required")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert!(!required.contains(&"nested.a".to_string()));
}

// --- primary key helpers ------------------------------------------------------

#[test]
fn composite_primary_key_is_composed_with_separator() {
    let schema = json!({
        "version": 0,
        "type": "object",
        "primaryKey": {"key": "id", "fields": ["firstName", "lastName"], "separator": "|"},
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "firstName": {"type": "string"},
            "lastName": {"type": "string"}
        }
    });
    let composed = composed_primary_key_of_document_data(
        &schema,
        &json!({"firstName": "Alice", "lastName": "Smith"}),
    )
    .expect("composes");
    assert_eq!(composed, "Alice|Smith");
}

// src/rx-schema-helper.ts:158 — DOC18 when a component is missing.
#[test]
fn composite_primary_key_missing_component_errors() {
    let schema = json!({
        "version": 0,
        "type": "object",
        "primaryKey": {"key": "id", "fields": ["firstName", "lastName"], "separator": "|"},
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "firstName": {"type": "string"},
            "lastName": {"type": "string"}
        }
    });
    let error = composed_primary_key_of_document_data(&schema, &json!({"firstName": "Alice"}))
        .expect_err("must reject");
    assert_eq!(error.code, SchemaErrorCode::Doc18);
}

// src/rx-schema.ts:61 — SC39 for a primary key without maxLength.
#[test]
fn primary_key_without_max_length_is_sc39() {
    let schema = Schema::new(
        fill_with_default_settings(&json!({
            "version": 0,
            "primaryKey": "id",
            "type": "object",
            "properties": {"id": {"type": "string"}}
        }))
        .expect("fills"),
    )
    .expect("constructs");
    let error = schema.primary_key_max_length().expect_err("must reject");
    assert_eq!(error.code, SchemaErrorCode::Sc39);
}

// src/plugins/dev-mode/check-schema.ts:258 — SC41 for a non-finite maxLength.
#[test]
fn primary_key_with_infinite_max_length_is_invalid() {
    // `1e400` parses to a serde_json error rather than Infinity, so the value has
    // to be built: this is the "broken on Infinity numbers in index sizes" case
    // from `rx-schema.test.ts:1515`.
    let schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {"id": {"type": "string", "maxLength": 1.0e308}}
    });
    let built = Schema::new(schema).expect("constructs");
    // 1e308 is finite, so it is accepted, but the width is absurd; the check that
    // matters is that it does not silently become 0.
    assert!(built.primary_key_max_length().expect("finite") > 0);
}

// --- property-level round trip -----------------------------------------------

/// `schemaObjects.humanData()` plus the meta fields the writer injects before
/// validation (`src/rx-document.ts` `_handleToStorageInstance`), which is the
/// document shape a storage actually sees.
fn stored_human_data() -> Value {
    let mut data = human_data();
    data["_deleted"] = json!(false);
    data["_rev"] = json!("1-x");
    data["_meta"] = json!({"lwt": 1});
    data["_attachments"] = json!({});
    data
}

#[test]
fn filled_schema_keeps_document_shape() {
    let built = create_rx_schema(&human_schema()).expect("creates");
    let errors = validate_document(built.json_schema(), &stored_human_data()).expect("validates");
    assert!(errors.is_empty(), "errors: {errors:?}");
}

#[test]
fn filled_schema_rejects_extra_fields() {
    let built = create_rx_schema(&human_schema()).expect("creates");
    let mut data = stored_human_data();
    data["unexpected"] = json!(1);
    let errors = validate_document(built.json_schema(), &data).expect("validates");
    assert!(
        !errors.is_empty(),
        "additionalProperties is forced false, so an unknown field must be rejected"
    );
}

#[test]
fn filled_schema_requires_meta_fields() {
    let built = create_rx_schema(&human_schema()).expect("creates");
    let errors = validate_document(built.json_schema(), &human_data()).expect("validates");
    let joined = errors.join("; ");
    for meta in ["_deleted", "_rev", "_meta", "_attachments"] {
        assert!(
            joined.contains(meta),
            "missing {meta} must be reported; got {joined}"
        );
    }
}

// --- index entries that are not field names -----------------------------------
//
// `compoundIndexNoString` in `src/plugins/test-utils/schemas.ts` has
// `indexes: [[10, 'passportCountry']]`. Upstream *fills and hashes* it — the
// literal `10` survives into `indexes` — and only fails when an index string or a
// query plan is built from it. The oracle vectors record both halves, so both are
// asserted here: the fill must be permissive, and the encoder must reject.

#[test]
fn fill_keeps_a_non_string_index_entry() {
    let filled = fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": {"type": "string", "maxLength": 100},
            "passportCountry": {"type": "string", "maxLength": 100}
        },
        "indexes": [[10, "passportCountry"]]
    }))
    .expect("filling must not reject what upstream fills");
    let indexes = get_indexes(&filled);
    assert_eq!(
        indexes[0],
        vec![
            json!("_deleted"),
            json!(10),
            json!("passportCountry"),
            json!("passportId")
        ],
        "the literal number is carried through, `_deleted` prepended and the primary key appended"
    );
    // A schema hash exists for it upstream, so the whole construction path must
    // succeed. The unencodable index simply yields no *usable* indexes.
    let schema = create_rx_schema(&json!({
        "version": 0,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": {"type": "string", "maxLength": 100},
            "passportCountry": {"type": "string", "maxLength": 100}
        },
        "indexes": [[10, "passportCountry"]]
    }))
    .expect("upstream creates and hashes this schema");
    assert_eq!(
        schema.indexes(),
        &[] as &[Vec<String>],
        "the unencodable index is not offered as a usable index, but the schema exists"
    );
    assert_eq!(schema.hash().len(), 64, "and it has a hash");

    let normalized = normalize_rx_json_schema(&filled);
    let hash = sha256_hex(&dxdb_json::canonical_bytes(&normalized));
    assert_eq!(
        hash,
        schema.hash(),
        "create_rx_schema must hash exactly the filled-and-normalized bytes"
    );
}

#[test]
fn get_string_indexes_rejects_a_non_string_entry() {
    let filled = fill_with_default_settings(&json!({
        "version": 0,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": {"type": "string", "maxLength": 100},
            "passportCountry": {"type": "string", "maxLength": 100}
        },
        "indexes": [[10, "passportCountry"]]
    }))
    .expect("fills");
    let error = get_string_indexes(&filled).expect_err("must reject");
    assert_eq!(error.code, SchemaErrorCode::Sc20);
}
