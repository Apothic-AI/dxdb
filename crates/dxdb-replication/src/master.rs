//! The master side of the protocol: a [`ReplicationHandler`] built over a local storage instance.
//!
//! Ports `rxStorageInstanceToReplicationHandler` (`replication-protocol/index.ts:184-330`).
//!
//! Every adapter in [`crate::adapters`] is a **client** of some remote provider. This module is
//! the other half: it turns *any* `RxStorageInstance` into the handler a server answers with, so
//! a provider adapter on one side and a local storage on the other converge through the same
//! three methods. Upstream uses it for exactly that in four places — the WebSocket server
//! (`replication-websocket/websocket-server.ts:77`), WebRTC peers
//! (`replication-webrtc/index.ts:294`), the migration state (`migration-schema/rx-migration-state.ts:552`),
//! and the test helpers (`test-utils/replication.ts:20`).
//!
//! # Why this is protocol, not a test fixture
//!
//! The ported conformance suite's cases each supply a master, and before this module every one
//! of them hand-rolled the master's write rules against a bespoke JSON store
//! (`tests/common/double.rs`). That meant the rules the *master* applies —
//! "no master state ⇒ insert, master state without an assumed state ⇒ conflict, assumed state
//! differs ⇒ conflict, otherwise write" — were re-implemented per test and were not the code any
//! product would run. A storage-backed handler removes that whole class of drift: a test that
//! wants a master now names a storage, and the rules under test are these.
//!
//! It also settles the question of *what the master does with attachments*, which is not the
//! same as what a client does:
//!
//! > "Notice that the master never knows the client state of the document. Therefore we always
//! > send all attachments data." — `index.ts:212-217`, `:242-247`
//!
//! So a pull from this handler fills every attachment payload unconditionally, via
//! [`crate::attachments::fill_write_data_for_attachments_change`] with a `None` assumed state.
//! There is no negotiation and no "you already have this". The live change stream applies the
//! same rule through one shared helper ([`client_view`]), because upstream applies the same two
//! steps in both places and a divergence would mean a client that converges on a poll but not on
//! a push.
//!
//! # What is not here
//!
//! * **Authorization.** A handler answers; deciding whether the caller may ask is above it.
//! * **Batching policy.** The handler serves the `batch_size` it is given. Whether a transport
//!   asks for 20 or 200 is the transport's business.
//! * **`keep_meta` for the migration path.** The parameter exists upstream because the schema
//!   migration needs checkpoints to stay valid across a rewrite
//!   (`index.ts:188-193`). It is honoured here, but nothing in this crate currently sets it
//!   besides tests; the migration plugin is M6.
//! * **Failure reporting on the change stream.** `PullStreamItem` has no `Err` variant, so a
//!   storage error while mapping a bulk ends the stream rather than surfacing as an item. A
//!   transport that wants to report it subscribes to `instance.change_stream()` itself, or
//!   observes the close. That is a deliberate shape, not an oversight: upstream's `mergeMap`
//!   propagates the rejection to `error$`, which is the protocol's error channel, and the
//!   protocol layer here does the same from `master_changes_since`.

use std::rc::Rc;

use dxdb_storage::{
    BulkWriteRow, Checkpoint, Clock, DocumentData, DocumentWriteData, MonotonicClock,
    RxStorageInstance, StorageError, WriteError,
};

use crate::attachments::fill_write_data_for_attachments_change;
use crate::checkpoint::Checkpoint as ProtocolCheckpoint;
use crate::conflict::ConflictHandler;
use crate::convert::{
    doc_state_to_write_doc, document_with_attachment_payloads, write_doc_to_doc_state,
};
use crate::state::{
    BoxFuture, DocumentsWithCheckpoint, PullStream, PullStreamItem, ReplicationHandler,
    WriteToMasterRow,
};

/// The `bulk_write` context string for rows this handler writes (`index.ts:311`).
///
/// Kept identical because it reaches a caller through [`WriteError::Conflict::context`], and a
/// test that asserts on the context is asserting that the *master* rejected the row rather than
/// some other writer.
pub const MASTER_WRITE_CONTEXT: &str = "replication-master-write";

/// The `isEqual` context string (`index.ts:295`).
///
/// **Deliberately a different string from [`MASTER_WRITE_CONTEXT`].** Upstream uses
/// `rxStorageInstanceToReplicationHandler-masterWrite` for the conflict handler's context and
/// `replication-master-write` for `bulkWrite`'s. They are not the same string, they are not
/// meant to be, and a port that notices they differ and "tidies" them into one has changed
/// behaviour: the context identifies *which* comparison and *which* write to a caller reading a
/// diagnostic, and the two appear in different places (a conflict handler callback vs a
/// `WriteError`).
pub const MASTER_WRITE_EQUALITY_CONTEXT: &str = "rxStorageInstanceToReplicationHandler-masterWrite";

/// A [`ReplicationHandler`] answering from a local storage instance.
///
/// Generic over the storage rather than `dyn` because the protocol's call pattern is
/// sequential and single-threaded (D1/D3): there is exactly one storage per handler, and boxing
/// it would only hide that.
pub struct StorageMaster<S> {
    instance: Rc<S>,
    conflict_handler: Box<dyn ConflictHandler>,
    /// The database instance token this master writes under. It appears in `_meta.o` on
    /// documents it authored, which is how a peer recognises a write as replication-authored.
    database_instance_token: String,
    /// The injected clock (D4). A master write assigns `_meta.lwt`, and that value is ordering
    /// information a client will compare against, so it comes from the same kind of clock the
    /// protocol uses rather than from the storage.
    clock: Box<dyn Clock>,
    /// Keep pushed documents' `_meta` (the migration case, `index.ts:188-193`).
    keep_meta: bool,
}

impl<S: RxStorageInstance> StorageMaster<S> {
    /// Builds the handler.
    ///
    /// `database_instance_token` is the *master's* token, not the client's: every document this
    /// handler writes carries it as the `_meta.o` write tag.
    pub fn new(
        instance: Rc<S>,
        conflict_handler: Box<dyn ConflictHandler>,
        database_instance_token: impl Into<String>,
    ) -> Self {
        StorageMaster {
            instance,
            conflict_handler,
            database_instance_token: database_instance_token.into(),
            clock: Box::new(MonotonicClock::new()),
            keep_meta: false,
        }
    }

    /// Replaces the clock (D4).
    pub fn with_clock(mut self, clock: Box<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Keeps the `_meta` of pushed documents rather than letting the storage restamp it.
    ///
    /// Only the migration path wants this (`index.ts:188-193`): a migration rewrites documents
    /// under a new schema and must not move their checkpoints, so a pushed document's existing
    /// `_meta.lwt` is preserved.
    pub fn with_keep_meta(mut self, keep_meta: bool) -> Self {
        self.keep_meta = keep_meta;
        self
    }

    fn has_attachments(&self) -> bool {
        self.instance.schema().has_attachments
    }

    fn primary_path(&self) -> String {
        self.instance.schema().primary_path.clone()
    }

    /// `writeDocToDocState(document, hasAttachments, keepMeta)` — what a peer receives.
    fn to_client_state(&self, stored: &DocumentData) -> DocumentData {
        write_doc_to_doc_state(stored, self.has_attachments(), self.keep_meta)
    }
}

/// The carried state of the master change stream.
///
/// A named struct rather than a five-tuple: `unfold`'s state is threaded through every item, and
/// a tuple this wide is where an argument-order mistake hides. It also lets [`map_one_bulk`] be a
/// plain async fn that returns `Result`, so the fallible part is testable and the infallible
/// part (ending the stream) is the only thing the closure does.
struct MappingState<S: RxStorageInstance> {
    stream: dxdb_storage::ChangeStream,
    instance: Rc<S>,
    primary_path: String,
    has_attachments: bool,
    keep_meta: bool,
}

/// One storage bulk → one `PullStreamItem::Changes`.
///
/// `Ok(None)` means the storage's feed ended; `Err` means the mapping failed and the stream
/// should stop. Ports the `mergeMap` body at `index.ts:201-225`.
async fn map_one_bulk<S: RxStorageInstance>(
    mut state: MappingState<S>,
) -> Result<Option<(PullStreamItem, MappingState<S>)>, StorageError> {
    let Some(bulk) =
        futures::future::poll_fn(|context| state.stream.as_mut().poll_next(context)).await
    else {
        return Ok(None);
    };

    let checkpoint = match bulk.checkpoint.clone() {
        Some(checkpoint) => Some(
            ProtocolCheckpoint::new(checkpoint.to_json())
                .map_err(|error| StorageError::Unsupported(error.to_string()))?,
        ),
        None => None,
    };

    let mut documents = Vec::with_capacity(bulk.events.len());
    for event in &bulk.events {
        documents.push(
            client_view(
                state.instance.as_ref(),
                &state.primary_path,
                state.has_attachments,
                state.keep_meta,
                &event.document_data,
            )
            .await?,
        );
    }

    Ok(Some((
        PullStreamItem::Changes(DocumentsWithCheckpoint {
            documents,
            checkpoint,
        }),
        state,
    )))
}

/// A peer's view of one stored document: state first, then every attachment payload.
///
/// Shared by [`StorageMaster::master_changes_since`] and [`StorageMaster::master_change_stream`]
/// because upstream applies the same two steps in both (`index.ts:205-219` for the stream,
/// `:236-250` for the pull). Keeping one copy means a change to the attachment rule cannot apply
/// to a pull but not to a live push, which is a divergence that would show up as a client that
/// converges slowly but never in sync.
///
/// `original_document = None` in the fill call is the master's rule: it does not know what a
/// given client already has, so it always sends every payload (`index.ts:212-217`).
async fn client_view<S: RxStorageInstance>(
    instance: &S,
    primary_path: &str,
    has_attachments: bool,
    keep_meta: bool,
    stored: &DocumentData,
) -> Result<DocumentData, StorageError> {
    let state = write_doc_to_doc_state(stored, has_attachments, keep_meta);
    if !has_attachments {
        return Ok(state);
    }
    let write =
        fill_write_data_for_attachments_change(instance, primary_path, &state, None).await?;
    // `DocumentWriteData` keeps payloads beside the JSON and a protocol document cannot carry
    // bytes, so they go back where a JS peer looks for them.
    let payloads = write.attachment_payloads().clone();
    let document = write.into_document_data();
    Ok(document_with_attachment_payloads(&document, &payloads))
}

impl<S: RxStorageInstance + 'static> ReplicationHandler for StorageMaster<S> {
    fn master_changes_since(
        &self,
        checkpoint: Option<&ProtocolCheckpoint>,
        batch_size: usize,
    ) -> BoxFuture<'_, Result<DocumentsWithCheckpoint, StorageError>> {
        // Cloned before the future is built: the trait's `BoxFuture<'_, _>` is tied to `&self`,
        // so the async block must not borrow the caller's checkpoint.
        let caller_checkpoint = checkpoint.cloned();
        let storage_checkpoint = match &caller_checkpoint {
            None => None,
            Some(checkpoint) => match Checkpoint::from_json(checkpoint.as_value()) {
                Some(checkpoint) => Some(checkpoint),
                // The handler is generic over the transport's checkpoint, but *this*
                // implementation's storage feed is a `{id, lwt}` feed. A checkpoint that is not
                // that shape cannot name a position in it.
                //
                // Upstream would silently pass such a value to `getChangedDocumentsSince` and
                // get whatever the storage did with it. Failing is the safer choice and it is
                // the same rule D7 states from the other side: the protocol must not be
                // constrained to `{id, lwt}`, and a storage-backed master is one caller that
                // *is* so constrained. A sharding or MongoDB-shaped checkpoint needs a different
                // master, which is exactly what the adapters provide.
                None => {
                    return Box::pin(async {
                        Err(StorageError::Unsupported(
                            "a storage-backed master answers a {id, lwt} checkpoint feed; this \
                             checkpoint is not of that shape, so it cannot name a position in it"
                                .to_string(),
                        ))
                    })
                }
            },
        };

        Box::pin(async move {
            let changed = self
                .instance
                .get_changed_documents_since(batch_size, storage_checkpoint)
                .await?;

            let primary = self.primary_path();
            let has_attachments = self.has_attachments();
            let mut documents = Vec::with_capacity(changed.documents.len());
            for stored in &changed.documents {
                documents.push(
                    client_view(
                        self.instance.as_ref(),
                        &primary,
                        has_attachments,
                        self.keep_meta,
                        stored,
                    )
                    .await?,
                );
            }

            // `checkpoint: result.documents.length > 0 ? result.checkpoint : checkpoint`
            // (`index.ts:234`). Re-reporting the storage's own beginning checkpoint when the
            // batch was empty would move a client's checkpoint backwards to the start of time.
            let checkpoint = if documents.is_empty() {
                caller_checkpoint.clone()
            } else {
                Some(
                    ProtocolCheckpoint::new(changed.checkpoint.to_json().clone())
                        .map_err(|error| StorageError::Unsupported(error.to_string()))?,
                )
            };

            Ok(DocumentsWithCheckpoint {
                documents,
                checkpoint,
            })
        })
    }

    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> BoxFuture<'_, Result<Vec<DocumentData>, StorageError>> {
        if rows.is_empty() {
            return Box::pin(async { Ok(Vec::new()) });
        }

        Box::pin(async move {
            let primary = self.primary_path();

            // `rowById` — upstream de-duplicates by primary before looking anything up
            // (`index.ts:259-264`), and the last row for an id wins. Keeping the order of first
            // appearance would change which row is written when a client (incorrectly) sends two
            // for one document.
            let mut ids: Vec<String> = Vec::new();
            let mut by_id: std::collections::BTreeMap<String, &WriteToMasterRow> =
                std::collections::BTreeMap::new();
            for row in &rows {
                let id = row
                    .new_document_state
                    .id(&primary)
                    .map_err(|error| StorageError::InvalidDocument(error.to_string()))?
                    .to_string();
                if !by_id.contains_key(&id) {
                    ids.push(id.clone());
                }
                by_id.insert(id, row);
            }

            // One lookup for the whole batch, with tombstones included: a document deleted on
            // the master is still a master state, and treating it as absent would let a client
            // resurrect it without a conflict.
            let master_docs = self
                .instance
                .find_documents_by_id(ids.clone(), true)
                .await?;
            let mut master_state: std::collections::BTreeMap<String, DocumentData> =
                std::collections::BTreeMap::new();
            for document in master_docs {
                let id = document
                    .id(&primary)
                    .map_err(|error| StorageError::InvalidDocument(error.to_string()))?
                    .to_string();
                master_state.insert(id, document);
            }

            let mut conflicts: Vec<DocumentData> = Vec::new();
            let mut writes: Vec<(String, BulkWriteRow)> = Vec::new();

            for id in &ids {
                let row = by_id[id];
                let stored = master_state.get(id);

                match (stored, &row.assumed_master_state) {
                    // No master state: an insert, whatever the client assumed.
                    (None, _) => {
                        let document = doc_state_to_write_doc(
                            &self.database_instance_token,
                            self.has_attachments(),
                            self.keep_meta,
                            &row.new_document_state,
                            None,
                            self.clock.now(),
                        );
                        writes.push((
                            id.clone(),
                            BulkWriteRow {
                                previous: None,
                                document,
                            },
                        ));
                    }
                    // Master state exists but the client assumed nothing: the client did not know
                    // what it was overwriting, so this is a conflict, not a blind write.
                    (Some(stored), None) => {
                        conflicts.push(self.to_client_state(stored));
                    }
                    // The client's assumed state must equal the master's, compared on document
                    // states (not revisions — those are per-side bookkeeping).
                    (Some(stored), Some(assumed)) => {
                        let stored_client_state = self.to_client_state(stored);
                        if !self.conflict_handler.is_equal(
                            &stored_client_state,
                            assumed,
                            MASTER_WRITE_EQUALITY_CONTEXT,
                        ) {
                            conflicts.push(stored_client_state);
                        } else {
                            let document = doc_state_to_write_doc(
                                &self.database_instance_token,
                                self.has_attachments(),
                                self.keep_meta,
                                &row.new_document_state,
                                Some(stored),
                                self.clock.now(),
                            );
                            writes.push((
                                id.clone(),
                                BulkWriteRow {
                                    previous: Some(stored.clone()),
                                    document,
                                },
                            ));
                        }
                    }
                }
            }

            // Attachment payloads travelled beside the document (`WriteToMasterRow::
            // attachment_payloads`) because a JSON document cannot hold bytes. The storage takes
            // them the same way, inside `DocumentWriteData`, so they are carried across without
            // ever being encoded into the JSON.
            let write_rows = writes
                .into_iter()
                .map(|(id, row)| {
                    match by_id
                        .get(&id)
                        .filter(|row| !row.attachment_payloads.is_empty())
                    {
                        Some(source) => BulkWriteRow {
                            previous: row.previous,
                            document: DocumentWriteData::with_payloads(
                                row.document.into_document_data(),
                                source.attachment_payloads.clone(),
                            ),
                        },
                        None => row,
                    }
                })
                .collect::<Vec<_>>();

            let response = self
                .instance
                .bulk_write(write_rows, MASTER_WRITE_CONTEXT.to_string())
                .await?;

            // A row the storage rejected as a conflict is a conflict for the protocol too: the
            // storage saw a `previous` that had moved. Its `document_in_db` is the truth to
            // report, so the client can resolve against what is actually there.
            for error in response.error {
                match &error {
                    WriteError::Conflict { document_in_db, .. } => match document_in_db {
                        Some(document) => conflicts.push(self.to_client_state(document)),
                        // A storage may report a conflict without the stored state. The honest
                        // answer then is the state the client assumed, which it can re-send
                        // against; inventing one would resolve against a fiction.
                        None => {
                            let id = error.document_id();
                            if let Some(row) = by_id.get(id) {
                                if let Some(assumed) = &row.assumed_master_state {
                                    conflicts.push(assumed.clone());
                                }
                            }
                        }
                    },
                    // A validation or attachment failure is not a conflict. Reporting it as one
                    // would make the client retry the same bad document forever, so it surfaces.
                    other => {
                        let status = other.status();
                        let document_id = other.document_id().to_string();
                        return Err(StorageError::Unsupported(format!(
                            "the master's storage rejected a write row with status {status} \
                             (document {document_id})"
                        )));
                    }
                }
            }

            Ok(conflicts)
        })
    }

    fn master_change_stream(&self) -> PullStream {
        // Ports `masterChangeStream$` (`index.ts:200-226`): the storage's own feed, with each
        // bulk mapped through `writeDocToDocState` and — when the collection has attachments —
        // `fillWriteDataForAttachmentsChange(.., None)`, i.e. every payload, because the master
        // does not know what a given client holds.
        //
        // `unfold`'s closure yields `Option<(item, state)>`, so it cannot carry a `Result`. The
        // mapping step can fail (a malformed checkpoint, an attachment read error) and
        // `PullStreamItem` has no `Err` variant to report it in, so both "the feed ended" and
        // "the mapping failed" end the stream. That is a deliberate choice, not `?` abuse:
        // a closed stream means "re-subscribe", the same recovery a transport already has for a
        // dropped socket, and the protocol's authoritative error channel is
        // `master_changes_since`, which does report errors. Spelled out rather than using
        // `.ok()?` so the discard is visible at the call site.
        let mapping = |state: MappingState<S>| async move {
            // `unwrap_or_default()` on an `Option<Option<_>>` would be wrong here, so this is
            // the `Result<Option<_>, _>` -> `Option<_>` collapse the item type forces.
            map_one_bulk(state).await.unwrap_or(None)
        };

        let mapped = futures::stream::unfold(
            MappingState {
                stream: self.instance.change_stream(),
                instance: self.instance.clone(),
                primary_path: self.primary_path(),
                has_attachments: self.has_attachments(),
                keep_meta: self.keep_meta,
            },
            mapping,
        );

        Box::pin(mapped)
    }
}
