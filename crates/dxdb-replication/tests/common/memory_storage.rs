//! THROWAWAY in-memory storage: a real `RxStorageInstance` implementation.
//!
//! REPLACE-WHEN: `dxdb-memory` (lane D) implements `RxStorageInstance`. Then delete
//! this file; every test that uses it switches its constructor and nothing else.
//!
//! # Why this is not the same thing as `double.rs`
//!
//! `double.rs` is a *protocol-shaped* fake: it implements the endpoint adapter the
//! conformance suite is written against, with no storage traits at all. This file is the
//! opposite: a genuine [`RxStorageInstance`], so the real `upstream`/`downstream` code can
//! be exercised against it. The protocol only talks to storage through that trait, so a
//! faithful implementation is enough to run the protocol for real — which is the whole
//! reason the crate's brief says "build against `dxdb-storage`'s traits with a test
//! double".
//!
//! # What is faithful, and what is deliberately minimal
//!
//! Faithful, because the protocol depends on each one:
//!
//! * **Per-document atomic `bulk_write`.** A `previous._rev` mismatch is a
//!   `WriteError::Conflict` carrying `document_in_db`, and the other rows in the same
//!   call still succeed (`PLAN.md` 4.2).
//! * **Events before resolution.** Every event a write causes is queued and the
//!   subscribers woken before `bulk_write` returns.
//! * **`context` on the event bulk**, which is how the upstream recognises and ignores its
//!   own downstream writes.
//! * **`get_changed_documents_since`** in `(_meta.lwt, primary)` order with a checkpoint
//!   that resumes exactly after the last returned document, tombstones included.
//! * **Attachment payloads** kept out of the document tree and addressed by digest, with a
//!   digest mismatch reported as an error rather than served.
//! * **`_rev` discipline**: `{height}-{token}`, height 1 on insert and `previous + 1`
//!   after, and insert-onto-existing is a conflict.
//! * **`underlying_persistent_storage` returning `None`**, which is the normal case.
//!
//! Minimal on purpose: `query` and `count` return [`StorageError::Unsupported`], because
//! there is no selector engine in the tree yet (`dxdb-query` does not exist). Nothing in
//! the replication protocol calls them — `get_changed_documents_since` is implemented
//! natively here, so the trait's default body is not used — and a fixture that faked a
//! selector matcher would be pretending to be a storage it is not.
//!
//! # Sample: what a test looks like
//!
//! ```ignore
//! let store = MemoryStorage::new("db", "human", human_schema(), true);
//! let instance = store.instance();
//! instance.bulk_write(vec![insert_row(doc)], "ctx".to_string()).await;
//! ```

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use dxdb_json::Value;
use dxdb_storage::{
    insert_row, BulkWriteResponse, BulkWriteRow, ChangeEvent, ChangeStream, ChangedDocuments,
    Checkpoint, CountResult, DocumentData, EventBulk, Operation, PreparedQuery, RxStorageInstance,
    StorageError, StorageSchema, WriteError,
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// One subscriber's queue plus its waker.
#[derive(Default)]
struct Subscriber {
    queue: VecDeque<EventBulk>,
    waker: Option<Waker>,
}

#[derive(Default)]
struct StoreState {
    /// Documents by primary value, each at its current state.
    documents: BTreeMap<String, DocumentData>,
    /// Attachment payloads: `(document id, attachment id)` -> bytes.
    payloads: BTreeMap<(String, String), Vec<u8>>,
    /// Per-write event streams.
    subscribers: Vec<Rc<RefCell<Subscriber>>>,
    /// Monotonic counter for `EventBulk::id`, so a duplicate is detectable.
    bulk_sequence: u64,
    closed: bool,
    removed: bool,
    /// The revision token this instance mints. A real storage uses the database
    /// instance token; a fixture uses a fixed string so a test can predict it.
    revision_token: String,
    /// Millisecond component of the last-write clock.
    ///
    /// A storage owns `_meta.lwt`; a document's own value is not trusted. Every write takes
    /// the next tick, so the `(_meta.lwt, primary)` order is total and a checkpoint is a
    /// real position. A fixture that reused the incoming `lwt` would give every document the
    /// same timestamp, and then a change feed ordered by `(lwt, primary)` can report
    /// "nothing after the checkpoint" while a document with a larger primary still sorts
    /// after it -- which is a data-loss bug wearing a passing test's clothes.
    lwt_ms: u64,
    /// Sub-millisecond component, 1..=99, matching upstream's `now()` shape (D4).
    lwt_sub: u8,
    /// Counts of calls, so a test can assert which path ran.
    writes: u64,
}

/// An in-memory storage instance.
///
/// Cheap to clone: clones share the same documents and event subscribers, which is what
/// upstream's multi-instance behaviour looks like.
#[derive(Clone)]
pub struct MemoryStorage {
    inner: Rc<RefCell<StoreState>>,
    database_name: String,
    collection_name: String,
    schema: StorageSchema,
}

impl MemoryStorage {
    /// Builds a store.
    pub fn new(
        database_name: &str,
        collection_name: &str,
        schema: Value,
        revision_token: &str,
    ) -> Self {
        let schema = StorageSchema::from_value(&schema).expect("the fixture schema is valid");
        let inner = StoreState {
            revision_token: revision_token.to_string(),
            // A fixed epoch keeps a test replayable; the increasing ticks matter, the
            // absolute value does not.
            lwt_ms: 1_700_000_000_000,
            lwt_sub: 0,
            ..Default::default()
        };
        MemoryStorage {
            inner: Rc::new(RefCell::new(inner)),
            database_name: database_name.to_string(),
            collection_name: collection_name.to_string(),
            schema,
        }
    }

    /// A second handle onto the same data, as a second instance of one collection.
    pub fn instance(&self) -> Self {
        self.clone()
    }

    /// Every stored document, tombstones included, in primary order.
    pub fn all_documents(&self) -> Vec<DocumentData> {
        self.inner.borrow().documents.values().cloned().collect()
    }

    /// A document's current state.
    pub fn get(&self, id: &str) -> Option<DocumentData> {
        self.inner.borrow().documents.get(id).cloned()
    }

    /// How many `bulk_write` calls this store has served.
    pub fn write_count(&self) -> u64 {
        self.inner.borrow().writes
    }

    /// Records an attachment payload and its stub, without going through the protocol.
    ///
    /// Used to seed a collection with an attachment in a test, which is what upstream's
    /// `doc.putAttachment()` does at the collection layer.
    pub fn put_attachment(
        &self,
        document_id: &str,
        attachment_id: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> String {
        let digest = sha256_hex(bytes);
        let stub = dxdb_storage::AttachmentData {
            length: bytes.len() as u64,
            content_type: content_type.to_string(),
            digest: digest.clone(),
        };
        let mut state = self.inner.borrow_mut();
        state.payloads.insert(
            (document_id.to_string(), attachment_id.to_string()),
            bytes.to_vec(),
        );
        if let Some(document) = state.documents.get_mut(document_id) {
            let attachments = document
                .as_object_mut()
                .entry("_attachments".to_string())
                .or_insert_with(|| Value::Object(dxdb_json::Map::new()));
            if let Value::Object(map) = attachments {
                map.insert(attachment_id.to_string(), stub.to_value());
            }
        }
        digest
    }

    fn primary_path(&self) -> &str {
        &self.schema.primary_path
    }

    /// The next revision, given a borrow the caller already holds.
    ///
    /// Takes `token` rather than reading `self.inner` because `bulk_write` holds a mutable
    /// borrow of that cell for the whole write loop; a second `borrow()` inside it would
    /// panic at runtime. Passing the token keeps one borrow per scope.
    fn revision_for(token: &str, previous: Option<&DocumentData>) -> String {
        dxdb_storage::create_revision(token, previous.and_then(DocumentData::rev))
    }

    fn emit(&self, bulk: EventBulk) {
        let subscribers = self.inner.borrow().subscribers.clone();
        for subscriber in subscribers {
            let waker = {
                let mut slot = subscriber.borrow_mut();
                slot.queue.push_back(bulk.clone());
                slot.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

/// SHA-256, lowercase hex. The same digest upstream's `defaultHashSha256` produces, so a
/// stub written here is byte-comparable with one a JS peer wrote.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn check_usable(state: &StoreState) -> Result<(), StorageError> {
    if state.removed {
        return Err(StorageError::Removed);
    }
    if state.closed {
        return Err(StorageError::Closed);
    }
    Ok(())
}

impl RxStorageInstance for MemoryStorage {
    fn database_name(&self) -> &str {
        &self.database_name
    }

    fn collection_name(&self) -> &str {
        &self.collection_name
    }

    fn schema(&self) -> &StorageSchema {
        &self.schema
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn bulk_write(
        &self,
        document_writes: Vec<BulkWriteRow>,
        context: String,
    ) -> Result<BulkWriteResponse, StorageError> {
        let primary_path = self.primary_path().to_string();
        let mut errors: Vec<WriteError> = Vec::new();
        // One bulk per call, carrying only the rows that actually wrote. Upstream emits
        // no bulk at all when every row failed.
        let mut events: Vec<ChangeEvent> = Vec::new();
        let mut checkpoint: Option<Checkpoint> = None;

        {
            let mut state = self.inner.borrow_mut();
            check_usable(&state)?;
            state.writes += 1;

            for row in &document_writes {
                let write = &row.document;
                let Ok(id) = write.id(&primary_path) else {
                    errors.push(WriteError::Conflict {
                        document_id: String::new(),
                        write_row: row.clone(),
                        document_in_db: None,
                        context: context.clone(),
                    });
                    continue;
                };
                let id = id.to_string();
                let existing = state.documents.get(&id).cloned();

                // Conflict detection: `previous` is what the caller believed was stored.
                let conflict = match (&existing, &row.previous) {
                    (Some(in_db), None) => Some(Some(in_db.clone())),
                    (Some(in_db), Some(previous)) => {
                        if in_db.rev() != previous.rev() {
                            Some(Some(in_db.clone()))
                        } else {
                            None
                        }
                    }
                    (None, Some(_)) => Some(None),
                    (None, None) => None,
                };
                if let Some(document_in_db) = conflict {
                    errors.push(WriteError::Conflict {
                        document_id: id,
                        write_row: row.clone(),
                        document_in_db,
                        context: context.clone(),
                    });
                    continue;
                }

                // Payloads: a stub whose digest changed needs its bytes in this write,
                // otherwise the stored stub would point at data that does not exist.
                let mut stored = write.document().clone();
                for (attachment_id, stub) in stored.attachments().cloned().unwrap_or_default() {
                    let digest = stub
                        .get("digest")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let supplied = write
                        .attachment_payload(attachment_id.as_str())
                        .map(<[u8]>::to_vec);
                    match supplied {
                        Some(bytes) => {
                            // A supplied payload must match the stub it accompanies.
                            let actual = sha256_hex(&bytes);
                            if actual != digest {
                                errors.push(WriteError::Attachment {
                                    document_id: id.clone(),
                                    write_row: row.clone(),
                                    attachment_id: attachment_id.clone(),
                                    document_in_db: existing.clone(),
                                    context: context.clone(),
                                });
                                continue;
                            }
                            state
                                .payloads
                                .insert((id.clone(), attachment_id.clone()), bytes);
                        }
                        None => {
                            // No payload supplied. Upstream's rule
                            // (`rx-storage-helper.ts:385-407`) is narrow: a `510` is an
                            // attachment that is **new relative to the previous document
                            // state** and carries no data. An attachment the stored document
                            // already had may be written stub-only, because its bytes are
                            // already here -- which is exactly what the pull does: it copies
                            // the master's stubs and relies on the local payloads still being
                            // present for the digests that did not change.
                            let existed_before = existing
                                .as_ref()
                                .and_then(|previous| previous.attachment(attachment_id.as_str()))
                                .is_some();
                            let known = state
                                .payloads
                                .contains_key(&(id.clone(), attachment_id.clone()));
                            if !existed_before && !known {
                                errors.push(WriteError::Attachment {
                                    document_id: id.clone(),
                                    write_row: row.clone(),
                                    attachment_id: attachment_id.clone(),
                                    document_in_db: existing.clone(),
                                    context: context.clone(),
                                });
                                continue;
                            }
                        }
                    }
                }

                // Assign the revision and the last-write time, as a storage must. The
                // incoming lwt is ignored: it belongs to whichever instance wrote the
                // document before, and this instance's clock is the one its own change feed
                // is ordered by.
                let revision = Self::revision_for(&state.revision_token, existing.as_ref());
                stored.set("_rev", Value::String(revision));
                state.lwt_sub += 1;
                if state.lwt_sub >= 100 {
                    state.lwt_ms += 1;
                    state.lwt_sub = 1;
                }
                let lwt = state.lwt_ms as f64 + state.lwt_sub as f64 * 0.01;
                stored.set_lwt(lwt)?;

                let operation = match &existing {
                    None => Operation::Insert,
                    Some(previous) => {
                        if previous.is_deleted() && !stored.is_deleted() {
                            Operation::Insert
                        } else if stored.is_deleted() {
                            Operation::Delete
                        } else {
                            Operation::Update
                        }
                    }
                };
                if let Some(lwt) = stored.lwt() {
                    checkpoint = Some(Checkpoint::new(id.clone(), lwt));
                }
                events.push(ChangeEvent {
                    document_id: id.clone(),
                    operation,
                    document_data: stored.clone(),
                    previous_document_data: existing,
                });
                state.documents.insert(id, stored);
            }
        }

        if !events.is_empty() {
            let bulk_id = {
                let mut state = self.inner.borrow_mut();
                state.bulk_sequence += 1;
                format!("bulk-{}", state.bulk_sequence)
            };
            self.emit(EventBulk {
                id: bulk_id,
                events,
                checkpoint,
                context,
            });
        }

        Ok(BulkWriteResponse { error: errors })
    }

    async fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> Result<Vec<DocumentData>, StorageError> {
        let state = self.inner.borrow();
        check_usable(&state)?;
        Ok(ids
            .into_iter()
            .filter_map(|id| state.documents.get(&id).cloned())
            .filter(|document| with_deleted || !document.is_deleted())
            .collect())
    }

    async fn query(&self, _prepared: PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        // No selector engine exists in the tree yet, and the replication protocol never
        // calls this: it uses `get_changed_documents_since`, implemented natively below.
        Err(StorageError::Unsupported(
            "the in-memory fixture does not implement query(); dxdb-query does not exist yet \
         and the replication protocol does not call it"
                .to_string(),
        ))
    }

    async fn count(&self, _prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        Err(StorageError::Unsupported(
            "the in-memory fixture does not implement count(); see query()".to_string(),
        ))
    }

    async fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
        digest: String,
    ) -> Result<Vec<u8>, StorageError> {
        let state = self.inner.borrow();
        check_usable(&state)?;
        let document = state.documents.get(&document_id).ok_or_else(|| {
            StorageError::MissingAttachment(format!("{document_id}/{attachment_id}"))
        })?;
        // The stub is the authority on what digest is expected; a request for any other
        // digest is an error rather than a read of whatever happens to be stored.
        let stub = document.attachment(&attachment_id).ok_or_else(|| {
            StorageError::MissingAttachment(format!("{document_id}/{attachment_id}"))
        })?;
        let stored_digest = stub.get("digest").and_then(Value::as_str).ok_or_else(|| {
            StorageError::MissingAttachment(format!("{document_id}/{attachment_id}"))
        })?;
        if stored_digest != digest {
            return Err(StorageError::MissingAttachment(format!(
                "{document_id}/{attachment_id}: asked for digest {digest}, stored {stored_digest}"
            )));
        }
        state
            .payloads
            .get(&(document_id.clone(), attachment_id.clone()))
            .cloned()
            .ok_or_else(|| {
                StorageError::MissingAttachment(format!("{document_id}/{attachment_id}"))
            })
    }

    async fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<Checkpoint>,
    ) -> Result<ChangedDocuments, StorageError> {
        let state = self.inner.borrow();
        check_usable(&state)?;

        let start = checkpoint.unwrap_or_else(Checkpoint::beginning);
        let mut ordered: Vec<&DocumentData> = state.documents.values().collect();
        // `(_meta.lwt, primary)` ascending, which is the total order the checkpoint
        // resumes over. Primary comparison is JS code-unit order (D5).
        ordered.sort_by(|a, b| {
            let la = a.lwt().unwrap_or(0.0);
            let lb = b.lwt().unwrap_or(0.0);
            la.partial_cmp(&lb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let ia = a
                        .get(self.primary_path())
                        .map(dxdb_json::to_js_json)
                        .unwrap_or_default();
                    let ib = b
                        .get(self.primary_path())
                        .map(dxdb_json::to_js_json)
                        .unwrap_or_default();
                    dxdb_jsstr::compare_js(&ia, &ib)
                })
        });

        let documents: Vec<DocumentData> = ordered
            .into_iter()
            .filter(|document| {
                let lwt = document.lwt().unwrap_or(0.0);
                let id = document
                    .get(self.primary_path())
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if lwt > start.lwt {
                    true
                } else if lwt == start.lwt {
                    // Exclusive at the checkpoint: the checkpoint document itself is not
                    // returned again.
                    dxdb_jsstr::compare_js(id, &start.id) == std::cmp::Ordering::Greater
                } else {
                    false
                }
            })
            .take(limit)
            .cloned()
            .collect();

        let checkpoint = match documents.last() {
            Some(last) => Checkpoint::new(
                last.get(self.primary_path())
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                last.lwt().unwrap_or(start.lwt),
            ),
            None => start,
        };

        Ok(ChangedDocuments {
            documents,
            checkpoint,
        })
    }

    fn change_stream(&self) -> ChangeStream {
        let subscriber = Rc::new(RefCell::new(Subscriber::default()));
        self.inner.borrow_mut().subscribers.push(subscriber.clone());
        Box::pin(ChangeStreamReader { subscriber })
    }

    async fn cleanup(&self, _minimum_deleted_time: f64) -> Result<bool, StorageError> {
        let state = self.inner.borrow();
        check_usable(&state)?;
        // One pass purges everything, so "more work remains" is never true.
        let tombstones: Vec<String> = state
            .documents
            .iter()
            .filter(|(_, document)| document.is_deleted())
            .map(|(id, _)| id.clone())
            .collect();
        drop(state);
        let mut state = self.inner.borrow_mut();
        for id in tombstones {
            state.documents.remove(&id);
            state
                .payloads
                .retain(|(document_id, _), _| document_id != &id);
        }
        Ok(true)
    }

    async fn close(&self) -> Result<(), StorageError> {
        // Closing twice must not error, per upstream's conformance suite.
        let mut state = self.inner.borrow_mut();
        state.closed = true;
        Ok(())
    }

    async fn remove(&self) -> Result<(), StorageError> {
        let mut state = self.inner.borrow_mut();
        if state.closed {
            return Err(StorageError::Closed);
        }
        state.removed = true;
        state.documents.clear();
        state.payloads.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The change stream
// ---------------------------------------------------------------------------

struct ChangeStreamReader {
    subscriber: Rc<RefCell<Subscriber>>,
}

impl futures::Stream for ChangeStreamReader {
    type Item = EventBulk;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut slot = self.subscriber.borrow_mut();
        match slot.queue.pop_front() {
            Some(bulk) => Poll::Ready(Some(bulk)),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Reads everything currently queued on a change stream without blocking.
///
/// The protocol is pumped by the caller (D3: no runtime), so "did anything arrive" has to
/// be a non-blocking question. Polling with a no-op waker leaves a real wake registered
/// for a real executor and returns `Pending` for "nothing yet".
pub fn drain_stream(stream: &mut ChangeStream) -> Vec<EventBulk> {
    let mut out = Vec::new();
    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    loop {
        match stream.as_mut().poll_next(&mut context) {
            Poll::Ready(Some(bulk)) => out.push(bulk),
            Poll::Ready(None) | Poll::Pending => return out,
        }
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

/// Builds a document with the fields every stored document needs.
///
/// The fixture is not a schema validator, so this is a convenience, not a constraint.
pub fn stored_document(primary_path: &str, id: &str, fields: &[(&str, Value)]) -> DocumentData {
    let mut map = dxdb_json::Map::new();
    map.insert(primary_path.to_string(), Value::String(id.to_string()));
    for (key, value) in fields {
        map.insert((*key).to_string(), value.clone());
    }
    map.insert("_deleted".to_string(), Value::Bool(false));
    map.insert(
        "_attachments".to_string(),
        Value::Object(dxdb_json::Map::new()),
    );
    map.insert("_rev".to_string(), Value::String(String::new()));
    let mut meta = dxdb_json::Map::new();
    meta.insert("lwt".to_string(), Value::from(1.0));
    map.insert("_meta".to_string(), Value::Object(meta));
    DocumentData::new(map)
}

/// Wraps a document as an insert row.
pub fn insert(document: DocumentData) -> BulkWriteRow {
    insert_row(document)
}

/// A map of ids to documents, for assertions.
pub fn by_id(documents: &[DocumentData], primary_path: &str) -> HashMap<String, DocumentData> {
    documents
        .iter()
        .filter_map(|document| {
            document
                .get(primary_path)
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), document.clone()))
        })
        .collect()
}
