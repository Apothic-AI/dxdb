//! `dxdb-opfs`: Origin Private File System storage for dxdb.
//!
//! # The two paths, and why this is not one storage with a flag
//!
//! PLAN.md 5.4: *"`createSyncAccessHandle` exists only in a dedicated worker, so the
//! fast OPFS path is inherently a worker story."* That is not a limitation this crate
//! works around; it is the shape of the platform, and the crate is organised around
//! making the consequence observable:
//!
//! - [`AccessPath::SyncAccessHandle`] — the fast path, only inside a dedicated worker.
//! - [`AccessPath::AsyncWritable`] — `createWritable`, works anywhere, one `await` and
//!   a stream round trip per write.
//!
//! [`OpfsStorage::path`] returns which one is in use, and the conformance runner reports
//! it. A fallback nobody can see is how "OPFS support" claims usually fall apart, so the
//! path is data rather than an implementation detail.
//!
//! JSPI would eventually let the *sync* implementation run on the main thread (PLAN.md
//! 5.2). The [`FileStore`] trait is deliberately narrow enough that a JSPI shim would be
//! a third backend rather than a rewrite.
//!
//! # On-disk layout
//!
//! One file per record, because OPFS is a byte-oriented file API and a random-access
//! page manager is `dxdb-sqlite`'s job:
//!
//! | key | contents |
//! | --- | --- |
//! | `{db}/{collection}/v{version}/doc/{primary}.json` | one stored document |
//! | `{db}/{collection}/v{version}/idx/{indexName}/{indexString}.json` | `[primary, ...]` in encoded order |
//! | `{db}/{collection}/v{version}/att/{primary}__{attachmentId}.bin` | raw attachment bytes |
//! | `{db}/{collection}/v{version}/meta.json` | schema version + primary path |
//!
//! Index keys are `{indexString}` under an index directory, so listing an index
//! directory and sorting the keys *is* reading the index in encoded order — OPFS file
//! names sort by code unit, which is the ordering the encoded index string defines.
//! That replaces IndexedDB's key-range `getAll`; the equivalent batching happens over
//! a sorted key vector.
//!
//! Attachments are stored as raw bytes, not base64: unlike `localStorage`, OPFS holds
//! binary natively, and encoding would cost 33% on disk for no benefit.

#![forbid(unsafe_code)]

pub mod backing;

#[cfg(target_arch = "wasm32")]
pub mod web;

#[cfg(feature = "conformance")]
pub mod conformance;

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;

use serde_json::Value;

use dxdb_query::custom_index::{
    get_indexable_string, get_start_index_string_from_lower_bound,
    get_start_index_string_from_upper_bound, BoundValue,
};
use dxdb_query::matcher::Matcher;
use dxdb_query::sort::get_sort_comparator;
use dxdb_storage::{
    categorize_bulk_write_rows, index_name, BulkWriteResponse, BulkWriteRow, ChangeStream,
    CountMode, CountResult, DocumentData, DynRxStorageInstance, EventBulk, FindAttachmentResult,
    InstanceCreationParams, PreparedQuery, RxStorage, RxStorageInstance, StorageError,
    StorageSchema,
};

pub use backing::{AccessPath, FileStore, MemoryFiles, OpfsError};

/// The storage name upstream reports (`RX_STORAGE_NAME_OPFS`).
pub const STORAGE_NAME: &str = "opfs";

/// Keys read per batch when walking an index. Mirrors the IndexedDB `batchSize` default
/// so both storages bound their work the same way.
pub const DEFAULT_BATCH_SIZE: usize = 300;

const CLEANUP_INDEX: [&str; 3] = ["_deleted", "_meta.lwt", ""];

// ---------------------------------------------------------------------------
// The factory
// ---------------------------------------------------------------------------

/// Opens instances over one OPFS-like file store.
pub struct OpfsStorage<S: FileStore + 'static> {
    store: Rc<S>,
    batch_size: usize,
}

impl<S: FileStore + 'static> OpfsStorage<S> {
    #[must_use]
    pub fn new(store: Rc<S>) -> Self {
        Self {
            store,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    #[must_use]
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// Which OPFS API this storage is using. **Reported, not assumed.**
    #[must_use]
    pub fn path(&self) -> AccessPath {
        self.store.path()
    }

    #[must_use]
    pub fn store(&self) -> &Rc<S> {
        &self.store
    }
}

impl<S: FileStore + 'static> RxStorage for OpfsStorage<S> {
    fn name(&self) -> &str {
        STORAGE_NAME
    }

    fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> impl Future<Output = Result<Box<DynRxStorageInstance<'static>>, StorageError>> {
        let store = Rc::clone(&self.store);
        let batch_size = self.batch_size;
        let path = self.store.path();
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
            let instance = OpfsInstance::open(store, params, batch_size, path).await?;
            Ok(DynRxStorageInstance::new_box(instance))
        }
    }
}

// ---------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------

struct OpfsInstance<S: FileStore + 'static> {
    store: Rc<S>,
    /// `{db}/{collection}/v{version}` — shared by every key this instance owns.
    root: String,
    database_name: String,
    collection_name: String,
    schema: StorageSchema,
    /// Read per batch when walking an index. Kept per instance so a caller can tune it
    /// without changing the backend; `DEFAULT_BATCH_SIZE` matches IndexedDB.
    #[allow(dead_code)]
    batch_size: usize,
    /// Which API the backing is using, cached so a report does not have to reach
    /// through the store.
    #[allow(dead_code)]
    path: AccessPath,
    subscribers: RefCell<Vec<futures_channel::mpsc::UnboundedSender<EventBulk>>>,
    closed: RefCell<bool>,
    removed: RefCell<bool>,
}

impl<S: FileStore + 'static> OpfsInstance<S> {
    async fn open(
        store: Rc<S>,
        params: InstanceCreationParams,
        batch_size: usize,
        path: AccessPath,
    ) -> Result<Self, StorageError> {
        let root = format!(
            "{db}/{collection}/v{version}",
            db = params.database_name,
            collection = params.collection_name,
            version = params.schema.version
        );
        let instance = Self {
            store,
            root,
            database_name: params.database_name,
            collection_name: params.collection_name,
            schema: params.schema,
            batch_size,
            path,
            subscribers: RefCell::new(Vec::new()),
            closed: RefCell::new(false),
            removed: RefCell::new(false),
        };
        for dir in ["doc", "idx", "att"] {
            instance
                .store
                .create_dir(&instance.dir(dir))
                .await
                .map_err(to_storage_error)?;
        }
        // Record the schema this store was created for, so a re-open with a different
        // version or primary path is a loud error rather than mixed documents.
        let meta = serde_json::json!({
            "version": instance.schema.version,
            "primary_path": instance.schema.primary_path,
        });
        instance
            .store
            .write(
                &instance.key("meta.json"),
                serde_json::to_string(&meta)
                    .expect("meta is serialisable")
                    .as_bytes(),
            )
            .await
            .map_err(to_storage_error)?;
        Ok(instance)
    }

    fn dir(&self, name: &str) -> String {
        format!("{}/{name}", self.root)
    }

    fn key(&self, rest: &str) -> String {
        format!("{}/{rest}", self.root)
    }

    fn doc_key(&self, id: &str) -> String {
        format!("{}/doc/{}.json", self.root, escape_id(id))
    }

    fn index_dir(&self, name: &str) -> String {
        format!("{}/idx/{}", self.root, escape_id(name))
    }

    fn attachment_key(&self, doc_id: &str, attachment_id: &str) -> String {
        format!(
            "{}/att/{}__{}.bin",
            self.root,
            escape_id(doc_id),
            escape_id(attachment_id)
        )
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

    fn encode(&self, index: &[String], doc: &DocumentData) -> Result<String, StorageError> {
        get_indexable_string(&self.schema.raw, index, doc.as_value())
            .map_err(|e| StorageError::InvalidSchema(format!("index string: {e}")))
    }

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

    async fn read_doc(&self, id: &str) -> Result<Option<DocumentData>, StorageError> {
        let raw = self
            .store
            .read(&self.doc_key(id))
            .await
            .map_err(to_storage_error)?;
        let Some(bytes) = raw else { return Ok(None) };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| StorageError::InvalidDocument(format!("stored document {id}: {e}")))?;
        Ok(Some(DocumentData::from_value(value)?))
    }

    /// Every entry of one index, in encoded order.
    ///
    /// OPFS has no key-range read, so the index is a directory whose file names *are*
    /// the encoded index strings; several primaries can share one index string, so each
    /// file holds a JSON array of them. Sorting the names gives the index order, because
    /// that is what the encoder guarantees.
    async fn read_index(&self, name: &str) -> Result<Vec<(String, Vec<String>)>, StorageError> {
        let dir = self.index_dir(name);
        let prefix = format!("{dir}/");
        let mut keys = self
            .store
            .keys()
            .await
            .map_err(to_storage_error)?
            .into_iter()
            .filter(|key| key.starts_with(&prefix))
            .collect::<Vec<_>>();
        keys.sort();

        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(bytes) = self.store.read(&key).await.map_err(to_storage_error)? else {
                continue;
            };
            let primaries: Vec<String> = serde_json::from_slice(&bytes)
                .map_err(|e| StorageError::InvalidSchema(format!("index entry {key}: {e}")))?;
            // The file name (minus `.json`) is the encoded index string.
            let encoded = key
                .strip_prefix(&prefix)
                .unwrap_or_default()
                .trim_end_matches(".json")
                .to_string();
            out.push((unescape_id(&encoded), primaries));
        }
        Ok(out)
    }

    async fn write_index_entries(
        &self,
        name: &str,
        encoded: &str,
        primaries: &[String],
    ) -> Result<(), StorageError> {
        let key = format!("{}/{}.json", self.index_dir(name), escape_id(encoded));
        if primaries.is_empty() {
            self.store.remove(&key).await.map_err(to_storage_error)?;
        } else {
            let bytes = serde_json::to_vec(primaries).expect("a string list is serialisable");
            self.store
                .write(&key, &bytes)
                .await
                .map_err(to_storage_error)?;
        }
        Ok(())
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

impl<S: FileStore + 'static> RxStorageInstance for OpfsInstance<S> {
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

        // Read the affected documents first: conflict-vs-update is decided from the
        // *stored* state.
        let mut docs_in_db: HashMap<String, DocumentData> = HashMap::new();
        for row in &document_writes {
            let id = row.document.id(&self.schema.primary_path)?.to_string();
            if let Some(existing) = self.read_doc(&id).await? {
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

        for add in &categorized.attachments_add {
            self.store
                .write(
                    &self.attachment_key(&add.document_id, &add.attachment_id),
                    &add.data,
                )
                .await
                .map_err(to_storage_error)?;
        }
        for update in &categorized.attachments_update {
            self.store
                .write(
                    &self.attachment_key(&update.document_id, &update.attachment_id),
                    &update.data,
                )
                .await
                .map_err(to_storage_error)?;
        }
        for remove in &categorized.attachments_remove {
            self.store
                .remove(&self.attachment_key(&remove.document_id, &remove.attachment_id))
                .await
                .map_err(to_storage_error)?;
        }

        let indexes = self.maintained_indexes();
        for row in categorized
            .bulk_insert_docs
            .iter()
            .chain(categorized.bulk_update_docs.iter())
        {
            let id = row.document.id(&self.schema.primary_path)?.to_string();

            // Remove the previous index entries for this document.
            if let Some(previous) = &row.previous {
                for index in &indexes {
                    let name = index_name(index);
                    let old = self.encode(index, previous)?;
                    let mut entries = self.read_index(&name).await?;
                    if let Some((_, primaries)) =
                        entries.iter_mut().find(|(encoded, _)| *encoded == old)
                    {
                        primaries.retain(|primary| *primary != id);
                        let remaining = primaries.clone();
                        self.write_index_entries(&name, &old, &remaining).await?;
                    }
                }
            }

            let bytes = serde_json::to_vec(row.document.as_value())
                .expect("a DocumentData is always serialisable");
            self.store
                .write(&self.doc_key(&id), &bytes)
                .await
                .map_err(to_storage_error)?;

            // Add the new entries.
            for index in &indexes {
                let name = index_name(index);
                let encoded = self.encode(index, &row.document)?;
                let mut entries = self.read_index(&name).await?;
                match entries
                    .iter_mut()
                    .find(|(existing, _)| *existing == encoded)
                {
                    Some((_, primaries)) => {
                        if !primaries.iter().any(|primary| *primary == id) {
                            primaries.push(id.clone());
                            primaries.sort();
                        }
                    }
                    None => entries.push((encoded.clone(), vec![id.clone()])),
                }
                let primaries = entries
                    .iter()
                    .find(|(existing, _)| *existing == encoded)
                    .map(|(_, primaries)| primaries.clone())
                    .unwrap_or_default();
                self.write_index_entries(&name, &encoded, &primaries)
                    .await?;
            }
        }

        if !categorized.events.is_empty() {
            self.emit(EventBulk {
                id: format!("{}/{}", self.root, context),
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
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(doc) = self.read_doc(&id).await? {
                if with_deleted || !is_deleted(&doc) {
                    out.push(doc);
                }
            }
        }
        Ok(out)
    }

    async fn query(&self, prepared: PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_open()?;
        let index: Vec<String> = prepared.query_plan.index.clone();
        let name = index_name(&index);

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

        // The exclusive bound is on the `(encoded, primary)` *pair*, exactly as in
        // dxdb-indexeddb: appending the checkpoint primary excludes only that document,
        // while a document sharing its `lwt` but with a larger primary still sorts after.
        let checkpoint_primary: Option<String> =
            prepared
                .query_plan
                .start_keys
                .last()
                .and_then(|key| match key {
                    dxdb_storage::PlanKey::String(text) => Some(text.clone()),
                    _ => None,
                });

        let entries = self.read_index(&name).await?;
        let total_needed = prepared.query.skip_plus_limit();
        let mut primaries: Vec<String> = Vec::new();

        for (encoded, ids) in entries {
            // Lower bound, exclusive on the pair.
            let after_start = if prepared.query_plan.inclusive_start {
                encoded.as_str() >= lower.as_str()
            } else {
                // The baseline comparison is on the encoded string; when it is equal, the
                // primary decides, which is what makes the pair exclusive rather than the
                // whole lwt group.
                encoded.as_str() > lower.as_str()
            };
            if !after_start {
                continue;
            }
            // Upper bound.
            let before_end = if prepared.query_plan.inclusive_end {
                encoded.as_str() <= upper.as_str()
            } else {
                encoded.as_str() < upper.as_str()
            };
            if !before_end {
                break;
            }

            for id in ids {
                // When the encoded string ties with the lower bound, an exclusive plan
                // must still skip primaries at or before the checkpoint's.
                if !prepared.query_plan.inclusive_start && encoded.as_str() == lower.as_str() {
                    if let Some(checkpoint) = &checkpoint_primary {
                        if id.as_str() <= checkpoint.as_str() {
                            continue;
                        }
                    }
                }
                primaries.push(id);
                if primaries.len() >= total_needed {
                    break;
                }
            }
            if primaries.len() >= total_needed {
                break;
            }
        }

        let mut documents: Vec<DocumentData> = Vec::with_capacity(primaries.len());
        for id in &primaries {
            if let Some(doc) = self.read_doc(id).await? {
                documents.push(doc);
            }
        }

        if !prepared.query_plan.selector_satisfied_by_index {
            let matcher = Matcher::new(&prepared.query.selector)
                .map_err(|e| StorageError::InvalidSchema(format!("selector: {e}")))?;
            documents.retain(|doc| matcher.matches(doc.as_value()));
        }

        if !prepared.query_plan.sort_satisfied_by_index {
            let comparator = get_sort_comparator(&lift_query(&prepared.query));
            documents.sort_by(|a, b| comparator.compare(a.as_value(), b.as_value()));
        }

        Ok(documents
            .into_iter()
            .skip(prepared.query.skip)
            .take(prepared.query.limit.unwrap_or(usize::MAX))
            .collect())
    }

    async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        let full_scan = prepared.query_plan.is_full_scan();
        let documents = self.query(prepared).await?;
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
            .read_doc(&document_id)
            .await?
            .ok_or_else(|| StorageError::MissingAttachment(format!("{document_id}")))?;
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
                    "{document_id}||{attachment_id}: not declared"
                )))
            }
        }
        self.store
            .read(&self.attachment_key(&document_id, &attachment_id))
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

        let entries = self.read_index(&name).await?;
        let mut victims: Vec<String> = Vec::new();
        for (_, ids) in entries {
            for id in ids {
                let Some(doc) = self.read_doc(&id).await? else {
                    continue;
                };
                if is_deleted(&doc) && doc.lwt().unwrap_or(0.0) < cutoff {
                    victims.push(id);
                }
            }
        }

        if victims.is_empty() {
            return Ok(true);
        }
        for id in &victims {
            self.store
                .remove(&self.doc_key(id))
                .await
                .map_err(to_storage_error)?;
        }
        // Rebuild the indexes without the victims, so no entry points at a deleted doc.
        for index in &self.maintained_indexes() {
            let name = index_name(index);
            let entries = self.read_index(&name).await?;
            for (encoded, ids) in entries {
                let kept: Vec<String> =
                    ids.into_iter().filter(|id| !victims.contains(id)).collect();
                self.write_index_entries(&name, &encoded, &kept).await?;
            }
        }
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
        self.store
            .remove_dir(&self.root)
            .await
            .map_err(to_storage_error)?;
        self.subscribers.borrow_mut().clear();
        *self.removed.borrow_mut() = true;
        *self.closed.borrow_mut() = true;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encodes a string for use as one path segment.
///
/// A primary value or attachment id can contain `/` (a path separator) or a character
/// OPFS rejects in a name, so it is percent-encoded with a scheme that is reversible and
/// keeps ordinary ids readable. `%` itself is escaped first, or decoding would be
/// ambiguous.
fn escape_id(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn unescape_id(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_deleted(doc: &DocumentData) -> bool {
    matches!(doc.get_object_path("_deleted"), Some(Value::Bool(true)))
}

fn to_storage_error(error: OpfsError) -> StorageError {
    match error {
        OpfsError::Unavailable(reason) => StorageError::Unsupported(reason),
        OpfsError::Quota => StorageError::Unsupported("storage quota exceeded".to_string()),
        OpfsError::NotFound(key) => StorageError::MissingAttachment(key),
        OpfsError::Other(reason) => StorageError::Unsupported(reason),
    }
}

#[derive(Clone, Copy)]
enum Side {
    Lower,
    Upper,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_round_trips_awkward_ids() {
        for id in [
            "plain",
            "with/slash",
            "with space",
            "with%percent",
            "Ünïcodé 世界",
            "Bob 🦀",
            "a..b",
            "",
        ] {
            assert_eq!(unescape_id(&escape_id(id)), id, "id {id:?}");
        }
    }

    #[test]
    fn escaped_ids_contain_no_path_separator() {
        assert!(!escape_id("a/b/c").contains('/'));
    }

    /// The storage sorts *encoded index strings*, never ids, so id ordering is not a
    /// property it relies on. What it does rely on is that escaping is injective: two
    /// distinct ids must never share a path, or one document would overwrite another.
    #[test]
    fn escaping_is_injective() {
        let ids = [
            "plain",
            "with/slash",
            "with space",
            "with%percent",
            "with%2Fencoded",
            "Ünïcodé 世界",
            "",
        ];
        let mut escaped: Vec<String> = ids.iter().map(|id| escape_id(id)).collect();
        let count_before = escaped.len();
        escaped.sort();
        escaped.dedup();
        assert_eq!(
            escaped.len(),
            count_before,
            "two different ids escaped to the same path"
        );
    }
}
