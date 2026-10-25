//! The meta instance: the schema of its rows, and the assumed-master state they hold.
//!
//! Ports `src/replication-protocol/meta-instance.ts` (195 lines).
//!
//! # Two kinds of row in one collection
//!
//! | `isCheckpoint` | `itemId` | carries |
//! | --- | --- | --- |
//! | `"1"` | `"up"` / `"down"` | `checkpointData` |
//! | `"0"` | a document primary key | `docData` (the master's state), maybe `isResolvedConflict` |
//!
//! The primary key is composed: `itemId` and `isCheckpoint` joined with `"|"`. So a
//! checkpoint row's id is literally `"down|1"` and a document row's is
//! `"passport-abc|0"`. That string is persisted, so it is wire behaviour.
//!
//! # Why the schema is generated, and what the arithmetic is for
//!
//! Upstream builds a schema per replication (`getRxReplicationMetaInstanceSchema`) rather
//! than shipping a fixed one, because three things depend on the *fork's* schema:
//!
//! 1. `version` and `keyCompression` are copied, so a schema migration changes the meta
//!    collection's identity too.
//! 2. `properties.docData.properties` is copied from the fork, so validation of a stored
//!    master state uses the same field definitions.
//! 3. `id.maxLength` is derived from the fork's primary-key length, because the composed
//!    id is `itemId + "|" + isCheckpoint` and it has to fit.
//!
//! That derivation is:
//!
//! ```text
//! parentPrimaryKeyLength = getLengthOfPrimaryKey(forkSchema)   // properties[pk].maxLength
//! idLength               = max(parentPrimaryKeyLength + 2, 6) // +1 separator +1 flag
//! itemId.maxLength       = max(parentPrimaryKeyLength, 4)     // 'down' must fit
//! ```
//!
//! Both numbers are in the persisted schema, and therefore in its hash, so they have to
//! match a JS peer's exactly. The `4` is upstream's comment: `'DOWN'` has four characters,
//! even though the value written is the lowercase `'down'`.
//!
//! # `isResolvedConflict` must be absent, not null
//!
//! Upstream: "Sending isResolvedConflict with the value undefined will throw a schema
//! validation error because it must be either not set or have a string"
//! (`meta-instance.ts:170-177`). So [`get_meta_write_row`] inserts the key only when it
//! has a value; a `None` removes it rather than writing `null`.

use dxdb_json::{Map, Value};
use dxdb_storage::{DocumentData, DocumentWriteData, RxStorageInstance, StorageError};

use crate::checkpoint::meta_doc_id;
use crate::state::ReplicationState;

/// Upstream's schema title, which is part of the schema's identity and therefore its hash.
pub const META_INSTANCE_SCHEMA_TITLE: &str = "RxReplicationProtocolMetaData";

/// The minimum `itemId` length: `"down"` must fit.
///
/// Upstream's comment names `'DOWN'`; the value written is lowercase, and four characters
/// covers both.
pub const MIN_ITEM_ID_LENGTH: usize = 4;

/// The derived lengths a meta schema needs from the fork schema.
///
/// Returned as a struct rather than recomputed per call so the arithmetic exists once and
/// a test can pin it against a table of primary-key lengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetaIdLengths {
    /// The composed id's maximum length.
    pub id_length: usize,
    /// The `itemId` field's maximum length.
    pub item_id_length: usize,
}

impl MetaIdLengths {
    /// Derives both lengths from the fork schema's primary-key length.
    ///
    /// ```text
    /// id_length      = max(parent + 2, 6)   // +1 for '|', +1 for the isCheckpoint flag
    /// item_id_length = max(parent, 4)       // 'down' must fit
    /// ```
    pub fn from_parent_primary_key_length(parent_length: usize) -> Self {
        let min_id_length = MIN_ITEM_ID_LENGTH + 2; // 'down|1' must fit
        MetaIdLengths {
            id_length: (parent_length + 2).max(min_id_length),
            item_id_length: parent_length.max(MIN_ITEM_ID_LENGTH),
        }
    }
}

/// The maximum length a fork schema declares for its primary key.
///
/// Ports `getLengthOfPrimaryKey` (`rx-schema-helper.ts:136-142`): read
/// `properties[primaryKey].maxLength`, and fail when it is absent.
///
/// For a **composite** primary key upstream reads `properties[primaryKey.key]`, and
/// `fillWithDefaultSettings` does not inject that property, so the lookup fails unless the
/// schema declared one. That failure is reproduced rather than papered over with a
/// default — a made-up length would produce a different persisted schema hash.
pub fn parent_primary_key_length(
    schema: &dxdb_storage::StorageSchema,
) -> Result<usize, StorageError> {
    let raw = schema
        .raw
        .as_object()
        .ok_or_else(|| StorageError::InvalidSchema("schema must be an object".to_string()))?;
    let properties = raw
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            StorageError::InvalidSchema("schema.properties must be an object".to_string())
        })?;
    let primary = properties
        .get(&schema.primary_path)
        .and_then(Value::as_object)
        .ok_or_else(|| {
            StorageError::InvalidSchema(format!(
                "schema.properties.{} is missing; the replication meta schema needs its maxLength",
                schema.primary_path
            ))
        })?;
    let max_length = primary
        .get("maxLength")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            StorageError::InvalidSchema(format!(
                "schema.properties.{}.maxLength is missing; the replication meta schema needs it",
                schema.primary_path
            ))
        })?;
    usize::try_from(max_length).map_err(|_| {
        StorageError::InvalidSchema("primary key maxLength does not fit usize".to_string())
    })
}

/// Builds the meta instance's schema.
///
/// Ports `getRxReplicationMetaInstanceSchema` (`meta-instance.ts:26-99`). Field order
/// follows upstream's literal so a stored schema compares equal to a JS peer's, and so a
/// schema hash computed over it matches.
///
/// `encrypted` is `true` when the fork schema has encrypted fields: upstream then adds
/// `docData` to the `encrypted` list, because a master state is as sensitive as the
/// document it came from.
pub fn replication_meta_instance_schema(
    fork_schema: &dxdb_storage::StorageSchema,
    encrypted: bool,
) -> Result<Value, StorageError> {
    let parent_length = parent_primary_key_length(fork_schema)?;
    let lengths = MetaIdLengths::from_parent_primary_key_length(parent_length);

    let mut properties = Map::new();

    let mut id = Map::new();
    id.insert("type".to_string(), Value::String("string".to_string()));
    id.insert("minLength".to_string(), Value::from(1u64));
    id.insert(
        "maxLength".to_string(),
        Value::from(lengths.id_length as u64),
    );
    properties.insert("id".to_string(), Value::Object(id));

    let mut is_checkpoint = Map::new();
    is_checkpoint.insert("type".to_string(), Value::String("string".to_string()));
    is_checkpoint.insert(
        "enum".to_string(),
        Value::Array(vec![
            Value::String("0".to_string()),
            Value::String("1".to_string()),
        ]),
    );
    is_checkpoint.insert("minLength".to_string(), Value::from(1u64));
    is_checkpoint.insert("maxLength".to_string(), Value::from(1u64));
    properties.insert("isCheckpoint".to_string(), Value::Object(is_checkpoint));

    let mut item_id = Map::new();
    item_id.insert("type".to_string(), Value::String("string".to_string()));
    item_id.insert(
        "maxLength".to_string(),
        Value::from(lengths.item_id_length as u64),
    );
    properties.insert("itemId".to_string(), Value::Object(item_id));

    let mut checkpoint_data = Map::new();
    checkpoint_data.insert("type".to_string(), Value::String("object".to_string()));
    checkpoint_data.insert("additionalProperties".to_string(), Value::Bool(true));
    properties.insert("checkpointData".to_string(), Value::Object(checkpoint_data));

    // `docData.properties` is copied from the fork schema, per upstream. A fork schema
    // without a `properties` object cannot produce this, which is an error rather than an
    // empty object: an empty one would accept any document.
    let fork_properties = fork_schema
        .raw
        .get("properties")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let mut doc_data = Map::new();
    doc_data.insert("type".to_string(), Value::String("object".to_string()));
    doc_data.insert("properties".to_string(), fork_properties);
    properties.insert("docData".to_string(), Value::Object(doc_data));

    let mut is_resolved_conflict = Map::new();
    is_resolved_conflict.insert("type".to_string(), Value::String("string".to_string()));
    properties.insert(
        "isResolvedConflict".to_string(),
        Value::Object(is_resolved_conflict),
    );

    let mut primary_key = Map::new();
    primary_key.insert("key".to_string(), Value::String("id".to_string()));
    primary_key.insert(
        "fields".to_string(),
        Value::Array(vec![
            Value::String("itemId".to_string()),
            Value::String("isCheckpoint".to_string()),
        ]),
    );
    primary_key.insert("separator".to_string(), Value::String("|".to_string()));

    let mut schema = Map::new();
    schema.insert(
        "title".to_string(),
        Value::String(META_INSTANCE_SCHEMA_TITLE.to_string()),
    );
    schema.insert("primaryKey".to_string(), Value::Object(primary_key));
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("version".to_string(), Value::from(fork_schema.version));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));
    schema.insert("properties".to_string(), Value::Object(properties));
    schema.insert(
        "keyCompression".to_string(),
        Value::Bool(fork_schema.key_compression),
    );
    schema.insert(
        "required".to_string(),
        Value::Array(vec![
            Value::String("id".to_string()),
            Value::String("isCheckpoint".to_string()),
            Value::String("itemId".to_string()),
        ]),
    );
    if encrypted {
        schema.insert(
            "encrypted".to_string(),
            Value::Array(vec![Value::String("docData".to_string())]),
        );
    }
    Ok(Value::Object(schema))
}

/// The assumed-master state for one document: what the fork believes the master holds.
///
/// Ports `getAssumedMasterState` (`meta-instance.ts:107-144`) for one id. Upstream returns
/// a map keyed by `itemId` built from the found rows *only* — a row that does not exist
/// yields no entry rather than an entry with no state, which is how "the master has never
/// seen this document" is distinguished from "the master has it and it happens to be
/// empty".
#[derive(Clone, Debug, PartialEq)]
pub struct AssumedMasterState {
    /// The master's document state.
    pub doc_data: DocumentData,
    /// The whole meta row, because callers need its `_rev` and `isResolvedConflict`.
    pub meta_document: DocumentData,
}

impl AssumedMasterState {
    /// The fork revision this assumed-master state came from, when it was written while
    /// resolving a conflict.
    ///
    /// Ports the `isResolvedConflict` comparison both directions make
    /// (`downstream.ts:314-322`, `upstream.ts:344-348`).
    pub fn is_resolved_conflict(&self) -> Option<&str> {
        self.meta_document
            .get("isResolvedConflict")
            .and_then(Value::as_str)
    }
}

/// Reads the assumed-master states for a batch of document ids.
///
/// Ports `getAssumedMasterState` (`meta-instance.ts:107-144`). One storage read for the
/// whole batch, `with_deleted: true` because a meta row for a document that was
/// hard-removed is still meaningful bookkeeping and its absence would be indistinguishable
/// from "never replicated".
pub async fn get_assumed_master_state<S>(
    state: &ReplicationState<S>,
    doc_ids: &[String],
) -> Result<std::collections::HashMap<String, AssumedMasterState>, StorageError>
where
    S: RxStorageInstance,
{
    if doc_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let meta_ids: Vec<String> = doc_ids.iter().map(|id| meta_doc_id(id)).collect();
    let rows = state
        .meta_instance()
        .find_documents_by_id(meta_ids, true)
        .await?;

    let mut out = std::collections::HashMap::new();
    for row in rows {
        // Keyed by `itemId`, which is the document's primary value — not by `id`, which is
        // the composed one. Upstream does the same (`metaDoc.itemId`).
        let Some(item_id) = row.get("itemId").and_then(Value::as_str) else {
            continue;
        };
        let Some(doc_data) = row.get("docData").cloned() else {
            continue;
        };
        let Ok(doc_data) = DocumentData::from_value(doc_data) else {
            continue;
        };
        out.insert(
            item_id.to_string(),
            AssumedMasterState {
                doc_data,
                meta_document: row,
            },
        );
    }
    Ok(out)
}

/// Builds the meta row that records a new assumed-master state.
///
/// Ports `getMetaWriteRow` (`meta-instance.ts:147-195`). Steps:
///
/// 1. Start from the previous row cloned, or from a fresh row with `_meta.lwt = 0`.
/// 2. Replace `docData` with the new master state.
/// 3. Set `isResolvedConflict` **only** when there is a value.
/// 4. Fresh `_meta.lwt` from the injected clock.
/// 5. Composed `id`, then `_rev` from `create_revision(checkpoint_key, previous)`.
///
/// `is_resolved_conflict` is the fork revision the resolution wrote, and it is what the two
/// directions later compare against a fork `_rev` to agree that "this state is a resolution
/// that still has to be pushed".
pub fn get_meta_write_row<S>(
    state: &ReplicationState<S>,
    new_master_doc_state: &DocumentData,
    previous: Option<&DocumentData>,
    is_resolved_conflict: Option<&str>,
) -> Result<dxdb_storage::BulkWriteRow, StorageError>
where
    S: RxStorageInstance,
{
    let doc_id = new_master_doc_state
        .id(state.primary_path())
        .map_err(|error| StorageError::InvalidDocument(error.to_string()))?
        .to_string();

    let mut row = match previous {
        // `flatCloneDocWithMeta`: clone the document and its `_meta`, so a shared `_meta`
        // object cannot be mutated underneath another owner.
        Some(previous) => {
            let mut cloned = previous.clone();
            let meta = previous.meta().cloned().unwrap_or_default();
            cloned.set("_meta", Value::Object(meta));
            cloned
        }
        None => {
            let mut map = Map::new();
            map.insert("id".to_string(), Value::String(String::new()));
            map.insert("isCheckpoint".to_string(), Value::String("0".to_string()));
            map.insert("itemId".to_string(), Value::String(doc_id.clone()));
            map.insert(
                "docData".to_string(),
                new_master_doc_state.clone().into_value(),
            );
            map.insert("_attachments".to_string(), Value::Object(Map::new()));
            map.insert("_deleted".to_string(), Value::Bool(false));
            map.insert("_rev".to_string(), Value::String(String::new()));
            let mut meta = Map::new();
            meta.insert("lwt".to_string(), Value::from(0u64));
            map.insert("_meta".to_string(), Value::Object(meta));
            DocumentData::new(map)
        }
    };

    row.set("docData", new_master_doc_state.clone().into_value());

    match is_resolved_conflict {
        // Only insert the key when there is a value: `null` would fail validation.
        Some(revision) => {
            row.set("isResolvedConflict", Value::String(revision.to_string()));
        }
        None => {
            row.as_object_mut().remove("isResolvedConflict");
        }
    }

    row.set_lwt(state.lwt())?;
    row.set("id", Value::String(meta_doc_id(&doc_id)));
    row.set(
        "_rev",
        Value::String(dxdb_storage::create_revision(
            state.checkpoint_key(),
            previous.and_then(DocumentData::rev),
        )),
    );

    Ok(crate::checkpoint::meta_write_row(previous.cloned(), row))
}

/// The write row for a meta document, exposed so the direction modules can build one.
pub fn meta_row(
    previous: Option<DocumentData>,
    document: DocumentData,
) -> dxdb_storage::BulkWriteRow {
    dxdb_storage::BulkWriteRow {
        previous,
        document: DocumentWriteData::new(document),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fork_schema(max_length: Option<u64>) -> dxdb_storage::StorageSchema {
        let mut properties = Map::new();
        let mut passport = Map::new();
        passport.insert("type".to_string(), Value::String("string".to_string()));
        if let Some(max_length) = max_length {
            passport.insert("maxLength".to_string(), Value::from(max_length));
        }
        properties.insert("passportId".to_string(), Value::Object(passport));
        let schema = dxdb_json::parse(&format!(
            r#"{{"version":3,"keyCompression":false,"primaryKey":"passportId","type":"object","properties":{}}}"#,
            dxdb_json::to_js_json(&Value::Object(properties))
        ))
        .expect("valid");
        dxdb_storage::StorageSchema::from_value(&schema).expect("valid schema")
    }

    #[test]
    fn id_lengths_come_from_the_arithmetic_not_a_default() {
        // parent 100 -> id 102, itemId 100
        let lengths = MetaIdLengths::from_parent_primary_key_length(100);
        assert_eq!(lengths.id_length, 102);
        assert_eq!(lengths.item_id_length, 100);

        // parent 1 -> id must still hold 'down|1' (6), itemId must still hold 'down' (4)
        let lengths = MetaIdLengths::from_parent_primary_key_length(1);
        assert_eq!(lengths.id_length, 6);
        assert_eq!(lengths.item_id_length, 4);

        // parent 4 -> id 6 (4+2), itemId 4
        let lengths = MetaIdLengths::from_parent_primary_key_length(4);
        assert_eq!(lengths.id_length, 6);
        assert_eq!(lengths.item_id_length, 4);

        // parent 5 -> id 7, itemId 5
        let lengths = MetaIdLengths::from_parent_primary_key_length(5);
        assert_eq!(lengths.id_length, 7);
        assert_eq!(lengths.item_id_length, 5);
    }

    #[test]
    fn the_schema_carries_the_composed_primary_key_and_copied_version() {
        let schema =
            replication_meta_instance_schema(&fork_schema(Some(100)), false).expect("schema");
        assert_eq!(
            schema.get("title").and_then(Value::as_str),
            Some(META_INSTANCE_SCHEMA_TITLE)
        );
        let primary = schema.get("primaryKey").expect("primaryKey");
        assert_eq!(primary.get("key").and_then(Value::as_str), Some("id"));
        assert_eq!(primary.get("separator").and_then(Value::as_str), Some("|"));
        assert_eq!(
            primary.get("fields").cloned(),
            Some(Value::Array(vec![
                Value::String("itemId".to_string()),
                Value::String("isCheckpoint".to_string())
            ]))
        );
        assert_eq!(
            schema.get("version").and_then(Value::as_u64),
            Some(3),
            "the meta schema copies the fork's version"
        );
        assert_eq!(
            schema.get("additionalProperties").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            schema.get("required").cloned(),
            Some(Value::Array(vec![
                Value::String("id".to_string()),
                Value::String("isCheckpoint".to_string()),
                Value::String("itemId".to_string())
            ]))
        );
    }

    #[test]
    fn encryption_adds_doc_data_to_the_encrypted_list() {
        let plain = replication_meta_instance_schema(&fork_schema(Some(10)), false).expect("plain");
        assert!(plain.get("encrypted").is_none());
        let encrypted =
            replication_meta_instance_schema(&fork_schema(Some(10)), true).expect("encrypted");
        assert_eq!(
            encrypted.get("encrypted").cloned(),
            Some(Value::Array(vec![Value::String("docData".to_string())]))
        );
    }

    #[test]
    fn a_missing_max_length_is_an_error_not_a_default() {
        let schema = fork_schema(None);
        let error = parent_primary_key_length(&schema).expect_err("must fail");
        assert!(
            matches!(error, StorageError::InvalidSchema(_)),
            "expected an InvalidSchema, got {error:?}"
        );
    }

    #[test]
    fn the_schema_properties_are_in_upstream_order() {
        let schema =
            replication_meta_instance_schema(&fork_schema(Some(10)), false).expect("schema");
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        let keys: Vec<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "isCheckpoint",
                "itemId",
                "checkpointData",
                "docData",
                "isResolvedConflict"
            ],
            "field order reaches the persisted schema and its hash"
        );
    }

    #[test]
    fn doc_data_properties_are_copied_from_the_fork_schema() {
        let schema =
            replication_meta_instance_schema(&fork_schema(Some(10)), false).expect("schema");
        let doc_data = schema
            .get("properties")
            .and_then(|p| p.get("docData"))
            .expect("docData");
        let properties = doc_data
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(
            properties.contains_key("passportId"),
            "the fork's field definitions must be carried over"
        );
    }
}
