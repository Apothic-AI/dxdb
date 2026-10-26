//! The storage contract tests.
//!
//! # Why these exist instead of the conformance suite
//!
//! `docs/GATES.md` names `dxdb-testkit`'s `storage_implementations` suite as the
//! acceptance oracle for every storage, and `make gate-storage` runs it. As of this
//! tranche that crate does not exist in the workspace — `crates/dxdb-testkit/` is
//! absent on `master`, `make gate-storage` therefore cannot run, and the only
//! artifact is `docs/lanes/m2-testkit.md` (a brief). `dxdb-memory`, the parity
//! oracle those suites would be written against, is also still a stub.
//!
//! So these tests are what exists, and they are written to be **replaced** rather
//! than to compete: each one names the upstream assertion it stands in for, so
//! when the ported suite lands the mapping is mechanical and the weaker local test
//! is deleted rather than kept alongside.
//!
//! # What they do assert
//!
//! The properties that are contract, not implementation: per-document atomicity of
//! `bulk_write` with a partial error list, revision-conflict rejection, `_deleted`
//! filtering in both directions, `count` agreeing with `query().len()` including
//! with a limit, `cleanup`'s `true`/`false` meaning, close/remove idempotency and
//! ordering, the change-feed shape, and the "no empty event bulk" rule.
//!
//! Every test runs the *same* `LocalStorageInstance` code the browser runs, over an
//! in-memory `Kv`. That is the point of the `Kv` abstraction: there is no
//! browser-only code path to hide a bug in.

use std::rc::Rc;

use serde_json::{json, Value};

use dxdb_localstorage::{Kv, LocalStorageStorage, MemoryKv};
use dxdb_storage::{
    BulkWriteRow, DocumentData, DocumentWriteData, DynRxStorageInstance, InstanceCreationParams,
    RxStorage, RxStorageInstance, StorageSchema,
};

/// Upstream's `humans` fixture schema (`src/plugins/test-utils/human.ts`), reduced
/// to the fields the storage contract cares about: a string primary key, one
/// optional string field, one optional number field, and an index on each.
///
/// Indexes are given here already `_deleted`-prefixed and primary-key-suffixed, as
/// `fill_with_default_settings` produces them, because `StorageSchema::from_value`
/// takes them as-is by design.
///
/// `_deleted` and `_meta` are declared in `properties` because they have to be:
/// `index_meta` (the ported upstream `getIndexMeta`) is `CI1` — a hard error — for
/// an index field that is not in the schema. That is deliberate on upstream's part
/// (an unrecognised field would otherwise encode as a fixed-width run of spaces and
/// match every document), and it means a storage test that hand-writes a schema
/// must include the meta fields exactly as `fill_with_default_settings` would.
fn humans_schema(version: u32) -> Value {
    json!({
        "version": version,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "name": {"type": "string", "maxLength": 100},
            "age": {"type": "number", "minimum": 0, "maximum": 150, "multipleOf": 1},
            "_deleted": {"type": "boolean"},
            "_rev": {"type": "string", "maxLength": 100},
            "_attachments": {"type": "object"},
            "_meta": {
                "type": "object",
                "properties": {
                    "lwt": {"type": "number", "minimum": 1, "maximum": 1e15, "multipleOf": 0.01},
                    "ck": {"type": "string"}
                },
                "required": ["lwt"]
            }
        },
        "required": ["id", "_deleted", "_rev", "_meta"],
        "indexes": [["_deleted", "name", "id"], ["_deleted", "age", "id"]]
    })
}

fn params(version: u32) -> InstanceCreationParams {
    let schema_value = humans_schema(version);
    InstanceCreationParams {
        database_instance_token: "test-token".to_string(),
        database_name: "testdb".to_string(),
        collection_name: "humans".to_string(),
        schema: StorageSchema::from_value(&schema_value).expect("valid schema"),
        options: json!({}),
        multi_instance: false,
        dev_mode: false,
    }
}

/// Opens an instance over a fresh store.
fn open(kv: &Rc<MemoryKv>) -> Box<dxdb_storage::DynRxStorageInstance<'static>> {
    let storage = LocalStorageStorage::new(Rc::clone(kv));
    within(
        "create_storage_instance",
        storage.create_storage_instance(params(0)),
    )
    .expect("instance opens")
}

fn open_version(
    kv: &Rc<MemoryKv>,
    version: u32,
) -> Box<dxdb_storage::DynRxStorageInstance<'static>> {
    let storage = LocalStorageStorage::new(Rc::clone(kv));
    within(
        "create_storage_instance",
        storage.create_storage_instance(params(version)),
    )
    .expect("instance opens")
}

/// A live document at revision 1.
fn doc(id: &str, name: &str, age: f64) -> DocumentData {
    DocumentData::from_value(json!({
        "id": id,
        "name": name,
        "age": age,
        "_deleted": false,
        "_rev": format!("1-{id}"),
        "_meta": {"lwt": 1000.0, "ck": ""}
    }))
    .expect("valid document")
}

/// A tombstone for `id` at revision 2.
fn deleted_doc(id: &str) -> DocumentData {
    DocumentData::from_value(json!({
        "id": id,
        "name": "gone",
        "age": 0,
        "_deleted": true,
        "_rev": format!("2-{id}"),
        "_meta": {"lwt": 2000.0, "ck": ""}
    }))
    .expect("valid tombstone")
}

fn insert(document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::insert(DocumentWriteData::new(document))
}

fn update(previous: DocumentData, document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::update(previous, DocumentWriteData::new(document))
}

fn empty_query() -> dxdb_storage::PreparedQuery {
    use dxdb_storage::{MangoQuery, PlanKey, QueryPlan, SortPart};
    dxdb_storage::PreparedQuery::new(
        MangoQuery {
            selector: json!({}),
            sort: vec![SortPart::asc("id")],
            skip: 0,
            limit: None,
            index: None,
        },
        QueryPlan {
            index: vec!["_deleted".into(), "id".into()],
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

fn write(
    instance: &DynRxStorageInstance<'_>,
    rows: Vec<BulkWriteRow>,
) -> dxdb_storage::BulkWriteResponse {
    within(
        "bulk_write",
        instance.bulk_write(rows, "test-context".to_string()),
    )
    .expect("bulk_write does not fail wholesale")
}

fn find(instance: &DynRxStorageInstance<'_>, id: &str, with_deleted: bool) -> Option<DocumentData> {
    within(
        "find_documents_by_id",
        instance.find_documents_by_id(vec![id.to_string()], with_deleted),
    )
    .expect("find succeeds")
    .into_iter()
    .next()
}

fn ids(documents: &[DocumentData]) -> Vec<String> {
    documents
        .iter()
        .map(|d| {
            d.get_object_path("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The watchdog: no test in this file is allowed to block forever
// ---------------------------------------------------------------------------
//
// A contract test that can hang is worse than one that fails. It held the
// fleet-wide build lock for 58 minutes, and from outside it looked like an idle
// box rather than a bug: load average 0.01, no compiler running, every other lane
// queued behind a futex wait. This file caused exactly that, so it carries its own
// cure.
//
// Every await point goes through `within`, which races the future against a
// watchdog thread. A future that does not finish within `DEADLINE` prints which
// test hung and aborts the process: a failure with a name, at a boundary, instead
// of silence.
//
// Deliberately a thread and `process::abort` rather than
// `futures::FutureExt::timeout`: a timer future needs a reactor, this crate has no
// runtime by policy, and a timeout that is itself driven by the executor cannot
// fire when the executor is the thing that is blocked. `abort` is required because
// the panic would have to unwind out of a blocked thread, which is exactly what
// cannot happen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long any single await may take before the process is aborted. Generous:
/// these operations are in-memory over a `HashMap`, so anything near this is
/// definitionally stuck rather than slow.
const DEADLINE: Duration = Duration::from_secs(10);

/// Drives `future` to completion, aborting the process if it takes longer than
/// [`DEADLINE`].
///
/// `what` names the await point, so the abort message says which test hung.
fn within<F: std::future::Future>(what: &str, future: F) -> F::Output {
    let finished = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finished);
    let label = what.to_string();
    std::thread::spawn(move || {
        let steps = DEADLINE.as_millis() / 25;
        for _ in 0..steps {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!(
            "\nHANG: `{label}` did not complete within {DEADLINE:?}.\n\
             A storage contract test blocked forever. This is a deadlock, not slow \
             code: nothing is computing. Usual causes are a channel whose sender was \
             dropped without closing, a lock held across an await, or a test awaiting \
             an event the write path never emits.\n\
             Aborting so the build lock is released and the failure has a name.\n"
        );
        std::process::abort();
    });

    let output = futures::executor::block_on(future);
    finished.store(true, Ordering::SeqCst);
    output
}

// ---------------------------------------------------------------------------
// bulk_write
// ---------------------------------------------------------------------------

/// Upstream: "should insert a document" / "should find it again".
#[test]
fn inserts_and_reads_back_a_document() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);

    let response = write(&instance, vec![insert(doc("a", "alice", 30.0))]);
    assert!(
        response.is_ok(),
        "insert reported errors: {:?}",
        response.error
    );

    let found = find(&instance, "a", false).expect("document is stored");
    assert_eq!(
        found.get_object_path("name").and_then(Value::as_str),
        Some("alice")
    );
}

/// Upstream: a bulk write is **per-document atomic** — some rows succeed, the
/// failures come back per row, and there is no all-or-nothing rollback.
#[test]
fn bulk_write_is_per_document_atomic() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    write(&instance, vec![insert(doc("a", "alice", 30.0))]);

    let response = write(
        &instance,
        vec![
            insert(doc("b", "bob", 40.0)), // must succeed
            // A conflicting insert: the id already exists, so `previous: None`
            // against an existing row is a 409 and must not be applied.
            insert(doc("a", "impostor", 99.0)),
            insert(doc("c", "carol", 50.0)), // must succeed
        ],
    );

    assert_eq!(response.error.len(), 1, "exactly one row must fail");
    let failure = &response.error[0];
    assert_eq!(failure.document_id(), "a");
    assert!(failure.is_conflict(), "the failure must be a 409 conflict");
    assert_eq!(failure.status(), 409);

    // The two good rows landed despite the failure in between.
    assert!(find(&instance, "b", false).is_some(), "b was written");
    assert!(find(&instance, "c", false).is_some(), "c was written");
    // And the failed row did not overwrite the good state.
    let a = find(&instance, "a", false).expect("a is still there");
    assert_eq!(
        a.get_object_path("name").and_then(Value::as_str),
        Some("alice")
    );
}

/// Upstream: an update whose `previous` does not match the stored revision is a
/// conflict and must not be applied.
#[test]
fn stale_update_is_rejected_and_leaves_state_alone() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    let original = doc("a", "alice", 30.0);
    write(&instance, vec![insert(original.clone())]);

    // A second writer saw a *different revision*. The other fields are deliberately
    // the same, because conflict detection compares `_rev` and nothing else
    // (categorize.rs's table: "previous._rev != stored._rev -> 409"). An earlier
    // version of this test changed `name` and left `_rev` in place, which is a
    // matching revision and therefore a legitimate update, not a conflict.
    let stale_previous = DocumentData::from_value(json!({
        "id": "a", "name": "alice", "age": 30.0,
        "_deleted": false,
        "_rev": "1-someone-elses-token",
        "_meta": {"lwt": 999.0, "ck": ""}
    }))
    .unwrap();
    let response = write(
        &instance,
        vec![update(stale_previous, doc("a", "overwritten", 2.0))],
    );

    assert_eq!(response.error.len(), 1);
    assert!(response.error[0].is_conflict());
    let a = find(&instance, "a", false).expect("still present");
    assert_eq!(
        a.get_object_path("name").and_then(Value::as_str),
        Some("alice"),
        "a conflicting update must not change the stored document"
    );
}

/// Upstream: a correct update succeeds and bumps the revision it stores.
#[test]
fn a_matching_update_is_applied() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    let original = doc("a", "alice", 30.0);
    write(&instance, vec![insert(original.clone())]);

    let updated = DocumentData::from_value(json!({
        "id": "a", "name": "alice", "age": 31.0,
        "_deleted": false,
        "_rev": "2-aaa",
        "_meta": {"lwt": 1500.0, "ck": ""}
    }))
    .unwrap();
    let response = write(&instance, vec![update(original, updated)]);
    assert!(response.is_ok(), "update failed: {:?}", response.error);

    let a = find(&instance, "a", false).expect("present");
    assert_eq!(a.get_object_path("age").and_then(Value::as_f64), Some(31.0));
}

// ---------------------------------------------------------------------------
// deleted documents
// ---------------------------------------------------------------------------

/// Upstream: `findDocumentsById(ids, false)` hides tombstones and
/// `findDocumentsById(ids, true)` includes them. Both directions are asserted
/// because a tombstone invisible to `true` would make conflict resolution
/// impossible.
#[test]
fn with_deleted_controls_visibility_of_tombstones() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    let original = doc("a", "alice", 30.0);
    write(&instance, vec![insert(original.clone())]);

    let response = write(&instance, vec![update(original, deleted_doc("a"))]);
    assert!(response.is_ok(), "delete failed: {:?}", response.error);

    assert!(
        find(&instance, "a", false).is_none(),
        "a tombstone must be invisible with with_deleted = false"
    );
    let tombstone = find(&instance, "a", true).expect("visible with with_deleted = true");
    assert_eq!(
        tombstone
            .get_object_path("_deleted")
            .and_then(Value::as_bool),
        Some(true)
    );
}

// ---------------------------------------------------------------------------
// query and count
// ---------------------------------------------------------------------------

/// Upstream: `count()` equals `query().documents.length`, **including with a
/// limit** — `.count({limit: 3})` must return what
/// `.query({limit: 3}).documents.length` returns.
#[test]
fn count_agrees_with_query_including_with_a_limit() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);

    let rows: Vec<_> = (0..10)
        .map(|i| insert(doc(&format!("h{i}"), &format!("human{i}"), f64::from(i))))
        .collect();
    write(&instance, rows);

    let unlimited = within("query", instance.query(empty_query())).expect("query");
    let count_unlimited = within("count", instance.count(empty_query())).expect("count");
    assert_eq!(count_unlimited.count, unlimited.len());
    assert_eq!(count_unlimited.count, 10);

    let mut limited_query = empty_query();
    limited_query.query.limit = Some(3);
    let limited = within("query limited", instance.query(limited_query.clone())).expect("query");
    let limited_count = within("count limited", instance.count(limited_query)).expect("count");
    assert_eq!(limited.len(), 3);
    assert_eq!(
        limited_count.count, 3,
        "count with a limit must match query with that limit"
    );
}

/// Upstream: a query returns documents in sort order and skips tombstones.
#[test]
fn query_returns_in_sort_order_and_skips_deleted() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);

    write(
        &instance,
        vec![
            insert(doc("c", "carol", 3.0)),
            insert(doc("a", "alice", 1.0)),
            insert(doc("b", "bob", 2.0)),
        ],
    );

    let results = within("query", instance.query(empty_query())).expect("query");
    assert_eq!(ids(&results), vec!["a", "b", "c"], "ascending by id");
}

// ---------------------------------------------------------------------------
// change feed
// ---------------------------------------------------------------------------

/// Upstream: a write emits one `EventBulk` with a checkpoint on the change feed.
#[test]
fn a_write_emits_an_event_bulk() {
    use futures::StreamExt as _;

    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);

    let mut stream = instance.change_stream();
    write(&instance, vec![insert(doc("a", "alice", 30.0))]);

    let bulk = within("change_stream next", stream.next()).expect("an event bulk");
    assert_eq!(bulk.events.len(), 1);
    assert_eq!(bulk.events[0].document_id, "a");
    assert_eq!(bulk.context, "test-context");
    assert!(
        bulk.checkpoint.is_some(),
        "a non-empty bulk must carry a checkpoint"
    );
}

/// Upstream: "should not emit an empty eventBulk when the write had only errors".
/// A write that changed nothing must look like no write at all on the feed.
#[test]
fn a_failed_write_emits_nothing() {
    use futures::StreamExt as _;

    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    write(&instance, vec![insert(doc("a", "alice", 30.0))]);

    let mut stream = instance.change_stream();
    let response = write(&instance, vec![insert(doc("a", "impostor", 99.0))]);
    assert_eq!(response.error.len(), 1);

    // Nothing was written, so nothing must be emitted — but the feed is *live*, so
    // waiting for the stream to end would hang. Poll once with a discarded timeout
    // instead: a pending result is the pass condition.
    //
    // This distinction is the reason the assertion is written this way. An earlier
    // version awaited `stream.next()` and deadlocked the whole `make test`, because
    // the feed correctly stays open after a write that emitted nothing.
    let next = futures::future::poll_immediate(stream.next());
    futures::pin_mut!(next);
    // `poll_immediate` yields `Some(item)` if ready, `None` if it would block.
    let immediate = within("change_stream poll", next);
    assert!(
        immediate.is_none(),
        "a write that only produced errors must not emit a bulk, got {immediate:?}"
    );
}

// ---------------------------------------------------------------------------
// cleanup
// ---------------------------------------------------------------------------

/// Upstream: `cleanup` purges tombstones older than `minimumDeletedTime` and
/// returns `true` when there is nothing left to do.
#[test]
fn cleanup_purges_old_tombstones_and_reports_done() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    let original = doc("a", "alice", 30.0);
    write(&instance, vec![insert(original.clone())]);
    write(&instance, vec![update(original, deleted_doc("a"))]);

    // Everything is cleanable: `now()` is far past the tombstone's lwt of 2000.
    let done = within("cleanup", instance.cleanup(0.0)).expect("cleanup");
    assert!(
        done,
        "localStorage's sweep is exhaustive, so it reports done"
    );

    assert!(
        find(&instance, "a", true).is_none(),
        "the tombstone must be gone after cleanup"
    );

    // A second pass finds nothing and still reports done.
    let again = within("cleanup", instance.cleanup(0.0)).expect("cleanup");
    assert!(again);
}

/// A tombstone newer than the threshold survives.
#[test]
fn cleanup_keeps_recent_tombstones() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    let original = doc("a", "alice", 30.0);
    write(&instance, vec![insert(original.clone())]);
    write(&instance, vec![update(original, deleted_doc("a"))]);

    // A huge `minimumDeletedTime` makes every tombstone too new to purge.
    let done = within("cleanup recent", instance.cleanup(1e18)).expect("cleanup");
    assert!(done);
    assert!(
        find(&instance, "a", true).is_some(),
        "a recent tombstone must survive cleanup"
    );
}

// ---------------------------------------------------------------------------
// close / remove
// ---------------------------------------------------------------------------

/// Upstream: "closing multiple times should not error".
#[test]
fn close_is_idempotent() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    within("close", instance.close()).expect("first close");
    within("close again", instance.close()).expect("second close");
}

/// Upstream has two separate assertions here: `.remove()` after `.close()` must
/// error, and `.close()` after `.remove()` must not.
#[test]
fn remove_and_close_follow_the_documented_ordering_rules() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    write(&instance, vec![insert(doc("a", "alice", 30.0))]);

    within("close", instance.close()).expect("close");
    assert!(
        within("remove after close", instance.remove()).is_err(),
        "remove() after close() must error"
    );

    let kv2 = Rc::new(MemoryKv::new());
    let second_kv = Rc::clone(&kv2);
    let second = open(&kv2);
    write(&second, vec![insert(doc("b", "bob", 40.0))]);
    within("remove", second.remove()).expect("remove");
    within("close after remove", second.close()).expect("close after remove must not error");

    // `remove()` also marks the instance removed, so reads through it now fail with
    // `Removed` rather than returning nothing — that is the trait's contract
    // (`StorageError::Removed`: "every other instance on the same collection is dead
    // too"). So the assertion is about the *store*, read through a fresh `Kv` handle:
    // no document keys may remain.
    let remaining: Vec<String> = second_kv
        .sorted_keys()
        .into_iter()
        .filter(|k| k.contains("/doc/"))
        .collect();
    assert!(
        remaining.is_empty(),
        "remove must delete the documents, but these keys remain: {remaining:?}"
    );
}

/// A closed instance refuses further writes rather than silently accepting them.
#[test]
fn a_closed_instance_refuses_writes() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    within("close", instance.close()).expect("close");

    let result = within(
        "bulk_write after close",
        instance.bulk_write(
            vec![insert(doc("a", "alice", 30.0))],
            "after-close".to_string(),
        ),
    );
    assert!(
        matches!(result, Err(dxdb_storage::StorageError::Closed)),
        "expected StorageError::Closed, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// schema versions and quota
// ---------------------------------------------------------------------------

/// Upstream: "should not mix up documents stored with different schema versions".
/// The version is part of the collection key, so two instances over one store must
/// not see each other's documents.
#[test]
fn schema_versions_do_not_share_state() {
    let kv = Rc::new(MemoryKv::new());
    let v0 = open_version(&kv, 0);
    let v1 = open_version(&kv, 1);

    write(&v0, vec![insert(doc("a", "in-v0", 1.0))]);

    assert!(
        find(&v0, "a", true).is_some(),
        "the writer sees its own document"
    );
    assert!(
        find(&v1, "a", true).is_none(),
        "a different schema version must not see v0's documents"
    );
}

/// A full quota surfaces as an error rather than a panic or a silent drop. This is
/// the failure a browser user actually hits, so it is worth a test even though a
/// `HashMap` cannot really fill.
#[test]
fn a_quota_failure_is_reported_not_swallowed() {
    let kv = Rc::new(MemoryKv::new());
    let instance = open(&kv);
    write(&instance, vec![insert(doc("a", "alice", 30.0))]);

    kv.fail_all_writes();
    let result = within(
        "bulk_write under quota pressure",
        instance.bulk_write(vec![insert(doc("b", "bob", 40.0))], "quota".to_string()),
    );
    assert!(
        result.is_err(),
        "a quota failure must surface rather than be reported as a successful write"
    );
}

/// `MemoryKv`'s own contract, so the fixture the other tests rely on is itself
/// checked.
#[test]
fn memory_kv_behaves_like_a_kv() {
    let kv = MemoryKv::new();
    assert!(kv.get_item("missing").expect("read").is_none());
    kv.set_item("k", "v").expect("write");
    assert_eq!(kv.get_item("k").expect("read").as_deref(), Some("v"));
    assert_eq!(kv.keys().expect("keys"), vec!["k".to_string()]);
    kv.remove_item("k").expect("remove");
    assert!(kv.get_item("k").expect("read").is_none());
    // Removing an absent key succeeds, as in localStorage.
    kv.remove_item("k").expect("remove absent key");
}
