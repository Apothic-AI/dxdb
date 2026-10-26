//! A record-and-replay storage double, shared by both test binaries.
//!
//! `dxdb-memory` is still a scaffold, so the wrapper is exercised against this
//! instead. It is deliberately a *faithful* fake rather than a mock: it stores
//! documents by primary key, honours `with_deleted`, applies a real sort, and emits
//! change events, because the invariants under test are about what the wrapper does
//! to real documents, not about call counts.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use dxdb_storage::{
    ChangeEvent, ChangeStream, Checkpoint, CountMode, CountResult, DocumentData,
    DocumentWriteData, DynRxStorageInstance, EventBulk, InstanceCreationParams, Operation,
    PreparedQuery, RxStorage, RxStorageInstance, StorageError, StorageSchema, WriteError,
};
use dxdb_storage::{BulkWriteResponse, BulkWriteRow};

/// What the fake stores, and what it observed.
#[derive(Debug, Default)]
pub struct Inner {
    /// Documents by primary key, in their stored (possibly ciphertext) form.
    pub documents: BTreeMap<String, DocumentData>,
    /// Every write that reached the fake.
    pub writes: Vec<String>,
    /// Every query that reached the fake.
    pub queries: usize,
}

/// The fake storage.
#[derive(Debug, Clone, Default)]
pub struct FakeStorage {
    /// Shared state, so a test can inspect what reached the fake.
    pub inner: Arc<Mutex<Inner>>,
}

impl FakeStorage {
    /// A new empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    /// The stored documents, cloned out.
    pub fn documents(&self) -> BTreeMap<String, DocumentData> {
        self.inner.lock().expect("poisoned").documents.clone()
    }

    /// The number of writes the fake received.
    pub fn write_count(&self) -> usize {
        self.inner.lock().expect("poisoned").writes.len()
    }

    /// The number of queries the fake received.
    pub fn query_count(&self) -> usize {
        self.inner.lock().expect("poisoned").queries
    }
}

impl RxStorage for FakeStorage {
    fn name(&self) -> &str {
        "fake-memory"
    }

    async fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> Result<Box<DynRxStorageInstance<'static>>, StorageError> {
        let instance = FakeInstance {
            inner: Arc::clone(&self.inner),
            schema: params.schema,
            database_name: params.database_name,
            collection_name: params.collection_name,
        };
        Ok(DynRxStorageInstance::new_box(instance))
    }
}

/// One fake instance.
#[derive(Debug, Clone)]
pub struct FakeInstance {
    inner: Arc<Mutex<Inner>>,
    schema: StorageSchema,
    database_name: String,
    collection_name: String,
}

fn sort_key(document: &DocumentData, sort: &[dxdb_storage::SortPart]) -> Vec<String> {
    sort.iter()
        .map(|part| {
            let value = dxdb_storage::get_object_path(document.as_value(), &part.field)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            dxdb_json::to_js_json(&value)
        })
        .collect()
}

impl RxStorageInstance for FakeInstance {
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
        _context: String,
    ) -> Result<BulkWriteResponse, StorageError> {
        let mut state = self.inner.lock().expect("poisoned");
        let mut errors = Vec::new();
        for row in document_writes {
            let id = row
                .document
                .document()
                .id(&self.schema.primary_path)
                .map_err(|error| match error {
                    StorageError::InvalidDocument(message) => {
                        StorageError::InvalidDocument(message)
                    }
                    other => other,
                })?
                .to_string();
            // Conflict detection: a write whose `previous` does not match the stored
            // revision is a per-row failure, exactly as the trait documents.
            if let Some(previous) = row.previous.as_ref() {
                let stored = state.documents.get(&id);
                if stored.and_then(|doc| doc.rev()) != previous.rev() {
                    errors.push(WriteError::Conflict {
                        document_id: id.clone(),
                        write_row: row.clone(),
                        document_in_db: state.documents.get(&id).cloned(),
                        context: String::new(),
                    });
                    continue;
                }
            } else if state.documents.contains_key(&id) {
                // An insert against an existing document is a conflict too.
                errors.push(WriteError::Conflict {
                    document_id: id.clone(),
                    write_row: row.clone(),
                    document_in_db: state.documents.get(&id).cloned(),
                    context: String::new(),
                });
                continue;
            }
            state.writes.push(id.clone());
            state
                .documents
                .insert(id, row.document.document().clone());
        }
        Ok(BulkWriteResponse::with_errors(errors))
    }

    async fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> Result<Vec<DocumentData>, StorageError> {
        let state = self.inner.lock().expect("poisoned");
        let mut out = Vec::new();
        for id in ids {
            if let Some(document) = state.documents.get(&id) {
                if with_deleted || !document.is_deleted() {
                    out.push(document.clone());
                }
            }
        }
        Ok(out)
    }

    async fn query(&self, prepared: PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        let mut state = self.inner.lock().expect("poisoned");
        state.queries += 1;
        let mut documents: Vec<DocumentData> = state
            .documents
            .values()
            .filter(|document| !document.is_deleted())
            .cloned()
            .collect();
        documents.sort_by_key(|document| sort_key(document, &prepared.query.sort));
        if let Some(limit) = prepared.query.limit {
            documents.truncate(prepared.query.skip.saturating_add(limit).min(documents.len()));
        }
        Ok(documents.into_iter().skip(prepared.query.skip).collect())
    }

    async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        let documents = self.query(prepared.clone()).await?;
        Ok(CountResult {
            count: documents.len(),
            mode: CountMode::Slow,
        })
    }

    async fn get_attachment_data(
        &self,
        _document_id: String,
        _attachment_id: String,
        _digest: String,
    ) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::MissingAttachment("none stored".to_string()))
    }

    /// A faithful change feed: `(_meta.lwt, primary)` order, strictly after the
    /// checkpoint, at most `limit`, each document at its current state.
    ///
    /// Implemented rather than inherited from the trait's default body because the
    /// default body runs a *planned range query*, and this fake's `query` does not
    /// honour plan start keys. A double that quietly ignores the checkpoint makes a
    /// multi-pass consumer look broken when the double is what is wrong.
    async fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<Checkpoint>,
    ) -> Result<dxdb_storage::ChangedDocuments, StorageError> {
        let state = self.inner.lock().expect("poisoned");
        let mut documents: Vec<DocumentData> = state.documents.values().cloned().collect();
        documents.sort_by(|left, right| {
            let key = |document: &DocumentData| {
                (
                    document.lwt().unwrap_or(0.0),
                    document
                        .id(&self.schema.primary_path)
                        .unwrap_or_default()
                        .to_string(),
                )
            };
            key(left).partial_cmp(&key(right)).unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(checkpoint) = checkpoint.as_ref() {
            documents.retain(|document| {
                let lwt = document.lwt().unwrap_or(0.0);
                let id = document
                    .id(&self.schema.primary_path)
                    .unwrap_or_default()
                    .to_string();
                (lwt, id) > (checkpoint.lwt, checkpoint.id.clone())
            });
        }
        documents.truncate(limit);
        let checkpoint = match documents.last() {
            Some(last) => Checkpoint::new(
                last.id(&self.schema.primary_path)
                    .unwrap_or_default()
                    .to_string(),
                last.lwt().unwrap_or(0.0),
            ),
            None => checkpoint.unwrap_or_else(Checkpoint::beginning),
        };
        Ok(dxdb_storage::ChangedDocuments {
            documents,
            checkpoint,
        })
    }

    fn change_stream(&self) -> ChangeStream {
        Box::pin(futures::stream::empty())
    }

    async fn cleanup(&self, _minimum_deleted_time: f64) -> Result<bool, StorageError> {
        Ok(true)
    }

    async fn close(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn remove(&self) -> Result<(), StorageError> {
        self.inner.lock().expect("poisoned").documents.clear();
        Ok(())
    }
}

/// A schema value with `encrypted` paths, ready to hand to a storage.
pub fn schema(encrypted: &[&str], attachments_encrypted: bool) -> serde_json::Value {
    let mut value = serde_json::json!({
        "version": 0,
        "primaryKey": "id",
        "indexes": [],
        "encrypted": encrypted,
    });
    if attachments_encrypted {
        value["attachments"] = serde_json::json!({ "encrypted": true });
    }
    value
}

/// The `InstanceCreationParams` for `schema`.
pub fn params(schema_value: &serde_json::Value) -> InstanceCreationParams {
    InstanceCreationParams {
        database_instance_token: "token".to_string(),
        database_name: "testdb".to_string(),
        collection_name: "humans".to_string(),
        schema: StorageSchema::from_value(schema_value).expect("valid schema"),
        options: serde_json::json!({}),
        multi_instance: false,
        dev_mode: true,
    }
}

/// A document with a revision, ready to write.
pub fn document(id: &str, revision: &str, fields: serde_json::Value) -> DocumentData {
    let mut object = serde_json::Map::new();
    object.insert("id".to_string(), serde_json::json!(id));
    object.insert("_rev".to_string(), serde_json::json!(revision));
    object.insert("_deleted".to_string(), serde_json::json!(false));
    object.insert("_meta".to_string(), serde_json::json!({"lwt": 1000}));
    object.insert(
        "_attachments".to_string(),
        serde_json::Value::Object(serde_json::Map::new()),
    );
    for (key, value) in fields.as_object().expect("object") {
        object.insert(key.clone(), value.clone());
    }
    DocumentData::from_value(serde_json::Value::Object(object)).expect("valid document")
}

/// An insert row for `document`.
pub fn insert_row(document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::insert(DocumentWriteData::new(document))
}

/// An update row replacing `previous` with `document`.
pub fn update_row(previous: DocumentData, document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::update(previous, DocumentWriteData::new(document))
}

/// A trivial query that returns every non-deleted document sorted by id.
pub fn all_documents_query() -> PreparedQuery {
    let query = dxdb_storage::MangoQuery {
        selector: serde_json::json!({}),
        sort: vec![dxdb_storage::SortPart::asc("id")],
        skip: 0,
        limit: None,
        index: None,
    };
    let plan = dxdb_storage::QueryPlan {
        index: vec!["_deleted".to_string(), "id".to_string()],
        sort_satisfied_by_index: false,
        selector_satisfied_by_index: false,
        start_keys: vec![],
        end_keys: vec![],
        inclusive_start: true,
        inclusive_end: true,
    };
    PreparedQuery::new(query, plan)
}

/// A query whose selector names `field`.
pub fn query_selecting(field: &str) -> PreparedQuery {
    let query = dxdb_storage::MangoQuery {
        selector: serde_json::json!({ field: { "$eq": "x" } }),
        sort: vec![dxdb_storage::SortPart::asc("id")],
        skip: 0,
        limit: None,
        index: None,
    };
    let plan = dxdb_storage::QueryPlan {
        index: vec!["_deleted".to_string(), "id".to_string()],
        sort_satisfied_by_index: false,
        selector_satisfied_by_index: false,
        start_keys: vec![],
        end_keys: vec![],
        inclusive_start: true,
        inclusive_end: true,
    };
    PreparedQuery::new(query, plan)
}

/// A query whose sort names `field`.
pub fn query_sorted_by(field: &str) -> PreparedQuery {
    let query = dxdb_storage::MangoQuery {
        selector: serde_json::json!({}),
        sort: vec![dxdb_storage::SortPart::asc(field)],
        skip: 0,
        limit: None,
        index: None,
    };
    let plan = dxdb_storage::QueryPlan {
        index: vec!["_deleted".to_string(), field.to_string()],
        sort_satisfied_by_index: false,
        selector_satisfied_by_index: false,
        start_keys: vec![],
        end_keys: vec![],
        inclusive_start: true,
        inclusive_end: true,
    };
    PreparedQuery::new(query, plan)
}

/// Unused-but-kept imports for the event types the wrapper's stream uses.
const _: () = {
    let _ = std::mem::size_of::<ChangeEvent>();
    let _ = std::mem::size_of::<EventBulk>();
    let _ = std::mem::size_of::<Checkpoint>();
    let _ = std::mem::size_of::<Operation>();
};
