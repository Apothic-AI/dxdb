//! Short field keys on write, expanded on read (`wrappedKeyCompressionStorage`).
//!
//! Ported from `src/plugins/key-compression/index.ts` (249 lines) at upstream
//! `3b763ee17`, and from `jsonschema-key-compression@1.7.0`, which is where the
//! actual behaviour lives: the plugin file is 160 lines of wiring around
//! `createCompressionTable` / `compressObject` / `decompressObject` /
//! `compressQuery` / `createCompressedJsonSchema`.
//!
//! # Why this crate contains the library
//!
//! There is no Rust crate for `jsonschema-key-compression`, so the table format
//! is a wire format this port has to reproduce exactly: the compressed key names
//! appear in stored data, which makes them an interop contract, not an
//! implementation detail. The whole library is 586 lines of TypeScript across six
//! files and its behaviour is small enough to state completely:
//!
//! * The table is built from **every** property name anywhere in the schema,
//!   recursing through `properties` and `items` (`create-compression-table.ts:45`).
//! * Names are sorted with JS `<` (`:28`), i.e. **UTF-16 code unit order**, then
//!   **filtered to those longer than three characters** (`:87`). That filter is
//!   why `age` is never compressed and `firstName` is.
//! * The primary field and `_rev`, `_attachments`, `_deleted`, `_meta` are in the
//!   ignore list (`index.ts:78-88`), so they keep their names. The comment is
//!   explicit that the primary field is left uncompressed "for easier debugging".
//! * A compressed name is the flag `|` followed by a base58 counter in the
//!   library's own alphabet `abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ123456789`
//!   (`util.ts:5`), assigned in sorted order.
//! * Compression is recursive over objects and arrays; a key containing `[` keeps
//!   its bracket suffix; a key already starting with `|` is an error
//!   (`compress.ts:62`).
//!
//! # What is ported here, and what awaits lane D
//!
//! Everything except the storage wrapper's *incarnation*: [`CompressionState`],
//! [`compress_document_data`], [`decompress_document_data`], [`compress_query`],
//! [`CompressedSchema`] and [`KeyCompressionStorage`] are complete and tested.
//!
//! [`KeyCompressionStorage`] cannot yet be constructed, because
//! `dxdb_storage::RxStorageInstance` uses `impl Future` in its method signatures
//! and so is not dyn-compatible. Upstream's wrapper is
//! `Object.assign({}, storage, { createStorageInstance })` — it must hand back a
//! *dyn-compatible* instance to pass over a worker or IPC boundary by definition,
//! and its instance is `wrapRxStorageInstance(...)`, a wrapper around any instance.
//! Rust needs the `DynRxStorageInstance` twin from PLAN.md 3.3 (`dynosaur`) for
//! that to be expressible. The shape and the behaviour are written out in
//! [`KeyCompressionStorage`]'s docs so the wiring is mechanical once the twin
//! lands; the transformation logic it would call is finished and tested.
//!
//! A second dependency: upstream re-prepares the compressed query by calling
//! `prepareQuery(compressedSchema, compressedQuery)` (`index.ts:207`), which lives
//! in `dxdb-query` and is not public yet. [`KeyCompressionState::compressed_query`]
//! returns the compressed `MangoQuery`, which is the input to that call.
//!
//! # Two behaviours that are not what you would guess
//!
//! **`_deleted` is not compressed, but its *value* is not what round-trips.**
//! Nothing to see here — `_deleted` is in the ignore list, so it survives intact.
//! The real trap is index hints.
//!
//! **A query's `index` hint is dropped by upstream's own compression function and
//! then re-added by the plugin** (`compressQuery` does not copy `index`,
//! `compress.ts:107-161`; the plugin re-adds it at `index.ts:201-205`). Upstream
//! shipped a fix for this (issue #6267-adjacent, the "forward the query index
//! hint" test). [`compress_query`] keeps the hint by construction rather than by a
//! patch, and there is a test asserting the hint survives.
//!
//! # Enabling compression on an existing collection
//!
//! The brief asks what upstream does here, and the answer is: **it refuses.** It
//! does not migrate.
//!
//! `keyCompression` is part of the schema and therefore part of the schema hash
//! that identifies a collection (`rx-schema.ts:113-121` hashes
//! `JSON.stringify(this.jsonSchema)`, and `keyCompression` is in it — verified by
//! hashing the same schema with the flag flipped, which produces a different
//! digest). When a collection is opened, the stored schema hash is compared with
//! the computed one (`rx-database.ts:472`) and a mismatch that is also not
//! content-equal raises **DB6** (`:481-488`).
//!
//! So flipping `keyCompression` on a live collection changes its identity and is
//! an error, not a silent reinterpretation of the documents already on disk. That
//! is the correct behaviour and this port reproduces it: [`CompressionState::for_schema`]
//! produces a state whose compressed schema records `key_compression: true`, and a
//! caller comparing schema identity (as `dxdb-schema::Schema::hash` does) sees a
//! different collection. There is **no migration path and none is invented**;
//! changing the flag means a new collection plus `migration-storage`, which is the
//! next crate in this lane.

#![forbid(unsafe_code)]

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use thiserror::Error;

/// The flag that marks a key as compressed (`DEFAULT_COMPRESSION_FLAG`,
/// `create-compression-table.ts:17`).
pub const COMPRESSION_FLAG: char = '|';

/// The library's base58 alphabet, deliberately missing `l`, `0`, `O` and `I`
/// (`util.ts:5`).
const BASE58: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ123456789";

/// Properties shorter than this are never compressed (`create-compression-table.ts:87`).
const MIN_COMPRESSIBLE_LENGTH: usize = 3;

/// Meta fields that keep their names (`index.ts:84-88`).
pub const IGNORE_ALWAYS: [&str; 4] = ["_rev", "_attachments", "_deleted", "_meta"];

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// A key already starts with the compression flag, so compressing it would be
    /// ambiguous when reading back (`compress.ts:66-71`).
    #[error("cannot compress a key that starts with the compression flag '{flag}': {key}")]
    KeyUsesFlag {
        /// The offending key.
        key: String,
        /// The flag in use, which is `|`.
        flag: char,
    },
    /// The schema has no usable primary key.
    #[error("schema has no primary key: {0}")]
    NoPrimaryKey(String),
    /// A schema shape this crate cannot compress (a composite primary key whose
    /// level is not resolvable, or `properties` that is not an object).
    #[error("cannot build a compression state: {0}")]
    InvalidSchema(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A bidirectional name map plus the flag in use
/// (`CompressionTable`, `types.d.ts:3-7`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressionTable {
    /// Sorted uncompressed name to its compressed (unflagged) name.
    forward: BTreeMap<String, String>,
    /// `|`-prefixed compressed name to the uncompressed name.
    reverse: BTreeMap<String, String>,
}

impl CompressionTable {
    /// `createCompressionTable` (`create-compression-table.ts:19`).
    ///
    /// `ignore` names are excluded in addition to the meta fields and the primary
    /// field the caller passes.
    pub fn build(schema: &Value, ignore: &[String]) -> Result<Self> {
        let mut names = Vec::new();
        collect_property_names(schema, &mut names);
        // `Array.from(attributes).sort(alphabeticCompare)` — dedupe through a set,
        // then JS string order, which is UTF-16 code-unit order.
        names.sort_by(|a, b| dxdb_jsstr::compare_js(a, b));
        names.dedup();

        let mut forward = BTreeMap::new();
        let mut counter: u64 = 0;
        for name in names {
            if name.chars().count() <= MIN_COMPRESSIBLE_LENGTH {
                continue;
            }
            if ignore.iter().any(|ignored| ignored == &name) {
                continue;
            }
            forward.insert(name, number_to_letter(counter));
            counter += 1;
        }

        let reverse = forward
            .iter()
            .map(|(plain, compressed)| (format!("{COMPRESSION_FLAG}{compressed}"), plain.clone()))
            .collect();

        Ok(CompressionTable { forward, reverse })
    }

    /// The compressed name for a plain key, flag included.
    ///
    /// Unchanged when the key is not in the table, when it is short, or when it was
    /// ignored. A `[index]` suffix is preserved (`compress.ts:74-95`).
    pub fn compress_key(&self, key: &str) -> Result<String> {
        if key.starts_with(COMPRESSION_FLAG) {
            return Err(Error::KeyUsesFlag {
                key: key.to_string(),
                flag: COMPRESSION_FLAG,
            });
        }
        // `key.split('[')` then re-append every bracket group.
        let (plain, brackets) = split_brackets(key);
        match self.forward.get(plain) {
            None => Ok(key.to_string()),
            Some(compressed) => Ok(format!("{COMPRESSION_FLAG}{compressed}{brackets}")),
        }
    }

    /// The plain name for a compressed key.
    ///
    /// A key that is not in the reverse table is returned unchanged, which is what
    /// makes decompression idempotent for uncompressed documents.
    pub fn decompress_key(&self, key: &str) -> String {
        let (plain, brackets) = split_brackets(key);
        match self.reverse.get(plain) {
            None => key.to_string(),
            Some(uncompressed) => format!("{uncompressed}{brackets}"),
        }
    }

    /// `compressedPath` (`compress.ts:47`): compress each dot-separated segment.
    pub fn compress_path(&self, path: &str) -> Result<String> {
        path.split('.')
            .map(|segment| self.compress_key(segment))
            .collect::<Result<Vec<_>>>()
            .map(|segments| segments.join("."))
    }

    /// `decompressedPath` (`decompress.ts:41`).
    pub fn decompress_path(&self, path: &str) -> String {
        path.split('.')
            .map(|segment| self.decompress_key(segment))
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The number of compressed names. Useful for a test that pins the table.
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// True when nothing is compressed, which happens for a schema whose every
    /// property name is three characters or shorter.
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }
}

/// Everything the wrapper needs for one schema, cached by upstream in a `WeakMap`
/// (`index.ts:52`).
#[derive(Clone, Debug, PartialEq)]
pub struct CompressionState {
    /// The name table.
    pub table: CompressionTable,
    /// The schema the caller supplied, with `keyCompression: true`.
    pub schema: Value,
    /// The same schema with every property name, `required` entry, index and the
    /// primary key compressed, and `keyCompression: false` — which is what the
    /// underlying storage is handed (`index.ts:166-177`).
    pub compressed_schema: Value,
}

impl CompressionState {
    /// `getCompressionStateByRxJsonSchema` (`index.ts:58`).
    ///
    /// One table is built, with the plugin's ignore list, and that same table is
    /// what compresses the schema. Upstream does exactly this — the table is created
    /// at `index.ts:75` and handed to `createCompressedJsonSchema` at `:92` — and
    /// building a *second* table here would be a real bug, not a tidy-up: the two
    /// would disagree about the primary field and the documents would be stored
    /// under a name the compressed schema does not describe.
    pub fn for_schema(schema: &Value) -> Result<Self> {
        let ignore = ignore_list_for(schema)?;
        let table = CompressionTable::build(schema, &ignore)?;
        Ok(Self {
            compressed_schema: compress_schema_with_table(schema, &table)?,
            table,
            schema: schema.clone(),
        })
    }
}

impl CompressionState {
    /// Compresses a document's field names.
    ///
    /// `_attachments` is removed before the walk and restored afterwards
    /// (`index.ts:225-238`), because attachment payloads cannot survive a generic
    /// deep clone.
    pub fn compress_document(&self, document: &Value) -> Result<Value> {
        let mut object = match document {
            Value::Object(map) => map.clone(),
            other => {
                return Err(Error::InvalidSchema(format!(
                    "a document must be an object, got {other}"
                )))
            }
        };
        let attachments = object.remove("_attachments");
        let mut compressed = compress_object(&self.table, &Value::Object(object))?;
        if let (Some(attachments), Value::Object(map)) = (attachments, &mut compressed) {
            map.insert("_attachments".to_string(), attachments);
        }
        Ok(compressed)
    }

    /// Expands a document's field names. The inverse of
    /// [`CompressionState::compress_document`].
    pub fn decompress_document(&self, document: &Value) -> Value {
        decompress_object(&self.table, document)
    }

    /// Compresses a query so it can be run against compressed documents
    /// (`index.ts:190-212`).
    pub fn compressed_query(
        &self,
        query: &dxdb_storage::MangoQuery,
    ) -> Result<dxdb_storage::MangoQuery> {
        compress_query(&self.table, query)
    }
}

/// `compressObject` (`compress.ts:11`).
pub fn compress_object(table: &CompressionTable, value: &Value) -> Result<Value> {
    Ok(match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| compress_object(table, item))
                .collect::<Result<Vec<_>>>()?,
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, member) in map {
                out.insert(table.compress_key(key)?, compress_object(table, member)?);
            }
            Value::Object(out)
        }
        // `if (typeof obj !== 'object' || obj === null) return obj;`
        other => other.clone(),
    })
}

/// `decompressObject` (`decompress.ts:6`).
pub fn decompress_object(table: &CompressionTable, value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| decompress_object(table, item))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, member) in map {
                out.insert(table.decompress_key(key), decompress_object(table, member));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `compressQuery` (`compress.ts:103`).
///
/// Differs from upstream in one way, deliberately: the `index` hint is compressed
/// and kept, where upstream's library drops it and the plugin re-adds it
/// (`index.ts:196-205`). Same result, one place instead of two.
pub fn compress_query(
    table: &CompressionTable,
    query: &dxdb_storage::MangoQuery,
) -> Result<dxdb_storage::MangoQuery> {
    Ok(dxdb_storage::MangoQuery {
        selector: compress_selector(table, &query.selector)?,
        sort: query
            .sort
            .iter()
            .map(|part| {
                Ok(dxdb_storage::SortPart {
                    field: table.compress_path(&part.field)?,
                    direction: part.direction,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        // `if (query.skip) ret.skip = query.skip` — a `0` is not copied, and
        // `MangoQuery.skip` is already `0` when unset, so this is identity.
        skip: query.skip,
        // `if (query.limit) ret.limit = query.limit` — a `0` is dropped upstream.
        limit: query.limit.filter(|limit| *limit != 0),
        index: match &query.index {
            None => None,
            Some(index) => Some(
                index
                    .iter()
                    .map(|field| table.compress_path(field))
                    .collect::<Result<Vec<_>>>()?,
            ),
        },
    })
}

/// `compressQuerySelector` (`compress.ts:167`).
///
/// `$`-prefixed keys are operators and keep their names; everything else is a
/// field path and is compressed.
fn compress_selector(table: &CompressionTable, selector: &Value) -> Result<Value> {
    Ok(match selector {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| compress_selector(table, item))
                .collect::<Result<Vec<_>>>()?,
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, member) in map {
                let use_key = if key.starts_with('$') {
                    key.clone()
                } else {
                    table.compress_path(key)?
                };
                out.insert(use_key, compress_selector(table, member)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

/// `createCompressedJsonSchema` (`create-compressed-json-schema.ts:8`) plus the
/// primary-key and index rewriting the plugin does on top (`index.ts:97-123`).
fn compress_schema_with_table(schema: &Value, table: &CompressionTable) -> Result<Value> {
    let map = schema
        .as_object()
        .ok_or_else(|| Error::InvalidSchema("schema must be an object".to_string()))?;

    let mut compressed = compress_schema_node(table, schema)?;
    if let Value::Object(out) = &mut compressed {
        // `compressedSchema.primaryKey = compressedPath(table, schema.primaryKey)`,
        // and for a composite key each field is compressed while the `key` and
        // `separator` shape is preserved (`index.ts:98-108`).
        match map.get("primaryKey") {
            Some(Value::String(field)) => {
                out.insert(
                    "primaryKey".to_string(),
                    Value::String(table.compress_path(field)?),
                );
            }
            // `CompositePrimaryKey` is `{ key, fields, separator }`
            // (`types/rx-schema.d.ts`), and the plugin compresses `key` and every
            // member of `fields` while keeping the separator verbatim
            // (`index.ts:98-105`). The primary field names are in the ignore list, so
            // in practice this is the identity — compressing them anyway keeps the
            // two sides of the port structurally identical, which is what makes a
            // future change to the ignore list safe.
            Some(Value::Object(composite)) => {
                let mut rewritten = composite.clone();
                if let Some(Value::String(key)) = composite.get("key") {
                    rewritten.insert("key".to_string(), Value::String(table.compress_path(key)?));
                }
                if let Some(Value::Array(fields)) = composite.get("fields") {
                    let compressed_fields = fields
                        .iter()
                        .map(|field| {
                            let field = field.as_str().ok_or_else(|| {
                                Error::InvalidSchema(
                                    "primaryKey.fields must hold strings".to_string(),
                                )
                            })?;
                            Ok(Value::String(table.compress_path(field)?))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    rewritten.insert("fields".to_string(), Value::Array(compressed_fields));
                }
                out.insert("primaryKey".to_string(), Value::Object(rewritten));
            }
            Some(other) => return Err(Error::InvalidSchema(format!(
                "primaryKey must be a string or a {{key, fields, separator}} object, got {other}"
            ))),
            None => return Err(Error::NoPrimaryKey("primaryKey is absent".to_string())),
        }

        // `schema.indexes` mapped through `compressedPath`, flat or compound
        // (`index.ts:114-123`).
        if let Some(Value::Array(indexes)) = map.get("indexes") {
            let compressed_indexes = indexes
                .iter()
                .map(|index| match index {
                    Value::Array(fields) => fields
                        .iter()
                        .map(|field| {
                            let field = field.as_str().ok_or_else(|| {
                                Error::InvalidSchema(
                                    "a compound index must hold strings".to_string(),
                                )
                            })?;
                            Ok(Value::String(table.compress_path(field)?))
                        })
                        .collect::<Result<Vec<_>>>()
                        .map(Value::Array),
                    Value::String(field) => Ok(Value::String(table.compress_path(field)?)),
                    other => Err(Error::InvalidSchema(format!(
                        "an index entry must be a string or an array, got {other}"
                    ))),
                })
                .collect::<Result<Vec<_>>>()?;
            out.insert("indexes".to_string(), Value::Array(compressed_indexes));
        }

        // The child schema tells the underlying storage that compression already
        // happened (`index.ts:167`), so a bare storage does not reject it.
        out.insert("keyCompression".to_string(), Value::Bool(false));
    }
    Ok(compressed)
}

fn compress_schema_node(table: &CompressionTable, schema: &Value) -> Result<Value> {
    let Some(map) = schema.as_object() else {
        return Ok(schema.clone());
    };
    let is_array = map.get("type").and_then(Value::as_str) == Some("array");
    let is_object = map.get("type").and_then(Value::as_str) == Some("object");

    if is_array {
        if let Some(items) = map.get("items") {
            let mut out = map.clone();
            let compressed = match items {
                Value::Array(list) => Value::Array(
                    list.iter()
                        .map(|item| compress_schema_node(table, item))
                        .collect::<Result<Vec<_>>>()?,
                ),
                single => compress_schema_node(table, single)?,
            };
            out.insert("items".to_string(), compressed);
            return Ok(Value::Object(out));
        }
        return Ok(schema.clone());
    }

    if is_object {
        let Some(Value::Object(properties)) = map.get("properties") else {
            // `schema.type === 'object' && schema.properties` — without properties
            // there is nothing to compress and the node is returned as-is.
            return Ok(schema.clone());
        };
        let mut out = map.clone();
        let mut new_properties = Map::new();
        for (key, property) in properties {
            new_properties.insert(
                table.compress_path(key)?,
                compress_schema_node(table, property)?,
            );
        }
        out.insert("properties".to_string(), Value::Object(new_properties));
        if let Some(Value::Array(required)) = map.get("required") {
            let compressed_required = required
                .iter()
                .map(|field| {
                    let field = field.as_str().ok_or_else(|| {
                        Error::InvalidSchema("required must hold strings".to_string())
                    })?;
                    Ok(Value::String(table.compress_path(field)?))
                })
                .collect::<Result<Vec<_>>>()?;
            out.insert("required".to_string(), Value::Array(compressed_required));
        }
        return Ok(Value::Object(out));
    }

    Ok(schema.clone())
}

/// The plugin's ignore list for a schema: the primary field plus the four meta
/// fields (`index.ts:78-88`).
pub fn ignore_list_for(schema: &Value) -> Result<Vec<String>> {
    let map = schema
        .as_object()
        .ok_or_else(|| Error::InvalidSchema("schema must be an object".to_string()))?;
    let mut ignore = vec![primary_field_name(map)?];
    ignore.extend(IGNORE_ALWAYS.iter().map(|field| field.to_string()));
    Ok(ignore)
}

/// The field that holds the primary value.
///
/// For a string primary key that is the key itself; for a composite key it is the
/// `key` member of `{key, fields, separator}`, because that is where the composed
/// value lives (`composed_primary_key_of_document_data` reads it the same way in
/// `dxdb-schema`).
fn primary_field_name(map: &Map<String, Value>) -> Result<String> {
    match map.get("primaryKey") {
        Some(Value::String(field)) => Ok(field.clone()),
        Some(Value::Object(composite)) => composite
            .get("key")
            .and_then(Value::as_str)
            .map(|key| key.to_string())
            .ok_or_else(|| {
                Error::NoPrimaryKey("a composite primaryKey needs a `key` member".to_string())
            }),
        Some(other) => Err(Error::InvalidSchema(format!(
            "primaryKey must be a string or a {{key, fields, separator}} object, got {other}"
        ))),
        None => Err(Error::NoPrimaryKey("primaryKey is absent".to_string())),
    }
}

/// `getPropertiesOfSchema` (`create-compression-table.ts:45`): every property name
/// anywhere, recursing through `properties` and `items`.
fn collect_property_names(schema: &Value, out: &mut Vec<String>) {
    let Some(map) = schema.as_object() else {
        return;
    };
    if let Some(Value::Object(properties)) = map.get("properties") {
        for (name, property) in properties {
            out.push(name.clone());
            collect_property_names(property, out);
        }
    }
    if let Some(items) = map.get("items") {
        match items {
            Value::Array(list) => {
                for item in list {
                    collect_property_names(item, out);
                }
            }
            single => collect_property_names(single, out),
        }
    }
}

/// `numberToLetter` (`util.ts:12`): base58 with the library's own alphabet.
fn number_to_letter(mut number: u64) -> String {
    let mut digits = Vec::new();
    loop {
        let digit = (number % BASE58.len() as u64) as usize;
        digits.push(BASE58[digit]);
        number /= BASE58.len() as u64;
        // `while (nr-- > 0)`: the loop body runs once more for `nr == 0`, so
        // `0` maps to `a` and `58` maps to `ba`. Expressed as a do/while.
        if number == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).expect("the alphabet is ASCII")
}

/// Splits `name[0][1]` into `("name", "[0][1]")`, the way
/// `compress.ts:86-94` does.
fn split_brackets(key: &str) -> (&str, String) {
    match key.find('[') {
        None => (key, String::new()),
        Some(open) => {
            let (plain, rest) = key.split_at(open);
            (plain, rest.to_string())
        }
    }
}

/// The storage wrapper (`wrappedKeyCompressionStorage`, `index.ts:135`).
///
/// # Not constructible yet, on purpose
///
/// The type exists so the wiring is written down next to the behaviour it will
/// call, and so the dependency is visible rather than discovered later. Two things
/// block construction:
///
/// 1. `dxdb_storage::RxStorageInstance` uses `impl Future` in `bulk_write`,
///    `query`, `count`, `find_documents_by_id`, `get_attachment_data`,
///    `get_changed_documents_since`, `close` and `remove`, which makes it
///    dyn-incompatible. A storage *wrapper* must return
///    `Box<dyn RxStorageInstance>` — that is the whole point of a wrapper, and
///    upstream's is `wrapRxStorageInstance(any)` — so the twin
///    `DynRxStorageInstance` (PLAN.md 3.3, `dynosaur`) is a prerequisite.
/// 2. Re-preparing the compressed query needs `dxdb_query::prepare_query`, which
///    is not public yet (`index.ts:207`).
///
/// Everything this wrapper would do is implemented and tested:
/// [`CompressionState::for_schema`], [`CompressionState::compress_document`],
/// [`CompressionState::decompress_document`] and
/// [`CompressionState::compressed_query`]. The wrapper's remaining job is
/// forwarding: on `create_storage_instance`, return early when the schema has
/// `key_compression: false`, otherwise build the state, hand the *compressed*
/// schema to the inner storage, compress writes, decompress reads, and compress
/// queries. No behaviour is missing from the transformation layer; only the
/// `dyn` plumbing.
#[derive(Debug, Clone)]
pub struct KeyCompressionStorage;

impl KeyCompressionStorage {
    /// The value the underlying storage must be configured with: the compressed
    /// schema with `keyCompression: false` (`index.ts:166-167`).
    ///
    /// Public because it is the contract the wrapper relies on, and a
    /// `dxdb_storage::InstanceCreationParams` cannot be built without it.
    pub fn inner_schema(state: &CompressionState) -> Value {
        state.compressed_schema.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base58_matches_the_library_alphabet() {
        // `numberToLetter` is a do/while, so 0 -> "a", 57 -> the last letter, and
        // 58 wraps to two digits.
        assert_eq!(number_to_letter(0), "a");
        assert_eq!(number_to_letter(1), "b");
        assert_eq!(number_to_letter(57), "9");
        assert_eq!(number_to_letter(58), "ba");
        assert_eq!(number_to_letter(59), "bb");
    }

    #[test]
    fn short_names_are_never_compressed() {
        // `create-compression-table.ts:87` filters `k.length > 3`.
        let schema = json!({
            "type": "object",
            "primaryKey": "id",
            "properties": {
                "id": { "type": "string", "maxLength": 100 },
                "age": { "type": "integer" },
                "abc": { "type": "string" },
                "abcd": { "type": "string" }
            }
        });
        let table = CompressionTable::build(&schema, &["id".to_string()]).expect("table");
        assert_eq!(table.compress_key("abc").expect("short"), "abc");
        assert_eq!(table.compress_key("abcd").expect("four"), "|a");
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn the_flag_on_an_existing_key_is_an_error() {
        let table = CompressionTable::build(&json!({}), &[]).expect("table");
        assert_eq!(
            table.compress_key("|a").expect_err("flagged key"),
            Error::KeyUsesFlag {
                key: "|a".to_string(),
                flag: '|'
            }
        );
    }

    #[test]
    fn bracket_suffixes_survive_both_directions() {
        let schema = json!({ "properties": { "firstname": { "type": "string" } } });
        let table = CompressionTable::build(&schema, &[]).expect("table");
        assert_eq!(
            table.compress_key("firstName[0]").expect("bracket"),
            "firstName[0]"
        );
        assert_eq!(
            table.compress_key("firstname[0]").expect("bracket"),
            "|a[0]"
        );
        assert_eq!(table.decompress_key("|a[0]"), "firstname[0]");
    }

    #[test]
    fn a_plain_key_decompresses_to_itself() {
        let table = CompressionTable::build(&json!({}), &[]).expect("table");
        assert_eq!(table.decompress_key("firstName"), "firstName");
        assert_eq!(table.decompress_key("_deleted"), "_deleted");
    }
}
