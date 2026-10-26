//! Collections: the CRUD surface, and the layer that owns the cache and write queue.
//!
//! Ported from `src/rx-collection.ts` (1,362 lines). The behaviours that matter, and
//! where each comes from:
//!
//! | Behaviour | Upstream | Why it is load-bearing |
//! | --- | --- | --- |
//! | `bulkInsert` rejects duplicate ids in one call (`COL22`) | `:487` | The storage would accept both rows and the second would conflict opaquely; failing up front names the ids |
//! | a closed collection refuses writes (`COL21`) | `:394` `isWriteAllowed` | A write after `close()` must not reach a closed storage |
//! | `bulkInsert` returns `success` + `error`, and success is computed from the *rows minus errors* | `:527` | The storage only returns errors; reconstructing success is what keeps a bulk transfer small |
//! | `insertIfNotExists` swallows a `409` and returns the stored document | `:405` | Used by replication and by `upsert` |
//! | `modify`/`incrementalModify` go through the write queue | `:731`, `:812` | Two concurrent modifications must not lose one |
//! | `remove()` of a document writes a tombstone, never a physical delete | `:1148` | Deletes are revisions, so replication sees them |
//!
//! # Concurrency shape
//!
//! The cache and the queue are `RefCell`s, and no borrow is held across an `await`. That
//! is a real constraint rather than a stylistic one: holding a `RefCell` borrow across a
//! suspension point would let a second task re-enter and panic, which is the Rust
//! equivalent of the re-entrancy bugs the upstream write queue exists to prevent.

use std::cell::{Cell, RefCell};

use dxdb_storage::{
    BulkWriteResponse, DocumentData, DocumentWriteData, DynRxStorageInstance, PreparedQuery,
    RxStorageInstance, SortDirection, SortPart, WriteError,
};
use serde_json::Value;

use crate::cache::DocumentCache;
use crate::document::{Document, PatchOperation};
use crate::error::{CoreError, CoreResult};
use crate::hooks::{HookName, HookOutcome, HookPayload, Hooks};
use crate::queue::WriteQueue;
use crate::write::{assign_revisions, fill_object_data_before_insert, primary_of, PreparedRow};

/// The `context` string upstream uses for collection inserts
/// (`RX_COLLECTION_BULK_INSERT_CONTEXT`).
pub const BULK_INSERT_CONTEXT: &str = "rx-collection-bulk-insert";
/// The `context` upstream uses for an update
/// (`RX_COLLECTION_BULK_UPDATE_CONTEXT`).
pub const UPDATE_CONTEXT: &str = "rx-collection-bulk-update";
/// The `context` upstream uses for an incremental write.
pub const INCREMENTAL_WRITE_CONTEXT: &str = "rx-collection-incremental-write";

/// The outcome of a bulk write, mirroring upstream's
/// `{ success, error }`.
#[derive(Clone, Debug, Default)]
pub struct BulkResult {
    /// Documents that were written, as handles.
    pub success: Vec<Document>,
    /// Per-row failures, in row order. A `409` here is a normal outcome of a racing
    /// write, not an exceptional condition.
    pub error: Vec<WriteError>,
}

impl BulkResult {
    /// True when every row was written.
    pub fn is_ok(&self) -> bool {
        self.error.is_empty()
    }
}

/// One collection.
pub struct Collection {
    name: String,
    database_name: String,
    schema: dxdb_schema::Schema,
    storage: Box<DynRxStorageInstance<'static>>,
    cache: RefCell<DocumentCache>,
    queue: RefCell<WriteQueue>,
    hooks: RefCell<Hooks>,
    closed: Cell<bool>,
    /// Token used as the second half of every `_rev`.
    database_instance_token: String,
}

impl Collection {
    /// Builds a collection over an open storage instance.
    pub(crate) fn new(
        name: String,
        database_name: String,
        schema: dxdb_schema::Schema,
        storage: Box<DynRxStorageInstance<'static>>,
        database_instance_token: String,
    ) -> Self {
        Collection {
            name,
            database_name,
            schema,
            storage,
            cache: RefCell::new(DocumentCache::new()),
            queue: RefCell::new(WriteQueue::new()),
            hooks: RefCell::new(Hooks::new()),
            closed: Cell::new(false),
            database_instance_token,
        }
    }

    /// The collection name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The database that owns this collection.
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// The schema.
    pub fn schema(&self) -> &dxdb_schema::Schema {
        &self.schema
    }

    /// The schema version.
    pub fn version(&self) -> u32 {
        self.schema.version()
    }

    /// True after [`Collection::close`].
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// The hook registry this collection owns.
    pub fn hooks(&self) -> &RefCell<Hooks> {
        &self.hooks
    }

    /// Handles currently cached.
    pub fn cached_count(&self) -> usize {
        self.cache.borrow().len()
    }

    /// The `isWriteAllowed` check (`src/rx-collection-helper.ts:260`).
    fn ensure_write_allowed(&self) -> CoreResult<()> {
        if self.closed.get() {
            return Err(CoreError::CollectionClosed {
                collection: self.name.clone(),
            });
        }
        Ok(())
    }

    /// Builds a handle and records it in the cache.
    fn handle(&self, state: DocumentData) -> CoreResult<Document> {
        let primary = primary_of(&state, self.schema.primary_path())?;
        let cached = self
            .cache
            .borrow_mut()
            .insert(&primary, state.clone())?
            .clone();
        let mut document = Document::new(
            self.name.clone(),
            self.schema.primary_path(),
            cached.clone(),
        );
        // A `preCreateRxDocument` hook may rewrite the state before the handle exists.
        let mut payload = HookPayload::Document {
            collection: self.name.clone(),
            document: cached.clone(),
        };
        let outcome = self
            .hooks
            .borrow_mut()
            .fire(HookName::PreCreateRxDocument, &mut payload);
        if outcome == HookOutcome::Continue {
            if let HookPayload::Document {
                document: rewritten,
                ..
            } = payload
            {
                if rewritten != cached {
                    document = Document::new(
                        self.name.clone(),
                        self.schema.primary_path(),
                        rewritten.clone(),
                    );
                }
            }
        }
        Ok(document)
    }

    /// `insert($1)` (`src/rx-collection.ts:389`): one document, throws on conflict.
    pub async fn insert(&self, data: &Value) -> CoreResult<Document> {
        let result = self.bulk_insert(std::slice::from_ref(data)).await?;
        if let Some(error) = result.error.first() {
            return Err(match error {
                WriteError::Conflict { document_id, .. } => CoreError::Conflict {
                    id: document_id.clone(),
                },
                other => CoreError::Storage(format!(
                    "write failed with status {} for '{}'",
                    other.status(),
                    other.document_id()
                )),
            });
        }
        result
            .success
            .into_iter()
            .next()
            .ok_or_else(|| CoreError::ShouldNeverHappen("insert produced no document".to_string()))
    }

    /// `insertIfNotExists` (`src/rx-collection.ts:405`): a `409` returns the stored
    /// document instead of throwing.
    pub async fn insert_if_not_exists(&self, data: &Value) -> CoreResult<Document> {
        let result = self.bulk_insert(std::slice::from_ref(data)).await?;
        if result.is_ok() {
            return result
                .success
                .into_iter()
                .next()
                .ok_or_else(|| CoreError::ShouldNeverHappen("no document".to_string()));
        }
        match &result.error[0] {
            WriteError::Conflict {
                document_in_db: Some(stored),
                ..
            } => self.handle(stored.clone()),
            error => Err(CoreError::Storage(format!(
                "write failed with status {}",
                error.status()
            ))),
        }
    }

    /// `bulkInsert($1)` (`src/rx-collection.ts:418`).
    ///
    /// Duplicate primary values within one call are rejected before any write, with
    /// `COL22`, because letting them through would apply one row and conflict the other
    /// while reporting only a `409`.
    pub async fn bulk_insert(&self, documents: &[Value]) -> CoreResult<BulkResult> {
        self.ensure_write_allowed()?;
        // "do nothing when called with an empty array".
        if documents.is_empty() {
            return Ok(BulkResult::default());
        }

        let primary_path = self.schema.primary_path().to_string();
        let mut rows = Vec::with_capacity(documents.len());
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut duplicates = Vec::new();

        for document in documents {
            let filled = fill_object_data_before_insert(&self.schema, document, false)?;
            let primary = primary_of(&filled, &primary_path)?;
            // `ids.add(...)` then `ids.size !== docsData.length` upstream.
            if let Some(previous_position) = seen.insert(primary.clone(), rows.len()) {
                let _ = previous_position;
                if !duplicates.contains(&primary) {
                    duplicates.push(primary.clone());
                }
            }
            // Revisions are assigned before the storage sees the row, and before any
            // `preStorageWrite` hook so a hook can rewrite a revision-assigned row.
            rows.push(PreparedRow {
                previous: None,
                document: DocumentWriteData::new(filled),
            });
        }
        if !duplicates.is_empty() {
            return Err(CoreError::DuplicateIds { ids: duplicates });
        }

        let rows = assign_revisions(rows, &self.database_instance_token)?;
        let (response, written) = self
            .write_rows(rows, BULK_INSERT_CONTEXT.to_string())
            .await?;

        // Success is "the rows minus the failed ones", reconstructed from the rows that
        // were actually written rather than from the caller's input.
        let mut success = Vec::with_capacity(written.len());
        for row in written {
            let state = row.document.into_document_data();
            let primary = primary_of(&state, primary_path.as_str())?;
            self.queue.borrow_mut().observe(&primary, state.clone())?;
            success.push(self.handle(state)?);
        }
        Ok(BulkResult {
            success,
            error: response.error,
        })
    }

    /// Fires `preStorageWrite`, writes, and returns the storage response **and** the
    /// rows that were written.
    ///
    /// Returning the rows matters: the caller needs the *assigned* state (with `_rev`
    /// and `_meta`) to build a document handle. Recomputing that state from the input
    /// document loses both fields, which is exactly the bug that made every insert
    /// return a handle the cache rejected.
    async fn write_rows(
        &self,
        mut rows: Vec<PreparedRow>,
        context: String,
    ) -> CoreResult<(BulkWriteResponse, Vec<PreparedRow>)> {
        let mut payload = HookPayload::Write {
            collection: self.name.clone(),
            rows: std::mem::take(&mut rows),
            context: context.clone(),
        };
        let outcome = self
            .hooks
            .borrow_mut()
            .fire(HookName::PreStorageWrite, &mut payload);
        if outcome == HookOutcome::Stop {
            return Err(CoreError::ShouldNeverHappen(
                "a preStorageWrite hook vetoed the write".to_string(),
            ));
        }
        let rows = match payload {
            HookPayload::Write { rows, .. } => rows,
            other => {
                return Err(CoreError::ShouldNeverHappen(format!(
                    "preStorageWrite received the wrong payload: {}",
                    other.kind()
                )))
            }
        };

        let storage_rows: Vec<dxdb_storage::BulkWriteRow> = rows
            .iter()
            .map(|row| dxdb_storage::BulkWriteRow {
                previous: row.previous.clone(),
                document: row.document.clone(),
            })
            .collect();

        let response = self
            .storage
            .bulk_write(storage_rows, context)
            .await
            .map_err(|error| CoreError::Storage(error.to_string()))?;

        // Drop the rows the storage rejected, so the returned set is exactly what landed.
        let failed: std::collections::HashSet<String> = response
            .error
            .iter()
            .map(|error| error.document_id().to_string())
            .collect();
        let written = rows
            .into_iter()
            .filter(|row| {
                row.document
                    .id(self.schema.primary_path())
                    .map(|id| !failed.contains(id))
                    // A row with no usable id could not have been written either.
                    .unwrap_or(false)
            })
            .collect();
        Ok((response, written))
    }

    /// `findOne($1)` (`src/rx-collection.ts:856`): one query result.
    pub async fn find_one(&self, query: &Value) -> CoreResult<Option<Document>> {
        let mut query = query.clone();
        if let Some(map) = query.as_object_mut() {
            map.insert("limit".to_string(), Value::from(1));
        }
        let documents = self.find(&query).await?;
        Ok(documents.into_iter().next())
    }

    /// `find()` (`src/rx-collection.ts:834`): every matching document.
    pub async fn find(&self, query: &Value) -> CoreResult<Vec<Document>> {
        let states = self.run_query(query).await?;
        let mut documents = Vec::with_capacity(states.len());
        for state in states {
            documents.push(self.handle(state)?);
        }
        Ok(documents)
    }

    /// `findByIds($1)` (`src/rx-collection.ts:918`).
    ///
    /// Returns only documents that exist and are not deleted, which is what the query
    /// form of the same call does after `_deleted` normalisation.
    pub async fn find_by_ids(&self, ids: &[String]) -> CoreResult<Vec<Document>> {
        let states = self
            .storage
            .find_documents_by_id(ids.to_vec(), false)
            .await
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        let mut documents = Vec::with_capacity(states.len());
        for state in states {
            documents.push(self.handle(state)?);
        }
        Ok(documents)
    }

    /// `count()` (`src/rx-collection.ts:900`). Uses the storage's `count`, not
    /// `find().len()`, so an indexed count is fast and the two agree by construction.
    pub async fn count(&self, query: &Value) -> CoreResult<usize> {
        let prepared = self.prepare(query)?;
        let result = self
            .storage
            .count(prepared)
            .await
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        Ok(result.count)
    }

    /// Runs a query through the planner and then the storage.
    async fn run_query(&self, query: &Value) -> CoreResult<Vec<DocumentData>> {
        let prepared = self.prepare(query)?;
        let states = self
            .storage
            .query(prepared)
            .await
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        // Cache everything the query touched, so a subsequent `findOne` by id is served
        // from the cache and sees the same revision.
        self.cache
            .borrow_mut()
            .insert_many(states.clone(), self.schema.primary_path())?;
        Ok(states)
    }

    /// Normalises and plans a query, producing the storage ABI's `PreparedQuery`.
    ///
    /// This is the one place the collection meets `dxdb-query`. It also enforces
    /// upstream's rule that a query on a collection filters out tombstones: the
    /// `_deleted: false` selector is added here, not by the storage.
    fn prepare(&self, query: &Value) -> CoreResult<PreparedQuery> {
        let filled =
            crate::query::prepare(self.schema.json_schema(), query, self.schema.primary_path())
                .map_err(|error| CoreError::Query(error.to_string()))?;
        Ok(filled)
    }

    /// `update($1, $2)` / `incrementalModify` (`src/rx-collection.ts:731`).
    ///
    /// The modifier receives the **current stored state** and returns the next one; the
    /// queue guarantees it is not stale, and the revision is derived from `previous` by
    /// [`assign_revisions`]. Returning `None` writes nothing, which is how upstream's
    /// "modify returned the same document" case avoids burning a revision.
    pub async fn incremental_modify(
        &self,
        id: &str,
        mut modifier: impl FnMut(Option<&DocumentData>) -> CoreResult<Option<DocumentData>>,
    ) -> CoreResult<Option<Document>> {
        self.ensure_write_allowed()?;
        let primary_path = self.schema.primary_path().to_string();
        let stored = self
            .storage
            .find_documents_by_id(vec![id.to_string()], true)
            .await
            .map_err(|error| CoreError::Storage(error.to_string()))?
            .into_iter()
            .next();

        let mut next: Option<DocumentData> = None;
        let outcome: Option<()> = self.queue.borrow_mut().run(
            id,
            || Ok(stored.clone()),
            |current| {
                next = modifier(current)?;
                Ok(Some(()))
            },
        )?;
        let _ = outcome;

        let Some(next) = next else {
            return Ok(None);
        };
        if next.is_deleted() {
            return Err(CoreError::InvalidDocument(
                "a modification may not produce a tombstone; use remove()".to_string(),
            ));
        }

        let row = PreparedRow {
            previous: stored.clone(),
            document: DocumentWriteData::new(next),
        };
        let rows = assign_revisions(vec![row], &self.database_instance_token)?;
        let document = rows[0].document.document().clone();
        let (response, _written) = self
            .write_rows(rows, INCREMENTAL_WRITE_CONTEXT.to_string())
            .await?;
        if let Some(error) = response.error.first() {
            return Err(match error {
                WriteError::Conflict { document_id, .. } => CoreError::Conflict {
                    id: document_id.clone(),
                },
                other => CoreError::Storage(format!("status {}", other.status())),
            });
        }
        let primary = primary_of(&document, &primary_path)?;
        self.queue
            .borrow_mut()
            .observe(&primary, document.clone())?;
        Ok(Some(self.handle(document)?))
    }

    /// Applies a patch to a document (`modify` with a patch object,
    /// `src/rx-document.ts:461`).
    pub async fn modify(&self, id: &str, patch: PatchOperation) -> CoreResult<Option<Document>> {
        self.incremental_modify(id, |current| {
            let Some(current) = current else {
                return Ok(None);
            };
            let mut state = current.clone();
            patch.apply(&mut state);
            Ok(Some(state))
        })
        .await
    }

    /// `patch()` (`src/rx-document.ts:815`).
    pub async fn patch(&self, id: &str, patch: Value) -> CoreResult<Option<Document>> {
        self.modify(id, PatchOperation::merge(patch)).await
    }

    /// `RxDocument.remove()` (`src/rx-document.ts:197`): writes a tombstone.
    ///
    /// A delete is a revision, not a physical delete, so replication sees it and the
    /// storage can clean it up later.
    pub async fn remove(&self, id: &str) -> CoreResult<()> {
        self.ensure_write_allowed()?;
        let primary_path = self.schema.primary_path().to_string();
        let stored = self
            .storage
            .find_documents_by_id(vec![id.to_string()], false)
            .await
            .map_err(|error| CoreError::Storage(error.to_string()))?
            .into_iter()
            .next();
        let Some(stored) = stored else {
            // Upstream's `remove()` on an already-gone document is a no-op rather than an
            // error, because two callers may race to delete.
            return Ok(());
        };

        let mut tombstone = stored.clone();
        tombstone.set("_deleted", Value::Bool(true));
        // "Deleted documents must loose all their attachments."
        tombstone.set("_attachments", Value::Object(Default::default()));

        let rows = assign_revisions(
            vec![PreparedRow {
                previous: Some(stored),
                document: DocumentWriteData::new(tombstone),
            }],
            &self.database_instance_token,
        )?;
        let document = rows[0].document.document().clone();
        let (response, _written) = self.write_rows(rows, UPDATE_CONTEXT.to_string()).await?;
        if let Some(error) = response.error.first() {
            return Err(match error {
                WriteError::Conflict { document_id, .. } => CoreError::Conflict {
                    id: document_id.clone(),
                },
                other => CoreError::Storage(format!("status {}", other.status())),
            });
        }
        let primary = primary_of(&document, &primary_path)?;
        self.queue.borrow_mut().forget(&primary);
        self.cache.borrow_mut().remove(&primary);
        Ok(())
    }

    /// `close()` (`src/rx-collection.ts`). Closing twice is not an error, matching the
    /// storage contract.
    pub async fn close(&self) -> CoreResult<()> {
        if self.closed.replace(true) {
            return Ok(());
        }
        self.storage
            .close()
            .await
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        self.cache.borrow_mut().clear();
        self.queue.borrow_mut().clear();
        self.hooks.borrow_mut().clear();
        Ok(())
    }

    /// The storage instance, for a caller that needs to bypass the collection (a
    /// migration, a replication running on the underlying storage).
    pub fn storage(&self) -> &DynRxStorageInstance<'static> {
        &self.storage
    }
}

/// A sort part, re-exported so a query builder does not need `dxdb-storage`.
pub fn sort_part(field: impl Into<String>, direction: SortDirection) -> SortPart {
    SortPart {
        field: field.into(),
        direction,
    }
}
