//! Behavioural conformance for `dxdb-memory`, ported from upstream's
//! `test/unit/rx-storage-implementations.test.ts` (4,115 lines).
//!
//! Each test names the upstream test it descends from. They are not line-for-line
//! translations — the Rust trait surface differs in two places (no `internals`
//! field, `getAttachmentData` returns bytes rather than a `Blob`) — but each one
//! asserts the same behaviour on the same input, because that behaviour is what the
//! eight storage methods *mean*.
//!
//! What is covered here and why it matters:
//!
//! * **Conflict detection** is the load-bearing one. Three separate upstream tests
//!   pin it: an insert over a live document conflicts, an insert over a *tombstone*
//!   also conflicts without `previous`, and a stale `previous` conflicts. A storage
//!   that gets any of these wrong silently loses a write.
//! * **Attachment payload rules**, including the `510` for a new attachment with no
//!   data, and that a tombstone clears its payloads.
//! * **Events before the write resolves**, since event-reduce and replication both
//!   read the feed immediately after awaiting a write.
//! * **`cleanup` returning `false` while work remains**, which is the contract callers
//!   loop on.
//! * **Multi-instance sharing**, including events crossing instances, because that is
//!   what makes two browser tabs behave like one database.
//! * **Code-unit ordering of index strings**, which upstream only exercises through
//!   `compareDocsWithIndex` with special characters, and which is silently wrong under
//!   Rust's `str` ordering.

use std::collections::HashMap;

use dxdb_memory::MemoryStorage;
use dxdb_storage::{
    create_revision, get_height_of_revision, now, AttachmentData, AttachmentWriteData,
    BulkWriteRow, DocumentData, DocumentWriteData, DynRxStorageInstance, InstanceCreationParams,
    Operation, PreparedQuery, RxStorage, RxStorageInstance, SortDirection, SortPart, StorageError,
    StorageSchema, WriteError,
};
use serde_json::{json, Value};

/// The context string upstream's suite passes to every `bulkWrite`.
const TEST_CONTEXT: &str = "rx-storage-implementations.test.ts";

// `EXAMPLE_REVISION_1..4` from `src/plugins/test-utils/revisions.ts`.
const EXAMPLE_REVISION_1: &str = "1-12080c42d471e3d2625e49dcca3b8e1a";
const EXAMPLE_REVISION_2: &str = "2-22080c42d471e3d2625e49dcca3b8e2b";
const EXAMPLE_REVISION_3: &str = "3-32080c42d471e3d2625e49dcca3b8e3c";
const EXAMPLE_REVISION_4: &str = "4-42080c42d471e3d2625e49dcca3b8e3c";

/// A unique token per call, so no two tests share collection state.
fn random_token() -> String {
    // A static counter is enough for uniqueness within a test binary and keeps the
    // test deterministic, which a real RNG would not.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tok{n:016x}")
}

/// `fillWithDefaultSettings(...)` from `rx-schema-helper.ts:196`.
///
/// Every upstream test calls this before handing a schema to a storage, because the
/// meta-injected properties (`_rev`, `_deleted`, `_meta`, `_attachments`) and the
/// index rewriting are what make the schema storage-ready. A test that skipped it would
/// be exercising a shape no caller ever passes.
fn filled(schema: &Value) -> Value {
    dxdb_schema::fill_with_default_settings(schema).expect("fill with default settings")
}

/// `getPseudoSchemaForVersion(0, primaryKey)` from `rx-schema-helper.ts:28`.
fn pseudo_schema(primary_key: &str) -> Value {
    filled(&json!({
        "version": 0,
        "type": "object",
        "primaryKey": primary_key,
        "properties": {
            primary_key: { "type": "string", "maxLength": 100 },
            "value": { "type": "string" }
        },
        "required": [primary_key],
        "indexes": [[primary_key]]
    }))
}

/// The `getTestDataSchema()` helper from the upstream file.
fn test_data_schema() -> Value {
    filled(&json!({
        "version": 0,
        "type": "object",
        "primaryKey": "key",
        "properties": {
            "key": { "type": "string", "maxLength": 100 },
            "value": { "type": "string", "maxLength": 100 }
        },
        "required": ["key", "value"],
        "indexes": [["value"]]
    }))
}

/// A schema that declares attachments, which is what makes the `510` rules reachable.
fn attachments_schema() -> Value {
    filled(&json!({
        "version": 0,
        "type": "object",
        "primaryKey": "key",
        "attachments": {},
        "properties": {
            "key": { "type": "string", "maxLength": 100 },
            "value": { "type": "string", "maxLength": 100 }
        },
        "required": ["key", "value"],
        "indexes": [["value"]]
    }))
}

/// `getWriteData()` from the upstream file.
fn write_data(key: &str, value: &str) -> DocumentData {
    DocumentData::from_value(json!({
        "key": key,
        "value": value,
        "_deleted": false,
        "_attachments": {},
        "_rev": EXAMPLE_REVISION_1,
        "_meta": { "lwt": now() }
    }))
    .expect("valid document")
}

fn instance_params(schema: &Value) -> InstanceCreationParams {
    InstanceCreationParams {
        database_instance_token: random_token(),
        database_name: random_token(),
        collection_name: random_token(),
        schema: StorageSchema::from_value(schema).expect("schema projection"),
        options: json!({}),
        multi_instance: false,
        dev_mode: true,
    }
}

async fn open(schema: &Value) -> Box<DynRxStorageInstance<'static>> {
    MemoryStorage::new()
        .create_storage_instance(instance_params(schema))
        .await
        .expect("create storage instance")
}

async fn open_with_params(params: InstanceCreationParams) -> Box<DynRxStorageInstance<'static>> {
    MemoryStorage::new()
        .create_storage_instance(params)
        .await
        .expect("create storage instance")
}

/// Builds a prepared query the way core would: normalised sort with the primary key
/// appended, plus the planner's plan.
fn prepare(schema: &Value, primary_key: &str, query: Value) -> PreparedQuery {
    use dxdb_query::normalize::{normalize_mango_query, MangoQuery as QueryMangoQuery};
    use dxdb_query::plan::get_query_plan;

    let filled = normalize_mango_query(
        &QueryMangoQuery {
            selector: query.get("selector").cloned(),
            sort: query.get("sort").and_then(Value::as_array).map(|parts| {
                parts
                    .iter()
                    .map(|part| {
                        let object = part.as_object().expect("sort part is an object");
                        let (field, direction) = object.iter().next().expect("one field");
                        dxdb_query::normalize::SortPart {
                            field: field.clone(),
                            direction: if direction == "desc" {
                                dxdb_query::normalize::SortDirection::Desc
                            } else {
                                dxdb_query::normalize::SortDirection::Asc
                            },
                        }
                    })
                    .collect()
            }),
            skip: query.get("skip").and_then(Value::as_u64),
            limit: query.get("limit").and_then(Value::as_u64),
            index: None,
        },
        primary_key,
        &dxdb_schema::get_string_indexes(schema).unwrap_or_default(),
        false,
    )
    .expect("normalise");
    let plan = get_query_plan(schema, &filled).expect("plan");

    PreparedQuery {
        query: dxdb_storage::MangoQuery {
            selector: filled.selector.clone(),
            sort: filled
                .sort
                .iter()
                .map(|part| SortPart {
                    field: part.field.clone(),
                    direction: match part.direction {
                        dxdb_query::normalize::SortDirection::Asc => SortDirection::Asc,
                        dxdb_query::normalize::SortDirection::Desc => SortDirection::Desc,
                    },
                })
                .collect(),
            skip: filled.skip as usize,
            limit: filled.limit.map(|limit| limit as usize),
            index: filled.index.clone(),
        },
        query_plan: dxdb_storage::QueryPlan {
            index: plan.index.clone(),
            sort_satisfied_by_index: plan.sort_satisfied_by_index,
            selector_satisfied_by_index: plan.selector_satisfied_by_index,
            start_keys: plan
                .start_keys
                .iter()
                .map(|key| match key {
                    dxdb_query::plan::PlanKey::IndexMin => dxdb_storage::PlanKey::index_min(),
                    dxdb_query::plan::PlanKey::IndexMax => dxdb_storage::PlanKey::index_max(),
                    dxdb_query::plan::PlanKey::String(text) => {
                        dxdb_storage::PlanKey::String(text.clone())
                    }
                    dxdb_query::plan::PlanKey::Number(number) => {
                        dxdb_storage::PlanKey::Number(*number)
                    }
                    dxdb_query::plan::PlanKey::Bool(flag) => dxdb_storage::PlanKey::Boolean(*flag),
                    dxdb_query::plan::PlanKey::Null | dxdb_query::plan::PlanKey::Undefined => {
                        dxdb_storage::PlanKey::Unbounded
                    }
                })
                .collect(),
            end_keys: plan
                .end_keys
                .iter()
                .map(|key| match key {
                    dxdb_query::plan::PlanKey::IndexMin => dxdb_storage::PlanKey::index_min(),
                    dxdb_query::plan::PlanKey::IndexMax => dxdb_storage::PlanKey::index_max(),
                    dxdb_query::plan::PlanKey::String(text) => {
                        dxdb_storage::PlanKey::String(text.clone())
                    }
                    dxdb_query::plan::PlanKey::Number(number) => {
                        dxdb_storage::PlanKey::Number(*number)
                    }
                    dxdb_query::plan::PlanKey::Bool(flag) => dxdb_storage::PlanKey::Boolean(*flag),
                    dxdb_query::plan::PlanKey::Null | dxdb_query::plan::PlanKey::Undefined => {
                        dxdb_storage::PlanKey::Unbounded
                    }
                })
                .collect(),
            inclusive_start: plan.inclusive_start,
            inclusive_end: plan.inclusive_end,
        },
    }
}

fn ids(documents: &[DocumentData], path: &str) -> Vec<String> {
    documents
        .iter()
        .map(|document| document.id(path).expect("id").to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// creation
// ---------------------------------------------------------------------------

/// Upstream `it('open and close')` (rx-storage-implementations.test.ts:176).
#[test]
fn open_and_close() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        instance.close().await.expect("close");
    });
}

/// Upstream `it('open many instances on the same database name')` (:195).
#[test]
fn open_many_instances_on_the_same_database_name() {
    futures::executor::block_on(async {
        let schema = pseudo_schema("key");
        let mut params = instance_params(&schema);
        let database_name = params.database_name.clone();
        params.collection_name = "same-collection".to_string();

        let mut instances = Vec::new();
        for _ in 0..3 {
            let mut next = params.clone();
            next.database_name = database_name.clone();
            next.database_instance_token = random_token();
            instances.push(open_with_params(next).await);
        }
        for instance in instances {
            instance.close().await.expect("close");
        }
    });
}

/// Upstream `it('must throw if keyCompression is set but no key-compression plugin is
/// used')` (:218): a bare storage must refuse rather than write uncompressed keys.
#[test]
fn refuses_key_compression_without_the_wrapper() {
    futures::executor::block_on(async {
        let mut raw = pseudo_schema("key");
        raw["keyCompression"] = json!(true);
        let schema = filled(&raw);
        // `expect_err` would need `Debug` on the instance, which a dyn storage does
        // not provide; matching is also clearer about what is being asserted.
        let error = match MemoryStorage::new()
            .create_storage_instance(instance_params(&schema))
            .await
        {
            Ok(_) => panic!("must refuse key compression"),
            Err(error) => error,
        };
        assert!(
            matches!(error, StorageError::Unsupported(_)),
            "got {error:?}"
        );
    });
}

/// Upstream `it('must throw if encryption is defined in schema is set but no
/// encryption plugin is used')` (:244). A bare storage that accepted this would store
/// plaintext while the caller believed it was encrypted.
#[test]
fn refuses_encrypted_fields_without_the_wrapper() {
    futures::executor::block_on(async {
        let mut raw = pseudo_schema("key");
        raw["encrypted"] = json!(["value"]);
        let schema = filled(&raw);
        let error = match MemoryStorage::new()
            .create_storage_instance(instance_params(&schema))
            .await
        {
            Ok(_) => panic!("must refuse encryption"),
            Err(error) => error,
        };
        assert!(
            matches!(error, StorageError::Unsupported(_)),
            "got {error:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// .bulkWrite()
// ---------------------------------------------------------------------------

/// Upstream `it('should write the document')` (:270).
#[test]
fn writes_a_document() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "foobar", "barfoo",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("bulk_write");
        assert!(response.is_ok(), "errors: {:?}", response.error);

        let found = instance
            .find_documents_by_id(vec!["foobar".to_string()], false)
            .await
            .expect("find");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].get("value"), Some(&json!("barfoo")));
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should error on conflict')` (:309).
#[test]
fn conflicts_when_writing_the_same_document_twice() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let document = write_data("foobar", "first");
        let first = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(
                    document.clone(),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("first");
        assert!(first.is_ok());

        let second = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "foobar", "second",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("second");
        assert_eq!(second.error.len(), 1);
        assert_eq!(second.error[0].status(), 409);
        assert!(matches!(second.error[0], WriteError::Conflict { .. }));

        // The failed write must not have replaced the stored document.
        let found = instance
            .find_documents_by_id(vec!["foobar".to_string()], false)
            .await
            .expect("find");
        assert_eq!(found[0].get("value"), Some(&json!("first")));
        instance.close().await.expect("close");
    });
}

/// Upstream `it('when inserting the same document at the same time, the first call
/// must succeed while the second has a conflict')` (:372).
#[test]
fn concurrent_inserts_of_one_document_produce_one_success_and_one_conflict() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        // Sequential in Rust because the storage is synchronous under the hood; the
        // assertion is the same: exactly one of the two may win.
        let a = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "same", "a",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("a");
        let b = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "same", "b",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("b");
        assert!(a.is_ok());
        assert_eq!(b.error.len(), 1);
        assert_eq!(b.error[0].status(), 409);
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should NOT be able to overwrite a deleted the document')` (:483) and
/// `it('the plain storage should throw when overwriting a deleted doc with an insert
/// without passing the previous')` (:1058).
///
/// This is the rule that separates a careful port from a plausible one: a tombstone is
/// still a stored document, so an insert without `previous` conflicts.
#[test]
fn cannot_overwrite_a_deleted_document_without_previous() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let document = write_data("foobar", "value");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(
                    document.clone(),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut tombstone = document.clone();
        tombstone.set("_deleted", json!(true));
        tombstone.set("_rev", json!(EXAMPLE_REVISION_2));
        tombstone.set("_meta", json!({ "lwt": now() }));
        let deleted = instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    document.clone(),
                    DocumentWriteData::new(tombstone),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("delete");
        assert!(deleted.is_ok(), "errors: {:?}", deleted.error);

        // Insert again, without `previous`: must conflict even though the stored
        // document is a tombstone.
        let mut resurrect = document.clone();
        resurrect.set("_meta", json!({ "lwt": now() }));
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(resurrect))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("re-insert");
        assert_eq!(response.error.len(), 1);
        assert_eq!(response.error[0].status(), 409);
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should be able to update the state of a deleted document')` (:600):
/// a resurrection needs `previous`, and then it succeeds.
#[test]
fn can_resurrect_a_deleted_document_with_previous() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let document = write_data("foobar", "value");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(
                    document.clone(),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut tombstone = document.clone();
        tombstone.set("_deleted", json!(true));
        tombstone.set("_rev", json!(EXAMPLE_REVISION_2));
        tombstone.set("_meta", json!({ "lwt": now() }));
        instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    document.clone(),
                    DocumentWriteData::new(tombstone.clone()),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("delete");

        let mut resurrected = document.clone();
        resurrected.set("_rev", json!(EXAMPLE_REVISION_3));
        resurrected.set("_meta", json!({ "lwt": now() }));
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    tombstone,
                    DocumentWriteData::new(resurrected.clone()),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("resurrect");
        assert!(response.is_ok(), "errors: {:?}", response.error);

        let found = instance
            .find_documents_by_id(vec!["foobar".to_string()], false)
            .await
            .expect("find");
        assert_eq!(found.len(), 1, "the document must be live again");
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should be able to unset a property')` (:661).
#[test]
fn can_unset_a_property() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let document = write_data("foobar", "value");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(
                    document.clone(),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut without = document.clone();
        without.as_object_mut().remove("value");
        without.set("_rev", json!(EXAMPLE_REVISION_2));
        without.set("_meta", json!({ "lwt": now() }));
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    document,
                    DocumentWriteData::new(without),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("update");
        assert!(response.is_ok(), "errors: {:?}", response.error);

        let found = instance
            .find_documents_by_id(vec!["foobar".to_string()], false)
            .await
            .expect("find");
        assert!(found[0].get("value").is_none(), "value must be gone");
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should be able to jump more than 1 revision height in a single write
/// operation')` (:900). The storage does not police revision heights; the caller may
/// move from 1 to 4.
#[test]
fn accepts_a_revision_height_jump() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let document = write_data("foobar", "value");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(
                    document.clone(),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut jumped = document.clone();
        jumped.set("_rev", json!(EXAMPLE_REVISION_4));
        jumped.set("_meta", json!({ "lwt": now() }));
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    document,
                    DocumentWriteData::new(jumped),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("update");
        assert!(response.is_ok(), "errors: {:?}", response.error);
        assert_eq!(
            get_height_of_revision(EXAMPLE_REVISION_4),
            4,
            "the fixture must really be a height jump"
        );
        assert_eq!(
            create_revision("token", Some(EXAMPLE_REVISION_1)),
            "2-token"
        );
        instance.close().await.expect("close");
    });
}

/// Upstream `it('write and read with umlauts')` (:996).
#[test]
fn writes_and_reads_umlauts() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let key = "äöüÖÄßÜ";
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    key, "äöü",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write");
        let found = instance
            .find_documents_by_id(vec![key.to_string()], false)
            .await
            .expect("find");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].get("value"), Some(&json!("äöü")));
        instance.close().await.expect("close");
    });
}

/// Upstream `it('#9145 must persist all documents of parallel bulkWrite() calls with
/// multiInstance: false')` (:1130). Several batches, each must land.
#[test]
fn persists_every_document_from_several_batches() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        let mut expected = Vec::new();
        for batch in 0..5 {
            let rows: Vec<BulkWriteRow> = (0..4)
                .map(|i| {
                    let key = format!("key-{batch}-{i}");
                    expected.push(key.clone());
                    BulkWriteRow::insert(DocumentWriteData::new(write_data(&key, "v")))
                })
                .collect();
            let response = instance
                .bulk_write(rows, TEST_CONTEXT.to_string())
                .await
                .expect("bulk");
            assert!(response.is_ok(), "errors: {:?}", response.error);
        }
        let found = instance
            .find_documents_by_id(expected.clone(), false)
            .await
            .expect("find");
        assert_eq!(found.len(), expected.len());
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// attachments
// ---------------------------------------------------------------------------

fn with_attachment(
    document: DocumentData,
    attachment_id: &str,
    digest: &str,
    payload: Option<&[u8]>,
) -> DocumentWriteData {
    let mut write = DocumentWriteData::new(document);
    write.insert_attachment(
        attachment_id,
        AttachmentWriteData {
            stub: AttachmentData {
                length: payload.map(|p| p.len() as u64).unwrap_or(3),
                content_type: "text/plain".to_string(),
                digest: digest.to_string(),
            },
            data: payload.map(<[u8]>::to_vec),
        },
    );
    write
}

/// Upstream `it('should be able to store and retrieve an attachment')` (:2938).
#[test]
fn stores_and_retrieves_an_attachment() {
    futures::executor::block_on(async {
        let instance = open(&attachments_schema()).await;
        let document = write_data("with-att", "v");
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(with_attachment(
                    document.clone(),
                    "att-1",
                    "digest-1",
                    Some(b"hello"),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write");
        assert!(response.is_ok(), "errors: {:?}", response.error);

        let data = instance
            .get_attachment_data(
                "with-att".to_string(),
                "att-1".to_string(),
                "digest-1".to_string(),
            )
            .await
            .expect("attachment data");
        assert_eq!(data, b"hello");

        // The stored document must hold the stub, not the payload.
        let found = instance
            .find_documents_by_id(vec!["with-att".to_string()], false)
            .await
            .expect("find");
        let stub = found[0]
            .attachment("att-1")
            .expect("attachment stub present");
        assert!(
            stub.get("data").is_none(),
            "payload must not be in the document"
        );
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should be able to add multiple attachments, one each write')` (:3109).
#[test]
fn adds_attachments_across_several_writes() {
    futures::executor::block_on(async {
        let instance = open(&attachments_schema()).await;
        let document = write_data("multi-att", "v");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(with_attachment(
                    document.clone(),
                    "att-1",
                    "digest-1",
                    Some(b"one"),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("first");

        // Read back the stored state, add a second attachment, write with `previous`.
        let stored = instance
            .find_documents_by_id(vec!["multi-att".to_string()], false)
            .await
            .expect("find")
            .remove(0);
        let mut next = stored.clone();
        next.set("_rev", json!(EXAMPLE_REVISION_2));
        next.set("_meta", json!({ "lwt": now() }));
        let mut write = DocumentWriteData::new(next);
        // Re-state the existing stub (no payload: "unchanged") and add a new one.
        write.insert_attachment(
            "att-1",
            AttachmentWriteData {
                stub: AttachmentData {
                    length: 3,
                    content_type: "text/plain".to_string(),
                    digest: "digest-1".to_string(),
                },
                data: None,
            },
        );
        write.insert_attachment(
            "att-2",
            AttachmentWriteData {
                stub: AttachmentData {
                    length: 3,
                    content_type: "text/plain".to_string(),
                    digest: "digest-2".to_string(),
                },
                data: Some(b"two".to_vec()),
            },
        );
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::update(stored, write)],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("second");
        assert!(response.is_ok(), "errors: {:?}", response.error);

        assert_eq!(
            instance
                .get_attachment_data("multi-att".into(), "att-1".into(), "digest-1".into())
                .await
                .expect("att-1"),
            b"one"
        );
        assert_eq!(
            instance
                .get_attachment_data("multi-att".into(), "att-2".into(), "digest-2".into())
                .await
                .expect("att-2"),
            b"two"
        );
        instance.close().await.expect("close");
    });
}

/// Upstream `it('_deleted documents must loose all attachments')` (:3193).
#[test]
fn deleted_documents_lose_their_attachments() {
    futures::executor::block_on(async {
        let instance = open(&attachments_schema()).await;
        let document = write_data("goes-away", "v");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(with_attachment(
                    document.clone(),
                    "att-1",
                    "digest-1",
                    Some(b"payload"),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut tombstone = document.clone();
        tombstone.set("_deleted", json!(true));
        tombstone.set("_meta", json!({ "lwt": now() }));
        tombstone.set("_rev", json!(EXAMPLE_REVISION_2));
        tombstone.set("_attachments", json!({}));
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    document,
                    DocumentWriteData::new(tombstone),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("delete");
        assert!(response.is_ok(), "errors: {:?}", response.error);

        // The payload must be gone, not just unreachable through the stub.
        let error = instance
            .get_attachment_data("goes-away".into(), "att-1".into(), "digest-1".into())
            .await
            .expect_err("payload must be gone");
        assert!(
            matches!(error, StorageError::MissingAttachment(_)),
            "got {error:?}"
        );
        instance.close().await.expect("close");
    });
}

/// The `510` rule: a new attachment with no payload is refused and the row is not
/// written. Upstream's attachments suite exercises this through the `510` status in
/// `categorizeBulkWriteRows`, which the oracle already pinned; this asserts the
/// storage honours it.
#[test]
fn refuses_a_new_attachment_without_payload() {
    futures::executor::block_on(async {
        let instance = open(&attachments_schema()).await;
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(with_attachment(
                    write_data("no-payload", "v"),
                    "att-1",
                    "digest-1",
                    None,
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write");
        assert_eq!(response.error.len(), 1);
        assert_eq!(response.error[0].status(), 510);

        let found = instance
            .find_documents_by_id(vec!["no-payload".to_string()], true)
            .await
            .expect("find");
        assert!(found.is_empty(), "the row must not have been written");
        instance.close().await.expect("close");
    });
}

/// A wrong digest is an error, not a mismatch to reconcile: the digest is how a caller
/// knows the bytes it received are the bytes it asked for.
#[test]
fn a_wrong_digest_is_an_error() {
    futures::executor::block_on(async {
        let instance = open(&attachments_schema()).await;
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(with_attachment(
                    write_data("digest-check", "v"),
                    "att-1",
                    "right",
                    Some(b"bytes"),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");
        let error = instance
            .get_attachment_data("digest-check".into(), "att-1".into(), "wrong".into())
            .await
            .expect_err("must reject a wrong digest");
        assert!(
            matches!(error, StorageError::MissingAttachment(_)),
            "got {error:?}"
        );
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// .findDocumentsById()
// ---------------------------------------------------------------------------

/// Upstream `it('should find the documents')` (:2184).
#[test]
fn finds_documents_by_id() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        instance
            .bulk_write(
                vec![
                    BulkWriteRow::insert(DocumentWriteData::new(write_data("a", "1"))),
                    BulkWriteRow::insert(DocumentWriteData::new(write_data("b", "2"))),
                ],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write");
        let found = instance
            .find_documents_by_id(vec!["a".into(), "b".into(), "missing".into()], false)
            .await
            .expect("find");
        assert_eq!(ids(&found, "key"), vec!["a", "b"]);
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should find the documents when the primary key is _id')` (:2221).
#[test]
fn finds_documents_when_primary_key_is_underscore_id() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("_id")).await;
        let document = DocumentData::from_value(json!({
            "_id": "the-id",
            "value": "v",
            "_deleted": false,
            "_attachments": {},
            "_rev": EXAMPLE_REVISION_1,
            "_meta": { "lwt": now() }
        }))
        .expect("valid");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(document))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write");
        let found = instance
            .find_documents_by_id(vec!["the-id".into()], false)
            .await
            .expect("find");
        assert_eq!(found.len(), 1);
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should not find the deleted document when findDocumentsById(false)')`
/// (:420) and `it('should find deleted documents')` (:2292) and
/// `it('if withDeleted=true then even the non-deleted document must be found')`
/// (:2346). All three directions of the one flag.
#[test]
fn with_deleted_controls_visibility_of_tombstones() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let live = write_data("live", "v");
        let doomed = write_data("doomed", "v");
        instance
            .bulk_write(
                vec![
                    BulkWriteRow::insert(DocumentWriteData::new(live)),
                    BulkWriteRow::insert(DocumentWriteData::new(doomed.clone())),
                ],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut tombstone = doomed.clone();
        tombstone.set("_deleted", json!(true));
        tombstone.set("_rev", json!(EXAMPLE_REVISION_2));
        tombstone.set("_meta", json!({ "lwt": now() }));
        instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    doomed,
                    DocumentWriteData::new(tombstone),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("delete");

        let without = instance
            .find_documents_by_id(vec!["live".into(), "doomed".into()], false)
            .await
            .expect("without deleted");
        assert_eq!(ids(&without, "key"), vec!["live"]);

        let with = instance
            .find_documents_by_id(vec!["live".into(), "doomed".into()], true)
            .await
            .expect("with deleted");
        assert_eq!(ids(&with, "key"), vec!["live", "doomed"]);
        assert!(with[1].is_deleted());
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should be able to insert and fetch many documents')` (:2430).
#[test]
fn inserts_and_fetches_many_documents() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        let keys: Vec<String> = (0..200).map(|i| format!("key-{i}")).collect();
        let rows: Vec<BulkWriteRow> = keys
            .iter()
            .map(|key| BulkWriteRow::insert(DocumentWriteData::new(write_data(key, "v"))))
            .collect();
        instance
            .bulk_write(rows, TEST_CONTEXT.to_string())
            .await
            .expect("write");
        let found = instance
            .find_documents_by_id(keys.clone(), false)
            .await
            .expect("find");
        assert_eq!(found.len(), keys.len());
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// .query() and .count()
// ---------------------------------------------------------------------------

/// Upstream `it('should sort in the correct order')` (:1578 and :1216).
#[test]
fn sorts_by_the_requested_field() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        for (key, value) in [("k1", "c"), ("k2", "a"), ("k3", "b")] {
            instance
                .bulk_write(
                    vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                        key, value,
                    )))],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .expect("write");
        }

        let ascending = instance
            .query(prepare(
                &test_data_schema(),
                "key",
                json!({ "selector": { "_deleted": false }, "sort": [{ "value": "asc" }] }),
            ))
            .await
            .expect("query asc");
        let values: Vec<&str> = ascending
            .iter()
            .map(|document| {
                document
                    .get("value")
                    .and_then(Value::as_str)
                    .expect("value")
            })
            .collect();
        assert_eq!(values, vec!["a", "b", "c"]);

        // A `desc` must force a re-sort, because no storage has descending indexes
        // (`query-planner.ts:48`).
        let descending = instance
            .query(prepare(
                &test_data_schema(),
                "key",
                json!({ "selector": { "_deleted": false }, "sort": [{ "value": "desc" }] }),
            ))
            .await
            .expect("query desc");
        let values: Vec<&str> = descending
            .iter()
            .map(|document| {
                document
                    .get("value")
                    .and_then(Value::as_str)
                    .expect("value")
            })
            .collect();
        assert_eq!(values, vec!["c", "b", "a"]);
        instance.close().await.expect("close");
    });
}

/// Upstream `it('must find deleted documents')` (:1626). "Notice that the RxStorage
/// itself runs whatever query you give it, filtering out deleted documents is done by
/// RxDB, not by the storage."
#[test]
fn a_query_without_a_deleted_selector_finds_tombstones() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let document = write_data("foobar", "barfoo");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(
                    document.clone(),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");
        let mut tombstone = document.clone();
        tombstone.set("_deleted", json!(true));
        tombstone.set("_rev", json!(EXAMPLE_REVISION_2));
        tombstone.set("_meta", json!({ "lwt": now() }));
        instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    document,
                    DocumentWriteData::new(tombstone),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("delete");

        let found = instance
            .query(prepare(
                &pseudo_schema("key"),
                "key",
                json!({ "selector": {}, "sort": [{ "key": "asc" }] }),
            ))
            .await
            .expect("query");
        assert_eq!(
            found.len(),
            1,
            "the storage must not filter tombstones itself"
        );
        assert!(found[0].is_deleted());
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should count the correct amount')` (:2123). The count must equal the
/// query length, including with a limit.
#[test]
fn count_equals_query_length() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        let rows: Vec<BulkWriteRow> = (0..10)
            .map(|i| {
                BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    &format!("key-{i}"),
                    "same",
                )))
            })
            .collect();
        instance
            .bulk_write(rows, TEST_CONTEXT.to_string())
            .await
            .expect("write");

        for limit in [None, Some(1u64), Some(3), Some(10), Some(50)] {
            let mut query =
                json!({ "selector": { "_deleted": false }, "sort": [{ "key": "asc" }] });
            if let Some(limit) = limit {
                query["limit"] = json!(limit);
            }
            let prepared = prepare(&test_data_schema(), "key", query);
            let documents = instance.query(prepared.clone()).await.expect("query").len();
            let counted = instance.count(prepared).await.expect("count").count;
            assert_eq!(
                counted, documents,
                "count must equal query length at limit {limit:?}"
            );
        }
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should still sort in correct order when docs do not match the
/// selector')` (:1275): a re-sort must not be skipped for the filtered-out rows.
#[test]
fn sorts_correctly_when_some_documents_do_not_match() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        for (key, value) in [("k1", "c"), ("k2", "skip-me"), ("k3", "a")] {
            instance
                .bulk_write(
                    vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                        key, value,
                    )))],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .expect("write");
        }
        let found = instance
            .query(prepare(
                &test_data_schema(),
                "key",
                json!({
                    "selector": { "value": { "$ne": "skip-me" } },
                    "sort": [{ "value": "asc" }]
                }),
            ))
            .await
            .expect("query");
        let values: Vec<&str> = found
            .iter()
            .map(|document| {
                document
                    .get("value")
                    .and_then(Value::as_str)
                    .expect("value")
            })
            .collect();
        assert_eq!(values, vec!["a", "c"]);
        instance.close().await.expect("close");
    });
}

/// A query naming an index the instance did not materialise must error, not silently
/// full-scan. Upstream throws `index does not exist <name>` and the conformance suite
/// depends on it.
#[test]
fn an_unknown_index_is_an_error() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        let mut prepared = prepare(
            &test_data_schema(),
            "key",
            json!({ "selector": {}, "sort": [{ "key": "asc" }] }),
        );
        prepared.query_plan.index = vec!["_deleted".to_string(), "nope".to_string()];
        let error = instance
            .query(prepared)
            .await
            .expect_err("must reject unknown index");
        assert!(
            matches!(error, StorageError::UnknownIndex(_)),
            "got {error:?}"
        );
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// index-string ordering
// ---------------------------------------------------------------------------

/// Index strings must be ordered by UTF-16 code unit, not by Rust's byte order.
///
/// Upstream only reaches this through `compareDocsWithIndex` with special characters
/// (`memory-helper.ts:337`); a storage that used `str` ordering would return the right
/// answer for every ASCII test and the wrong one here. `"\u{FFFF}"` sorts *after* an
/// astral character in code-unit order and *before* it in UTF-8 byte order, so the
/// two orders give opposite results.
#[test]
fn orders_index_strings_by_code_unit_not_by_bytes() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        // `key` maxLength 100 and the index is `(_deleted, key)`, so these are the
        // index strings' prefix after the `_deleted` character.
        for key in ["\u{FFFF}", "😀"] {
            instance
                .bulk_write(
                    vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                        key, "v",
                    )))],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .expect("write");
        }
        let found = instance
            .query(prepare(
                &test_data_schema(),
                "key",
                json!({ "selector": {}, "sort": [{ "key": "asc" }] }),
            ))
            .await
            .expect("query");
        // Code-unit order: the first UTF-16 unit of the emoji is 0xD83D, which is less
        // than 0xFFFF, so the emoji sorts first.
        assert_eq!(ids(&found, "key"), vec!["😀", "\u{FFFF}"]);
        // Rust's `str` order is UTF-8 byte order, where the emoji starts 0xF0 and so
        // sorts *after* 0xEF/0xBF/0xBF. The two orders must disagree here, which is why
        // this test exists: a storage using `str` ordering passes every ASCII test and
        // silently mis-sorts this case.
        assert!(
            "\u{FFFF}" < "😀",
            "Rust byte order must disagree with code-unit order for this pair"
        );
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// .changeStream()
// ---------------------------------------------------------------------------

/// Upstream `it('should emit exactly one event on write')` (:2696).
#[test]
fn emits_exactly_one_event_on_write() {
    use futures::StreamExt;
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let mut feed = instance.change_stream();

        let context = random_token();
        let document = write_data("foobar", "one");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(document))],
                context.clone(),
            )
            .await
            .expect("write");

        // "Do not await any time after the insert. By definition, a call to
        // bulkWrite() must have emitted all of its resulting events BEFORE the call to
        // bulkWrite() returns."
        let bulk = feed
            .next()
            .await
            .expect("the write must have queued a bulk");
        assert_eq!(bulk.events.len(), 1);
        assert_eq!(bulk.context, context);
        assert_eq!(bulk.events[0].operation, Operation::Insert);
        assert!(bulk.checkpoint.is_some(), "must have a checkpoint");
        assert!(bulk.events[0].document_data.lwt().is_some());
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should emit all events')` (:2791).
#[test]
fn emits_every_event_of_a_bulk() {
    use futures::StreamExt;
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let mut feed = instance.change_stream();
        let rows: Vec<BulkWriteRow> = (0..3)
            .map(|i| {
                BulkWriteRow::insert(DocumentWriteData::new(write_data(&format!("k{i}"), "v")))
            })
            .collect();
        instance
            .bulk_write(rows, TEST_CONTEXT.to_string())
            .await
            .expect("write");
        let bulk = feed.next().await.expect("bulk");
        assert_eq!(bulk.events.len(), 3);
        instance.close().await.expect("close");
    });
}

/// Upstream `it('it should not emit an empty eventBulk when the write had only
/// errors')` (:2881).
#[test]
fn does_not_emit_a_bulk_when_every_row_failed() {
    use futures::StreamExt;
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let document = write_data("foobar", "one");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(document))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("first");

        let mut feed = instance.change_stream();
        // A conflicting write produces no events, so nothing must be queued.
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "foobar", "two",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("second");
        assert_eq!(response.error.len(), 1);

        // `try_next` rather than `next`: there must be nothing waiting.
        assert!(
            futures::poll!(feed.next()).is_pending(),
            "an all-error write must not emit a bulk"
        );
        instance.close().await.expect("close");
    });
}

/// The deletion event carries the tombstone and its previous state, which is what
/// `rx-change-event.ts:19` reads.
#[test]
fn delete_events_carry_the_tombstone_and_the_previous_state() {
    use futures::StreamExt;
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let document = write_data("doomed", "v");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(
                    document.clone(),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut feed = instance.change_stream();
        let mut tombstone = document.clone();
        tombstone.set("_deleted", json!(true));
        tombstone.set("_rev", json!(EXAMPLE_REVISION_2));
        tombstone.set("_meta", json!({ "lwt": now() }));
        instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    document.clone(),
                    DocumentWriteData::new(tombstone),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("delete");

        let bulk = feed.next().await.expect("bulk");
        let event = &bulk.events[0];
        assert_eq!(event.operation, Operation::Delete);
        assert!(event.document_data.is_deleted());
        assert_eq!(event.previous_document_data.as_ref(), Some(&document));
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// multiInstance
// ---------------------------------------------------------------------------

/// Upstream `it('should update the state on the other instance')` (:3757) and
/// `it('should emit events from one instance to the other')` (:4026). Two instances on
/// the same collection share documents *and* events, which is what makes two browser
/// tabs one database.
#[test]
fn instances_on_one_collection_share_documents_and_events() {
    use futures::StreamExt;
    futures::executor::block_on(async {
        let schema = pseudo_schema("key");
        let mut params = instance_params(&schema);
        params.multi_instance = true;
        let first = open_with_params(params.clone()).await;
        let second = open_with_params(params).await;

        let mut feed_on_second = second.change_stream();
        first
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "shared", "v",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write on the first");

        // Visible to the second instance.
        let found = second
            .find_documents_by_id(vec!["shared".into()], false)
            .await
            .expect("find on the second");
        assert_eq!(found.len(), 1);

        // And its feed received the event, because the instances share one state.
        let bulk = feed_on_second
            .next()
            .await
            .expect("the second instance must see the event");
        assert_eq!(bulk.events[0].document_id, "shared");
        first.close().await.expect("close first");
        second.close().await.expect("close second");
    });
}

/// Upstream `it('should not mix up documents stored with different schema versions')`
/// (:3890): the version is part of the collection key.
#[test]
fn schema_versions_do_not_share_state() {
    futures::executor::block_on(async {
        let schema_v0 = pseudo_schema("key");
        let mut raw_v1 = pseudo_schema("key");
        raw_v1["version"] = json!(1);
        let schema_v1 = filled(&raw_v1);

        let mut params = instance_params(&schema_v0);
        params.database_name = "versioned-db".to_string();
        params.collection_name = "versioned".to_string();
        let v0 = open_with_params(params.clone()).await;

        let mut params_v1 = params.clone();
        params_v1.schema = StorageSchema::from_value(&schema_v1).expect("schema");
        let v1 = open_with_params(params_v1).await;

        v0.bulk_write(
            vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                "only-in-v0",
                "v",
            )))],
            TEST_CONTEXT.to_string(),
        )
        .await
        .expect("write");

        let in_v1 = v1
            .find_documents_by_id(vec!["only-in-v0".into()], true)
            .await
            .expect("find");
        assert!(in_v1.is_empty(), "schema versions must not share documents");
        v0.close().await.expect("close v0");
        v1.close().await.expect("close v1");
    });
}

/// Upstream `it('should not mix up documents stored in a different database name')`
/// (:3971).
#[test]
fn database_names_do_not_share_state() {
    futures::executor::block_on(async {
        let schema = pseudo_schema("key");
        let mut params = instance_params(&schema);
        params.database_name = "db-one".to_string();
        params.collection_name = "same".to_string();
        let one = open_with_params(params.clone()).await;

        let mut params_two = params.clone();
        params_two.database_name = "db-two".to_string();
        let two = open_with_params(params_two).await;

        one.bulk_write(
            vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                "only-in-one",
                "v",
            )))],
            TEST_CONTEXT.to_string(),
        )
        .await
        .expect("write");
        assert!(two
            .find_documents_by_id(vec!["only-in-one".into()], true)
            .await
            .expect("find")
            .is_empty());
        one.close().await.expect("close one");
        two.close().await.expect("close two");
    });
}

// ---------------------------------------------------------------------------
// .cleanup
// ---------------------------------------------------------------------------

/// Upstream `it('should have cleaned up the deleted document')` (:3369). Note the loop
/// condition: `cleanup` returns `false` while work remains, so callers must call again.
#[test]
fn cleanup_purges_tombstones_older_than_the_threshold() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let kept = write_data("kept", "v");
        let doomed = write_data("doomed", "v");
        instance
            .bulk_write(
                vec![
                    BulkWriteRow::insert(DocumentWriteData::new(kept)),
                    BulkWriteRow::insert(DocumentWriteData::new(doomed.clone())),
                ],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut tombstone = doomed.clone();
        tombstone.set("_deleted", json!(true));
        tombstone.set("_rev", json!(EXAMPLE_REVISION_2));
        tombstone.set("_meta", json!({ "lwt": now() }));
        instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    doomed,
                    DocumentWriteData::new(tombstone),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("delete");

        // A long threshold must not purge a just-deleted document.
        while !instance
            .cleanup(1000.0 * 60.0 * 6.0)
            .await
            .expect("cleanup")
        {}
        let still_there = instance
            .find_documents_by_id(vec!["doomed".into()], true)
            .await
            .expect("find");
        assert_eq!(still_there.len(), 1);
        assert!(still_there[0].is_deleted());

        // A zero threshold purges it.
        while !instance.cleanup(0.0).await.expect("cleanup") {}
        assert!(
            instance
                .find_documents_by_id(vec!["doomed".into()], true)
                .await
                .expect("find")
                .is_empty(),
            "the tombstone must be purged"
        );
        // Live documents survive.
        assert_eq!(
            instance
                .find_documents_by_id(vec!["kept".into()], true)
                .await
                .expect("find")
                .len(),
            1
        );
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should clean up all deleted documents when multiple are deleted')`
/// (:3470).
#[test]
fn cleanup_purges_several_tombstones() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let mut documents = Vec::new();
        for i in 0..5 {
            let document = write_data(&format!("k{i}"), "v");
            instance
                .bulk_write(
                    vec![BulkWriteRow::insert(DocumentWriteData::new(
                        document.clone(),
                    ))],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .expect("insert");
            documents.push(document);
        }
        for document in &documents {
            let id = document.id("key").expect("id").to_string();
            let mut tombstone = document.clone();
            tombstone.set("_deleted", json!(true));
            tombstone.set("_rev", json!(EXAMPLE_REVISION_2));
            tombstone.set("_meta", json!({ "lwt": now() }));
            instance
                .bulk_write(
                    vec![BulkWriteRow::update(
                        document.clone(),
                        DocumentWriteData::new(tombstone),
                    )],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .unwrap_or_else(|error| panic!("delete {id}: {error:?}"));
        }
        while !instance.cleanup(0.0).await.expect("cleanup") {}
        let remaining = instance
            .find_documents_by_id((0..5).map(|i| format!("k{i}")).collect::<Vec<_>>(), true)
            .await
            .expect("find");
        assert!(remaining.is_empty(), "every tombstone must be purged");
        instance.close().await.expect("close");
    });
}

/// Upstream `it('should at some time return true (when all docs are cleaned up)')`
/// (:3550): `true` is the "done" signal, and it must arrive.
#[test]
fn cleanup_eventually_reports_done() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        // Nothing to clean at all: the first call must already say "done".
        assert!(instance.cleanup(0.0).await.expect("cleanup"));
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// .close() and .remove()
// ---------------------------------------------------------------------------

/// Upstream `it('closing multiple times should not error')` (:3613).
#[test]
fn closing_twice_does_not_error() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        instance.close().await.expect("first close");
        instance.close().await.expect("second close");
    });
}

/// Upstream `it('should have deleted all data')` (:3633).
#[test]
fn remove_deletes_all_data() {
    futures::executor::block_on(async {
        let schema = pseudo_schema("key");
        let params = instance_params(&schema);
        let instance = open_with_params(params.clone()).await;
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "gone", "v",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write");
        instance.remove().await.expect("remove");

        // A fresh instance on the same collection must start empty.
        let reopened = open_with_params(params).await;
        assert!(
            reopened
                .find_documents_by_id(vec!["gone".into()], true)
                .await
                .expect("find")
                .is_empty(),
            "remove must delete the data"
        );
        reopened.close().await.expect("close");
    });
}

/// Upstream `it('should throw on call to .remove() after .close()')` (:3676) and
/// `it('should NOT throw on call to .close() after .remove()')` (:3693). The asymmetry
/// is real and both directions are asserted.
#[test]
fn remove_after_close_errors_but_close_after_remove_does_not() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        instance.close().await.expect("close");
        let error = instance
            .remove()
            .await
            .expect_err("remove after close must error");
        assert!(matches!(error, StorageError::Closed), "got {error:?}");

        let other = open(&pseudo_schema("key")).await;
        other.remove().await.expect("remove");
        // `close` after `remove` must not error. Note `remove` already closed it.
        other.close().await.expect("close after remove");
    });
}

/// A closed instance must refuse work rather than silently no-op.
#[test]
fn a_closed_instance_refuses_writes() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        instance.close().await.expect("close");
        let error = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "late", "v",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect_err("a closed instance must refuse a write");
        assert!(matches!(error, StorageError::Closed), "got {error:?}");
    });
}

// ---------------------------------------------------------------------------
// getChangedDocumentsSince (via the documented fallback)
// ---------------------------------------------------------------------------

/// Upstream `it('should get the latest change')` (:2462),
/// `it('should return the correct amount of documents')` (:2546) and
/// `it('should be able to correctly iterate over the checkpoints')` (:2642).
///
/// The memory storage uses the trait's fallback (upstream does not override it), so
/// this also pins the fallback.
#[test]
fn changed_documents_since_paginates_by_checkpoint() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        for i in 0..5 {
            instance
                .bulk_write(
                    vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                        &format!("k{i}"),
                        "v",
                    )))],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .expect("write");
        }

        // Walk the whole feed two at a time.
        let mut seen = Vec::new();
        let mut checkpoint = None;
        loop {
            let page = instance
                .get_changed_documents_since(2, checkpoint.clone())
                .await
                .expect("changed since");
            if page.documents.is_empty() {
                break;
            }
            seen.extend(ids(&page.documents, "key"));
            checkpoint = Some(page.checkpoint);
            if seen.len() >= 5 {
                break;
            }
        }
        assert_eq!(seen, vec!["k0", "k1", "k2", "k3", "k4"]);

        // Continuing from the last checkpoint must return nothing new, which is what
        // replication relies on to avoid re-sending.
        let empty = instance
            .get_changed_documents_since(10, checkpoint)
            .await
            .expect("changed since at end");
        assert!(empty.documents.is_empty());
        instance.close().await.expect("close");
    });
}

/// The changed-since feed returns the *current* state of a changed document, not its
/// state at the time of the change.
#[test]
fn changed_documents_since_returns_current_state() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        let document = write_data("mutating", "first");
        instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(
                    document.clone(),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("insert");

        let mut second = document.clone();
        second.set("value", json!("second"));
        second.set("_rev", json!(EXAMPLE_REVISION_2));
        second.set("_meta", json!({ "lwt": now() }));
        instance
            .bulk_write(
                vec![BulkWriteRow::update(
                    document,
                    DocumentWriteData::new(second),
                )],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("update");

        let page = instance
            .get_changed_documents_since(10, None)
            .await
            .expect("changed since");
        assert_eq!(page.documents.len(), 1, "one document, current state only");
        assert_eq!(page.documents[0].get("value"), Some(&json!("second")));
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// per-document atomicity (the contract the whole system rests on)
// ---------------------------------------------------------------------------

/// `bulk_write` is per-document atomic with no cross-document transaction: some rows
/// succeed while others fail, and the failures are reported per row.
///
/// Upstream states this in `rx-storage.interface.d.ts` and PLAN.md 4.2 repeats it as
/// load-bearing. It is asserted here directly because every retry and conflict path
/// above the storage assumes it.
#[test]
fn a_batch_writes_the_good_rows_and_reports_the_bad_ones() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        // Two documents already stored, so two of the rows below must conflict.
        for key in ["existing-a", "existing-b"] {
            instance
                .bulk_write(
                    vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                        key, "old",
                    )))],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .expect("seed");
        }

        let rows = vec![
            BulkWriteRow::insert(DocumentWriteData::new(write_data("new-1", "v"))),
            BulkWriteRow::insert(DocumentWriteData::new(write_data("existing-a", "v"))),
            BulkWriteRow::insert(DocumentWriteData::new(write_data("new-2", "v"))),
            BulkWriteRow::insert(DocumentWriteData::new(write_data("existing-b", "v"))),
            BulkWriteRow::insert(DocumentWriteData::new(write_data("new-3", "v"))),
        ];
        let response = instance
            .bulk_write(rows, TEST_CONTEXT.to_string())
            .await
            .expect("bulk");

        assert_eq!(response.error.len(), 2, "two rows must conflict");
        assert_eq!(
            response.failed_ids(),
            vec!["existing-a", "existing-b"],
            "the reported ids must be the failed ones"
        );
        // The good rows landed.
        let found = instance
            .find_documents_by_id(vec!["new-1".into(), "new-2".into(), "new-3".into()], false)
            .await
            .expect("find");
        assert_eq!(found.len(), 3, "the non-conflicting rows must be written");
        // And the conflicting ones were not overwritten.
        let unchanged = instance
            .find_documents_by_id(vec!["existing-a".into()], false)
            .await
            .expect("find");
        assert_eq!(unchanged[0].get("value"), Some(&json!("old")));
        instance.close().await.expect("close");
    });
}

/// A row with no usable primary key is rejected rather than written under an empty id.
#[test]
fn a_row_without_a_primary_key_is_rejected() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let orphan = DocumentData::from_value(json!({
            "value": "no key",
            "_deleted": false,
            "_attachments": {},
            "_rev": EXAMPLE_REVISION_1,
            "_meta": { "lwt": now() }
        }))
        .expect("valid object");
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(orphan))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("bulk");
        assert_eq!(response.error.len(), 1);
        assert!(
            instance
                .find_documents_by_id(vec![String::new()], true)
                .await
                .expect("find")
                .is_empty(),
            "a row with no primary key must not be stored under an empty id"
        );
        instance.close().await.expect("close");
    });
}

/// Unused in this file but kept for symmetry with the upstream helpers.
#[allow(dead_code)]
fn by_id(documents: &[DocumentData], path: &str) -> HashMap<String, DocumentData> {
    documents
        .iter()
        .map(|document| (document.id(path).expect("id").to_string(), document.clone()))
        .collect()
}
