//! Change events: what a write broadcasts, and where replication resumes from.
//!
//! Upstream emits **bulks**, not individual events: `EventBulk<RxStorageChangeEvent,
//! Checkpoint>` bundles every event a single `bulkWrite` call produced, with one
//! id, one context, and one checkpoint (`rx-storage-helper.ts:205`
//! `categorizeBulkWriteRows` builds exactly this). Two consequences that show up
//! in the contract:
//!
//! * a checkpoint only means anything attached to a bulk, because it is derived
//!   from the *newest row* of that bulk;
//! * `context` is how a wrapper recognises events caused by its own write.
//!
//! `futures::Stream` replaces `Observable` (PLAN.md 3.3). A subscriber is a stream
//! consumer; dropping the stream unsubscribes. There is no `unsubscribe()` to
//! forget.

use futures::Stream;
use serde_json::Value;
use std::pin::Pin;

use crate::document::DocumentData;

/// What happened to one document in a bulk.
///
/// Note the asymmetry upstream guarantees and that code above the storage relies
/// on: a resurrected document (`_deleted: true` -> `false`) is reported as
/// `INSERT`, not `UPDATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// New document, or a deleted document coming back to life.
    Insert,
    /// Existing live document changed.
    Update,
    /// Document became `_deleted: true`.
    Delete,
}

impl Operation {
    /// The upstream spelling, which appears in every serialised event.
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Insert => "INSERT",
            Operation::Update => "UPDATE",
            Operation::Delete => "DELETE",
        }
    }

    /// Parses the upstream spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "INSERT" => Some(Operation::Insert),
            "UPDATE" => Some(Operation::Update),
            "DELETE" => Some(Operation::Delete),
            _ => None,
        }
    }
}

/// One document's change.
///
/// `document_data` / `previous_document_data` have different shapes per
/// operation, exactly as upstream's `RxChangeEventToEventReduceChangeEvent`
/// (`rx-change-event.ts:19`) reads them:
///
/// | operation | `document_data` | `previous_document_data` |
/// | --- | --- | --- |
/// | `INSERT` | the new document | the previous stored document, or `None` |
/// | `UPDATE` | the new document | the previous document (always present) |
/// | `DELETE` | the tombstone | the previous document (always present) |
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeEvent {
    /// Primary value of the changed document.
    pub document_id: String,
    /// What happened.
    pub operation: Operation,
    /// The new state. Present for all three operations; a `DELETE` carries the
    /// tombstone so a consumer can read the new `_meta.lwt`.
    pub document_data: DocumentData,
    /// The state before the write. `None` only for an `INSERT` that had no
    /// previous document.
    pub previous_document_data: Option<DocumentData>,
}

/// Where a change feed resumes.
///
/// `_meta.lwt` plus primary value is upstream's default checkpoint
/// (`RxStorageDefaultCheckpoint`), and it is the total order
/// `get_changed_documents_since` paginates over. Stored as the typed pair rather
/// than opaque bytes so every storage agrees on the comparison and so a
/// checkpoint is cheap to serialise and to compare.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    /// Primary value of the last document in the bulk.
    pub id: String,
    /// That document's `_meta.lwt`.
    pub lwt: f64,
}

impl Checkpoint {
    /// Builds a checkpoint.
    pub fn new(id: impl Into<String>, lwt: f64) -> Self {
        Checkpoint {
            id: id.into(),
            lwt,
        }
    }

    /// `{ id, lwt }` as JSON, with JavaScript number formatting for `lwt`.
    ///
    /// Replication persists checkpoints, so this must round-trip through
    /// `JSON.stringify` and back without drift — hence `dxdb-jsnum` and hence the
    /// lwt being a number rather than a string.
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("id".to_string(), Value::String(self.id.clone()));
        // `lwt` is always finite (it comes from `now()` or from a stored
        // `_meta.lwt`, whose schema caps it at 1e15).
        if let Some(number) = serde_json::Number::from_f64(self.lwt) {
            map.insert("lwt".to_string(), Value::Number(number));
        }
        Value::Object(map)
    }

    /// Parses `{ id, lwt }`.
    pub fn from_json(value: &Value) -> Option<Self> {
        let map = value.as_object()?;
        let id = map.get("id")?.as_str()?.to_string();
        let lwt = map.get("lwt")?.as_f64()?;
        Some(Checkpoint { id, lwt })
    }

    /// The "before everything" checkpoint upstream uses when none is given
    /// (`RX_META_LWT_MINIMUM` with an empty id), so a caller can always compare
    /// against a checkpoint instead of special-casing `None`.
    pub fn beginning() -> Self {
        Checkpoint {
            id: String::new(),
            lwt: crate::document::META_LWT_MINIMUM,
        }
    }
}

/// Every event one `bulk_write` call produced.
///
/// The id exists so a consumer can drop a duplicate: change feeds are
/// at-least-once across instances (`rx-storage-multiinstance.ts` forwards bulks
/// from other instances of the same collection).
#[derive(Clone, Debug, PartialEq)]
pub struct EventBulk {
    /// Unique per bulk. Stable within a process; not a persistent sequence number.
    pub id: String,
    /// The events, in write-row order.
    pub events: Vec<ChangeEvent>,
    /// Where to resume after this bulk. Present once the bulk has at least one
    /// event; `None` for an empty bulk (which storages must not emit at all,
    /// per the "no empty eventBulk when the write had only errors" test).
    pub checkpoint: Option<Checkpoint>,
    /// The `context` string the caller passed to `bulk_write`.
    pub context: String,
}

impl EventBulk {
    /// True when this bulk carries no events, i.e. it must not be emitted.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// The change feed of one storage instance.
///
/// A pinned boxed stream of `EventBulk`s. Pinned and boxed because the trait
/// method that produces it is called on a `dyn`-dispatched storage (the wrapper
/// stack), and because a storage whose feed is a channel receiver and one whose
/// feed fans out to several subscribers would otherwise need the same enum.
pub type ChangeStream = Pin<Box<dyn Stream<Item = EventBulk>>>;
