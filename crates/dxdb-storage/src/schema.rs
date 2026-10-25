//! The schema projection a storage engine needs.
//!
//! **Not** `dxdb-schema` (lane B). Schema validation, meta-field injection,
//! hashing and index defaulting all live there. This module is the subset a
//! storage must know to store and retrieve documents:
//!
//! * which field is the primary key, and whether it is composite;
//! * which index fields exist (the planner decides *which* to use; the storage
//!   must have materialised them);
//! * whether the schema declares attachments;
//! * the version, which participates in the storage's collection key so two
//!   schema versions of the same collection never share state.

use serde_json::{Map, Value};

use crate::document::{FIELD_ATTACHMENTS, FIELD_DELETED, FIELD_REV};

/// What a storage reads out of an `RxJsonSchema`.
///
/// Built by `dxdb-schema` from a filled schema and handed to
/// [`crate::RxStorage::create_storage_instance`] through
/// [`crate::InstanceCreationParams`]. Every index here is stored in the order
/// upstream's `fillWithDefaultSettings` produces: `_deleted` first, primary key
/// last.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageSchema {
    /// Schema version. Part of the storage collection key.
    pub version: u32,
    /// The primary key field path.
    pub primary_path: String,
    /// Set when the schema uses a composite primary key: the field that holds the
    /// concatenated key. Upstream reads the primary value from
    /// `primary_key.key` and computes it from `primary_key.fields`.
    pub composite_primary_fields: Option<Vec<String>>,
    /// Materialised indexes, each already `_deleted`-prefixed and primary-key
    /// suffixed.
    pub indexes: Vec<Vec<String>>,
    /// True when the schema declares `attachments`, which is what makes
    /// `_attachments` payload handling and the `510` error reachable.
    pub has_attachments: bool,
    /// True when the schema sets `keyCompression`, which the key-compression
    /// wrapper needs to know about and a bare storage must reject or ignore.
    pub key_compression: bool,
    /// True when the schema declares `encrypted` fields. A bare storage cannot
    /// honour them, so it must refuse rather than store plaintext.
    pub has_encrypted_fields: bool,
    /// The raw schema, for a storage that wants a field's bounds (SQLite's
    /// per-field columns) or that records it for debugging.
    pub raw: Value,
}

impl StorageSchema {
    /// Reads the storage projection out of a filled schema value.
    ///
    /// Deliberately tolerant about *defaulting* (it does not inject meta fields
    /// or pad indexes — that is `dxdb-schema`'s job, and doing it twice would
    /// disagree with the schema hash) but strict about *shape*: a schema without
    /// a primary key or without an `indexes` array is a bug in the caller.
    ///
    /// `indexes` are taken as-is. A schema that has not been through
    /// `fill_with_default_settings` will therefore produce indexes that are not
    /// `_deleted`-prefixed, which the conformance suite will catch immediately
    /// rather than silently sorting wrongly.
    pub fn from_value(schema: &Value) -> Result<Self, crate::traits::StorageError> {
        let map = schema.as_object().ok_or_else(|| {
            crate::traits::StorageError::InvalidSchema("schema must be a JSON object".to_string())
        })?;

        let version = map
            .get("version")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .try_into()
            .map_err(|_| {
                crate::traits::StorageError::InvalidSchema(
                    "schema version does not fit in u32".to_string(),
                )
            })?;

        let (primary_path, composite_primary_fields) = parse_primary_key(map)?;

        let mut indexes = Vec::new();
        if let Some(list) = map.get("indexes") {
            let list = list.as_array().ok_or_else(|| {
                crate::traits::StorageError::InvalidSchema(
                    "schema.indexes must be an array".to_string(),
                )
            })?;
            for index in list {
                indexes.push(parse_index(index)?);
            }
        }

        let has_attachments = map
            .get("attachments")
            .map(|value| !value.is_null())
            .unwrap_or(false);

        let key_compression = map
            .get("keyCompression")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let has_encrypted_fields = map
            .get("encrypted")
            .and_then(Value::as_array)
            .map(|fields| !fields.is_empty())
            .unwrap_or(false);

        Ok(StorageSchema {
            version,
            primary_path,
            composite_primary_fields,
            indexes,
            has_attachments,
            key_compression,
            has_encrypted_fields,
            raw: schema.clone(),
        })
    }

    /// True when the primary key is composite.
    pub fn is_composite_primary(&self) -> bool {
        self.composite_primary_fields.is_some()
    }

    /// The index the change feed paginates over (`['_meta.lwt', primaryPath]`,
    /// added unconditionally by `fillWithDefaultSettings`). `None` when the schema
    /// has not been filled, which a storage should treat as a programming error
    /// and which the testkit asserts against.
    pub fn change_feed_index(&self) -> Option<&Vec<String>> {
        self.indexes
            .iter()
            .find(|index| index.len() == 2 && index[0] == "_meta.lwt")
    }

    /// The cleanup index (`['_deleted', '_meta.lwt', primaryPath]`, added by the
    /// memory storage itself). `None` when absent.
    pub fn cleanup_index(&self) -> Option<&Vec<String>> {
        self.indexes.iter().find(|index| {
            index.len() == 3
                && index[0] == FIELD_DELETED
                && index[1] == "_meta.lwt"
                && index[2] == self.primary_path
        })
    }

    /// The meta fields every stored document must carry, for a storage that wants
    /// to assert its input in dev mode.
    pub fn required_meta_fields() -> [&'static str; 3] {
        [FIELD_DELETED, FIELD_REV, FIELD_ATTACHMENTS]
    }
}

fn parse_primary_key(
    map: &Map<String, Value>,
) -> Result<(String, Option<Vec<String>>), crate::traits::StorageError> {
    let primary = map.get("primaryKey").ok_or_else(|| {
        crate::traits::StorageError::InvalidSchema("schema has no primaryKey".to_string())
    })?;
    match primary {
        Value::String(path) => Ok((path.clone(), None)),
        Value::Object(obj) => {
            let key = obj.get("key").and_then(Value::as_str).ok_or_else(|| {
                crate::traits::StorageError::InvalidSchema(
                    "composite primaryKey has no 'key' field".to_string(),
                )
            })?;
            let fields = obj
                .get("fields")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    crate::traits::StorageError::InvalidSchema(
                        "composite primaryKey has no 'fields' array".to_string(),
                    )
                })?
                .iter()
                .map(|field| {
                    field.as_str().map(str::to_string).ok_or_else(|| {
                        crate::traits::StorageError::InvalidSchema(
                            "composite primaryKey field is not a string".to_string(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((key.to_string(), Some(fields)))
        }
        _ => Err(crate::traits::StorageError::InvalidSchema(
            "primaryKey must be a string or an object".to_string(),
        )),
    }
}

fn parse_index(index: &Value) -> Result<Vec<String>, crate::traits::StorageError> {
    match index {
        // A single-field index may be written as a bare string upstream.
        Value::String(field) => Ok(vec![field.clone()]),
        Value::Array(fields) => fields
            .iter()
            .map(|field| {
                field.as_str().map(str::to_string).ok_or_else(|| {
                    crate::traits::StorageError::InvalidSchema(
                        "index field is not a string".to_string(),
                    )
                })
            })
            .collect(),
        _ => Err(crate::traits::StorageError::InvalidSchema(
            "index must be a string or an array of strings".to_string(),
        )),
    }
}

/// The key a storage uses for an index: `index.join(',')`, matching upstream's
/// `getMemoryIndexName` (`memory-indexes.ts:34`).
///
/// Shared here rather than duplicated per storage because two storages that name
/// their indexes differently would produce different query plans for the same
/// schema once the planner starts caching by index name.
pub fn index_name(index: &[String]) -> String {
    index.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_simple_primary_key_and_indexes() {
        let schema = json!({
            "version": 3,
            "primaryKey": "id",
            "indexes": [["_deleted", "age", "id"], ["_meta.lwt", "id"]],
            "properties": {}
        });
        let parsed = StorageSchema::from_value(&schema).expect("valid");
        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.primary_path, "id");
        assert!(!parsed.is_composite_primary());
        assert_eq!(parsed.indexes.len(), 2);
        assert_eq!(
            parsed.change_feed_index(),
            Some(&vec!["_meta.lwt".to_string(), "id".to_string()])
        );
    }

    #[test]
    fn reads_a_composite_primary_key() {
        let schema = json!({
            "primaryKey": { "key": "id", "fields": ["first", "second"] },
            "indexes": []
        });
        let parsed = StorageSchema::from_value(&schema).expect("valid");
        assert_eq!(parsed.primary_path, "id");
        assert_eq!(
            parsed.composite_primary_fields,
            Some(vec!["first".to_string(), "second".to_string()])
        );
        assert!(parsed.is_composite_primary());
    }

    #[test]
    fn a_bare_string_index_becomes_a_single_field_index() {
        let schema = json!({ "primaryKey": "id", "indexes": ["firstName"] });
        let parsed = StorageSchema::from_value(&schema).expect("valid");
        assert_eq!(parsed.indexes, vec![vec!["firstName".to_string()]]);
    }

    #[test]
    fn detects_attachments_and_encrypted_fields() {
        let schema = json!({
            "primaryKey": "id",
            "attachments": {},
            "encrypted": ["secret"],
            "indexes": []
        });
        let parsed = StorageSchema::from_value(&schema).expect("valid");
        assert!(parsed.has_attachments);
        assert!(parsed.has_encrypted_fields);
        assert!(!parsed.key_compression);
    }

    #[test]
    fn a_schema_without_a_primary_key_is_rejected() {
        assert!(StorageSchema::from_value(&json!({ "indexes": [] })).is_err());
    }

    #[test]
    fn index_name_matches_the_memory_storage_convention() {
        assert_eq!(
            index_name(&["_deleted".to_string(), "age".to_string(), "id".to_string()]),
            "_deleted,age,id"
        );
    }
}
