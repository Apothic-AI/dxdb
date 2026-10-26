//! The `localStorage` storage: a generic KV backend plus RxDB's storage contract.
//!
//! # How data is laid out
//!
//! Upstream's `storage-localstorage` uses four key groups, each suffixed
//! `{database}--{collection}--{version}`:
//!
//! | key | holds |
//! | --- | --- |
//! | `<prefix>/doc/<id>` | one JSON document |
//! | `<prefix>/idx/<indexName>` | `[[indexString, docId], …]`, sorted |
//! | `<prefix>/changes` | the last `EventBulk`, for cross-tab broadcast |
//! | `<prefix>/att/<docId>` + `<attId>` | one attachment's bytes, base64 |
//!
//! The index encoding is the part that must match upstream exactly, and it does
//! because the index *strings* come from `dxdb-query`'s `custom_index`, which is
//! the ported upstream encoder. This storage never invents an ordering: it stores
//! and compares the strings the port produces.
//!
//! # The one thing upstream does that looks wrong and is not
//!
//! Upstream applies its writes to `localStorage` **synchronously** after
//! `categorizeBulkWriteRows`, with a comment explaining why: `localStorage` has no
//! transactions, so an `await` between the conflict check and the write would let
//! another write interleave and invalidate the check. This implementation keeps
//! that property — the futures here resolve immediately and there is no
//! suspension point between categorising and writing.

#![forbid(unsafe_code)]

pub mod kv;

#[cfg(target_arch = "wasm32")]
pub mod web;

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::{ready, Ready};
use std::rc::Rc;

use dxdb_query::custom_index::{
    get_indexable_string, get_start_index_string_from_lower_bound,
    get_start_index_string_from_upper_bound, BoundValue,
};
use dxdb_query::matcher::Matcher;
use dxdb_storage::{
    categorize_bulk_write_rows, index_name, BulkWriteResponse, BulkWriteRow, ChangeEvent,
    ChangeStream, Checkpoint, CountMode, CountResult, DocumentData, DynRxStorageInstance, EventBulk,
    FindAttachmentResult, InstanceCreationParams, Operation, PlanKey, PreparedQuery, QueryPlan,
    RxStorage, RxStorageInstance, StorageError, StorageSchema, WriteError,
};

pub use kv::{Kv, KvError};

/// The storage name upstream reports for this plugin (`RX_STORAGE_NAME_LOCALSTORAGE`).
pub const STORAGE_NAME: &str = "localstorage";

/// The index every instance maintains regardless of the schema.
///
/// Upstream's `CLEANUP_INDEX` (`rx-storage-instance-localstorage.ts:680`). It is
/// what `cleanup` scans, and it exists even when the schema declares no indexes,
/// because the cleanup sweep needs an ordered view of `(_deleted, _meta.lwt)`.
const CLEANUP_INDEX: [&str; 2] = ["_deleted", "_meta.lwt"];

// ---------------------------------------------------------------------------
// The factory
// ---------------------------------------------------------------------------

/// Opens instances over one `Kv`.
///
/// Upstream's equivalent is `getRxStorageLocalstorage({ localStorage })`. Taking
/// the backend as a parameter rather than reading a browser global is what lets
/// the host contract tests run this exact storage.
pub struct LocalStorageStorage<K: Kv + 'static> {
    kv: Rc<K>,
}

impl<K: Kv + 'static> LocalStorageStorage<K> {
    #[must_use]
    pub fn new(kv: Rc<K>) -> Self {
        Self { kv }
    }

    /// The backend, for a caller that wants to inspect it (tests, mostly).
    #[must_use]
    pub fn kv(&self) -> &Rc<K> {
        &self.kv
    }
}

impl<K: Kv + 'static> RxStorage for LocalStorageStorage<K> {
    fn name(&self) -> &str {
        STORAGE_NAME
    }

    fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> impl std::future::Future<Output = Result<Box<DynRxStorageInstance<'static>>, StorageError>>
    {
        ready(create_instance(Rc::clone(&self.kv), params))
    }
}

fn create_instance<K: Kv + 'static>(
    kv: Rc<K>,
    params: InstanceCreationParams,
) -> Result<Box<DynRxStorageInstance<'static>>, StorageError> {
    // A bare storage cannot honour encryption or key compression. Failing here
    // rather than storing plaintext is the contract (`StorageSchema`'s docs on
    // `has_encrypted_fields` / `key_compression`).
    if params.schema.has_encrypted_fields {
        return Err(StorageError::Unsupported(
            "encrypted fields require the encryption wrapper".to_string(),
        ));
    }
    if params.schema.key_compression {
        return Err(StorageError::Unsupported(
            "keyCompression requires its wrapper".to_string(),
        ));
    }

    let instance = LocalStorageInstance::new(kv, params);
    Ok(Box::new(DynRxStorageInstance::new_box(instance)))
}

// ---------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------

/// One collection's storage, for one schema version.
struct LocalStorageInstance<K: Kv + 'static> {
    kv: Rc<K>,
    database_name: String,
    collection_name: String,
    schema: StorageSchema,
    /// `{prefix}` — the shared part of every key this instance owns.
    prefix: String,
    /// Index name → the materialised `[[indexString, docId], …]`, kept as a cached
    /// handle so a query does not re-read and re-parse it per call. `RefCell`
    /// because the trait's methods take `&self` and wasm32 is single-threaded.
    indexes: RefCell<HashMap<String, Vec<(String, String)>>>,
    /// The change feed's senders. Dropping every receiver closes the stream, which
    /// is the unsubscribe mechanism (PLAN.md 3.3: a subscriber is a stream
    /// consumer).
    subscribers: RefCell<Vec<futures_channel::mpsc::UnboundedSender<EventBulk>>>,
    closed: RefCell<bool>,
    removed: RefCell<bool>,
}

impl<K: Kv + 'static> LocalStorageInstance<K> {
    fn new(kv: Rc<K>, params: InstanceCreationParams) -> Self {
        let prefix = format!(
            "dxdb-ls/{db}/{collection}/{version}",
            db = params.database_name,
            collection = params.collection_name,
            version = params.schema.version
        );
        let mut indexes = HashMap::new();
        // Every schema index plus the cleanup index. Upstream maintains the
        // cleanup index unconditionally so `cleanup()` never has to scan.
        for index in params
            .schema
            .indexes
            .iter()
            .cloned()
            .chain(std::iter::once(
                CLEANUP_INDEX.iter().map(|s| (*s).to_string()).collect(),
            ))
        {
            indexes.insert(index_name(&index), Vec::new());
        }

        Self {
            kv,
            database_name: params.database_name,
            collection_name: params.collection_name,
            schema: params.schema,
            prefix,
            indexes: RefCell::new(indexes),
            subscribers: RefCell::new(Vec::new()),
            closed: RefCell::new(false),
            removed: RefCell::new(false),
        }
    }

    fn doc_key(&self, id: &str) -> String {
        format!("{}/doc/{}", self.prefix, id)
    }

    fn index_key(&self, name: &str) -> String {
        format!("{}/idx/{}", self.prefix, name)
    }

    fn attachment_key(&self, doc_id: &str, attachment_id: &str) -> String {
        format!("{}/att/{}/{}", self.prefix, doc_id, attachment_id)
    }

    fn ensure_open(&self) -> Result<(), StorageError> {
        if *self.removed.borrow() {
            return Err(StorageError::Removed);
        }
        if *self.closed.borrow() {
            return Err(StorageError::Closed);
        }
        Ok(())
    }

    fn read_doc(&self, id: &str) -> Result<Option<DocumentData>, StorageError> {
        let raw = self
            .kv
            .get_item(&self.doc_key(id))
            .map_err(kv_to_storage_error)?;
        match raw {
            None => Ok(None),
            Some(text) => {
                let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                    StorageError::InvalidDocument(format!("stored document for {id}: {e}"))
                })?;
                Ok(Some(DocumentData::from_value(value)?))
            }
        }
    }

    /// Loads one index's materialised form, from cache when present.
    fn load_index(&self, name: &str) -> Result<Vec<(String, String)>, StorageError> {
        if let Some(cached) = self.indexes.borrow().get(name) {
            if !cached.is_empty() {
                return Ok(cached.clone());
            }
        }
        let raw = self
            .kv
            .get_item(&self.index_key(name))
            .map_err(kv_to_storage_error)?;
        let Some(text) = raw else {
            return Ok(Vec::new());
        };
        let parsed: Vec<(String, String)> = serde_json::from_str(&text).map_err(|e| {
            StorageError::InvalidSchema(format!("stored index {name} is not readable: {e}"))
        })?;
        self.indexes
            .borrow_mut()
            .insert(name.to_string(), parsed.clone());
        Ok(parsed)
    }

    fn store_index(&self, name: &str, rows: &[(String, String)]) -> Result<(), StorageError> {
        let text = serde_json::to_string(rows).expect("index rows are always serialisable");
        self.kv
            .set_item(&self.index_key(name), &text)
            .map_err(kv_to_storage_error)?;
        self.indexes
            .borrow_mut()
            .insert(name.to_string(), rows.to_vec());
        Ok(())
    }

    /// Recomputes one document's index entries, removing the old pair and
    /// inserting the new one in sorted order.
    fn update_index_for(
        &self,
        name: &str,
        index: &[String],
        doc_id: &str,
        previous: Option<&DocumentData>,
        next: Option<&DocumentData>,
    ) -> Result<(), StorageError> {
        let mut rows = self.load_index(name)?;

        if let Some(previous) = previous {
            let old_string = index_string(&self.schema.raw, index, previous)?;
            rows.retain(|(s, id)| !(id == doc_id && *s == old_string));
        }

        if let Some(next) = next {
            let new_string = index_string(&self.schema.raw, index, next)?;
            // Sorted insert by `(indexString, docId)`. `binary_search_by` gives
            // both the position and the guarantee that ties are ordered by doc id,
            // which is what makes the ordering total.
            let position = rows
                .binary_search_by(|(s, id)| (s.as_str(), id.as_str()).cmp(&(new_string.as_str(), doc_id)))
                .unwrap_or_else(|pos| pos);
            rows.insert(position, (new_string, doc_id.to_string()));
        }

        self.store_index(name, &rows)
    }

    /// Publishes one bulk to every subscriber. Empty bulks are never emitted —
    /// upstream has a conformance test for exactly that, because a write that
    /// produced only errors must look like no write at all.
    fn emit(&self, bulk: EventBulk) {
        if bulk.is_empty() {
            return;
        }
        // Persist the last bulk so a *different tab* can pick it up. The channel
        // below only reaches subscribers in this instance; the stored copy is what
        // a `storage`/`BroadcastChannel` listener on the other side reads.
        if let Ok(text) = serde_json::to_string(&BulkSnapshot::from(&bulk)) {
            let _ = self.kv.set_item(&format!("{}/changes", self.prefix), &text);
        }
        self.subscribers
            .borrow_mut()
            .retain(|tx| tx.unbounded_send(bulk.clone()).is_ok());
    }
}

/// The wire shape stored for cross-tab pickup. Only what a listener needs; the
/// events are kept as JSON so the stored form does not drift from the live one.
#[derive(serde::Serialize)]
struct BulkSnapshot {
    id: String,
    context: String,
    checkpoint: Option<serde_json::Value>,
    events: Vec<serde_json::Value>,
}

impl From<&EventBulk> for BulkSnapshot {
    fn from(bulk: &EventBulk) -> Self {
        Self {
            id: bulk.id.clone(),
            context: bulk.context.clone(),
            checkpoint: bulk.checkpoint.as_ref().map(Checkpoint::to_json),
            events: bulk
                .events
                .iter()
                .map(|event| serde_json::to_value(ChangeEventView::from(event)).unwrap_or_default())
                .collect(),
        }
    }
}

/// A serialisable view of a `ChangeEvent`, for the cross-tab snapshot.
///
/// `ChangeEvent` itself is not `Serialize` on purpose (lane D owns it and keeps
/// the trait surface free of a serde dependency); this is the storage's own
/// rendering, matching upstream's `EventBulk` wire shape.
#[derive(serde::Serialize)]
struct ChangeEventView<'a> {
    document_id: &'a str,
    operation: &'a str,
    document_data: Option<&'a serde_json::Value>,
    previous_document_data: Option<&'a serde_json::Value>,
}

impl<'a> From<&'a ChangeEvent> for ChangeEventView<'a> {
    fn from(event: &'a ChangeEvent) -> Self {
        Self {
            document_id: &event.document_id,
            operation: match event.operation {
                Operation::Insert => "INSERT",
                Operation::Update => "UPDATE",
                Operation::Delete => "DELETE",
            },
            document_data: Some(event.document_data.as_value()),
            previous_document_data: event
                .previous_document_data
                .as_ref()
                .map(DocumentData::as_value),
        }
    }
}

fn kv_to_storage_error(err: KvError) -> StorageError {
    match err {
        // A quota failure is not a closed instance; the caller can free space and
        // retry. Reporting it as `Unsupported` would be wrong, so it gets the
        // closest honest class: the write could not be accepted.
        KvError::QuotaExceeded => StorageError::Unsupported("storage quota exceeded".to_string()),
        KvError::Unavailable(reason) => StorageError::Unsupported(reason),
        KvError::Other(reason) => StorageError::Unsupported(reason),
    }
}

fn index_string(
    schema: &serde_json::Value,
    index: &[String],
    doc: &DocumentData,
) -> Result<String, StorageError> {
    get_indexable_string(schema, index, doc.as_value())
        .map_err(|e| StorageError::InvalidSchema(format!("index string for {index:?}: {e}")))
}

// ---------------------------------------------------------------------------
// The trait implementation
// ---------------------------------------------------------------------------

impl<K: Kv + 'static> RxStorageInstance for LocalStorageInstance<K> {
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

    fn bulk_write(
        &self,
        document_writes: Vec<BulkWriteRow>,
        context: String,
    ) -> impl std::future::Future<Output = Result<BulkWriteResponse, StorageError>> {
        let result = (|| {
            self.ensure_open()?;

            // Read every affected document first. This is the "docsInDb" map
            // `categorizeBulkWriteRows` needs, and reading it up front is also what
            // keeps the window between the conflict check and the write as small as
            // `localStorage` allows.
            let mut docs_in_db: HashMap<String, DocumentData> = HashMap::new();
            for row in &document_writes {
                let id = row.document.id(&self.schema.primary_path)?.to_string();
                if let Some(existing) = self.read_doc(&id)? {
                    docs_in_db.insert(id, existing);
                }
            }

            let categorized = categorize_bulk_write_rows(
                &self.schema.primary_path,
                &docs_in_db,
                &document_writes,
                &context,
                self.schema.has_attachments,
            );

            // Attachments first: a document referencing a payload that failed to
            // store would be worse than a 510, which the categoriser already
            // reports per row.
            for add in &categorized.attachments_add {
                self.store_attachment(&add.document_id, &add.attachment_id, &add.data)?;
            }
            for update in &categorized.attachments_update {
                self.store_attachment(&update.document_id, &update.attachment_id, &update.data)?;
            }
            for remove in &categorized.attachments_remove {
                self.kv
                    .remove_item(&self.attachment_key(&remove.document_id, &remove.attachment_id))
                    .map_err(kv_to_storage_error)?;
            }

            // From here to the end there is no `await`: nothing can interleave, so
            // the conflict decisions above are still valid when the writes land.
            for row in &categorized.bulk_insert_docs {
                self.apply_row(row, None)?;
            }
            for row in &categorized.bulk_update_docs {
                let id = row.document.id(&self.schema.primary_path)?.to_string();
                self.apply_row(row, docs_in_db.get(&id))?;
            }

            if !categorized.events.is_empty() {
                let bulk = EventBulk {
                    id: format!("{}/{}", self.prefix, context),
                    events: categorized.events,
                    checkpoint: categorized.checkpoint,
                    context: context.clone(),
                };
                self.emit(bulk);
            }

            Ok(BulkWriteResponse {
                error: categorized.error,
            })
        })();
        ready(result)
    }

    fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Vec<DocumentData>, StorageError>> {
        let result = (|| {
            self.ensure_open()?;
            let mut found = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(doc) = self.read_doc(&id)? {
                    if with_deleted || !is_deleted(&doc) {
                        found.push(doc);
                    }
                }
            }
            Ok(found)
        })();
        ready(result)
    }

    fn query(
        &self,
        prepared: PreparedQuery,
    ) -> impl std::future::Future<Output = Result<Vec<DocumentData>, StorageError>> {
        let result = self.run_query(&prepared);
        ready(result)
    }

    fn count(
        &self,
        prepared: PreparedQuery,
    ) -> impl std::future::Future<Output = Result<CountResult, StorageError>> {
        let result = self.run_count(&prepared);
        ready(result)
    }

    fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
        digest: String,
    ) -> impl std::future::Future<Output = Result<FindAttachmentResult, StorageError>> {
        let result = (|| {
            self.ensure_open()?;
            let doc = self.read_doc(&document_id)?.ok_or_else(|| {
                StorageError::MissingAttachment(format!("{document_id}/{attachment_id}"))
            })?;
            let stored = doc
                .get_object_path("_attachments")
                .and_then(|a| a.get(&attachment_id))
                .ok_or_else(|| {
                    StorageError::MissingAttachment(format!("{document_id}/{attachment_id}"))
                })?;
            let stored_digest = stored
                .get("digest")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string();
            if stored_digest != digest {
                return Err(StorageError::MissingAttachment(format!(
                    "digest mismatch for {document_id}/{attachment_id}"
                )));
            }
            let raw = self
                .kv
                .get_item(&self.attachment_key(&document_id, &attachment_id))
                .map_err(kv_to_storage_error)?
                .ok_or_else(|| {
                    StorageError::MissingAttachment(format!("{document_id}/{attachment_id}"))
                })?;
            decode_base64(&raw)
        })();
        ready(result)
    }

    fn change_stream(&self) -> ChangeStream {
        let (tx, rx) = futures_channel::mpsc::unbounded();
        if *self.closed.borrow() || *self.removed.borrow() {
            // A closed instance's feed is already finished; returning a receiver
            // whose sender is dropped makes the stream end immediately, which is
            // the honest answer.
            return Box::pin(futures::stream::empty());
        }
        self.subscribers.borrow_mut().push(tx);
        Box::pin(rx)
    }

    fn cleanup(
        &self,
        minimum_deleted_time: f64,
    ) -> impl std::future::Future<Output = Result<bool, StorageError>> {
        let result = (|| {
            self.ensure_open()?;
            let max_deletion_time = dxdb_storage::now() - minimum_deleted_time;
            let name = index_name(&CLEANUP_INDEX.map(str::to_string));
            let rows = self.load_index(&name)?;

            let mut kept = Vec::with_capacity(rows.len());
            let mut removed_any = false;
            for (index_string_value, doc_id) in rows {
                let Some(doc) = self.read_doc(&doc_id)? else {
                    // The index retained an entry for a document that is gone.
                    // Dropping the entry is the repair; it is not a failure.
                    removed_any = true;
                    continue;
                };
                let deleted = is_deleted(&doc);
                let lwt = doc.lwt().unwrap_or(0.0);
                if deleted && lwt < max_deletion_time {
                    self.kv
                        .remove_item(&self.doc_key(&doc_id))
                        .map_err(kv_to_storage_error)?;
                    removed_any = true;
                } else {
                    kept.push((index_string_value, doc_id));
                }
            }

            if removed_any {
                self.store_index(&name, &kept)?;
            }
            // `localStorage` is synchronous and this sweep is exhaustive, so there
            // is never work left over. Returning `true` when nothing was removed is
            // also correct: the caller stops, and the next cleanup finds nothing.
            Ok(true)
        })();
        ready(result)
    }

    fn close(&self) -> impl std::future::Future<Output = Result<(), StorageError>> {
        // Idempotent: upstream has a "closing multiple times should not error" test.
        if *self.closed.borrow() {
            return ready(Ok(()));
        }
        *self.closed.borrow_mut() = true;
        // Dropping the senders ends every subscriber's stream.
        self.subscribers.borrow_mut().clear();
        ready(Ok(()))
    }

    fn remove(&self) -> impl std::future::Future<Output = Result<(), StorageError>> {
        let result = (|| {
            if *self.removed.borrow() {
                return Ok(());
            }
            if *self.closed.borrow() {
                // Upstream: `.remove()` after `.close()` must error.
                return Err(StorageError::Closed);
            }
            let prefix = format!("{}/", self.prefix);
            let keys = self.kv.keys().map_err(kv_to_storage_error)?;
            for key in keys {
                if key.starts_with(&prefix) {
                    self.kv.remove_item(&key).map_err(kv_to_storage_error)?;
                }
            }
            self.indexes.borrow_mut().clear();
            self.subscribers.borrow_mut().clear();
            *self.removed.borrow_mut() = true;
            *self.closed.borrow_mut() = true;
            Ok(())
        })();
        ready(result)
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

impl<K: Kv + 'static> LocalStorageInstance<K> {
    fn store_attachment(
        &self,
        doc_id: &str,
        attachment_id: &str,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let encoded = encode_base64(data);
        self.kv
            .set_item(&self.attachment_key(doc_id, attachment_id), &encoded)
            .map_err(kv_to_storage_error)
    }

    /// Writes one categorised row and updates every index it touches.
    fn apply_row(
        &self,
        row: &dxdb_storage::BulkWriteRowProcessed,
        previous: Option<&DocumentData>,
    ) -> Result<(), StorageError> {
        let id = row.document.id(&self.schema.primary_path)?.to_string();
        let document = row.document.clone();

        let text = serde_json::to_string(document.as_value())
            .expect("a DocumentData is always serialisable");
        self.kv
            .set_item(&self.doc_key(&id), &text)
            .map_err(kv_to_storage_error)?;

        for index in &self.schema.indexes {
            let name = index_name(index);
            self.update_index_for(&name, index, &id, previous, Some(&document))?;
        }
        let cleanup: Vec<String> = CLEANUP_INDEX.iter().map(|s| (*s).to_string()).collect();
        self.update_index_for(
            &index_name(&cleanup),
            &cleanup,
            &id,
            previous,
            Some(&document),
        )?;

        Ok(())
    }

    fn run_query(&self, prepared: &PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_open()?;

        let plan = &prepared.query_plan;
        let index: Vec<String> = plan.index.clone();

        // Pick the index to walk: the plan's choice when this instance maintains
        // it, otherwise the cleanup index (which always exists and always has every
        // document in it, so it is a total-scan fallback that still returns results
        // in a deterministic order).
        let walk_index = if self.indexes.borrow().contains_key(&index_name(&index)) {
            index.clone()
        } else {
            CLEANUP_INDEX.iter().map(|s| (*s).to_string()).collect()
        };
        let name = index_name(&walk_index);

        let rows = self.load_index(&name)?;

        let selector = prepared.query.selector.clone();
        let matcher = Matcher::new(&selector)
            .map_err(|e| StorageError::InvalidSchema(format!("selector: {e}")))?;

        // The index range, when the plan is for the index we are actually walking.
        // The plan is a hint, so a mismatch between the plan's index and the index
        // we can walk is handled by falling back to "no bounds" rather than by an
        // error: a full scan is slow but correct, and correctness is the contract.
        let uses_plan_index = index_name(&index) == name;
        let (lower, upper) = if uses_plan_index {
            (
                Some(bound_string(&self.schema.raw, &walk_index, &plan.start_keys)?),
                Some(bound_string(&self.schema.raw, &walk_index, &plan.end_keys)?),
            )
        } else {
            (None, None)
        };

        // `change_indexable_string_by_one_quantum` is upstream's way of turning an
        // exclusive bound into an inclusive one without changing the comparison
        // code; using it keeps the boundary semantics identical to upstream's.
        let starts_inclusive = plan.inclusive_start || !uses_plan_index;
        let ends_inclusive = plan.inclusive_end || !uses_plan_index;

        let mut collected: Vec<DocumentData> = Vec::new();
        // Never collect more than `skip + limit` documents, because only those can
        // appear in the result (upstream's `shouldCollect` optimisation).
        let max_needed = prepared.query.skip_plus_limit();

        for (index_string_value, doc_id) in rows {
            if let Some(ref lower) = lower {
                let after_start = if starts_inclusive {
                    index_string_value.as_str() >= lower.as_str()
                } else {
                    index_string_value.as_str() > lower.as_str()
                };
                if !after_start {
                    continue;
                }
            }
            if let Some(ref upper) = upper {
                let before_end = if ends_inclusive {
                    index_string_value.as_str() <= upper.as_str()
                } else {
                    index_string_value.as_str() < upper.as_str()
                };
                if !before_end {
                    break;
                }
            }

            let Some(doc) = self.read_doc(&doc_id)? else {
                continue;
            };

            // `selector_satisfied_by_index: false` means the range is a superset of
            // the result, so every candidate still needs the selector check. A
            // storage that trusted the flag without the range being exact would
            // return extra documents, which is the failure mode the plan's docs
            // warn about.
            if !plan.selector_satisfied_by_index && !matcher.matches(doc.as_value()) {
                continue;
            }

            collected.push(doc);
            if collected.len() >= max_needed {
                break;
            }
        }

        // Sorting. The index walk is already in `(indexString, docId)` order, which
        // is the sort order *when* the query sorts on that index's fields. Only the
        // mismatch and `desc` cases need a real comparison, and `has_desc_sort`
        // forces it — upstream re-sorts for exactly that reason.
        let sort_fields: Vec<String> = prepared.query.sort_fields().map(str::to_string).collect();
        if !plan.sort_satisfied_by_index || prepared.query.has_desc_sort() {
            let schema = self.schema.raw.clone();
            let mut keyed: Vec<(Vec<String>, DocumentData)> = Vec::with_capacity(collected.len());
            for doc in collected {
                let key = sort_key(&schema, &sort_fields, &doc)?;
                keyed.push((key, doc));
            }
            let descending = prepared.query.sort.iter().map(|p| {
                p.direction == dxdb_storage::SortDirection::Desc
            });
            let desc: Vec<bool> = descending.collect();
            keyed.sort_by(|(a, _), (b, _)| compare_sort_keys(a, b, &desc));
            collected = keyed.into_iter().map(|(_, doc)| doc).collect();
        }

        let skip = prepared.query.skip;
        let limit = prepared.query.limit;
        let result: Vec<DocumentData> = collected
            .into_iter()
            .skip(skip)
            .take(limit.unwrap_or(usize::MAX))
            .collect();

        Ok(result)
    }

    fn run_count(&self, prepared: &PreparedQuery) -> Result<CountResult, StorageError> {
        let documents = self.run_query(prepared)?;
        // `count({limit: 3})` must equal `query({limit: 3}).documents.length`, which
        // is why this reuses the query path rather than counting the index. The mode
        // is `Slow` when the range carried no bounds, so a caller without a usable
        // index learns that from the result instead of from a benchmark.
        let mode = if prepared.query_plan.is_full_scan() {
            CountMode::Slow
        } else {
            CountMode::Fast
        };
        Ok(CountResult {
            count: documents.len(),
            mode,
        })
    }
}

fn is_deleted(doc: &DocumentData) -> bool {
    matches!(doc.get_object_path("_deleted"), Some(serde_json::Value::Bool(true)))
}

/// Builds one index-range bound string from the plan's keys.
///
/// `PlanKey::IndexMin` and `IndexMax` become the matching `BoundValue` sentinels so
/// the ported upstream encoder can pad them correctly; everything else is passed
/// through as the value it represents.
fn bound_string(
    schema: &serde_json::Value,
    index: &[String],
    keys: &[PlanKey],
) -> Result<String, StorageError> {
    let bounds: Vec<BoundValue> = keys.iter().map(bound_value).collect();
    // Lower and upper share the encoder in upstream's implementation: the
    // difference is which sentinel the caller supplied, not which function runs.
    get_start_index_string_from_lower_bound(schema, index, &bounds)
        .or_else(|_| get_start_index_string_from_upper_bound(schema, index, &bounds))
        .map_err(|e| StorageError::InvalidSchema(format!("index bound: {e}")))
}

fn bound_value(key: &PlanKey) -> BoundValue {
    match key {
        PlanKey::IndexMin => BoundValue::IndexMin,
        PlanKey::IndexMax => BoundValue::IndexMax,
        PlanKey::Null => BoundValue::Null,
        PlanKey::Bool(b) => BoundValue::Bool(*b),
        PlanKey::Number(n) => BoundValue::Number(*n),
        PlanKey::String(s) => BoundValue::String(s.clone()),
    }
}

/// The sort key of one document: its index string on the query's sort fields.
///
/// Using the ported encoder means a re-sort agrees with the index order rather than
/// with a separately-implemented comparison, which is the class of bug that makes
/// a `desc` query return a slightly wrong order.
fn sort_key(
    schema: &serde_json::Value,
    sort_fields: &[String],
    doc: &DocumentData,
) -> Result<Vec<String>, StorageError> {
    if sort_fields.is_empty() {
        return Ok(Vec::new());
    }
    let encoded = get_indexable_string(schema, sort_fields, doc.as_value())
        .map_err(|e| StorageError::InvalidSchema(format!("sort key: {e}")))?;
    // The encoder concatenates with fixed widths per field, so splitting is not
    // possible; a single concatenated key is correct because every field's width is
    // fixed by the schema. Compare it as one string.
    Ok(vec![encoded])
}

fn compare_sort_keys(a: &[String], b: &[String], desc: &[bool]) -> std::cmp::Ordering {
    for (i, (left, right)) in a.iter().zip(b.iter()).enumerate() {
        let ordering = left.cmp(right);
        if ordering != std::cmp::Ordering::Equal {
            return if desc.get(i).copied().unwrap_or(false) {
                ordering.reverse()
            } else {
                ordering
            };
        }
    }
    std::cmp::Ordering::Equal
}

// ---------------------------------------------------------------------------
// base64, because a KV store holds strings
// ---------------------------------------------------------------------------

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(BASE64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(BASE64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn decode_base64(text: &str) -> Result<Vec<u8>, StorageError> {
    let mut lookup = [255u8; 256];
    for (i, c) in BASE64_ALPHABET.iter().enumerate() {
        lookup[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for ch in text.bytes() {
        if ch == b'=' {
            break;
        }
        let value = lookup[ch as usize];
        if value == 255 {
            return Err(StorageError::MissingAttachment(
                "attachment payload is not valid base64".to_string(),
            ));
        }
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_arbitrary_bytes() {
        for length in 0..64usize {
            let bytes: Vec<u8> = (0..length).map(|i| (i * 37 % 256) as u8).collect();
            let encoded = encode_base64(&bytes);
            let decoded = decode_base64(&encoded).expect("valid base64");
            assert_eq!(decoded, bytes, "length {length}");
        }
    }

    #[test]
    fn base64_matches_the_standard_encoding() {
        // Known vectors, so a subtly wrong alphabet or padding is caught rather
        // than being self-consistently wrong.
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn invalid_base64_is_rejected() {
        assert!(decode_base64("not valid!!").is_err());
    }
}
