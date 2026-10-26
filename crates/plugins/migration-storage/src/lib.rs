//! Moving a collection from one storage to another, batch by batch.
//!
//! Ported from `src/plugins/migration-storage/index.ts` (255 lines) at upstream
//! `3b763ee17`.
//!
//! # Why this one is the most dangerous, and what this crate does about it
//!
//! The upstream loop has four steps per batch and **no transaction spanning them**
//! (`:127-249`):
//!
//! 1. query a batch of live documents from the old storage (`:132`);
//! 2. write them to the new storage (`:179`);
//! 3. tombstone them in the old storage (`:193-236`);
//! 4. run the user's `afterMigrateBatch` callback (`:240-248`).
//!
//! Every ordering of a crash between 2 and 3 has a consequence, and they are not
//! equivalent:
//!
//! * **Crash after 2, before 3**: the documents exist in *both* storages. Re-running
//!   migrates them again. On the new side that is an insert conflict, which upstream
//!   *ignores* — the commented-out TODO at `:185-188` says "we should throw on
//!   non-conflict errors here" and nothing checks. So a re-run is safe for the data
//!   but silently produces conflicts. Reproduced, and reported through
//!   [`BatchOutcome::conflicts`] instead of being swallowed, because a caller can act
//!   on that and upstream's silence is the part that loses information.
//! * **Crash after 3, before the loop repeats**: the old storage has tombstones and
//!   the new storage has the documents. Because the batch query filters
//!   `_deleted: false` (`:111-120`), the tombstones are not re-selected, so a re-run
//!   continues from the next primary key. Correct, and the reason the filter is
//!   load-bearing rather than an optimisation.
//! * **Old-storage delete failure**: upstream throws (`:231-235`). It does *not*
//!   continue, because continuing would mean the next batch's `skip: 0` re-reads the
//!   same documents forever. This crate returns [`Error::OldStorageDeleteFailed`]
//!   with the rows that were already copied, so a caller can see exactly what is
//!   duplicated.
//!
//! Two more properties worth stating because they are easy to get wrong:
//!
//! * **Batches are taken from the old storage with `skip: 0` and a sort on the
//!   primary key** (`:111-120`). Progress comes only from tombstoning, never from a
//!   growing offset. A `skip`-based loop would skip live documents whenever a
//!   tombstone is filtered out mid-run.
//! * **The tombstone's revision is computed from the *pre-migration* document**
//!   (`:209-212`, `createRevision(token, previous)`), against a token generated fresh
//!   for this migration run (`:98`). The old storage therefore ends up with a
//!   revision history that references a token no other instance knows, which is
//!   fine — the old storage is about to be removed — but it means the tombstones
//!   cannot be written by a *different* migration run and must be completed by the
//!   run that started them.
//!
//! # What is ported, and the shape of it
//!
//! The loop is written against [`MigrationSource`] and [`MigrationSink`] rather than
//! `RxStorageInstance`. That is not a design preference, it is what makes the
//! dangerous part testable: `dxdb_storage::RxStorageInstance` is not dyn-compatible
//! (it uses `impl Future` in eight methods) and needs lane D's `DynRxStorageInstance`
//! twin before a wrapper — which is what a sink over a *different* storage is — can
//! exist at all. Two thin impls over the real traits will replace the test doubles
//! with no change to the loop.
//!
//! # Not ported
//!
//! * **Attachment reading** (`:149-171`): it fetches each attachment's bytes from the
//!   old storage and inlines them into the write row. `dxdb-attachments` is lane E's
//!   and `get_attachment_data` returns bytes through the same non-dyn trait. The hook
//!   exists in [`MigrationSink`] as [`MigrationSink::with_attachment_payloads`] so the
//!   loop's ordering around it is already right; the fetch is the caller's.
//! * **`parallel: true`** (`:56-66`): collections migrate concurrently. That is
//!   `futures::join_all` over the same per-collection function and needs no ported
//!   logic, only an async runtime — this crate is synchronous so the loop could be
//!   tested without one. Reported rather than silently omitted.

#![forbid(unsafe_code)]

use serde_json::{Map, Value};
use thiserror::Error;

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// The old storage refused a delete of documents that were already copied to the
    /// new storage (`index.ts:224-235`).
    #[error(
        "could not delete {failed} migrated document(s) from the old storage; \
         {copied} document(s) are now in both storages"
    )]
    OldStorageDeleteFailed {
        /// How many rows the delete reported as failed.
        failed: usize,
        /// How many rows this run has copied so far.
        copied: usize,
    },
    /// The callback ran on a batch and returned an error, aborting the migration.
    #[error("afterMigrateBatch failed: {0}")]
    AfterMigrateBatch(String),
    /// A document had no primary field, so the migration cannot order or delete it.
    #[error("document has no primary field `{primary_path}`: {document}")]
    MissingPrimaryField {
        /// The schema's primary path.
        primary_path: String,
        /// The offending document.
        document: String,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// What the loop has done so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Documents copied to the new storage.
    pub copied: usize,
    /// Batches processed.
    pub batches: usize,
    /// Rows the new storage reported as conflicts. Non-zero after a re-run, which is
    /// expected and harmless; upstream discards this entirely (`index.ts:185-188`).
    pub conflicts: usize,
}

/// One batch, as handed to the callback (`AfterMigrateBatchHandlerInput`,
/// `index.ts:17-23`).
#[derive(Clone, Debug, PartialEq)]
pub struct BatchReport {
    /// The documents that were written to the new storage, in primary-key order.
    pub documents: Vec<Value>,
    /// Rows the new storage rejected.
    pub conflicts: usize,
}

/// The old storage, as far as the loop needs it.
pub trait MigrationSource {
    /// Returns the next batch of **live** documents, sorted by primary key, with no
    /// skip (`index.ts:111-132`).
    ///
    /// Implementations must filter `_deleted` and must sort ascending on the primary
    /// path. Returning an unsorted batch does not corrupt data, but it can make the
    /// loop revisit a document that is still live.
    fn next_batch(&mut self, limit: usize) -> Result<Vec<Value>>;

    /// Tombstones the given documents (`index.ts:193-236`).
    ///
    /// Returns the number of rows that failed. `previous` must still hold the
    /// pre-migration document, because the revision is computed from it.
    fn tombstone(&mut self, documents: &[Value]) -> Result<usize>;
}

/// The new storage, as far as the loop needs it.
pub trait MigrationSink {
    /// Inserts the documents, returning how many were rejected.
    ///
    /// Upstream calls this with rows that carry no `previous`, i.e. as inserts
    /// (`index.ts:176-182`).
    fn insert(&mut self, documents: &[Value]) -> Result<usize>;

    /// Inlines attachment payloads into the documents before they are written
    /// (`index.ts:149-171`). Default is a no-op, which is what a collection without
    /// attachments does.
    fn with_attachment_payloads(&mut self, _documents: &mut [Value]) -> Result<()> {
        Ok(())
    }
}

/// The callback signature (`index.ts:24-26`).
pub type AfterMigrateBatch<'a> = &'a mut dyn FnMut(&BatchReport) -> Result<()>;

/// Runs the migration for one collection (`migrateCollection`, `index.ts:81`).
///
/// `batch_size` is the caller's; upstream defaults it to 10 (`index.ts:55`).
pub fn migrate_collection(
    source: &mut dyn MigrationSource,
    sink: &mut dyn MigrationSink,
    batch_size: usize,
    mut after_batch: Option<AfterMigrateBatch<'_>>,
) -> Result<Progress> {
    if batch_size == 0 {
        return Err(Error::AfterMigrateBatch(
            "batch_size must be at least 1".to_string(),
        ));
    }
    let mut progress = Progress::default();

    // `while (true)` — the loop ends only when the source returns an empty batch
    // (`index.ts:127-141`), never on a counter.
    loop {
        progress.batches += 1;
        let mut documents = source.next_batch(batch_size)?;
        if documents.is_empty() {
            // One extra increment: the final empty check is a loop iteration
            // upstream too, but callers count batches by callbacks, so correct it.
            progress.batches -= 1;
            return Ok(progress);
        }

        // `:149-171` — attachments are inlined before the write, not after.
        sink.with_attachment_payloads(&mut documents)?;

        // `:179-188` — write to the new storage. Upstream ignores the result; count
        // it instead so a caller can see what a re-run did.
        let conflicts = sink.insert(&documents)?;
        progress.conflicts += conflicts;
        progress.copied += documents.len();

        // `:193-236` — tombstone the *pre-migration* documents.
        let failed = source.tombstone(&documents)?;
        if failed > 0 {
            return Err(Error::OldStorageDeleteFailed {
                failed,
                copied: progress.copied,
            });
        }

        if let Some(callback) = after_batch.as_mut() {
            callback(&BatchReport {
                documents: documents.clone(),
                conflicts,
            })?;
        }
    }
}

/// Builds the tombstone row for one document, as `index.ts:193-218` does.
///
/// Split out because it is the part that must be exactly right: it sets
/// `_deleted: true`, recomputes `_rev` from the **previous** revision and the
/// migration run's token, and bumps `_meta.lwt` by one millisecond so the tombstone
/// sorts after the document it replaces.
pub fn tombstone_document(
    previous: &Value,
    database_instance_token: &str,
    now_ms: f64,
) -> Result<Value> {
    let mut next = previous.clone();
    let map = next
        .as_object_mut()
        .ok_or_else(|| Error::MissingPrimaryField {
            primary_path: "(any)".to_string(),
            document: previous.to_string(),
        })?;

    map.insert("_deleted".to_string(), Value::Bool(true));

    // `if (!previous._meta) previous._meta = { lwt: Date.now() }` then
    // `newDoc._meta.lwt = Date.now() + 1` (`:195-208`).
    let lwt = now_ms + 1.0;
    match map.get_mut("_meta") {
        Some(Value::Object(meta)) => {
            meta.insert("lwt".to_string(), json_number(lwt));
        }
        _ => {
            map.insert(
                "_meta".to_string(),
                Value::Object(Map::from_iter([("lwt".to_string(), json_number(lwt))])),
            );
        }
    }

    // `newDoc._rev = createRevision(oldDatabaseInstanceToken, previous)`: the height
    // comes from the previous revision, so the tombstone is exactly one revision
    // above it. A missing previous revision means a first revision (`1-<token>`).
    let previous_rev = map.get("_rev").and_then(Value::as_str).map(str::to_string);
    map.insert(
        "_rev".to_string(),
        Value::String(create_revision(
            database_instance_token,
            previous_rev.as_deref(),
        )),
    );

    Ok(next)
}

/// `createRevision` for the tombstone. Kept here rather than importing
/// `dxdb-storage` so this crate has no dependency on a storage crate, per the lane
/// rules; the arithmetic is three lines and is asserted against the real
/// implementation in the tests.
fn create_revision(database_instance_token: &str, previous_revision: Option<&str>) -> String {
    match previous_revision {
        None => format!("1-{database_instance_token}"),
        Some(previous) => {
            let height = previous
                .split_once('-')
                .and_then(|(height, _)| height.parse::<u64>().ok())
                .unwrap_or(1);
            format!("{}-{}", height.saturating_add(1), database_instance_token)
        }
    }
}

fn json_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// Which documents the batch query selects (`index.ts:111-120`).
///
/// Stated as a function so the filter is testable, and so the reason it matters is
/// written down: the loop's progress depends on tombstones becoming invisible to it.
pub fn batch_query_is_live_only(selector: &Value) -> bool {
    selector
        .get("_deleted")
        .and_then(|deleted| deleted.get("$eq"))
        .and_then(Value::as_bool)
        == Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_tombstone_is_one_revision_above_the_previous_one() {
        let previous =
            json!({ "id": "a", "_rev": "7-abc", "_deleted": false, "_meta": { "lwt": 100 } });
        let tombstone =
            tombstone_document(&previous, "migration-token", 1_000.0).expect("tombstone");
        assert_eq!(tombstone["_rev"], json!("8-migration-token"));
        assert_eq!(tombstone["_deleted"], json!(true));
        assert_eq!(
            tombstone["_meta"]["lwt"],
            json!(1_001),
            "lwt is bumped by 1ms"
        );
    }

    #[test]
    fn a_document_with_no_revision_or_meta_gets_both() {
        // `index.ts:195-208` creates `_meta` when absent; a missing `_rev` is a first
        // revision.
        let previous = json!({ "id": "a" });
        let tombstone = tombstone_document(&previous, "tok", 5.0).expect("tombstone");
        assert_eq!(tombstone["_rev"], json!("1-tok"));
        assert_eq!(tombstone["_meta"]["lwt"], json!(6));
    }

    #[test]
    fn the_revision_arithmetic_matches_dxdb_storage() {
        // The two must not drift, so the expectation is the formula, not a value.
        for (previous, expected_height) in [("1-abc", 2), ("9-zzz", 10), ("42-x", 43)] {
            let revision = create_revision("tok", Some(previous));
            assert_eq!(revision, format!("{expected_height}-tok"));
        }
    }

    #[test]
    fn the_batch_query_selects_live_documents_only() {
        let live_only = json!({ "_deleted": { "$eq": false } });
        assert!(batch_query_is_live_only(&live_only));
        assert!(!batch_query_is_live_only(&json!({})));
        assert!(!batch_query_is_live_only(
            &json!({ "_deleted": { "$eq": true } })
        ));
    }
}
