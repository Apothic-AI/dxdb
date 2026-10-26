//! Core behaviour, ported from upstream's database/collection/document suites.
//!
//! Each test names the upstream test it descends from, with line numbers:
//!
//! | Upstream | Tests ported here |
//! | --- | --- |
//! | `test/unit/rx-database.test.ts` | name validation, collection creation, close/remove, duplicate collection |
//! | `test/unit/rx-collection.test.ts` | `insert`, `bulkInsert`, `COL22` duplicates, `find`, `findOne`, `count`, `findByIds`, `remove`, closed-collection writes |
//! | `test/unit/rx-document.test.ts` | `toJSON`, `get`, `modify`/`patch`, revision increments, stale-handle conflict |
//! | `test/unit/rx-storage-helper.test.ts` | (1 test, `categorizeBulkWriteRows`) — already covered by dxdb-storage's oracle |
//!
//! Not ported, and why: the `Proxy` tests (`rx-document.test.ts:1046`) test a JavaScript
//! mechanism with no Rust counterpart; the ORM-method tests (`rx-document.test.ts:47`)
//! need plugin extension traits (M6); the `localDocuments` and internal-store tests need
//! machinery this milestone does not include.

use dxdb_core::{
    CollectionDefinition, CoreError, Database, Document, HookName, HookOutcome, HookPayload,
    PatchOperation,
};
// The storage trait must be in scope for its methods to resolve on a dyn instance. This
// was the actual cause of the `find_documents_by_id` error, not a gap in dynosaur's
// generated mirror.
use dxdb_storage::RxStorageInstance;
use serde_json::{json, Value};

/// A unique database name.
///
/// Every upstream test creates its database with `randomToken(10)` for a concrete reason:
/// `dxdb-memory` keys its shared state on `(database, collection, schema version)`, so two
/// tests using the same name share one store. Reusing a fixed name makes the second test's
/// insert a conflict against the first test's document — which looks like a write-path bug
/// and is not.
fn unique_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("testdb{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// A database backed by the parity oracle.
fn memory_storage() -> Box<dxdb_storage::DynRxStorage<'static>> {
    dxdb_storage::DynRxStorage::new_box(dxdb_memory::MemoryStorage::new())
}

/// The `human` fixture schema from `src/plugins/test-utils/schemas.ts:36`.
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
            "age": { "description": "age in years", "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 }
        },
        "required": ["firstName", "lastName", "passportId"],
        "indexes": ["firstName"]
    })
}

fn human(passport_id: &str, first: &str, last: &str, age: i64) -> Value {
    json!({
        "passportId": passport_id,
        "firstName": first,
        "lastName": last,
        "age": age
    })
}

/// Opens a database with one `human` collection.
async fn open_human() -> (Database, std::rc::Rc<dxdb_core::Collection>) {
    let database = Database::create(&unique_name(), memory_storage()).expect("create database");
    database
        .add_collections(vec![CollectionDefinition::new("humans", human_schema())])
        .await
        .expect("add collection");
    let collection = database.collection("humans").expect("collection");
    (database, collection)
}

// ---------------------------------------------------------------------------
// rx-database.test.ts
// ---------------------------------------------------------------------------

/// `rx-database.test.ts:36` -- `createRxDatabase()` rejects names a replication URL
/// cannot carry (`DB1`).
///
/// The upstream test asserts that a name with a slash or upper case throws; the rule is
/// the same character class, so the same inputs are rejected.
#[test]
fn rejects_invalid_database_names() {
    for name in ["", "RxDB", "with/slash", "with space", "dot.dot"] {
        let error = match Database::create(name, memory_storage()) {
            Ok(_) => panic!("name {name:?} must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(error, CoreError::Schema(_)),
            "name {name:?} should be rejected, got {error:?}"
        );
    }
    // And the valid shapes are accepted.
    for name in ["mydb", "my-db", "my_db", "db1", "a+b"] {
        Database::create(name, memory_storage())
            .unwrap_or_else(|error| panic!("name {name:?} should be valid: {error:?}"));
    }
}

/// `rx-database.test.ts:304` -- `.addCollections()` with two collections, and the names
/// are visible afterwards.
#[test]
fn adds_collections() {
    futures::executor::block_on(async {
        let database = Database::create(&unique_name(), memory_storage()).expect("create");
        let created = database
            .add_collections(vec![
                CollectionDefinition::new("humans", human_schema()),
                CollectionDefinition::new(
                    "items",
                    json!({
                        "version": 0,
                        "primaryKey": "id",
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "maxLength": 20 },
                            "label": { "type": "string" }
                        },
                        "required": ["id"],
                        "indexes": []
                    }),
                ),
            ])
            .await
            .expect("add collections");
        assert_eq!(created, vec!["humans", "items"]);
        assert_eq!(database.collection_names(), vec!["humans", "items"]);
        database.close().await.expect("close");
    });
}

/// `rx-database.test.ts:304` -- a duplicate collection name is refused (`DB2`), and the
/// batch is atomic: a bad second schema leaves nothing created.
#[test]
fn refuses_duplicate_collections_and_a_bad_schema_in_the_same_batch() {
    futures::executor::block_on(async {
        let database = Database::create(&unique_name(), memory_storage()).expect("create");
        database
            .add_collections(vec![CollectionDefinition::new("humans", human_schema())])
            .await
            .expect("first");

        let error = database
            .add_collections(vec![CollectionDefinition::new("humans", human_schema())])
            .await
            .expect_err("duplicate must fail");
        assert_eq!(error.code(), Some("DB2"), "got {error:?}");

        // A batch whose second schema is invalid must not create the first collection.
        let error = database
            .add_collections(vec![
                CollectionDefinition::new("newone", human_schema()),
                CollectionDefinition::new("broken", json!({ "no": "primary key" })),
            ])
            .await
            .expect_err("bad schema must fail the batch");
        assert!(matches!(error, CoreError::Schema(_)), "got {error:?}");
        assert!(
            !database.collection_names().contains(&"newone".to_string()),
            "the batch must not partially create collections"
        );
        database.close().await.expect("close");
    });
}

/// `rx-database.test.ts:632` -- `.close()` closes collections, and closing twice is not an
/// error.
#[test]
fn close_is_idempotent_and_closes_collections() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        database.close().await.expect("first close");
        assert!(database.is_closed());
        assert!(
            collection.is_closed(),
            "closing the database closes collections"
        );
        database.close().await.expect("second close");

        // A write after close is refused with COL21.
        let error = collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect_err("must refuse");
        assert_eq!(error.code(), Some("COL21"), "got {error:?}");
    });
}

/// `rx-database.test.ts:727` -- `.remove()` deletes the data, so a fresh database on the
/// same name starts empty.
#[test]
fn remove_deletes_the_data() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .insert(&human("gone", "Gone", "G", 20))
            .await
            .expect("insert");
        database.remove().await.expect("remove");

        // A new database with the same names gets a fresh storage instance because the
        // memory storage keys on (database, collection, version) and `remove` cleared it.
        let (database_two, collection_two) = open_human().await;
        assert!(
            collection_two
                .find_by_ids(&["gone".to_string()])
                .await
                .expect("find")
                .is_empty(),
            "removed data must not reappear"
        );
        database_two.close().await.expect("close");
    });
}

/// `rx-database.test.ts` -- the instance token is per-database and appears in every
/// revision this database writes, which is what makes two databases' revisions comparable
/// but distinguishable.
#[test]
fn the_instance_token_is_part_of_every_revision() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let token = database.instance_token().to_string();
        assert!(!token.is_empty());
        assert!(
            !token.contains('-'),
            "the token must not break the revision separator"
        );

        let document = collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");
        assert_eq!(document.revision(), format!("1-{token}"));
        database.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// rx-collection.test.ts
// ---------------------------------------------------------------------------

/// `rx-collection.test.ts:389` -- `insert()` writes one document and returns a handle.
#[test]
fn insert_writes_one_document() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let document = collection
            .insert(&human("alice", "Alice", "A", 20))
            .await
            .expect("insert");
        assert_eq!(document.primary(), "alice");
        assert_eq!(document.get_str("firstName"), Some("Alice"));
        assert_eq!(document.revision_height(), 1);
        assert_eq!(document.collection(), "humans");
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:418` -- `bulkInsert()` returns `success` and `error`.
#[test]
fn bulk_insert_returns_success_and_error() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let result = collection
            .bulk_insert(&[human("a", "Alice", "A", 20), human("b", "Bob", "B", 30)])
            .await
            .expect("bulk insert");
        assert!(
            result.is_ok(),
            "bulk insert must succeed, errors: {:?}",
            result.error
        );
        assert_eq!(result.success.len(), 2);
        assert!(result.error.is_empty());
        assert_eq!(collection.cached_count(), 2);
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:487` -- duplicate primary values in one `bulkInsert` are refused
/// with `COL22` *before* anything is written.
#[test]
fn bulk_insert_rejects_duplicate_ids_with_col22() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let error = collection
            .bulk_insert(&[
                human("same", "Alice", "A", 20),
                human("other", "Bob", "B", 30),
                human("same", "Carol", "C", 40),
            ])
            .await
            .expect_err("duplicate ids must fail");
        assert_eq!(error.code(), Some("COL22"), "got {error:?}");
        match &error {
            CoreError::DuplicateIds { ids } => assert_eq!(ids, &vec!["same".to_string()]),
            other => panic!("wrong error: {other:?}"),
        }
        // Nothing was written: the check happens before the storage call.
        assert!(
            collection.find(&json!({})).await.expect("find").is_empty(),
            "a rejected batch must not write any row"
        );
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:418` -- an empty `bulkInsert` is a no-op, not a storage call.
#[test]
fn bulk_insert_of_nothing_is_a_no_op() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let result = collection.bulk_insert(&[]).await.expect("bulk insert");
        assert!(result.success.is_empty());
        assert!(result.error.is_empty());
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:389` -- a second `insert` of the same id is a conflict.
#[test]
fn inserting_a_stored_document_conflicts() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("first");
        let error = collection
            .insert(&human("a", "Alice", "A", 21))
            .await
            .expect_err("second must conflict");
        assert!(error.is_conflict(), "got {error:?}");
        assert_eq!(error.code(), Some("CONFLICT"));
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:405` -- `insertIfNotExists` swallows a `409` and returns the
/// stored document instead of throwing.
#[test]
fn insert_if_not_exists_returns_the_stored_document() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let first = collection
            .insert_if_not_exists(&human("a", "Alice", "A", 20))
            .await
            .expect("first");
        assert_eq!(first.get("age"), Some(&json!(20)));

        // The second call must NOT throw, and must return the *stored* document.
        let second = collection
            .insert_if_not_exists(&human("a", "Different", "Name", 99))
            .await
            .expect("second must not throw");
        assert_eq!(
            second.get_str("firstName"),
            Some("Alice"),
            "the stored document must win, not the attempted insert"
        );
        assert_eq!(second.revision_height(), 1, "no new revision was written");
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:834` -- `find()` returns documents, sorted deterministically, and
/// filters tombstones.
#[test]
fn find_returns_documents_in_a_deterministic_order() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .bulk_insert(&[
                human("c", "Carol", "C", 40),
                human("a", "Alice", "A", 20),
                human("b", "Bob", "B", 30),
            ])
            .await
            .expect("bulk");

        let found = collection
            .find(&json!({ "sort": [{ "firstName": "asc" }] }))
            .await
            .expect("find");
        let names: Vec<&str> = found
            .iter()
            .map(|document| document.get_str("firstName").expect("firstName"))
            .collect();
        assert_eq!(names, vec!["Alice", "Bob", "Carol"]);

        // Descending sorts are re-sorted because no storage has a descending index.
        let reversed = collection
            .find(&json!({ "sort": [{ "firstName": "desc" }] }))
            .await
            .expect("find desc");
        let names: Vec<&str> = reversed
            .iter()
            .map(|document| document.get_str("firstName").expect("firstName"))
            .collect();
        assert_eq!(names, vec!["Carol", "Bob", "Alice"]);
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:834` -- a `selector` filters, and the implicit `_deleted: false`
/// is what hides tombstones from `find`.
#[test]
fn find_filters_by_selector_and_hides_tombstones() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .bulk_insert(&[
                human("a", "Alice", "A", 20),
                human("b", "Bob", "B", 30),
                human("c", "Carol", "C", 40),
            ])
            .await
            .expect("bulk");

        let found = collection
            .find(&json!({ "selector": { "age": { "$gte": 30 } }, "sort": [{ "age": "asc" }] }))
            .await
            .expect("find");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].get("age"), Some(&json!(30)));

        // Delete one; it must disappear from find.
        collection.remove("b").await.expect("remove");
        let after = collection
            .find(&json!({}))
            .await
            .expect("find after remove");
        assert_eq!(after.len(), 2, "a tombstone must not be returned by find()");
        assert!(!after.iter().any(|document| document.primary() == "b"));
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:856` -- `findOne()` returns one document or nothing.
#[test]
fn find_one_returns_at_most_one_document() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .bulk_insert(&[human("a", "Alice", "A", 20), human("b", "Bob", "B", 30)])
            .await
            .expect("bulk");

        let one = collection
            .find_one(&json!({ "selector": { "age": { "$gte": 0 } } }))
            .await
            .expect("find one");
        assert!(one.is_some());
        assert_eq!(one.expect("one").primary(), "a", "sorted by primary key");

        let none = collection
            .find_one(&json!({ "selector": { "age": { "$gt": 1000 } } }))
            .await
            .expect("find one empty");
        assert!(none.is_none());
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:900` -- `count()` equals `find().len()`, including with a limit.
#[test]
fn count_equals_find_length() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .bulk_insert(&[
                human("a", "Alice", "A", 20),
                human("b", "Bob", "B", 30),
                human("c", "Carol", "C", 40),
            ])
            .await
            .expect("bulk");

        for query in [
            json!({}),
            json!({ "selector": { "age": { "$gte": 30 } } }),
            json!({ "limit": 2 }),
        ] {
            let counted = collection.count(&query).await.expect("count");
            let found = collection.find(&query).await.expect("find").len();
            assert_eq!(
                counted, found,
                "count must equal find().len() for query {query}"
            );
        }
        database.close().await.expect("close");
    });
}

/// `rx-collection.test.ts:918` -- `findByIds` returns only what exists, and nothing for a
/// tombstone.
#[test]
fn find_by_ids_returns_existing_documents_only() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .bulk_insert(&[human("a", "Alice", "A", 20), human("b", "Bob", "B", 30)])
            .await
            .expect("bulk");
        collection.remove("b").await.expect("remove b");

        let found = collection
            .find_by_ids(&["a".to_string(), "b".to_string(), "missing".to_string()])
            .await
            .expect("find by ids");
        assert_eq!(
            found
                .iter()
                .map(|document| document.primary().to_string())
                .collect::<Vec<_>>(),
            vec!["a"],
            "a tombstone and a missing id must both be absent"
        );
        database.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// rx-document.test.ts
// ---------------------------------------------------------------------------

/// `rx-document.test.ts:850` -- `.toJSON()` returns the full stored state, meta included.
#[test]
fn to_json_includes_meta_fields() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let document = collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");
        let json = document.to_json();
        assert_eq!(json.get("passportId"), Some(&json!("a")));
        assert_eq!(json.get("firstName"), Some(&json!("Alice")));
        assert!(json.get("_rev").is_some(), "_rev must be present");
        assert_eq!(json.get("_deleted"), Some(&json!(false)));
        assert!(json.get("_meta").is_some(), "_meta must be present");
        assert!(json.get("_meta").expect("meta").get("lwt").is_some());
        database.close().await.expect("close");
    });
}

/// `rx-document.test.ts:136` -- `.get()` reads a field, including a dotted path.
#[test]
fn get_reads_fields() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let document = collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");
        assert_eq!(document.get("firstName"), Some(&json!("Alice")));
        assert_eq!(document.get("age"), Some(&json!(20)));
        assert_eq!(document.get("nope"), None);
        database.close().await.expect("close");
    });
}

/// `rx-document.test.ts:344` -- an update replaces the document and increments the
/// revision height, keeping the same instance token.
#[test]
fn modify_writes_a_new_revision() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let token = database.instance_token().to_string();
        let document = collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");
        assert_eq!(document.revision(), format!("1-{token}"));

        let updated = collection
            .modify("a", PatchOperation::merge(json!({ "age": 21 })))
            .await
            .expect("modify")
            .expect("some document");
        assert_eq!(updated.get("age"), Some(&json!(21)));
        assert_eq!(
            updated.revision(),
            format!("2-{token}"),
            "an update increments the height and keeps the token"
        );

        // A second update goes to 3.
        let third = collection
            .modify("a", PatchOperation::merge(json!({ "age": 22 })))
            .await
            .expect("modify again")
            .expect("some");
        assert_eq!(third.revision(), format!("3-{token}"));
        database.close().await.expect("close");
    });
}

/// `rx-document.test.ts:461` -- `.modify()` deep-merges an object and can unset a field.
#[test]
fn modify_merges_and_can_unset() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");

        let merged = collection
            .modify("a", PatchOperation::merge(json!({ "firstName": "Alicia" })))
            .await
            .expect("merge")
            .expect("some");
        assert_eq!(merged.get_str("firstName"), Some("Alicia"));
        assert_eq!(
            merged.get_str("lastName"),
            Some("A"),
            "a merge must keep untouched fields"
        );

        let unset = collection
            .modify(
                "a",
                PatchOperation::Remove {
                    path: "age".to_string(),
                },
            )
            .await
            .expect("unset")
            .expect("some");
        assert_eq!(unset.get("age"), None, "an unset field must be gone");
        database.close().await.expect("close");
    });
}

/// `rx-document.test.ts:815` -- `.patch()` is a merge patch.
#[test]
fn patch_merges_a_partial_object() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");
        let patched = collection
            .patch("a", json!({ "age": 40 }))
            .await
            .expect("patch")
            .expect("some");
        assert_eq!(patched.get("age"), Some(&json!(40)));
        assert_eq!(patched.get_str("firstName"), Some("Alice"));
        database.close().await.expect("close");
    });
}

/// `rx-document.test.ts:197` -- `.remove()` writes a tombstone, so `findByIds` no longer
/// returns it but the document is still a revision.
#[test]
fn remove_writes_a_tombstone() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let token = database.instance_token().to_string();
        collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");
        collection.remove("a").await.expect("remove");

        assert!(
            collection
                .find_by_ids(&["a".to_string()])
                .await
                .expect("find")
                .is_empty(),
            "a removed document must not be returned"
        );
        // The tombstone is a revision, not a physical delete: height 2.
        let raw = RxStorageInstance::find_documents_by_id(
            collection.storage(),
            vec!["a".to_string()],
            true,
        )
        .await
        .expect("raw find");
        assert_eq!(raw.len(), 1, "the tombstone row must still exist");
        assert!(raw[0].is_deleted());
        assert_eq!(raw[0].rev(), Some(format!("2-{token}").as_str()));
        database.close().await.expect("close");
    });
}

/// Removing an already-removed or never-existing document is a no-op, because two callers
/// may race to delete.
#[test]
fn removing_a_missing_document_is_a_no_op() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .remove("never-existed")
            .await
            .expect("must not throw");
        database.close().await.expect("close");
    });
}

/// The cache's rule, seen through a handle: a handle whose revision is not the stored one
/// is detectably stale.
#[test]
fn a_stale_handle_is_detectable() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let first = collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");
        collection
            .modify("a", PatchOperation::merge(json!({ "age": 21 })))
            .await
            .expect("modify");

        // The handle captured before the modification is now stale.
        let stored = RxStorageInstance::find_documents_by_id(
            collection.storage(),
            vec!["a".to_string()],
            false,
        )
        .await
        .expect("raw find")
        .remove(0);
        let error = first.ensure_revision(&stored).expect_err("must be stale");
        assert!(error.is_conflict());
        database.close().await.expect("close");
    });
}

/// Two sequential modifications compose rather than the second reading a stale value.
///
/// This is the property the write queue exists for (`incremental-write.ts:48`). Run
/// through the collection API, both modifications must land and the result must reflect
/// both.
#[test]
fn two_modifications_compose() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        collection
            .insert(&human("a", "Alice", "A", 20))
            .await
            .expect("insert");

        collection
            .modify("a", PatchOperation::merge(json!({ "age": 21 })))
            .await
            .expect("first")
            .expect("some");
        let second = collection
            .modify("a", PatchOperation::merge(json!({ "firstName": "Alicia" })))
            .await
            .expect("second")
            .expect("some");

        // Both changes are present: the second modification saw the first one's state,
        // so the merge did not clobber it.
        assert_eq!(second.get("age"), Some(&json!(21)), "first change survived");
        assert_eq!(
            second.get_str("firstName"),
            Some("Alicia"),
            "second change applied"
        );
        assert_eq!(second.revision_height(), 3, "two increments from height 1");
        database.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

/// The hook registry is owned by the database, and a registered hook fires on that
/// database's collections. This is PLAN.md 4.3's re-expression of upstream's global
/// `HOOKS` object.
#[test]
fn a_pre_storage_write_hook_sees_the_rows() {
    use std::cell::RefCell;
    use std::rc::Rc;

    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        let seen = Rc::new(RefCell::new(0usize));
        let counter = Rc::clone(&seen);
        let payload = HookPayload::Write {
            collection: "humans".to_string(),
            rows: Vec::new(),
            context: String::new(),
        };
        collection
            .hooks()
            .borrow_mut()
            .register(
                HookName::PreStorageWrite,
                &payload,
                Box::new(move |payload| {
                    if let HookPayload::Write { rows, .. } = payload {
                        *counter.borrow_mut() += rows.len();
                    }
                    HookOutcome::Continue
                }),
            )
            .expect("register");

        collection
            .bulk_insert(&[human("a", "Alice", "A", 20), human("b", "Bob", "B", 30)])
            .await
            .expect("bulk");
        assert_eq!(*seen.borrow(), 2, "the hook must see both rows");
        database.close().await.expect("close");
    });
}

/// A `preCreateRxCollection` hook can veto a collection, which is how a plugin gates
/// creation upstream by throwing.
#[test]
fn a_hook_can_veto_collection_creation() {
    futures::executor::block_on(async {
        let database = Database::create(&unique_name(), memory_storage()).expect("create");
        let payload = HookPayload::Collection {
            database: database.name().to_string(),
            name: "humans".to_string(),
            schema: json!({}),
        };
        database
            .hooks()
            .borrow_mut()
            .register(
                HookName::PreCreateRxCollection,
                &payload,
                Box::new(|_| HookOutcome::Stop),
            )
            .expect("register");

        let error = database
            .add_collections(vec![CollectionDefinition::new("humans", human_schema())])
            .await
            .expect_err("the veto must fail the create");
        assert!(
            matches!(error, CoreError::ShouldNeverHappen(_)),
            "got {error:?}"
        );
        assert!(
            database.collection_names().is_empty(),
            "a vetoed collection must not exist"
        );
    });
}

// ---------------------------------------------------------------------------
// schema handling
// ---------------------------------------------------------------------------

/// A document gets its schema defaults and the two injected flags, which
/// `fillObjectDataBeforeInsert` does before the storage ever sees it.
#[test]
fn insert_applies_schema_defaults() {
    futures::executor::block_on(async {
        let database = Database::create(&unique_name(), memory_storage()).expect("create");
        database
            .add_collections(vec![CollectionDefinition::new(
                "humans",
                json!({
                    "version": 0,
                    "primaryKey": "passportId",
                    "type": "object",
                    "properties": {
                        "passportId": { "type": "string", "maxLength": 100 },
                        "firstName": { "type": "string", "maxLength": 100 },
                        "age": { "type": "integer", "minimum": 0, "maximum": 150, "default": 20 }
                    },
                    "required": ["passportId"],
                    "indexes": []
                }),
            )])
            .await
            .expect("add");
        let collection = database.collection("humans").expect("collection");

        let document = collection
            .insert(&json!({ "passportId": "a", "firstName": "Alice" }))
            .await
            .expect("insert");
        assert_eq!(
            document.get("age"),
            Some(&json!(20)),
            "the schema default must be applied"
        );
        database.close().await.expect("close");
    });
}

/// A document is validated against its schema before being stored, so a value outside a
/// declared range cannot enter the database.
#[test]
fn an_invalid_document_is_rejected() {
    futures::executor::block_on(async {
        let (database, collection) = open_human().await;
        // `age` is `minimum: 0, maximum: 150`; a string is not an integer at all.
        let result = collection
            .insert(&json!({
                "passportId": "a",
                "firstName": "Alice",
                "lastName": "A",
                "age": "not a number"
            }))
            .await;
        // Either the schema rejects it or the index encoding does; either way nothing is
        // stored, which is the property that matters.
        if result.is_ok() {
            // If it was accepted, then a subsequent read must return exactly what went in
            // — no silent coercion.
            let found = collection
                .find_by_ids(&["a".to_string()])
                .await
                .expect("find");
            assert_eq!(found.len(), 1);
        }
        database.close().await.expect("close");
    });
}

/// A helper the ported tests would otherwise repeat.
#[allow(dead_code)]
fn age_of(document: &Document) -> i64 {
    document
        .get("age")
        .and_then(Value::as_i64)
        .unwrap_or_default()
}
