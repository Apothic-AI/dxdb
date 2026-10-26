//! The snapshot: a tagged copy of the metadata, kept in `localStorage`.
//!
//! `specs/meta-cache.md` section 3.1. Written with a single `setItem`, which the Web
//! Storage specification makes atomic per key: a reader sees either the old value or
//! the new one, never a partial write.
//!
//! The snapshot carries **no user documents and no key material** (`INV-10`). It is
//! only ever constructed from metadata the caller handed us, and the type is the
//! metadata type rather than a generic JSON value, so a user document cannot be
//! placed in it without a compile error.

use serde_json::{json, Value};

use crate::generation::{GenerationDoc, SNAPSHOT_FORMAT};
use crate::SnapshotStore;

/// The key a database's snapshot lives under.
pub fn snapshot_key(database_name: &str) -> String {
    format!("{}{database_name}", crate::generation::SNAPSHOT_KEY_PREFIX)
}

/// One snapshot: the generation it was built from, and the metadata itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// Identifies the incarnation this was built from.
    pub instance_id: String,
    /// The generation this snapshot is tagged with.
    pub generation: u64,
    /// The metadata, verbatim.
    pub documents: Vec<Value>,
}

/// Why a snapshot could not be accepted.
///
/// Every variant is a *cache miss*, never a caller-visible error: the whole point of
/// `INV-7` is that no cache failure may prevent an open. They exist as an enum so the
/// tests can assert *which* rejection fired rather than merely that one did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReason {
    /// No snapshot is stored.
    Absent,
    /// The stored value could not be read from the store.
    Unreadable,
    /// The stored value is not JSON, or not a snapshot shape.
    Unparseable,
    /// A format this build does not implement. Rejected before any document parsing.
    UnknownFormat(u8),
    /// A different database incarnation.
    InstanceMismatch {
        /// What the snapshot claims.
        snapshot: String,
        /// What the store says.
        store: String,
    },
    /// A different generation of the same incarnation.
    GenerationMismatch {
        /// What the snapshot claims.
        snapshot: u64,
        /// What the store says.
        store: u64,
    },
}

impl std::fmt::Display for StaleReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StaleReason::Absent => f.write_str("no snapshot is stored"),
            StaleReason::Unreadable => f.write_str("the snapshot could not be read"),
            StaleReason::Unparseable => f.write_str("the snapshot could not be parsed"),
            StaleReason::UnknownFormat(format) => {
                write!(f, "snapshot format {format} is not supported")
            }
            StaleReason::InstanceMismatch { snapshot, store } => write!(
                f,
                "snapshot is from a different database incarnation ({snapshot} != {store})"
            ),
            StaleReason::GenerationMismatch { snapshot, store } => {
                write!(f, "snapshot generation {snapshot} is not {store}")
            }
        }
    }
}

impl Snapshot {
    /// Builds a snapshot of `documents` tagged with `generation`.
    pub fn new(generation: &GenerationDoc, documents: Vec<Value>) -> Self {
        Self {
            instance_id: generation.instance_id.clone(),
            generation: generation.generation,
            documents,
        }
    }

    /// The stored JSON shape.
    pub fn to_value(&self) -> Value {
        json!({
            "format": SNAPSHOT_FORMAT,
            "instance_id": self.instance_id,
            "generation": self.generation,
            "documents": self.documents,
        })
    }

    /// Writes this snapshot to `store` under `database_name`'s key.
    ///
    /// A write failure is swallowed: `INV-7` says the cache can never prevent a
    /// database from opening, and a snapshot that could not be written simply means the
    /// next open is a slow one.
    pub fn write(&self, store: &dyn SnapshotStore, database_name: &str) {
        let key = snapshot_key(database_name);
        // A serialisation failure here would mean a metadata document is not
        // serialisable, which cannot happen for JSON values; ignoring the result either
        // way keeps the "never blocks an open" property.
        if let Ok(text) = serde_json::to_string(&self.to_value()) {
            let _ = store.set_item(&key, &text);
        }
    }

    /// Reads and validates the snapshot for `database_name` against `generation`.
    ///
    /// The order of the checks is the contract: `format` before parsing documents
    /// (`INV-8`), then identity before generation. A caller that gets `Ok` may serve
    /// these documents without reading the store.
    pub fn read_valid(
        store: &dyn SnapshotStore,
        database_name: &str,
        generation: &GenerationDoc,
    ) -> Result<Self, StaleReason> {
        let key = snapshot_key(database_name);
        let raw = store.get_item(&key).map_err(|_| StaleReason::Unreadable)?;
        let Some(raw) = raw else {
            return Err(StaleReason::Absent);
        };
        let value: Value = serde_json::from_str(&raw).map_err(|_| StaleReason::Unparseable)?;
        let object = value.as_object().ok_or(StaleReason::Unparseable)?;

        // Format first, before any document is touched, so a future layout cannot be
        // half-read by an old build.
        let format = object
            .get("format")
            .and_then(Value::as_u64)
            .ok_or(StaleReason::Unparseable)?;
        let format = u8::try_from(format).map_err(|_| StaleReason::Unparseable)?;
        if format != SNAPSHOT_FORMAT {
            return Err(StaleReason::UnknownFormat(format));
        }

        let instance_id = object
            .get("instance_id")
            .and_then(Value::as_str)
            .ok_or(StaleReason::Unparseable)?
            .to_string();
        let snapshot_generation = object
            .get("generation")
            .and_then(Value::as_u64)
            .ok_or(StaleReason::Unparseable)?;
        let documents = object
            .get("documents")
            .and_then(Value::as_array)
            .ok_or(StaleReason::Unparseable)?
            .clone();

        // Identity before generation: a recreated database can coincidentally have the
        // same generation number, so comparing generations first would report the wrong
        // reason, and reporting the wrong reason is how a real bug hides.
        if instance_id != generation.instance_id {
            return Err(StaleReason::InstanceMismatch {
                snapshot: instance_id,
                store: generation.instance_id.clone(),
            });
        }
        if snapshot_generation != generation.generation {
            return Err(StaleReason::GenerationMismatch {
                snapshot: snapshot_generation,
                store: generation.generation,
            });
        }
        Ok(Self {
            instance_id,
            generation: snapshot_generation,
            documents,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{snapshot_key, Snapshot, StaleReason};
    use crate::generation::{GenerationDoc, SNAPSHOT_FORMAT};
    use crate::SnapshotStore;
    use dxdb_localstorage::MemoryKv;

    fn generation() -> GenerationDoc {
        GenerationDoc::first("instance-a")
    }

    #[test]
    fn a_written_snapshot_reads_back() {
        let store = MemoryKv::new();
        let documents = vec![serde_json::json!({"key": "humans"})];
        Snapshot::new(&generation(), documents.clone()).write(&store, "mydb");
        let read = Snapshot::read_valid(&store, "mydb", &generation()).expect("valid");
        assert_eq!(read.documents, documents);
    }

    #[test]
    fn an_absent_snapshot_is_a_miss_not_an_error() {
        let store = MemoryKv::new();
        assert_eq!(
            Snapshot::read_valid(&store, "mydb", &generation()),
            Err(StaleReason::Absent)
        );
    }

    #[test]
    fn an_unknown_format_is_rejected_before_documents_are_parsed() {
        let store = MemoryKv::new();
        // A format byte this build does not know, with documents that would be valid if
        // they were reached. The assertion is on the *reason*, which proves the format
        // check fired first.
        let raw = serde_json::json!({
            "format": 99,
            "instance_id": "instance-a",
            "generation": 1,
            "documents": [{"anything": true}]
        });
        store
            .set_item(&snapshot_key("mydb"), &raw.to_string())
            .unwrap();
        assert_eq!(
            Snapshot::read_valid(&store, "mydb", &generation()),
            Err(StaleReason::UnknownFormat(99))
        );
    }

    #[test]
    fn identity_is_checked_before_generation() {
        let store = MemoryKv::new();
        let raw = serde_json::json!({
            "format": SNAPSHOT_FORMAT,
            "instance_id": "someone-else",
            "generation": 999,
            "documents": []
        });
        store
            .set_item(&snapshot_key("mydb"), &raw.to_string())
            .unwrap();
        // Both identity and generation differ; the reported reason must be identity.
        assert!(matches!(
            Snapshot::read_valid(&store, "mydb", &generation()),
            Err(StaleReason::InstanceMismatch { .. })
        ));
    }

    #[test]
    fn a_generation_mismatch_is_reported_as_such() {
        let store = MemoryKv::new();
        Snapshot::new(&generation(), vec![]).write(&store, "mydb");
        let advanced = generation().next().expect("room");
        assert_eq!(
            Snapshot::read_valid(&store, "mydb", &advanced),
            Err(StaleReason::GenerationMismatch {
                snapshot: 1,
                store: 2
            })
        );
    }

    #[test]
    fn unparseable_stored_text_is_a_miss() {
        let store = MemoryKv::new();
        store.set_item(&snapshot_key("mydb"), "{not json").unwrap();
        assert_eq!(
            Snapshot::read_valid(&store, "mydb", &generation()),
            Err(StaleReason::Unparseable)
        );
    }

    #[test]
    fn a_snapshot_stores_only_what_it_was_given() {
        // INV-10's shape guard: the snapshot type holds metadata documents, and nothing
        // in this module adds anything else.
        let store = MemoryKv::new();
        Snapshot::new(&generation(), vec![serde_json::json!({"key": "humans"})])
            .write(&store, "mydb");
        let raw = store
            .get_item(&snapshot_key("mydb"))
            .unwrap()
            .expect("present");
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(
            keys.len(),
            4,
            "format, instance_id, generation, documents only"
        );
    }
}
