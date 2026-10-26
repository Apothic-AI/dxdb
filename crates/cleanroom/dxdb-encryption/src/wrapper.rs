//! The wrapper: field encryption as a `RxStorageInstance` decorator.
//!
//! `specs/encryption.md` section 1.1: encryption is applied at the storage-instance
//! boundary, so everything above sees plaintext and everything below sees
//! ciphertext. This is the same wrapper position as upstream's
//! `wrappedKeyEncryptionCryptoJsStorage`, which is why CLEANROOM.md section 3 can
//! say the clean-room crates plug into the port without touching core.
//!
//! # Why the wrapper is where the invariants live
//!
//! Every operation that can return a document must decrypt on the way out, and every
//! operation that can store one must encrypt on the way in. Missing one is a silent
//! ciphertext leak (INV-2), so the operations are enumerated rather than
//! inherited: `find_documents_by_id`, `query`, `get_changed_documents_since` and
//! `change_stream` all return documents, and all four are handled explicitly below.

use std::sync::Arc;

use dxdb_storage::{ChangeEvent, ChangeStream, EventBulk};
use dxdb_storage::{DocumentData, DocumentWriteData};
use dxdb_storage::PreparedQuery;
use dxdb_storage::StorageSchema;
use dxdb_storage::{
    ChangedDocuments, CountResult, DynRxStorageInstance, InstanceCreationParams, RxStorage,
    RxStorageInstance, StorageError,
};
use dxdb_storage::{BulkWriteResponse, BulkWriteRow};
use futures::StreamExt as _;

use crate::codec::{attachment_field_path, FieldCodec};
use crate::config::EncryptedPaths;
use crate::entropy::EntropySource;
use crate::keys::{KeyMaterial, Keys, KdfParams, SALT_LEN};
use crate::EncryptionError;

/// The settings one collection is wrapped with.
#[derive(Debug, Clone)]
pub struct EncryptionConfig {
    /// The validated `encrypted` list.
    pub encrypted: EncryptedPaths,
    /// Whether attachment payloads are encrypted.
    pub attachments_encrypted: bool,
}

impl EncryptionConfig {
    /// Reads a config out of a schema value.
    pub fn from_schema(schema: &serde_json::Value) -> Result<Self, EncryptionError> {
        let encrypted = EncryptedPaths::from_schema(schema)?;
        let attachments_encrypted = schema
            .get("attachments")
            .and_then(|attachments| attachments.get("encrypted"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Ok(Self {
            encrypted,
            attachments_encrypted,
        })
    }

    /// True when there is nothing to encrypt.
    pub fn is_inert(&self) -> bool {
        self.encrypted.is_empty() && !self.attachments_encrypted
    }
}

/// A storage wrapped for field encryption.
pub struct EncryptionStorage<S> {
    inner: S,
    keys: Arc<Keys>,
    config: EncryptionConfig,
}

impl<S> EncryptionStorage<S> {
    /// Wraps `inner`.
    pub fn new(inner: S, keys: Arc<Keys>, config: EncryptionConfig) -> Self {
        Self {
            inner,
            keys,
            config,
        }
    }

    /// The wrapped storage.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The open keys. Exposed so a test (or a caller wiring a second instance of the
    /// same database) can share them; it reveals no key material, only the ring
    /// state, and `Keys` has no accessor that returns key bytes.
    pub fn keys(&self) -> &Arc<Keys> {
        &self.keys
    }

    /// The config this storage was built with.
    pub fn config(&self) -> &EncryptionConfig {
        &self.config
    }
}

impl<S: RxStorage> RxStorage for EncryptionStorage<S> {
    fn name(&self) -> &str {
        "dxdb-encryption"
    }

    async fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> Result<Box<DynRxStorageInstance<'static>>, StorageError> {
        let declared = crate::EncryptedPaths::from_schema(&params.schema.raw)
            .map_err(|error| StorageError::InvalidSchema(error.to_string()))?;
        if declared != self.config.encrypted {
            // A config that does not match the schema it is applied to would encrypt
            // a different set of paths than the schema says, which is a silent
            // correctness gap in both directions.
            return Err(StorageError::InvalidSchema(
                "encryption config does not match the schema's encrypted list".to_string(),
            ));
        }
        let inner = self.inner.create_storage_instance(params.clone()).await?;
        let instance = EncryptionInstance::new(
            inner,
            Arc::clone(&self.keys),
            self.config.clone(),
            params.schema,
        );
        Ok(DynRxStorageInstance::new_box(instance))
    }
}

/// One collection's encryption wrapper.
pub struct EncryptionInstance {
    inner: Box<DynRxStorageInstance<'static>>,
    keys: Arc<Keys>,
    config: EncryptionConfig,
    codec: FieldCodec,
    schema: StorageSchema,
}

impl std::fmt::Debug for EncryptionInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptionInstance")
            .field("primary_path", &self.schema.primary_path)
            .field("encrypted_paths", &self.config.encrypted.as_slice())
            .field("attachments_encrypted", &self.config.attachments_encrypted)
            .field("generation", &self.keys.current_generation())
            .finish_non_exhaustive()
    }
}

/// Maps an encryption failure into the storage error space.
///
/// The storage layer above this wrapper has one error type, so every encryption
/// failure arrives as `Unsupported` with a message that names the cause. The one
/// case that matters for a caller's control flow — a query on an encrypted path —
/// keeps its own prefix so it is greppable and testable.
fn to_storage_error(error: EncryptionError) -> StorageError {
    match error {
        EncryptionError::EncryptedPathInQuery(path) => {
            StorageError::Unsupported(format!("encrypted path in query: {path}"))
        }
        other => StorageError::Unsupported(other.to_string()),
    }
}

impl EncryptionInstance {
    /// Builds an instance from parts.
    ///
    /// Public because a caller that already holds an inner instance (a rotation, a
    /// second view of the same collection) needs to wrap *it* rather than create a
    /// new one through `RxStorage::create_storage_instance`.
    pub fn new(
        inner: Box<DynRxStorageInstance<'static>>,
        keys: std::sync::Arc<Keys>,
        config: EncryptionConfig,
        schema: StorageSchema,
    ) -> Self {
        let codec = FieldCodec {
            paths: config.encrypted.clone(),
            schema_hash: keys.schema_hash(),
            primary_path: schema.primary_path.clone(),
        };
        Self {
            inner,
            keys,
            config,
            codec,
            schema,
        }
    }

    /// Rejects a query that names an encrypted path.
    ///
    /// INV-12, `docs/CLEANROOM-GAPS.md` G-031. Both the sort and the selector are
    /// checked: a sort on ciphertext is a sort on random bytes, and a selector on
    /// ciphertext silently matches nothing.
    fn reject_encrypted_paths(&self, prepared: &PreparedQuery) -> Result<(), StorageError> {
        if self.config.encrypted.is_empty() {
            return Ok(());
        }
        for sort in &prepared.query.sort {
            if self.config.encrypted.covers(&sort.field) {
                return Err(to_storage_error(EncryptionError::EncryptedPathInQuery(
                    sort.field.clone(),
                )));
            }
        }
        if let Some(selector) = prepared.query.selector.as_object() {
            for field in selector.keys() {
                if self.config.encrypted.covers(field) {
                    return Err(to_storage_error(EncryptionError::EncryptedPathInQuery(
                        field.clone(),
                    )));
                }
            }
        }
        Ok(())
    }

    /// Decrypts a batch of documents in place.
    fn open_all<'a>(
        &self,
        documents: impl IntoIterator<Item = &'a mut DocumentData>,
    ) -> Result<(), StorageError> {
        if self.config.is_inert() {
            return Ok(());
        }
        for document in documents {
            self.codec
                .open_document(document, &self.keys)
                .map_err(to_storage_error)?;
        }
        Ok(())
    }
}

impl RxStorageInstance for EncryptionInstance {
    fn database_name(&self) -> &str {
        self.inner.database_name()
    }

    fn collection_name(&self) -> &str {
        self.inner.collection_name()
    }

    fn schema(&self) -> &StorageSchema {
        &self.schema
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn underlying_persistent_storage(&self) -> Option<&DynRxStorageInstance<'_>> {
        // Encryption is not a cache, so there is no "underlying persistent" instance
        // for replication to redirect to. Forwarding the inner answer would be
        // wrong: replication must run *through* encryption, never around it.
        None
    }

    async fn bulk_write(
        &self,
        document_writes: Vec<BulkWriteRow>,
        context: String,
    ) -> Result<BulkWriteResponse, StorageError> {
        if self.config.is_inert() {
            return self.inner.bulk_write(document_writes, context).await;
        }
        let mut sealed_rows = Vec::with_capacity(document_writes.len());
        for row in document_writes {
            let BulkWriteRow {
                previous,
                document: write_data,
            } = row;
            let revision = write_data.document().rev().map(str::to_string);
            let document_id = write_data
                .document()
                .id(&self.codec.primary_path)
                .map_err(|_| to_storage_error(EncryptionError::MissingDocumentId))?
                .to_string();

            let mut sealed_document = write_data.document().clone();
            self.codec
                .seal_document(&mut sealed_document, &self.keys)
                .map_err(to_storage_error)?;

            let mut sealed_payloads = std::collections::BTreeMap::new();
            if self.config.attachments_encrypted {
                for (attachment_id, payload) in write_data.attachment_payloads() {
                    let field_path = attachment_field_path(attachment_id);
                    let sealed = self.codec.seal_bytes(
                        &self.keys,
                        &document_id,
                        &field_path,
                        revision.as_deref(),
                        payload,
                    );
                    sealed_payloads.insert(attachment_id.clone(), sealed.into_bytes());
                }
            } else {
                for (attachment_id, payload) in write_data.attachment_payloads() {
                    sealed_payloads.insert(attachment_id.clone(), payload.clone());
                }
            }
            sealed_rows.push(BulkWriteRow {
                previous,
                document: DocumentWriteData::with_payloads(sealed_document, sealed_payloads),
            });
        }
        self.inner.bulk_write(sealed_rows, context).await
    }

    async fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> Result<Vec<DocumentData>, StorageError> {
        let mut documents = self.inner.find_documents_by_id(ids, with_deleted).await?;
        // A tombstone is decrypted too when it is handed back: `with_deleted: true`
        // is how conflict resolution reads a document, and a caller must never
        // receive ciphertext in a field (INV-2).
        self.open_all(documents.iter_mut())?;
        Ok(documents)
    }

    async fn query(&self, prepared: PreparedQuery) -> Result<Vec<DocumentData>, StorageError> {
        self.reject_encrypted_paths(&prepared)?;
        let mut documents = self.inner.query(prepared).await?;
        self.open_all(documents.iter_mut())?;
        Ok(documents)
    }

    async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        // Count returns no documents, so there is nothing to decrypt and nothing to
        // leak. It still rejects an encrypted path: a caller counting documents by a
        // selector they cannot run would otherwise get a plausible-looking zero.
        self.reject_encrypted_paths(&prepared)?;
        self.inner.count(prepared).await
    }

    async fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
        digest: String,
    ) -> Result<Vec<u8>, StorageError> {
        let stored = self
            .inner
            .get_attachment_data(document_id.clone(), attachment_id.clone(), digest)
            .await?;
        if !self.config.attachments_encrypted {
            return Ok(stored);
        }
        // An attachment read has no document in hand, so the revision is not
        // available and the attachment AAD carries `revision: None` — the same value
        // the write path used for an attachment. Recorded in
        // `docs/CLEANROOM-GAPS.md` G-045 as a documented narrowing: attachment
        // ciphertexts are bound to their document and attachment id but not to a
        // revision.
        let field_path = attachment_field_path(&attachment_id);
        self.codec
            .open_bytes(&self.keys, &document_id, &field_path, None, &stored)
            .map(|bytes| bytes.as_slice().to_vec())
            .map_err(to_storage_error)
    }

    async fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<dxdb_storage::Checkpoint>,
    ) -> Result<ChangedDocuments, StorageError> {
        let mut changed = self
            .inner
            .get_changed_documents_since(limit, checkpoint)
            .await?;
        self.open_all(changed.documents.iter_mut())?;
        Ok(changed)
    }

    fn change_stream(&self) -> ChangeStream {
        if self.config.is_inert() {
            return self.inner.change_stream();
        }
        // The change feed carries whole documents, so passing it through would hand
        // every subscriber ciphertext in `document_data`. That is the easiest way for
        // INV-2 to be violated in practice: a live query above this layer would
        // compare its selector against a sealed string and silently match nothing.
        let inner = self.inner.change_stream();
        let keys = Arc::clone(&self.keys);
        let codec = self.codec.clone();
        Box::pin(inner.map(move |bulk| decrypt_bulk(&codec, &keys, bulk)))
    }

    async fn cleanup(&self, minimum_deleted_time: f64) -> Result<bool, StorageError> {
        self.inner.cleanup(minimum_deleted_time).await
    }

    async fn close(&self) -> Result<(), StorageError> {
        self.inner.close().await
    }

    async fn remove(&self) -> Result<(), StorageError> {
        self.inner.remove().await
    }
}

/// Decrypts one change bulk.
///
/// An event whose documents cannot be opened is dropped rather than emitted with
/// ciphertext still in it. A missing event is recoverable — the next
/// `get_changed_documents_since` returns the document — whereas an event whose
/// `document_data` is ciphertext would look like a document that no longer matches
/// the subscriber's selector, which is a silent wrong answer.
fn decrypt_bulk(codec: &FieldCodec, keys: &Keys, bulk: EventBulk) -> EventBulk {
    let events = bulk
        .events
        .into_iter()
        .filter_map(|event| decrypt_event(codec, keys, event))
        .collect();
    EventBulk {
        id: bulk.id,
        events,
        checkpoint: bulk.checkpoint,
        context: bulk.context,
    }
}

fn decrypt_event(codec: &FieldCodec, keys: &Keys, event: ChangeEvent) -> Option<ChangeEvent> {
    let mut document = event.document_data;
    codec.open_document(&mut document, keys).ok()?;
    let previous = match event.previous_document_data {
        Some(mut previous) => {
            codec.open_document(&mut previous, keys).ok()?;
            Some(previous)
        }
        None => None,
    };
    Some(ChangeEvent {
        document_id: event.document_id,
        operation: event.operation,
        document_data: document,
        previous_document_data: previous,
    })
}

/// Rotation: re-encrypt every document at the next key generation.
///
/// `specs/encryption.md` section 3.6, INV-7. The guarantee is that every document's
/// plaintext after rotation equals its plaintext before, byte for byte, and the
/// mechanism that makes it true is that rotation **decrypts to plaintext and then
/// re-encrypts**, rather than re-wrapping ciphertext. Re-wrapping would be cheaper
/// but would leave the `gen` byte as the only thing that changed, which is exactly
/// the kind of shortcut that makes a rotation test pass while the data is still
/// readable under the retired key.
pub async fn rotate(
    instance: &EncryptionInstance,
    mut new_data_key: crate::keys::Key,
    batch_size: usize,
) -> Result<RotationProgress, StorageError> {
    if batch_size == 0 {
        return Err(StorageError::Unsupported(
            "rotation batch size must be positive".to_string(),
        ));
    }
    // Step 1: generate and persist the next generation before touching a single
    // document, so an interruption leaves a database that can still be opened at
    // the old generation with the new key present but unused.
    let (next_ring, wrapped) = {
        let current = instance.keys.ring().clone();
        let new_key = std::mem::replace(&mut new_data_key, zeroize::Zeroizing::new([0u8; 32]));
        current.rotate(new_key).ok_or_else(|| {
            StorageError::Unsupported("key generation overflowed 255".to_string())
        })?
    };
    let new_generation = next_ring.current;

    // Step 2: walk the collection in `(_meta.lwt, primary)` order, which is the
    // order `get_changed_documents_since` provides, so a rotation is resumable from
    // a checkpoint rather than from the beginning.
    let mut checkpoint: Option<dxdb_storage::Checkpoint> = None;
    let mut rewritten = 0usize;
    loop {
        let changed = instance
            .inner
            .get_changed_documents_since(batch_size, checkpoint.clone())
            .await?;
        if changed.documents.is_empty() {
            break;
        }
        // Termination is the checkpoint *advancing*. `len == batch_size` is not a
        // sufficient condition: a storage whose feed does not move its checkpoint
        // would make this loop rewrite the same documents forever, and a rotation
        // that never ends is worse than one that stops early. The next checkpoint
        // must differ from the last, or there is nothing left to consume.
        let next_checkpoint = changed.checkpoint.clone();
        if checkpoint.as_ref() == Some(&next_checkpoint) {
            break;
        }
        checkpoint = Some(next_checkpoint);
        let mut rows = Vec::with_capacity(changed.documents.len());
        // One view of the new ring for the whole loop. It must be the *same* key
        // material across passes, because a multi-pass rotation re-reads documents it
        // already rewrote, and those are at the new generation.
        let rotated_keys = Keys::with_ring(&instance.keys, next_ring.clone());
        for mut document in changed.documents {
            // These come from the *inner* storage, so they are still ciphertext.
            // Decrypt with the **rotated** ring, which can read both generations: the
            // documents in a later pass are already at the new generation, and an
            // old-only ring would refuse to open them.
            instance
                .codec
                .open_document(&mut document, &rotated_keys)
                .map_err(to_storage_error)?;
            let previous = document.clone();
            instance
                .codec
                .seal_document(&mut document, &rotated_keys)
                .map_err(to_storage_error)?;
            rows.push(BulkWriteRow {
                previous: Some(previous),
                document: DocumentWriteData::new(document),
            });
        }
        rewritten += rows.len();
        let response = instance.inner.bulk_write(rows, "dxdb-rotation".to_string()).await?;
        if !response.is_ok() {
            // A conflict during rotation means someone wrote concurrently. Stopping
            // is correct: the next rotation pass picks the document up at its new
            // revision, and continuing would overwrite a concurrent write.
            return Err(StorageError::Unsupported(format!(
                "rotation stopped on {} conflicting writes; re-run to continue",
                response.error.len()
            )));
        }
    }

    Ok(RotationProgress {
        generation: new_generation,
        rewritten,
        wrapped_key: wrapped,
    })
}

/// What a rotation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationProgress {
    /// The generation the collection now writes at.
    pub generation: u8,
    /// How many documents were re-encrypted.
    pub rewritten: usize,
    /// The wrapped key to persist alongside the new `current_generation`.
    pub wrapped_key: Vec<u8>,
}

/// Builds key material for a fresh database.
pub fn create_key_material(
    password: crate::config::Password,
    schema_hash: [u8; 32],
    entropy: Arc<dyn EntropySource>,
) -> Result<(Keys, KeyMaterial), EncryptionError> {
    Keys::create(password, schema_hash, entropy)
}

/// A convenience for a collection with no encrypted paths: the wrapper still exists
/// so the stack shape is uniform, but every operation is a pass-through except the
/// query check (which has nothing to check).
pub fn inert_config() -> EncryptionConfig {
    EncryptionConfig {
        encrypted: EncryptedPaths::new(std::iter::empty()).expect("empty is valid"),
        attachments_encrypted: false,
    }
}

/// The KDF parameters a new database is created with, re-exported so a caller can
/// record them without reaching into `keys`.
pub const NEW_DATABASE_KDF: KdfParams = KdfParams::RFC9106_SECOND_RECOMMENDED;

/// The salt length a new database's salt has.
pub const NEW_DATABASE_SALT_LEN: usize = SALT_LEN;
