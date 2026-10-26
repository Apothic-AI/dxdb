//! Tests for the push direction: the observable contract, the pause fix, and the default
//! conflict handler against the three conflict-input shapes upstream distinguishes.
//!
//! These run against [`MemoryStorage`], a real `RxStorageInstance` (see
//! `common/memory_storage.rs`), so what is exercised is the actual protocol code, not a
//! fixture of it.

mod common {
    pub mod http_transport;
    pub mod memory_storage;
}

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use common::memory_storage::{insert, stored_document, MemoryStorage};
use dxdb_json::Value;
use dxdb_replication::checkpoint::{Checkpoint, Sha256HashFunction};
use dxdb_replication::conflict::{ConflictHandler, DefaultConflictHandler};
use dxdb_replication::meta_instance::get_assumed_master_state;
use dxdb_replication::state::{
    BatchSize, DocumentsWithCheckpoint, InitialCheckpoint, PullStream, ReplicationHandler,
    ReplicationInput, ReplicationState, WriteToMasterRow,
};
use dxdb_replication::upstream::Upstream;
use dxdb_storage::{DocumentData, RxStorageInstance, StorageError};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The human schema the fixtures use, with `maxLength` because the meta schema needs it.
fn human_schema() -> Value {
    dxdb_json::parse(
        r#"{
            "version": 0,
            "keyCompression": false,
            "primaryKey": "passportId",
            "type": "object",
            "properties": {
                "passportId": { "type": "string", "maxLength": 100 },
                "firstName": { "type": "string", "maxLength": 100 },
                "age": { "type": "integer" }
            },
            "indexes": ["firstName"]
        }"#,
    )
    .expect("valid schema")
}

/// A document with the fixture's fields.
fn human(id: &str, first: &str, age: i64) -> DocumentData {
    let mut document = stored_document(
        "passportId",
        id,
        &[
            ("firstName", Value::String(first.to_string())),
            ("age", Value::from(age)),
        ],
    );
    // A document that exists on a master carries a revision that master minted. The
    // fixture default is the empty string, which `try_get_height_of_revision` reads as
    // "no revision yet" -- correct for a fresh fork document, wrong for a master's.
    document.set("_rev", Value::String("1-master".to_string()));
    document
}

/// A hook the master runs before returning from `master_write`.
///
/// Named rather than inline because the type is the whole mechanism of the pause test: it is
/// how a test lands a `pause()` in the window between "the master applied the write" and "the
/// protocol recorded it as sent".
type PauseHook = Box<dyn FnOnce(&RecordingMaster)>;

/// A master that records what it was asked and answers from an in-memory map.
///
/// The four `masterWrite` rules are reproduced because the protocol's conflict handling
/// only becomes reachable when the master can *refuse* a row:
///
/// * no master state -> insert;
/// * master state but no assumed state -> conflict;
/// * assumed state equals the master's -> write;
/// * otherwise -> conflict.
#[derive(Default)]
struct RecordingMaster {
    documents: RefCell<Vec<DocumentData>>,
    clock: RefCell<f64>,
    /// Every row batch this master was handed, in order.
    write_batches: RefCell<Vec<Vec<WriteToMasterRow>>>,
    /// The real master states it answered with.
    conflicts_reported: RefCell<Vec<DocumentData>>,
    /// When set, the next `master_write` runs this hook before returning its response.
    /// `None` means normal behaviour. This is how a test pauses *inside* a push.
    before_returning: RefCell<Option<PauseHook>>,
    changes_since_calls: RefCell<u64>,
}

impl RecordingMaster {
    fn new(documents: Vec<DocumentData>) -> Rc<Self> {
        let master = RecordingMaster::default();
        *master.clock.borrow_mut() = 100.0;
        *master.documents.borrow_mut() = documents;
        Rc::new(master)
    }

    fn stored(&self) -> Vec<DocumentData> {
        self.documents.borrow().clone()
    }

    fn write_batches(&self) -> Vec<Vec<WriteToMasterRow>> {
        self.write_batches.borrow().clone()
    }
}

fn same_content(a: &DocumentData, b: &DocumentData) -> bool {
    fn projection(document: &DocumentData) -> Value {
        let mut map = dxdb_json::Map::new();
        for (key, value) in document.as_object() {
            if matches!(key.as_str(), "_rev" | "_meta") {
                continue;
            }
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
    projection(a) == projection(b)
}

impl ReplicationHandler for RecordingMaster {
    fn master_changes_since(
        &self,
        checkpoint: Option<&Checkpoint>,
        batch_size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentsWithCheckpoint, StorageError>>>> {
        *self.changes_since_calls.borrow_mut() += 1;
        let start = checkpoint.cloned().map(|checkpoint| {
            (
                checkpoint
                    .as_map()
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                checkpoint
                    .as_map()
                    .get("lwt")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
            )
        });
        let mut documents: Vec<DocumentData> = self.documents.borrow().clone();
        documents.sort_by(|a, b| {
            a.lwt()
                .unwrap_or(0.0)
                .partial_cmp(&b.lwt().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let documents: Vec<DocumentData> = documents
            .into_iter()
            .filter(|document| match &start {
                None => true,
                Some((id, lwt)) => {
                    let doc_lwt = document.lwt().unwrap_or(0.0);
                    let doc_id = document
                        .get("passportId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    doc_lwt > *lwt
                        || (doc_lwt == *lwt
                            && dxdb_jsstr::compare_js(doc_id, id) == std::cmp::Ordering::Greater)
                }
            })
            .take(batch_size)
            .collect();
        let checkpoint = documents.last().map(|last| {
            let mut map = dxdb_json::Map::new();
            map.insert(
                "id".to_string(),
                last.get("passportId").cloned().unwrap_or(Value::Null),
            );
            map.insert("lwt".to_string(), Value::from(last.lwt().unwrap_or(0.0)));
            Checkpoint::new(Value::Object(map)).expect("an object checkpoint")
        });
        // `documents.length > 0 ? result.checkpoint : checkpoint`, verbatim: a no-op poll
        // must NOT advance the checkpoint, or the client would skip documents that arrive
        // next.
        let checkpoint = if documents.is_empty() {
            checkpoint_input(start.clone())
        } else {
            checkpoint
        };
        Box::pin(async move {
            Ok(DocumentsWithCheckpoint {
                documents,
                checkpoint,
            })
        })
    }

    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DocumentData>, StorageError>>>> {
        self.write_batches.borrow_mut().push(rows.clone());
        let mut conflicts = Vec::new();
        {
            let mut documents = self.documents.borrow_mut();
            let mut clock = self.clock.borrow_mut();
            for row in &rows {
                let id = row
                    .new_document_state
                    .get("passportId")
                    .and_then(Value::as_str)
                    .expect("a row has a primary key")
                    .to_string();
                let existing = documents
                    .iter()
                    .find(|document| {
                        document.get("passportId").and_then(Value::as_str) == Some(&id)
                    })
                    .cloned();
                let decision = match (&existing, &row.assumed_master_state) {
                    (None, _) => Some(row.new_document_state.clone()),
                    (Some(master), None) => {
                        conflicts.push(master.clone());
                        None
                    }
                    (Some(master), Some(assumed)) => {
                        if same_content(master, assumed) {
                            Some(row.new_document_state.clone())
                        } else {
                            conflicts.push(master.clone());
                            None
                        }
                    }
                };
                if let Some(mut document) = decision {
                    *clock += 1.0;
                    // `try_get_height_of_revision`: a document that has not been through
                    // this master yet carries no valid revision (the fixture default is the
                    // empty string), which is height 0 rather than a panic. A real master
                    // only ever holds revisions it minted itself.
                    let height = existing
                        .as_ref()
                        .and_then(DocumentData::rev)
                        .and_then(dxdb_storage::try_get_height_of_revision)
                        .unwrap_or(0)
                        + 1;
                    document.set("_rev", Value::String(format!("{height}-master")));
                    document.set_lwt(*clock).expect("finite clock");
                    match documents
                        .iter_mut()
                        .find(|slot| slot.get("passportId").and_then(Value::as_str) == Some(&id))
                    {
                        Some(slot) => *slot = document,
                        None => documents.push(document),
                    }
                }
            }
        }
        self.conflicts_reported
            .borrow_mut()
            .extend(conflicts.clone());

        // Run the armed hook *after* the master has applied the write and *before* the
        // response is returned. That window is the one upstream's pause guard protects:
        // the request succeeded on the master, but the client must not record it as sent
        // while paused. Without invoking the hook here the pause test would assert against
        // a state nothing ever changed, and pass no matter what the protocol did.
        if let Some(hook) = self.before_returning.borrow_mut().take() {
            hook(self);
        }

        Box::pin(async move { Ok(conflicts) })
    }

    fn master_change_stream(&self) -> PullStream {
        Box::pin(futures::stream::empty())
    }
}

/// Rebuilds the input checkpoint for the empty-batch case.
///
/// Upstream returns the *input* checkpoint when nothing changed, so a no-op poll cannot
/// advance it. `RecordingMaster` stores the input as `(id, lwt)`, which is the shape this
/// fixture's checkpoints have.
fn checkpoint_input(input: Option<(String, f64)>) -> Option<Checkpoint> {
    input.and_then(|(id, lwt)| {
        let mut map = dxdb_json::Map::new();
        map.insert("id".to_string(), Value::String(id));
        map.insert("lwt".to_string(), Value::from(lwt));
        Checkpoint::new(Value::Object(map)).ok()
    })
}

/// Builds a fork, a meta instance, a state and an upstream.
struct Harness {
    fork: MemoryStorage,
    master: Rc<RecordingMaster>,
    state: ReplicationState<MemoryStorage>,
}

fn harness(master_documents: Vec<DocumentData>) -> Harness {
    let fork = MemoryStorage::new("db", "human", human_schema(), "fork-token");
    let meta_schema =
        dxdb_replication::meta_instance::replication_meta_instance_schema(fork.schema(), false)
            .expect("the meta schema is derivable");
    let meta = MemoryStorage::new("db", "rx-replication-meta", meta_schema, "meta-token");
    let master = RecordingMaster::new(master_documents);
    let handler: Box<dyn ReplicationHandler> = Box::new(SharedMaster(master.clone()));
    let state = ReplicationState::new(ReplicationInput {
        identifier: "rxdbreplicationtest".to_string(),
        pull_batch_size: BatchSize::new(100),
        push_batch_size: BatchSize::new(100),
        handler,
        conflict_handler: Box::new(DefaultConflictHandler),
        fork: fork.instance(),
        meta: meta.instance(),
        hash_function: Rc::new(Sha256HashFunction),
        skip_storing_pull_meta: false,
        keep_meta: false,
        initial_checkpoint: InitialCheckpoint::default(),
        wait_before_persist: None,
    });
    // `meta` is moved into the state; the handle is not needed separately, because
    // `get_assumed_master_state` reads it through the state.
    let _ = &meta;
    Harness {
        fork,
        master,
        state,
    }
}

/// Wraps the `Rc` master so it can be the state's handler.
struct SharedMaster(Rc<RecordingMaster>);

impl ReplicationHandler for SharedMaster {
    fn master_changes_since(
        &self,
        checkpoint: Option<&Checkpoint>,
        batch_size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentsWithCheckpoint, StorageError>>>> {
        // Clone the checkpoint into the future: the trait's return type carries no borrow
        // of `self`, so the delegate's future must own what it reads.
        let checkpoint = checkpoint.cloned();
        let inner = self.0.clone();
        Box::pin(async move {
            inner
                .master_changes_since(checkpoint.as_ref(), batch_size)
                .await
        })
    }
    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DocumentData>, StorageError>>>> {
        let inner = self.0.clone();
        Box::pin(async move { inner.master_write(rows).await })
    }
    fn master_change_stream(&self) -> PullStream {
        self.0.master_change_stream()
    }
}

/// Runs a future to completion on the current thread.
///
/// The protocol is written to be pumped by a caller's executor (D2/D3), and a test is a
/// caller. This is the smallest executor that is correct for futures with no real I/O: it
/// polls, and on `Pending` re-polls after yielding to the queued work. It is a test helper,
/// not a runtime.
fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Noop;
    impl Wake for Noop {
        fn wake(self: std::sync::Arc<Self>) {}
        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }
    let waker = Waker::from(std::sync::Arc::new(Noop));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => {
                // Our futures either complete or wait on a queue that is drained
                // synchronously; a yield is enough to let the queue advance.
                std::thread::yield_now();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The observable contract
// ---------------------------------------------------------------------------

/// A local write reaches the master as `{assumedMasterState, newDocumentState}`, and the
/// master's answer is empty because nothing conflicted.
///
/// This is the contract in one test: the request shape, the response shape, and the fact
/// that the assumed state is `None` for an insert the master has never seen.
#[test]
fn a_local_write_is_pushed_with_an_assumed_master_state() {
    let h = harness(Vec::new());
    block_on(
        h.fork
            .bulk_write(vec![insert(human("a", "alice", 30))], "local".to_string()),
    )
    .expect("the fork write succeeds");

    let mut upstream = Upstream::new(h.state.clone());
    block_on(upstream.pump()).expect("the pump succeeds");

    let batches = h.master.write_batches();
    assert_eq!(
        batches.len(),
        1,
        "exactly one master_write for one document"
    );
    assert_eq!(batches[0].len(), 1);
    let row = &batches[0][0];
    assert!(
        row.assumed_master_state.is_none(),
        "the fork has never seen this document on the master, so it assumes nothing"
    );
    assert_eq!(
        row.new_document_state
            .get("firstName")
            .and_then(Value::as_str),
        Some("alice")
    );

    // The master holds it, and reported no conflicts.
    let stored = h.master.stored();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].get("passportId").and_then(Value::as_str),
        Some("a")
    );
    assert!(
        h.master.conflicts_reported.borrow().is_empty(),
        "a clean push reports no conflicts"
    );

    // And the assumed-master state was recorded, which is what makes the *next* push a
    // compare-and-set instead of a blind overwrite.
    let assumed = block_on(get_assumed_master_state(&h.state, &["a".to_string()]))
        .expect("the meta read succeeds");
    assert!(
        assumed.contains_key("a"),
        "the push must record what it believes the master now holds"
    );
}

/// The master's answer is *only* conflicts: a successful push returns nothing.
#[test]
fn master_write_returns_only_conflicts() {
    // The master already holds a different state for "a".
    let master_doc = human("a", "from-master", 99);
    let h = harness(vec![master_doc]);
    // The fork writes a divergent state without having seen the master's.
    block_on(h.fork.bulk_write(
        vec![insert(human("a", "from-fork", 10))],
        "local".to_string(),
    ))
    .expect("the fork write succeeds");

    let mut upstream = Upstream::new(h.state.clone());
    block_on(upstream.pump()).expect("the pump succeeds");

    let reported = h.master.conflicts_reported.borrow().clone();
    assert_eq!(
        reported.len(),
        1,
        "the master must report exactly the one refused row"
    );
    assert_eq!(
        reported[0].get("firstName").and_then(Value::as_str),
        Some("from-master"),
        "the returned state is the master's real state, not the fork's"
    );

    // That conflict was resolved by keeping the master, so the fork now holds it.
    let fork_doc = h.fork.get("a").expect("the fork has the document");
    assert_eq!(
        fork_doc.get("firstName").and_then(Value::as_str),
        Some("from-master"),
        "the default handler drops the fork and keeps the master"
    );
}

/// The push never sends more rows than `push_batch_size`.
#[test]
fn master_write_is_never_handed_more_than_the_batch_size() {
    let fork = MemoryStorage::new("db", "human", human_schema(), "fork-token");
    let meta_schema =
        dxdb_replication::meta_instance::replication_meta_instance_schema(fork.schema(), false)
            .expect("schema");
    let meta = MemoryStorage::new("db", "rx-replication-meta", meta_schema, "meta-token");
    let master = RecordingMaster::new(Vec::new());
    let state = ReplicationState::new(ReplicationInput {
        identifier: "rxdbreplicationbatch".to_string(),
        pull_batch_size: BatchSize::new(100),
        // The protocol's job is to respect this number; upstream splits rows itself
        // (`upstream.ts:449-455`).
        push_batch_size: BatchSize::new(3),
        handler: Box::new(SharedMaster(master.clone())),
        conflict_handler: Box::new(DefaultConflictHandler),
        fork: fork.instance(),
        meta: meta.instance(),
        hash_function: Rc::new(Sha256HashFunction),
        skip_storing_pull_meta: false,
        keep_meta: false,
        initial_checkpoint: InitialCheckpoint::default(),
        wait_before_persist: None,
    });

    for index in 0..7 {
        block_on(fork.bulk_write(
            vec![insert(human(&format!("doc-{index}"), "x", 20 + index))],
            "local".to_string(),
        ))
        .expect("the fork write succeeds");
    }

    let mut upstream = Upstream::new(state);
    block_on(upstream.pump()).expect("the pump succeeds");

    let batches = master.write_batches();
    assert!(
        batches.iter().all(|batch| batch.len() <= 3),
        "no master_write call may exceed push_batch_size, got lengths {:?}",
        batches.iter().map(Vec::len).collect::<Vec<_>>()
    );
    let total: usize = batches.iter().map(Vec::len).sum();
    assert_eq!(total, 7, "every document must be pushed exactly once");
}

/// The own-write filter is real, and it lives on the change-stream path only.
///
/// The filter reads `bulk.context` and drops documents whose bulk was caused by the
/// downstream write. Two facts make this test what it is:
///
/// 1. The filter cannot apply to the initial sync. `getChangedDocumentsSince(forkInstance,
///    ...)` (`upstream.ts:174`) returns *documents*, and a document carries no bulk context,
///    so there is nothing for the initial sync to filter on. Upstream has the same shape.
/// 2. Upstream *also* drops change-stream tasks that arrived before the last initial-sync
///    fetch (`upstream.ts:236-243`), precisely so those documents are not processed twice.
///
/// Together those mean: on the very first pump, a document already in the fork is handled by
/// the initial sync no matter which context wrote it. The filter matters on a *subsequent*
/// pump, when the replication is live and a pull write arrives as an event. So that is what
/// this test sets up — and it asserts the discriminating property directly, because a filter
/// that dropped everything would also pass a "was not pushed" assertion.
#[test]
fn the_own_write_filter_skips_only_the_downstream_context() {
    let h = harness(Vec::new());
    let mut upstream = Upstream::new(h.state.clone());

    // Establish a checkpoint so the initial sync has a position and the document below is
    // unambiguously a *post-start* event rather than backlog.
    block_on(upstream.pump()).expect("the priming pump succeeds");
    upstream.begin_live_session();

    let downstream_flag = h.state.downstream_bulk_write_flag().to_string();
    block_on(
        h.fork
            .bulk_write(vec![insert(human("a", "from-master", 1))], downstream_flag),
    )
    .expect("the downstream write succeeds");

    let pushes_before = h.master.write_batches().len();
    block_on(upstream.pump_live_only()).expect("the live pump succeeds");

    assert_eq!(
        h.master.write_batches().len(),
        pushes_before,
        "a document written by the pull must not be pushed back to the master"
    );

    // And a local write on the same path *is* pushed, so the filter discriminates rather
    // than dropping everything.
    block_on(
        h.fork
            .bulk_write(vec![insert(human("b", "local", 2))], "local".to_string()),
    )
    .expect("the local write succeeds");
    block_on(upstream.pump_live_only()).expect("the live pump succeeds");
    assert!(
        h.master.write_batches().iter().flatten().any(|row| row
            .new_document_state
            .get("passportId")
            .and_then(Value::as_str)
            == Some("b")),
        "a local write must reach the master"
    );
}

/// **Pausing mid-push leaves the document pending.**
///
/// This is the regression test for the explicit upstream fix at `upstream.ts:435-446`:
///
/// > "Check for canceled or paused state before marking documents as successfully pushed.
/// > When the replication is paused during a push retry, masterWrite() returns [] even
/// > though no documents were actually sent. Without this check the meta instance would be
/// > updated, causing the documents to never be retried on resume."
///
/// The failure this guards against is silent and permanent: a paused push records the
/// assumed master state as if the write had landed, so on resume the document compares
/// equal to the assumed master and is skipped forever. The document is lost.
///
/// The harness pauses *from inside* `master_write`, which is the only way to hit the window
/// between the call returning and the meta write.
#[test]
fn pause_mid_push_leaves_the_document_pending() {
    let h = harness(Vec::new());
    block_on(
        h.fork
            .bulk_write(vec![insert(human("a", "alice", 30))], "local".to_string()),
    )
    .expect("the fork write succeeds");

    let mut upstream = Upstream::new(h.state.clone());

    // Arm a pause that fires while `master_write` is still executing. This models
    // `replicationState.pause()` landing during an in-flight request, which is the race
    // upstream's comment describes.
    {
        let state = h.state.clone();
        let mut hook = h.master.before_returning.borrow_mut();
        *hook = Some(Box::new(move |_master| {
            state.pause();
        }));
    }

    // Drive the pump far enough to reach `master_write`. The pump stops early when paused,
    // which is itself part of the behaviour under test.
    let outcome = block_on(upstream.pump());

    // The master did receive the write (the request was in flight). What matters is what
    // the *fork's bookkeeping* says afterwards.
    let assumed = block_on(get_assumed_master_state(&h.state, &["a".to_string()]))
        .expect("the meta read succeeds");
    assert!(
        !assumed.contains_key("a"),
        "a paused push must NOT record the assumed master state: that would mark an \
         unconfirmed write as sent and the document would never be retried. \
         Got {assumed:?}"
    );

    // Nothing was emitted as pushed either, so a UI observing `sent$` is not lied to.
    assert!(
        h.state.events().processed_up().is_empty(),
        "a paused push must not report the document as processed"
    );

    // The pump reports no progress rather than claiming success.
    if let Ok(outcome) = outcome {
        assert_eq!(
            outcome.pushed, 0,
            "the paused push must not count the document as pushed"
        );
    }

    // --- and on resume it is retried ------------------------------------------------
    // Clearing the pause and pumping again must push the document for real. This is the
    // half that proves the document was not lost.
    h.state.resume();
    h.master.documents.borrow_mut().clear();
    let resumed = block_on(upstream.pump()).expect("the resumed pump succeeds");
    assert!(
        resumed.pushed >= 1,
        "after resume the document must be pushed again, got {resumed:?}"
    );
    assert_eq!(
        h.master.stored().len(),
        1,
        "the master must hold the document after the resumed push"
    );
}

/// A cancelled push is not recorded as sent either.
///
/// The same guard covers cancellation, and the consequence is the same. Tested separately
/// because cancellation is terminal: the assertion is about the meta instance, not about a
/// retry.
#[test]
fn cancel_mid_push_does_not_record_the_write() {
    let h = harness(Vec::new());
    block_on(
        h.fork
            .bulk_write(vec![insert(human("a", "alice", 30))], "local".to_string()),
    )
    .expect("the fork write succeeds");

    let mut upstream = Upstream::new(h.state.clone());
    {
        let state = h.state.clone();
        let mut hook = h.master.before_returning.borrow_mut();
        *hook = Some(Box::new(move |_master| {
            state.cancel();
        }));
    }
    let _ = block_on(upstream.pump());

    let assumed = block_on(get_assumed_master_state(&h.state, &["a".to_string()]))
        .expect("the meta read succeeds");
    assert!(
        !assumed.contains_key("a"),
        "a cancelled push must not record the assumed master state"
    );
    assert!(
        h.state.events().processed_up().is_empty(),
        "a cancelled push must not emit processed.up"
    );
}

// ---------------------------------------------------------------------------
// The default conflict handler, against the three shapes upstream distinguishes
// ---------------------------------------------------------------------------

/// Upstream's `masterWrite` distinguishes exactly three shapes of conflict input
/// (`rxStorageInstanceToReplicationHandler`, `index.ts:87-105`), and each has a different
/// meaning for the handler. Testing them separately matters because two of the three look
/// identical from the handler's point of view — the difference is whether
/// `assumedMasterState` is present — and a port that collapsed them would resolve a
/// genuine conflict as if it were an insert.
///
/// Shape 1: **the master has the document, the fork assumed nothing.**
/// This happens when a pull brought the document in but the meta write was lost. The
/// handler must resolve, and the fork must end up with the master's state.
#[test]
fn conflict_shape_one_master_known_fork_assumed_nothing() {
    let h = harness(vec![human("a", "from-master", 99)]);
    block_on(h.fork.bulk_write(
        vec![insert(human("a", "from-fork", 10))],
        "local".to_string(),
    ))
    .expect("the fork write succeeds");

    let mut upstream = Upstream::new(h.state.clone());
    block_on(upstream.pump()).expect("the pump succeeds");

    // The row that went out had no assumed state, which is what makes this shape 1.
    let batches = h.master.write_batches();
    let row = batches
        .first()
        .and_then(|batch| batch.first())
        .expect("one row was pushed");
    assert!(
        row.assumed_master_state.is_none(),
        "shape 1: the fork assumed nothing"
    );

    // The handler resolved it, and the resolution is observable.
    let conflicts = h.state.events().resolved_conflicts();
    assert_eq!(conflicts.len(), 1, "one conflict was resolved");
    assert!(
        conflicts[0].input.assumed_master_state.is_none(),
        "the handler was told the assumed state was absent"
    );
    assert_eq!(
        conflicts[0]
            .input
            .real_master_state
            .get("firstName")
            .and_then(Value::as_str),
        Some("from-master"),
        "the handler was given the master's real state"
    );

    // The default handler returned the master state.
    assert_eq!(
        conflicts[0].output.get("firstName").and_then(Value::as_str),
        Some("from-master"),
        "the default handler drops the fork"
    );
    assert_eq!(
        h.fork
            .get("a")
            .and_then(|doc| doc.get("firstName").cloned()),
        Some(Value::String("from-master".to_string())),
        "and the fork holds the master's state afterwards"
    );
}

/// Shape 2: **the master moved since the fork last looked.**
/// `assumedMasterState` is present and differs from the master. This is the ordinary
/// concurrent-edit conflict, and it is the case the conformance suite's conflict test
/// exercises.
#[test]
fn conflict_shape_two_master_moved_since_the_assumption() {
    // Master and fork start from the *same* state, so the fork's assumption is real.
    let base = human("a", "base", 20);
    let h = harness(vec![base.clone()]);
    block_on(h.fork.bulk_write(vec![insert(base)], "local".to_string()))
        .expect("the fork write succeeds");

    // The first pump establishes the assumption. Whether it has to send a row depends on
    // whether the fork's state is already identical to the master's; what matters for shape 2
    // is that afterwards the fork *has* an assumed-master state to compare against.
    let mut upstream = Upstream::new(h.state.clone());
    block_on(upstream.pump()).expect("the first pump succeeds");
    let assumed_after_first =
        block_on(get_assumed_master_state(&h.state, &["a".to_string()])).expect("read");
    assert!(
        assumed_after_first.contains_key("a"),
        "the first pump must record what the master holds, or the second pump cannot detect \
         that the master moved"
    );

    // Now move the master behind the fork's back, and edit the fork too.
    {
        let mut documents = h.master.documents.borrow_mut();
        let mut moved = documents[0].clone();
        moved.set("firstName", Value::String("master-moved".to_string()));
        moved.set_lwt(555.0).expect("finite");
        documents[0] = moved;
    }
    block_on(h.fork.bulk_write(
        vec![dxdb_storage::update_row(
            h.fork.get("a").expect("fork doc"),
            human("a", "fork-edited", 21),
        )],
        "local".to_string(),
    ))
    .expect("the fork update succeeds");

    let mut upstream = Upstream::new(h.state.clone());
    block_on(upstream.pump()).expect("the second pump succeeds");

    // The pushed row carried an assumed state, which is what makes it shape 2.
    let batches = h.master.write_batches();
    let last = batches
        .last()
        .and_then(|batch| batch.first())
        .expect("a row was pushed");
    assert!(
        last.assumed_master_state.is_some(),
        "shape 2: the fork assumed a master state"
    );

    let conflicts = h.state.events().resolved_conflicts();
    assert!(
        !conflicts.is_empty(),
        "the master's divergence must produce at least one conflict"
    );
    // The same document can be resolved more than once per pump: the initial sync sees it
    // (it has no context to filter on) and the change stream sees the event. Upstream has the
    // same property, and it is harmless -- each resolution writes the same master state. What
    // matters is *what* the handler was told and *what* it returned.
    let conflict = conflicts.last().expect("checked above");
    assert_eq!(
        conflict
            .input
            .real_master_state
            .get("firstName")
            .and_then(Value::as_str),
        Some("master-moved"),
        "the handler was given the master's real, moved state"
    );
    assert_eq!(
        conflict.output.get("firstName").and_then(Value::as_str),
        Some("master-moved"),
        "the master's state wins"
    );
}

/// Shape 3: **the insert race.** The master has no document and the fork assumed nothing,
/// so there is no conflict at all — but if two forks insert the same id, the second one
/// arrives as shape 1 from the master's point of view and must not be silently merged.
///
/// This is the shape a port is most likely to get wrong by treating "no assumed state" as
/// "no conflict", which would drop one of the two inserts.
#[test]
fn conflict_shape_three_two_inserts_of_the_same_id() {
    // Two independent forks, each inserting the same id from an empty base.
    let h = harness(Vec::new());
    block_on(h.fork.bulk_write(
        vec![insert(human("dup", "fork-one", 1))],
        "local".to_string(),
    ))
    .expect("the first fork write succeeds");

    let mut upstream = Upstream::new(h.state.clone());
    block_on(upstream.pump()).expect("the first pump succeeds");
    assert!(
        h.master.conflicts_reported.borrow().is_empty(),
        "the first insert is uncontested, so it must not be reported as a conflict"
    );

    // A second fork inserts the same id, having never seen the master.
    let fork_two = MemoryStorage::new("db2", "human", human_schema(), "fork-two-token");
    block_on(fork_two.bulk_write(
        vec![insert(human("dup", "fork-two", 2))],
        "local".to_string(),
    ))
    .expect("the second fork write succeeds");
    // Point the same master at it via a second state.
    let state_two = ReplicationState::new(ReplicationInput {
        identifier: "rxdbreplicationalt".to_string(),
        pull_batch_size: BatchSize::new(100),
        push_batch_size: BatchSize::new(100),
        handler: Box::new(SharedMaster(h.master.clone())),
        conflict_handler: Box::new(DefaultConflictHandler),
        fork: fork_two.instance(),
        meta: {
            let meta_schema = dxdb_replication::meta_instance::replication_meta_instance_schema(
                fork_two.schema(),
                false,
            )
            .expect("schema");
            MemoryStorage::new("db2", "rx-replication-meta", meta_schema, "meta-two-token")
                .instance()
        },
        hash_function: Rc::new(Sha256HashFunction),
        skip_storing_pull_meta: false,
        keep_meta: false,
        initial_checkpoint: InitialCheckpoint::default(),
        wait_before_persist: None,
    });
    let mut upstream_two = Upstream::new(state_two.clone());
    block_on(upstream_two.pump()).expect("the second pump succeeds");

    // The master refused the second insert: it held a different document under that id.
    let reported = h.master.conflicts_reported.borrow().clone();
    assert_eq!(
        reported.len(),
        1,
        "the second insert of the same id is a conflict, not a silent overwrite"
    );
    assert_eq!(
        reported[0].get("firstName").and_then(Value::as_str),
        Some("fork-one"),
        "the master keeps the first insert"
    );

    // And the second fork adopted the master's state rather than losing its document.
    let resolved = state_two.events().resolved_conflicts();
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        fork_two
            .get("dup")
            .and_then(|doc| doc.get("firstName").cloned()),
        Some(Value::String("fork-one".to_string())),
        "the losing fork holds the winning state, and still holds the document"
    );
}

// ---------------------------------------------------------------------------
// The default handler's contract, directly
// ---------------------------------------------------------------------------

/// The default handler returns the master state unchanged for every shape.
///
/// Called directly rather than through a push, so the assertion is about the handler
/// rather than about the pipeline around it.
#[test]
fn the_default_handler_returns_the_master_for_all_three_shapes() {
    let handler = DefaultConflictHandler;
    let assumed = human("a", "assumed", 1);
    let master = human("a", "master", 2);
    let fork = human("a", "fork", 3);

    for (label, assumed_state) in [
        ("shape 1: no assumption", None),
        ("shape 2: stale assumption", Some(assumed.clone())),
        ("shape 3: assumption equal to the fork", Some(fork.clone())),
    ] {
        let input = dxdb_replication::conflict::ConflictHandlerInput {
            assumed_master_state: assumed_state,
            real_master_state: master.clone(),
            new_document_state: fork.clone(),
        };
        let resolved = handler.resolve(&input, "test");
        assert_eq!(
            resolved, master,
            "{label}: the default handler must return the master state verbatim"
        );
    }
}

/// `isEqual` is equality over the *replicated content*, and the protocol is what removes
/// local bookkeeping before calling it.
///
/// This is the detail easiest to get wrong in both directions. Upstream's `deepEqual`
/// compares the whole object, so a `_rev` difference **is** a difference — but that never
/// arises in practice because both call sites pass the output of
/// `writeDocToDocState(doc, hasAttachments, keepMeta)` with `keepMeta = false`
/// (`upstream.ts:339`, and the master side of the comparison comes from the meta row's
/// `docData`, which `getMetaWriteRow` filled from the same projection). So the contract is a
/// split one: the *handler* is exact, and the *caller* supplies bookkeeping-free states.
///
/// Asserting only one half would leave the other unguarded — a handler that ignored `_rev`
/// would pass a test that fed it projections, and a caller that forgot the projection would
/// pass a test that only exercised the handler.
#[test]
fn is_equal_is_exact_over_the_states_the_protocol_supplies() {
    use dxdb_replication::convert::write_doc_to_doc_state;

    let handler = DefaultConflictHandler;
    let a = human("a", "same", 1);

    // Half 1: given raw stored documents, differing bookkeeping IS a difference. `deepEqual`
    // sees the whole object, which is why the protocol never compares raw documents.
    let mut b = a.clone();
    b.set("_rev", Value::String("99-other".to_string()));
    assert!(
        !handler.is_equal(&a, &b, "raw"),
        "deepEqual is exact: a differing _rev on raw documents is a real difference"
    );

    // Half 2: given the projections the protocol actually passes, the same pair is equal,
    // because neither carries `_rev` or `_meta` any more.
    let a_state = write_doc_to_doc_state(&a, false, false);
    let b_state = write_doc_to_doc_state(&b, false, false);
    assert!(
        handler.is_equal(&a_state, &b_state, "projected"),
        "the protocol compares bookkeeping-free states, so the same content is equal"
    );
    assert!(
        a_state.get("_rev").is_none() && a_state.get("_meta").is_none(),
        "the projection is what removes the bookkeeping"
    );

    // And genuinely different content is unequal in both forms.
    let mut c = a.clone();
    c.set("firstName", Value::String("different".to_string()));
    assert!(!handler.is_equal(&a, &c, "raw"));
    assert!(!handler.is_equal(
        &a_state,
        &write_doc_to_doc_state(&c, false, false),
        "projected"
    ));
}
