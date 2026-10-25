//! The reference in-memory storage.
//!
//! Ported from `src/plugins/storage-memory/`: `rx-storage-instance-memory.ts` (the
//! instance) and `memory-helper.ts` (the index structures). Every other storage is
//! expected to behave identically to this one, so the parts that matter most are the
//! parts a "spiritual port" gets wrong.
//!
//! # The index structure
//!
//! Each materialised index is a `Vec` of `(index string, primary value)` kept sorted
//! by index string, and lookups are binary searches over it. That is exactly
//! upstream's `MemoryStorageInternalsByIndex.docsWithIndex`, and it is why the
//! encoder in `dxdb-query` matters so much here: the array *is* the sort order.
//!
//! **Comparison is code-unit order via [`dxdb_jsstr::compare_js`], everywhere.** The
//! index strings are UTF-16-encoded field values, so two of them must be ordered the
//! way JavaScript's `<` orders them — `memory-helper.ts:337` `compareDocsWithIndex`.
//! Rust's `str` ordering agrees on ASCII and disagrees on astral characters versus
//! BMP characters in `0xE000..=0xFFFF`, which is precisely the case that would pass
//! every hand-written test and corrupt a real dataset (PLAN.md 3.2).
//!
//! # Bulk writes
//!
//! Every decision — insert vs update, conflict, attachment payload rules, the event's
//! operation, the checkpoint — comes from
//! [`dxdb_storage::categorize_bulk_write_rows`], which was verified case-by-case
//! against 600 recorded upstream write batches before this storage existed. This
//! module applies its output; it does not re-derive it. The brief asks for that
//! explicitly, and it is the reason a second storage is cheap to write and easy to
//! review.
//!
//! Per-document atomicity falls out of that: the categorisation rejects per row, and
//! only the accepted rows are applied, so a batch with three conflicts writes the
//! other rows and reports three errors.
//!
//! # Lazy index materialisation is *not* reproduced
//!
//! Upstream defers writing the index until an idle callback, so a `bulkWrite` can
//! resolve before its rows are searchable (`ensurePersistence` +
//! `requestIdlePromise`). That is a DOM-scheduler optimisation with no equivalent in
//! Rust, and reproducing it would mean a query could observably miss a write that
//! already resolved — the opposite of the per-document-atomic contract callers rely
//! on. This storage applies index changes inside `bulk_write`, before it resolves and
//! before it emits. The observable difference is that a read immediately after a
//! write always sees it, which is what the conformance suites' "persist all documents
//! of parallel bulkWrite() calls" test wants anyway.
//!
//! # What is not here yet
//!
//! `query`/`count` currently scan the chosen index range and filter with the
//! matcher; the top-k collector and the `selector_satisfied_by_index` fast path are
//! performance work that lands with the conformance suite. Behaviour is correct
//! either way, because both paths must return the same documents — but `count`
//! reports [`dxdb_storage::CountMode::Fast`] only when it genuinely used the index.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard};

use dxdb_query::matcher::Matcher;
use dxdb_query::plan::{PlanKey as QueryPlanKey, QueryPlan};
use dxdb_query::sort::get_sort_comparator;
use dxdb_storage::{
    categorize_bulk_write_rows, index_name, now, AttachmentData, BulkWriteResponse, BulkWriteRow,
    ChangeEvent, ChangeStream, ChangedDocuments, Checkpoint, CountMode, CountResult, DocumentData,
    DocumentWriteData, DynRxStorageInstance, EventBulk, InstanceCreationParams, Operation,
    PreparedQuery, RxStorage, RxStorageInstance, StorageError, StorageSchema, WriteError,
};
use futures::channel::mpsc;
use serde_json::Value;

/// How many events a change feed buffers before a slow subscriber starts losing
/// bulks.
///
/// The feed is lossy on purpose: a subscriber that cannot keep up must not be able to
/// stall writes, because a stalled write is a stalled database. Replication does not
/// depend on the feed for correctness — it resumes from checkpoints via
/// `get_changed_documents_since` — so a dropped bulk costs latency, not data.
const CHANGE_FEED_BUFFER: usize = 1024;

/// A document plus the primary value it is stored under.
#[derive(Clone, Debug)]
struct Row {
    /// The stored document. Ordered JSON; attachments hold stubs only.
    document: DocumentData,
}

impl Row {
    fn lwt(&self) -> f64 {
        self.document.lwt().unwrap_or(0.0)
    }
}

/// One materialised index: a sorted array of `(index string, primary value)`.
///
/// Sorted by index string in code-unit order, with the primary value stored alongside
/// so a document can be found in the `documents` map without re-deriving it.
#[derive(Default)]
struct IndexData {
    /// `(index string, primary value)`, sorted by the string.
    entries: Vec<(String, String)>,
}

impl IndexData {
    /// Inserts or replaces one document's entry.
    ///
    /// An existing entry for the same primary value is replaced in place when the
    /// index string did not change (upstream's documented performance shortcut) and
    /// removed then re-inserted when it did.
    fn put(&mut self, index_string: String, primary: &str) {
        if let Some(position) = self.position_of_primary(primary, &index_string) {
            self.entries[position] = (index_string, primary.to_string());
            return;
        }
        // Binary search over the *strings* to find the insertion point. Equivalent to
        // upstream's `pushAtSortPosition` with `compareDocsWithIndex`.
        let at = self.lower_bound(&index_string);
        self.entries.insert(at, (index_string, primary.to_string()));
    }

    /// Removes one document's entry, if present.
    fn remove(&mut self, index_string: &str, primary: &str) {
        if let Some(position) = self.position_of_primary(primary, index_string) {
            self.entries.remove(position);
        }
    }

    /// The first position whose index string is `>= index_string`.
    fn lower_bound(&self, index_string: &str) -> usize {
        let mut low = 0usize;
        let mut high = self.entries.len();
        while low < high {
            let middle = low + (high - low) / 2;
            // Code-unit order, not `str` order: see the module docs.
            if dxdb_jsstr::compare_js(&self.entries[middle].0, index_string)
                == std::cmp::Ordering::Less
            {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// The first position whose index string is strictly `> index_string`.
    fn upper_bound(&self, index_string: &str) -> usize {
        let mut low = 0usize;
        let mut high = self.entries.len();
        while low < high {
            let middle = low + (high - low) / 2;
            if dxdb_jsstr::compare_js(&self.entries[middle].0, index_string)
                == std::cmp::Ordering::Greater
            {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        low
    }

    /// Finds an entry for one primary value among the entries that share its index
    /// string. Several documents can share an index string when the schema does not
    /// end its index in the primary key, so the neighbours are checked too.
    fn position_of_primary(&self, primary: &str, index_string: &str) -> Option<usize> {
        let start = self.lower_bound(index_string);
        let mut position = start;
        while position < self.entries.len() {
            let (candidate, candidate_primary) = &self.entries[position];
            if dxdb_jsstr::compare_js(candidate, index_string) != std::cmp::Ordering::Equal {
                break;
            }
            if candidate_primary == primary {
                return Some(position);
            }
            position += 1;
        }
        None
    }
}

/// Shared state for one `(database, collection, schema version)`.
///
/// Shared between every instance opened on the same triple, which is what makes
/// upstream's multi-instance tests pass: two instances see one set of documents and
/// both receive each other's change events.
#[derive(Default)]
struct Internals {
    /// Every document, keyed by primary value.
    documents: HashMap<String, Row>,
    /// One sorted index per schema index, keyed by `index_name`.
    indexes: HashMap<String, IndexData>,
    /// Attachment payloads, keyed by `(primary value, attachment id)`.
    attachments: HashMap<(String, String), Vec<u8>>,
    /// Change-feed subscribers for this collection.
    subscribers: Vec<mpsc::Sender<EventBulk>>,
    /// Reference count so the state is dropped when the last instance closes.
    ref_count: usize,
    /// Set by `remove()`; every other instance on the collection then fails.
    removed: bool,
    /// The schema the state was created with, for the dev-mode mismatch check.
    schema_version: u32,
    /// Highest `lwt` emitted so far, so a checkpoint never goes backwards.
    last_lwt: f64,
}

/// Process-wide registry of collection state, keyed like
/// `getMemoryCollectionKey` (`memory-helper.ts:18`).
static REGISTRY: Mutex<Option<HashMap<String, Arc<Mutex<Internals>>>>> = Mutex::new(None);

fn registry() -> MutexGuard<'static, Option<HashMap<String, Arc<Mutex<Internals>>>>> {
    match REGISTRY.lock() {
        Ok(guard) => guard,
        // A previous holder panicked while holding the registry. The map is a plain
        // `HashMap` of `Arc`s with no partial-update hazard, so recovering the guard is
        // safe and better than poisoning every later write. `forbid(unsafe_code)`
        // rules out anything cleverer.
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock(state: &Arc<Mutex<Internals>>) -> MutexGuard<'_, Internals> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Counter for change-bulk ids, mirroring upstream's `nextEventBulkId`.
static EVENT_BULK_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_event_bulk_id() -> String {
    format!(
        "memory-{}",
        EVENT_BULK_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
    )
}

/// The in-memory storage engine.
#[derive(Clone, Default)]
pub struct MemoryStorage;

impl MemoryStorage {
    /// A new engine handle. Instances opened through it share state with every other
    /// handle in the process whose `(database, collection, version)` matches.
    pub fn new() -> Self {
        MemoryStorage
    }
}

impl RxStorage for MemoryStorage {
    fn name(&self) -> &str {
        "memory"
    }

    fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> impl std::future::Future<Output = Result<Box<DynRxStorageInstance<'static>>, StorageError>>
    {
        async move {
            // Upstream refuses a schema whose key compression or encryption is not
            // handled by a wrapper. A bare storage that accepted either would store
            // plaintext and un-compressed keys while the caller believed otherwise.
            if params.schema.key_compression {
                return Err(StorageError::Unsupported(
                    "schema sets keyCompression but the key-compression wrapper is not in the stack"
                        .to_string(),
                ));
            }
            if params.schema.has_encrypted_fields {
                return Err(StorageError::Unsupported(
                    "schema declares encrypted fields but the encryption wrapper is not in the stack"
                        .to_string(),
                ));
            }

            let collection_key = params.collection_key();
            let state = {
                let mut guard = registry();
                let map = guard.get_or_insert_with(HashMap::new);
                match map.get(&collection_key) {
                    Some(existing) => {
                        let mut internals = lock(existing);
                        // `devMode` catches the hard-to-debug case of two instances
                        // disagreeing about the schema they are sharing.
                        if params.dev_mode && internals.schema_version != params.schema.version {
                            return Err(StorageError::InvalidSchema(format!(
                                "storage was already created with schema version {}",
                                internals.schema_version
                            )));
                        }
                        internals.ref_count += 1;
                        Arc::clone(existing)
                    }
                    None => {
                        let mut internals = Internals {
                            schema_version: params.schema.version,
                            ref_count: 1,
                            ..Default::default()
                        };
                        for index in &params.schema.indexes {
                            internals
                                .indexes
                                .insert(index_name(index), IndexData::default());
                        }
                        // The cleanup index. Upstream's memory storage adds it itself
                        // (`memory-indexes.ts:12`) rather than relying on the schema,
                        // because `cleanup` needs a `(_deleted, _meta.lwt, primary)`
                        // range even when the schema declares no such index.
                        let cleanup_index = vec![
                            "_deleted".to_string(),
                            "_meta.lwt".to_string(),
                            params.schema.primary_path.clone(),
                        ];
                        internals
                            .indexes
                            .entry(index_name(&cleanup_index))
                            .or_default();
                        let state = Arc::new(Mutex::new(internals));
                        map.insert(collection_key.clone(), Arc::clone(&state));
                        state
                    }
                }
            };

            let instance = MemoryStorageInstance {
                database_name: params.database_name.clone(),
                collection_name: params.collection_name.clone(),
                schema: params.schema.clone(),
                state,
                collection_key,
                closed: Mutex::new(false),
            };
            Ok(DynRxStorageInstance::new_box(instance))
        }
    }
}

/// One instance on a collection.
///
/// Cloning is not supported on purpose: two instances are created through
/// [`MemoryStorage::create_storage_instance`] so the reference count stays right.
pub struct MemoryStorageInstance {
    database_name: String,
    collection_name: String,
    schema: StorageSchema,
    state: Arc<Mutex<Internals>>,
    collection_key: String,
    closed: Mutex<bool>,
}

impl MemoryStorageInstance {
    /// The dot paths of every index this instance materialised.
    pub fn index_names(&self) -> Vec<String> {
        let internals = lock(&self.state);
        internals.indexes.keys().cloned().collect()
    }

    /// Number of stored documents, including tombstones. Exposed for tests and for
    /// the dev-mode integrity checks core performs.
    pub fn stored_document_count(&self) -> usize {
        lock(&self.state).documents.len()
    }

    fn ensure_usable(&self) -> Result<(), StorageError> {
        let closed = match self.closed.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        };
        if closed {
            return Err(StorageError::Closed);
        }
        if lock(&self.state).removed {
            return Err(StorageError::Removed);
        }
        Ok(())
    }

    /// Encodes a document against one schema index.
    fn index_string(&self, index: &[String], document: &DocumentData) -> Result<String, StorageError> {
        dxdb_query::custom_index::get_indexable_string(
            &self.schema.raw,
            index,
            document.as_value(),
        )
        .map_err(|error| StorageError::InvalidSchema(error.to_string()))
    }

    /// Every index a stored document participates in, as `(name, index string)`.
    fn index_strings(&self, document: &DocumentData) -> Result<Vec<(String, String)>, StorageError> {
        let mut out = Vec::new();
        for index in &self.schema.indexes {
            out.push((index_name(index), self.index_string(index, document)?));
        }
        // The cleanup index is materialised whether or not the schema declares it.
        let cleanup = vec![
            "_deleted".to_string(),
            "_meta.lwt".to_string(),
            self.schema.primary_path.clone(),
        ];
        out.push((index_name(&cleanup), self.index_string(&cleanup, document)?));
        Ok(out)
    }
}

impl RxStorageInstance for MemoryStorageInstance {
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
        self.ensure_usable()?;
        let primary_path = self.schema.primary_path.clone();
        let has_attachments = self.schema.has_attachments;

        // Snapshot the stored state for every id the batch mentions. Taking the
        // snapshot before the loop is what makes two rows for one document both see
        // the pre-batch state, matching upstream.
        let snapshot: HashMap<String, DocumentData> = {
            let internals = lock(&self.state);
            document_writes
                .iter()
                .filter_map(|row| row.document.id(&primary_path).ok().map(str::to_string))
                .filter_map(|id| {
                    internals
                        .documents
                        .get(&id)
                        .map(|row| (id, row.document.clone()))
                })
                .collect()
        };

        // Every decision comes from the verified pure function.
        let categorised = categorize_bulk_write_rows(
            &primary_path,
            &snapshot,
            &document_writes,
            &context,
            has_attachments,
        );

        // Apply the accepted rows, then emit. Both under one lock so a reader cannot
        // observe the write before its event, which is what the conformance suite's
        // "emit exactly one event on write" checks by reading the feed immediately
        // after awaiting `bulk_write`.
        let mut feed = Vec::new();
        {
            let mut internals = lock(&self.state);
            for row in &categorised.bulk_insert_docs {
                self.apply_row(&mut internals, row.document.clone())?;
            }
            for row in &categorised.bulk_update_docs {
                self.apply_row(&mut internals, row.document.clone())?;
            }
            for add in &categorised.attachments_add {
                internals
                    .attachments
                    .insert((add.document_id.clone(), add.attachment_id.clone()), add.data.clone());
            }
            for update in &categorised.attachments_update {
                internals.attachments.insert(
                    (update.document_id.clone(), update.attachment_id.clone()),
                    update.data.clone(),
                );
            }
            for remove in &categorised.attachments_remove {
                internals
                    .attachments
                    .remove(&(remove.document_id.clone(), remove.attachment_id.clone()));
            }
            // A tombstone drops its payloads: the row's `_attachments` is empty, so a
            // payload left behind would be unreachable but still readable by id.
            for row in categorised
                .bulk_update_docs
                .iter()
                .filter(|row| row.document.is_deleted())
            {
                if let Ok(id) = row.document.id(&primary_path) {
                    internals.attachments.retain(|(document_id, _), _| document_id != id);
                }
            }

            if !categorised.events.is_empty() {
                if let Some(checkpoint) = &categorised.checkpoint {
                    internals.last_lwt = internals.last_lwt.max(checkpoint.lwt);
                }
                let bulk = EventBulk {
                    id: next_event_bulk_id(),
                    events: categorised.events.clone(),
                    checkpoint: categorised.checkpoint.clone(),
                    context: context.clone(),
                };
                // Queue for subscribers outside this lock; draining here would let a
                // slow channel receiver hold the storage lock.
                feed.push(bulk);
            }
        }

        // Emit to every subscriber. `try_send` on a bounded channel: a subscriber that
        // is full loses this bulk rather than blocking the write, which is the
        // documented behaviour (see `CHANGE_FEED_BUFFER`).
        if !feed.is_empty() {
            let mut internals = lock(&self.state);
            let subscribers = std::mem::take(&mut internals.subscribers);
            let mut still_open = Vec::with_capacity(subscribers.len());
            for mut sender in subscribers {
                for bulk in &feed {
                    if sender.try_send(bulk.clone()).is_err() {
                        // Full or closed. Closed means the receiver was dropped, so the
                        // sender is dead; full means slow, and dropping the bulk is
                        // preferable to stalling a write.
                        if sender.is_closed() {
                            break;
                        }
                    }
                }
                if !sender.is_closed() {
                    still_open.push(sender);
                }
            }
            internals.subscribers = still_open;
        }

        Ok(categorised.response())
    }

    async fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_usable()?;
        let internals = lock(&self.state);
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = internals.documents.get(&id) {
                if with_deleted || !row.document.is_deleted() {
                    out.push(row.document.clone());
                }
            }
        }
        Ok(out)
    }

    async fn query(&self, prepared: PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_usable()?;
        let internals = lock(&self.state);

        // Convert the storage ABI's plan into the query crate's plan. This is the D8
        // boundary: exactly one conversion, in one place.
        let plan = storage_plan_to_query_plan(&prepared.query_plan)?;
        let index = plan.index.clone();
        let index_name = index_name(&index);
        let index_data = internals.indexes.get(&index_name).ok_or_else(|| {
            StorageError::UnknownIndex(index_name.clone())
        })?;

        // Bound strings, from the plan's keys rather than from the selector.
        let lower = bound_string(
            &self.schema.raw,
            &index,
            &plan.start_keys,
            RangeSide::Lower,
        )?;
        let upper = bound_string(&self.schema.raw, &index, &plan.end_keys, RangeSide::Upper)?;

        let start = if plan.inclusive_start {
            index_data.lower_bound(&lower)
        } else {
            index_data.upper_bound(&lower)
        };
        let end_exclusive = if plan.inclusive_end {
            index_data.upper_bound(&upper)
        } else {
            index_data.lower_bound(&upper)
        };

        let mut documents: Vec<DocumentData> = Vec::new();
        if start < end_exclusive {
            // The selector still has to run unless the plan proves the range is exact.
            let matcher = if plan.selector_satisfied_by_index {
                None
            } else {
                Some(
                    Matcher::new(&prepared.query.selector)
                        .map_err(|error| StorageError::InvalidDocument(error.to_string()))?,
                )
            };
            for (_, primary) in &index_data.entries[start..end_exclusive] {
                if let Some(row) = internals.documents.get(primary) {
                    let keep = match &matcher {
                        Some(matcher) => matcher.matches(row.document.as_value()),
                        None => true,
                    };
                    if keep {
                        documents.push(row.document.clone());
                    }
                }
            }
        }

        // Re-sort when the index order is not the requested order. Always ascending
        // index order, because storages do not support descending indexes
        // (`query-planner.ts:48`).
        if !plan.sort_satisfied_by_index {
            let comparator = get_sort_comparator(&lift_query(&prepared.query));
            documents.sort_by(|a, b| comparator.compare(a.as_value(), b.as_value()));
        }

        let skip = prepared.query.skip;
        let limit = prepared.query.limit;
        let documents = match limit {
            Some(limit) => documents
                .into_iter()
                .skip(skip)
                .take(limit)
                .collect::<Vec<_>>(),
            None => documents.into_iter().skip(skip).collect::<Vec<_>>(),
        };
        Ok(documents)
    }

    async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        self.ensure_usable()?;
        let internals = lock(&self.state);
        let plan = storage_plan_to_query_plan(&prepared.query_plan)?;
        let index = plan.index.clone();
        let index_name = index_name(&index);
        let index_data = internals
            .indexes
            .get(&index_name)
            .ok_or_else(|| StorageError::UnknownIndex(index_name.clone()))?;

        let lower = bound_string(
            &self.schema.raw,
            &index,
            &plan.start_keys,
            RangeSide::Lower,
        )?;
        let upper = bound_string(&self.schema.raw, &index, &plan.end_keys, RangeSide::Upper)?;
        let start = if plan.inclusive_start {
            index_data.lower_bound(&lower)
        } else {
            index_data.upper_bound(&lower)
        };
        let end_exclusive = if plan.inclusive_end {
            index_data.upper_bound(&upper)
        } else {
            index_data.lower_bound(&upper)
        };
        let in_range = end_exclusive.saturating_sub(start);

        // `count` must equal `query().len()` *including with skip and limit*, which
        // upstream spells out: `.count({limit: 3})` equals
        // `.query({limit: 3}).documents.length`.
        let skip = prepared.query.skip;
        if plan.selector_satisfied_by_index {
            let available = in_range.saturating_sub(skip);
            let count = match prepared.query.limit {
                Some(limit) => available.min(limit),
                None => available,
            };
            return Ok(CountResult {
                count,
                // The index range was the answer; no documents were materialised.
                mode: CountMode::Fast,
            });
        }

        // Otherwise filter, which means walking the range. Still `Fast` from upstream's
        // point of view because it is an index range rather than a full collection
        // scan; the `Slow` mode upstream reports is reserved for storages that cannot
        // count at all.
        let matcher = Matcher::new(&prepared.query.selector)
            .map_err(|error| StorageError::InvalidDocument(error.to_string()))?;
        let mut matched = 0usize;
        let mut skipped = 0usize;
        for (_, primary) in &index_data.entries[start..end_exclusive] {
            if let Some(row) = internals.documents.get(primary) {
                if matcher.matches(row.document.as_value()) {
                    if skipped < skip {
                        skipped += 1;
                        continue;
                    }
                    if let Some(limit) = prepared.query.limit {
                        if matched == limit {
                            break;
                        }
                    }
                    matched += 1;
                }
            }
        }
        Ok(CountResult {
            count: matched,
            mode: CountMode::Fast,
        })
    }

    async fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
        digest: String,
    ) -> Result<Vec<u8>, StorageError> {
        self.ensure_usable()?;
        let internals = lock(&self.state);
        let key = (document_id.clone(), attachment_id.clone());
        let data = internals
            .attachments
            .get(&key)
            .ok_or_else(|| StorageError::MissingAttachment(format!("{document_id}||{attachment_id}")))?;

        // The digest is how the caller knows the bytes are the bytes it asked for, so a
        // mismatch is an error rather than something to reconcile.
        let stored_digest = internals
            .documents
            .get(&document_id)
            .and_then(|row| row.document.attachment(&attachment_id).cloned())
            .and_then(|stub| {
                AttachmentData::from_value(&stub).map(|attachment| attachment.digest)
            });
        match stored_digest {
            Some(stored) if stored == digest => Ok(data.clone()),
            Some(stored) => Err(StorageError::MissingAttachment(format!(
                "{document_id}||{attachment_id}: digest {stored} does not match {digest}"
            ))),
            None => Err(StorageError::MissingAttachment(format!(
                "{document_id}||{attachment_id}: not declared on the document"
            ))),
        }
    }

    // No `get_changed_documents_since` override, deliberately.
    //
    // Upstream's memory storage does not implement it either
    // (`src/plugins/storage-memory/rx-storage-instance-memory.ts` has no such method),
    // so it runs the documented fallback in `rx-storage-helper.ts:936`: a
    // `(_meta.lwt, primary)`-ordered query, checkpointing on the last document. The
    // trait's default body is that fallback, exactly, so leaving it unimplemented is
    // the faithful port — and a storage that overrode it with a native ordering would
    // be *faster than upstream and less tested*, which is the wrong trade for the
    // reference implementation.

    fn change_stream(&self) -> ChangeStream {
        let (sender, receiver) = mpsc::channel(CHANGE_FEED_BUFFER);
        lock(&self.state).subscribers.push(sender);
        Box::pin(receiver)
    }

    async fn cleanup(&self, minimum_deleted_time: f64) -> Result<bool, StorageError> {
        self.ensure_usable()?;
        let primary_path = self.schema.primary_path.clone();
        let mut internals = lock(&self.state);

        // `(_deleted, _meta.lwt, primary)` range starting just above
        // `(true, 0, "")`, exactly upstream's bounds, so tombstones are visited in
        // deletion-time order.
        let cleanup_index = vec![
            "_deleted".to_string(),
            "_meta.lwt".to_string(),
            primary_path.clone(),
        ];
        let name = index_name(&cleanup_index);
        let lower = bound_string(
            &self.schema.raw,
            &cleanup_index,
            &[
                QueryPlanKey::Bool(true),
                QueryPlanKey::Number(0.0),
                QueryPlanKey::String(String::new()),
            ],
            RangeSide::Lower,
        )?;
        let cutoff = now() - minimum_deleted_time;

        // Collect first, then remove, so the borrow of the index does not overlap the
        // mutation.
        let victims: Vec<String> = {
            let Some(index_data) = internals.indexes.get(&name) else {
                return Ok(true);
            };
            let start = index_data.upper_bound(&lower);
            let mut victims = Vec::new();
            for (_, primary) in &index_data.entries[start..] {
                let Some(row) = internals.documents.get(primary) else {
                    continue;
                };
                if row.lwt() > cutoff {
                    // Ordered by lwt, so everything after this is newer.
                    break;
                }
                victims.push(primary.clone());
            }
            victims
        };

        for primary in &victims {
            if let Some(row) = internals.documents.get(primary) {
                let document = row.document.clone();
                let strings = self.index_strings(&document)?;
                for (name, index_string) in strings {
                    if let Some(index_data) = internals.indexes.get_mut(&name) {
                        index_data.remove(&index_string, primary);
                    }
                }
            }
            internals.documents.remove(primary);
            internals
                .attachments
                .retain(|(document_id, _), _| document_id != primary);
        }

        // `true` means "all cleanable documents have been removed". This storage
        // removes everything eligible in one pass, so it always has more work only if
        // a future change makes it batch.
        Ok(true)
    }

    async fn close(&self) -> Result<(), StorageError> {
        {
            let mut closed = match self.closed.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Closing twice must not error (upstream's "closing multiple times should
            // not error").
            if *closed {
                return Ok(());
            }
            *closed = true;
        }
        let should_drop = {
            let mut internals = lock(&self.state);
            internals.ref_count = internals.ref_count.saturating_sub(1);
            internals.ref_count == 0
        };
        if should_drop {
            // Drop the shared state only if nobody else is using it. `removed` state
            // is dropped too, so a later `create` starts clean.
            let mut guard = registry();
            if let Some(map) = guard.as_mut() {
                if let Some(state) = map.get(&self.collection_key) {
                    // Only remove the entry if it is the same allocation we hold.
                    if Arc::ptr_eq(state, &self.state) {
                        map.remove(&self.collection_key);
                    }
                }
            }
        }
        Ok(())
    }

    async fn remove(&self) -> Result<(), StorageError> {
        // `remove` after `close` must error; `close` after `remove` must not.
        {
            let closed = match self.closed.lock() {
                Ok(guard) => *guard,
                Err(poisoned) => *poisoned.into_inner(),
            };
            if closed {
                return Err(StorageError::Closed);
            }
        }
        // Mark removed so every other instance on this collection fails, exactly as
        // upstream's `ensureNotRemoved` does.
        {
            let mut internals = lock(&self.state);
            internals.removed = true;
            internals.documents.clear();
            internals.attachments.clear();
            for index in internals.indexes.values_mut() {
                index.entries.clear();
            }
        }
        self.close().await
    }
}

impl MemoryStorageInstance {
    /// Applies one stored document to the document map and every index.
    fn apply_row(
        &self,
        internals: &mut Internals,
        document: DocumentData,
    ) -> Result<(), StorageError> {
        let primary = document
            .id(&self.schema.primary_path)
            .map_err(|error| StorageError::InvalidDocument(error.to_string()))?
            .to_string();
        let strings = self.index_strings(&document)?;
        // Remove the old entry from every index the document had, which may be a
        // different set from the new one only if the index strings moved; the index
        // *names* are fixed by the schema, so re-putting covers both cases.
        if let Some(previous) = internals.documents.get(&primary) {
            let previous = previous.document.clone();
            let previous_strings = self.index_strings(&previous)?;
            for (name, index_string) in previous_strings {
                if let Some(index_data) = internals.indexes.get_mut(&name) {
                    index_data.remove(&index_string, &primary);
                }
            }
        }
        for (name, index_string) in strings {
            let index_data = internals.indexes.entry(name).or_default();
            index_data.put(index_string, &primary);
        }
        internals.documents.insert(
            primary,
            Row {
                document: document.clone(),
            },
        );
        Ok(())
    }
}

/// Which side of a range a bound string is for.
#[derive(Clone, Copy)]
enum RangeSide {
    Lower,
    Upper,
}

/// Builds a bound string from plan keys.
fn bound_string(
    schema: &Value,
    index: &[String],
    keys: &[QueryPlanKey],
    side: RangeSide,
) -> Result<String, StorageError> {
    let bounds: Vec<dxdb_query::custom_index::BoundValue> = keys
        .iter()
        .map(|key| dxdb_query::custom_index::BoundValue::from_json(Some(&key.to_json())))
        .collect();
    let encoded = match side {
        RangeSide::Lower => {
            dxdb_query::custom_index::get_start_index_string_from_lower_bound(schema, index, &bounds)
        }
        RangeSide::Upper => {
            dxdb_query::custom_index::get_start_index_string_from_upper_bound(schema, index, &bounds)
        }
    };
    encoded.map_err(|error| StorageError::InvalidSchema(error.to_string()))
}

/// Converts the storage-side plan into the query crate's plan.
///
/// This is the D8 boundary: `dxdb-storage`'s `QueryPlan` is the ABI a wrapper stack
/// sees, and `dxdb-query`'s is what the encoder and comparator functions consume. The
/// conversion is total — every storage-side key has exactly one query-side
/// counterpart — and it is the only place the two live types meet.
fn storage_plan_to_query_plan(
    plan: &dxdb_storage::QueryPlan,
) -> Result<QueryPlan, StorageError> {
    Ok(QueryPlan {
        index: plan.index.clone(),
        start_keys: plan.start_keys.iter().map(lift_key).collect(),
        end_keys: plan.end_keys.iter().map(lift_key).collect(),
        inclusive_start: plan.inclusive_start,
        inclusive_end: plan.inclusive_end,
        sort_satisfied_by_index: plan.sort_satisfied_by_index,
        selector_satisfied_by_index: plan.selector_satisfied_by_index,
    })
}

/// Lifts one storage-side bound key into the query crate's key type.
///
/// The sentinels are lifted by *value*, not by variant name, so a caller that passed
/// the literal `MIN_SAFE_INTEGER` or the literal `U+FFFF` string gets the same
/// behaviour as one that used the named sentinel — which is what upstream's `===`
/// comparisons do.
fn lift_key(key: &dxdb_storage::PlanKey) -> QueryPlanKey {
    match key {
        dxdb_storage::PlanKey::String(text) => QueryPlanKey::from_value(Some(&Value::String(
            text.clone(),
        ))),
        dxdb_storage::PlanKey::Number(number) => QueryPlanKey::from_value(Some(
            &serde_json::Number::from_f64(*number)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        )),
        dxdb_storage::PlanKey::Boolean(flag) => {
            QueryPlanKey::from_value(Some(&Value::Bool(*flag)))
        }
        dxdb_storage::PlanKey::Unbounded => QueryPlanKey::from_value(None),
    }
}

/// Lifts the storage ABI's `MangoQuery` into the query crate's filled query, so the
/// comparator and matcher take the same input as everywhere else.
fn lift_query(query: &dxdb_storage::MangoQuery) -> dxdb_query::normalize::FilledMangoQuery {
    dxdb_query::normalize::FilledMangoQuery {
        selector: query.selector.clone(),
        sort: query
            .sort
            .iter()
            .map(|part| dxdb_query::normalize::SortPart {
                field: part.field.clone(),
                direction: match part.direction {
                    dxdb_storage::SortDirection::Asc => dxdb_query::normalize::SortDirection::Asc,
                    dxdb_storage::SortDirection::Desc => dxdb_query::normalize::SortDirection::Desc,
                },
            })
            .collect(),
        skip: query.skip as u64,
        limit: query.limit.map(|limit| limit as u64),
        index: query.index.clone(),
    }
}

/// Re-exported so a caller can name the event type without depending on
/// `dxdb-storage` directly.
pub use dxdb_storage::Operation as ChangeOperation;

/// Re-exported alongside [`ChangeOperation`].
pub use dxdb_storage::ChangeEvent as ChangeEventAlias;

/// The `(primary, attachment)` key type used by the payload map.
pub type AttachmentKey = (String, String);

/// Payload map type, exposed for a caller that wants to inspect stored attachments.
pub type AttachmentMap = BTreeMap<AttachmentKey, Vec<u8>>;

/// Names the write-error type so a caller matching on a `bulk_write` result does not
/// have to import it from `dxdb-storage` separately.
pub use dxdb_storage::WriteError as WriteErrorAlias;


