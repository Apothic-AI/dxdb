//! The five opt-in attachment cases, ported from
//! `src/plugins/test-utils/replication-base-test-suite.ts`.
//!
//! These were skeletons gated on capabilities that did not exist. Each gate is now resolved,
//! and the resolution is recorded per case in `conformance.rs`; the short version:
//!
//! | blocker | resolution |
//! | --- | --- |
//! | digest | `Sha256HashFunction`, SHA-256 lowercase hex |
//! | a digest-verifying payload store | `dxdb_storage::RxStorageInstance::get_attachment_data`, implemented by the storage under test |
//! | the protocol putting payloads on the wire | [`dxdb_replication::attachments::fill_write_data_for_attachments_change`], called from the push |
//! | `put_attachment` / `get_attachment` | **not** a collection-layer prerequisite here: these tests use the storage trait directly, which is what the case bodies' assertions actually check |
//!
//! # The honest note on the last row
//!
//! Upstream's case bodies call `doc.putAttachment(...)` and `att2.getStringData()` on
//! `RxDocument` objects, which do not exist in this port yet (`dxdb-core` has `Document` but
//! no attachment methods). Rather than skip the cases until that lands, the port performs the
//! same **storage-level** operation: write the stub and the payload through `bulk_write`, then
//! read the bytes back through `get_attachment_data`. That is the mechanism the collection
//! layer would use, so a bug in the protocol's payload handling is caught either way; what is
//! *not* covered is the collection layer's argument-handling. The doc comment on each case
//! says which upstream assertions map onto which storage assertion, so nothing is implied to
//! be covered that is not.

mod common {
    pub mod http_transport;
    pub mod memory_storage;
}

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use common::memory_storage::{sha256_hex, stored_document, MemoryStorage};
use dxdb_json::Value;
use dxdb_replication::attachments::fill_write_data_for_attachments_change;
use dxdb_replication::checkpoint::Sha256HashFunction;
use dxdb_replication::conflict::DefaultConflictHandler;
use dxdb_replication::downstream::Downstream;
use dxdb_replication::state::{
    BatchSize, DocumentsWithCheckpoint, InitialCheckpoint, PullStream, ReplicationHandler,
    ReplicationInput, ReplicationState, WriteToMasterRow,
};
use dxdb_replication::upstream::Upstream;
use dxdb_storage::{
    AttachmentData, AttachmentWriteData, DocumentData, DocumentWriteData, RxStorageInstance,
    StorageError,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn attachment_schema() -> Value {
    dxdb_json::parse(
        r#"{
            "version": 0,
            "keyCompression": false,
            "primaryKey": "passportId",
            "type": "object",
            "attachments": {},
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

fn plain_schema() -> Value {
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

fn human(id: &str, first: &str, age: i64) -> DocumentData {
    let mut document = stored_document(
        "passportId",
        id,
        &[
            ("firstName", Value::String(first.to_string())),
            ("age", Value::from(age)),
        ],
    );
    document.set("_rev", Value::String("1-master".to_string()));
    document
}

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
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A master that stores attachment payloads, so the round trip is real in both directions.
///
/// The payload handling is the point: a master that dropped bytes would make an attachment
/// test pass while the protocol never transferred anything. It also enforces the same rule a
/// real master does — a stub whose digest the master cannot resolve is a `510`, not a silent
/// write of a dangling pointer.
#[derive(Default)]
struct AttachmentMaster {
    documents: RefCell<Vec<DocumentData>>,
    payloads: RefCell<BTreeMap<(String, String), Vec<u8>>>,
    clock: RefCell<f64>,
    write_batches: RefCell<Vec<Vec<WriteToMasterRow>>>,
}

impl AttachmentMaster {
    fn new() -> Rc<Self> {
        let master = AttachmentMaster::default();
        *master.clock.borrow_mut() = 100.0;
        Rc::new(master)
    }

    fn stored(&self) -> Vec<DocumentData> {
        self.documents.borrow().clone()
    }

    fn payloads(&self) -> BTreeMap<(String, String), Vec<u8>> {
        self.payloads.borrow().clone()
    }

    fn write_batches(&self) -> Vec<Vec<WriteToMasterRow>> {
        self.write_batches.borrow().clone()
    }
}

/// Puts each attachment's payload into its stub as a byte array, the way a wire document
/// carries it.
///
/// A JS `Blob` is embedded at `_attachments[id].data`; this is the same place, with the bytes
/// as a JSON array. The receiving pull lifts them back out into the storage's payload map.
fn with_payloads(
    document: &DocumentData,
    payloads: &BTreeMap<(String, String), Vec<u8>>,
) -> DocumentData {
    let Some(attachments) = document.attachments() else {
        return document.clone();
    };
    let document_id = document
        .get("passportId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut out = attachments.clone();
    for (attachment_id, stub) in attachments {
        let Some(bytes) = payloads.get(&(document_id.clone(), attachment_id.clone())) else {
            continue;
        };
        let Some(map) = stub.as_object() else {
            continue;
        };
        let mut stub = map.clone();
        stub.insert(
            "data".to_string(),
            Value::Array(bytes.iter().map(|byte| Value::from(*byte as u64)).collect()),
        );
        out.insert(attachment_id.clone(), Value::Object(stub));
    }
    let mut document = document.clone();
    document.set("_attachments", Value::Object(out));
    document
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

impl ReplicationHandler for SharedAttachmentMaster {
    fn master_changes_since(
        &self,
        checkpoint: Option<&dxdb_replication::checkpoint::Checkpoint>,
        batch_size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentsWithCheckpoint, StorageError>>>> {
        let start = checkpoint.cloned();
        let mut documents: Vec<DocumentData> = self.0.documents.borrow().clone();
        documents.sort_by(|a, b| {
            a.lwt()
                .unwrap_or(0.0)
                .partial_cmp(&b.lwt().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let payloads = self.0.payloads.borrow().clone();
        let documents: Vec<DocumentData> = documents
            .into_iter()
            .filter(|document| match &start {
                None => true,
                Some(checkpoint) => {
                    let start_lwt = checkpoint
                        .as_map()
                        .get("lwt")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    let start_id = checkpoint
                        .as_map()
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let lwt = document.lwt().unwrap_or(0.0);
                    let id = document
                        .get("passportId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    lwt > start_lwt
                        || (lwt == start_lwt
                            && dxdb_jsstr::compare_js(id, start_id) == std::cmp::Ordering::Greater)
                }
            })
            .take(batch_size)
            // Every payload for every shipped document, unconditionally. Upstream does this in
            // `masterChangesSince` (`index.ts:241-247`) with the comment "the master never
            // knows the client state of the document. Therefore we always send all attachments
            // data" -- the receiving fork may have none of them, so it cannot be trusted to
            // hold any. Without this the pull delivers stubs whose bytes the fork lacks, and a
            // correct storage rejects the write.
            // Every payload for every shipped document, unconditionally. Upstream does this
            // in `masterChangesSince` (`index.ts:241-247`) with the comment "the master never
            // knows the client state of the document. Therefore we always send all
            // attachments data" -- the receiving fork may have none of them, so none can be
            // assumed. Without it the pull delivers stubs whose bytes the fork lacks, and a
            // correct storage rejects the write with `510`.
            .map(|document| with_payloads(&document, &payloads))
            .collect();
        let checkpoint = documents.last().map(|last| {
            let mut map = dxdb_json::Map::new();
            map.insert(
                "id".to_string(),
                last.get("passportId").cloned().unwrap_or(Value::Null),
            );
            map.insert("lwt".to_string(), Value::from(last.lwt().unwrap_or(0.0)));
            dxdb_replication::checkpoint::Checkpoint::new(Value::Object(map)).expect("object")
        });
        let checkpoint = if documents.is_empty() {
            start
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
        self.0.write_batches.borrow_mut().push(rows.clone());
        let mut conflicts = Vec::new();
        {
            let mut documents = self.0.documents.borrow_mut();
            let mut payloads = self.0.payloads.borrow_mut();
            let mut clock = self.0.clock.borrow_mut();
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

                // Four rules, same as the plain master.
                let accepted = match (&existing, &row.assumed_master_state) {
                    (None, _) => true,
                    (Some(master), None) => {
                        conflicts.push(master.clone());
                        false
                    }
                    (Some(master), Some(assumed)) => {
                        if same_content(master, assumed) {
                            true
                        } else {
                            conflicts.push(master.clone());
                            false
                        }
                    }
                };
                if !accepted {
                    continue;
                }

                // Store the payloads this row carries, verifying each against its stub. A
                // stub whose bytes are absent and unknown is a 510.
                for (attachment_id, stub) in row
                    .new_document_state
                    .attachments()
                    .cloned()
                    .unwrap_or_default()
                {
                    let digest = stub
                        .get("digest")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    match row.attachment_payloads.get(&attachment_id) {
                        Some(bytes) => {
                            if sha256_hex(bytes) != digest {
                                let message = format!(
                                    "row carried bytes for {attachment_id} that do not match \
                                     its stub digest"
                                );
                                return Box::pin(async move {
                                    Err(StorageError::InvalidDocument(message))
                                });
                            }
                            payloads.insert((id.clone(), attachment_id.clone()), bytes.clone());
                        }
                        None => {
                            // No bytes: legal only when the master's existing document already
                            // had this attachment, which mirrors upstream's narrow 510 rule
                            // (`rx-storage-helper.ts:385-407`).
                            let existed_before = existing.as_ref().is_some_and(|document| {
                                document.attachment(attachment_id.as_str()).is_some()
                            });
                            if !existed_before {
                                let message = format!(
                                    "a new attachment {attachment_id} arrived with no payload"
                                );
                                return Box::pin(async move {
                                    Err(StorageError::InvalidDocument(message))
                                });
                            }
                        }
                    }
                }
                // A removed attachment's payload goes too, so a later re-add cannot serve
                // stale bytes.
                if let Some(master_document) = &existing {
                    let now_present: Vec<String> = row
                        .new_document_state
                        .attachments()
                        .map(|attachments| attachments.keys().cloned().collect())
                        .unwrap_or_default();
                    for previously in master_document
                        .attachments()
                        .map(|attachments| attachments.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                    {
                        if !now_present.contains(&previously) {
                            payloads.remove(&(id.clone(), previously));
                        }
                    }
                }

                *clock += 1.0;
                let height = existing
                    .as_ref()
                    .and_then(DocumentData::rev)
                    .and_then(dxdb_storage::try_get_height_of_revision)
                    .unwrap_or(0)
                    + 1;
                let mut document = row.new_document_state.clone();
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
        Box::pin(async move { Ok(conflicts) })
    }

    fn master_change_stream(&self) -> PullStream {
        Box::pin(futures::stream::empty())
    }
}

/// Newtype so the trait can be implemented: `Rc<AttachmentMaster>` is `Rc`, a foreign type,
/// and the orphan rule forbids `impl ForeignTrait for ForeignType<Local>`.
struct SharedAttachmentMaster(Rc<AttachmentMaster>);

/// One side of the replication: fork + meta + state.
struct Side {
    fork: MemoryStorage,
    state: ReplicationState<MemoryStorage>,
}

fn side(master: Rc<AttachmentMaster>, identifier: &str, schema: Value) -> Side {
    let handler: Box<dyn ReplicationHandler> = Box::new(SharedAttachmentMaster(master));
    let fork = MemoryStorage::new("db", "human", schema.clone(), "fork-token");
    let meta_schema =
        dxdb_replication::meta_instance::replication_meta_instance_schema(fork.schema(), false)
            .expect("meta schema");
    let meta = MemoryStorage::new("db", "rx-replication-meta", meta_schema, "meta-token");
    let state = ReplicationState::new(ReplicationInput {
        identifier: identifier.to_string(),
        pull_batch_size: BatchSize::new(100),
        push_batch_size: BatchSize::new(100),
        handler,
        conflict_handler: Box::new(DefaultConflictHandler),
        fork: fork.clone(),
        meta: meta.instance(),
        hash_function: Rc::new(Sha256HashFunction),
        skip_storing_pull_meta: false,
        keep_meta: false,
        initial_checkpoint: InitialCheckpoint::default(),
        wait_before_persist: None,
    });
    Side { fork, state }
}

/// Upstream `putAttachment`: write the stub and the payload in one `bulk_write`.
///
/// This is the storage-level equivalent of the collection's `doc.putAttachment`. The payload
/// travels in `DocumentWriteData::insert_attachment`, which is exactly how a real caller
/// supplies bytes: the document's `_attachments` holds the stub, and the bytes ride beside it.
/// Passing a bare document with a stub and no payload is the `510` case, which is why this
/// helper builds the write data rather than a document.
fn put_attachment(
    fork: &MemoryStorage,
    document_id: &str,
    attachment_id: &str,
    content_type: &str,
    bytes: &[u8],
) -> DocumentData {
    let digest = sha256_hex(bytes);
    let existing = fork.get(document_id).expect("the document exists");
    let stub = AttachmentData {
        length: bytes.len() as u64,
        content_type: content_type.to_string(),
        digest,
    };

    let mut write = DocumentWriteData::new(existing.clone());
    write.insert_attachment(
        attachment_id,
        AttachmentWriteData {
            stub,
            data: Some(bytes.to_vec()),
        },
    );

    let response = block_on(fork.bulk_write(
        vec![dxdb_storage::BulkWriteRow {
            previous: Some(existing),
            document: write,
        }],
        "local".to_string(),
    ))
    .expect("the attachment write succeeds");
    assert!(
        response.is_ok(),
        "putting an attachment must not fail: {:?}",
        response.error
    );
    fork.get(document_id).expect("the document still exists")
}

/// Upstream `getAttachment(id).getStringData()`.
fn attachment_bytes(
    fork: &MemoryStorage,
    document_id: &str,
    attachment_id: &str,
) -> Option<Vec<u8>> {
    let document = fork.get(document_id)?;
    let stub = document.attachment(attachment_id)?;
    let digest = stub.get("digest").and_then(Value::as_str)?;
    block_on(fork.get_attachment_data(
        document_id.to_string(),
        attachment_id.to_string(),
        digest.to_string(),
    ))
    .ok()
}

fn attachment_string(
    fork: &MemoryStorage,
    document_id: &str,
    attachment_id: &str,
) -> Option<String> {
    attachment_bytes(fork, document_id, attachment_id)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
}

// ---------------------------------------------------------------------------
// Case 1: an inserted attachment replicates
// ---------------------------------------------------------------------------

/// Upstream `base test suite > attachment replication > should replicate an inserted
/// attachment`, `replication-base-test-suite.ts:276-299`.
///
/// Upstream's body, and the storage-level equivalent used here:
///
/// | upstream | here |
/// | --- | --- |
/// | `c1`, `c2` from `createAttachments(0)` | two `Side`s over one master, attachment schema |
/// | `doc1.putAttachment({id:'test.txt', data: Blob(['hello replication'])})` | [`put_attachment`] |
/// | `syncOnceWithAttachments(c1)`, then `(c2)` | one push then one pull |
/// | `doc2.getAttachment('test.txt')` exists | the fork's document has the stub |
/// | `await att2.getStringData() === 'hello replication'` | [`attachment_string`] |
#[test]
fn attachment_inserted() {
    let master = AttachmentMaster::new();
    let c1 = side(master.clone(), "repl-att-insert-c1", attachment_schema());
    let c2 = side(master.clone(), "repl-att-insert-c2", attachment_schema());

    // c1 inserts a document, then attaches a payload.
    block_on(c1.fork.bulk_write(
        vec![dxdb_storage::insert_row(human("att-insert-base", "a", 20))],
        "local".to_string(),
    ))
    .expect("the document write succeeds");
    put_attachment(
        &c1.fork,
        "att-insert-base",
        "test.txt",
        "text/plain",
        b"hello replication",
    );

    // sync c1: push the document and its payload.
    let mut upstream = Upstream::new(c1.state.clone());
    block_on(upstream.pump()).expect("the push succeeds");

    // The master must hold the bytes, not just the stub. This is the assertion that would
    // fail if the protocol sent stubs only.
    let payloads = master.payloads();
    assert_eq!(
        payloads.len(),
        1,
        "the master must hold the attachment payload after the push"
    );
    assert_eq!(
        payloads
            .get(&("att-insert-base".to_string(), "test.txt".to_string()))
            .map(|bytes| String::from_utf8_lossy(bytes).to_string()),
        Some("hello replication".to_string()),
        "the bytes must survive the push"
    );

    // sync c2: pull it.
    let mut downstream = Downstream::new(c2.state.clone(), c2.state.identifier_hash());
    block_on(downstream.resync()).expect("the pull succeeds");

    // Upstream: `doc2.getAttachment('test.txt')` must exist and read back the payload.
    let pulled = c2
        .fork
        .get("att-insert-base")
        .expect("the pulled document exists on c2");
    assert!(
        pulled.attachment("test.txt").is_some(),
        "the attachment stub must have replicated to c2, got {}",
        pulled.to_js_json()
    );
    assert_eq!(
        attachment_string(&c2.fork, "att-insert-base", "test.txt").as_deref(),
        Some("hello replication"),
        "the payload must be readable on c2"
    );
}

// ---------------------------------------------------------------------------
// Case 2: an updated attachment replicates
// ---------------------------------------------------------------------------

/// Upstream `base test suite > attachment replication > should replicate an updated
/// attachment`, `replication-base-test-suite.ts:301-334`.
///
/// The property: re-putting an existing attachment id with new bytes must ship the new
/// payload, not be mistaken for "already replicated" because the id is unchanged.
#[test]
fn attachment_updated() {
    let master = AttachmentMaster::new();
    let c1 = side(master.clone(), "repl-att-update-c1", attachment_schema());
    let c2 = side(master.clone(), "repl-att-update-c2", attachment_schema());

    block_on(c1.fork.bulk_write(
        vec![dxdb_storage::insert_row(human("att-update-base", "a", 20))],
        "local".to_string(),
    ))
    .expect("the document write succeeds");
    put_attachment(&c1.fork, "att-update-base", "file.txt", "text/plain", b"v1");

    let mut upstream = Upstream::new(c1.state.clone());
    block_on(upstream.pump()).expect("the first push succeeds");
    let mut downstream = Downstream::new(c2.state.clone(), c2.state.identifier_hash());
    block_on(downstream.resync()).expect("the first pull succeeds");
    assert_eq!(
        attachment_string(&c2.fork, "att-update-base", "file.txt").as_deref(),
        Some("v1"),
        "the first version must arrive"
    );

    // Update on c1: same id, new bytes.
    put_attachment(&c1.fork, "att-update-base", "file.txt", "text/plain", b"v2");
    let mut upstream = Upstream::new(c1.state.clone());
    block_on(upstream.pump()).expect("the update push succeeds");

    // The payloads on the wire for that push: exactly one, and it must be the new bytes.
    let batches = master.write_batches();
    let update_batch = batches.last().expect("an update push happened");
    let update_row = update_batch
        .iter()
        .find(|row| {
            row.new_document_state
                .get("passportId")
                .and_then(Value::as_str)
                == Some("att-update-base")
        })
        .expect("the updated document is in the batch");
    assert_eq!(
        update_row
            .attachment_payloads
            .get("file.txt")
            .map(|bytes| String::from_utf8_lossy(bytes).to_string()),
        Some("v2".to_string()),
        "an updated attachment must carry its new bytes, not be skipped because the id matched"
    );

    let mut downstream = Downstream::new(c2.state.clone(), c2.state.identifier_hash());
    block_on(downstream.resync()).expect("the update pull succeeds");
    assert_eq!(
        attachment_string(&c2.fork, "att-update-base", "file.txt").as_deref(),
        Some("v2"),
        "c2 must read the updated payload"
    );
}

// ---------------------------------------------------------------------------
// Case 3: a deleted attachment replicates
// ---------------------------------------------------------------------------

/// Upstream `base test suite > attachment replication > should replicate a deleted
/// attachment`, `replication-base-test-suite.ts:336-367`.
///
/// The property: removing the last attachment must replicate as a *missing stub*, and the
/// document must survive with an empty `_attachments` object rather than a dangling one.
#[test]
fn attachment_deleted() {
    let master = AttachmentMaster::new();
    let c1 = side(master.clone(), "repl-att-delete-c1", attachment_schema());
    let c2 = side(master.clone(), "repl-att-delete-c2", attachment_schema());

    block_on(c1.fork.bulk_write(
        vec![dxdb_storage::insert_row(human("att-delete-base", "a", 20))],
        "local".to_string(),
    ))
    .expect("the document write succeeds");
    put_attachment(
        &c1.fork,
        "att-delete-base",
        "remove.txt",
        "text/plain",
        b"bye",
    );

    let mut upstream = Upstream::new(c1.state.clone());
    block_on(upstream.pump()).expect("the first push succeeds");
    let mut downstream = Downstream::new(c2.state.clone(), c2.state.identifier_hash());
    block_on(downstream.resync()).expect("the first pull succeeds");
    assert_eq!(
        attachment_string(&c2.fork, "att-delete-base", "remove.txt").as_deref(),
        Some("bye"),
        "the attachment must arrive before it can be deleted"
    );

    // Delete on c1: drop the stub from the document and the payload from storage.
    let existing = c1.fork.get("att-delete-base").expect("document exists");
    let mut document = existing.clone();
    document.set("_attachments", Value::Object(dxdb_json::Map::new()));
    block_on(c1.fork.bulk_write(
        vec![dxdb_storage::update_row(existing, document)],
        "local".to_string(),
    ))
    .expect("the attachment removal write succeeds");

    let mut upstream = Upstream::new(c1.state.clone());
    block_on(upstream.pump()).expect("the removal push succeeds");
    let mut downstream = Downstream::new(c2.state.clone(), c2.state.identifier_hash());
    block_on(downstream.resync()).expect("the removal pull succeeds");

    // Upstream: `doc2.getAttachment('remove.txt') === null`.
    let pulled = c2
        .fork
        .get("att-delete-base")
        .expect("the document still exists on c2");
    assert!(
        pulled.attachment("remove.txt").is_none(),
        "the deleted attachment must not exist on c2, got {}",
        pulled.to_js_json()
    );
    assert!(
        pulled
            .attachments()
            .is_some_and(|attachments| attachments.is_empty()),
        "the document must keep an empty _attachments object, not lose the field"
    );
}

// ---------------------------------------------------------------------------
// Case 4: the master's attachment state wins a conflict
// ---------------------------------------------------------------------------

/// Upstream `base test suite > attachment replication > should keep the master attachment
/// state on conflict`, `replication-base-test-suite.ts:369-418`.
///
/// The most interesting attachment case: the only difference between the two states is
/// *inside* an attachment, so a conflict handler that compared document fields alone would
/// see no conflict at all. The default handler compares the stub digest, sees the difference,
/// and keeps the master.
#[test]
fn attachment_master_state_on_conflict() {
    let master = AttachmentMaster::new();
    let c1 = side(master.clone(), "repl-att-conflict-c1", attachment_schema());
    let c2 = side(master.clone(), "repl-att-conflict-c2", attachment_schema());

    block_on(c1.fork.bulk_write(
        vec![dxdb_storage::insert_row(human(
            "att-conflict-base",
            "a",
            20,
        ))],
        "local".to_string(),
    ))
    .expect("the document write succeeds");
    put_attachment(
        &c1.fork,
        "att-conflict-base",
        "shared.txt",
        "text/plain",
        b"initial",
    );

    // Both sides start from the same attachment state.
    let mut upstream = Upstream::new(c1.state.clone());
    block_on(upstream.pump()).expect("the initial push succeeds");
    let mut downstream = Downstream::new(c2.state.clone(), c2.state.identifier_hash());
    block_on(downstream.resync()).expect("the initial pull succeeds");
    assert_eq!(
        attachment_string(&c2.fork, "att-conflict-base", "shared.txt").as_deref(),
        Some("initial")
    );

    // c2 updates and pushes first, so c2's version becomes the master's.
    put_attachment(
        &c2.fork,
        "att-conflict-base",
        "shared.txt",
        "text/plain",
        b"from c2 - master",
    );
    let mut upstream2 = Upstream::new(c2.state.clone());
    block_on(upstream2.pump()).expect("the c2 push succeeds");

    // c1 updates from its stale base and pushes second: a conflict, and the master wins.
    put_attachment(
        &c1.fork,
        "att-conflict-base",
        "shared.txt",
        "text/plain",
        b"from c1 - should lose",
    );
    let mut upstream1 = Upstream::new(c1.state.clone());
    block_on(upstream1.pump()).expect("the c1 push succeeds");

    // Upstream runs two more rounds "because some backends need additional sync cycles for
    // the conflict-resolved attachment state to propagate" (`:414-416`).
    let mut downstream1 = Downstream::new(c1.state.clone(), c1.state.identifier_hash());
    block_on(downstream1.resync()).expect("the c1 pull succeeds");
    let mut upstream1 = Upstream::new(c1.state.clone());
    block_on(upstream1.pump()).expect("the c1 re-push succeeds");

    // Upstream: `att1Final.getStringData() === 'from c2 - master'`.
    assert_eq!(
        attachment_string(&c1.fork, "att-conflict-base", "shared.txt").as_deref(),
        Some("from c2 - master"),
        "the master's attachment state must win the conflict"
    );
    assert!(
        !master
            .payloads()
            .values()
            .any(|bytes| String::from_utf8_lossy(bytes) == "from c1 - should lose"),
        "the losing payload must never have reached the master"
    );
}

// ---------------------------------------------------------------------------
// Case 5: attachment data does not replicate when attachments are disabled
// ---------------------------------------------------------------------------

/// Upstream `base test suite > attachment replication disabled > should not replicate
/// attachment data when attachments are disabled`,
/// `replication-base-test-suite.ts:424-450`.
///
/// The assertion is deliberately two-sided upstream, and the port keeps that: the *document*
/// must replicate, and if a stub exists at all its bytes must not be the secret. A backend may
/// legitimately strip the stub entirely, so requiring one would be stricter than upstream.
#[test]
fn attachment_disabled_binary_not_replicated() {
    let master = AttachmentMaster::new();
    // A collection whose schema does NOT declare `attachments: {}`, so hasAttachments is
    // false and the protocol forces `_attachments: {}` on the way out.
    let c1 = side(master.clone(), "repl-att-disabled-c1", plain_schema());
    let c2 = side(master.clone(), "repl-att-disabled-c2", plain_schema());

    block_on(c1.fork.bulk_write(
        vec![dxdb_storage::insert_row(human("att-disabled", "a", 20))],
        "local".to_string(),
    ))
    .expect("the document write succeeds");
    // Attach locally anyway: a document can carry a stub even where the collection does not
    // replicate attachments, which is the situation the case describes.
    put_attachment(
        &c1.fork,
        "att-disabled",
        "no-sync.txt",
        "text/plain",
        b"secret",
    );

    let mut upstream = Upstream::new(c1.state.clone());
    block_on(upstream.pump()).expect("the push succeeds");

    // The document reached the master.
    let stored = master.stored();
    assert_eq!(
        stored.len(),
        1,
        "the document itself must replicate even when attachment payloads do not"
    );

    // The payload did not.
    let master_payloads = master.payloads();
    assert!(
        master_payloads.is_empty(),
        "no attachment payload may reach the master when attachments are disabled, got {:?}",
        master_payloads.keys().collect::<Vec<_>>()
    );
    assert!(
        !master_payloads
            .values()
            .any(|bytes| String::from_utf8_lossy(bytes) == "secret"),
        "the secret payload must not have replicated"
    );

    // The document on the master has no usable stub: either none, or one whose bytes are
    // unreachable. Upstream accepts both.
    let master_document = &stored[0];
    if let Some(stub) = master_document.attachment("no-sync.txt") {
        let digest = stub
            .get("digest")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        assert!(
            !master_payloads.contains_key(&("att-disabled".to_string(), "no-sync.txt".to_string())),
            "a stub may be present, but its payload must not be ({digest})"
        );
    }

    // And c2, pulling, never acquires the bytes either.
    let mut downstream = Downstream::new(c2.state.clone(), c2.state.identifier_hash());
    block_on(downstream.resync()).expect("the pull succeeds");
    let pulled = c2.fork.get("att-disabled").expect("the document arrived");
    assert!(
        !pulled
            .attachments()
            .is_some_and(|attachments| attachments.contains_key("no-sync.txt")),
        "the disabled attachment must not appear on the second collection"
    );
}

// ---------------------------------------------------------------------------
// The mechanism, tested directly
// ---------------------------------------------------------------------------

/// `fillWriteDataForAttachmentsChange` sends bytes only for new or changed attachments.
///
/// The performance property the protocol depends on: editing an unrelated field of a
/// document with a large attachment must not re-upload the attachment.
#[test]
fn only_new_or_changed_attachments_carry_payloads() {
    let fork = MemoryStorage::new("db", "human", attachment_schema(), "t");
    block_on(fork.bulk_write(
        vec![dxdb_storage::insert_row(human("doc", "a", 1))],
        "local".to_string(),
    ))
    .expect("write");
    put_attachment(
        &fork,
        "doc",
        "big.bin",
        "application/octet-stream",
        &[7u8; 4096],
    );

    let stored = fork.get("doc").expect("document");
    // The receiver already holds this attachment.
    let filled = block_on(fill_write_data_for_attachments_change(
        &fork,
        "passportId",
        &stored,
        Some(&stored),
    ))
    .expect("fill");
    assert!(
        filled.attachment_payloads().is_empty(),
        "an unchanged attachment must not be re-sent"
    );

    // With no original, everything is sent: "the master never knows the client state".
    let filled = block_on(fill_write_data_for_attachments_change(
        &fork,
        "passportId",
        &stored,
        None,
    ))
    .expect("fill");
    assert_eq!(
        filled.attachment_payloads().len(),
        1,
        "with no assumed master state, every payload must be sent"
    );
    assert_eq!(
        filled.attachment_payloads().get("big.bin").map(Vec::len),
        Some(4096),
        "and it must be the real bytes"
    );

    // A changed digest re-sends.
    put_attachment(
        &fork,
        "doc",
        "big.bin",
        "application/octet-stream",
        &[8u8; 16],
    );
    let changed = fork.get("doc").expect("document");
    let filled = block_on(fill_write_data_for_attachments_change(
        &fork,
        "passportId",
        &changed,
        Some(&stored),
    ))
    .expect("fill");
    assert_eq!(
        filled.attachment_payloads().get("big.bin").map(Vec::len),
        Some(16),
        "a changed digest must be re-sent, with the new size"
    );
}
