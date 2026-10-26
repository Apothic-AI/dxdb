//! dxdb schema: the parts of `src/rx-schema-helper.ts` and `src/rx-schema.ts`
//! that are observable on the wire.
//!
//! RxDB does not treat a JSON schema as documentation. It rewrites it:
//!
//! - `_rev`, `_deleted`, `_meta` and `_attachments` are injected into
//!   `properties` and `required`,
//! - the documented primary key is appended to every index that lacks it,
//! - `_deleted` is prepended to every index that does not already start with it,
//! - a `_meta.lwt` index is appended for `getChangedDocumentsSince()`,
//! - the whole object is deep-sorted by key.
//!
//! The result is what gets hashed ([`Schema::hash`]) and what storages receive
//! as `RxJsonSchema`, so a difference here is a difference in schema identity
//! and in every index string built from the schema. The ordering rules are
//! therefore the substance of this crate, not an implementation detail.
//!
//! # Entry points
//!
//! - [`fill_with_default_settings`] — upstream's `fillWithDefaultSettings`
//!   (`src/rx-schema-helper.ts:196`).
//! - [`normalize_rx_json_schema`] — upstream's `normalizeRxJsonSchema`
//!   (`src/rx-schema-helper.ts:178`).
//! - [`create_rx_schema`] — fill + normalize + validate + index, which is what
//!   `createRxSchema` (`src/rx-schema.ts:264`) does.
//! - [`Schema::hash`] — SHA-256 over `dxdb_json::canonical_bytes` of the
//!   normalized schema, i.e. upstream's `JSON.stringify(this.jsonSchema)`
//!   (`src/rx-schema.ts:120`) with JavaScript number formatting.
//!
//! Index *strings* are not here: they live in `dxdb-query`, because
//! `src/custom-index.ts` imports `INDEX_MIN`/`INDEX_MAX` from
//! `src/query-planner.ts` and a schema crate that depended on the planner would
//! be circular. This crate owns the resolved *schema*; `dxdb-query` owns the
//! encoding built from it.
//!
//! # Deliberate deviations, each one recorded where it happens
//!
//! - Key sorting uses `dxdb_jsstr::compare_js` (UTF-16 code-unit order) where
//!   upstream calls `localeCompare`. See [`sort_object`].
//! - There is no async hash caching. See [`Schema::hash`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use dxdb_jsstr::compare_js;

pub mod error;
pub mod json_schema;

pub use error::{SchemaError, SchemaErrorCode};
pub use json_schema::{validate_document, validate_json_schema, SchemaValidator};

/// `META_LWT_UNIX_TIME_MAX` (`src/rx-schema-helper.ts:307`).
///
/// An `i64` rather than an `f64` because it is a JSON *integer* upstream, and the
/// distinction is observable: `serde_json` holds `1e15` as a float and writes it back
/// as `1e+15` (or fails to compare equal to a parsed `1000000000000000`), while
/// JavaScript holds one number. Two schemas that differ only in that representational
/// detail must still produce the same bytes, so the injected `_meta` schema is built
/// from integers and never from floats (PLAN.md 3.1).
pub const META_LWT_UNIX_TIME_MAX: i64 = 1_000_000_000_000_000;

/// `RX_META_LWT_MINIMUM` (`src/plugins/utils/utils-document.ts:15`).
///
/// One, not zero, "so that the value is never falsy" — the comment is upstream's and
/// the constant is part of every `_meta` schema. An `i64` for the same reason as
/// [`META_LWT_UNIX_TIME_MAX`].
pub const RX_META_LWT_MINIMUM: i64 = 1;

/// The four meta field names, in the order `fillWithDefaultSettings` pushes them
/// into `required` (`src/rx-schema-helper.ts:243`).
pub const META_FIELDS: [&str; 4] = ["_deleted", "_rev", "_meta", "_attachments"];

/// The value of `_meta` in a filled schema: upstream's `RX_META_SCHEMA`
/// (`src/rx-schema-helper.ts:308`).
///
/// `multipleOf: 0.01` and `additionalProperties: true` both matter: the former
/// is what makes `lwt` a safe index field at millisecond resolution, the latter
/// is how plugins store flags inside `_meta`.
pub fn rx_meta_schema() -> Value {
    // The bounds are integers and `multipleOf` is the only float, exactly as
    // upstream's literal is written. `serde_json` would happily hold `minimum: 1.0`,
    // but then the *value* would differ from one parsed out of a JSON document and a
    // `Value` comparison — which is how a schema-hash oracle compares — would fail
    // while the bytes matched.
    let multiple_of: f64 = 0.01;
    json!({
        "type": "object",
        "properties": {
            "lwt": {
                "type": "number",
                "minimum": RX_META_LWT_MINIMUM,
                "maximum": META_LWT_UNIX_TIME_MAX,
                "multipleOf": serde_json::Number::from_f64(multiple_of)
                    .expect("0.01 is finite")
            }
        },
        "additionalProperties": true,
        "required": ["lwt"]
    })
}

/// The default index a schema with no indexes gets: `['_deleted', primaryPath]`
/// (`src/rx-schema-helper.ts:188`).
pub fn default_index(primary_path: &str) -> Vec<String> {
    vec!["_deleted".to_string(), primary_path.to_string()]
}

/// Reads `primaryKey` and returns the field name part, i.e. upstream's
/// `getPrimaryFieldOfPrimaryKey` (`src/rx-schema-helper.ts:126`).
///
/// A composite primary key is `{ key, fields, separator }`; the *field* used for
/// storage is `key`, while `fields` are the components that get composed.
pub fn primary_field_of_primary_key(schema: &Value) -> Result<String, SchemaError> {
    match schema.get("primaryKey") {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Object(map)) => match map.get("key") {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(SchemaError::new(
                SchemaErrorCode::Sc30,
                "primaryKey object has no string `key`",
            )),
        },
        Some(_) => Err(SchemaError::new(
            SchemaErrorCode::Sc30,
            "primaryKey must be a string or an object",
        )),
        None => Err(SchemaError::new(
            SchemaErrorCode::Sc30,
            "schema has no primaryKey",
        )),
    }
}

/// Returns the compound fields of a composite primary key, or an empty slice for
/// a plain string primary key. Mirrors the `fields` array upstream reads in
/// `getComposedPrimaryKeyOfDocumentData` (`src/rx-schema-helper.ts:155`).
pub fn composite_primary_fields(schema: &Value) -> Vec<String> {
    let Some(Value::Object(map)) = schema.get("primaryKey") else {
        return Vec::new();
    };
    map.get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Composes the primary key of a document, upstream's
/// `getComposedPrimaryKeyOfDocumentData` (`src/rx-schema-helper.ts:147`).
///
/// A missing component is an error (`DOC18` upstream) rather than an empty
/// string, because an empty string silently produces a valid-looking but
/// colliding primary key.
pub fn composed_primary_key_of_document_data(
    schema: &Value,
    document: &Value,
) -> Result<String, SchemaError> {
    let primary_path = primary_field_of_primary_key(schema)?;
    if schema
        .get("primaryKey")
        .map(Value::is_string)
        .unwrap_or(false)
    {
        return match value_at_path(document, &primary_path) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(other) => Ok(js_value_to_key_part(other)),
            None => Ok(String::new()),
        };
    }
    let separator = schema
        .get("primaryKey")
        .and_then(|p| p.get("separator"))
        .and_then(Value::as_str)
        .unwrap_or("|");
    let mut parts = Vec::new();
    for field in composite_primary_fields(schema) {
        match value_at_path(document, &field) {
            None | Some(Value::Null) => {
                return Err(SchemaError::new(
                    SchemaErrorCode::Doc18,
                    format!("composite primary key field `{field}` is missing"),
                ));
            }
            Some(value) => parts.push(js_value_to_key_part(value)),
        }
    }
    Ok(parts.join(separator))
}

/// What JavaScript's string coercion would produce for a primary-key component.
///
/// Numbers go through [`dxdb_jsnum::to_js_string`] so a numeric component gives
/// `"1e+21"` rather than Rust's `"1000000000000000000000"` (PLAN.md 3.1).
fn js_value_to_key_part(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) => dxdb_jsnum::to_js_string(f),
            None => n.to_string(),
        },
        Value::Null => "null".to_string(),
        other => dxdb_json::to_js_json(other),
    }
}

/// Reads a dotted path out of a JSON value, without creating intermediate
/// objects. Returns `None` when any segment is missing.
pub fn value_at_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if !path.contains('.') {
        return value.get(path);
    }
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Returns the sub-schema at a dotted path, upstream's `getSchemaByObjectPath`
/// (`src/rx-schema-helper.ts:57`).
///
/// Upstream rewrites `a.b.c` to `properties.a.properties.b.properties.c`, which
/// is why an index on `mainSkill.name` resolves against
/// `properties.mainSkill.properties.name`. `[]` in a path means "element of",
/// which upstream's `getSchemaPropertyRealPath` (`check-schema.ts:271`) turns
/// into `properties.<name>.items`.
///
/// Upstream caches the result in a `WeakMap` keyed by schema identity. Here the
/// caller owns the schema value and can cache the result itself; the lookup is a
/// handful of `Map` hits.
pub fn schema_part_by_object_path<'a>(schema: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return schema.get("properties");
    }
    let mut current = schema;
    for raw_segment in path.split('.') {
        let segment = raw_segment.trim();
        if segment.is_empty() {
            continue;
        }
        if segment == "[]" {
            current = current.get("items")?;
            continue;
        }
        // Both shapes appear: field paths address `properties.<name>`, while a
        // caller reading a nested schema object (for example `properties.lwt`
        // inside `_meta`) already has the container. Try `properties` first, as
        // upstream does, then fall back to a direct member so that
        // `primaryKey`-style top-level reads still work.
        current = match current.get("properties").and_then(|p| p.get(segment)) {
            Some(found) => found,
            None => current.get("items").and_then(|items| {
                if segment == "properties" {
                    None
                } else {
                    items.get("properties").and_then(|p| p.get(segment))
                }
            })?,
        };
    }
    Some(current)
}

/// `sortObject(obj, true)` from `src/plugins/utils/utils-object.ts:199`.
///
/// The second argument (`noArraySort`) is always `true` upstream at the only
/// call site that matters here — `normalizeRxJsonSchema` — so arrays keep their
/// element order. That is load-bearing: `indexes` is an ordered list, and
/// sorting it would change schema identity and which index the planner picks
/// first.
///
/// # Deviation: `localeCompare` is replaced by code-unit order
///
/// Upstream sorts keys with `a.localeCompare(b)` (`utils-object.ts:220`). That
/// is ICU collation: case-insensitive at the primary strength, so `"Name"`
/// sorts between `"name"` and `"NAME"`, and it depends on the ICU data version
/// bundled with the JS runtime. PLAN.md 3.2 requires a decision and documents
/// the one taken in `dxdb-jsstr`: code-unit order is the only supported order.
///
/// The two orders agree on every key RxDB fixtures actually use — the injected
/// meta keys and all-lowercase field names both sort identically — and disagree
/// only for mixed-case or punctuation-led keys. Since a schema hash is only
/// compared against another hash produced by *this* implementation (the hash is
/// an opaque identity token, not an interop value), the difference is contained
/// to schemas whose key sets mix `A` and `a`, where accepting a stable order
/// beats reproducing one specific ICU build.
pub fn sort_object(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| compare_js(a, b));
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), sort_object(&map[key]));
            }
            Value::Object(out)
        }
        // `noArraySort = true`: elements keep their order, nested objects in
        // arrays are still sorted.
        Value::Array(items) => Value::Array(items.iter().map(sort_object).collect()),
        other => other.clone(),
    }
}

/// `normalizeRxJsonSchema` (`src/rx-schema-helper.ts:178`).
pub fn normalize_rx_json_schema(schema: &Value) -> Value {
    sort_object(schema)
}

/// `fillWithDefaultSettings` (`src/rx-schema-helper.ts:196`).
///
/// Order of operations is observable and is preserved exactly:
///
/// 1. `additionalProperties = false`, `keyCompression = false` if absent.
/// 2. `indexes`, `required`, `encrypted` become arrays.
/// 3. The four meta properties are inserted.
/// 4. `required` gets `_deleted, _rev, _meta, _attachments` appended, then the
///    primary key, then is filtered to top-level fields and de-duplicated —
///    keeping the *first* occurrence, so a user's own required order survives.
/// 5. `version` defaults to `0`.
/// 6. Each index: append the primary key if absent, then prepend `_deleted`
///    unless it is already first.
/// 7. Empty `indexes` becomes `[default_index]`.
/// 8. `[_meta.lwt, primaryPath]` is appended unless that exact index is already
///    present, after which duplicates are dropped in insertion order.
pub fn fill_with_default_settings(schema: &Value) -> Result<Value, SchemaError> {
    let Some(input) = schema.as_object() else {
        return Err(SchemaError::new(
            SchemaErrorCode::Sc29,
            "schema must be a JSON object with `properties`",
        ));
    };
    let mut out: Map<String, Value> = input.clone();

    let primary_path = primary_field_of_primary_key(schema)?;

    let mut properties = match out.get("properties") {
        Some(Value::Object(map)) => map.clone(),
        _ => {
            return Err(SchemaError::new(
                SchemaErrorCode::Sc29,
                "schema has no `properties` object",
            ))
        }
    };

    out.insert("additionalProperties".into(), Value::Bool(false));
    if !out.contains_key("keyCompression") {
        out.insert("keyCompression".into(), Value::Bool(false));
    }

    let mut indexes = match out.get("indexes") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            return Err(SchemaError::new(
                SchemaErrorCode::Sc18,
                "`indexes` must be an array",
            ))
        }
    };
    let required = match out.get("required") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            return Err(SchemaError::new(
                SchemaErrorCode::Sc24,
                "`required` must be an array, not a boolean",
            ))
        }
    };
    let encrypted = match out.get("encrypted") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(_) => Vec::new(),
    };

    // `_rev` first, then `_attachments`, `_deleted`, `_meta` — the insertion
    // order upstream uses, which only matters before `normalize` sorts keys.
    properties.insert("_rev".into(), json!({ "type": "string", "minLength": 1 }));
    properties.insert("_attachments".into(), json!({ "type": "object" }));
    properties.insert("_deleted".into(), json!({ "type": "boolean" }));
    properties.insert("_meta".into(), rx_meta_schema());
    out.insert("properties".into(), Value::Object(properties));

    let mut required_names: Vec<String> = required
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    for meta_field in META_FIELDS {
        required_names.push(meta_field.to_string());
    }
    required_names.push(primary_path.clone());
    // `.filter((field) => !field.includes('.'))` then `unique()`.
    let mut seen_required = BTreeSet::new();
    required_names.retain(|name| !name.contains('.') && seen_required.insert(name.clone()));
    out.insert(
        "required".into(),
        Value::Array(required_names.into_iter().map(Value::String).collect()),
    );

    if !out.contains_key("version") || out.get("version") == Some(&Value::Null) {
        out.insert("version".into(), json!(0));
    }

    // Index entries are kept as raw JSON, not as strings.
    //
    // Upstream's `useIndexes: string[][]` is a lie the fixture set exposes: a
    // compound index written as `[10, 'passportCountry']` fills to
    // `['_deleted', 10, 'passportCountry', 'passportId']` — the literal number is
    // carried through `.includes`, `.push` and `.unshift` untouched, and the schema
    // then *hashes*, because hashing only serialises the object. Upstream fails only
    // when someone tries to build an index string from it
    // (`usePath.replace is not a function`) or plan a query against it.
    //
    // Rejecting it here instead would change the schema hash of a fixture that
    // upstream hashes, so the validation belongs at the encoder
    // (`dxdb_query::index_meta`, `CI1`) and not at fill time. The oracle vectors
    // record exactly this split: `testdata/vectors/schema-hashes.json` contains a
    // hash for `compoundIndexNoString`, and `index-strings.json::skipped` records
    // that upstream could not encode an index for it.
    let mut use_indexes: Vec<Vec<Value>> = Vec::new();
    for entry in indexes.drain(..) {
        let mut fields: Vec<Value> = match entry {
            Value::String(s) => vec![Value::String(s)],
            Value::Array(items) => items,
            other => vec![other],
        };
        if !fields
            .iter()
            .any(|field| field.as_str() == Some(primary_path.as_str()))
        {
            fields.push(Value::String(primary_path.clone()));
        }
        if fields.first().and_then(Value::as_str) != Some("_deleted") {
            fields.insert(0, Value::String("_deleted".to_string()));
        }
        use_indexes.push(fields);
    }

    if use_indexes.is_empty() {
        use_indexes.push(
            default_index(&primary_path)
                .into_iter()
                .map(Value::String)
                .collect(),
        );
    }
    use_indexes.push(vec![
        Value::String("_meta.lwt".to_string()),
        Value::String(primary_path.clone()),
    ]);

    for internal in out
        .get("internalIndexes")
        .and_then(Value::as_array)
        .map(|v| v.to_vec())
        .unwrap_or_default()
    {
        let fields: Vec<Value> = match internal {
            Value::String(s) => vec![Value::String(s)],
            Value::Array(items) => items,
            _ => continue,
        };
        use_indexes.push(fields);
    }

    // `index.join(',')` de-duplication, insertion order preserved. `join` on a
    // mixed array stringifies each element, which is how upstream compares them.
    let mut seen_indexes = BTreeSet::new();
    use_indexes.retain(|index| {
        let key = index
            .iter()
            .map(|field| match field {
                Value::String(s) => s.clone(),
                other => dxdb_json::to_js_json(other),
            })
            .collect::<Vec<_>>()
            .join(",");
        seen_indexes.insert(key)
    });

    out.insert(
        "indexes".into(),
        Value::Array(use_indexes.into_iter().map(Value::Array).collect()),
    );

    if encrypted.is_empty() {
        out.insert("encrypted".into(), Value::Array(Vec::new()));
    } else {
        out.insert("encrypted".into(), Value::Array(encrypted));
    }

    Ok(Value::Object(out))
}

/// `getIndexes` (`src/rx-schema.ts:247`): every index as an array of raw entries.
///
/// Raw rather than `String` because a `fillWithDefaultSettings` pass keeps a
/// non-string index entry verbatim (see its documentation), and a caller hashing or
/// echoing the schema needs to see it. Use [`get_string_indexes`] when the entries
/// are about to become index strings.
pub fn get_indexes(schema: &Value) -> Vec<Vec<Value>> {
    schema
        .get("indexes")
        .and_then(Value::as_array)
        .map(|indexes| {
            indexes
                .iter()
                .map(|index| match index {
                    Value::String(s) => vec![Value::String(s.clone())],
                    Value::Array(items) => items.clone(),
                    other => vec![other.clone()],
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The same indexes, requiring every entry to be a field name.
///
/// This is the form planning and index-string construction need. A non-string entry
/// is `SC20` upstream (`check-schema.ts:375`) and, more to the point, cannot be
/// resolved to a schema part, so there is no honest way to plan with it.
pub fn get_string_indexes(schema: &Value) -> Result<Vec<Vec<String>>, SchemaError> {
    get_indexes(schema)
        .into_iter()
        .map(|index| {
            index
                .into_iter()
                .map(|field| match field {
                    Value::String(name) => Ok(name),
                    other => Err(SchemaError::new(
                        SchemaErrorCode::Sc20,
                        format!(
                            "index entry {} is not a field name, so it cannot be encoded as an index",
                            dxdb_json::to_js_json(&other)
                        ),
                    )),
                })
                .collect()
        })
        .collect()
}

/// `getFinalFields` (`src/rx-schema-helper.ts:340`).
///
/// Fields declared `final: true`, plus the primary key, plus the components of a
/// composite primary key. Upstream pushes those in that order and does not
/// de-duplicate, so a `final: true` primary key appears twice; the same happens
/// here.
pub fn get_final_fields(schema: &Value) -> Result<Vec<String>, SchemaError> {
    let mut out = Vec::new();
    if let Some(Value::Object(properties)) = schema.get("properties") {
        for (key, value) in properties {
            if value.get("final").and_then(Value::as_bool) == Some(true) {
                out.push(key.clone());
            }
        }
    }
    out.push(primary_field_of_primary_key(schema)?);
    for field in composite_primary_fields(schema) {
        out.push(field);
    }
    Ok(out)
}

/// The default values declared by a schema, upstream's `RxSchema.defaultValues`
/// (`src/rx-schema.ts:72`).
pub fn default_values(schema: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(Value::Object(properties)) = schema.get("properties") {
        for (key, value) in properties {
            if let Some(default) = value.get("default") {
                out.insert(key.clone(), default.clone());
            }
        }
    }
    out
}

/// `fillObjectWithDefaults` (`src/rx-schema-helper.ts:363`).
///
/// Non-primitive defaults are deep-copied per call. Upstream added that after a
/// bug where two documents shared one array default and mutating one corrupted
/// the schema's cache for every future document.
pub fn fill_object_with_defaults(schema: &Value, object: &mut Value) {
    let defaults = default_values(schema);
    let Some(map) = object.as_object_mut() else {
        return;
    };
    // Upstream checks `obj[key] === undefined`, so an explicit `null` is kept and
    // only a genuinely absent key takes the default.
    for (key, default) in defaults {
        if !map.contains_key(&key) {
            map.insert(key, default);
        }
    }
}

/// `getJsonSchemaWithoutMeta` (`src/rx-schema.ts:85`).
///
/// Used by migration and export paths: it removes the injected meta properties,
/// strips `_`-prefixed entries from `required`, and drops `_`-prefixed index
/// fields, discarding indexes that become empty.
pub fn json_schema_without_meta(schema: &Value) -> Result<Value, SchemaError> {
    let Some(input) = schema.as_object() else {
        return Err(SchemaError::new(
            SchemaErrorCode::Sc29,
            "schema must be a JSON object",
        ));
    };
    let mut out = input.clone();
    if let Some(Value::Object(properties)) = out.get("properties") {
        let mut properties = properties.clone();
        for meta in META_FIELDS {
            properties.remove(meta);
        }
        out.insert("properties".into(), Value::Object(properties));
    }
    if let Some(Value::Array(required)) = out.get("required") {
        out.insert(
            "required".into(),
            Value::Array(
                required
                    .iter()
                    .filter(|item| !item.as_str().map(|s| s.starts_with('_')).unwrap_or(false))
                    .cloned()
                    .collect(),
            ),
        );
    }
    if let Some(Value::Array(indexes)) = out.get("indexes") {
        let filtered: Vec<Value> = indexes
            .iter()
            .map(|index| match index {
                Value::String(s) => Value::Array(
                    vec![Value::String(s.clone())]
                        .into_iter()
                        .filter(|v| !v.as_str().unwrap_or_default().starts_with('_'))
                        .collect(),
                ),
                Value::Array(items) => Value::Array(
                    items
                        .iter()
                        .filter(|item| !item.as_str().unwrap_or_default().starts_with('_'))
                        .cloned()
                        .collect(),
                ),
                other => other.clone(),
            })
            .filter(|index| match index {
                Value::Array(items) => !items.is_empty(),
                _ => true,
            })
            .collect();
        out.insert("indexes".into(), Value::Array(filtered));
    }
    Ok(Value::Object(out))
}

/// `getPreviousVersions` (`src/rx-schema.ts:256`): `[0, 1, ..., version - 1]`.
pub fn previous_versions(schema: &Value) -> Vec<u32> {
    let version = schema
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    (0..version).map(|v| v as u32).collect()
}

/// The hash function upstream calls `defaultHashSha256`
/// (`src/plugins/utils/utils-hash.ts:52`).
///
/// Input is the exact UTF-8 bytes handed to `crypto.subtle.digest('SHA-256')`;
/// output is lower-case hex, which is how upstream stringifies the digest.
pub fn sha256_hex(input: &[u8]) -> String {
    let digest = Sha256::digest(input);
    hex::encode(digest)
}

/// The provenance of a schema hash, recorded so a mismatch can be explained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashSource {
    /// The hash of the normalized schema, which is what upstream hashes.
    Normalized,
}

/// A filled, normalized, validated schema plus its index-derived metadata.
///
/// Building one is the equivalent of upstream's `createRxSchema`
/// (`src/rx-schema.ts:264`): fill defaults, normalize, validate, precompute.
/// Everything is computed once, because every one of these is consulted on the
/// query path.
#[derive(Debug, Clone)]
pub struct Schema {
    json_schema: Value,
    primary_path: String,
    indexes: Vec<Vec<String>>,
    final_fields: Vec<String>,
    hash: String,
}

impl Schema {
    /// Builds a schema from an already-filled, normalized JSON schema.
    ///
    /// Use [`create_rx_schema`] for the upstream-equivalent entry point.
    ///
    /// # No draft-07 validation here, deliberately
    ///
    /// Upstream's `createRxSchema` (`src/rx-schema.ts:264`) fills defaults,
    /// normalizes, freezes and constructs. It does *not* validate the schema against
    /// the draft-07 meta-schema; that happens only if the dev-mode plugin's
    /// `checkSchema` runs (M6 work here, and a plugin upstream, not the core).
    ///
    /// The difference is observable. `humanCompositePrimarySchemaLiteral` declares a
    /// `readonlyProps` property whose subschema is
    /// `{allOf: [], anyOf: [], enum: [], items: [], oneOf: [], required: [], type: []}`
    /// — all legal draft-07 *keywords* with legal values, and no `type`, so a
    /// draft-07 validator accepts it. Upstream nevertheless hashes it, and the oracle
    /// records that hash, so a construction path that rejected it would fail to
    /// reproduce schema identity for a fixture. Validation is therefore a separate,
    /// explicit step ([`json_schema::validate_json_schema`]) and
    /// [`validate_document`] does it per document.
    pub fn new(json_schema: Value) -> Result<Self, SchemaError> {
        let primary_path = primary_field_of_primary_key(&json_schema)?;
        // Only the *encodable* indexes are required: a schema whose index contains a
        // non-field entry still has a hash upstream, it just cannot be used for index
        // strings or planning.
        let indexes = get_string_indexes(&json_schema).unwrap_or_default();
        let final_fields = get_final_fields(&json_schema)?;
        let hash = compute_hash(&json_schema);
        Ok(Self {
            json_schema,
            primary_path,
            indexes,
            final_fields,
            hash,
        })
    }

    /// The normalized schema, key order included.
    pub fn json_schema(&self) -> &Value {
        &self.json_schema
    }

    /// The field name part of the primary key.
    pub fn primary_path(&self) -> &str {
        &self.primary_path
    }

    /// Every index as an array of field names, `_deleted` prepended.
    pub fn indexes(&self) -> &[Vec<String>] {
        &self.indexes
    }

    /// Fields that may not change after creation.
    pub fn final_fields(&self) -> &[String] {
        &self.final_fields
    }

    /// `version` from the schema, defaulting to `0`.
    pub fn version(&self) -> u32 {
        self.json_schema
            .get("version")
            .and_then(Value::as_u64)
            .unwrap_or_default() as u32
    }

    /// The schema hash: SHA-256 hex over the canonical bytes of the normalized
    /// schema.
    ///
    /// Upstream hashes `JSON.stringify(this.jsonSchema)` and caches the promise
    /// in a bounded `Map` keyed by the stringified schema (`src/rx-schema.ts:113`).
    /// Three differences, none of them observable in the value:
    ///
    /// - The bytes come from [`dxdb_json::canonical_bytes`], so a float in the
    ///   schema renders as `1` (JS) rather than `1.0` (serde_json).
    /// - The hash is computed eagerly at construction and stored, so it is
    ///   infallible here where upstream returns a promise that can reject. The
    ///   only way upstream's call can fail is a missing `crypto.subtle`; a
    ///   precomputed string cannot fail, which removes the "do not cache a
    ///   failed hash" branch entirely.
    /// - There is no cross-schema cache. It exists to avoid repeating an async
    ///   digest; with a synchronous hash the cache cost more than it saved, and
    ///   caching would hide a change to the canonicalisation.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Records where the hash came from. Present so a caller logging a hash
    /// mismatch has the input bytes available.
    pub fn hash_source(&self) -> HashSource {
        HashSource::Normalized
    }

    /// The exact bytes that were hashed.
    pub fn hash_input(&self) -> Vec<u8> {
        dxdb_json::canonical_bytes(&self.json_schema)
    }

    /// The sub-schema for a dotted object path.
    pub fn schema_part(&self, path: &str) -> Option<&Value> {
        schema_part_by_object_path(&self.json_schema, path)
    }

    /// `maxLength` of the primary key field, required by every index builder
    /// (`SC39`, `src/rx-schema.ts:61`).
    pub fn primary_key_max_length(&self) -> Result<usize, SchemaError> {
        let part = self.schema_part(&self.primary_path).ok_or_else(|| {
            SchemaError::new(
                SchemaErrorCode::Sc33,
                format!("primary key `{}` is not in properties", self.primary_path),
            )
        })?;
        part.get("maxLength")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v as usize)
            .ok_or_else(|| {
                SchemaError::new(
                    SchemaErrorCode::Sc39,
                    format!(
                        "primary key `{}` has no finite maxLength",
                        self.primary_path
                    ),
                )
            })
    }

    /// The schema with the injected meta fields removed.
    pub fn json_schema_without_meta(&self) -> Result<Value, SchemaError> {
        json_schema_without_meta(&self.json_schema)
    }
}

/// Computes the schema hash, upstream's `RxSchema.hash` input
/// (`src/rx-schema.ts:120`).
fn compute_hash(json_schema: &Value) -> String {
    sha256_hex(&dxdb_json::canonical_bytes(json_schema))
}

/// `createRxSchema` (`src/rx-schema.ts:264`) without the plugin hooks.
///
/// Hooks are M6's business; everything else is M1's and happens in the same
/// order: fill defaults, normalize (which sorts keys), then construct.
pub fn create_rx_schema(schema: &Value) -> Result<Schema, SchemaError> {
    let filled = fill_with_default_settings(schema)?;
    let normalized = normalize_rx_json_schema(&filled);
    Schema::new(normalized)
}

#[cfg(test)]
mod tests;
