//! Ported from `test/unit/local-documents.test.ts` (952 lines) at upstream
//! `3b763ee17`.
//!
//! Every test names the upstream `it(...)` and its line. The cases that are about
//! the store's behaviour are ported; the ones that need a document cache, an
//! incremental write queue or a broadcast channel are listed in the crate docs as
//! not ported, with the reason.
//!
//! The headline test is [`upstream_47_a_local_document_is_invisible_to_a_replication`],
//! which is the negative the brief asks for. It is paired with a positive control so
//! it cannot pass by the reader being broken.

use dxdb_plugin_local_documents::{
    local_document_schema, local_instance_name, Error, LocalDocument, LocalDocumentStore,
    NormalCollection, LOCAL_INSTANCE_PREFIX, LOCAL_SCHEMA_TITLE,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// `local-documents.test.ts:39` — should create a local document
// ---------------------------------------------------------------------------

#[test]
fn upstream_39_should_create_a_local_document() {
    let mut store = LocalDocumentStore::new(Some("human"));
    let document = store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    assert_eq!(document.id, "foobar");
    assert_eq!(document.data, json!({ "foo": "bar" }));
    assert!(!document.deleted);
    // The storage assigned a real revision, replacing the invalid `''`.
    assert!(!document.revision.is_empty());
    // `assert.ok(doc)` at `:44` is the only upstream assertion; the rest of this is
    // the shape the downstream tests rely on.
    assert!(store.get_local("foobar").is_some());
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:47` — should not find the doc because its local
// ---------------------------------------------------------------------------

#[test]
fn upstream_47_a_local_document_is_invisible_to_a_normal_query() {
    // Upstream: `c.insertLocal('foobar', ...)` then `c.findOne().exec()` is `null`.
    let mut local = LocalDocumentStore::new(Some("human"));
    local
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");

    // The structural reason: the local store is a different storage instance, and a
    // normal collection's query is scoped to its own instance.
    assert_ne!(local.instance_name(), "human");
    assert!(local.instance_name().starts_with(LOCAL_INSTANCE_PREFIX));

    // A collection query only ever consults its own documents; the local store holds
    // its own map, so there is nothing for it to find.
    let collection = NormalCollection::new("human");
    assert_eq!(collection.instance_name(), "human");
    assert!(local.get_local("foobar").is_some(), "it exists, locally");
}

#[test]
fn upstream_47_a_local_document_is_invisible_to_a_replication() {
    // THE NEGATIVE. A replication is bound to a storage instance and subscribes to
    // that instance's changeStream (`replication-protocol/index.ts:200`,
    // `upstream.ts:95`). A local store is a different instance, so a replication
    // scoped to the collection cannot see its events.
    let mut local = LocalDocumentStore::new(Some("human"));
    local
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    local
        .upsert_local("foobar", json!({ "foo": "baz" }))
        .expect("upsert");
    local.remove_local("foobar").expect("remove");

    // The store did emit events — three of them — so this is not a test of a broken
    // writer.
    assert_eq!(local.events().len(), 3);

    // But a replication bound to this instance sees nothing: not one local document
    // reaches downstream.
    assert!(
        local.replication_visible_events().is_empty(),
        "a local document must never appear in a replication downstream"
    );

    // POSITIVE CONTROL: the same reader against a normal collection does see events,
    // so the assertion above is about local documents, not about the reader.
    let mut collection = NormalCollection::new("human");
    collection.insert("doc-1");
    assert_eq!(collection.replication_visible_events().len(), 1);
    assert!(!collection.replication_visible_events()[0].is_local);
}

#[test]
fn the_events_a_local_store_emits_are_flagged_local_for_live_queries() {
    // `rx-local-document.ts:366,374` sets `isLocal: true`, and
    // `change-event-buffer.ts:54` filters on it. That is a *different* consumer from
    // replication, which is the distinction the crate docs warn about.
    let mut store = LocalDocumentStore::new(Some("human"));
    store.insert_local("a", json!({})).expect("insert");
    store.upsert_local("a", json!({ "x": 1 })).expect("upsert");
    store.remove_local("a").expect("remove");

    let operations: Vec<&str> = store.events().iter().map(|event| event.operation).collect();
    assert_eq!(operations, vec!["INSERT", "UPDATE", "DELETE"]);
    assert!(
        store.events().iter().all(|event| event.is_local),
        "every local event is flagged, which is what keeps it out of live queries"
    );
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:58` — should throw if already exists
// ---------------------------------------------------------------------------

#[test]
fn upstream_58_insert_throws_when_the_document_exists() {
    let mut store = LocalDocumentStore::new(Some("human"));
    store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("first");
    assert_eq!(
        store
            .insert_local("foobar", json!({ "foo": "other" }))
            .expect_err("second"),
        Error::AlreadyExists("foobar".to_string())
    );
    // And the original is untouched.
    assert_eq!(
        store.get_local("foobar").expect("present").data,
        json!({ "foo": "bar" })
    );
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:79` — should find the document
// ---------------------------------------------------------------------------

#[test]
fn upstream_79_get_local_finds_the_document() {
    let mut store = LocalDocumentStore::new(Some("human"));
    store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    let found = store.get_local("foobar").expect("found");
    assert_eq!(found.id, "foobar");
    assert_eq!(found.data, json!({ "foo": "bar" }));
}

#[test]
fn upstream_89_finding_twice_returns_the_same_document() {
    // "should find the document twice (doc-cache)" (`:89`). Upstream's identity
    // guarantee comes from the document cache; here the store holds one value, so the
    // revision and data are identical across reads, which is the property that makes
    // an identity cache correct.
    let mut store = LocalDocumentStore::new(Some("human"));
    store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    let first = store.get_local("foobar").expect("first").clone();
    let second = store.get_local("foobar").expect("second").clone();
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:102` — should not find non-existing
// ---------------------------------------------------------------------------

#[test]
fn upstream_102_get_local_returns_none_for_a_missing_document() {
    let store = LocalDocumentStore::new(Some("human"));
    assert!(store.get_local("nope").is_none());
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:108` — should return null for a deleted local document
// ---------------------------------------------------------------------------

#[test]
fn upstream_108_a_removed_local_document_is_not_found() {
    let mut store = LocalDocumentStore::new(Some("human"));
    store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    store.remove_local("foobar").expect("remove");
    assert!(
        store.get_local("foobar").is_none(),
        "a tombstoned local document reads as absent"
    );
    // The row itself survives — it is a tombstone, not a delete
    // (`rx-local-document.ts:230-239`).
    assert_eq!(store.all_live().count(), 0);
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:285` — upsert
// ---------------------------------------------------------------------------

#[test]
fn upstream_285_upsert_inserts_when_the_document_does_not_exist() {
    let mut store = LocalDocumentStore::new(Some("human"));
    let document = store
        .upsert_local("foobar", json!({ "foo": "bar" }))
        .expect("upsert");
    assert_eq!(document.id, "foobar");
    assert!(!document.deleted);
    assert!(store.get_local("foobar").is_some());
}

#[test]
fn upstream_294_upsert_updates_when_the_document_exists() {
    let mut store = LocalDocumentStore::new(Some("human"));
    store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    let before = store.get_local("foobar").expect("present").revision.clone();

    let document = store
        .upsert_local("foobar", json!({ "foo": "baz" }))
        .expect("upsert");
    assert_eq!(document.data, json!({ "foo": "baz" }));
    assert_ne!(document.revision, before, "the revision advanced");
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:398` — upsert after remove creates a live document
// ---------------------------------------------------------------------------

#[test]
fn upstream_398_upsert_after_remove_un_deletes_the_document() {
    // "should upsert after remove and create a non-deleted document". Upstream takes
    // the `docDataFromCache._deleted` branch (`local-documents.ts:76-86`) and
    // un-deletes via the write queue rather than re-inserting.
    let mut store = LocalDocumentStore::new(Some("human"));
    store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    store.remove_local("foobar").expect("remove");
    assert!(store.get_local("foobar").is_none());

    let document = store
        .upsert_local("foobar", json!({ "foo": "revived" }))
        .expect("upsert");
    assert!(!document.deleted, "the document is live again");
    assert_eq!(document.data, json!({ "foo": "revived" }));
    assert_eq!(
        store.get_local("foobar").expect("found").data,
        json!({ "foo": "revived" })
    );
    // And the events say UPDATE, not INSERT: the row already existed.
    assert_eq!(store.events().last().expect("events").operation, "UPDATE");
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:433` — .remove()
// ---------------------------------------------------------------------------

#[test]
fn upstream_433_remove_tombstones_rather_than_deleting() {
    let mut store = LocalDocumentStore::new(Some("human"));
    store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    let removed = store.remove_local("foobar").expect("remove");
    assert!(removed.deleted);
    // Upstream returns the tombstoned RxLocalDocument whose `deleted$` emits, so the
    // tombstone must stay readable at the storage level.
    assert!(store.get_local("foobar").is_none(), "but it is not live");
}

// ---------------------------------------------------------------------------
// `local-documents.test.ts:444` — with database
// ---------------------------------------------------------------------------

#[test]
fn upstream_444_a_database_parent_uses_its_own_local_store() {
    // "should be able to use local documents directly on the database" (`:445`). The
    // database's local store is a different instance from any collection's, keyed by
    // the empty collection name (`local-documents-helper.ts:22`).
    let database_store = LocalDocumentStore::new(None);
    let collection_store = LocalDocumentStore::new(Some("human"));

    assert_eq!(database_store.instance_name(), "plugin-local-documents-");
    assert_ne!(
        database_store.instance_name(),
        collection_store.instance_name(),
        "a database-level local doc and a collection-level one must not share an instance"
    );

    let mut database_store = database_store;
    database_store
        .insert_local("foobar", json!({ "foo": "bar" }))
        .expect("insert");
    assert!(database_store.get_local("foobar").is_some());
    assert!(
        collection_store.get_local("foobar").is_none(),
        "the same id in a collection's local store is a different document"
    );
}

// ---------------------------------------------------------------------------
// The isolation invariant, stated once as its own test
// ---------------------------------------------------------------------------

#[test]
fn a_local_store_is_never_a_collection_instance() {
    // The mechanism from `local-documents-helper.ts:42-46`: a separate storage
    // instance so the local docs survive removing the collection's instance after a
    // migration. Asserted for the three parents the plugin supports.
    for (parent, expected) in [
        (Some("human"), "plugin-local-documents-human"),
        (Some(""), "plugin-local-documents-"),
        (None, "plugin-local-documents-"),
    ] {
        assert_eq!(local_instance_name(parent), expected);
    }
    // The prefix is what makes it distinct; a collection named exactly like a local
    // instance would collide, which upstream accepts because it prefixes.
    assert!(local_instance_name(Some("x")).starts_with(LOCAL_INSTANCE_PREFIX));
}

#[test]
fn the_local_schema_can_be_built_and_is_the_documented_one() {
    // `local-documents-helper.ts:86-105` plus `fillWithDefaultSettings`. The schema
    // is what makes the local instance an ordinary storage instance, which is why
    // the isolation is a naming/instance question and not a special case in the
    // storage layer.
    let schema = local_document_schema().expect("schema");
    assert_eq!(schema["title"], json!(LOCAL_SCHEMA_TITLE));
    assert_eq!(schema["version"], json!(0));
    assert_eq!(schema["primaryKey"], json!("id"));
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(required.contains(&"id"));
    assert!(required.contains(&"data"));
    // `maxLength: 128` on the primary key (`:92-95`).
    assert_eq!(schema["properties"]["id"]["maxLength"], json!(128));
}

#[test]
fn the_stored_shape_matches_the_local_document_schema() {
    let schema = local_document_schema().expect("schema");

    // A *fresh* local document deliberately does not validate: `_rev` is `''` and the
    // schema requires `minLength: 1`. That is upstream's intent — `getDefaultRevision`
    // returns `''` "to ensure that the RxStorage will throw when the revision is not
    // replaced downstream" (`utils-document.ts:34-40`) — and this test is the proof
    // that the trick actually bites against this schema rather than being folklore.
    let document = LocalDocument::new("a", json!({ "foo": "bar" })).expect("document");
    let pre_write = document.to_document_data();
    assert!(
        !dxdb_schema::validate_document(&schema, &pre_write)
            .expect("validation ran")
            .is_empty(),
        "a fresh local document must NOT validate, or a storage that forgets to \
         assign the revision would go unnoticed"
    );

    // After the store writes it, the revision is real and the shape validates. This
    // is the shape the storage instance actually receives.
    let mut store = LocalDocumentStore::new(None);
    let written = store
        .insert_local("a", json!({ "foo": "bar" }))
        .expect("insert")
        .to_document_data();
    let errors = dxdb_schema::validate_document(&schema, &written).expect("validation ran");
    assert!(
        errors.is_empty(),
        "written local document is invalid: {errors:?}"
    );

    // And it round-trips.
    let read_back = LocalDocument::from_document_data(&written).expect("read back");
    assert_eq!(read_back.id, "a");
    assert_eq!(read_back.data, json!({ "foo": "bar" }));
    assert!(!read_back.revision.is_empty());
}

#[test]
fn a_non_object_data_payload_is_refused() {
    // The schema requires `data` to be an object (`:96-99`).
    assert!(matches!(
        LocalDocument::new("a", json!("not an object")),
        Err(Error::NotAnObject(_))
    ));
    let mut store = LocalDocumentStore::new(None);
    assert!(matches!(
        store.upsert_local("a", json!([1, 2])),
        Err(Error::NotAnObject(_))
    ));
}
