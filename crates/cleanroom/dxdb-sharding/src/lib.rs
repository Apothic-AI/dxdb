//! The wrapper: one collection, N shards, unsharded-looking results.
//!
//! `specs/sharding.md`. INV-1 is the whole point: for every query the inner storage
//! supports, a sharded collection returns exactly what an unsharded one holding the same
//! documents returns — same documents, same order, same count.
//!
//! The pieces are deliberately separate so each can be tested on its own:
//!
//! * [`routing`] decides where a document lives (a pure, specified hash);
//! * [`merge`] turns N sorted shard results into one, then applies the window;
//! * this file wires them to the storage trait and forwards everything else.

#![forbid(unsafe_code)]

pub mod merge;
pub mod routing;

use std::sync::Arc;

use dxdb_storage::{
    BulkWriteResponse, ChangeStream, ChangedDocuments, Checkpoint, CountResult, DocumentData,
    DocumentWriteData, DynRxStorageInstance, EventBulk, InstanceCreationParams, PreparedQuery,
    RxStorage, RxStorageInstance, StorageError, StorageSchema, WriteError,
};

use crate::merge::{
    apply_window, merge_sorted, per_shard_limit, shard_query, sharded_count, MergeOrder,
};
use crate::routing::{shard_names, ShardingConfig};

/// Everything this crate can fail with.
///
/// The storage trait has one error type, so these are carried as
/// [`StorageError::InvalidSchema`] or `Unsupported` at the boundary; they exist so a
/// caller reading a message gets a real cause rather than "sharding failed".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShardingError {
    /// The `sharding` block is missing or malformed.
    #[error(transparent)]
    Config(#[from] routing::ConfigError),
    /// The collection was opened with a different shard count or mode than it was
    /// created with. Changing either means every document's address moves at once, so
    /// the storage would look empty rather than corrupted — `specs/sharding.md` 1.2.
    #[error("sharding config mismatch: stored {stored:?}, requested {requested:?}")]
    ConfigMismatch {
        /// What the database was created with.
        stored: ShardingConfig,
        /// What this open asked for.
        requested: ShardingConfig,
    },
    /// A shard's storage instance is absent.
    #[error("shard {0} is missing")]
    ShardMissing(u32),
}

/// A storage whose collections are split across shards.
pub struct ShardedStorage<S> {
    inner: S,
    config: ShardingConfig,
}

impl<S> ShardedStorage<S> {
    /// Wraps `inner` with `config`.
    pub fn new(inner: S, config: ShardingConfig) -> Self {
        Self { inner, config }
    }

    /// The wrapped storage.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The sharding configuration.
    pub fn config(&self) -> ShardingConfig {
        self.config
    }
}

/// The shard identity a collection was created with.
///
/// Persisted in the shard instance's *collection name* rather than in a separate
/// metadata document: the name is already part of the storage's collection key, so a
/// mismatch surfaces as a different set of instances rather than as data that reads
/// back wrong.
fn shard_marker(config: ShardingConfig) -> String {
    format!("shards{}:{}", config.shards, config.mode.as_str())
}

impl<S: RxStorage> RxStorage for ShardedStorage<S> {
    fn name(&self) -> &str {
        "dxdb-sharding"
    }

    async fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> Result<Box<DynRxStorageInstance<'static>>, StorageError> {
        let declared = ShardingConfig::from_schema(&params.schema.raw)
            .map_err(|error| StorageError::InvalidSchema(error.to_string()))?;
        let Some(declared) = declared else {
            // A schema without a `sharding` block on a sharded wrapper: pass through
            // unsharded. Documented in section 2 of the spec, because "wrapped but not
            // configured" is a real configuration and silently sharding it would be a
            // surprise.
            return self.inner.create_storage_instance(params).await;
        };
        if declared != self.config {
            return Err(StorageError::InvalidSchema(
                ShardingError::ConfigMismatch {
                    stored: declared,
                    requested: self.config,
                }
                .to_string(),
            ));
        }

        // Build every shard instance up front. A missing shard must fail at open, not
        // mid-query: an incomplete collection returning a subset looks like data loss
        // without being reported as such.
        let mut shards = Vec::with_capacity(self.config.shards.get() as usize);
        for shard in 0..self.config.shards.get() {
            let (database_name, collection_name) = shard_names(
                self.config.mode,
                &params.database_name,
                &params.collection_name,
                shard,
            );
            let mut shard_params = params.clone();
            shard_params.database_name = database_name;
            // The shard marker rides in the collection name so two configs cannot share
            // one set of instances.
            shard_params.collection_name =
                format!("{collection_name}#{}", shard_marker(self.config));
            let instance = self
                .inner
                .create_storage_instance(shard_params)
                .await
                .map_err(|error| {
                    StorageError::InvalidSchema(
                        ShardingError::ShardMissing(shard).to_string() + ": " + &error.to_string(),
                    )
                })?;
            shards.push(instance);
        }

        Ok(DynRxStorageInstance::new_box(ShardedInstance {
            shards,
            config: self.config,
            schema: params.schema,
        }))
    }
}

/// One sharded collection.
pub struct ShardedInstance {
    shards: Vec<Box<DynRxStorageInstance<'static>>>,
    config: ShardingConfig,
    schema: StorageSchema,
}

impl std::fmt::Debug for ShardedInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardedInstance")
            .field("shards", &self.shards.len())
            .field("mode", &self.config.mode)
            .finish_non_exhaustive()
    }
}

impl ShardedInstance {
    /// The shard a document id belongs to.
    fn shard_index(&self, document_id: &str) -> usize {
        self.config.shard_of(document_id) as usize
    }

    /// The shard a document write belongs to.
    ///
    /// Routed by the **write's** primary key, which is immutable by definition; routing
    /// on anything mutable would move a document between shards on update, which INV-3
    /// forbids.
    fn shard_of_write(&self, row: &dxdb_storage::BulkWriteRow) -> Result<usize, StorageError> {
        let id = row
            .document
            .id(&self.schema.primary_path)
            .map_err(|error| StorageError::InvalidDocument(error.to_string()))?;
        Ok(self.shard_index(id))
    }
}

impl RxStorageInstance for ShardedInstance {
    fn database_name(&self) -> &str {
        self.shards
            .first()
            .map(|shard| shard.database_name())
            .unwrap_or_default()
    }

    fn collection_name(&self) -> &str {
        self.shards
            .first()
            .map(|shard| shard.collection_name())
            .unwrap_or_default()
    }

    fn schema(&self) -> &StorageSchema {
        &self.schema
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Writes each row to its own shard and concatenates the per-row failures.
    ///
    /// INV-5: the per-row error *set* must match what an unsharded storage reports for
    /// the same rows. Because each row goes to exactly one shard and each shard's
    /// `bulk_write` is per-document atomic, the union of the shards' error lists is
    /// exactly the set of failing rows.
    ///
    /// Cross-shard writes are **not** atomic and cannot be: the inner contract has no
    /// cross-document transaction (PLAN.md 4.2), so the wrapper cannot invent one.
    /// Stated in the spec's failure table.
    async fn bulk_write(
        &self,
        document_writes: Vec<dxdb_storage::BulkWriteRow>,
        context: String,
    ) -> Result<BulkWriteResponse, StorageError> {
        // Bucket the rows by shard, preserving each row's position so the response's
        // error order is deterministic.
        let mut buckets: Vec<Vec<dxdb_storage::BulkWriteRow>> =
            (0..self.shards.len()).map(|_| Vec::new()).collect();
        for row in document_writes {
            let shard = self.shard_of_write(&row)?;
            buckets[shard].push(row);
        }
        let mut errors: Vec<WriteError> = Vec::new();
        for (shard_index, rows) in buckets.into_iter().enumerate() {
            if rows.is_empty() {
                continue;
            }
            let response = self.shards[shard_index]
                .bulk_write(rows, context.clone())
                .await?;
            errors.extend(response.error);
        }
        Ok(BulkWriteResponse::with_errors(errors))
    }

    /// Reads documents by primary key, each from its own shard.
    ///
    /// INV-3 and INV-10: `find_documents_by_id` groups by shard, and the result order
    /// matches the unsharded storage's, which is the order of the requested ids.
    async fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> Result<Vec<DocumentData>, StorageError> {
        let mut buckets: Vec<Vec<String>> = (0..self.shards.len()).map(|_| Vec::new()).collect();
        for id in &ids {
            buckets[self.shard_index(id)].push(id.clone());
        }
        let mut found: std::collections::HashMap<String, DocumentData> =
            std::collections::HashMap::with_capacity(ids.len());
        for (shard_index, shard_ids) in buckets.into_iter().enumerate() {
            if shard_ids.is_empty() {
                continue;
            }
            let documents = self.shards[shard_index]
                .find_documents_by_id(shard_ids, with_deleted)
                .await?;
            for document in documents {
                let id = document
                    .id(&self.schema.primary_path)
                    .map_err(|error| StorageError::InvalidDocument(error.to_string()))?
                    .to_string();
                found.insert(id, document);
            }
        }
        // Return in the order the caller asked, which is what an unsharded storage does.
        Ok(ids.into_iter().filter_map(|id| found.remove(&id)).collect())
    }

    /// Runs the query on every shard and merges the results.
    ///
    /// INV-1. Each shard gets the same selector, sort and index with the window widened
    /// to `skip + limit` and its own `skip` cleared; the global window is applied after
    /// the merge (`specs/sharding.md` 3.2).
    async fn query(&self, prepared: PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        let order = MergeOrder::for_prepared(&prepared, &self.schema.primary_path);
        let shard_limit = per_shard_limit(&prepared);
        let per_shard = shard_query(&prepared, shard_limit);

        let mut results = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            results.push(shard.query(per_shard.clone()).await?);
        }
        let merged = merge_sorted(&order, results);
        Ok(apply_window(
            merged,
            prepared.query.skip,
            prepared.query.limit,
        ))
    }

    /// Counts by asking every shard and applying the clamp rule.
    ///
    /// INV-2: the result must equal the length the equivalent query returns, which is
    /// why this is not a plain sum (`specs/sharding.md` 3.3).
    async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        let shard_limit = per_shard_limit(&prepared);
        let per_shard_query = shard_query(&prepared, shard_limit);
        let mut counts = Vec::with_capacity(self.shards.len());
        let mut modes = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            let result = shard.count(per_shard_query.clone()).await?;
            counts.push(result.count);
            modes.push(result.mode);
        }
        let count = sharded_count(&counts, prepared.query.skip, prepared.query.limit);
        // If any shard had to scan, the count is a slow count: a caller that set
        // `allowSlowCount: false` must not be told this was index-only.
        let mode = if modes
            .iter()
            .any(|mode| matches!(mode, dxdb_storage::CountMode::Slow))
        {
            dxdb_storage::CountMode::Slow
        } else {
            dxdb_storage::CountMode::Fast
        };
        Ok(CountResult { count, mode })
    }

    /// Reads an attachment from its document's shard.
    ///
    /// INV-10: routing by document id means an attachment never needs a cross-shard
    /// lookup and never becomes unreachable.
    async fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
        digest: String,
    ) -> Result<Vec<u8>, StorageError> {
        let shard = self.shard_index(&document_id);
        self.shards[shard]
            .get_attachment_data(document_id, attachment_id, digest)
            .await
    }

    /// The change feed, merged across shards.
    ///
    /// INV-6: every change reaches every subscriber exactly once. The shards' feeds are
    /// concatenated rather than merged by checkpoint, because a checkpoint is
    /// per-storage and the events of one shard do not order against another's; the
    /// consumer's `EventBulk` contract is per-bulk, and each bulk remains valid.
    fn change_stream(&self) -> ChangeStream {
        let streams: Vec<ChangeStream> = self
            .shards
            .iter()
            .map(|shard| shard.change_stream())
            .collect();
        Box::pin(futures::stream::select_all(streams))
    }

    /// Cleans every shard; `false` if **any** shard has more work.
    ///
    /// INV-9. Folding with `&&` is the only correct combination: returning `true` when
    /// one shard still has tombstones would make a caller stop early and leave them.
    async fn cleanup(&self, minimum_deleted_time: f64) -> Result<bool, StorageError> {
        let mut all_clean = true;
        for shard in &self.shards {
            let cleaned = shard.cleanup(minimum_deleted_time).await?;
            all_clean = all_clean && cleaned;
        }
        Ok(all_clean)
    }

    async fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<Checkpoint>,
    ) -> Result<ChangedDocuments, StorageError> {
        // Merge the shards' feeds in the feed's own order. Each shard returns at most
        // `limit`; taking the first `limit` of the merged sequence is the same
        // reasoning as INV-1's window, over the `(_meta.lwt, primary)` order the feed
        // is defined in.
        let order = MergeOrder::for_change_feed(&self.schema.primary_path);
        let mut results = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            let changed = shard
                .get_changed_documents_since(limit, checkpoint.clone())
                .await?;
            results.push(changed.documents);
        }
        let merged = merge_sorted(&order, results);
        let documents = apply_window(merged, 0, Some(limit));
        let checkpoint = match documents.last() {
            Some(last) => Checkpoint::new(
                last.id(&self.schema.primary_path)
                    .map_err(|error| StorageError::InvalidDocument(error.to_string()))?
                    .to_string(),
                last.lwt().unwrap_or(0.0),
            ),
            // No documents: keep the caller's checkpoint so the next call resumes where
            // this one started rather than from the beginning.
            None => checkpoint.unwrap_or_else(Checkpoint::beginning),
        };
        Ok(ChangedDocuments {
            documents,
            checkpoint,
        })
    }

    async fn close(&self) -> Result<(), StorageError> {
        // Every shard is closed even if one fails, so one bad shard cannot leave the
        // rest open and leaking.
        let mut first_error = None;
        for shard in &self.shards {
            if let Err(error) = shard.close().await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn remove(&self) -> Result<(), StorageError> {
        let mut first_error = None;
        for shard in &self.shards {
            if let Err(error) = shard.remove().await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Unused-import guard: these are part of the trait surface this file implements but
/// not all appear in a single code path.
const _: () = {
    let _ = std::mem::size_of::<DocumentWriteData>();
    let _ = std::mem::size_of::<EventBulk>();
    let _ = std::mem::size_of::<Arc<()>>();
};
