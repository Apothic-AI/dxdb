//! Schema fixtures, ported from upstream `src/plugins/test-utils/schemas.ts`.
//!
//! Each constant is a `serde_json::Value` holding the schema *literal* upstream
//! defines — before `fillWithDefaultSettings` runs. Nothing here injects
//! `_deleted`, `_meta` or the `_deleted`-prefixed indexes, because those are
//! `dxdb-schema`'s job (lane B) and injecting them twice would disagree with the
//! schema hash. A suite that needs a filled schema calls
//! [`crate::fill_with_default_settings`].
//!
//! Why `serde_json::Value` rather than typed structs: the storage layer takes
//! schemas as JSON ([`dxdb_storage::StorageSchema::from_value`]), so a typed
//! mirror would be a second source of truth that could drift from the value the
//! test actually passes. Upstream's own fixtures are plain object literals for
//! the same reason.
//!
//! # Provenance
//!
//! Every constant names its upstream line. When upstream changes a fixture, the
//! line reference is what makes the diff reviewable.

use serde_json::{json, Value};

/// `humanSchemaLiteral` — `schemas.ts:36`. The `humans` collection's schema.
///
/// This is the schema most of both conformance suites run against. It is ported
/// character for character including the `description` on `age` and the
/// `multipleOf: 1`, because a schema's hash covers all of it and the hash is part
/// of schema identity.
pub fn human() -> Value {
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

/// `humanMinimal` — `schemas.ts:1045`. No indexes, `oneOptional` optional.
///
/// Used by the query-correctness suite's `$or` group and by the
/// "Using undefined in a selector" group, both of which depend on a field being
/// *absent* rather than null.
pub fn human_minimal() -> Value {
    json!({
        "title": "human schema",
        "description": "describes a human being",
        "version": 0,
        "keyCompression": false,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "age": { "type": "integer" },
            "oneOptional": { "type": "string" }
        },
        "indexes": [],
        "required": ["passportId", "age"]
    })
}

/// `nestedHuman` — `schemas.ts:280`. A nested object with `mainSkill.name` and
/// `mainSkill.level`.
///
/// The `additionalProperties: false` on `mainSkill` and the `required` inside it
/// are upstream's; they matter because a storage that flattens a nested object
/// when building an index string will fail the `sort by nested mainSkill.name`
/// case rather than silently sorting wrongly.
pub fn nested_human() -> Value {
    json!({
        "title": "human nested",
        "version": 0,
        "description": "describes a human being with a nested field",
        "keyCompression": false,
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "firstName": { "type": "string", "maxLength": 100 },
            "mainSkill": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "maxLength": 10 },
                    "level": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 10,
                        "multipleOf": 1
                    }
                },
                "required": ["name", "level"],
                "additionalProperties": false
            }
        },
        "required": ["firstName"],
        "indexes": []
    })
}

/// `heroArray` — `schemas.ts:641`. Primary key `name`, `skills` an array of
/// objects.
///
/// The array-of-objects shape is what exercises `$elemMatch` and `$size`, and the
/// `uniqueItems: true` / `maxItems: 5` come from upstream.
pub fn hero_array() -> Value {
    json!({
        "title": "hero schema",
        "version": 0,
        "keyCompression": false,
        "description": "describes a hero with an array-field",
        "primaryKey": "name",
        "type": "object",
        "properties": {
            "name": { "type": "string", "maxLength": 100 },
            "skills": {
                "type": "array",
                "maxItems": 5,
                "uniqueItems": true,
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "damage": { "type": "number" }
                    }
                }
            }
        },
        "required": ["name"]
    })
}

/// `simpleArrayHero` — `schemas.ts:675`. Primary key `name`, `skills` an array of
/// *strings*.
///
/// The string-array sibling of [`hero_array`], used for the
/// "$in for array fields named skills" group: `$in` against an array field has
/// different semantics from `$in` against a scalar, and this is the fixture that
/// distinguishes them.
pub fn simple_array_hero() -> Value {
    json!({
        "title": "hero schema",
        "version": 0,
        "keyCompression": false,
        "description": "describes a hero with a string-array-field",
        "primaryKey": "name",
        "type": "object",
        "properties": {
            "name": { "type": "string", "maxLength": 100 },
            "skills": {
                "type": "array",
                "maxItems": 5,
                "uniqueItems": true,
                "items": { "type": "string" }
            }
        },
        "required": ["name"]
    })
}

/// `averageSchema()` — `schemas.ts:948`.
///
/// Upstream's is a *function* that appends `randomString(5)` to the title
/// ("randomisation used so hash differs"), which the key-compression test relies
/// on: two schemas that are structurally identical but differ in title hash
/// differently, and the test needs a schema it has never seen before.
///
/// The `sharding` block and the compound index `['var2', 'var1']` are ported as
/// upstream has them. A storage that ignores `sharding` is fine; a storage that
/// chokes on an unknown schema key is not, which is why it is kept.
pub fn average_schema() -> Value {
    let title = format!(
        "averageSchema_{}",
        crate::util::random_string(5, crate::util::COUCH_NAME_CHARS)
    );
    json!({
        "title": title,
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "keyCompression": false,
        "properties": {
            "id": { "description": "id", "type": "string", "maxLength": 12 },
            "var1": { "description": "var1", "type": "string", "maxLength": 12 },
            "var2": {
                "description": "var2",
                "type": "number",
                "minimum": 0,
                "maximum": 50000,
                "multipleOf": 1
            },
            "deep": {
                "type": "object",
                "properties": {
                    "deep1": { "type": "string", "maxLength": 10 },
                    "deep2": { "type": "string", "maxLength": 10 }
                }
            },
            "list": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "deep1": { "type": "string" },
                        "deep2": { "type": "string" }
                    }
                }
            }
        },
        "required": ["id", "var1", "var2"],
        "indexes": ["var1", "var2", "deep.deep1", ["var2", "var1"]],
        "sharding": { "shards": 6, "mode": "collection" }
    })
}

/// `point` — `schemas.ts:1024`. Two numeric fields, no indexes.
pub fn point() -> Value {
    json!({
        "title": "point schema",
        "version": 0,
        "description": "describes coordinates in 2d space",
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "x": { "type": "number" },
            "y": { "type": "number" }
        },
        "required": ["x", "y"]
    })
}

/// `humanWithAllIndex` — `schemas.ts:248`. Every scalar field indexed.
pub fn human_with_all_index() -> Value {
    json!({
        "title": "human schema",
        "version": 0,
        "keyCompression": false,
        "description": "describes a human being",
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "firstName": { "type": "string" },
            "lastName": { "type": "string" },
            "age": {
                "description": "Age in years",
                "type": "integer",
                "minimum": 0,
                "maximum": 150,
                "multipleOf": 1
            }
        },
        "required": ["firstName", "lastName", "age"],
        "indexes": ["firstName", "lastName", "age"]
    })
}

/// `humanAgeIndex` — `schemas.ts:183`. Only `age` indexed.
pub fn human_age_index() -> Value {
    json!({
        "title": "human schema",
        "version": 0,
        "keyCompression": false,
        "description": "describes a human being",
        "primaryKey": "passportId",
        "type": "object",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "firstName": { "type": "string" },
            "lastName": { "type": "string" },
            "age": {
                "description": "Age in years",
                "type": "integer",
                "minimum": 0,
                "maximum": 150,
                "multipleOf": 1
            }
        },
        "required": ["firstName", "lastName", "age"],
        "indexes": ["age"]
    })
}

/// `noIndexHuman` — `schemas.ts:355`. Primary key `firstName`, no indexes.
///
/// A primary key that is *not* `id` and not `passportId`, which is what catches a
/// storage that hard-coded `id` anywhere.
pub fn no_index_human() -> Value {
    json!({
        "title": "human schema",
        "version": 0,
        "keyCompression": false,
        "description": "no index",
        "primaryKey": "firstName",
        "type": "object",
        "properties": {
            "firstName": { "type": "string", "maxLength": 100 },
            "lastName": { "type": "string", "maxLength": 100 }
        },
        "required": ["firstName", "lastName"],
        "indexes": []
    })
}

/// The `TestDocType` pseudo-schema — `rx-schema-helper.ts:28`
/// `getPseudoSchemaForVersion(version, primaryKey)`.
///
/// Used by the creation, `.close()` and `.remove()` sections of the
/// implementations suite, which care about instance lifecycle and not about
/// documents. Ported as its own function so a ported call site keeps the
/// `getPseudoSchemaForVersion(0, "key")` phrasing and its meaning: a two-field
/// schema whose only index is the primary key.
pub fn pseudo_schema_for_version(version: u32, primary_key: &str) -> Value {
    json!({
        "version": version,
        "type": "object",
        "primaryKey": primary_key,
        "properties": {
            primary_key: { "type": "string", "maxLength": 100 },
            "value": { "type": "string" }
        },
        "indexes": [[primary_key]],
        "required": [primary_key]
    })
}

/// The `TestDocType` schema — `rx-storage-implementations.test.ts:101`
/// `getTestDataSchema()`.
///
/// Distinct from [`pseudo_schema_for_version`] in one way that matters: it
/// indexes a *non-primary* field (`value`), so the storage has a real secondary
/// index to maintain and to query.
pub fn test_data_schema() -> Value {
    json!({
        "version": 0,
        "type": "object",
        "primaryKey": "key",
        "properties": {
            "key": { "type": "string", "maxLength": 100 },
            "value": { "type": "string", "maxLength": 100 }
        },
        "required": ["key", "value"],
        "indexes": ["value"]
    })
}

/// `getNestedDocSchema()` — `rx-storage-implementations.test.ts:126`.
///
/// Its index is the compound `['nes.ted', 'id']`, i.e. a nested field *plus* the
/// primary key spelled out explicitly. Upstream includes it because a storage
/// that appends the primary key unconditionally would produce a four-field index
/// here and fail the query that uses it.
pub fn nested_doc_schema() -> Value {
    json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "nes": {
                "type": "object",
                "properties": {
                    "ted": { "type": "string", "maxLength": 100 }
                },
                "required": ["ted"]
            }
        },
        "indexes": [["nes.ted", "id"]],
        "required": ["id", "nes"]
    })
}

/// `humanCompositePrimary` — `schemas.ts:807`. A composite primary key.
///
/// `primaryKey` as an object with `fields` and a `separator` is the shape a
/// storage must handle to pass the "$eq operator with composite primary key"
/// block, where `expectedResultDocIds` contain `one|1|1` rather than a field
/// value.
pub fn human_composite_primary() -> Value {
    json!({
        "title": "human schema",
        "description": "describes a human being",
        "version": 0,
        "keyCompression": false,
        "primaryKey": {
            "key": "id",
            "fields": ["firstName", "info.age"],
            "separator": "|"
        },
        "type": "object",
        "properties": {
            "id": { "type": "string", "maxLength": 100 },
            "firstName": { "type": "string", "maxLength": 100 },
            "lastName": { "type": "string" },
            "info": {
                "type": "object",
                "properties": {
                    "age": {
                        "description": "age in years",
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 150
                    }
                },
                "required": ["age"]
            }
        },
        "required": ["id", "firstName", "lastName", "info"],
        "indexes": ["firstName"]
    })
}

/// The `_id`-primary schema the correctness suite inlines for two groups
/// (`rx-storage-query-correctness.test.ts:1750`, `:1957`).
///
/// Kept as a named fixture rather than inlined 25 times, because the composite index
/// list is the subject of the "wrong results on complex index" group.
pub fn query_correctness_complex_index_schema() -> Value {
    json!({
        "primaryKey": "_id",
        "type": "object",
        "version": 0,
        "properties": {
            "_id": { "type": "string", "maxLength": 20 },
            "name": { "type": "string", "maxLength": 20 },
            "gender": { "type": "string", "enum": ["f", "m", "x"], "maxLength": 1 },
            "age": { "type": "number", "minimum": 0, "maximum": 100, "multipleOf": 1 }
        },
        "required": ["_id", "name", "gender", "age"],
        "indexes": [
            ["name", "gender", "age", "_id"],
            ["gender", "age", "name", "_id"],
            ["age", "name", "gender", "_id"]
        ]
    })
}

/// Every fixture this module defines, for the provenance self-check.
pub fn all_fixtures() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("human", "src/plugins/test-utils/schemas.ts:36", human()),
        (
            "human_minimal",
            "src/plugins/test-utils/schemas.ts:1045",
            human_minimal(),
        ),
        (
            "nested_human",
            "src/plugins/test-utils/schemas.ts:280",
            nested_human(),
        ),
        (
            "hero_array",
            "src/plugins/test-utils/schemas.ts:641",
            hero_array(),
        ),
        (
            "simple_array_hero",
            "src/plugins/test-utils/schemas.ts:675",
            simple_array_hero(),
        ),
        (
            "average_schema",
            "src/plugins/test-utils/schemas.ts:948",
            average_schema(),
        ),
        ("point", "src/plugins/test-utils/schemas.ts:1024", point()),
        (
            "human_with_all_index",
            "src/plugins/test-utils/schemas.ts:248",
            human_with_all_index(),
        ),
        (
            "human_age_index",
            "src/plugins/test-utils/schemas.ts:183",
            human_age_index(),
        ),
        (
            "no_index_human",
            "src/plugins/test-utils/schemas.ts:355",
            no_index_human(),
        ),
        (
            "test_data_schema",
            "test/unit/rx-storage-implementations.test.ts:101",
            test_data_schema(),
        ),
        (
            "nested_doc_schema",
            "test/unit/rx-storage-implementations.test.ts:126",
            nested_doc_schema(),
        ),
        (
            "human_composite_primary",
            "src/plugins/test-utils/schemas.ts:807",
            human_composite_primary(),
        ),
        (
            "query_correctness_complex_index_schema",
            "test/unit/rx-storage-query-correctness.test.ts:1750",
            query_correctness_complex_index_schema(),
        ),
    ]
}
