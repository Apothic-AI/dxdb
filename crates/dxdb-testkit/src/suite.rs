//! The executable suites: ported cases, generic over the storage traits.
//!
//! Each function here is one upstream case, or a small number of upstream cases that
//! share a body. The signature is generic over [`RxStorage`] so the same code runs
//! against memory, sqlite, indexeddb or a clean-room storage, which is the entire
//! point of the testkit.
//!
//! # Anatomy of a ported case
//!
//! ```ignore
//! /// Upstream: `test/unit/rx-storage-implementations.test.ts:270`
//! pub async fn should_write_the_document<S: RxStorage>(storage: &S) {
//!     // ... the upstream body, translated
//! }
//! ```
//!
//! Every function carries the upstream line in its doc comment. The declaration table
//! in [`crate::decl`] carries the same line, and a test asserts the two agree, so a
//! case cannot be declared exercised while its body is missing or drifted.
//!
//! # What "exercised" means here
//!
//! A case is exercised when calling the function against a storage can fail. That
//! rules out:
//!
//! * a function that opens an instance and asserts nothing;
//! * a case whose assertion is weaker than upstream's (a `find` asserted only for
//!   "no error" where upstream asserts the document contents);
//! * a case that returns early on the storage it is running against.
//!
//! Where this crate's assertion is weaker than upstream's, the divergence is a
//! comment at the assertion, per the lane brief's fidelity rule 3.

use serde_json::{Map, Value};

use dxdb_storage::{
    BulkWriteRow, Checkpoint, DocumentData, Operation, RxStorage, RxStorageInstance,
    WRITE_ERROR_CONFLICT,
};

use crate::fixtures::objects::{EXAMPLE_REVISION_1, EXAMPLE_REVISION_2, EXAMPLE_REVISION_3};
use crate::fixtures::write::{insert_row, test_doc_write_data, update_row};
use crate::harness::{describe, fill_with_default_settings, ids, Harness, InstanceSpec};

/// The context string every write in the ported suite passes, matching upstream's
/// `testContext`.
pub const TEST_CONTEXT: &str = "rx-storage-implementations.test.ts";

/// Opens a `TestDocType` instance: pseudo-schema version 0, primary key `key`.
async fn open_test_doc<S: RxStorage>(storage: &S) -> (Harness, InstanceSpec) {
    let spec = InstanceSpec::new(fill_with_default_settings(
        crate::schemas::pseudo_schema_for_version(0, "key"),
    ));
    let harness = Harness::open(storage, &spec)
        .await
        .expect("create_storage_instance failed");
    (harness, spec)
}

/// An `lwt` that is *recent*, *ordered*, and **never in the future**.
///
/// Three properties, all load-bearing, and getting the third one wrong twice is why this
/// has such a long comment:
///
/// * **Ordered and distinct**, so a case asserting `(_meta.lwt, primary)` order or that a
///   checkpoint advances has something to assert. `lwt(2) > lwt(1)`.
/// * **Recent**, so a retention assertion is about retention. The first version used the
///   constant `1_700_000_000_000`; that is 2023-11-14, so every fixture tombstone was
///   1,076 days old and `cleanup(1000 * 60 * 6)` — a six-minute window — correctly purged
///   it. Three cases failed against a scaffold that was right.
/// * **Not in the future.** The second version used `base + n`, where `base` is `now()`.
///   With `n` up to a few hundred that puts `lwt` milliseconds *ahead* of the clock, and
///   `cleanup(0)` — whose cutoff is exactly `now()` — then correctly refuses to purge
///   them, because a tombstone cannot be aged out before it was written. That failed two
///   different cases, again against a correct scaffold.
///
/// So every `lwt` is anchored to a base that is a few seconds in the **past**, and `n`
/// ascends from there: `lwt(1) = base + 1`, `lwt(2) = base + 2`, and so on. Ascending, so
/// `lwt(2) > lwt(1)` as every ordering and checkpoint assertion expects; anchored in the
/// past by enough headroom that even a few hundred distinct documents stay behind `now()`.
fn lwt(n: u64) -> f64 {
    base_lwt() + n as f64
}

/// The fixture `lwt` base, exposed so a test can assert it is near the clock.
///
/// Public because the assertion that matters — "the fixture is recent enough that a
/// retention window admits it" — is a property of this number, and duplicating the number
/// in a test is how the original bug survived review.
pub fn fixture_lwt_base() -> f64 {
    base_lwt()
}

/// The `lwt` base for one process: `now()`, floored to a whole millisecond, minus a
/// one-second margin, resolved once.
///
/// Once per process, so two cases never share a base that moved between them and a case
/// that writes `lwt(1)` then reads it back sees the same number.
///
/// The one-second margin exists so that `lwt(0)` — the newest fixture document — is
/// comfortably in the past even if the process runs for a while before a `cleanup` call.
/// Without it, a long test run could put `now()` *before* `base` for a case late in the
/// binary, which is the future-dated bug in a subtler form. A second is far below any
/// retention window a case uses (the smallest is zero, and its cutoff is `now()`, which is
/// still later than `base`), so it costs nothing.
fn base_lwt() -> f64 {
    use std::sync::OnceLock;
    static BASE: OnceLock<f64> = OnceLock::new();
    *BASE.get_or_init(|| dxdb_storage::now().floor() - FIXTURE_LWT_HEADROOM_MS)
}

/// How far in the past the fixture `lwt` base sits.
///
/// Large enough that a case may write a few hundred ascending `lwt`s (`lwt(500)`) and still
/// be behind `now()`, small enough that the fixture is unambiguously *recent* for any
/// retention window a case uses. One minute satisfies both: the largest `n` in the suite is
/// under a thousand, so the newest fixture document is at most a second past the base and
/// still ~59 seconds before now.
const FIXTURE_LWT_HEADROOM_MS: f64 = 60_000.0;

// ---------------------------------------------------------------------------
// creation
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:176` — `open and close`.
///
/// Upstream asserts that creating an instance does not mutate the collection or
/// database name it was given, then removes it. The first half is the part that
/// catches a storage that normalises a name in place (trimming, lowercasing) on the
/// object it was handed.
pub async fn open_and_close<S: RxStorage>(storage: &S) {
    let database_name = crate::util::random_token(12);
    let collection_name = crate::util::random_token(12);
    let spec = InstanceSpec::named(
        fill_with_default_settings(crate::schemas::pseudo_schema_for_version(0, "key")),
        &database_name,
        &collection_name,
    );
    let harness = Harness::open(storage, &spec)
        .await
        .expect("create_storage_instance failed");

    assert_eq!(
        RxStorageInstance::collection_name(harness.instance.as_ref()),
        collection_name,
        "creation must not mutate the collection name it was given"
    );
    assert_eq!(
        RxStorageInstance::database_name(harness.instance.as_ref()),
        database_name,
        "creation must not mutate the database name it was given"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:195` — `open many instances
/// on the same database name`.
///
/// Upstream opens 20 (5 on deno); the count is fixed here because a Rust test has no
/// `isDeno`. Each has a *different* collection name, which is what makes them
/// independent; a storage that keys its state on the database name alone fails here.
pub async fn open_many_instances_same_database<S: RxStorage>(storage: &S) {
    let database_name = crate::util::random_token(12);
    let mut opened = Vec::new();
    for _ in 0..20 {
        let spec = InstanceSpec::named(
            fill_with_default_settings(crate::schemas::pseudo_schema_for_version(0, "key")),
            &database_name,
            &crate::util::random_token(12),
        );
        opened.push(
            Harness::open(storage, &spec)
                .await
                .expect("create_storage_instance failed"),
        );
    }
    for harness in opened {
        harness.remove().await;
    }
}

// ---------------------------------------------------------------------------
// .bulkWrite()
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:270` — `should write the
/// document`.
pub async fn should_write_the_document<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let document = test_doc_write_data("foobar", "barfoo", lwt(1), Map::new());

    harness.write_ok(vec![insert_row(document.clone())]).await;

    let found = harness.find(&["foobar"], false).await;
    assert_eq!(found.len(), 1, "the written document must be findable");
    assert_eq!(
        found[0].get("value").and_then(Value::as_str),
        Some("barfoo"),
        "the written document must round-trip its payload"
    );
    // Upstream also asserts the stored revision equals the written one. That is the
    // assertion that catches a storage which rewrites `_rev` on write.
    assert_eq!(
        found[0].rev(),
        Some(EXAMPLE_REVISION_1),
        "a storage must store the revision it was given, not invent one"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:309` — `should error on
/// conflict`.
///
/// Two writes of the same primary with no `previous` on the second: the response
/// carries exactly one error, its status is 409, and the stored document is
/// unchanged.
pub async fn should_error_on_conflict<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let first = test_doc_write_data("foobar", "barfoo", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(first.clone())]).await;

    let second = test_doc_write_data("foobar", "barfoo2", lwt(2), Map::new());
    let response = harness.write(vec![insert_row(second)]).await;

    assert_eq!(
        response.error.len(),
        1,
        "inserting over an existing document must produce exactly one error"
    );
    assert_eq!(
        response.error[0].status(),
        WRITE_ERROR_CONFLICT,
        "the error status must be 409 (upstream `assert.strictEqual(error.status, 409)`)"
    );
    assert_eq!(response.error[0].document_id(), "foobar");

    let found = harness.find(&["foobar"], false).await;
    assert_eq!(
        found[0].get("value").and_then(Value::as_str),
        Some("barfoo"),
        "a conflicting write must not be applied"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:372` — `when inserting the
/// same document at the same time, the first call must succeed while the second has a
/// conflict`.
///
/// The two writes are issued as two separate awaited calls rather than as
/// `Promise.all`. In upstream the point is that both are *in flight* together; in Rust
/// the sequential executor (D3) has no other ordering available, so issuing them in
/// sequence asserts the same invariant — the second must observe the first — and the
/// comment records why the shape changed.
pub async fn concurrent_insert_conflicts<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let first = test_doc_write_data("foobar", "barfoo", lwt(1), Map::new());
    let second = test_doc_write_data("foobar", "barfoo2", lwt(2), Map::new());

    let first_response = harness.write(vec![insert_row(first)]).await;
    assert!(
        first_response.is_ok(),
        "the first insert must succeed, got {:?}",
        first_response.error
    );
    let second_response = harness.write(vec![insert_row(second)]).await;
    assert_eq!(second_response.error.len(), 1);
    assert_eq!(second_response.error[0].status(), WRITE_ERROR_CONFLICT);

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:420` — `should not find the
/// deleted document when findDocumentsById(false)`.
pub async fn deleted_not_found_without_with_deleted<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("foobar", "barfoo", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(live.clone())]).await;

    let tombstone = test_doc_write_data(
        "foobar",
        "barfoo",
        lwt(2),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(live.clone(), tombstone.clone())])
        .await;

    let without = harness.find(&["foobar"], false).await;
    assert!(
        without.is_empty(),
        "with_deleted=false must hide the tombstone, got {:?}",
        without.iter().map(describe).collect::<Vec<_>>()
    );

    let with = harness.find(&["foobar"], true).await;
    assert_eq!(
        with.len(),
        1,
        "with_deleted=true must return the tombstone"
    );
    assert!(
        with[0].is_deleted(),
        "the returned document must be marked deleted"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:483` — `should NOT be able
/// to overwrite a deleted the document`.
///
/// A tombstone cannot be replaced by an insert that does not pass `previous`, and it
/// cannot be replaced by an update whose `previous` is the *pre-delete* state. Both
/// are 409s; the second is the subtle one, because the document did exist with that
/// revision once.
pub async fn cannot_overwrite_deleted_without_previous<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("foobar", "barfoo", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(live.clone())]).await;

    let tombstone = test_doc_write_data(
        "foobar",
        "barfoo",
        lwt(2),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(live.clone(), tombstone.clone())])
        .await;

    // 1. An insert with no `previous` over a tombstone.
    let as_insert =
        test_doc_write_data("foobar", "barfoo3", lwt(3), Map::new());
    let response = harness.write(vec![insert_row(as_insert)]).await;
    assert_eq!(
        response.error.len(),
        1,
        "an insert over a tombstone must be rejected"
    );
    assert_eq!(response.error[0].status(), WRITE_ERROR_CONFLICT);

    // 2. An update whose `previous` is the state before the delete.
    let as_stale_update = test_doc_write_data(
        "foobar",
        "barfoo4",
        lwt(4),
        Map::from_iter([(
            "_rev".to_string(),
            Value::String(EXAMPLE_REVISION_3.to_string()),
        )]),
    );
    let response = harness
        .write(vec![update_row(live.clone(), as_stale_update)])
        .await;
    assert_eq!(
        response.error.len(),
        1,
        "updating over a tombstone from a stale previous must be rejected"
    );
    assert_eq!(response.error[0].status(), WRITE_ERROR_CONFLICT);

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:600` — `should be able to
/// update the state of a deleted document`.
///
/// A tombstone is a real document: it can be updated to a new revision and keeps the
/// payload written to it, while remaining deleted. A storage that treats a delete as a
/// removal fails here.
pub async fn update_deleted_document_state<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("foobar", "barfoo1", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(live.clone())]).await;

    let tombstone = test_doc_write_data(
        "foobar",
        "barfoo1",
        lwt(2),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(live.clone(), tombstone.clone())])
        .await;

    let modified = test_doc_write_data(
        "foobar",
        "barfoo2",
        lwt(3),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_3.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(tombstone.clone(), modified)])
        .await;

    let found = harness.find(&["foobar"], true).await;
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].get("value").and_then(Value::as_str),
        Some("barfoo2"),
        "a tombstone's payload must be updatable"
    );
    assert!(
        found[0].is_deleted(),
        "the document must still be deleted after the update"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:661` — `should be able to
/// unset a property`.
///
/// Writing a document without a property must *remove* it, not leave the old value and
/// not write `null`. This is the difference between a patch and a replace, and a
/// storage that merges instead of replacing silently resurrects old data.
pub async fn unset_property<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let first = test_doc_write_data("foobar", "barfoo", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(first.clone())]).await;

    // The second document simply omits `value`.
    let mut without_value = Map::new();
    without_value.insert("key".to_string(), Value::String("foobar".to_string()));
    let second = crate::fixtures::write::write_data_with(
        without_value,
        lwt(2),
        Map::from_iter([(
            "_rev".to_string(),
            Value::String(EXAMPLE_REVISION_2.to_string()),
        )]),
    );
    harness
        .write_ok(vec![update_row(first.clone(), second)])
        .await;

    let found = harness.find(&["foobar"], false).await;
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].get("value"),
        None,
        "an omitted property must be removed, not retained or nulled"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:773` — `should be able to do
/// a write where only _meta fields are changed`.
///
/// The revised document differs only in `_rev` and `_meta.lwt`. Upstream asserts the
/// stored revision parses at the expected height, which is the assertion that catches a
/// storage that special-cases "nothing changed" and skips the write.
pub async fn write_only_meta_fields_changed<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let first = test_doc_write_data("foobar", "barfoo1", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(first.clone())]).await;

    let meta_only = test_doc_write_data(
        "foobar",
        "barfoo1",
        lwt(2),
        Map::from_iter([(
            "_rev".to_string(),
            Value::String(EXAMPLE_REVISION_2.to_string()),
        )]),
    );
    harness
        .write_ok(vec![update_row(first.clone(), meta_only.clone())])
        .await;

    let found = harness.find(&["foobar"], false).await;
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].rev(),
        Some(EXAMPLE_REVISION_2),
        "a _meta-only write must still advance the stored revision"
    );
    assert_eq!(
        found[0].lwt(),
        Some(lwt(2)),
        "a _meta-only write must store the new lwt"
    );

    // Upstream then writes a third revision and asserts the height parses as 3.
    let third = test_doc_write_data(
        "foobar",
        "barfoo1",
        lwt(3),
        Map::from_iter([(
            "_rev".to_string(),
            Value::String(EXAMPLE_REVISION_3.to_string()),
        )]),
    );
    harness
        .write_ok(vec![update_row(meta_only, third)])
        .await;
    let found = harness.find(&["foobar"], false).await;
    let height = dxdb_storage::get_height_of_revision(found[0].rev().expect("rev"));
    assert_eq!(height, 3, "the third write must be at revision height 3");

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:848` — `should be able to
/// create another instance after a write`.
///
/// A *second* instance, on a different collection name, writes the same document.
/// Upstream uses a fresh collection name, so this is really "creating an instance after
/// another wrote does not corrupt state" — a storage that caches a schema across
/// collections fails here.
pub async fn create_second_instance_after_write<S: RxStorage>(storage: &S) {
    let (harness, spec) = open_test_doc(storage).await;
    let document = test_doc_write_data("foobar", "barfoo1", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(document.clone())]).await;

    let second_spec = spec.clone().collection(&crate::util::random_token(12));
    let second = Harness::open(storage, &second_spec)
        .await
        .expect("second create_storage_instance failed");
    let revised = test_doc_write_data(
        "foobar",
        "barfoo1",
        lwt(2),
        Map::from_iter([(
            "_rev".to_string(),
            Value::String(EXAMPLE_REVISION_2.to_string()),
        )]),
    );
    second.write_ok(vec![insert_row(revised)]).await;

    harness.close().await;
    second.close().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:900` — `should be able to
/// jump more than 1 revision height in a single write operation`.
///
/// Writing revision 3 over revision 1 must be accepted: the storage compares the
/// previous *revision string*, not the height, so a height jump is legal. A storage
/// that validates `new height == old height + 1` rejects this and breaks migration.
pub async fn revision_height_may_jump<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let first = test_doc_write_data("foobar", "barfoo", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(first.clone())]).await;

    let jumped = test_doc_write_data(
        "foobar",
        "barfoo2",
        lwt(2),
        Map::from_iter([(
            "_rev".to_string(),
            Value::String(EXAMPLE_REVISION_3.to_string()),
        )]),
    );
    let response = harness.write(vec![update_row(first.clone(), jumped)]).await;
    assert!(
        response.is_ok(),
        "a height jump must be accepted, got {:?}",
        response.error
    );

    let found = harness.find(&["foobar"], false).await;
    assert_eq!(
        found[0].rev(),
        Some(EXAMPLE_REVISION_3),
        "the jumped revision must be the stored one"
    );

    harness.close().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:951` — `must be able create
/// multiple storage instances on the same database and write documents`.
///
/// 3 collections x 3 documents on one database name.
pub async fn multiple_instances_same_database_write<S: RxStorage>(storage: &S) {
    let database_name = crate::util::random_token(10);
    let database_instance_token = crate::util::random_token(10);
    let mut harnesses = Vec::new();
    let mut next_lwt = 1u64;

    for _ in 0..3 {
        let mut spec = InstanceSpec::named(
            fill_with_default_settings(crate::schemas::pseudo_schema_for_version(0, "key")),
            &database_name,
            &crate::util::random_token(12),
        );
        spec.database_instance_token = database_instance_token.clone();
        let harness = Harness::open(storage, &spec)
            .await
            .expect("create_storage_instance failed");

        let mut rows = Vec::new();
        for doc_id in 0..3 {
            rows.push(insert_row(test_doc_write_data(
                &doc_id.to_string(),
                &crate::util::random_token(5),
                lwt(next_lwt),
                Map::new(),
            )));
            next_lwt += 1;
        }
        harness.write_ok(rows).await;
        harnesses.push(harness);
    }

    for harness in harnesses {
        harness.remove().await;
    }
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:996` — `write and read with
/// umlauts`.
///
/// The fixture strings carry `äöüÖÄßÜ`. A storage that compares UTF-8 bytes where it
/// should compare UTF-16 code units, or that mangles a multi-byte character on a
/// round-trip, fails here rather than in production.
pub async fn write_and_read_umlauts<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    // `TEST_DATA_CHARSET` is the upstream charset, so this is upstream's own alphabet.
    let value = format!("äöüÖÄßÜ{}", crate::util::TEST_DATA_CHARSET);
    let document = test_doc_write_data("key-with-umlaut-äöü", &value, lwt(1), Map::new());
    harness.write_ok(vec![insert_row(document)]).await;

    let found = harness.find(&["key-with-umlaut-äöü"], false).await;
    assert_eq!(found.len(), 1, "a umlaut primary value must round-trip");
    assert_eq!(
        found[0].get("value").and_then(Value::as_str),
        Some(value.as_str()),
        "a umlaut payload must round-trip byte for byte"
    );

    harness.close().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:1058` — `the plain storage
/// should throw when overwriting a deleted doc with an insert without passing the
/// previous`.
///
/// Where [`cannot_overwrite_deleted_without_previous`] asserts the 409 in the error
/// list, upstream's version here covers the resurrections path: a *correct* resurrection
/// (`previous` set to the tombstone, `_deleted: false`, bumped revision) must be
/// accepted and reported as an INSERT.
pub async fn resurrect_deleted_document_with_previous<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("foobar", "barfoo", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(live.clone())]).await;

    let tombstone = test_doc_write_data(
        "foobar",
        "barfoo",
        lwt(2),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(live.clone(), tombstone.clone())])
        .await;

    let resurrected = test_doc_write_data(
        "foobar",
        "barfoo",
        lwt(3),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(false)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_3.to_string()),
            ),
        ]),
    );
    let response = harness
        .write(vec![update_row(tombstone.clone(), resurrected)])
        .await;
    assert!(
        response.is_ok(),
        "resurrecting with the correct previous must be accepted, got {:?}",
        response.error
    );

    let found = harness.find(&["foobar"], false).await;
    assert_eq!(
        found.len(),
        1,
        "a resurrected document must be visible with with_deleted=false"
    );
    assert!(!found[0].is_deleted());

    harness.close().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:1130` — `#9145 must persist
/// all documents of parallel bulkWrite() calls with multiInstance: false`.
///
/// A read first (upstream's comment: "so that storages which cache their internal state
/// already have it initialized"), then 5 batches x 50 documents. The read-first step is
/// load-bearing for storages that lazily load their index; skipping it hides the bug.
pub async fn persists_all_parallel_bulk_writes<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;

    // Upstream's comment, verbatim in intent: run a read first.
    harness.find(&["foobar"], false).await;

    let batch_amount = 5;
    let docs_per_batch = 50;
    let mut all_ids = Vec::new();
    let mut next_lwt = 1u64;
    for batch in 0..batch_amount {
        let mut rows = Vec::new();
        for i in 0..docs_per_batch {
            let key = format!("b{batch}-d{i:03}");
            all_ids.push(key.clone());
            rows.push(insert_row(test_doc_write_data(
                &key,
                &format!("value-{key}"),
                lwt(next_lwt),
                Map::new(),
            )));
            next_lwt += 1;
        }
        harness.write_ok(rows).await;
    }

    let id_refs: Vec<&str> = all_ids.iter().map(String::as_str).collect();
    let found = harness.find(&id_refs, false).await;
    assert_eq!(
        found.len(),
        batch_amount * docs_per_batch,
        "every document written in a batch must be retrievable"
    );

    harness.close().await;
}

// ---------------------------------------------------------------------------
// .query()
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:1512` — `should find all
/// documents`.
pub async fn query_find_all<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let amount = 10;
    let mut rows = Vec::new();
    let mut expected = Vec::new();
    for i in 0..amount {
        let key = format!("key-{i:02}");
        expected.push(key.clone());
        rows.push(insert_row(test_doc_write_data(
            &key,
            "value",
            lwt(i as u64 + 1),
            Map::new(),
        )));
    }
    harness.write_ok(rows).await;

    let prepared = harness.prepare(&serde_json::json!({ "selector": {}, "sort": [{ "key": "asc" }] }));
    let found = harness.query_ids(prepared).await;
    assert_eq!(
        found, expected,
        "an empty selector must return every document in sort order"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:1578` — `should sort in the
/// correct order`.
///
/// The values are chosen so that code-unit order and the insertion order differ: a
/// storage that returns insertion order passes nothing here.
pub async fn query_sorts_correctly<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    // Inserted out of order; the sort must reorder them.
    let inserted = ["dd", "aa", "cc", "bb"];
    let mut rows = Vec::new();
    for (n, key) in inserted.iter().enumerate() {
        rows.push(insert_row(test_doc_write_data(
            key,
            "value",
            lwt(n as u64 + 1),
            Map::new(),
        )));
    }
    harness.write_ok(rows).await;

    let prepared = harness.prepare(&serde_json::json!({ "selector": {}, "sort": [{ "key": "asc" }] }));
    let found = harness.query_ids(prepared).await;
    assert_eq!(
        found,
        vec!["aa", "bb", "cc", "dd"],
        "query must return documents in sort order, not insertion order"
    );

    // And descending, which upstream notes no storage has an index for: the storage
    // must re-sort.
    let prepared = harness.prepare(&serde_json::json!({ "selector": {}, "sort": [{ "key": "desc" }] }));
    let found = harness.query_ids(prepared).await;
    assert_eq!(
        found,
        vec!["dd", "cc", "bb", "aa"],
        "a descending sort must be applied by the storage"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:1626` — `must find deleted
/// documents`.
///
/// A selector of `{_deleted: true}` must return tombstones. The `_deleted` prefix on
/// every index exists for exactly this query, and a storage that filters tombstones
/// unconditionally makes conflict resolution impossible.
pub async fn query_finds_deleted_documents<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("aaa", "live", lwt(1), Map::new());
    let doomed = test_doc_write_data("bbb", "doomed", lwt(2), Map::new());
    harness
        .write_ok(vec![insert_row(live.clone()), insert_row(doomed.clone())])
        .await;

    let tombstone = test_doc_write_data(
        "bbb",
        "doomed",
        lwt(3),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(doomed.clone(), tombstone)])
        .await;

    let prepared = harness.prepare(&serde_json::json!({
        "selector": { "_deleted": true },
        "sort": [{ "key": "asc" }],
    }));
    let found = harness.query_ids(prepared).await;
    assert_eq!(
        found,
        vec!["bbb"],
        "a selector on _deleted=true must return tombstones"
    );

    // And the same query for live documents must exclude it.
    let prepared = harness.prepare(&serde_json::json!({
        "selector": { "_deleted": false },
        "sort": [{ "key": "asc" }],
    }));
    let found = harness.query_ids(prepared).await;
    assert_eq!(
        found,
        vec!["aaa"],
        "a selector on _deleted=false must exclude tombstones"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:1693` — `should have the same
/// deterministic order of .query() and .getSortComparator()`.
///
/// The generator half of upstream's case: several documents sharing a sort value, so
/// only the primary-key tiebreaker makes the order deterministic. Upstream half-tests
/// the comparator here, which needs `dxdb-query`; the storage half does not.
pub async fn query_deterministic_order<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    // Every document shares `value`, so the sort on `value` is a tie for all of them
    // and only the appended primary key can order them.
    let mut rows = Vec::new();
    for (n, key) in ["dd", "bb", "aa", "cc"].iter().enumerate() {
        rows.push(insert_row(test_doc_write_data(
            key,
            "same",
            lwt(n as u64 + 1),
            Map::new(),
        )));
    }
    harness.write_ok(rows).await;

    let prepared = harness.prepare(&serde_json::json!({ "selector": {}, "sort": [{ "value": "asc" }] }));
    let first = harness.query_ids(prepared.clone()).await;
    assert_eq!(
        first,
        vec!["aa", "bb", "cc", "dd"],
        "ties on the sort field must be broken by the primary key"
    );

    // Running it again must produce the same order: determinism is the property.
    let second = harness.query_ids(prepared).await;
    assert_eq!(first, second, "query order must be deterministic");

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:1847` — `should return the
/// correct documents when sorting by a non-indexed field with skip and limit`.
///
/// The generator half: enough documents that skip+limit lands mid-range, sorted on a
/// field with no index, so the storage cannot satisfy the sort from an index and must
/// materialise and sort. Deterministic data, unlike upstream's random population,
/// because the expected slice has to be nameable.
pub async fn query_skip_limit_non_indexed_sort<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    // 20 documents; `value` is `v00`..`v19` and also holds the order, `key` is
    // deliberately in the opposite order so sorting by `value` is not sorting by key.
    let mut rows = Vec::new();
    for i in 0..20u64 {
        rows.push(insert_row(test_doc_write_data(
            &format!("k{:02}", 19 - i),
            &format!("v{i:02}"),
            lwt(i + 1),
            Map::new(),
        )));
    }
    harness.write_ok(rows).await;

    let prepared = harness.prepare(&serde_json::json!({
        "selector": {},
        "sort": [{ "value": "asc" }],
        "skip": 5,
        "limit": 5,
    }));
    let found = harness.query_ids(prepared).await;
    // `value` ascending is v05..v09, whose keys are k14..k10.
    assert_eq!(
        found,
        vec!["k14", "k13", "k12", "k11", "k10"],
        "skip+limit over a non-indexed sort must slice the sorted result"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2021` — `should be able to
/// search over a nested object`.
///
/// Upstream uses `getNestedDocSchema()` (index `['nes.ted', 'id']`) and queries
/// `nes.ted`. A storage that flattens nested objects or that cannot index a dot path
/// fails here.
pub async fn query_over_nested_object<S: RxStorage>(storage: &S) {
    let spec = InstanceSpec::new(fill_with_default_settings(
        crate::schemas::nested_doc_schema(),
    ));
    let harness = Harness::open(storage, &spec)
        .await
        .expect("create_storage_instance failed");

    let mut rows = Vec::new();
    for (n, (id, ted)) in [("a", "one"), ("b", "two"), ("c", "one")].iter().enumerate() {
        rows.push(insert_row(crate::fixtures::write::write_data_with(
            Map::from_iter([
                ("id".to_string(), Value::String((*id).to_string())),
                (
                    "nes".to_string(),
                    serde_json::json!({ "ted": *ted }),
                ),
            ]),
            lwt(n as u64 + 1),
            Map::new(),
        )));
    }
    harness.write_ok(rows).await;

    let prepared = harness.prepare(&serde_json::json!({
        "selector": { "nes.ted": "one" },
        "sort": [{ "id": "asc" }],
    }));
    let found = harness.query_ids(prepared).await;
    assert_eq!(
        found,
        vec!["a", "c"],
        "a selector on a nested path must match"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2077` — `querying many
/// documents should work`.
///
/// The count is `is_fast_mode()`-scaled, exactly as upstream scales it (100 vs 200).
pub async fn query_many_documents<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let amount = crate::util::fast_or(200, 100);
    let mut rows = Vec::new();
    for i in 0..amount {
        rows.push(insert_row(test_doc_write_data(
            &format!("key-{i:05}"),
            "value",
            lwt(i as u64 + 1),
            Map::new(),
        )));
    }
    harness.write_ok(rows).await;

    let prepared = harness.prepare(&serde_json::json!({ "selector": {}, "sort": [{ "key": "asc" }] }));
    let found = harness.query_ids(prepared).await;
    assert_eq!(
        found.len(),
        amount,
        "query must return every document written"
    );
    assert_eq!(found.first().map(String::as_str), Some("key-00000"));
    assert_eq!(
        found.last().map(String::as_str),
        Some(format!("key-{:05}", amount - 1).as_str())
    );

    harness.close().await;
}

// ---------------------------------------------------------------------------
// .count()
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2123` — `should count the
/// correct amount`.
///
/// Includes the two cases upstream calls out: count with a `limit` must equal the
/// length of the same query, and count must ignore tombstones.
pub async fn count_correct_amount<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let amount = 10u64;
    let mut rows = Vec::new();
    for i in 0..amount {
        rows.push(insert_row(test_doc_write_data(
            &format!("key-{i:02}"),
            "value",
            lwt(i + 1),
            Map::new(),
        )));
    }
    harness.write_ok(rows).await;

    let prepared = harness.prepare(&serde_json::json!({ "selector": {}, "sort": [{ "key": "asc" }] }));
    let count = harness.count(prepared.clone()).await;
    assert_eq!(
        count.count as u64, amount,
        "count must equal the number of live documents"
    );

    // With a limit, count must agree with query().len() rather than with the total.
    let limited = harness.prepare(
        &serde_json::json!({ "selector": {}, "sort": [{ "key": "asc" }], "limit": 3 }),
    );
    let count = harness.count(limited.clone()).await;
    let query = harness.query(limited).await;
    assert_eq!(
        count.count, 3,
        "count with a limit must be the limited count"
    );
    assert_eq!(
        count.count,
        query.len(),
        "count() must equal query().len() for the same prepared query"
    );

    // A tombstone is still counted, and this is upstream's assertion, not a slip:
    // `rx-storage-implementations.test.ts:2172` says verbatim
    //   "must still be 2 because the storage itself also counts deleted docs".
    //
    // A bare storage does not filter `_deleted`; that filtering is the caller's job and
    // the query-correctness suite performs it by injecting `_deleted: false` into the
    // selector (`:212`). Asserting the opposite here would have made the testkit demand
    // behaviour upstream explicitly rejects, and any storage that satisfied it would fail
    // upstream's own suite.
    let found = harness.find(&["key-00"], false).await;
    let previous = found.into_iter().next().expect("key-00 exists");
    let tombstone = test_doc_write_data(
        "key-00",
        "value",
        lwt(101),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(previous.into_value(), tombstone)])
        .await;

    let prepared = harness.prepare(&serde_json::json!({ "selector": {}, "sort": [{ "key": "asc" }] }));
    let count = harness.count(prepared).await;
    assert_eq!(
        count.count as u64, amount,
        "count must still include the deleted document: upstream says \"the storage \
         itself also counts deleted docs\" (rx-storage-implementations.test.ts:2172)"
    );

    // And the caller-side filter must exclude it, which is what the correctness suite
    // relies on. Asserting both halves pins the division of responsibility.
    let filtered = harness.prepare(&serde_json::json!({
        "selector": { "_deleted": false },
        "sort": [{ "key": "asc" }],
    }));
    let count = harness.count(filtered).await;
    assert_eq!(
        count.count as u64,
        amount - 1,
        "with `_deleted: false` injected by the caller, a tombstone must be excluded"
    );

    harness.remove().await;
}

// ---------------------------------------------------------------------------
// .findDocumentsById()
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2184` — `should find the
/// documents`.
pub async fn find_documents_by_id<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    harness
        .write_ok(vec![
            insert_row(test_doc_write_data("aa", "a", lwt(1), Map::new())),
            insert_row(test_doc_write_data("bb", "b", lwt(2), Map::new())),
        ])
        .await;

    let found = harness.find(&["aa", "bb"], false).await;
    assert_eq!(found.len(), 2, "both documents must be found");
    let mut names = ids(&found, &harness.primary_path);
    names.sort();
    assert_eq!(names, vec!["aa", "bb"]);

    // A missing id is simply absent, not an error.
    let found = harness.find(&["aa", "does-not-exist"], false).await;
    assert_eq!(
        found.len(),
        1,
        "an unknown id must be omitted rather than fail the call"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2221` — `should find the
/// documents when the primary key is _id`.
///
/// A primary key named `_id` is the single most likely thing for a storage to
/// hard-code wrongly, because it looks like a meta field. It is not one.
pub async fn find_documents_primary_key_underscore_id<S: RxStorage>(storage: &S) {
    let schema = fill_with_default_settings(serde_json::json!({
        "version": 0,
        "primaryKey": "_id",
        "type": "object",
        "properties": {
            "_id": { "type": "string", "maxLength": 20 },
            "name": { "type": "string", "maxLength": 20 }
        },
        "required": ["_id", "name"],
        "indexes": ["name"]
    }));
    let spec = InstanceSpec::new(schema);
    let harness = Harness::open(storage, &spec)
        .await
        .expect("create_storage_instance failed");

    harness
        .write_ok(vec![
            insert_row(crate::fixtures::write::write_data_with(
                Map::from_iter([
                    ("_id".to_string(), Value::String("aa".to_string())),
                    ("name".to_string(), Value::String("alice".to_string())),
                ]),
                lwt(1),
                Map::new(),
            )),
            insert_row(crate::fixtures::write::write_data_with(
                Map::from_iter([
                    ("_id".to_string(), Value::String("bb".to_string())),
                    ("name".to_string(), Value::String("bob".to_string())),
                ]),
                lwt(2),
                Map::new(),
            )),
        ])
        .await;

    let found = harness.find(&["aa", "bb"], false).await;
    assert_eq!(found.len(), 2, "a primary key named _id must work");
    assert_eq!(harness.primary_path, "_id");

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2292` — `should find deleted
/// documents`.
pub async fn find_deleted_documents<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("aa", "a", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(live.clone())]).await;
    let tombstone = test_doc_write_data(
        "aa",
        "a",
        lwt(2),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(live.clone(), tombstone)])
        .await;

    let with = harness.find(&["aa"], true).await;
    assert_eq!(with.len(), 1, "with_deleted=true must find a tombstone");
    assert!(with[0].is_deleted());

    let without = harness.find(&["aa"], false).await;
    assert!(
        without.is_empty(),
        "with_deleted=false must not find a tombstone"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2346` — `if withDeleted=true
/// then even the non-deleted document must be found`.
///
/// The asymmetry that a naive implementation gets wrong: `with_deleted=true` must not
/// *filter to* tombstones, it must stop filtering them out.
pub async fn with_deleted_finds_live_documents<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("aa", "a", lwt(1), Map::new());
    let doomed = test_doc_write_data("bb", "b", lwt(2), Map::new());
    harness
        .write_ok(vec![insert_row(live.clone()), insert_row(doomed.clone())])
        .await;

    let tombstone = test_doc_write_data(
        "bb",
        "b",
        lwt(3),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(doomed.clone(), tombstone)])
        .await;

    let all = harness.find(&["aa", "bb"], true).await;
    assert_eq!(
        all.len(),
        2,
        "with_deleted=true must return live documents as well as tombstones"
    );
    let live_count = all.iter().filter(|document| !document.is_deleted()).count();
    assert_eq!(live_count, 1, "exactly one of the two is live");

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2430` — `should be able to
/// insert and fetch many documents`.
pub async fn find_many_documents<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let amount = crate::util::fast_or(200, 100);
    let mut rows = Vec::new();
    let mut all_ids = Vec::new();
    for i in 0..amount {
        let key = format!("key-{i:05}");
        all_ids.push(key.clone());
        rows.push(insert_row(test_doc_write_data(&key, "v", lwt(i as u64 + 1), Map::new())));
    }
    harness.write_ok(rows).await;

    let id_refs: Vec<&str> = all_ids.iter().map(String::as_str).collect();
    let found = harness.find(&id_refs, false).await;
    assert_eq!(found.len(), amount, "findByIds over {amount} ids must return all");

    harness.remove().await;
}

// ---------------------------------------------------------------------------
// .getChangedDocumentsSince()
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2462` — `should get the
/// latest change`.
pub async fn changed_documents_since_latest<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;

    // From the beginning, one document per call.
    for n in 1..=3u64 {
        let key = format!("k{n}");
        harness
            .write_ok(vec![insert_row(test_doc_write_data(&key, "v", lwt(n), Map::new()))])
            .await;
    }

    let changed = harness
        .instance
        .get_changed_documents_since(1, None)
        .await
        .expect("get_changed_documents_since failed");
    assert_eq!(changed.documents.len(), 1, "limit 1 must return one document");
    assert_eq!(
        changed.documents[0].get("key").and_then(Value::as_str),
        Some("k1"),
        "the first change is the lowest lwt"
    );
    assert_eq!(changed.checkpoint.id, "k1");
    assert_eq!(changed.checkpoint.lwt, lwt(1));

    // Resuming from that checkpoint gives the next one, not the same one.
    let next = harness
        .instance
        .get_changed_documents_since(1, Some(changed.checkpoint.clone()))
        .await
        .expect("get_changed_documents_since failed");
    assert_eq!(
        next.documents[0].get("key").and_then(Value::as_str),
        Some("k2"),
        "the checkpoint must be exclusive"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2546` — `should return the
/// correct amount of documents`.
///
/// Walking the whole feed in pages must yield every document exactly once.
pub async fn changed_documents_since_amount<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let amount = 10u64;
    let mut rows = Vec::new();
    for i in 0..amount {
        rows.push(insert_row(test_doc_write_data(
            &format!("k{i:02}"),
            "v",
            lwt(i + 1),
            Map::new(),
        )));
    }
    harness.write_ok(rows).await;

    let mut checkpoint: Option<Checkpoint> = None;
    let mut seen = Vec::new();
    loop {
        let changed = harness
            .instance
            .get_changed_documents_since(3, checkpoint.clone())
            .await
            .expect("get_changed_documents_since failed");
        if changed.documents.is_empty() {
            break;
        }
        for document in &changed.documents {
            seen.push(
                document
                    .get("key")
                    .and_then(Value::as_str)
                    .expect("key")
                    .to_string(),
            );
        }
        checkpoint = Some(changed.checkpoint);
        if seen.len() as u64 > amount {
            panic!("the change feed returned more documents than were written");
        }
    }

    assert_eq!(
        seen.len() as u64,
        amount,
        "walking the feed must return every document exactly once, got {seen:?}"
    );
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), amount as usize, "no document may be returned twice");

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2642` — `should be able to
/// correctly iterate over the checkpoints`.
///
/// Interleaves writes with checkpoint walks, which is the real use: replication polls
/// while writes continue.
pub async fn changed_documents_since_iterate_checkpoints<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let mut checkpoint: Option<Checkpoint> = None;

    for n in 1..=5u64 {
        let key = format!("k{n}");
        harness
            .write_ok(vec![insert_row(test_doc_write_data(&key, "v", lwt(n), Map::new()))])
            .await;
        let changed = harness
            .instance
            .get_changed_documents_since(10, checkpoint.clone())
            .await
            .expect("get_changed_documents_since failed");
        assert_eq!(
            changed.documents.len(),
            1,
            "each poll must see exactly the one new document"
        );
        assert_eq!(
            changed.documents[0].get("key").and_then(Value::as_str),
            Some(key.as_str())
        );
        checkpoint = Some(changed.checkpoint);
    }

    harness.remove().await;
}

// ---------------------------------------------------------------------------
// .changeStream()
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2696` — `should emit exactly
/// one event on write`.
pub async fn change_stream_one_event_per_write<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let mut stream = harness.instance.as_ref().change_stream();
    let mut stream = Box::pin(&mut stream);

    harness
        .write_ok(vec![insert_row(test_doc_write_data("aa", "a", lwt(1), Map::new()))])
        .await;

    let bulks = crate::harness::collect_until(&mut stream, 8, |collected| {
        !crate::harness::flatten_events(collected).is_empty()
    })
    .await
    .expect("the write must emit an event");

    let events = crate::harness::flatten_events(&bulks);
    assert_eq!(events.len(), 1, "one write must emit exactly one event");
    assert_eq!(events[0].document_id, "aa");
    assert_eq!(events[0].operation, Operation::Insert);

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2791` — `should emit all
/// events`.
///
/// Two writes: the operations must be INSERT then INSERT, with the second document's
/// revision equal to what was written.
pub async fn change_stream_all_events<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let mut stream = harness.instance.as_ref().change_stream();
    let mut stream = Box::pin(&mut stream);

    harness
        .write_ok(vec![insert_row(test_doc_write_data("aa", "a", lwt(1), Map::new()))])
        .await;
    harness
        .write_ok(vec![insert_row(test_doc_write_data("bb", "b", lwt(2), Map::new()))])
        .await;

    let bulks = crate::harness::collect_until(&mut stream, 8, |collected| {
        crate::harness::flatten_events(collected).len() >= 2
    })
    .await
    .expect("both writes must emit events");

    let events = crate::harness::flatten_events(&bulks);
    assert_eq!(events.len(), 2, "two writes must emit two events");
    assert_eq!(
        events
            .iter()
            .map(|event| event.document_id.as_str())
            .collect::<Vec<_>>(),
        vec!["aa", "bb"],
        "events must be in write order"
    );
    assert!(events.iter().all(|event| event.operation == Operation::Insert));

    // The second write's revision must be the one written, through the event.
    assert_eq!(events[1].document_data.rev(), Some(EXAMPLE_REVISION_1));

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2881` — `it should not emit
/// an empty eventBulk when the write had only errors`.
///
/// A conflicting write must emit *nothing*. A storage that emits an empty bulk makes
/// every consumer do a no-op wakeup and, worse, advance a checkpoint with no progress.
pub async fn change_stream_no_empty_bulk<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    harness
        .write_ok(vec![insert_row(test_doc_write_data("aa", "a", lwt(1), Map::new()))])
        .await;

    let mut stream = harness.instance.as_ref().change_stream();
    let mut stream = Box::pin(&mut stream);

    // A conflicting write: only errors.
    let response = harness
        .write(vec![insert_row(test_doc_write_data("aa", "b", lwt(2), Map::new()))])
        .await;
    assert_eq!(response.error.len(), 1);

    // A subsequent successful write must produce the *first* event on the stream: if
    // the failed write emitted an empty bulk, it would arrive first.
    harness
        .write_ok(vec![insert_row(test_doc_write_data("bb", "b", lwt(3), Map::new()))])
        .await;

    let bulks = crate::harness::collect_until(&mut stream, 8, |collected| {
        !crate::harness::flatten_events(collected).is_empty()
    })
    .await
    .expect("the successful write must emit an event");

    let events = crate::harness::flatten_events(&bulks);
    assert!(
        !events.is_empty(),
        "a successful write must emit at least one event"
    );
    assert_eq!(
        events[0].document_id, "bb",
        "the first event must be the successful write, so the failed write emitted nothing"
    );
    assert!(
        bulks.iter().all(|bulk| !bulk.is_empty()),
        "no bulk on the stream may be empty"
    );

    harness.remove().await;
}

// ---------------------------------------------------------------------------
// attachments
// ---------------------------------------------------------------------------

/// A SHA-256 hex digest of `bytes`.
///
/// Reimplemented here rather than taking a dependency: the testkit's Cargo.toml is
/// deliberately one workspace dependency, and the algorithm is 25 lines. Checked
/// against the published NIST vectors in this crate's unit tests, because a wrong
/// digest would make every attachment case fail for the wrong reason.
pub fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        for byte in word.to_be_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

/// Builds a spec whose schema declares `attachments`, which is what makes the `510`
/// write error and the `_attachments` payload path reachable.
async fn open_attachment_instance<S: RxStorage>(storage: &S) -> Harness {
    let mut schema = fill_with_default_settings(crate::schemas::pseudo_schema_for_version(0, "key"));
    // Upstream's `createAttachments` does `const schemaJson = clone(schemas.human);
    // schemaJson.attachments = {};` — an empty object is the declaration.
    if let Some(object) = schema.as_object_mut() {
        object.insert("attachments".to_string(), Value::Object(Map::new()));
    }
    let spec = InstanceSpec::new(schema);
    Harness::open(storage, &spec)
        .await
        .expect("create_storage_instance failed")
}

/// Builds an attachment-bearing write document with one attachment.
///
/// Uses [`dxdb_storage::DocumentWriteData::insert_attachment`] rather than hand-building
/// the stub map, so the stub shape comes from the storage crate instead of from this
/// testkit. A testkit that hand-rolled the stub could pass against a storage by agreeing
/// with its own mistake.
fn attachment_write(
    document: Value,
    attachment_id: &str,
    content_type: &str,
    bytes: &[u8],
) -> (dxdb_storage::DocumentWriteData, String) {
    let digest = sha256_hex(bytes);
    let mut write = crate::fixtures::write::write(document.clone());
    write.insert_attachment(
        attachment_id,
        dxdb_storage::AttachmentWriteData {
            stub: dxdb_storage::AttachmentData {
                length: bytes.len() as u64,
                content_type: content_type.to_string(),
                digest: digest.clone(),
            },
            data: Some(bytes.to_vec()),
        },
    );
    (write, digest)
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2938` — `should be able to
/// store and retrieve an attachment`.
pub async fn store_and_retrieve_attachment<S: RxStorage>(storage: &S) {
    let harness = open_attachment_instance(storage).await;
    let bytes = b"attachment payload";
    let document = crate::fixtures::write::write_data_with(
        Map::from_iter([
            ("key".to_string(), Value::String("aa".to_string())),
            ("value".to_string(), Value::String("a".to_string())),
        ]),
        lwt(1),
        Map::new(),
    );
    let (write, digest) = attachment_write(document, "att-1", "text/plain", bytes);
    harness
        .write_ok(vec![BulkWriteRow::insert(write)])
        .await;

    let read = harness
        .instance
        .get_attachment_data("aa".to_string(), "att-1".to_string(), digest.clone())
        .await
        .expect("get_attachment_data failed");
    assert_eq!(
        read,
        bytes.to_vec(),
        "the attachment bytes must round-trip"
    );

    // A wrong digest must be an error, not a mismatch to reconcile.
    let wrong = harness
        .instance
        .get_attachment_data("aa".to_string(), "att-1".to_string(), "0".repeat(64))
        .await;
    assert!(
        wrong.is_err(),
        "a wrong digest must be an error, got {:?}",
        wrong.ok()
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:2995` — `should return the
/// correct attachment object on all document fetch methods`.
pub async fn attachment_stub_on_all_fetch_methods<S: RxStorage>(storage: &S) {
    let harness = open_attachment_instance(storage).await;
    let bytes = b"stub payload";
    let document = crate::fixtures::write::write_data_with(
        Map::from_iter([
            ("key".to_string(), Value::String("aa".to_string())),
            ("value".to_string(), Value::String("a".to_string())),
        ]),
        lwt(1),
        Map::new(),
    );
    let (write, digest) = attachment_write(document, "att-1", "text/plain", bytes);

    let mut stream = harness.instance.as_ref().change_stream();
    let mut stream = Box::pin(&mut stream);
    harness.write_ok(vec![BulkWriteRow::insert(write)]).await;

    let assert_stub = |document: &DocumentData, path: &str| {
        let attachments = document
            .attachments()
            .unwrap_or_else(|| panic!("{path}: no _attachments on the document"));
        let entry = attachments
            .get("att-1")
            .unwrap_or_else(|| panic!("{path}: att-1 missing from _attachments"));
        assert_eq!(
            entry.get("digest").and_then(Value::as_str),
            Some(digest.as_str()),
            "{path}: the stub must carry the digest"
        );
        assert_eq!(
            entry.get("type").and_then(Value::as_str),
            Some("text/plain"),
            "{path}: the stub must carry the content type"
        );
        assert_eq!(
            entry.get("length").and_then(Value::as_u64),
            Some(bytes.len() as u64),
            "{path}: the stub must carry the length"
        );
    };

    let found = harness.find(&["aa"], false).await;
    assert_eq!(found.len(), 1);
    assert_stub(&found[0], "findDocumentsById");

    let prepared = harness.prepare(&serde_json::json!({ "selector": {}, "sort": [{ "key": "asc" }] }));
    let queried = harness.query(prepared).await;
    assert_eq!(queried.len(), 1);
    assert_stub(&queried[0], "query");

    let changed = harness
        .instance
        .get_changed_documents_since(10, None)
        .await
        .expect("get_changed_documents_since failed");
    assert_eq!(changed.documents.len(), 1);
    assert_stub(&changed.documents[0], "getChangedDocumentsSince");

    let bulks = crate::harness::collect_until(&mut stream, 8, |collected| {
        !crate::harness::flatten_events(collected).is_empty()
    })
    .await
    .expect("the write must emit an event");
    let events = crate::harness::flatten_events(&bulks);
    assert_stub(&events[0].document_data, "changeStream");

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3109` — `should be able to add
/// multiple attachments, one each write`.
pub async fn add_multiple_attachments<S: RxStorage>(storage: &S) {
    let harness = open_attachment_instance(storage).await;
    let first_bytes = b"first";
    let document = crate::fixtures::write::write_data_with(
        Map::from_iter([
            ("key".to_string(), Value::String("aa".to_string())),
            ("value".to_string(), Value::String("a".to_string())),
        ]),
        lwt(1),
        Map::new(),
    );
    let (first_write, first_digest) = attachment_write(document, "att-1", "text/plain", first_bytes);
    harness
        .write_ok(vec![BulkWriteRow::insert(first_write)])
        .await;

    let previous = harness
        .find(&["aa"], false)
        .await
        .into_iter()
        .next()
        .expect("aa exists");

    // The second write carries the *existing* stub for att-1 (no payload) and a new
    // stub plus payload for att-2. That is exactly the "send the stub again" case
    // `AttachmentWriteData::data: None` exists for.
    let second_bytes = b"second";
    let second_digest = sha256_hex(second_bytes);
    let mut second_write = dxdb_storage::DocumentWriteData::new(previous.clone());
    let stored_stub = previous
        .attachment("att-1")
        .cloned()
        .expect("the first write must have stored the att-1 stub");
    second_write.document_mut().set(
        "_rev",
        Value::String(EXAMPLE_REVISION_2.to_string()),
    );
    second_write.document_mut().set_lwt(lwt(2)).expect("lwt");
    // att-1: same stub, no payload.
    let mut attachments = Map::new();
    attachments.insert("att-1".to_string(), stored_stub);
    attachments.insert(
        "att-2".to_string(),
        dxdb_storage::AttachmentData {
            length: second_bytes.len() as u64,
            content_type: "text/plain".to_string(),
            digest: second_digest.clone(),
        }
        .to_value(),
    );
    second_write
        .document_mut()
        .set("_attachments", Value::Object(attachments));
    second_write.insert_attachment(
        "att-2",
        dxdb_storage::AttachmentWriteData {
            stub: dxdb_storage::AttachmentData {
                length: second_bytes.len() as u64,
                content_type: "text/plain".to_string(),
                digest: second_digest.clone(),
            },
            data: Some(second_bytes.to_vec()),
        },
    );

    harness
        .write_ok(vec![BulkWriteRow::update(previous, second_write)])
        .await;

    let found = harness.find(&["aa"], false).await;
    let attachments = found[0].attachments().expect("_attachments");
    assert_eq!(
        attachments.len(),
        2,
        "both attachments must be present, got {:?}",
        attachments.keys().collect::<Vec<_>>()
    );

    let read_first = harness
        .instance
        .get_attachment_data("aa".to_string(), "att-1".to_string(), first_digest)
        .await
        .expect("att-1 must still be readable");
    assert_eq!(
        read_first,
        first_bytes.to_vec(),
        "an unchanged attachment's payload must survive a later write"
    );
    let read_second = harness
        .instance
        .get_attachment_data("aa".to_string(), "att-2".to_string(), second_digest)
        .await
        .expect("att-2 must be readable");
    assert_eq!(read_second, second_bytes.to_vec());

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3193` — `_deleted documents
/// must loose all attachments`.
pub async fn deleted_document_loses_attachments<S: RxStorage>(storage: &S) {
    let harness = open_attachment_instance(storage).await;
    let bytes = b"to be deleted";
    let document = crate::fixtures::write::write_data_with(
        Map::from_iter([
            ("key".to_string(), Value::String("aa".to_string())),
            ("value".to_string(), Value::String("a".to_string())),
        ]),
        lwt(1),
        Map::new(),
    );
    let (write, digest) = attachment_write(document, "att-1", "text/plain", bytes);
    harness
        .write_ok(vec![BulkWriteRow::insert(write)])
        .await;

    let previous = harness
        .find(&["aa"], false)
        .await
        .into_iter()
        .next()
        .expect("aa exists");

    // The tombstone carries no attachments at all.
    let tombstone = crate::fixtures::write::write_data_with(
        Map::from_iter([
            ("key".to_string(), Value::String("aa".to_string())),
            ("value".to_string(), Value::String("a".to_string())),
            ("_attachments".to_string(), Value::Object(Map::new())),
        ]),
        lwt(2),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![BulkWriteRow::update(
            previous,
            crate::fixtures::write::write(tombstone),
        )])
        .await;

    let read = harness
        .instance
        .get_attachment_data("aa".to_string(), "att-1".to_string(), digest)
        .await;
    assert!(
        read.is_err(),
        "a deleted document's attachment must not be readable, got {:?}",
        read.ok()
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3267` — `must be able to load
/// multiple attachments data in parallel`.
///
/// Ten attachments on one document, each read back. Upstream runs the reads in parallel;
/// here they are sequential because the sequential executor (D3) has no other ordering,
/// and the invariant under test — that each digest resolves to its own bytes — does not
/// depend on concurrency. The comment records the shape change.
pub async fn load_multiple_attachments<S: RxStorage>(storage: &S) {
    let harness = open_attachment_instance(storage).await;

    let mut write = crate::fixtures::write::write(crate::fixtures::write::write_data_with(
        Map::from_iter([
            ("key".to_string(), Value::String("aa".to_string())),
            ("value".to_string(), Value::String("a".to_string())),
        ]),
        lwt(1),
        Map::new(),
    ));
    let mut expected = Vec::new();
    for i in 0..10u32 {
        let bytes = format!("payload-{i}").into_bytes();
        let digest = sha256_hex(&bytes);
        let id = format!("att-{i}");
        write.insert_attachment(
            &id,
            dxdb_storage::AttachmentWriteData {
                stub: dxdb_storage::AttachmentData {
                    length: bytes.len() as u64,
                    content_type: "text/plain".to_string(),
                    digest: digest.clone(),
                },
                data: Some(bytes.clone()),
            },
        );
        expected.push((id, digest, bytes));
    }
    harness
        .write_ok(vec![BulkWriteRow::insert(write)])
        .await;

    for (id, digest, bytes) in expected {
        let read = harness
            .instance
            .get_attachment_data("aa".to_string(), id.clone(), digest)
            .await
            .unwrap_or_else(|err| panic!("{id} must be readable: {err}"));
        assert_eq!(read, bytes, "{id} must return its own bytes");
    }

    harness.remove().await;
}

// ---------------------------------------------------------------------------
// .cleanup
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3369` — `should have cleaned
/// up the deleted document`.
pub async fn cleanup_removes_deleted_document<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("aa", "a", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(live.clone())]).await;
    let tombstone = test_doc_write_data(
        "aa",
        "a",
        lwt(2),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(live.clone(), tombstone)])
        .await;

    // `cleanup(0)` removes every tombstone: the cutoff is `now() - 0`, and every stored
    // `lwt` is at or before now. Upstream calls exactly this
    // (`rx-storage-implementations.test.ts:3451`) and loops while it returns `false`.
    //
    // The parameter is a **duration in milliseconds**, not an absolute deadline. An
    // earlier version of this case passed `f64::MAX`, which as a duration is a cutoff
    // infinitely far in the past — so nothing matched and a correct storage looked broken.
    let mut attempts = 0;
    loop {
        attempts += 1;
        if harness
            .instance
            .cleanup(0.0)
            .await
            .expect("cleanup failed")
        {
            break;
        }
        assert!(
            attempts < 100,
            "cleanup(0) returned false 100 times; it must report completion eventually"
        );
    }

    let found = harness.find(&["aa"], true).await;
    assert!(
        found.is_empty(),
        "a cleaned tombstone must be gone even with with_deleted=true"
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3470` — `should clean up all
/// deleted documents when multiple are deleted`.
pub async fn cleanup_removes_all_deleted<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let mut rows = Vec::new();
    let mut tombstones = Vec::new();
    for n in 0..5u64 {
        let key = format!("k{n}");
        let live = test_doc_write_data(&key, "v", lwt(n * 2 + 1), Map::new());
        rows.push(insert_row(live));
    }
    harness.write_ok(rows).await;

    for n in 0..5u64 {
        let key = format!("k{n}");
        let previous = test_doc_write_data(&key, "v", lwt(n * 2 + 1), Map::new());
        let tombstone = test_doc_write_data(
            &key,
            "v",
            lwt(n * 2 + 2),
            Map::from_iter([
                ("_deleted".to_string(), Value::Bool(true)),
                (
                    "_rev".to_string(),
                    Value::String(EXAMPLE_REVISION_2.to_string()),
                ),
            ]),
        );
        tombstones.push(update_row(previous, tombstone));
    }
    harness.write_ok(tombstones).await;

    // `cleanup(0)` is upstream's call (`:3533`); the loop is the contract.
    let mut attempts = 0;
    loop {
        attempts += 1;
        if harness
            .instance
            .cleanup(0.0)
            .await
            .expect("cleanup failed")
        {
            break;
        }
        assert!(attempts < 100, "cleanup(0) must report completion eventually");
    }

    let found = harness.find(&["k0", "k1", "k2", "k3", "k4"], true).await;
    assert!(
        found.is_empty(),
        "every tombstone must be cleaned, {} remained",
        found.len()
    );

    harness.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3550` — `should at some time
/// return true (when all docs are cleaned up)`.
///
/// The contract that makes `cleanup -> bool` meaningful: `false` means "more work
/// remains, call me again", so a caller loops until `true`. A storage that purges in
/// batches and returns `true` early would strand tombstones forever.
///
/// Upstream asserts a document that is *not* old enough is kept, then makes it old
/// enough and asserts it goes. Here the threshold moves rather than the clock, which
/// asserts the same thing without sleeping.
pub async fn cleanup_returns_false_while_work_remains<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    let live = test_doc_write_data("aa", "a", lwt(1), Map::new());
    harness.write_ok(vec![insert_row(live.clone())]).await;
    let tombstone = test_doc_write_data(
        "aa",
        "a",
        lwt(2),
        Map::from_iter([
            ("_deleted".to_string(), Value::Bool(true)),
            (
                "_rev".to_string(),
                Value::String(EXAMPLE_REVISION_2.to_string()),
            ),
        ]),
    );
    harness
        .write_ok(vec![update_row(live.clone(), tombstone)])
        .await;

    // A six-minute **duration** must not remove a tombstone written just now. That is
    // upstream's first assertion in this case (`:3441`, `cleanup(1000 * 60 * 6)`), and it
    // is the half a storage that ignores its argument gets wrong.
    let mut attempts = 0;
    loop {
        attempts += 1;
        if harness
            .instance
            .cleanup(1000.0 * 60.0 * 6.0)
            .await
            .expect("cleanup failed")
        {
            break;
        }
        assert!(attempts < 100, "cleanup must report completion eventually");
    }
    let found = harness.find(&["aa"], true).await;
    assert_eq!(
        found.len(),
        1,
        "a tombstone newer than the six-minute window must be kept"
    );

    // Then `cleanup(0)`, which removes it. The difference between the two calls is the
    // whole assertion.
    let mut attempts = 0;
    loop {
        attempts += 1;
        if harness
            .instance
            .cleanup(0.0)
            .await
            .expect("cleanup failed")
        {
            break;
        }
        assert!(
            attempts < 100,
            "cleanup(0) returned false 100 times; it must report completion eventually"
        );
    }
    let found = harness.find(&["aa"], true).await;
    assert!(
        found.is_empty(),
        "after cleanup reported completion the tombstone must be gone"
    );

    harness.remove().await;
}

// ---------------------------------------------------------------------------
// .close() / .remove()
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3613` — `closing multiple
/// times should not error`.
pub async fn close_multiple_times<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    harness
        .instance
        .close()
        .await
        .expect("the first close must succeed");
    harness
        .instance
        .close()
        .await
        .expect("closing twice must not error");
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3633` — `should have deleted
/// all data`.
pub async fn remove_deletes_all_data<S: RxStorage>(storage: &S) {
    let (harness, spec) = open_test_doc(storage).await;
    harness
        .write_ok(vec![insert_row(test_doc_write_data("aa", "a", lwt(1), Map::new()))])
        .await;

    let database_name = spec.database_name.clone();
    let collection_name = spec.collection_name.clone();
    harness
        .instance
        .remove()
        .await
        .expect("remove() must succeed");

    // Re-opening the same database/collection must find nothing: `remove` deletes the
    // data, not just the handle.
    let reopened_spec = InstanceSpec::named(
        fill_with_default_settings(crate::schemas::pseudo_schema_for_version(0, "key")),
        &database_name,
        &collection_name,
    );
    let reopened = Harness::open(storage, &reopened_spec)
        .await
        .expect("re-opening after remove must succeed");
    let found = reopened.find(&["aa"], true).await;
    assert!(
        found.is_empty(),
        "remove() must delete the stored documents, found {}",
        found.len()
    );

    reopened.remove().await;
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3676` — `should throw on call
/// to .remove() after .close()`.
pub async fn remove_after_close_throws<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    harness.instance.close().await.expect("close must succeed");
    let result = harness.instance.remove().await;
    assert!(
        result.is_err(),
        "remove() after close() must be an error, got {result:?}"
    );
}

/// Upstream: `test/unit/rx-storage-implementations.test.ts:3693` — `should NOT throw on
/// call to .close() after .remove()`.
///
/// The asymmetry with the previous case is the contract: `remove` implies closed, so a
/// later `close` is a no-op, but `close` does not imply removed.
pub async fn close_after_remove_does_not_throw<S: RxStorage>(storage: &S) {
    let (harness, _spec) = open_test_doc(storage).await;
    harness.instance.remove().await.expect("remove must succeed");
    let result = harness.instance.close().await;
    assert!(
        result.is_ok(),
        "close() after remove() must not error, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// migration
// ---------------------------------------------------------------------------

/// Upstream: `test/unit/rx-storage-implementations.test.ts:4046` — `documents that are
/// stored on different schema versions, should not interfere`.
///
/// Two pseudo-schema versions of one collection, each with its own document. Reading
/// one must not see the other. This is the case that catches a storage whose collection
/// key omits the schema version.
pub async fn schema_versions_do_not_interfere<S: RxStorage>(storage: &S) {
    let database_name = crate::util::random_token(12);
    let collection_name = crate::util::random_token(12);

    let zero_spec = InstanceSpec::named(
        fill_with_default_settings(crate::schemas::pseudo_schema_for_version(0, "key")),
        &database_name,
        &collection_name,
    );
    let one_spec = InstanceSpec::named(
        fill_with_default_settings(crate::schemas::pseudo_schema_for_version(1, "key")),
        &database_name,
        &collection_name,
    );
    let zero = Harness::open(storage, &zero_spec)
        .await
        .expect("version 0 instance");
    let one = Harness::open(storage, &one_spec)
        .await
        .expect("version 1 instance");

    zero.write_ok(vec![insert_row(test_doc_write_data(
        "key",
        "zero",
        lwt(1),
        Map::new(),
    ))])
    .await;
    one.write_ok(vec![insert_row(test_doc_write_data(
        "key",
        "one",
        lwt(2),
        Map::new(),
    ))])
    .await;

    let zero_prepared = zero.prepare(&serde_json::json!({"selector": {}, "sort": [{"key": "asc"}]}));
    let zero_docs = zero.query(zero_prepared).await;
    assert_eq!(
        zero_docs.len(),
        1,
        "version 0 must see exactly its own document"
    );
    assert_eq!(
        zero_docs[0].get("value").and_then(Value::as_str),
        Some("zero")
    );

    let one_prepared = one.prepare(&serde_json::json!({"selector": {}, "sort": [{"key": "asc"}]}));
    let one_docs = one.query(one_prepared).await;
    assert_eq!(
        one_docs.len(),
        1,
        "version 1 must see exactly its own document"
    );
    assert_eq!(
        one_docs[0].get("value").and_then(Value::as_str),
        Some("one")
    );

    zero.remove().await;
    one.remove().await;
}

// ---------------------------------------------------------------------------
// The registry: what a gate test iterates
// ---------------------------------------------------------------------------

/// One executable case: its declaration id, and the function that runs it
/// generically over a storage.
///
/// A registry exists so the gate test can iterate the *table* rather than a
/// hand-maintained list, and so a case that is declared
/// [`crate::decl::PortStatus::Exercised`] but has no entry here is a failure the
/// testkit's own test can see.
///
/// The entry point is generic rather than a function pointer because a Rust
/// function pointer cannot be generic over `S: RxStorage`, and erasing the storage
/// to the dyn mirror would mean asserting against `DynRxStorageInstance` everywhere
/// instead of the trait — which is precisely the indirection a conformance suite
/// should not add to the thing it is measuring. So the registry lists the cases,
/// and [`run_case`] dispatches by id.
pub struct CaseEntry {
    /// The [`crate::decl::Case::id`] this runs.
    pub id: &'static str,
    /// The upstream title, for a failure message that reads like upstream's.
    pub title: &'static str,
}

/// The upstream line of the case [`run_exercised_case`] dispatches.
///
/// Keyed on the **upstream line**, not on the declared id, and that is deliberate: the
/// id is `impl-<kind>-<n>` where `n` counts within a kind, so reclassifying one case
/// renumbers every later case of that kind. A dispatch table keyed on ids would then
/// silently point at the wrong body — which is exactly the bug this shape prevents, and
/// which it caught once already. A line number is stable because the case it names is
/// the case at that line.
pub struct ExercisedCase {
    /// Upstream line of the `it(...)`.
    pub upstream_line: u32,
    /// The declared id at generation time, for a readable failure message.
    pub id: &'static str,
}

/// Runs the exercised case at `upstream_line` against `storage`.
///
/// Panics on a line with no body: the line comes from the declaration table, so a
/// mismatch means a case is declared Exercised without an implementation, which is the
/// failure the testkit's own `exercised_rows_have_bodies_and_bodies_have_rows` test
/// exists to catch.
pub async fn run_exercised_case<S: RxStorage>(storage: &S, upstream_line: u32) {
    match upstream_line {
        176 => open_and_close(storage).await, // impl-bulkwrite-01 BulkWrite
        195 => open_many_instances_same_database(storage).await, // impl-bulkwrite-02 BulkWrite
        270 => should_write_the_document(storage).await, // impl-insert-01 Insert
        309 => should_error_on_conflict(storage).await, // impl-conflict-01 Conflict
        372 => concurrent_insert_conflicts(storage).await, // impl-conflict-02 Conflict
        420 => deleted_not_found_without_with_deleted(storage).await, // impl-findbyids-01 FindByIds
        483 => cannot_overwrite_deleted_without_previous(storage).await, // impl-conflict-03 Conflict
        600 => update_deleted_document_state(storage).await, // impl-revision-01 Revision
        661 => unset_property(storage).await, // impl-bulkwrite-05 BulkWrite
        773 => write_only_meta_fields_changed(storage).await, // impl-revision-02 Revision
        848 => create_second_instance_after_write(storage).await, // impl-bulkwrite-07 BulkWrite
        900 => revision_height_may_jump(storage).await, // impl-revision-03 Revision
        951 => multiple_instances_same_database_write(storage).await, // impl-bulkwrite-08 BulkWrite
        996 => write_and_read_umlauts(storage).await, // impl-bulkwrite-09 BulkWrite
        1058 => resurrect_deleted_document_with_previous(storage).await, // impl-conflict-04 Conflict
        1130 => persists_all_parallel_bulk_writes(storage).await, // impl-bulkwrite-10 BulkWrite
        1512 => query_find_all(storage).await, // impl-query-01 Query
        1578 => query_sorts_correctly(storage).await, // impl-query-02 Query
        1626 => query_finds_deleted_documents(storage).await, // impl-query-03 Query
        1693 => query_deterministic_order(storage).await, // impl-query-04 Query
        1847 => query_skip_limit_non_indexed_sort(storage).await, // impl-query-05 Query
        2021 => query_over_nested_object(storage).await, // impl-query-06 Query
        2077 => query_many_documents(storage).await, // impl-query-07 Query
        2123 => count_correct_amount(storage).await, // impl-count-01 Count
        2184 => find_documents_by_id(storage).await, // impl-findbyids-02 FindByIds
        2221 => find_documents_primary_key_underscore_id(storage).await, // impl-findbyids-03 FindByIds
        2292 => find_deleted_documents(storage).await, // impl-findbyids-04 FindByIds
        2346 => with_deleted_finds_live_documents(storage).await, // impl-findbyids-05 FindByIds
        2430 => find_many_documents(storage).await, // impl-findbyids-06 FindByIds
        2462 => changed_documents_since_latest(storage).await, // impl-changedsince-01 ChangedSince
        2546 => changed_documents_since_amount(storage).await, // impl-changedsince-02 ChangedSince
        2642 => changed_documents_since_iterate_checkpoints(storage).await, // impl-changedsince-03 ChangedSince
        2696 => change_stream_one_event_per_write(storage).await, // impl-changestream-01 ChangeStream
        2791 => change_stream_all_events(storage).await, // impl-changestream-02 ChangeStream
        2881 => change_stream_no_empty_bulk(storage).await, // impl-changestream-03 ChangeStream
        2938 => store_and_retrieve_attachment(storage).await, // impl-attachment-01 Attachment
        2995 => attachment_stub_on_all_fetch_methods(storage).await, // impl-attachment-02 Attachment
        3109 => add_multiple_attachments(storage).await, // impl-attachment-03 Attachment
        3193 => deleted_document_loses_attachments(storage).await, // impl-attachment-04 Attachment
        3267 => load_multiple_attachments(storage).await, // impl-attachment-05 Attachment
        3369 => cleanup_removes_deleted_document(storage).await, // impl-cleanup-01 Cleanup
        3470 => cleanup_removes_all_deleted(storage).await, // impl-cleanup-02 Cleanup
        3550 => cleanup_returns_false_while_work_remains(storage).await, // impl-cleanup-03 Cleanup
        3613 => close_multiple_times(storage).await, // impl-cleanup-04 Cleanup
        3633 => remove_deletes_all_data(storage).await, // impl-cleanup-05 Cleanup
        3676 => remove_after_close_throws(storage).await, // impl-cleanup-06 Cleanup
        3693 => close_after_remove_does_not_throw(storage).await, // impl-cleanup-07 Cleanup
        4046 => schema_versions_do_not_interfere(storage).await, // impl-query-11 Query
        other => panic!(
            "no executable body for the case at upstream line {other}. Either the \
             declaration table and `suite::run_exercised_case` disagree, or the case is \
             declared Exercised without a body."
        ),
    }
}

/// Runs every exercised case against `storage`, returning the upstream lines that
/// failed.
///
/// A gate wants all failures rather than the first, because a storage with three bugs
/// should be told about three. A panic in one case is caught so it does not hide the
/// rest.
pub async fn run_all<S: RxStorage>(storage: &S) -> Vec<u32> {
    let mut failures = Vec::new();
    for case in crate::decl::exercised_cases() {
        let outcome = futures::FutureExt::catch_unwind(core::panic::AssertUnwindSafe(
            run_exercised_case(storage, case.upstream_line),
        ))
        .await;
        if outcome.is_err() {
            failures.push(case.upstream_line);
        }
    }
    failures
}
