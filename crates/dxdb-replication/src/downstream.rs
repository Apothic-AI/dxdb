//! The downstream: pulling the master's changes onto the fork.
//!
//! Ports `src/replication-protocol/downstream.ts` (550 lines).
//!
//! # Two operation modes
//!
//! Upstream runs the pull in two modes and needs both (`downstream.ts:34-38`):
//!
//! * **checkpoint iteration** (`downstreamResyncOnce`) for a first sync or a reconnect,
//!   where the master may have changed while the client was away;
//! * **change-stream batching** (`downstreamProcessChanges`) for a live connection, where
//!   several small batches are merged into one write to avoid a storage round-trip per
//!   document.
//!
//! [`Downstream::pump`] drains the stream and merges what is there; [`Downstream::resync`]
//! runs the checkpoint loop. That is the same split, expressed for a caller that drives the
//! protocol rather than an event loop (D2/D3).
//!
//! # The three-way state comparison, which is the heart of the pull
//!
//! For each incoming document the pull compares three states and reaches one of four
//! decisions. Getting the order wrong is how a pull loses local writes:
//!
//! | fork | assumed master | master | decision |
//! | --- | --- | --- | --- |
//! | absent | — | anything | write |
//! | present | equal to fork | — | skip (already replicated) |
//! | present | *differs* from fork | — | **skip and let the upstream go first** |
//! | present | — | equal to fork | meta-only patch |
//! | present | — | differs | write |
//!
//! Row 3 is the one that matters: the fork holds a local write the master has not seen, so
//! overwriting it would destroy that write. The pull must yield to the push. Upstream spells
//! this out at `downstream.ts:341-370` and links the bug it fixed
//! (<https://github.com/pubkey/rxdb/pull/7804>).
//!
//! # `_meta.o`, and why two different hashes exist
//!
//! A replicated write is tagged in the fork document's `_meta.o` with
//! `{_rev: <next height>, hash: <identifier hash>}`. That tag is how a later pull recognises
//! a write *it* made, and therefore knows not to wait for the push.
//!
//! Note the hash here is the **identifier hash**, not the checkpoint key
//! (`downstream.ts:67` vs `checkpoint.ts:145`): the checkpoint key is
//! `hash(identifier || db || collection)`, while this is `hash(identifier)`. They are
//! different values and swapping them silently changes which writes look self-authored. The
//! checkpoint key is still used for the fork revision token, which is why both appear in this
//! file.

use std::collections::BTreeMap;

use dxdb_json::Value;
use dxdb_storage::{
    BulkWriteRow, DocumentData, DocumentWriteData, RxStorageInstance, StorageError,
};

use crate::checkpoint::{last_checkpoint, set_checkpoint, Direction};
use crate::convert::{
    json_number, strip_attachments_data_from_meta_write_rows, was_written_by_replication,
    write_doc_to_doc_state, WriteTag,
};
use crate::meta_instance::{get_assumed_master_state, get_meta_write_row};
use crate::state::{DocumentsWithCheckpoint, ReplicationState};

/// What one pull did, so a caller can decide whether to keep pumping and a test can assert on
/// work rather than on a sleep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PullOutcome {
    /// Documents written to the fork.
    pub written: usize,
    /// Documents skipped because the master already agreed with the fork.
    pub unchanged: usize,
    /// Documents skipped because the fork holds an unpushed local write.
    pub deferred_to_upstream: usize,
    /// Meta-only patches (the fork state was already right, the bookkeeping was not).
    pub meta_only: usize,
    /// True when a first sync completed during this pull.
    pub first_sync_completed: bool,
}

impl PullOutcome {
    /// True when the pull made observable progress.
    pub fn made_progress(&self) -> bool {
        self.written > 0 || self.meta_only > 0
    }
}

/// The downstream: everything the pull direction owns.
pub struct Downstream<S: RxStorageInstance> {
    state: ReplicationState<S>,
    /// `hash(identifier)`, which tags replicated fork writes in `_meta.o`.
    ///
    /// Distinct from the state's checkpoint key; see the module docs.
    identifier_hash: String,
    /// The protocol checkpoint the next iteration resumes from.
    ///
    /// The transport speaks protocol checkpoints; the fork's own change feed speaks storage
    /// checkpoints. Keeping the protocol one here avoids a conversion on every iteration and
    /// makes it obvious which type a given line deals in.
    last_checkpoint: Option<crate::checkpoint::Checkpoint>,
}

impl<S: RxStorageInstance> Downstream<S> {
    /// Builds the downstream.
    pub fn new(state: ReplicationState<S>, identifier_hash: impl Into<String>) -> Self {
        Downstream {
            state,
            identifier_hash: identifier_hash.into(),
            last_checkpoint: None,
        }
    }

    /// The replication state.
    pub fn state(&self) -> &ReplicationState<S> {
        &self.state
    }

    /// The identifier hash written into `_meta.o`.
    pub fn identifier_hash(&self) -> &str {
        &self.identifier_hash
    }

    /// Seeds the checkpoint if the caller supplied an initial one and none is stored.
    ///
    /// Ports `downstream.ts:57-65`. An existing stored checkpoint always wins, so a caller
    /// cannot rewind a replication that has already run.
    pub async fn seed_initial_checkpoint(&mut self) -> Result<(), StorageError> {
        let Some(initial) = self.state.initial_checkpoint_for(Direction::Down).cloned() else {
            return Ok(());
        };
        let existing = last_checkpoint(&self.state, Direction::Down).await?;
        if existing.is_none() {
            set_checkpoint(&self.state, Direction::Down, Some(&initial)).await?;
        }
        self.state.checkpoint_queue().drain();
        Ok(())
    }

    /// One unit of pull work from the change stream: merge whatever `RESYNC` asked for, then
    /// apply everything the master sent.
    ///
    /// The `RESYNC` short-circuit is upstream's: a resync may have missed events, so it
    /// discards the queued batches and re-reads from the checkpoint rather than trusting them
    /// (`downstream.ts:93-110`).
    pub async fn pump(
        &mut self,
        items: Vec<crate::state::PullStreamItem>,
    ) -> Result<PullOutcome, StorageError> {
        if self.state.is_stopped_or_paused() {
            return Ok(PullOutcome::default());
        }
        let mut outcome = PullOutcome::default();

        // A resync winnows everything: anything queued behind it will be re-derived.
        if items
            .iter()
            .any(|item| matches!(item, crate::state::PullStreamItem::Resync))
        {
            return self.resync().await;
        }

        // Merge every batch into one write. Upstream does the same by concatenating task
        // documents before persisting (`downstream.ts:203-212`); the reason is that a
        // storage round-trip per document is the dominant cost when the master is fast.
        let mut documents: Vec<DocumentData> = Vec::new();
        let mut checkpoint: Option<crate::checkpoint::Checkpoint> = None;
        for item in items {
            if let crate::state::PullStreamItem::Changes(batch) = item {
                self.state
                    .update_down_stats(|stats| stats.master_change_stream_emit += 1);
                documents.extend(batch.documents);
                checkpoint = match (checkpoint, batch.checkpoint) {
                    (Some(existing), Some(next)) => Some(existing.stack(&next)),
                    (None, next) => next,
                    (existing, None) => existing,
                };
            }
        }
        if documents.is_empty() && checkpoint.is_none() {
            return Ok(outcome);
        }
        self.state
            .update_down_stats(|stats| stats.downstream_process_changes += 1);
        outcome = self.persist_from_master(documents, checkpoint).await?;
        self.checkpoint_queue_drain();
        Ok(outcome)
    }

    /// Iterates the master by checkpoint until it has nothing more.
    ///
    /// Ports `downstreamResyncOnce` (`downstream.ts:180-222`). Two exit conditions, both
    /// upstream's:
    ///
    /// * an **empty** batch: the master has nothing after the checkpoint;
    /// * a **short** batch: fewer than `pull_batch_size` documents means the master has no
    ///   more (`downstream.ts:212-218`). A *full* batch always continues, even if it was the
    ///   last one, so the loop cannot stop one batch early.
    pub async fn resync(&mut self) -> Result<PullOutcome, StorageError> {
        self.state
            .update_down_stats(|stats| stats.downstream_resync_once += 1);
        if self.state.is_cancelled() {
            return Ok(PullOutcome::default());
        }

        let mut outcome = PullOutcome::default();
        let mut checkpoint = match self.last_checkpoint.clone() {
            Some(checkpoint) => Some(checkpoint),
            None => last_checkpoint(&self.state, Direction::Down).await?,
        };
        self.last_checkpoint = checkpoint.clone();
        let batch_size = self.state.pull_batch_size();

        loop {
            if self.state.is_cancelled() || self.state.is_paused() {
                return Ok(outcome);
            }

            let pulled = self
                .state
                .handler()
                .master_changes_since(checkpoint.as_ref(), batch_size)
                .await?;

            if pulled.documents.is_empty() {
                break;
            }
            let count = pulled.documents.len();
            let next_protocol_checkpoint = pulled.checkpoint.clone();
            checkpoint = next_protocol_checkpoint.clone();

            let batch = self
                .persist_from_master(pulled.documents, next_protocol_checkpoint)
                .await?;
            outcome.written += batch.written;
            outcome.unchanged += batch.unchanged;
            outcome.deferred_to_upstream += batch.deferred_to_upstream;
            outcome.meta_only += batch.meta_only;

            if count < batch_size {
                break;
            }
        }

        self.checkpoint_queue_drain();
        if !self.state.is_cancelled() && !self.state.is_paused() {
            self.state.events().set_first_sync_done(Direction::Down);
            outcome.first_sync_completed = true;
        }
        Ok(outcome)
    }

    /// Applies a batch of master states to the fork and the meta instance.
    ///
    /// Ports `persistFromMaster`'s drain (`downstream.ts:246-545`). The decisions and their
    /// order are documented in the module header; this function is where each is implemented.
    async fn persist_from_master(
        &mut self,
        documents: Vec<DocumentData>,
        checkpoint: Option<crate::checkpoint::Checkpoint>,
    ) -> Result<PullOutcome, StorageError> {
        let mut outcome = PullOutcome::default();
        self.state
            .update_down_stats(|stats| stats.persist_from_master += 1);

        if self.state.is_cancelled() || documents.is_empty() {
            return Ok(outcome);
        }

        let primary_path = self.state.primary_path().to_string();
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
            return Ok(outcome);
        }

        // One read for the fork states, one for the assumed-master states.
        let found = self
            .state
            .fork()
            .find_documents_by_id(doc_ids.clone(), true)
            .await?;
        let mut fork_states: BTreeMap<String, DocumentData> = BTreeMap::new();
        for document in found {
            if let Some(id) = document
                .get(&primary_path)
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                fork_states.insert(id, document);
            }
        }
        let assumed = get_assumed_master_state(&self.state, &doc_ids).await?;

        let mut fork_writes: Vec<BulkWriteRow> = Vec::new();
        let mut meta_writes: Vec<BulkWriteRow> = Vec::new();
        let mut master_by_id: BTreeMap<String, DocumentData> = BTreeMap::new();

        for master_state in &documents {
            let Ok(id) = master_state.id(&primary_path) else {
                continue;
            };
            let id = id.to_string();
            master_by_id.insert(id.clone(), master_state.clone());

            let fork_document = fork_states.get(&id);
            let fork_state = fork_document.map(|document| {
                write_doc_to_doc_state(document, self.state.has_attachments(), false)
            });
            let assumed_row = assumed.get(&id);

            // -- 1. is the fork already at the assumed master state? -------------------
            let mut assumed_equals_fork = match (assumed_row, &fork_state) {
                (Some(assumed_row), Some(fork_state)) => self.state.conflict_handler().is_equal(
                    &assumed_row.doc_data,
                    fork_state,
                    "downstream-check-if-equal-0",
                ),
                _ => false,
            };
            // The `_rev` fallback: a master that carries `_rev` (CouchDB) can be compared by
            // height instead of content, and a fork document tagged with the height the pull
            // assigned proves the two agree (`downstream.ts:333-343`).
            if !assumed_equals_fork {
                if let (Some(assumed_row), Some(fork_document)) = (assumed_row, fork_document) {
                    let master_has_rev = assumed_row.doc_data.rev().is_some();
                    let tagged_height = fork_document
                        .meta()
                        .and_then(|meta| meta.get(self.state.identifier_value()))
                        .and_then(Value::as_u64);
                    if master_has_rev {
                        if let Some(tagged_height) = tagged_height {
                            if dxdb_storage::try_get_height_of_revision(
                                fork_document.rev().unwrap_or_default(),
                            ) == u32::try_from(tagged_height).ok()
                            {
                                assumed_equals_fork = true;
                            }
                        }
                    }
                }
            }

            // -- 2. does the fork hold an unpushed local write? ------------------------
            //
            // The pull must not overwrite it: the upstream has to send it first, or the local
            // write is lost. `<https://github.com/pubkey/rxdb/pull/7804>`
            let is_local_unpushed = match (fork_document, assumed_row) {
                (Some(_), Some(_)) => !assumed_equals_fork,
                (Some(_), None) => true,
                (None, _) => false,
            };
            let written_by_us = match fork_document {
                Some(fork_document) => {
                    let height = dxdb_storage::try_get_height_of_revision(
                        fork_document.rev().unwrap_or_default(),
                    )
                    .unwrap_or(0);
                    was_written_by_replication(fork_document, &self.identifier_hash, height)
                }
                None => false,
            };
            if is_local_unpushed && !self.state.skip_storing_pull_meta() && !written_by_us {
                outcome.deferred_to_upstream += 1;
                continue;
            }

            // -- 3. are the master and the fork already identical? ---------------------
            let states_exactly_equal = match &fork_state {
                Some(fork_state) => self.state.conflict_handler().is_equal(
                    master_state,
                    fork_state,
                    "downstream-check-if-equal-1",
                ),
                None => false,
            };
            if fork_state.is_some() && states_exactly_equal {
                // The states agree; only the bookkeeping may be missing, which happens when
                // the process died between the fork write and the meta write.
                if assumed_row.is_none() || !assumed_equals_fork {
                    if let Some(fork_state) = &fork_state {
                        meta_writes.push(get_meta_write_row(
                            &self.state,
                            fork_state,
                            assumed_row.map(|row| &row.meta_document),
                            None,
                        )?);
                        outcome.meta_only += 1;
                    }
                } else {
                    outcome.unchanged += 1;
                }
                continue;
            }

            // -- 4. write the master state to the fork --------------------------------
            let new_fork_state = build_fork_state(
                &self.state,
                master_state,
                fork_document,
                &self.identifier_hash,
            )?;
            // A master state that carries attachment payloads -- which it does whenever the
            // collection has attachments, because `masterChangesSince` sends them all
            // (`index.ts:241-247`) -- has them inside `_attachments[id].data` on the wire. The
            // storage layer takes payloads beside the document instead
            // (`DocumentWriteData::attachment_payloads`), so lift them out here.
            //
            // This is the receiving half of `fillWriteDataForAttachmentsChange`: the sender
            // puts the bytes on the wire, and the receiver must hand them to its own storage or
            // the write is a stub with no data -- which a correct storage rejects as `510`.
            let mut write_data = DocumentWriteData::new(new_fork_state);
            write_data = lift_attachment_payloads(write_data)?;
            fork_writes.push(dxdb_storage::BulkWriteRow {
                previous: fork_document.cloned(),
                document: write_data,
            });
            meta_writes.push(get_meta_write_row(
                &self.state,
                master_state,
                assumed_row.map(|row| &row.meta_document),
                None,
            )?);
        }

        if !fork_writes.is_empty() {
            // The downstream's own context, so the upstream recognises these as its writes
            // and does not push the master back to itself.
            let context = self.state.downstream_bulk_write_flag().to_string();
            let response = self
                .state
                .fork()
                .bulk_write(fork_writes.clone(), context)
                .await?;

            for (id, document) in response.written_documents(&primary_path, &fork_writes) {
                let _ = id;
                self.state.events().record_processed_down(document.clone());
                outcome.written += 1;
            }

            // A downstream 409 is not an error: the local write won, and it will go upstream
            // and come back (`downstream.ts:495-500`). Anything else is fatal.
            for error in &response.error {
                if error.is_conflict() {
                    continue;
                }
                let fatal = StorageError::InvalidDocument(format!(
                    "writing a pulled document failed with status {} for {}",
                    error.status(),
                    error.document_id()
                ));
                self.state.events().record_error(fatal.clone());
                return Err(fatal);
            }
        }

        if !meta_writes.is_empty() && !self.state.skip_storing_pull_meta() {
            let response = self
                .state
                .meta_instance()
                .bulk_write(
                    strip_attachments_data_from_meta_write_rows(
                        self.state.has_attachments(),
                        meta_writes,
                    ),
                    "replication-down-write-meta".to_string(),
                )
                .await?;
            for error in &response.error {
                self.state
                    .events()
                    .record_error(StorageError::InvalidDocument(format!(
                        "writing pull meta failed with status {} for {}",
                        error.status(),
                        error.document_id()
                    )));
            }
        }

        // The checkpoint is written last, after the documents it describes are durable.
        // `set_checkpoint` refuses to write it while cancelled, which is the second half of
        // the same protection the cancel path provides.
        set_checkpoint(&self.state, Direction::Down, checkpoint.as_ref()).await?;
        Ok(outcome)
    }

    fn checkpoint_queue_drain(&self) {
        self.state.checkpoint_queue().drain();
    }
}

/// Moves `_attachments[id].data` payloads out of the document and into the write's payload map.
///
/// On the wire a payload travels inside the stub as a byte array, because a JS `Blob` is
/// embedded that way and every transport encodes it however it likes. The storage layer keeps
/// payloads separately, so the document's `_attachments` must hold stubs only by the time
/// `bulk_write` sees it.
///
/// The `data` key is removed as the bytes are lifted, so a stored document never carries a
/// payload in its JSON — the same invariant `AttachmentWriteData` maintains on the write side.
fn lift_attachment_payloads(
    mut write_data: DocumentWriteData,
) -> Result<DocumentWriteData, StorageError> {
    let document = write_data.document().clone();
    let Some(attachments) = document.attachments() else {
        return Ok(write_data);
    };

    let mut stripped = attachments.clone();
    let mut lifted: Vec<(String, Vec<u8>)> = Vec::new();
    for (attachment_id, stub) in attachments {
        let Some(map) = stub.as_object() else {
            continue;
        };
        let Some(Value::Array(bytes)) = map.get("data") else {
            continue;
        };
        let payload: Vec<u8> = bytes
            .iter()
            .filter_map(Value::as_u64)
            .map(|byte| byte as u8)
            .collect();
        let mut stub_without_data = map.clone();
        stub_without_data.remove("data");
        stripped.insert(attachment_id.clone(), Value::Object(stub_without_data));
        lifted.push((attachment_id.clone(), payload));
    }

    if lifted.is_empty() {
        return Ok(write_data);
    }

    write_data
        .document_mut()
        .set("_attachments", Value::Object(stripped));
    for (attachment_id, payload) in lifted {
        let digest = document
            .attachment(&attachment_id)
            .and_then(|stub| stub.get("digest"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let content_type = document
            .attachment(&attachment_id)
            .and_then(|stub| stub.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string();
        let length = u64::try_from(payload.len()).unwrap_or(0);
        write_data.insert_attachment(
            &attachment_id,
            dxdb_storage::AttachmentWriteData {
                stub: dxdb_storage::AttachmentData {
                    length,
                    content_type,
                    digest,
                },
                data: Some(payload),
            },
        );
    }
    Ok(write_data)
}

/// Builds the fork's copy of a master document.
///
/// Ports the `Object.assign` block at `downstream.ts:419-465`, in the order upstream applies
/// it, because each step can overwrite the previous one:
///
/// 1. the master document's fields;
/// 2. `_meta` from the fork if it exists (else `{lwt: now()}`), `_attachments` from the
///    master only when the collection has attachments, `_rev` empty;
/// 3. if the master carries `_rev`: `_meta[identifier] = next height`, and `_rev = master._rev`
///    when `keepMeta`;
/// 4. if `keepMeta` and the master has `_meta`: `_meta = master._meta`;
/// 5. `_meta.o = {_rev: next height, hash: identifier hash}`;
/// 6. `_rev = createRevision(checkpointKey, previous)` when still empty.
fn build_fork_state<S: RxStorageInstance>(
    state: &ReplicationState<S>,
    master_state: &DocumentData,
    fork_document: Option<&DocumentData>,
    identifier_hash: &str,
) -> Result<DocumentData, StorageError> {
    let mut new_state = master_state.clone();

    // A pull never sends the master's `_rev`/`_meta` onward unless keepMeta; clear them first
    // so the branches below decide rather than inheriting.
    new_state.as_object_mut().remove("_rev");
    new_state.as_object_mut().remove("_meta");

    let next_height = match fork_document {
        Some(fork_document) => {
            dxdb_storage::try_get_height_of_revision(fork_document.rev().unwrap_or_default())
                .unwrap_or(0)
                + 1
        }
        None => 1,
    };

    let mut meta = match fork_document {
        Some(fork_document) => fork_document.meta().cloned().unwrap_or_default(),
        None => {
            let mut meta = dxdb_json::Map::new();
            if let Some(lwt) = json_number(state.lwt()) {
                meta.insert("lwt".to_string(), lwt);
            }
            meta
        }
    };

    let attachments = if state.has_attachments() {
        master_state
            .get("_attachments")
            .cloned()
            .unwrap_or_else(|| Value::Object(dxdb_json::Map::new()))
    } else {
        Value::Object(dxdb_json::Map::new())
    };
    new_state.set("_attachments", attachments);

    if master_state.rev().is_some() {
        meta.insert(
            state.identifier_value().to_string(),
            Value::from(next_height),
        );
    }
    if state.keep_meta() {
        if let Some(master_rev) = master_state.rev() {
            new_state.set("_rev", Value::String(master_rev.to_string()));
        }
        if let Some(master_meta) = master_state.meta() {
            meta = master_meta.clone();
        }
    }

    meta.insert(
        "o".to_string(),
        WriteTag::new(next_height, identifier_hash).to_value(),
    );
    new_state.set("_meta", Value::Object(meta));

    if new_state.rev().is_none() || new_state.rev() == Some("") {
        new_state.set(
            "_rev",
            Value::String(dxdb_storage::create_revision(
                state.checkpoint_key(),
                fork_document.and_then(DocumentData::rev),
            )),
        );
    }
    Ok(new_state)
}

/// Builds a `DocumentsWithCheckpoint` for a test or a transport that has both halves.
pub fn batch(
    documents: Vec<DocumentData>,
    checkpoint: Option<crate::checkpoint::Checkpoint>,
) -> DocumentsWithCheckpoint {
    DocumentsWithCheckpoint {
        documents,
        checkpoint,
    }
}
