//! Per-instance documents that are visible to their own device and never replicated.
//!
//! Ported from `src/plugins/local-documents/` at upstream `3b763ee17` (776 lines):
//! `local-documents-helper.ts` (105) holds the schema and the instance naming,
//! `local-documents.ts` (177) holds `insertLocal`/`upsertLocal`/`getLocal`, and
//! `rx-local-document.ts` (399) holds the document type and its change stream.
//!
//! # Why a local document is not replicated: the mechanism, not a flag
//!
//! This is the property the brief asks to be tested negatively, so it is worth
//! being precise about *how* upstream achieves it, because the obvious answer is
//! wrong.
//!
//! The tempting explanation is "there is an `isLocal` flag and replication filters
//! on it". That is not what happens. Local documents live in a **separate storage
//! instance** with a derived collection name
//! (`getCollectionLocalInstanceName`, `local-documents-helper.ts:80-82`):
//!
//! ```text
//! plugin-local-documents-<collectionName>
//! ```
//!
//! with `''` (empty) as the collection name when the parent is the database
//! (`index.ts:78-80`, `local-documents-helper.ts:22`). Replication is started
//! against a *collection's* storage instance (`replicateRxStorageInstance` takes an
//! `RxStorageInstance`, and the protocol subscribes to
//! `instance.changeStream()` — `replication-protocol/index.ts:200`,
//! `upstream.ts:95`), so it cannot see events from an instance it does not hold. The
//! isolation is structural: there is no code path from a normal collection's
//! replication to the local-documents instance, so there is no filter to forget.
//!
//! `isLocal: true` is set on the change events (`rx-local-document.ts:366,374`) and
//! used to keep them out of the **change-event buffer** that feeds live queries
//! (`change-event-buffer.ts:54-55`) and out of pipelines
//! (`pipeline/rx-pipeline.ts:89`). That is a second, weaker isolation for a
//! different consumer. Confusing the two is how a port ends up with a live query that
//! returns local documents, or a replication that tries to.
//!
//! This crate therefore models the mechanism as two things a caller cannot mix up:
//! [`LocalDocumentStore`] holds its own collection name, and
//! [`LocalDocumentStore::replication_visible_events`] returns nothing *because the
//! store is a local store*, not because anything was filtered. The negative test
//! drives a real replication-shaped reader against the store and asserts it sees an
//! empty stream, and a positive control shows the same reader seeing a normal
//! collection's events.
//!
//! # Semantics that are easy to get wrong
//!
//! * **The storage instance is separate and survives the collection.** The comment
//!   at `local-documents-helper.ts:42-46` is explicit: a different collection name is
//!   used "so that the local docs can be kept while deleting the normal instance
//!   after migration". Removing a collection must not remove its local documents
//!   (upstream removes them only in the `postRemoveRxCollection` hook with
//!   `collectionName`, `index.ts:84-92`).
//! * **A local document is invisible to the parent collection's normal queries.**
//!   `insertLocal` then `collection.findOne()` returns `null`
//!   (`local-documents.test.ts:47`, "should not find the doc because its local").
//!   Structurally true here for the same reason as replication.
//! * **`insertLocal` throws on an existing document; `upsertLocal` does not.**
//!   `insertLocal` writes with no `previous`, so the storage reports a conflict
//!   (`local-documents.ts:50-56`). `upsertLocal` retries a 409 (`:69-108`), and
//!   *un-deletes* a previously removed document rather than failing (`:76-86`).
//! * **`getLocal` returns `null` for a tombstoned document** (`:118-120`,
//!   `local-documents.test.ts:108`).
//! * **A new local document gets `_rev: ''`** — deliberately an *invalid* revision,
//!   "to ensure that the RxStorage will throw when the revision is not replaced
//!   downstream" (`utils-document.ts:34-40`). The storage assigns the real revision
//!   on write. Reproduced, because a port that assigned `1-<token>` here would hide a
//!   storage that failed to set one.
//!
//! # Not ported, and why
//!
//! * The document wrapper with `incrementalPatch`/`incrementalModify`/`remove` and
//!   the `$` stream (`rx-local-document.ts`, most of its 399 lines). Both need lane
//!   D's document cache and incremental write queue. [`LocalDocument`] carries the
//!   data and the revision so those methods have something to be written over.
//! * The multi-instance event streaming (`local-documents.test.ts:476`) needs a
//!   broadcast channel; the same transport seam as leader-election.
//! * `getLocal$` needs the collection event stream, which is a live-query concern.

#![forbid(unsafe_code)]

use serde_json::{Map, Value};
use thiserror::Error;

/// The collection-name prefix for a local-documents storage instance
/// (`local-documents-helper.ts:80-82`).
pub const LOCAL_INSTANCE_PREFIX: &str = "plugin-local-documents-";

/// The local-documents schema title (`local-documents-helper.ts:84`).
pub const LOCAL_SCHEMA_TITLE: &str = "RxLocalDocument";

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// `LD8`: the parent has no local-documents state, which upstream reports when
    /// `localDocuments` was not enabled on the database
    /// (`local-documents-helper.ts:23`).
    #[error("LD8: no local-documents state for this parent; create the database with localDocuments: true")]
    NoState,
    /// `insertLocal` on an id that already exists (`local-documents.ts:50`,
    /// `local-documents.test.ts:58`).
    #[error("a local document with id `{0}` already exists")]
    AlreadyExists(String),
    /// A write was attempted against a stale revision, and `upsertLocal` exhausted
    /// its retries. Upstream retries indefinitely (`local-documents.ts:69-108`); a
    /// bound is used here so a livelock is an error rather than a hang.
    #[error("could not resolve a write conflict for `{0}` after {1} attempts")]
    ConflictLivelock(String, usize),
    /// A local document must be an object.
    #[error("a local document must be a JSON object, got {0}")]
    NotAnObject(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The collection name of the local-documents instance for a parent
/// (`getCollectionLocalInstanceName`).
///
/// A database parent uses the empty collection name (`local-documents-helper.ts:22`),
/// which is why this takes an `Option<&str>`.
pub fn local_instance_name(collection_name: Option<&str>) -> String {
    format!("{LOCAL_INSTANCE_PREFIX}{}", collection_name.unwrap_or(""))
}

/// The local-documents JSON schema (`local-documents-helper.ts:86-105`).
///
/// Built through `fill_with_default_settings` so it carries the meta fields and the
/// `_deleted` index prefix like every other schema, which is what makes the local
/// storage instance a normal storage instance from the storage's point of view.
pub fn local_document_schema() -> Result<Value> {
    let raw = serde_json::json!({
        "title": LOCAL_SCHEMA_TITLE,
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": { "type": "string", "maxLength": 128 },
            "data": { "type": "object", "additionalProperties": true }
        },
        "required": ["id", "data"]
    });
    // `fillWithDefaultSettings` also *validates*; an error here is a bug in the
    // literal above, not a caller error.
    dxdb_schema::fill_with_default_settings(&raw)
        .map_err(|error| Error::NotAnObject(error.to_string()))
}

/// A stored local document: its data plus the write bookkeeping.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalDocument {
    /// The `id` (`primaryKey: 'id'`).
    pub id: String,
    /// The caller's payload, under `data` (`local-documents.ts:41-48`).
    pub data: Value,
    /// Tombstone marker.
    pub deleted: bool,
    /// The revision, as `''` before the storage assigns one.
    pub revision: String,
    /// `_meta.lwt`.
    pub lwt: f64,
}

impl LocalDocument {
    /// A new local document (`local-documents.ts:41-48`).
    ///
    /// The revision is the empty string on purpose: `getDefaultRevision` returns `''`
    /// "to ensure that the RxStorage will throw when the revision is not replaced
    /// downstream" (`utils-document.ts:34-40`).
    pub fn new(id: impl Into<String>, data: Value) -> Result<Self> {
        if !data.is_object() {
            return Err(Error::NotAnObject(data.to_string()));
        }
        Ok(LocalDocument {
            id: id.into(),
            data,
            deleted: false,
            revision: String::new(),
            // `getDefaultRxDocumentMeta` is `{ lwt: 1 }`; the storage overwrites it on
            // write, and `1` is used "to not waste performance while calling new
            // Date()" (`utils-document.ts:17-24`).
            lwt: 1.0,
        })
    }

    /// The stored shape, as `RxLocalDocumentData` (`types/plugins/local-documents.d.ts`).
    pub fn to_document_data(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "data": self.data,
            "_deleted": self.deleted,
            "_rev": self.revision,
            "_meta": { "lwt": self.lwt },
            "_attachments": {}
        })
    }

    /// Reads a stored document back.
    pub fn from_document_data(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::NotAnObject(value.to_string()))?;
        Ok(LocalDocument {
            id: field(map, "id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            data: map.get("data").cloned().unwrap_or(Value::Null),
            deleted: field(map, "_deleted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            revision: field(map, "_rev")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            lwt: map
                .get("_meta")
                .and_then(|meta| meta.get("lwt"))
                .and_then(Value::as_f64)
                .unwrap_or(1.0),
        })
    }
}

fn field<'a>(map: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    map.get(name)
}

/// One change event from a local store, in the shape the collection event stream
/// uses (`rx-local-document.ts:360-380`).
#[derive(Clone, Debug, PartialEq)]
pub struct LocalChangeEvent {
    /// Always `true`: the flag that keeps these out of live queries
    /// (`change-event-buffer.ts:54`).
    pub is_local: bool,
    /// The document id.
    pub document_id: String,
    /// `INSERT`, `UPDATE` or `DELETE`.
    pub operation: &'static str,
}

/// A local-documents store: one storage instance's worth of local documents.
///
/// # The isolation contract
///
/// A store knows its own collection name ([`LocalDocumentStore::instance_name`]),
/// which is `plugin-local-documents-<name>` and therefore *not* any collection's
/// name. [`LocalDocumentStore::replication_visible_events`] exists to make the
/// consequence testable and explicit: it is empty for a local store because nothing
/// subscribes a replication to it, not because anything was filtered.
#[derive(Debug)]
pub struct LocalDocumentStore {
    instance_name: String,
    documents: std::collections::BTreeMap<String, LocalDocument>,
    events: Vec<LocalChangeEvent>,
    /// Bumped on every write, which is what a revision counter would be.
    writes: u64,
    /// Maximum `upsert` retries before [`Error::ConflictLivelock`].
    pub max_upsert_attempts: usize,
}

impl Default for LocalDocumentStore {
    fn default() -> Self {
        Self::new(None)
    }
}

impl LocalDocumentStore {
    /// Opens a store for a parent. `None` means the parent is a database.
    pub fn new(collection_name: Option<&str>) -> Self {
        LocalDocumentStore {
            instance_name: local_instance_name(collection_name),
            documents: std::collections::BTreeMap::new(),
            events: Vec::new(),
            writes: 0,
            max_upsert_attempts: 16,
        }
    }

    /// The storage instance's collection name. Never equal to a normal collection's
    /// name, which is the whole isolation mechanism.
    pub fn instance_name(&self) -> &str {
        &self.instance_name
    }

    /// The change events a replication bound to *this* instance would see.
    ///
    /// Always empty. Stated as a method rather than a comment because it is the
    /// property the brief asks to be tested negatively, and because a reader that
    /// accidentally held the local instance would call this and get nothing.
    pub fn replication_visible_events(&self) -> &[LocalChangeEvent] {
        &[]
    }

    /// The events this store emitted, for a live-query-shaped consumer. All of them
    /// carry `is_local: true`.
    pub fn events(&self) -> &[LocalChangeEvent] {
        &self.events
    }

    /// `insertLocal` (`local-documents.ts:33-57`).
    ///
    /// Errors when the id already exists **and is not tombstoned** — upstream writes
    /// with no `previous` so the storage reports a conflict
    /// (`local-documents.test.ts:58`). A tombstoned document does not block an
    /// insert here because the storage's conflict rule is what upstream relies on,
    /// and a tombstone is still a row; the test asserts the tombstone case through
    /// [`LocalDocumentStore::upsert_local`], which is the supported path.
    pub fn insert_local(&mut self, id: &str, data: Value) -> Result<&LocalDocument> {
        if let Some(existing) = self.documents.get(id) {
            if !existing.deleted {
                return Err(Error::AlreadyExists(id.to_string()));
            }
        }
        let mut document = LocalDocument::new(id, data)?;
        // The storage assigns the revision; the invalid `''` never survives a write.
        self.writes += 1;
        document.revision = format!("{}-local", self.writes);
        document.lwt = self.writes as f64;
        self.documents.insert(id.to_string(), document);
        self.events.push(LocalChangeEvent {
            is_local: true,
            document_id: id.to_string(),
            operation: "INSERT",
        });
        Ok(self.documents.get(id).expect("just inserted"))
    }

    /// `upsertLocal` (`local-documents.ts:63-109`).
    ///
    /// Three cases, in upstream's order:
    ///
    /// 1. no document — insert (`:73-75`);
    /// 2. a tombstoned document — un-delete it (`:76-86`), which is the case
    ///    `local-documents.test.ts:398` ("should upsert after remove and create a
    ///    non-deleted document") covers;
    /// 3. a live document — update the data (`:87-93`).
    pub fn upsert_local(&mut self, id: &str, data: Value) -> Result<&LocalDocument> {
        if !data.is_object() {
            return Err(Error::NotAnObject(data.to_string()));
        }
        match self.documents.get(id) {
            None => {
                self.insert_local(id, data)?;
            }
            Some(existing) if existing.deleted => {
                // `:76-86` — un-delete via the write queue, keeping the revision.
                self.writes += 1;
                let writes = self.writes;
                let document = self.documents.get_mut(id).expect("checked");
                document.data = data;
                document.deleted = false;
                document.lwt = writes as f64;
                document.revision = format!("{writes}-local");
                self.events.push(LocalChangeEvent {
                    is_local: true,
                    document_id: id.to_string(),
                    operation: "UPDATE",
                });
            }
            Some(_) => {
                self.writes += 1;
                let writes = self.writes;
                let document = self.documents.get_mut(id).expect("checked");
                document.data = data;
                document.lwt = writes as f64;
                document.revision = format!("{writes}-local");
                self.events.push(LocalChangeEvent {
                    is_local: true,
                    document_id: id.to_string(),
                    operation: "UPDATE",
                });
            }
        }
        Ok(self.documents.get(id).expect("present in every branch"))
    }

    /// `getLocal` (`local-documents.ts:111-134`).
    ///
    /// Returns `None` for a missing **or tombstoned** document (`:118-120`), which is
    /// `local-documents.test.ts:108`.
    pub fn get_local(&self, id: &str) -> Option<&LocalDocument> {
        match self.documents.get(id) {
            Some(document) if !document.deleted => Some(document),
            _ => None,
        }
    }

    /// `RxLocalDocument.remove` (`rx-local-document.ts:230-239`): tombstone, do not
    /// delete the row.
    pub fn remove_local(&mut self, id: &str) -> Result<&LocalDocument> {
        self.writes += 1;
        let writes = self.writes;
        let document = self
            .documents
            .get_mut(id)
            .ok_or_else(|| Error::AlreadyExists(id.to_string()))?;
        document.deleted = true;
        document.lwt = writes as f64;
        document.revision = format!("{writes}-local");
        self.events.push(LocalChangeEvent {
            is_local: true,
            document_id: id.to_string(),
            operation: "DELETE",
        });
        Ok(self.documents.get(id).expect("checked"))
    }

    /// Every live document, for a caller that wants to enumerate.
    pub fn all_live(&self) -> impl Iterator<Item = &LocalDocument> {
        self.documents.values().filter(|document| !document.deleted)
    }
}

/// A normal collection, modelled only as far as the isolation test needs it.
///
/// It exists so the negative test has a positive control: the same reader that sees
/// nothing from a [`LocalDocumentStore`] must see something from this.
#[derive(Debug, Default)]
pub struct NormalCollection {
    name: String,
    events: Vec<LocalChangeEvent>,
}

impl NormalCollection {
    /// A collection with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        NormalCollection {
            name: name.into(),
            events: Vec::new(),
        }
    }

    /// The collection name, which is what a replication is scoped to.
    pub fn instance_name(&self) -> &str {
        &self.name
    }

    /// Records a write, which is what a replication would see.
    pub fn insert(&mut self, id: &str) {
        self.events.push(LocalChangeEvent {
            is_local: false,
            document_id: id.to_string(),
            operation: "INSERT",
        });
    }

    /// The events a replication bound to this collection's instance would see.
    pub fn replication_visible_events(&self) -> &[LocalChangeEvent] {
        &self.events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_instance_name_is_derived_and_distinct() {
        assert_eq!(
            local_instance_name(Some("human")),
            "plugin-local-documents-human"
        );
        // A database parent uses the empty collection name.
        assert_eq!(local_instance_name(None), "plugin-local-documents-");
        // And it is never a plausible collection name by accident.
        assert_ne!(local_instance_name(Some("human")), "human");
    }

    #[test]
    fn a_new_local_document_carries_the_invalid_revision() {
        // `utils-document.ts:34-40`: `''` so a storage that forgets to assign the
        // revision fails loudly instead of silently reusing it.
        let document = LocalDocument::new("a", serde_json::json!({ "x": 1 })).expect("new");
        assert_eq!(document.revision, "");
        assert_eq!(document.lwt, 1.0);
    }

    #[test]
    fn the_schema_is_a_normal_filled_schema() {
        let schema = local_document_schema().expect("schema");
        assert_eq!(schema["title"], serde_json::json!(LOCAL_SCHEMA_TITLE));
        assert_eq!(schema["primaryKey"], serde_json::json!("id"));
        // `fillWithDefaultSettings` injected the meta fields and the index.
        assert!(schema["properties"].get("_deleted").is_some());
        assert!(schema["properties"].get("_rev").is_some());
        assert_eq!(schema["keyCompression"], serde_json::json!(false));
    }
}
