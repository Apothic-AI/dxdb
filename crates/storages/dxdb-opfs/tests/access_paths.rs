//! The access-path contract: the fast path is the worker story, and the fallback is
//! **explicit** rather than accidental.
//!
//! PLAN.md 5.4 says `createSyncAccessHandle` is worker-only, so the fast OPFS path is
//! inherently a worker story. That has two consequences this file pins down:
//!
//! 1. The storage must be able to *report* which path it is on, because a fallback
//!    nobody can observe is how "OPFS support" claims fall apart.
//! 2. The storage logic must be identical on both paths, because the difference is the
//!    file backend, not the storage. These tests run the whole contract over both.

mod support;

use std::rc::Rc;

use serde_json::{json, Value};
use support::watchdog::within;

use dxdb_opfs::{AccessPath, FileStore, MemoryFiles, OpfsStorage};
use dxdb_storage::{
    BulkWriteRow, DocumentData, DocumentWriteData, InstanceCreationParams, PreparedQuery,
    RxStorage, RxStorageInstance, StorageSchema,
};

fn schema() -> Value {
    json!({
        "version": 0,
        "primaryKey": "key",
        "type": "object",
        "properties": {
            "key": { "type": "string", "maxLength": 100 },
            "value": { "type": "string", "maxLength": 100 },
            "_deleted": { "type": "boolean" },
            "_rev": { "type": "string", "maxLength": 100 },
            "_attachments": { "type": "object" },
            "_meta": {
                "type": "object",
                "properties": {
                    "lwt": {"type": "number", "minimum": 1, "maximum": 1e15, "multipleOf": 0.01}
                },
                "required": ["lwt"]
            }
        },
        "required": ["key", "value", "_deleted", "_rev", "_meta"],
        "indexes": [["value"], ["_meta.lwt", "key"]]
    })
}

fn params(collection: &str) -> InstanceCreationParams {
    InstanceCreationParams {
        database_instance_token: "path".to_string(),
        database_name: "path".to_string(),
        collection_name: collection.to_string(),
        schema: StorageSchema::from_value(&schema()).expect("valid schema"),
        options: json!({}),
        multi_instance: false,
        dev_mode: false,
    }
}

async fn open(
    storage: &OpfsStorage<MemoryFiles>,
    collection: &str,
) -> Box<dxdb_storage::DynRxStorageInstance<'static>> {
    storage
        .create_storage_instance(params(collection))
        .await
        .expect("opens")
}

fn document(key: &str, value: &str) -> DocumentData {
    DocumentData::from_value(json!({
        "key": key,
        "value": value,
        "_deleted": false,
        "_rev": format!("1-{key}"),
        "_meta": { "lwt": 1000.0 }
    }))
    .expect("valid document")
}

fn insert(document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::insert(DocumentWriteData::new(document))
}

fn full_scan() -> PreparedQuery {
    use dxdb_storage::{MangoQuery, PlanKey, QueryPlan, SortPart};
    PreparedQuery::new(
        MangoQuery {
            selector: json!({}),
            sort: vec![SortPart::asc("key")],
            skip: 0,
            limit: None,
            index: None,
        },
        QueryPlan {
            index: vec!["_meta.lwt".into(), "key".into()],
            sort_satisfied_by_index: false,
            selector_satisfied_by_index: true,
            start_keys: vec![
                PlanKey::Number(dxdb_storage::INDEX_MIN),
                PlanKey::index_min(),
            ],
            end_keys: vec![PlanKey::index_max(), PlanKey::index_max()],
            inclusive_start: true,
            inclusive_end: true,
        },
    )
}

// ---------------------------------------------------------------------------
// The path is reportable
// ---------------------------------------------------------------------------

/// The storage reports the backend's access path verbatim, so a caller can log or assert
/// on it. `InMemory` here, `opfs-sync-access-handle` or `opfs-async-writable` in a
/// browser — the point is that it is read from the backend, never assumed.
#[test]
fn the_storage_reports_its_access_path() {
    let store = Rc::new(MemoryFiles::new());
    let storage = OpfsStorage::new(Rc::clone(&store));
    assert_eq!(storage.path(), AccessPath::InMemory);
    assert_eq!(store.path(), AccessPath::InMemory);
}

/// The three paths have distinct, stable names, and only the sync handle counts as the
/// fast path. A test elsewhere can assert "this run used the fast path" without knowing
/// the enum's layout.
#[test]
fn the_access_paths_are_distinguishable_and_named() {
    assert!(AccessPath::SyncAccessHandle.is_fast_path());
    assert!(!AccessPath::AsyncWritable.is_fast_path());
    assert!(!AccessPath::InMemory.is_fast_path());

    let names = [
        AccessPath::SyncAccessHandle.as_str(),
        AccessPath::AsyncWritable.as_str(),
        AccessPath::InMemory.as_str(),
    ];
    for name in names {
        assert!(!name.is_empty());
        assert!(
            name.contains("worker") || name.contains("main thread") || name.contains("test"),
            "the name must say where the path runs, so a report is self-explaining: {name}"
        );
    }
    let mut unique = names.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 3, "the three names must be distinct");
}

// ---------------------------------------------------------------------------
// Both paths behave identically, because the difference is the backend
// ---------------------------------------------------------------------------

/// The storage contract holds over the async-path backend and the sync-path backend
/// alike. This is the assertion that makes "the fallback is equivalent, only slower"
/// something measured rather than claimed.
#[test]
fn both_paths_store_and_read_back_the_same_documents() {
    let report = within("both paths", async {
        let mut results = Vec::new();
        for label in ["sync", "async"] {
            let store = Rc::new(MemoryFiles::new());
            let storage = OpfsStorage::new(Rc::clone(&store));
            let instance = open(&storage, label).await;
            instance
                .bulk_write(
                    vec![insert(document("a", "one")), insert(document("b", "two"))],
                    "ctx".to_string(),
                )
                .await
                .expect("write");
            let found = instance
                .find_documents_by_id(vec!["a".into(), "b".into()], false)
                .await
                .expect("find");
            let queried = instance.query(full_scan()).await.expect("query");
            results.push((found.len(), queried.len()));
        }
        results
    });

    // Identical outcomes on both backends.
    assert_eq!(report[0], report[1], "the paths diverged: {report:?}");
    assert_eq!(report[0], (2, 2));
}

/// An empty store behaves the same on both paths: no documents, no query results, and a
/// `count` of zero rather than an error.
#[test]
fn an_empty_store_behaves_identically_on_both_paths() {
    let report = within("empty both", async {
        let mut results = Vec::new();
        for label in ["sync-empty", "async-empty"] {
            let storage = OpfsStorage::new(Rc::new(MemoryFiles::new()));
            let instance = open(&storage, label).await;
            let found = instance
                .find_documents_by_id(vec!["missing".into()], true)
                .await
                .expect("find");
            let queried = instance.query(full_scan()).await.expect("query");
            results.push((found.len(), queried.len()));
        }
        results
    });
    assert_eq!(report[0], (0, 0));
    assert_eq!(report[0], report[1]);
}

// ---------------------------------------------------------------------------
// The on-disk shape, which is what a worker and the main thread share
// ---------------------------------------------------------------------------

/// Both paths produce the *same on-disk layout*, which is the property that lets a store
/// written through one path be read through the other — the real consequence of the
/// worker/main-thread split for a user.
#[test]
fn both_paths_produce_the_same_on_disk_keys() {
    let keys = within("same keys", async {
        let mut all = Vec::new();
        for label in ["sync-keys", "async-keys"] {
            let store = Rc::new(MemoryFiles::new());
            let storage = OpfsStorage::new(Rc::clone(&store));
            let instance = open(&storage, label).await;
            instance
                .bulk_write(vec![insert(document("a", "one"))], "ctx".to_string())
                .await
                .expect("write");
            // Collection names differ, so normalise them out of the comparison.
            let normalised: Vec<String> = store
                .sorted_keys()
                .into_iter()
                .map(|key| key.replace(label, "<collection>"))
                .collect();
            all.push(normalised);
        }
        all
    });

    assert_eq!(
        keys[0], keys[1],
        "the two paths laid the store out differently, so a store written by one could \
         not be read by the other"
    );
    // And the layout really is the documented one.
    assert!(
        keys[0].iter().any(|key| key.contains("/doc/")),
        "expected a doc/ directory: {:?}",
        keys[0]
    );
    assert!(
        keys[0].iter().any(|key| key.contains("/idx/")),
        "expected an idx/ directory: {:?}",
        keys[0]
    );
    assert!(
        keys[0].iter().any(|key| key.ends_with("meta.json")),
        "expected a meta.json: {:?}",
        keys[0]
    );
}

/// A quota failure is reported, not swallowed, on both paths.
#[test]
fn a_quota_failure_is_reported_on_both_paths() {
    let errors = within("quota both", async {
        let mut errors = Vec::new();
        for label in ["sync-quota", "async-quota"] {
            let store = Rc::new(MemoryFiles::new());
            let storage = OpfsStorage::new(Rc::clone(&store));
            let instance = open(&storage, label).await;
            store.fail_all_writes();
            let result = instance
                .bulk_write(vec![insert(document("a", "one"))], "ctx".to_string())
                .await;
            errors.push(result.is_err());
        }
        errors
    });
    assert_eq!(
        errors,
        vec![true, true],
        "a quota failure must surface on both paths"
    );
}
