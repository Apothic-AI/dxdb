//! Proves the blocker: the testkit's stopgap `fill_with_default_settings` omits
//! `_meta.properties.lwt`, so *any* storage whose index encoding is the ported upstream
//! encoder cannot run the suite against it.
//!
//! This is diagnosis, not a substitute for the suite. It exists so the claim in the
//! lane report is a measurement rather than a reading of two functions.

mod support;

use std::rc::Rc;

use serde_json::{json, Value};
use support::watchdog::within;

use dxdb_indexeddb::{IndexedDbStorage, MemoryBacking};
use dxdb_storage::{
    BulkWriteRow, DocumentData, DocumentWriteData, InstanceCreationParams, RxStorage,
    RxStorageInstance, StorageSchema,
};

fn indexed(value: Value) -> IndexedDbStorage<MemoryBacking> {
    let _ = value;
    IndexedDbStorage::new(Rc::new(MemoryBacking::new()))
}

/// The testkit's own filled schema, byte for byte.
fn testkit_filled_schema() -> Value {
    dxdb_testkit::harness::fill_with_default_settings(dxdb_testkit::schemas::test_data_schema())
}

/// The same schema, with the one thing the real `fill_with_default_settings` provides
/// and the stopgap does not: `_meta.properties.lwt` (see `dxdb-schema/src/lib.rs:95`).
fn fully_filled_schema() -> Value {
    let mut schema = testkit_filled_schema();
    let meta = schema
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("_meta"))
        .expect("_meta is present");
    *meta = json!({
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "lwt": {"type": "number", "minimum": 1, "maximum": 1e15, "multipleOf": 0.01}
        },
        "required": ["lwt"]
    });
    schema
}

fn params(schema_value: Value, collection: &str) -> InstanceCreationParams {
    InstanceCreationParams {
        database_instance_token: "diagnosis".to_string(),
        database_name: "diagnosis".to_string(),
        collection_name: collection.to_string(),
        schema: StorageSchema::from_value(&schema_value).expect("valid schema"),
        options: json!({}),
        multi_instance: false,
        dev_mode: false,
    }
}

fn document(key: &str) -> DocumentData {
    DocumentData::from_value(json!({
        "key": key,
        "value": "v",
        "_deleted": false,
        "_rev": format!("1-{key}"),
        "_meta": {"lwt": 1000.0}
    }))
    .expect("valid document")
}

/// With the testkit's stopgap schema, the write fails at index encoding.
///
/// The assertion is on `_meta.lwt` specifically, because that is the field the missing
/// sub-schema belongs to — not a general "something failed".
#[test]
fn testkit_stopgap_schema_cannot_encode_the_change_feed_index() {
    let storage = indexed(Value::Null);
    let params = params(testkit_filled_schema(), "stopgap");

    let error = within("open + write with stopgap schema", async move {
        let instance = storage.create_storage_instance(params).await?;
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(document("a")))],
                "diagnosis".to_string(),
            )
            .await
    });

    let message = format!("{error:?}");
    assert!(
        message.contains("_meta.lwt") && message.contains("not in the schema"),
        "expected the index encoder to reject `_meta.lwt` because the stopgap fill does \
         not define `properties._meta.properties.lwt`; got: {message}"
    );
}

/// With `_meta.properties.lwt` present, the same write succeeds — so the storage is
/// fine and the schema projection was the blocker.
#[test]
fn the_same_write_succeeds_once_the_meta_subschema_is_present() {
    let storage = indexed(Value::Null);
    let params = params(fully_filled_schema(), "complete");

    let response = within("open + write with complete schema", async move {
        let instance = storage.create_storage_instance(params).await?;
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(document("a")))],
                "diagnosis".to_string(),
            )
            .await
    })
    .expect("bulk_write succeeds with a complete schema");

    assert!(
        response.is_ok(),
        "expected no per-row errors, got {:?}",
        response.error
    );
}
