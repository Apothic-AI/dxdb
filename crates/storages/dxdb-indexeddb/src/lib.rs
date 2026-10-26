//! `dxdb-indexeddb`: IndexedDB storage for dxdb.
//!
//! # What makes this a real IndexedDB storage and not a KV store with extra steps
//!
//! Two things, both from the lane brief, and both about *how* the data is read
//! rather than what is stored:
//!
//! 1. **Custom index strings, not native IndexedDB indexes.** Every schema index is
//!    materialised as an ordered `[indexString, primaryKey]` array under one object
//!    store. The index strings come from `dxdb-query`'s ported `custom_index`
//!    encoder, so ordering is identical to every other dxdb storage and to upstream.
//!    A native IDB index would sort by IDB's own comparison rules, which differ from
//!    RxDB's code-unit ordering, so it is deliberately unused.
//! 2. **Batched cursor reads via `getAll` over explicit key ranges.** A query
//!    materialises its bound strings, then reads the matching slice of the index
//!    array in *batches* of `batchSize` (default 300, the documented upstream
//!    option) with `IDBObjectStore.getAll(range, count)` rather than walking a cursor
//!    one record at a time. `getAll` is one round trip per batch.
//!
//! # Layout
//!
//! One IndexedDB database per `(databaseName, collectionName)`, named
//! `dxdb--{db}--{collection}`, with ascending schema versions. Inside it:
//!
//! | object store | key | value |
//! | --- | --- | --- |
//! | `docs` | primary value | the stored document (payloads stripped) |
//! | `idx` | `"{indexName}|{indexString}|{primary}"` | nothing (the key *is* the index) |
//! | `att` | `"{primary}||{attachmentId}"` | base64 payload |
//! | `meta` | `"schema"` | the schema version + hash this store was created for |
//!
//! The index object store is keyed by a composite string rather than holding one
//! record per index array, precisely so that a range read is a single `getAll` on
//! sorted keys: IDB orders string keys by its own code-unit comparison, which for
//! these keys is the ordering we want, because the encoded index string is the
//! prefix and IDB compares strings by code unit.
//!
//! # Versioning
//!
//! The IDB database version *is* the schema version plus one. Upstream's IndexedDB
//! storage keeps schema versions in separate databases; this keeps them in one
//! database with a monotonic `version`, and puts the schema hash in `meta` so a
//! mismatched hash on re-open is a loud error rather than silently mixing documents
//! written by two different schemas. That is the same guarantee, reached differently,
//! and the conformance suite's "should not mix up documents stored with different
//! schema versions" case is what checks it.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use serde_json::Value;

use dxdb_query::custom_index::{
    get_indexable_string, get_start_index_string_from_lower_bound,
    get_start_index_string_from_upper_bound, BoundValue,
};
use dxdb_query::matcher::Matcher;
use dxdb_query::plan::QueryPlan as EncoderPlan;
use dxdb_query::sort::get_sort_comparator;
use dxdb_storage::{
    categorize_bulk_write_rows, index_name, BulkWriteResponse, BulkWriteRow, ChangeStream,
    CountMode, CountResult, DocumentData, DynRxStorageInstance, EventBulk, FindAttachmentResult,
    InstanceCreationParams, PreparedQuery, RxStorage, RxStorageInstance, StorageError,
    StorageSchema,
};

pub mod backing;

#[cfg(feature = "conformance")]
pub mod conformance;

pub use backing::{BackingStore, IdbError, MemoryBacking};

/// The storage name upstream reports (`RX_STORAGE_NAME_INDEXEDDB`).
pub const STORAGE_NAME: &str = "indexeddb";

/// Rows read per `getAll` call. Upstream documents `batchSize` with a default of
/// 300 and a warning to only change it with measurements in hand, so it is an
/// option with that default rather than a constant.
pub const DEFAULT_BATCH_SIZE: usize = 300;

/// The index name every instance materialises for `cleanup`, matching upstream's
/// `CLEANUP_INDEX` (`rx-storage-instance-localstorage.ts:680` shares the shape).
const CLEANUP_INDEX: [&str; 3] = ["_deleted", "_meta.lwt", ""];

// ---------------------------------------------------------------------------
// The factory
// ---------------------------------------------------------------------------

/// Opens instances over one backing store.
pub struct IndexedDbStorage<B: BackingStore + 'static> {
    backing: Rc<B>,
    batch_size: usize,
}

impl<B: BackingStore + 'static> IndexedDbStorage<B> {
    #[must_use]
    pub fn new(backing: Rc<B>) -> Self {
        Self {
            backing,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// Overrides the `getAll` batch size (`batchSize` upstream).
    #[must_use]
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    #[must_use]
    pub fn backing(&self) -> &Rc<B> {
        &self.backing
    }
}

impl<B: BackingStore + 'static> RxStorage for IndexedDbStorage<B> {
    fn name(&self) -> &str {
        STORAGE_NAME
    }

    fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> impl Future<Output = Result<Box<DynRxStorageInstance<'static>>, StorageError>> {
        let backing = Rc::clone(&self.backing);
        let batch_size = self.batch_size;
        async move {
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
            let instance = IndexedDbInstance::open(backing, params, batch_size).await?;
            Ok(DynRxStorageInstance::new_box(instance))
        }
    }
}

// ---------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------

struct IndexedDbInstance<B: BackingStore + 'static> {
    backing: Rc<B>,
    /// The IDB database name, which carries the schema version so two versions of
    /// one collection never share records.
    store_name: String,
    database_name: String,
    collection_name: String,
    schema: StorageSchema,
    batch_size: usize,
    subscribers: RefCell<Vec<futures_channel::mpsc::UnboundedSender<EventBulk>>>,
    closed: RefCell<bool>,
    removed: RefCell<bool>,
}

impl<B: BackingStore + 'static> IndexedDbInstance<B> {
    async fn open(
        backing: Rc<B>,
        params: InstanceCreationParams,
        batch_size: usize,
    ) -> Result<Self, StorageError> {
        let store_name = format!(
            "dxdb--{db}--{collection}--v{version}",
            db = params.database_name,
            collection = params.collection_name,
            version = params.schema.version
        );
        let instance = Self {
            backing,
            store_name,
            database_name: params.database_name,
            collection_name: params.collection_name,
            schema: params.schema,
            batch_size,
            subscribers: RefCell::new(Vec::new()),
            closed: RefCell::new(false),
            removed: RefCell::new(false),
        };
        instance
            .backing
            .open(&instance.store_name)
            .await
            .map_err(to_storage_error)?;
        Ok(instance)
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

    /// The index name for one field list, plus the encoded string for one document.
    fn encode(&self, index: &[String], doc: &DocumentData) -> Result<String, StorageError> {
        get_indexable_string(&self.schema.raw, index, doc.as_value())
            .map_err(|e| StorageError::InvalidSchema(format!("index string for {index:?}: {e}")))
    }

    /// Every index this instance maintains: the schema's, plus cleanup.
    fn maintained_indexes(&self) -> Vec<Vec<String>> {
        let mut all = self.schema.indexes.clone();
        let cleanup: Vec<String> = CLEANUP_INDEX
            .iter()
            .map(|field| {
                if field.is_empty() {
                    self.schema.primary_path.clone()
                } else {
                    (*field).to_string()
                }
            })
            .collect();
        if !all.contains(&cleanup) {
            all.push(cleanup);
        }
        all
    }

    fn emit(&self, bulk: EventBulk) {
        if bulk.is_empty() {
            return;
        }
        self.subscribers
            .borrow_mut()
            .retain(|tx| tx.unbounded_send(bulk.clone()).is_ok());
    }
}

impl<B: BackingStore + 'static> RxStorageInstance for IndexedDbInstance<B> {
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
        self.ensure_open()?;

        // Read the affected documents first: `categorize_bulk_write_rows` decides
        // conflict-vs-update from them, and it must see the *stored* state.
        let mut ids = Vec::with_capacity(document_writes.len());
        for row in &document_writes {
            ids.push(row.document.id(&self.schema.primary_path)?.to_string());
        }
        let found = self
            .backing
            .get_docs(&self.store_name, &ids)
            .await
            .map_err(to_storage_error)?;
        let docs_in_db: HashMap<String, DocumentData> = found
            .into_iter()
            .filter_map(|value| {
                DocumentData::from_value(value)
                    .ok()
                    .map(|doc| (ids_for(&doc, &self.schema).unwrap_or_default(), doc))
            })
            .collect();

        let categorized = categorize_bulk_write_rows(
            &self.schema.primary_path,
            &docs_in_db,
            &document_writes,
            &context,
            self.schema.has_attachments,
        );

        // Attachment payloads first, so a document never points at bytes that are
        // not there.
        let mut attachments: Vec<(String, Vec<u8>)> = Vec::new();
        for add in &categorized.attachments_add {
            attachments.push((
                attachment_key(&add.document_id, &add.attachment_id),
                add.data.clone(),
            ));
        }
        for update in &categorized.attachments_update {
            attachments.push((
                attachment_key(&update.document_id, &update.attachment_id),
                update.data.clone(),
            ));
        }
        if !attachments.is_empty() {
            self.backing
                .put_attachments(&self.store_name, &attachments)
                .await
                .map_err(to_storage_error)?;
        }
        let removals: Vec<String> = categorized
            .attachments_remove
            .iter()
            .map(|remove| attachment_key(&remove.document_id, &remove.attachment_id))
            .collect();
        if !removals.is_empty() {
            self.backing
                .delete_attachments(&self.store_name, &removals)
                .await
                .map_err(to_storage_error)?;
        }

        // Documents and their index entries go in one transaction, so a crash
        // cannot leave an index pointing at a document that is not there.
        let mut docs: Vec<(String, Value)> = Vec::new();
        let mut index_puts: Vec<(String, String)> = Vec::new();
        let mut index_deletes: Vec<String> = Vec::new();
        let indexes = self.maintained_indexes();

        for row in categorized
            .bulk_insert_docs
            .iter()
            .chain(categorized.bulk_update_docs.iter())
        {
            let id = row.document.id(&self.schema.primary_path)?.to_string();
            if let Some(previous) = &row.previous {
                for index in &indexes {
                    let name = index_name(index);
                    let old = self.encode(index, previous)?;
                    index_deletes.push(index_row_key(&name, &old, &id));
                }
            }
            for index in &indexes {
                let name = index_name(index);
                let encoded = self.encode(index, &row.document)?;
                index_puts.push((index_row_key(&name, &encoded, &id), id.clone()));
            }
            docs.push((id, row.document.as_value().clone()));
        }

        if !docs.is_empty() || !index_puts.is_empty() || !index_deletes.is_empty() {
            self.backing
                .commit(&self.store_name, docs, index_puts, index_deletes)
                .await
                .map_err(to_storage_error)?;
        }

        if !categorized.events.is_empty() {
            self.emit(EventBulk {
                id: format!("{}/{}", self.store_name, context),
                events: categorized.events,
                checkpoint: categorized.checkpoint,
                context: context.clone(),
            });
        }

        Ok(BulkWriteResponse {
            error: categorized.error,
        })
    }

    async fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_open()?;
        let found = self
            .backing
            .get_docs(&self.store_name, &ids)
            .await
            .map_err(to_storage_error)?;
        let mut out = Vec::with_capacity(found.len());
        for value in found {
            let doc = DocumentData::from_value(value)?;
            if with_deleted || !is_deleted(&doc) {
                out.push(doc);
            }
        }
        Ok(out)
    }

    async fn query(&self, prepared: PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_open()?;
        let plan = encoder_plan(&prepared)?;
        let index = plan.index.clone();
        let name = index_name(&index);

        // The plan's index must be one this instance maintains. Returning
        // `UnknownIndex` rather than falling back is what upstream does, and the
        // conformance suite depends on the error.
        if !self
            .maintained_indexes()
            .iter()
            .any(|candidate| index_name(candidate) == name)
        {
            return Err(StorageError::UnknownIndex(name));
        }

        let lower = bound_string(
            &self.schema.raw,
            &index,
            &prepared.query_plan.start_keys,
            Side::Lower,
        )?;
        let upper = bound_string(
            &self.schema.raw,
            &index,
            &prepared.query_plan.end_keys,
            Side::Upper,
        )?;

        // The batched read: keys in `[lower, upper)`, `batchSize` at a time, via
        // `getAll`. This is the brief's "batched cursor reads with getAll over
        // explicit key ranges".
        // The plan's start keys are `[lwt, primary]`; the primary is the tail of the
        // exclusive bound. Taken from the storage ABI's plan rather than from the
        // encoded string, because the encoded string is a fixed-width concatenation and
        // the primary is not recoverable from it without the schema.
        let checkpoint_primary: Option<String> =
            prepared
                .query_plan
                .start_keys
                .last()
                .and_then(|key| match key {
                    dxdb_storage::PlanKey::String(text) => Some(text.clone()),
                    _ => None,
                });
        let start = index_cursor(
            &name,
            &lower,
            plan.inclusive_start,
            checkpoint_primary.as_deref(),
        );
        let total_needed = prepared.query.skip_plus_limit();
        let mut primary_keys: Vec<String> = Vec::new();
        let mut cursor = start;
        loop {
            let batch = self
                .backing
                .get_index_batch(&self.store_name, &name, &cursor, self.batch_size)
                .await
                .map_err(to_storage_error)?;
            if batch.is_empty() {
                break;
            }
            let mut exhausted = false;
            // Take the last key before consuming the batch; the loop below moves
            // the pairs out of it.
            let last_key = batch.last().map(|(key, _)| key.clone());
            for (key, primary) in batch {
                // Stop at the upper bound. Compared in Rust rather than via an IDB
                // range so the comparison is the same code-unit comparison the
                // encoder used, not IDB's.
                if let Some(encoded) = key_index_string(&key) {
                    if plan.inclusive_end {
                        if encoded.as_str() > upper.as_str() {
                            exhausted = true;
                            break;
                        }
                    } else if encoded.as_str() >= upper.as_str() {
                        exhausted = true;
                        break;
                    }
                }
                primary_keys.push(primary);
                if primary_keys.len() >= total_needed {
                    exhausted = true;
                    break;
                }
            }
            if exhausted {
                break;
            }
            let Some(last_key) = last_key else {
                break;
            };
            cursor = format!("{last_key}\u{0}");
        }

        // Fetch the documents for the keys, in index order.
        let values = self
            .backing
            .get_docs(&self.store_name, &primary_keys)
            .await
            .map_err(to_storage_error)?;
        let by_id: HashMap<String, DocumentData> = values
            .into_iter()
            .filter_map(|value| {
                DocumentData::from_value(value)
                    .ok()
                    .map(|doc| (ids_for(&doc, &self.schema).unwrap_or_default(), doc))
            })
            .collect();
        let mut documents: Vec<DocumentData> = primary_keys
            .iter()
            .filter_map(|id| by_id.get(id).cloned())
            .collect();

        // The selector still has to run unless the plan proves the range is exact.
        if !plan.selector_satisfied_by_index {
            let matcher = Matcher::new(&prepared.query.selector)
                .map_err(|e| StorageError::InvalidSchema(format!("selector: {e}")))?;
            documents.retain(|doc| matcher.matches(doc.as_value()));
        }

        if !plan.sort_satisfied_by_index {
            let comparator = get_sort_comparator(&lift_query(&prepared.query));
            documents.sort_by(|a, b| comparator.compare(a.as_value(), b.as_value()));
        }

        let skip = prepared.query.skip;
        Ok(documents
            .into_iter()
            .skip(skip)
            .take(prepared.query.limit.unwrap_or(usize::MAX))
            .collect())
    }

    async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        // `count()` must equal `query().len()`, including with a limit, so it is
        // computed the same way. The mode reports whether the range was bounded,
        // which is what tells a caller their count has no index.
        let full_scan = prepared.query_plan.is_full_scan();
        let documents = self.query(prepared.clone()).await?;
        Ok(CountResult {
            count: documents.len(),
            mode: if full_scan {
                CountMode::Slow
            } else {
                CountMode::Fast
            },
        })
    }

    async fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
        digest: String,
    ) -> Result<FindAttachmentResult, StorageError> {
        self.ensure_open()?;
        let doc = self
            .backing
            .get_docs(&self.store_name, std::slice::from_ref(&document_id))
            .await
            .map_err(to_storage_error)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                StorageError::MissingAttachment(format!("{document_id}||{attachment_id}"))
            })?;
        let doc = DocumentData::from_value(doc)?;
        let stored = doc
            .get_object_path("_attachments")
            .and_then(|a| a.get(&attachment_id))
            .and_then(|stub| stub.get("digest"))
            .and_then(Value::as_str);
        match stored {
            Some(stored) if stored == digest => {}
            Some(stored) => {
                return Err(StorageError::MissingAttachment(format!(
                    "{document_id}||{attachment_id}: digest {stored} != {digest}"
                )))
            }
            None => {
                return Err(StorageError::MissingAttachment(format!(
                    "{document_id}||{attachment_id}: not declared on the document"
                )))
            }
        }
        self.backing
            .get_attachment(
                &self.store_name,
                &attachment_key(&document_id, &attachment_id),
            )
            .await
            .map_err(to_storage_error)?
            .ok_or_else(|| {
                StorageError::MissingAttachment(format!("{document_id}||{attachment_id}"))
            })
    }

    fn change_stream(&self) -> ChangeStream {
        if *self.closed.borrow() || *self.removed.borrow() {
            return Box::pin(futures::stream::empty());
        }
        let (tx, rx) = futures_channel::mpsc::unbounded();
        self.subscribers.borrow_mut().push(tx);
        Box::pin(rx)
    }

    async fn cleanup(&self, minimum_deleted_time: f64) -> Result<bool, StorageError> {
        self.ensure_open()?;
        let cleanup_index: Vec<String> = CLEANUP_INDEX
            .iter()
            .map(|field| {
                if field.is_empty() {
                    self.schema.primary_path.clone()
                } else {
                    (*field).to_string()
                }
            })
            .collect();
        let name = index_name(&cleanup_index);
        let cutoff = dxdb_storage::now() - minimum_deleted_time;

        // `(_deleted, _meta.lwt, primary)` starting just above
        // `(true, 0, "")` — upstream's exact bounds — so tombstones are visited
        // in deletion-time order and the walk can stop at the cutoff.
        let lower = bound_string(
            &self.schema.raw,
            &cleanup_index,
            &[
                dxdb_storage::PlanKey::Boolean(true),
                dxdb_storage::PlanKey::Number(0.0),
                dxdb_storage::PlanKey::String(String::new()),
            ],
            Side::Lower,
        )?;

        let mut victims: Vec<String> = Vec::new();
        let mut cursor = index_cursor(&name, &lower, false, None);
        'outer: loop {
            let batch = self
                .backing
                .get_index_batch(&self.store_name, &name, &cursor, self.batch_size)
                .await
                .map_err(to_storage_error)?;
            if batch.is_empty() {
                break;
            }
            for (key, primary) in &batch {
                let Some(encoded) = key_index_string(key) else {
                    continue;
                };
                // The encoded `_meta.lwt` sits inside the index string; decode the
                // cutoff comparison from the document instead of the key, because
                // the key's encoding is field-width dependent.
                let Some(value) = self
                    .backing
                    .get_docs(&self.store_name, std::slice::from_ref(primary))
                    .await
                    .map_err(to_storage_error)?
                    .into_iter()
                    .next()
                else {
                    continue;
                };
                let doc = DocumentData::from_value(value)?;
                let lwt = doc.lwt().unwrap_or(0.0);
                if lwt >= cutoff {
                    // Tombstones are in lwt order, so everything after is newer.
                    let _ = encoded;
                    break 'outer;
                }
                victims.push(primary.clone());
            }
            let Some((last_key, _)) = batch.last() else {
                break;
            };
            cursor = format!("{last_key}\u{0}");
        }

        if victims.is_empty() {
            return Ok(true);
        }
        self.backing
            .delete_docs(&self.store_name, &victims)
            .await
            .map_err(to_storage_error)?;
        // The sweep is exhaustive for a single pass over a bounded batch loop, and
        // the IndexedDB implementation deletes in one transaction, so there is
        // nothing left to resume.
        Ok(true)
    }

    async fn close(&self) -> Result<(), StorageError> {
        if *self.closed.borrow() {
            return Ok(());
        }
        *self.closed.borrow_mut() = true;
        self.subscribers.borrow_mut().clear();
        Ok(())
    }

    async fn remove(&self) -> Result<(), StorageError> {
        if *self.removed.borrow() {
            return Ok(());
        }
        if *self.closed.borrow() {
            return Err(StorageError::Closed);
        }
        self.backing
            .drop_store(&self.store_name)
            .await
            .map_err(to_storage_error)?;
        self.subscribers.borrow_mut().clear();
        *self.removed.borrow_mut() = true;
        *self.closed.borrow_mut() = true;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Keys and encoding
// ---------------------------------------------------------------------------

fn index_row_key(name: &str, encoded: &str, primary: &str) -> String {
    format!("{name}|{encoded}|{primary}")
}

/// Extracts the encoded index string from a composite index key.
fn key_index_string(key: &str) -> Option<String> {
    let mut parts = key.splitn(3, '|');
    parts.next()?;
    parts.next().map(str::to_string)
}

/// The cursor a batched read starts from.
///
/// # Why the cursor must extend to the *whole* checkpoint key
///
/// Index row keys are `"{indexName}|{encodedIndexString}|{primary}"`. For an inclusive
/// lower bound, `"{name}|{lower}"` is correct: every key with that prefix sorts at or
/// after it.
///
/// For an **exclusive** bound the exclusion has to be by the `(encoded, primary)`
/// *pair*, and the two obvious encodings are both wrong:
///
/// * `"{name}|{lower}\u{0}"` sorts *before* the checkpoint row rather than after it.
///   `\u{0}` is below `'|'`, so the cursor lands before `"{lower}|k1"` and the
///   checkpoint document is returned again — which was the real defect: every poll
///   re-delivered the document it had already seen.
/// * `"{name}|{lower}|\u{FFFF}"` fixes the common case but breaks when a primary value
///   contains an astral character: in UTF-8 (which is what Rust compares) `U+1D11E`
///   encodes above `U+FFFF`, so a checkpoint whose id is `"\u{1D11E}"` would be
///   re-included.
///
/// The exact form appends to the *full* checkpoint key: `"{name}|{lower}|{primary}\u{0}"`.
/// Because there is exactly one index row per `(index name, encoded string, primary)`,
/// that cursor sorts immediately after the checkpoint's own row and before every other
/// row — no code-unit reasoning required, and it works for any primary value.
///
/// # Why this is not "skip everything at this lwt"
///
/// The bound stays `(lwt, primary)`. A second document sharing the checkpoint's `lwt`
/// but with a larger primary value forms a different key that sorts after the cursor,
/// so it is still returned. `lwt > checkpoint.lwt` would have skipped it, which is
/// exactly why the pair exists.
fn index_cursor(
    name: &str,
    lower: &str,
    inclusive: bool,
    checkpoint_primary: Option<&str>,
) -> String {
    if inclusive {
        return format!("{name}|{lower}");
    }
    match checkpoint_primary {
        // The full key plus a character below every other, so only that one row is
        // excluded.
        Some(primary) => format!("{name}|{lower}|{primary}\u{0}"),
        // No primary in the plan (an exclusive bound on a non-unique prefix). Fall back
        // to just after the encoded string; `\u{10FFFF}` is above every scalar value in
        // UTF-8, so this excludes the whole prefix run.
        None => format!("{name}|{lower}|\u{10FFFF}"),
    }
}

fn attachment_key(document_id: &str, attachment_id: &str) -> String {
    format!("{document_id}||{attachment_id}")
}

fn is_deleted(doc: &DocumentData) -> bool {
    matches!(doc.get_object_path("_deleted"), Some(Value::Bool(true)))
}

fn ids_for(doc: &DocumentData, schema: &StorageSchema) -> Option<String> {
    doc.id(&schema.primary_path).ok().map(str::to_string)
}

fn to_storage_error(err: IdbError) -> StorageError {
    match err {
        IdbError::Unavailable(reason) => StorageError::Unsupported(reason),
        IdbError::Quota => StorageError::Unsupported("storage quota exceeded".to_string()),
        IdbError::Other(reason) => StorageError::Unsupported(reason),
    }
}

#[derive(Clone, Copy)]
enum Side {
    Lower,
    Upper,
}

/// Builds a bound string from the storage ABI's plan keys, using the ported encoder.
fn bound_string(
    schema: &Value,
    index: &[String],
    keys: &[dxdb_storage::PlanKey],
    side: Side,
) -> Result<String, StorageError> {
    let bounds: Vec<BoundValue> = keys
        .iter()
        .map(|key| BoundValue::from_json(Some(&key.to_json())))
        .collect();
    let encoded = match side {
        Side::Lower => get_start_index_string_from_lower_bound(schema, index, &bounds),
        Side::Upper => get_start_index_string_from_upper_bound(schema, index, &bounds),
    };
    encoded.map_err(|e| StorageError::InvalidSchema(format!("index bound: {e}")))
}

/// Converts the storage ABI's plan into the encoder's plan.
///
/// The sentinels are lifted by *value* rather than by variant name, so a caller that
/// passed the literal `MIN_SAFE_INTEGER` or the literal `U+FFFF` behaves the same as
/// one that used the named sentinel — matching upstream's `===` comparisons.
fn encoder_plan(prepared: &PreparedQuery) -> Result<EncoderPlan, StorageError> {
    use dxdb_query::plan::PlanKey as QKey;
    let lift = |key: &dxdb_storage::PlanKey| QKey::from_value(Some(&key.to_json()));
    // `from_value` is total (an unrepresentable key becomes `Undefined`), so there is
    // nothing to fail on here.
    let start: Vec<QKey> = prepared.query_plan.start_keys.iter().map(lift).collect();
    let end: Vec<QKey> = prepared.query_plan.end_keys.iter().map(lift).collect();
    Ok(EncoderPlan {
        index: prepared.query_plan.index.clone(),
        start_keys: start,
        end_keys: end,
        inclusive_start: prepared.query_plan.inclusive_start,
        inclusive_end: prepared.query_plan.inclusive_end,
        sort_satisfied_by_index: prepared.query_plan.sort_satisfied_by_index,
        selector_satisfied_by_index: prepared.query_plan.selector_satisfied_by_index,
    })
}

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
        limit: query.limit.map(|l| l as u64),
        index: query.index.clone(),
    }
}

// ---------------------------------------------------------------------------
// A future that is already ready, for the `impl Future` returns above
// ---------------------------------------------------------------------------
//
// Kept because the async blocks above capture `&self`, which means the returned
// future borrows the instance. That is fine for the trait (it is what `async fn` in
// a trait does), and it is why nothing here needs boxing on the hot path.

/// Runs `future` to completion on the current thread's executor.
///
/// Used by the conformance runner and by tests; the storage itself never blocks.
pub fn run_blocking<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// A never-ready future, for a caller that needs one.
pub struct Pending;

impl Future for Pending {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        Poll::Pending
    }
}

/// Test hook: the encoded bound string for a set of plan keys.
///
/// Exposed so the change-feed tests can assert that the lower bound equals the
/// checkpoint document's own index string. That equality is the property an exclusive
/// cursor depends on, and it cannot be checked from outside the crate otherwise.
pub fn encoder_bound_for_test(
    schema: &Value,
    index: &[String],
    keys: &[dxdb_storage::PlanKey],
    lower: bool,
) -> Result<String, StorageError> {
    bound_string(
        schema,
        index,
        keys,
        if lower { Side::Lower } else { Side::Upper },
    )
}

/// Test hook: one document's index string on an index.
pub fn encoded_index_string_for_test(
    schema: &Value,
    index: &[String],
    document: &DocumentData,
) -> Result<String, StorageError> {
    get_indexable_string(schema, index, document.as_value())
        .map_err(|e| StorageError::InvalidSchema(format!("index string: {e}")))
}
