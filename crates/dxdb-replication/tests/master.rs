//! The master side: `rxStorageInstanceToReplicationHandler` over a real storage instance.
//!
//! Every case here names the upstream behaviour it pins, and each is chosen because it is a
//! rule a hand-rolled test master would get subtly wrong — which is what these tests replace.
//!
//! The last section runs the same assertions over **two** storages: `dxdb-memory` and
//! `dxdb-filesystem` (redb on disk, in a temp directory). That second one matters because
//! `docs/lanes/m3-port-plan.md` section 12 asks for the ported suite green "against in-memory
//! and filesystem endpoints", and until now only in-memory had ever been exercised.

mod common {
    pub mod memory_storage;
}

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use common::memory_storage::{insert, stored_document, MemoryStorage};
use dxdb_json::Value;
use dxdb_replication::checkpoint::Checkpoint as ProtocolCheckpoint;
use dxdb_replication::conflict::DefaultConflictHandler;
use dxdb_replication::convert::document_with_attachment_payloads;
use dxdb_replication::master::StorageMaster;
use dxdb_replication::state::{ReplicationHandler, WriteToMasterRow};
use dxdb_storage::{
    Clock, DynRxStorageInstance, InstanceCreationParams, RxStorage, RxStorageInstance,
    StorageSchema,
};

/// Polls a future to completion on this thread. The protocol is single-threaded (D1), so there
/// is no executor to start and nothing to yield to.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => return value,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A clock the master can be given, so `_meta.lwt` is reproducible (D4).
///
/// # Why it steps a whole millisecond
///
/// D4's real `lwt` is milliseconds plus `k * 0.01`, so two writes inside one millisecond is
/// ordinary. `dxdb-filesystem` currently keys its change feed on `lwt.round() as i64`, which
/// collapses both writes onto one key position; the feed's exclusivity rule then excludes a
/// document that was *updated* rather than created in that millisecond, and a pull that has
/// seen the insert never sees the update.
///
/// That is a defect in `dxdb-filesystem`, not in this handler (repro and mechanism:
/// `a_sub_millisecond_update_is_invisible_to_the_change_feed` below, and
/// `docs/lanes/m3-port-plan.md` §15.6). This clock steps a whole millisecond so the *master
/// contract* can be tested against the storage behaviour that exists today without that defect
/// masking it; it isolates the divergence to the one test that asserts it instead of hiding it.
struct TestClock(RefCell<f64>);

impl TestClock {
    fn new(start: f64) -> Self {
        TestClock(RefCell::new(start))
    }
}

impl Clock for TestClock {
    fn now(&self) -> f64 {
        let mut current = self.0.borrow_mut();
        // A whole millisecond: see the note above on why this is not 0.01.
        *current += 1.0;
        *current
    }
}

const PRIMARY: &str = "passportId";

fn human_schema() -> Value {
    dxdb_replication::conformance::human_schema()
}

/// A master over a memory instance with one seeded document.
fn master_with(ids: &[&str]) -> (Rc<MemoryStorage>, StorageMaster<MemoryStorage>) {
    let storage = MemoryStorage::new("master-db", "human", human_schema(), "master-token");
    for id in ids {
        block_on(storage.bulk_write(
            vec![insert(stored_document(
                PRIMARY,
                id,
                &[("name", Value::String(format!("doc {id}")))],
            ))],
            "seed".to_string(),
        ))
        .expect("the seed write succeeds");
    }
    let instance = Rc::new(storage);
    let master = StorageMaster::new(
        instance.clone(),
        Box::new(DefaultConflictHandler),
        "master-token",
    )
    .with_clock(Box::new(TestClock::new(1_800_000_000_000.0)));
    (instance, master)
}

fn row(assumed: Option<Value>, new_document: Value) -> WriteToMasterRow {
    WriteToMasterRow {
        assumed_master_state: assumed
            .map(|value| dxdb_storage::DocumentData::from_value(value).expect("a doc")),
        new_document_state: dxdb_storage::DocumentData::from_value(new_document).expect("a doc"),
        attachment_payloads: BTreeMap::new(),
    }
}

fn doc_state(id: &str, name: &str, rev: Option<&str>) -> Value {
    let mut map = dxdb_json::Map::new();
    map.insert("passportId".to_string(), Value::String(id.to_string()));
    map.insert("name".to_string(), Value::String(name.to_string()));
    map.insert("_deleted".to_string(), Value::Bool(false));
    if let Some(rev) = rev {
        map.insert("_rev".to_string(), Value::String(rev.to_string()));
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// masterChangesSince
// ---------------------------------------------------------------------------

#[test]
fn changes_since_returns_client_states_without_meta_or_rev() {
    // `writeDocToDocState(documentData, hasAttachments, keepMeta)` (`index.ts:206`): a peer is
    // handed the document *state*, so `_meta` and `_rev` are stripped. A master that leaked
    // `_meta` would let a client compare against a clock it does not have.
    let (_storage, master) = master_with(&["a"]);
    let result = block_on(master.master_changes_since(None, 10)).expect("the pull succeeds");

    assert_eq!(result.documents.len(), 1);
    let document = &result.documents[0];
    assert_eq!(
        document.get("name"),
        Some(&Value::String("doc a".to_string()))
    );
    assert!(
        document.get("_meta").is_none(),
        "a client state has no _meta"
    );
    assert!(document.get("_rev").is_none(), "a client state has no _rev");
    assert!(
        result.checkpoint.is_some(),
        "a non-empty batch must report a checkpoint"
    );
}

#[test]
fn an_empty_batch_reports_the_callers_own_checkpoint() {
    // `checkpoint: result.documents.length > 0 ? result.checkpoint : checkpoint`
    // (`index.ts:234`). Re-reporting the storage's beginning checkpoint here would walk a
    // client's checkpoint back to the start of time and re-send everything.
    let (_storage, master) = master_with(&["a"]);
    let first = block_on(master.master_changes_since(None, 10)).expect("first pull");
    let checkpoint = first.checkpoint.expect("a checkpoint");

    let second =
        block_on(master.master_changes_since(Some(&checkpoint), 10)).expect("second pull is empty");
    assert!(second.documents.is_empty(), "nothing changed");
    assert_eq!(
        second.checkpoint.as_ref(),
        Some(&checkpoint),
        "an empty batch must hand back the checkpoint it was given"
    );
}

#[test]
fn a_storage_checkpoint_of_another_shape_is_rejected_rather_than_misread() {
    // A `{id, lwt}` feed cannot be resumed from `{sequence}`. Upstream would pass it straight
    // through; failing loudly is the safer choice, and it is D7's other side: the protocol is
    // generic, but *this* master is a `{id, lwt}` caller.
    let (_storage, master) = master_with(&["a"]);
    let mut map = dxdb_json::Map::new();
    map.insert("sequence".to_string(), Value::from(7u64));
    let checkpoint = ProtocolCheckpoint::new(Value::Object(map)).expect("an object");

    let error = block_on(master.master_changes_since(Some(&checkpoint), 10))
        .expect_err("a foreign checkpoint must not be silently accepted");
    assert!(
        error.to_string().contains("id, lwt"),
        "the error should say what shape it wanted, got: {error}"
    );
}

#[test]
fn a_batch_size_is_honoured() {
    let (_storage, master) = master_with(&["a", "b", "c"]);
    let result = block_on(master.master_changes_since(None, 2)).expect("the pull succeeds");
    assert_eq!(result.documents.len(), 2, "the limit is the caller's");
}

// ---------------------------------------------------------------------------
// masterWrite
// ---------------------------------------------------------------------------

#[test]
fn a_write_with_no_master_state_inserts() {
    // `if (!masterState) { insert }` (`index.ts:288-291`).
    let (storage, master) = master_with(&[]);
    let conflicts = block_on(master.master_write(vec![row(None, doc_state("new", "fresh", None))]))
        .expect("the write succeeds");

    assert!(conflicts.is_empty(), "an insert has no conflict");
    let stored = block_on(storage.find_documents_by_id(vec!["new".to_string()], true))
        .expect("the lookup succeeds");
    assert_eq!(stored.len(), 1, "the document reached the master");
    assert_eq!(
        stored[0].get("name"),
        Some(&Value::String("fresh".to_string()))
    );
}

#[test]
fn a_write_with_no_assumed_state_over_existing_data_is_a_conflict() {
    // `else if (masterState && !row.assumedMasterState) { conflict }` (`index.ts:292-296`).
    // The client did not know what it was overwriting, so this is not a blind write.
    let (_storage, master) = master_with(&["existing"]);
    let conflicts =
        block_on(master.master_write(vec![row(None, doc_state("existing", "clobber", None))]))
            .expect("the call succeeds");

    assert_eq!(conflicts.len(), 1, "the second writer must be told");
    assert_eq!(
        conflicts[0].get("name"),
        Some(&Value::String("doc existing".to_string())),
        "the conflict reports the master's state, not the client's"
    );
}

#[test]
fn a_matching_assumed_state_writes() {
    // The equality branch: `conflictHandler.isEqual(masterState, assumedMasterState)`
    // (`index.ts:297-306`). Compared on document *states*, so a client that never saw the
    // master's `_meta` still matches.
    let (_storage, master) = master_with(&["known"]);
    let pulled = block_on(master.master_changes_since(None, 10)).expect("the pull succeeds");
    let assumed = pulled.documents[0].clone().into_value();

    let conflicts = block_on(master.master_write(vec![row(
        Some(assumed),
        doc_state("known", "updated", None),
    )]))
    .expect("the write succeeds");

    assert!(conflicts.is_empty(), "an on-top-of write is not a conflict");
}

#[test]
fn a_mismatched_assumed_state_is_a_conflict() {
    let (_storage, master) = master_with(&["known"]);
    // Pretend the client saw a different name than the master has.
    let conflicts = block_on(master.master_write(vec![row(
        Some(doc_state("known", "stale view", None)),
        doc_state("known", "my write", None),
    )]))
    .expect("the call succeeds");

    assert_eq!(conflicts.len(), 1, "a stale assumed state must lose");
}

#[test]
fn a_write_to_a_deleted_document_is_a_conflict_not_a_resurrection() {
    // The lookup uses `findDocumentsById(ids, true)` (`index.ts:270`): tombstones count as a
    // master state. Without that, a delete on the master followed by a client write would look
    // like "no master state ⇒ insert" and silently resurrect the document.
    let (storage, master) = master_with(&["gone"]);
    let pulled = block_on(master.master_changes_since(None, 10)).expect("the pull succeeds");
    let assumed = pulled.documents[0].clone().into_value();

    let mut deleted = doc_state("gone", "doc gone", Some("1-abc"));
    deleted
        .as_object_mut()
        .expect("object")
        .insert("_deleted".to_string(), Value::Bool(true));
    let delete_row = WriteToMasterRow {
        assumed_master_state: Some(dxdb_storage::DocumentData::from_value(assumed).expect("a doc")),
        new_document_state: dxdb_storage::DocumentData::from_value(deleted).expect("a doc"),
        attachment_payloads: BTreeMap::new(),
    };
    block_on(master.master_write(vec![delete_row])).expect("the delete succeeds");

    // Now a client that still thinks the document exists tries to write.
    let conflicts =
        block_on(master.master_write(vec![row(None, doc_state("gone", "resurrect", None))]))
            .expect("the call succeeds");
    assert_eq!(
        conflicts.len(),
        1,
        "a tombstone is a master state; the write must not resurrect the document"
    );

    let stored = block_on(storage.find_documents_by_id(vec!["gone".to_string()], true))
        .expect("the lookup succeeds");
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].get("_deleted"),
        Some(&Value::Bool(true)),
        "the master document must still be deleted"
    );
}

#[test]
fn two_rows_for_one_document_collapse_to_the_last() {
    // `rowById[docId] = row` (`index.ts:259-264`): the map keeps the last row for an id.
    // Writing both would make the storage reject the second as a conflict against the first,
    // which reports a conflict the client never caused.
    let (_storage, master) = master_with(&[]);
    let conflicts = block_on(master.master_write(vec![
        row(None, doc_state("dup", "first", None)),
        row(None, doc_state("dup", "second", None)),
    ]))
    .expect("the call succeeds");

    assert!(
        conflicts.is_empty(),
        "collapsing duplicate rows must not produce a conflict"
    );
}

#[test]
fn an_empty_push_is_a_no_op() {
    let (_storage, master) = master_with(&["a"]);
    let conflicts = block_on(master.master_write(Vec::new())).expect("the call succeeds");
    assert!(conflicts.is_empty());
}

// ---------------------------------------------------------------------------
// The wire shape of a payload: one helper, two users
// ---------------------------------------------------------------------------

#[test]
fn payloads_are_emitted_inside_the_attachment_stub() {
    // A JS peer reads `Blob` data at `_attachments[id].data`; that is also what
    // `downstream::lift_attachment_payloads` reads back. Pinned here because it is the one
    // place a byte-array encoding choice becomes a compatibility question.
    let mut map = dxdb_json::Map::new();
    map.insert("passportId".to_string(), Value::String("a".to_string()));
    let mut attachments = dxdb_json::Map::new();
    let mut stub = dxdb_json::Map::new();
    stub.insert("digest".to_string(), Value::String("sha-256-x".to_string()));
    stub.insert("length".to_string(), Value::from(2u64));
    stub.insert("type".to_string(), Value::String("text/plain".to_string()));
    attachments.insert("file".to_string(), Value::Object(stub));
    map.insert("_attachments".to_string(), Value::Object(attachments));
    let document = dxdb_storage::DocumentData::from_value(Value::Object(map)).expect("a doc");

    let mut payloads = BTreeMap::new();
    payloads.insert("file".to_string(), vec![104u8, 105u8]);
    let with_data = document_with_attachment_payloads(&document, &payloads);

    let bytes = with_data
        .get("_attachments")
        .and_then(|value| value.get("file"))
        .and_then(|value| value.get("data"))
        .and_then(Value::as_array)
        .expect("a byte array at _attachments.file.data");
    assert_eq!(bytes, &vec![Value::from(104u64), Value::from(105u64)]);
}

#[test]
fn a_stub_without_a_payload_is_left_alone() {
    // The legitimate "the receiver already has these bytes" case. Inventing an empty payload
    // would change the digest's meaning.
    let mut map = dxdb_json::Map::new();
    map.insert("passportId".to_string(), Value::String("a".to_string()));
    let document = dxdb_storage::DocumentData::from_value(Value::Object(map)).expect("a doc");
    let payloads = BTreeMap::new();
    assert_eq!(
        document_with_attachment_payloads(&document, &payloads),
        document
    );
}

// ---------------------------------------------------------------------------
// §12: the same rules over an in-memory and a filesystem endpoint
// ---------------------------------------------------------------------------

/// Creates a storage instance of the named kind, over a fresh temp directory for filesystem.
///
/// The filesystem instance borrows nothing from the test after creation, so a path that
/// outlives the call is enough; the directory is removed when the test ends.
struct Endpoint {
    instance: Box<DynRxStorageInstance<'static>>,
    /// Owns the directory a filesystem instance writes into, so it is removed when the test
    /// ends. A separate guard rather than a `Drop` impl on `Endpoint`, because `Endpoint` is
    /// consumed by value and a `Drop` impl would forbid moving the instance out of it.
    temp: Option<TempDir>,
}

/// A directory that deletes itself.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("dxdb-replication-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the temp dir");
        TempDir(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Endpoint {
    fn memory() -> Self {
        let storage = MemoryStorage::new("endpoint-db", "human", human_schema(), "endpoint-token");
        Endpoint {
            // The fixture is `'static` already.
            instance: DynRxStorageInstance::new_box(storage),
            temp: None,
        }
    }

    fn filesystem() -> Self {
        // A fresh directory per test so two runs cannot share rows, the same rule the
        // filesystem storage's own conformance test follows.
        let temp = TempDir::new("master");
        let storage = dxdb_filesystem::FilesystemRxStorage::new(temp.path().to_path_buf());
        let schema = StorageSchema::from_value(&human_schema()).expect("the schema is valid");
        let instance = block_on(storage.create_storage_instance(InstanceCreationParams {
            database_instance_token: "endpoint-token".to_string(),
            database_name: "endpoint-db".to_string(),
            collection_name: "human".to_string(),
            schema,
            options: Value::Object(dxdb_json::Map::new()),
            multi_instance: false,
            dev_mode: false,
        }))
        .expect("the filesystem instance is created");
        Endpoint {
            instance,
            temp: Some(temp),
        }
    }
}

/// The rules every master must obey, whoever is storing the documents.
///
/// Written once and run against both endpoints, because the point of §12's second bullet is
/// that the *protocol* does not change when the storage does. A divergence here would be a
/// storage bug or an assumption this handler makes about storage behaviour that is not in the
/// trait.
fn master_contract_over(endpoint: Endpoint) {
    // `_temp` is bound (not dropped with the struct's field order) so the directory outlives
    // the storage instance that holds the redb file open.
    let Endpoint {
        instance,
        temp: _temp,
    } = endpoint;
    // `Box<DynRxStorageInstance>` implements `RxStorageInstance` through dynosaur's blanket
    // bridge, so it can be handed to `StorageMaster` directly.
    let handle = Rc::new(instance);
    let master = StorageMaster::new(handle, Box::new(DefaultConflictHandler), "endpoint-token")
        .with_clock(Box::new(TestClock::new(1_800_000_000_000.0)));

    // 1. An insert lands and is readable back as a client state.
    let conflicts = block_on(master.master_write(vec![row(None, doc_state("e1", "first", None))]))
        .expect("the insert succeeds");
    assert!(conflicts.is_empty(), "an insert is not a conflict");

    let pulled = block_on(master.master_changes_since(None, 10)).expect("the pull succeeds");
    assert_eq!(pulled.documents.len(), 1);
    assert!(pulled.documents[0].get("_meta").is_none());

    // 2. The same write without an assumed state is a conflict.
    let conflicts = block_on(master.master_write(vec![row(None, doc_state("e1", "blind", None))]))
        .expect("the call succeeds");
    assert_eq!(conflicts.len(), 1, "a blind overwrite must conflict");

    // 3. An on-top-of write applies.
    let assumed = pulled.documents[0].clone().into_value();
    let conflicts = block_on(master.master_write(vec![row(
        Some(assumed.clone()),
        doc_state("e1", "second", None),
    )]))
    .expect("the write succeeds");
    assert!(conflicts.is_empty(), "an informed write must apply");

    // 4. The checkpoint advances and nothing is re-sent.
    let checkpoint = pulled.checkpoint.clone().expect("a checkpoint");
    let after =
        block_on(master.master_changes_since(Some(&checkpoint), 10)).expect("the pull succeeds");
    assert_eq!(
        after.documents.len(),
        1,
        "the write changed one document after the checkpoint"
    );
}

#[test]
fn the_master_contract_holds_over_an_in_memory_endpoint() {
    master_contract_over(Endpoint::memory());
}

#[test]
fn the_master_contract_holds_over_a_filesystem_endpoint() {
    // `docs/lanes/m3-port-plan.md` section 12: "the ported suite green against in-memory and
    // filesystem endpoints". This is the filesystem half, over redb on a real temp directory.
    master_contract_over(Endpoint::filesystem());
}

// ---------------------------------------------------------------------------
// A defect this endpoint exposed, kept as a runnable repro
// ---------------------------------------------------------------------------

/// A clock whose `lwt` advances by one hundredth, which is what D4 specifies and what
/// `dxdb-storage::lwt_from_parts` produces for two writes inside one millisecond.
struct HundredthsClock(RefCell<f64>);

impl Clock for HundredthsClock {
    fn now(&self) -> f64 {
        let mut current = self.0.borrow_mut();
        *current += 0.01;
        *current
    }
}

/// **A defect in `dxdb-filesystem`: an update can become invisible to the change feed.**
///
/// This is the replication-level symptom, found by running the master contract over a real
/// filesystem endpoint with a D4-shaped clock rather than a whole-millisecond one. It is
/// `#[ignore]`d so the gate stays honest while the owning lane fixes the storage; run it with
///
/// ```text
/// cargo test -p dxdb-replication --test master -- --ignored
/// ```
///
/// # What happens
///
/// `dxdb-filesystem` keys its change feed as `BE(flip(lwt.round() as i64)) ++ prefix ++ id`
/// (`layout::encode_lwt`, `FilesystemStorage::put_documents`). The `.round()` discards the
/// hundredths that D4 says the ordering logic *relies on being broken* ("A whole-millisecond
/// clock produces ties that the ordering logic relies on being broken").
///
/// So when a document is inserted and then updated inside one millisecond:
///
/// 1. both writes land on the same key position, the second overwriting the first in `chg`;
/// 2. the checkpoint a pull receives is `(id, lwt)` for that document;
/// 3. the feed's exclusivity rule is `lwt > start.lwt || (lwt == start.lwt && id > start.id)`;
/// 4. after rounding, the updated document's `lwt` *equals* the checkpoint's and its `id` is
///    *equal*, not greater, so it is excluded.
///
/// The pull therefore never sees the update. `dxdb-memory` does not have this problem — it
/// sorts and compares on the exact `f64` — so this is a storage divergence, not a protocol
/// rule, and it means the same replication is sound or lossy depending on which storage it runs
/// over.
///
/// # The fix, for whoever owns that crate
///
/// Key and compare on hundredths: `(lwt * 100).round() as i64` in `encode_lwt`/`decode_lwt` and
/// in ``put_documents`'s `i64` parameter, then divide on decode. `cleanup`'s cutoff needs the
/// same unit change. That keeps byte order == numeric order and preserves the tie-breaker.
///
/// Written up in `docs/lanes/m3-port-plan.md` §15.6.
#[test]
#[ignore = "dxdb-filesystem rounds lwt to whole ms, losing intra-millisecond updates; see \
            docs/lanes/m3-port-plan.md section 15.6"]
fn a_sub_millisecond_update_is_invisible_to_the_change_feed() {
    // Destructured so the instance can move into the `Rc` while the temp-directory guard is
    // bound to a local that lives to the end of the test: the directory holds the redb file the
    // instance keeps open, and `TempDir::drop` would otherwise delete it mid-test.
    let Endpoint {
        instance,
        temp: _temp,
    } = Endpoint::filesystem();
    let master = StorageMaster::new(
        Rc::new(instance),
        Box::new(DefaultConflictHandler),
        "endpoint-token",
    )
    .with_clock(Box::new(HundredthsClock(RefCell::new(1_800_000_000_000.0))));

    // 1. Insert, and take the checkpoint exactly as a pull would.
    block_on(master.master_write(vec![row(None, doc_state("u", "first", None))]))
        .expect("the insert succeeds");
    let pulled = block_on(master.master_changes_since(None, 10)).expect("the pull succeeds");
    assert_eq!(pulled.documents.len(), 1);
    let checkpoint = pulled.checkpoint.clone().expect("a checkpoint");
    let assumed = pulled.documents[0].clone().into_value();

    // 2. Update the same document, one hundredth of a millisecond later.
    block_on(master.master_write(vec![row(Some(assumed), doc_state("u", "second", None))]))
        .expect("the update succeeds");

    // 3. The pull must see the update.
    let after =
        block_on(master.master_changes_since(Some(&checkpoint), 10)).expect("the pull succeeds");
    assert_eq!(
        after.documents.len(),
        1,
        "an update to `u` is missing from the change feed: the rounded `lwt` key tied it with \
         its own checkpoint, so the exclusivity rule excluded it"
    );
    assert_eq!(
        after.documents[0].get("name"),
        Some(&Value::String("second".to_string()))
    );
}
