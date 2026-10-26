//! The upstream: pushing the fork's changes to the master.
//!
//! Ports `src/replication-protocol/upstream.ts` (595 lines).
//!
//! # The observable contract
//!
//! Two things leave and one thing comes back:
//!
//! * **out**: `{assumedMasterState, newDocumentState}` per document, in batches of at
//!   most `push_batch_size`;
//! * **back**: the master's real state, **for conflicts only** — an empty vector means
//!   every row was accepted.
//!
//! Everything else here exists to make those two statements true while surviving a
//! restart, a pause, and two directions writing at once.
//!
//! # The four mechanisms, and what each one prevents
//!
//! 1. **Assumed-master bookkeeping.** For each document the meta instance records what
//!    the fork believes the master holds. That record is what makes a push expressible as
//!    a compare-and-set instead of a blind overwrite, and it is why the pull writes meta
//!    rows too.
//! 2. **The own-write filter.** A downstream write lands on the fork and would come
//!    straight back on the fork's change stream, pushing the master's own data back to
//!    it. The bulk-write `context` identifies those, and
//!    [`Upstream`] skips their documents while still advancing the checkpoint.
//! 3. **The pause/cancel re-check after `master_write`.** Without it, a pause mid-push
//!    leaves rows that were never sent but are about to be recorded as sent, and they are
//!    never retried. Upstream added this deliberately (`upstream.ts:435-446`); the test
//!    `pause_mid_push_leaves_the_document_pending` is the assertion that it survives the
//!    port.
//! 4. **The initial-sync re-run.** A conflict resolved during the initial sync creates a
//!    new fork write. If the sync stopped there, that write would wait for the change
//!    stream; instead it re-runs until a pass reports no conflict writes.
//!
//! # Why the loops are pumped rather than awaited
//!
//! D2: the protocol is driven by the caller's executor, never by an internal runtime. So
//! [`Upstream::pump`] does as much work as it can and returns; the caller loops until
//! [`Upstream::is_idle`]. That is the same shape as upstream's promise chain, without
//! pretending there is an event loop.

use std::collections::{BTreeMap, HashSet};

use dxdb_json::Value;
use dxdb_storage::{
    BulkWriteRow, ChangeStream, DocumentData, DocumentWriteData, EventBulk, RxStorageInstance,
    StorageError, WriteError,
};

use crate::attachments::fill_write_data_for_attachments_change;
use crate::checkpoint::{last_checkpoint, set_checkpoint, Direction};
use crate::conflict::{is_conflict_error, resolve_conflict_error, ConflictHandlerInput};
use crate::convert::{strip_attachments_data_from_meta_write_rows, write_doc_to_doc_state};
use crate::meta_instance::{get_assumed_master_state, get_meta_write_row};
use crate::state::{PullStreamItem, ReplicationState, WriteToMasterRow};

/// What one pump did, so a caller can decide whether to keep going and a test can assert
/// on the work rather than on a sleep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PumpOutcome {
    /// Documents pushed to the master in this pump.
    pub pushed: usize,
    /// Conflicts the master reported in this pump.
    pub conflicts: usize,
    /// Conflict-resolution writes made to the fork in this pump.
    pub conflict_writes: usize,
    /// True when the initial sync ran to a fixpoint with no conflict writes, so
    /// `first_sync_done` was set.
    pub first_sync_completed: bool,
}

impl PumpOutcome {
    /// True when the pump made observable progress.
    pub fn made_progress(&self) -> bool {
        self.pushed > 0 || self.conflicts > 0 || self.conflict_writes > 0
    }
}

/// What one [`Upstream::push_batch`] call did, plus where its batch ended.
///
/// Not part of [`PumpOutcome`] because the batch end is protocol plumbing rather than
/// something a caller reports on, and because a `Checkpoint` is not `Copy` — keeping the
/// public summary `Copy` is worth one internal type.
struct PushedBatch {
    /// The counters this batch contributed.
    outcome: PumpOutcome,
    /// The protocol checkpoint at the end of the batch, when the batch carried documents.
    /// The caller advances the persisted checkpoint from this, so `push_batch` has no
    /// write to the checkpoint row itself.
    batch_end: Option<crate::checkpoint::Checkpoint>,
}

/// The upstream: everything the push direction owns.
///
/// Holds the fork's change stream between pumps, because a stream is stateful: dropping it
/// between pumps would lose whatever arrived in between.
pub struct Upstream<S: RxStorageInstance> {
    state: ReplicationState<S>,
    fork_stream: ChangeStream,
    /// The last checkpoint read from the meta instance, so a pump does not re-read it.
    last_checkpoint: Option<dxdb_storage::Checkpoint>,
    /// The protocol checkpoint corresponding to the last document this upstream handled.
    ///
    /// Separate from `last_checkpoint` (the *storage* checkpoint, which is what
    /// `get_changed_documents_since` takes) because they are different types with different
    /// encodings: the storage's is `{id, lwt}` for its own feed, and the one persisted in
    /// the meta instance is the protocol's opaque checkpoint. Advancing the persisted one
    /// from the batch we actually pushed is the only way it stays truthful while writes are
    /// arriving.
    last_pushed_checkpoint: Option<crate::checkpoint::Checkpoint>,
    /// Monotonic task counter, upstream's `timer` (`upstream.ts:78-80`).
    timer: u64,
    /// The timer value at the start of the most recent initial-sync fetch.
    ///
    /// Upstream's `initialSyncStartTime` (`upstream.ts:82`). A change-stream bulk stamped
    /// *before* this was already covered by that fetch, so processing it again would push the
    /// same document twice — harmless on the master, but it doubles the conflict events and
    /// makes `persistToMaster` counters meaningless. Dropping such tasks is upstream's
    /// `if (taskWithTime.time < initialSyncStartTime) continue`
    /// (`upstream.ts:236-243`), and it is the ordering rule the port plan flagged as the
    /// easiest part of the push to lose.
    initial_sync_start_time: u64,
    /// Telemetry: what the last `master_write` batch looked like.
    last_push_batch: Vec<WriteToMasterRow>,
    last_conflicts: Vec<DocumentData>,
}

impl<S: RxStorageInstance> Upstream<S> {
    /// Builds the upstream and subscribes to the fork's change stream.
    ///
    /// Upstream subscribes at start and keeps the subscription until cancellation
    /// (`upstream.ts:73-95`); keeping the stream here is the same lifetime without an
    /// unsubscribe to forget.
    pub fn new(state: ReplicationState<S>) -> Self {
        let fork_stream = state.fork().change_stream();
        Upstream {
            state,
            fork_stream,
            last_checkpoint: None,
            last_pushed_checkpoint: None,
            timer: 0,
            initial_sync_start_time: 0,
            last_push_batch: Vec::new(),
            last_conflicts: Vec::new(),
        }
    }

    /// The replication state.
    pub fn state(&self) -> &ReplicationState<S> {
        &self.state
    }

    /// The rows handed to `master_write` by the most recent push.
    pub fn last_push_batch(&self) -> &[WriteToMasterRow] {
        &self.last_push_batch
    }

    /// The real master states the most recent push received back.
    pub fn last_conflicts(&self) -> &[DocumentData] {
        &self.last_conflicts
    }

    /// Seeds the checkpoint if the caller supplied an initial one and none is stored.
    ///
    /// Ports `upstream.ts:44-56`. An **existing** stored checkpoint always wins; this only
    /// seeds a fresh replication. Draining the checkpoint queue at the end makes the write
    /// real before the first push, which is what "resume from here" requires.
    pub async fn seed_initial_checkpoint(&mut self) -> Result<(), StorageError> {
        let Some(initial) = self.state.initial_checkpoint_for(Direction::Up).cloned() else {
            return Ok(());
        };
        let existing = last_checkpoint(&self.state, Direction::Up).await?;
        if existing.is_none() {
            set_checkpoint(&self.state, Direction::Up, Some(&initial)).await?;
        }
        self.state.checkpoint_queue().drain();
        Ok(())
    }

    /// Drains the fork's change stream into a map of pending documents, plus a `RESYNC`
    /// flag.
    ///
    /// The own-write filter lives here rather than in the push loop because the filter is
    /// a property of the *event*: the bulk carries the context of the write that caused it,
    /// and a bulk whose context is the downstream flag contributes no documents. Its
    /// checkpoint is still consumed, because the checkpoint is about position, not about
    /// who wrote it (`upstream.ts:300-306`).
    fn collect_changes(&mut self) -> (BTreeMap<String, DocumentData>, bool) {
        let mut docs: BTreeMap<String, DocumentData> = BTreeMap::new();
        let mut resync = false;
        let primary_path = self.state.primary_path().to_string();
        let own_flag = self.state.downstream_bulk_write_flag().to_string();

        // Stamp every bulk as it is drained, then drop the ones the last initial-sync fetch
        // already covered. The stamp is taken *before* the fetch's own timestamp is
        // considered, which is what makes the comparison meaningful: a bulk that arrived
        // earlier is older than the fetch that superseded it.
        let collected: Vec<(u64, EventBulk)> = crate::drain_change_stream(&mut self.fork_stream)
            .into_iter()
            .map(|bulk| {
                self.timer += 1;
                (self.timer, bulk)
            })
            .collect();

        for (arrived_at, bulk) in collected {
            self.state
                .update_up_stats(|stats| stats.fork_change_stream_emit += 1);
            resync = resync || self.bulk_requests_resync(&bulk);
            if arrived_at < self.initial_sync_start_time {
                // Already covered by that fetch; pushing it again would be a duplicate.
                continue;
            }
            if bulk.context == own_flag {
                // Written by our own pull. Pushing it would send the master its own data,
                // and the assumed-master state already records it.
                continue;
            }
            for event in bulk.events {
                // A deleted document still has to be pushed: the master needs the
                // tombstone. `document_data` carries the tombstone.
                if let Ok(id) = event.document_data.id(&primary_path) {
                    docs.insert(id.to_string(), event.document_data);
                }
            }
        }
        (docs, resync)
    }

    /// True when a bulk asks for a resync.
    ///
    /// The protocol's resync signal arrives on the *master* stream (a transport that lost
    /// events), so a fork bulk never asks for one. Kept as a named function because the
    /// mirror of this check on the downstream side does read it, and a reader comparing the
    /// two files should see the difference stated rather than infer it.
    fn bulk_requests_resync(&self, _bulk: &EventBulk) -> bool {
        false
    }

    /// Runs one unit of push work.
    ///
    /// The order is upstream's: initial sync first (checkpoint iteration over the fork),
    /// then whatever the change stream has queued. Doing it the other way round would push
    /// a live event before the backlog, and the master would see the document move
    /// backwards.
    pub async fn pump(&mut self) -> Result<PumpOutcome, StorageError> {
        if self.state.is_stopped_or_paused() {
            return Ok(PumpOutcome::default());
        }

        let mut outcome = PumpOutcome::default();
        let (pending, _resync) = self.collect_changes();
        if !pending.is_empty() {
            self.state.update_up_stats(|stats| stats.process_tasks += 1);
            let docs: Vec<DocumentData> = pending.into_values().collect();
            let result = self.push_batch(docs).await?;
            outcome.pushed += result.outcome.pushed;
            outcome.conflicts += result.outcome.conflicts;
            outcome.conflict_writes += result.outcome.conflict_writes;
            if let Some(batch_end) = result.batch_end.clone() {
                // A batch that reported no end was abandoned by a pause or cancel; its
                // position must not be recorded.
                if !self.state.is_stopped_or_paused() {
                    set_checkpoint(&self.state, Direction::Up, Some(&batch_end)).await?;
                    self.state.checkpoint_queue().drain();
                    self.last_pushed_checkpoint = Some(batch_end);
                }
            }
        }

        // The initial sync is what establishes `first_sync_done`, and it re-runs while it
        // produces conflict writes. Running it after the change-stream drain means a live
        // write that arrived during the previous pump is included in this pass's
        // checkpoint iteration, which is why it is not duplicated.
        //
        // It runs on **every** pump, not only until the first sync completes. Upstream runs
        // `upstreamInitialSync()` unconditionally when the replication starts
        // (`upstream.ts:71-75`) because the fork's change stream only covers what happens
        // while the replication is running: anything written while it was stopped is
        // discoverable solely by iterating from the persisted checkpoint. Gating this on
        // `first_sync_done` makes a restarted upstream silently skip every document written
        // while it was down. The loop is cheap when there is nothing to do (one empty
        // `get_changed_documents_since`), so the cost of always running it is one read per
        // pump in exchange for never losing a backlog.
        let ran = self.initial_sync().await?;
        outcome.pushed += ran.pushed;
        outcome.conflicts += ran.conflicts;
        outcome.conflict_writes += ran.conflict_writes;
        outcome.first_sync_completed = ran.first_sync_completed;

        Ok(outcome)
    }

    /// Iterates the fork by checkpoint until the fork has nothing more, pushing each batch.
    ///
    /// Ports `upstreamInitialSync` (`upstream.ts:127-186`). The loop, and why each exit
    /// condition is there:
    ///
    /// * **empty batch**: the fork has no more changes after the checkpoint.
    /// * **cancelled**: checked before every read, because "the fork instance can already be
    ///   closed at this point, so we must not read from it anymore" (`upstream.ts:157-165`).
    /// * **conflict writes**: if any batch produced conflict-resolution writes, those are
    ///   new fork revisions that this pass's checkpoint already moved past, so the pass
    ///   re-runs from the stored checkpoint rather than reporting itself done.
    pub async fn initial_sync(&mut self) -> Result<PumpOutcome, StorageError> {
        self.state
            .update_up_stats(|stats| stats.upstream_initial_sync += 1);

        let mut outcome = PumpOutcome::default();
        let mut checkpoint = match self.last_checkpoint.clone() {
            Some(checkpoint) => Some(checkpoint),
            None => {
                let read = last_checkpoint(&self.state, Direction::Up).await?;
                read.and_then(|protocol| protocol.as_storage_checkpoint())
            }
        };
        self.last_checkpoint = checkpoint.clone();

        let batch_size = self.state.push_batch_size();

        // Everything drained from the change stream before this point is now covered by the
        // fetch below, so mark the boundary before reading.
        self.timer += 1;
        self.initial_sync_start_time = self.timer;

        let mut passes = 0usize;

        loop {
            if self.state.is_cancelled() {
                return Ok(outcome);
            }
            // A pause stops the loop without marking first-sync done: the work resumes
            // where it stopped, and nothing is recorded as sent that was not sent.
            if self.state.is_paused() {
                return Ok(outcome);
            }

            let changed = self
                .state
                .fork()
                .get_changed_documents_since(batch_size, checkpoint.clone())
                .await?;
            if changed.documents.is_empty() {
                break;
            }
            checkpoint = Some(changed.checkpoint.clone());

            let result = self.push_batch(changed.documents).await?;
            outcome.pushed += result.outcome.pushed;
            outcome.conflicts += result.outcome.conflicts;
            outcome.conflict_writes += result.outcome.conflict_writes;
            if let Some(batch_end) = result.batch_end.clone() {
                // A batch that reported no end was abandoned by a pause or cancel; its
                // position must not be recorded.
                if !self.state.is_stopped_or_paused() {
                    set_checkpoint(&self.state, Direction::Up, Some(&batch_end)).await?;
                    self.state.checkpoint_queue().drain();
                    self.last_pushed_checkpoint = Some(batch_end);
                }
            }

            // Re-run the pass when a conflict was resolved, because the resolution is a new
            // fork revision the checkpoint has already passed. Bounded so a pathological
            // handler cannot spin: each additional pass is real work, and 8 is far beyond
            // the two a single conflict needs.
            if result.outcome.conflict_writes > 0 {
                passes += 1;
                if passes >= MAX_INITIAL_SYNC_PASSES {
                    return Err(StorageError::Unsupported(format!(
                        "the upstream initial sync still had conflict writes after \
                         {MAX_INITIAL_SYNC_PASSES} passes; a conflict handler that never \
                         converges would spin forever"
                    )));
                }
                // Re-read from the meta instance rather than reusing the in-memory one:
                // the conflict-resolution write advanced the fork's own revision, and the
                // persisted checkpoint is the position the next pass must trust.
                let stored = last_checkpoint(&self.state, Direction::Up).await?;
                checkpoint = stored.and_then(|protocol| protocol.as_storage_checkpoint());
                self.last_checkpoint = checkpoint.clone();
                continue;
            }
        }

        if !self.state.is_cancelled() && !self.state.is_paused() {
            self.state.events().set_first_sync_done(Direction::Up);
            outcome.first_sync_completed = true;
        }
        Ok(outcome)
    }

    /// Pushes one batch of documents and handles whatever the master reports back.
    ///
    /// Ports `persistToMaster` (`upstream.ts:330-566`). The steps, in order:
    ///
    /// 1. Read the assumed-master states for the batch in one call.
    /// 2. Skip documents the master already holds — but **not** one whose meta row records
    ///    `isResolvedConflict` equal to the fork's current revision: that is a resolution
    ///    that still has to reach the master.
    /// 3. Build the rows and the meta rows.
    /// 4. `master_write` in batches of at most `push_batch_size`.
    /// 5. **Re-check pause/cancel and bail before writing meta.** This is the load-bearing
    ///    fix; see the module docs.
    /// 6. Write meta for the non-conflicting rows, emit `processed.up`.
    /// 7. For each conflict: resolve, write the resolution to the fork, write its meta row
    ///    with `isResolvedConflict`, emit `resolvedConflict`.
    /// 8. Advance the checkpoint — even when nothing was pushed, so a pull-only run still
    ///    moves forward (`upstream.ts:402-417`).
    async fn push_batch(
        &mut self,
        documents: Vec<DocumentData>,
    ) -> Result<PushedBatch, StorageError> {
        let mut outcome = PumpOutcome::default();
        let primary_path = self.state.primary_path().to_string();
        self.state
            .update_up_stats(|stats| stats.persist_to_master += 1);

        let doc_ids: Vec<String> = documents
            .iter()
            .filter_map(|document| {
                document
                    .get(&primary_path)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        if doc_ids.is_empty() {
            return Ok(PushedBatch {
                outcome,
                batch_end: None,
            });
        }

        // Where this batch ends, in the storage's order. Reported to the caller rather than
        // stored here: two writers to one checkpoint field is how an initial sync ends up
        // resuming from a position the change-stream path already moved past. The caller
        // owns advancement.
        let batch_end = documents
            .iter()
            .filter_map(|document| {
                let lwt = document.lwt()?;
                let id = document.get(&primary_path).and_then(Value::as_str)?;
                Some((lwt, id.to_string()))
            })
            .max_by(|(a_lwt, a_id), (b_lwt, b_id)| {
                a_lwt
                    .partial_cmp(b_lwt)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| dxdb_jsstr::compare_js(a_id, b_id))
            })
            .and_then(|(lwt, id)| {
                let mut map = dxdb_json::Map::new();
                map.insert("id".to_string(), Value::String(id));
                map.insert("lwt".to_string(), Value::from(lwt));
                crate::checkpoint::Checkpoint::new(Value::Object(map)).ok()
            });

        let assumed = get_assumed_master_state(&self.state, &doc_ids).await?;
        let mut to_push: Vec<WriteToMasterRow> = Vec::new();
        let mut fork_states: BTreeMap<String, DocumentData> = BTreeMap::new();
        let mut meta_rows: BTreeMap<String, BulkWriteRow> = BTreeMap::new();

        for document in documents {
            let Ok(id) = document.id(&primary_path) else {
                continue;
            };
            let id = id.to_string();
            fork_states.insert(id.clone(), document.clone());

            let doc_state = write_doc_to_doc_state(
                &document,
                self.state.has_attachments(),
                self.state.keep_meta(),
            );
            let assumed_row = assumed.get(&id);

            // Skip when the master already holds this state. The exemption matters: a meta
            // row whose `isResolvedConflict` is the fork's current revision says "this fork
            // state is a conflict resolution that has not been pushed yet", so it must go
            // even though the assumed master looks equal (`upstream.ts:344-348`).
            if let Some(assumed_row) = assumed_row {
                let is_pending_resolution = assumed_row.is_resolved_conflict() == document.rev();
                if !is_pending_resolution
                    && self.state.conflict_handler().is_equal(
                        &assumed_row.doc_data,
                        &doc_state,
                        "upstream-check-if-equal",
                    )
                {
                    continue;
                }
            }

            to_push.push(WriteToMasterRow::new(
                assumed_row.map(|row| row.doc_data.clone()),
                doc_state,
            ));
            meta_rows.insert(
                id.clone(),
                get_meta_write_row(
                    &self.state,
                    &write_doc_to_doc_state(
                        &document,
                        self.state.has_attachments(),
                        self.state.keep_meta(),
                    ),
                    assumed_row.map(|row| &row.meta_document),
                    None,
                )?,
            );
        }

        if to_push.is_empty() {
            // Nothing to send, but the batch was consumed, so the position advances.
            return Ok(PushedBatch { outcome, batch_end });
        }

        // `master_write` must never see more than `push_batch_size` rows
        // (`upstream.ts:449-455`).
        let batch_size = self.state.push_batch_size().max(1);
        let mut conflict_ids: HashSet<String> = HashSet::new();
        let mut conflict_states: BTreeMap<String, DocumentData> = BTreeMap::new();

        for chunk in to_push.chunks(batch_size) {
            let mut chunk_rows: Vec<WriteToMasterRow> = chunk.to_vec();

            // Attach the payloads the master does not have before sending. `assumed_master_state`
            // is the original: whatever the master is known to hold, so only new or changed
            // attachments carry bytes. A `None` assumed state means the master holds nothing,
            // so everything goes -- which is why a first push of an attachment-bearing document
            // is the expensive one.
            if self.state.has_attachments() {
                for row in chunk_rows.iter_mut() {
                    let filled = fill_write_data_for_attachments_change(
                        self.state.fork(),
                        self.state.primary_path(),
                        &row.new_document_state,
                        row.assumed_master_state.as_ref(),
                    )
                    .await?;
                    // The payloads ride beside the document, never inside its JSON.
                    row.attachment_payloads = filled.attachment_payloads().clone();
                }
            }

            self.last_push_batch = chunk_rows.clone();
            let reported = self.state.handler().master_write(chunk_rows).await?;
            for real_master_state in reported {
                if let Ok(id) = real_master_state.id(&primary_path) {
                    conflict_ids.insert(id.to_string());
                    conflict_states.insert(id.to_string(), real_master_state);
                }
            }
        }

        // --- the load-bearing re-check -------------------------------------------------
        //
        // "Check for canceled or paused state before marking documents as successfully
        // pushed. When the replication is paused during a push retry, masterWrite() returns
        // [] even though no documents were actually sent. Without this check the meta
        // instance would be updated, causing the documents to never be retried on resume."
        //   -- upstream.ts:435-441
        if self.state.is_stopped_or_paused() {
            // Keep the batch end: these documents were read and will not be read again,
            // so the checkpoint must still advance past them.
            return Ok(PushedBatch { outcome, batch_end });
        }

        // Meta for the rows the master accepted. A conflict's row is written later, with
        // its resolution marker, so it is excluded here.
        let accepted_meta: Vec<BulkWriteRow> = to_push
            .iter()
            .filter_map(|row| {
                row.new_document_state
                    .id(&primary_path)
                    .ok()
                    .filter(|id| !conflict_ids.contains(*id))
                    .and_then(|id| meta_rows.get(id).cloned())
            })
            .collect();
        let accepted_count = accepted_meta.len();
        if !accepted_meta.is_empty() && !self.state.skip_storing_pull_meta() {
            self.state
                .meta_instance()
                .bulk_write(
                    strip_attachments_data_from_meta_write_rows(
                        self.state.has_attachments(),
                        accepted_meta,
                    ),
                    "replication-up-write-meta".to_string(),
                )
                .await?;
        }
        for row in &to_push {
            if let Ok(id) = row.new_document_state.id(&primary_path) {
                if !conflict_ids.contains(id) {
                    self.state.events().record_processed_up(row.clone());
                }
            }
        }
        outcome.pushed = accepted_count;

        // --- conflicts ----------------------------------------------------------------
        if !conflict_ids.is_empty() {
            self.state
                .update_up_stats(|stats| stats.persist_to_master_had_conflicts += 1);
            outcome.conflicts = conflict_ids.len();

            let mut conflict_fork_writes: Vec<BulkWriteRow> = Vec::new();
            let mut conflict_meta_writes: Vec<(String, BulkWriteRow)> = Vec::new();

            for (id, real_master_state) in &conflict_states {
                let Some(fork_state) = fork_states.get(id) else {
                    continue;
                };
                let Some(row) = to_push.iter().find(|row| {
                    row.new_document_state
                        .id(&primary_path)
                        .map(|row_id| row_id == id)
                        .unwrap_or(false)
                }) else {
                    continue;
                };
                let input = ConflictHandlerInput {
                    assumed_master_state: row.assumed_master_state.clone(),
                    real_master_state: real_master_state.clone(),
                    new_document_state: row.new_document_state.clone(),
                };
                let resolved = resolve_conflict_error(
                    self.state.conflict_handler(),
                    &input,
                    fork_state,
                    self.state.checkpoint_key(),
                    self.state.lwt(),
                )
                .map_err(StorageError::InvalidDocument)?;

                self.state
                    .events()
                    .record_resolved_conflict(crate::state::ResolvedConflict {
                        input: input.clone(),
                        output: resolved.clone(),
                    });

                conflict_fork_writes.push(dxdb_storage::update_row(
                    fork_state.clone(),
                    resolved.clone(),
                ));
                conflict_meta_writes.push((
                    id.clone(),
                    get_meta_write_row(
                        &self.state,
                        real_master_state,
                        assumed.get(id).map(|row| &row.meta_document),
                        resolved.rev(),
                    )?,
                ));
            }

            if !conflict_fork_writes.is_empty() {
                self.state
                    .update_up_stats(|stats| stats.persist_to_master_conflict_writes += 1);
                let response = self
                    .state
                    .fork()
                    .bulk_write(
                        conflict_fork_writes.clone(),
                        "replication-up-write-conflict".to_string(),
                    )
                    .await?;

                let mut fatal: Option<StorageError> = None;
                for error in &response.error {
                    // A 409 here means something wrote the fork in between, which will
                    // trigger another upstream pass that resolves the conflict again.
                    // Non-409 errors are real and must surface.
                    if is_conflict_error(error) {
                        continue;
                    }
                    if let WriteError::Validation { document_id, .. } = error {
                        fatal = Some(StorageError::InvalidDocument(format!(
                            "writing the resolution for {document_id} failed validation"
                        )));
                    }
                }
                if let Some(error) = fatal {
                    self.state.events().record_error(error.clone());
                    return Err(error);
                }

                // The resolution wrote, so the resolved revision is what the master now
                // needs. `getMetaWriteRow(..., isResolvedConflict = resolved._rev)`.
                let written: HashSet<String> = response
                    .written_documents(&primary_path, &conflict_fork_writes)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                let use_meta: Vec<BulkWriteRow> = conflict_meta_writes
                    .into_iter()
                    .filter(|(id, _)| written.contains(id))
                    .map(|(_, row)| row)
                    .collect();
                outcome.conflict_writes = use_meta.len();
                if !use_meta.is_empty() && !self.state.is_cancelled() {
                    self.state
                        .meta_instance()
                        .bulk_write(
                            strip_attachments_data_from_meta_write_rows(
                                self.state.has_attachments(),
                                use_meta,
                            ),
                            "replication-up-write-conflict-meta".to_string(),
                        )
                        .await?;
                }
            }
        }

        self.last_conflicts = conflict_states.into_values().collect();

        Ok(PushedBatch { outcome, batch_end })
    }

    /// Marks the current position as "already synced", so the next
    /// [`Self::pump_live_only`] treats everything in the fork as backlog it may skip.
    ///
    /// Upstream's initial sync and its change-stream processing are separate modes joined by
    /// the `initialSyncStartTime` rule. This exposes that boundary so a test can exercise the
    /// live path on its own, which is the only path where the own-write filter can apply.
    pub fn begin_live_session(&mut self) {
        self.timer += 1;
        self.initial_sync_start_time = self.timer;
    }

    /// Pumps the change stream only, without running an initial sync.
    ///
    /// The live half of upstream's push: `processTasks()` with no checkpoint iteration
    /// (`upstream.ts:220-268`). Kept separate from [`Self::pump`] because the two have
    /// different rules — the initial sync cannot filter by context and drops nothing, the live
    /// path filters own-writes and drops pre-fetch tasks — and a test that wants to observe
    /// one of those rules must not have the other run over the same documents first.
    pub async fn pump_live_only(&mut self) -> Result<PumpOutcome, StorageError> {
        if self.state.is_stopped_or_paused() {
            return Ok(PumpOutcome::default());
        }
        let mut outcome = PumpOutcome::default();
        let (pending, _resync) = self.collect_changes();
        if pending.is_empty() {
            return Ok(outcome);
        }
        self.state.update_up_stats(|stats| stats.process_tasks += 1);
        let docs: Vec<DocumentData> = pending.into_values().collect();
        let result = self.push_batch(docs).await?;
        outcome.pushed += result.outcome.pushed;
        outcome.conflicts += result.outcome.conflicts;
        outcome.conflict_writes += result.outcome.conflict_writes;
        if let Some(batch_end) = result.batch_end {
            if !self.state.is_stopped_or_paused() {
                set_checkpoint(&self.state, Direction::Up, Some(&batch_end)).await?;
                self.state.checkpoint_queue().drain();
                self.last_pushed_checkpoint = Some(batch_end);
            }
        }
        Ok(outcome)
    }

    /// Feeds a `RESYNC` from the master into the push direction.
    ///
    /// Upstream subscribes to `masterChangeStream$` and re-runs the initial sync on
    /// `'RESYNC'` (`upstream.ts:96-104`): the fork lost no events, but the *master* may
    /// have, so the checkpoint iteration has to run again.
    pub async fn handle_pull_stream_item(
        &mut self,
        item: &PullStreamItem,
    ) -> Result<PumpOutcome, StorageError> {
        match item {
            PullStreamItem::Resync => {
                // Clearing the cached checkpoint forces the next pass to re-read it from
                // the meta instance, which is the state a resync is meant to trust.
                self.last_checkpoint = None;
                self.initial_sync().await
            }
            PullStreamItem::Changes(_) => Ok(PumpOutcome::default()),
        }
    }
}

/// How many times the initial sync may re-run because of conflict writes before it is
/// declared non-convergent.
pub const MAX_INITIAL_SYNC_PASSES: usize = 8;

/// Builds the fork's document states for a set of ids, for a caller that needs to inspect
/// what a push would have sent.
pub async fn fork_states<S: RxStorageInstance>(
    state: &ReplicationState<S>,
    ids: Vec<String>,
) -> Result<Vec<DocumentData>, StorageError> {
    state.fork().find_documents_by_id(ids, true).await
}

/// True when an event would be pushed: not from the downstream, not a duplicate.
///
/// Exposed so the own-write filter can be asserted directly rather than inferred from a
/// push count. The operation does not matter — a delete carries the tombstone the master
/// needs — which is why this does not look at [`Operation`].
pub fn is_pushable(context: &str, downstream_flag: &str) -> bool {
    context != downstream_flag
}

/// A write row for a document with no attachment payloads, for a caller assembling a
/// prefix.
pub fn plain_write_row(document: DocumentData) -> BulkWriteRow {
    BulkWriteRow {
        previous: None,
        document: DocumentWriteData::new(document),
    }
}
