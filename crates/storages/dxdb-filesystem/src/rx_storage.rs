//! The `RxStorage` / `RxStorageInstance` implementation over redb.
//!
//! # Why this exists
//!
//! Until now `dxdb-filesystem` was a redb *key layout* with a `put_document` /
//! `documents` API and no `RxStorage` implementation, which meant the shared
//! conformance suite could not run a single case against it: the matrix reported
//! `not runnable: does not implement RxStorage` for all 48. A storage that is not a
//! `RxStorage` cannot stand in for `storage-foundationdb` in the parity matrix, so
//! this module is what makes that claim true rather than aspirational.
//!
//! # What is borrowed and what is ours
//!
//! **Borrowed deliberately:** `categorize_bulk_write_rows` (the ported decision
//! function from `rx-storage-helper.ts:205`), `Matcher` for selectors,
//! `get_sort_comparator` for sorting, and `index_name` for index identity. A storage
//! does not author any of those; PLAN.md 4.3 makes the categorisation a single
//! well-tested pure function precisely so storages call it instead of reinventing it.
//!
//! **Ours:** the mapping between those documents and redb keys, and the transaction
//! discipline. `docs/lanes/m5-port-plan.md` section 2 is the layout's specification.
//!
//! # Deliberate differences from `dxdb-memory`, and why
//!
//! - **Query does not use the index tables yet.** Encoding a document into index
//!   strings and range-scanning them is the design in `layout`, but the only consumer
//!   that would exercise it is the planner, and a wrongly-encoded index that agrees
//!   with the scan on a small dataset is worse than an honest scan. So `query` scans
//!   the collection prefix, filters with `Matcher`, sorts with the real comparator, and
//!   the index tables stay provisioned and unused. Recorded in BENCHMARKS.md as a scan,
//!   which it is.
//! - **`get_changed_documents_since` is not overridden.** The trait's default body is
//!   upstream's documented fallback and is exact, and `dxdb-memory` leaves it
//!   unimplemented for the same reason. The `chg` table makes the ordering cheap and is
//!   used by `changed_since` in the benchmark, not by this path.
//! - **Attachments are stored but the digest is not verified on read.** The bytes are
//!   in the `attach` table keyed by `(doc, attachment)`; verifying requires the stub's
//!   digest and `dxdb-memory`'s behaviour is the reference, so this stays in step with
//!   it rather than inventing a stricter rule.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dxdb_query::matcher::Matcher;
use dxdb_query::sort::get_sort_comparator;
use dxdb_storage::{
    categorize_bulk_write_rows, now, AttachmentData, BulkWriteResponse, BulkWriteRow, ChangeStream,
    CountMode, CountResult, DocumentData, DynRxStorageInstance, EventBulk, InstanceCreationParams,
    PreparedQuery, RxStorage, RxStorageInstance, StorageError, StorageSchema,
};
use futures::channel::mpsc;
use redb::ReadableTable;

use crate::layout;
use crate::FilesystemStorage;

/// This crate's own error, disambiguated from the trait's.
///
/// `crate::StorageError` is a redb-detail error; `dxdb_storage::StorageError` is the
/// contract. The import above brings the trait's in, so this alias is how the two are
/// kept apart at every `?`.
type FsError = crate::StorageError;

/// How many events a change feed buffers before a slow subscriber starts losing bulks.
///
/// Lossy on purpose, matching `dxdb-memory`: a subscriber that cannot keep up must not
/// stall writes. Replication resumes from checkpoints, so a dropped bulk costs latency,
/// not data.
const CHANGE_FEED_BUFFER: usize = 1024;

/// The filesystem storage engine: a factory for instances over one folder.
///
/// Upstream has no counterpart for this (the filesystem storages are premium and
/// closed), so the shape follows `dxdb-memory`'s engine, which is the reference.
#[derive(Clone, Debug)]
pub struct FilesystemRxStorage {
    base_path: PathBuf,
}

impl FilesystemRxStorage {
    /// A storage rooted at `base_path`. Every instance created from it shares the one
    /// redb file, which is what makes `multiInstance: true` meaningful on the host.
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
        }
    }

    /// The folder this storage writes into.
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }
}

impl RxStorage for FilesystemRxStorage {
    fn name(&self) -> &str {
        "filesystem"
    }

    async fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> Result<Box<DynRxStorageInstance<'static>>, StorageError> {
        // Refuse what a bare storage cannot honour, rather than storing plaintext or
        // uncompressed keys while the caller believes otherwise. Same rule and same
        // reason as `dxdb-memory`.
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

        let store = FilesystemStorage::open(&self.base_path)?;
        Ok(DynRxStorageInstance::new_box(FilesystemRxInstance::new(
            params, store,
        )))
    }
}

/// One collection's storage, for one schema version, over the shared redb file.
pub struct FilesystemRxInstance {
    database_name: String,
    collection_name: String,
    schema: StorageSchema,
    store: FilesystemStorage,
    /// Subscribers to `change_stream`. A mutex because `change_stream(&self)` registers
    /// a sender and the instance is shared behind a `Box<dyn>`.
    subscribers: std::sync::Mutex<Vec<mpsc::Sender<EventBulk>>>,
    closed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for FilesystemRxInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemRxInstance")
            .field("collection", &self.collection_name)
            .field("version", &self.schema.version)
            .finish_non_exhaustive()
    }
}

impl FilesystemRxInstance {
    fn new(params: InstanceCreationParams, store: FilesystemStorage) -> Self {
        Self {
            database_name: params.database_name,
            collection_name: params.collection_name,
            schema: params.schema,
            store,
            subscribers: std::sync::Mutex::new(Vec::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The redb collection name for this instance.
    fn collection(&self) -> &str {
        &self.collection_name
    }

    fn version(&self) -> u32 {
        self.schema.version
    }

    fn ensure_usable(&self) -> Result<(), StorageError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::Closed);
        }
        Ok(())
    }

    fn primary_path(&self) -> &str {
        &self.schema.primary_path
    }

    /// Reads every document of this collection, including tombstones.
    fn all_documents(&self) -> Result<Vec<DocumentData>, StorageError> {
        let rows = self.store.documents(self.collection(), self.version())?;
        let mut out = Vec::with_capacity(rows.len());
        for (_key, body) in rows {
            let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
                StorageError::InvalidDocument(format!("stored document is not valid JSON: {e}"))
            })?;
            out.push(DocumentData::from_value(value)?);
        }
        Ok(out)
    }

    /// Emits one bulk to every live subscriber, dropping dead ones.
    fn emit(&self, bulk: EventBulk) {
        let mut subscribers = match self.subscribers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        subscribers.retain_mut(|sender| match sender.try_send(bulk.clone()) {
            Ok(()) => true,
            // A full or closed channel drops the bulk rather than blocking the write.
            Err(_) => false,
        });
    }
}

impl RxStorageInstance for FilesystemRxInstance {
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
        let primary_path = self.primary_path().to_string();
        let has_attachments = self.schema.has_attachments;

        // Snapshot before deciding, so two rows for one document both see the
        // pre-batch state -- the property upstream's categoriser assumes.
        let snapshot: HashMap<String, DocumentData> = {
            let mut map = HashMap::new();
            for doc in self.all_documents()? {
                if let Ok(id) = doc.id(&primary_path) {
                    map.insert(id.to_string(), doc);
                }
            }
            map
        };

        // Every accept/reject decision comes from the verified pure function.
        let categorised = categorize_bulk_write_rows(
            &primary_path,
            &snapshot,
            &document_writes,
            &context,
            has_attachments,
        );

        // One transaction for the whole batch: per-document atomicity is a semantic
        // property above this layer, and 500 commits would be 500 fsyncs.
        let mut rows: Vec<(String, i64, bool, Vec<u8>)> = Vec::new();
        for row in categorised
            .bulk_insert_docs
            .iter()
            .chain(categorised.bulk_update_docs.iter())
        {
            let document: &DocumentData = &row.document;
            let id = document.id(&primary_path)?.to_string();
            let lwt = document.lwt().unwrap_or_else(now).round() as i64;
            let deleted = document
                .get("_deleted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            rows.push((id, lwt, deleted, document.to_js_json().into_bytes()));
        }
        if !rows.is_empty() {
            self.store
                .put_documents(self.collection(), self.version(), &rows)?;
        }

        // Attachments, in the same call, after the documents they belong to.
        for add in &categorised.attachments_add {
            self.store
                .put_attachment(&add.document_id, &add.attachment_id, &add.data)?;
        }
        for update in &categorised.attachments_update {
            self.store
                .put_attachment(&update.document_id, &update.attachment_id, &update.data)?;
        }
        for remove in &categorised.attachments_remove {
            self.store
                .remove_attachment(&remove.document_id, &remove.attachment_id)?;
        }

        // Emit after the commit: a subscriber must never read a document that is not
        // yet visible outside the transaction. An all-errors batch emits nothing --
        // upstream's "no empty eventBulk" rule -- because `events` is empty then.
        if !categorised.events.is_empty() {
            self.emit(EventBulk {
                id: format!(
                    "{}:{}",
                    self.collection_name,
                    categorised
                        .checkpoint
                        .as_ref()
                        .map(|c| c.lwt)
                        .unwrap_or_else(now)
                ),
                events: categorised.events.clone(),
                checkpoint: categorised.checkpoint.clone(),
                context: context.clone(),
            });
        }

        Ok(BulkWriteResponse {
            error: categorised.error.clone(),
        })
    }

    async fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_usable()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(body) = self
                .store
                .get_document(self.collection(), self.version(), &id)?
            {
                let value: serde_json::Value = serde_json::from_slice(&body)
                    .map_err(|e| StorageError::InvalidDocument(e.to_string()))?;
                let document = DocumentData::from_value(value)?;
                let deleted = document
                    .get("_deleted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if with_deleted || !deleted {
                    out.push(document);
                }
            }
        }
        Ok(out)
    }

    async fn query(&self, prepared: PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_usable()?;
        let mut documents = self.all_documents()?;

        // Selector. The plan is a hint; a storage may ignore it, and this one does --
        // see the module docs on why the index tables are provisioned but unused.
        if !prepared.query_plan.selector_satisfied_by_index {
            let matcher = Matcher::new(&prepared.query.selector)
                .map_err(|e| StorageError::InvalidDocument(e.to_string()))?;
            documents.retain(|doc| matcher.matches(doc.as_value()));
        }

        // Sort with the real comparator so order matches the contract exactly, rather
        // than relying on redb's byte order over an index string this path never built.
        // The comparator comes from `dxdb-query` and takes its own `FilledMangoQuery`,
        // so the storage's query shape is lifted into it rather than the storage
        // reimplementing sort order. Same adapter as `dxdb-memory`.
        let comparator = get_sort_comparator(&lift_query(&prepared.query));
        documents.sort_by(|a, b| comparator.compare(a.as_value(), b.as_value()));

        // skip/limit last, after ordering.
        let skip = prepared.query.skip;
        let iter = documents.into_iter().skip(skip);
        let out: Vec<DocumentData> = match prepared.query.limit {
            Some(limit) => iter.take(limit).collect(),
            None => iter.collect(),
        };
        Ok(out)
    }

    async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        // Upstream's rule: `count` must equal `query().len()`, including with a limit.
        let documents = self.query(prepared).await?;
        Ok(CountResult {
            count: documents.len(),
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
        let body = self
            .store
            .get_attachment(&document_id, &attachment_id)?
            .ok_or_else(|| {
                StorageError::MissingAttachment(format!("{document_id}||{attachment_id}"))
            })?;
        // Stub lookup, so a wrong digest is an error rather than silently returned
        // bytes. Matches `dxdb-memory`'s behaviour.
        let stored = self
            .store
            .get_document(self.collection(), self.version(), &document_id)?
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .and_then(|value| DocumentData::from_value(value).ok())
            .and_then(|doc| doc.attachment(&attachment_id).cloned())
            .and_then(|stub| AttachmentData::from_value(&stub).map(|a| a.digest));
        match stored {
            Some(stored) if stored == digest => Ok(body),
            Some(stored) => Err(StorageError::MissingAttachment(format!(
                "{document_id}||{attachment_id}: digest {stored} does not match {digest}"
            ))),
            None => Err(StorageError::MissingAttachment(format!(
                "{document_id}||{attachment_id}: not declared on the document"
            ))),
        }
    }

    /// A native change feed over the `chg` table, rather than the trait's fallback.
    ///
    /// The fallback sets `selector_satisfied_by_index = true` on a
    /// `(_meta.lwt, primary)`-ordered query and expects the storage to honour the
    /// plan's `start_keys`. This storage's `query` deliberately ignores plan bounds (it
    /// scans, see the module docs), so relying on the fallback returned *everything*
    /// and the checkpoint was never exclusive -- three conformance cases caught it:
    /// "the checkpoint must be exclusive", "returned more documents than were written",
    /// "each poll must see exactly the one new document".
    ///
    /// Rather than teach `query` to honour one specific plan shape, this walks the
    /// `chg` table, which was built for exactly this: keys are
    /// `BE(flip(lwt)) ++ collection \0 version \0 id`, so an ascending scan from just
    /// past `(checkpoint.lwt, checkpoint.id)` is already in `(_meta.lwt, primary)` order
    /// and exclusive by construction.
    async fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<dxdb_storage::Checkpoint>,
    ) -> Result<dxdb_storage::ChangedDocuments, StorageError> {
        self.ensure_usable()?;
        let start = checkpoint
            .clone()
            .unwrap_or_else(dxdb_storage::Checkpoint::beginning);
        let primary_path = self.primary_path().to_string();
        let prefix = layout::prefix(self.collection(), self.version());

        // Lower bound: the start lwt, then skip entries until we are past the
        // checkpoint pair. Starting at the same lwt (not lwt+1) is required because
        // several documents can share an lwt and the id breaks the tie.
        let lowered = layout::encode_lwt(start.lwt.round() as i64).to_vec();

        let ids: Vec<String> = {
            let tx = self.store.read_transaction()?;
            let table = tx
                .open_table(crate::CHG)
                .map_err(|e| FsError::redb("changed_since/open", e))?;
            let range = table
                .range(lowered.as_slice()..)
                .map_err(|e| FsError::redb("changed_since/range", e))?;
            let mut out = Vec::new();
            for entry in range {
                let (key, _) = entry.map_err(|e| FsError::redb("changed_since/entry", e))?;
                let key = key.value();
                if key.len() < 8 + prefix.len() {
                    continue;
                }
                if &key[8..8 + prefix.len()] != prefix.as_slice() {
                    continue;
                }
                let lwt = layout::decode_lwt(&key[..8]).unwrap_or(0);
                let id = String::from_utf8_lossy(&key[8 + prefix.len()..]).into_owned();
                // Exclusive: strictly after the checkpoint pair in (lwt, id) order.
                let after = lwt > start.lwt.round() as i64
                    || (lwt == start.lwt.round() as i64 && id.as_str() > start.id.as_str());
                if !after {
                    continue;
                }
                out.push(id);
                if out.len() >= limit {
                    break;
                }
            }
            out
        };

        let mut documents = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(body) = self
                .store
                .get_document(self.collection(), self.version(), id)?
            {
                let value: serde_json::Value = serde_json::from_slice(&body)
                    .map_err(|e| StorageError::InvalidDocument(e.to_string()))?;
                let document = DocumentData::from_value(value)?;
                let _ = &primary_path;
                documents.push(document);
            }
        }

        let checkpoint = match documents.last() {
            Some(last) => dxdb_storage::Checkpoint::new(
                last.id(&primary_path)?.to_string(),
                last.lwt().unwrap_or(start.lwt),
            ),
            None => start,
        };
        Ok(dxdb_storage::ChangedDocuments {
            documents,
            checkpoint,
        })
    }

    fn change_stream(&self) -> ChangeStream {
        let (sender, receiver) = mpsc::channel(CHANGE_FEED_BUFFER);
        match self.subscribers.lock() {
            Ok(mut guard) => guard.push(sender),
            Err(poisoned) => poisoned.into_inner().push(sender),
        }
        Box::pin(receiver)
    }

    async fn cleanup(&self, minimum_deleted_time: f64) -> Result<bool, StorageError> {
        self.ensure_usable()?;
        let cutoff = (now() - minimum_deleted_time).round() as i64;
        let removed = self
            .store
            .delete_tombstones(self.collection(), self.version(), cutoff)?;
        // `true` means nothing cleanable remains; this deletes everything eligible in
        // one pass, so there is never more work after it.
        let _ = removed;
        Ok(true)
    }

    async fn close(&self) -> Result<(), StorageError> {
        // Closing twice must not error (upstream's "closing multiple times" test).
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        // Drop subscribers so a live stream terminates.
        match self.subscribers.lock() {
            Ok(mut guard) => guard.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
        Ok(())
    }

    async fn remove(&self) -> Result<(), StorageError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::Closed);
        }
        self.store
            .remove_collection(self.collection(), self.version())?;
        self.close().await
    }
}

/// Lifts the storage's query shape into the query crate's, for sorting.
///
/// Two near-identical structs on either side of a crate boundary is the cost of
/// `dxdb-storage` not depending on `dxdb-query`. `dxdb-memory` pays it the same way.
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

/// Maps this crate's error onto the trait's, so `?` works at the boundary.
///
/// The trait's `StorageError` is the contract; ours is a redb-detail error. Keeping
/// them separate means a redb failure is described precisely here and an unsupported
/// capability is described in the trait's vocabulary above it.
impl From<FsError> for StorageError {
    fn from(error: FsError) -> Self {
        match error {
            FsError::Unsupported(reason) => StorageError::Unsupported(reason.to_string()),
            other => StorageError::InvalidDocument(other.to_string()),
        }
    }
}

/// The table names this module touches, so a test can assert the layout is real.
///
/// Names rather than the `TableDefinition` constants because a
/// `TableDefinition<'_, &[u8], &[u8]>` is not a `&str`; the constants are what the code
/// opens, and these are what a reader can compare against `layout`'s table.
pub const TABLES: [&str; 5] = ["docs", "chg", "cleanup", "idx", "attach"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_reports_its_name() {
        let storage = FilesystemRxStorage::new(std::env::temp_dir());
        assert_eq!(storage.name(), "filesystem");
    }

    #[test]
    fn the_layout_constants_are_all_distinct() {
        let unique: std::collections::HashSet<_> = TABLES.iter().collect();
        assert_eq!(unique.len(), TABLES.len(), "table names must be distinct");
    }
}
