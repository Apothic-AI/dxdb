//! Checkpoints: the opaque stack, and where one is persisted.
//!
//! Ports `src/replication-protocol/checkpoint.ts` (154 lines) plus
//! `stackCheckpoints` from `src/rx-storage-helper.ts:150-157`.
//!
//! # The checkpoint is opaque, and that is load-bearing
//!
//! Upstream's `CheckpointType` is a type parameter and the protocol does three
//! things with it: stack it, `JSON.stringify` it to decide whether a write is
//! needed, and hand it back to the caller's `masterChangesSince`. It never reads a
//! field. A sharding storage's checkpoint is a partial; a transport's may be
//! `{seq}` or `{cursor, region}`.
//!
//! So [`Checkpoint`] here is a newtype over ordered JSON with no accessors, and
//! [`Checkpoint::stack`] is upstream's `Object.assign` merge. `dxdb-storage`'s
//! typed `{id, lwt}` checkpoint is *one* encoding a caller may use, not the
//! protocol's constraint (port plan §13.2).
//!
//! # Values that are easy to get wrong, all of them wire behaviour
//!
//! * **Later wins.** `Object.assign({}, a, b)` — `b`'s value replaces `a`'s for a
//!   shared key.
//! * **First insertion order.** A key keeps the position it first appeared at, even
//!   when a later checkpoint overwrites its value. Re-sorting keys changes the bytes
//!   and therefore breaks checkpoint equality against a JS peer.
//! * **Falsy checkpoints are dropped**, not merged: upstream's
//!   `.filter(x => !!x)` means an `undefined` initial checkpoint contributes nothing.
//! * **Equality is `JSON.stringify`**, i.e. `dxdb_json::to_js_json` on both sides. A
//!   `serde_json` float writer would make `1.0` and `1` compare unequal and cause a
//!   write upstream would have skipped.

use std::fmt;

use dxdb_json::{Map, Value};
use dxdb_storage::{DocumentData, RxStorageInstance, StorageError};

use crate::state::ReplicationState;

/// An opaque, ordered-JSON checkpoint.
///
/// Newtype rather than a bare `Value` so that the two rules that matter — always an
/// object, always ordered — are enforced at construction and cannot be forgotten at a
/// call site.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint(Value);

impl Checkpoint {
    /// Wraps a JSON object.
    ///
    /// # Errors
    ///
    /// A checkpoint that is not an object cannot be stacked (`Object.assign` over a
    /// scalar produces `{}`, silently losing the value), so it is rejected here
    /// rather than at the first merge.
    pub fn new(value: Value) -> Result<Self, CheckpointError> {
        match value {
            Value::Object(_) => Ok(Checkpoint(value)),
            other => Err(CheckpointError::NotAnObject(json_type_name(&other))),
        }
    }

    /// Wraps a JSON object from a map.
    pub fn from_map(map: Map<String, Value>) -> Self {
        Checkpoint(Value::Object(map))
    }

    /// The empty checkpoint. Stacking it is a no-op, which is the identity element
    /// upstream relies on when a replication has never run.
    pub fn empty() -> Self {
        Checkpoint(Value::Object(Map::new()))
    }

    /// The checkpoint `dxdb_storage::Checkpoint` encodes to.
    ///
    /// This is the bridge for the storage-to-storage adapter: the storage's typed
    /// `{id, lwt}` pair is a valid protocol checkpoint, so a caller holding one can
    /// hand it to the protocol without a conversion trait.
    pub fn from_storage_checkpoint(checkpoint: &dxdb_storage::Checkpoint) -> Self {
        Checkpoint(checkpoint.to_json())
    }

    /// Decodes into the storage's typed checkpoint, when this checkpoint is one.
    ///
    /// `None` for a checkpoint a transport invented, which is not an error: the
    /// protocol never needs this, only a storage-to-storage caller does.
    pub fn as_storage_checkpoint(&self) -> Option<dxdb_storage::Checkpoint> {
        dxdb_storage::Checkpoint::from_json(&self.0)
    }

    /// The ordered JSON.
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Consumes the checkpoint, returning its ordered JSON.
    pub fn into_value(self) -> Value {
        self.0
    }

    /// The object view.
    pub fn as_map(&self) -> &Map<String, Value> {
        match &self.0 {
            Value::Object(map) => map,
            // Unreachable: `new` rejects everything else and `from_map` builds it.
            _ => unreachable!("a Checkpoint is always a JSON object"),
        }
    }

    /// True when this is the empty checkpoint.
    pub fn is_empty(&self) -> bool {
        self.as_map().is_empty()
    }

    /// Upstream `stackCheckpoints([self, next])`.
    ///
    /// `Object.assign({}, this, next)`: shallow, later wins, first-insertion key
    /// order preserved. An empty `next` returns `self` unchanged, byte for byte.
    pub fn stack(&self, next: &Checkpoint) -> Checkpoint {
        if next.is_empty() {
            return self.clone();
        }
        if self.is_empty() {
            return next.clone();
        }
        // `serde_json::Map` with `preserve_order` keeps insertion order and `insert`
        // on an existing key replaces the value without moving the key, which is
        // exactly `Object.assign`'s behaviour.
        let mut merged = Map::new();
        for (key, value) in self.as_map() {
            merged.insert(key.clone(), value.clone());
        }
        for (key, value) in next.as_map() {
            merged.insert(key.clone(), value.clone());
        }
        Checkpoint(Value::Object(merged))
    }

    /// Upstream's skip test: `JSON.stringify(a) === JSON.stringify(b)`, which is
    /// `to_js_json` equality and **not** structural equality.
    ///
    /// It is deliberately not `PartialEq`: two checkpoints whose keys are in a
    /// different order are unequal here even though they are `==` as JSON values,
    /// because upstream compares their serialisations and a differing serialisation
    /// causes a write.
    pub fn same_serialised(&self, other: &Checkpoint) -> bool {
        self.to_js_json() == other.to_js_json()
    }

    /// Serialises with JavaScript number formatting.
    pub fn to_js_json(&self) -> String {
        dxdb_json::to_js_json(&self.0)
    }
}

impl fmt::Display for Checkpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_js_json())
    }
}

/// Why a value could not be a checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    /// Upstream's checkpoints are always objects; stacking a non-object loses it.
    #[error("a checkpoint must be a JSON object, got {0}")]
    NotAnObject(&'static str),
}

/// Upstream's free function, over a slice, including the falsy filter.
///
/// `stack_checkpoints([None, Some(a), Some(b)])` is `a` stacked with `b`. This exists
/// as well as [`Checkpoint::stack`] because the protocol stacks variadic lists (all
/// open tasks' checkpoints at once) and rewriting those as a fold would hide the
/// ordering rule inside a loop.
pub fn stack_checkpoints(checkpoints: &[Option<Checkpoint>]) -> Checkpoint {
    let mut merged = Map::new();
    for checkpoint in checkpoints.iter().flatten() {
        for (key, value) in checkpoint.as_map() {
            merged.insert(key.clone(), value.clone());
        }
    }
    Checkpoint(Value::Object(merged))
}

/// The meta row id for a checkpoint: `"{direction}|1"`.
///
/// Ports `getComposedPrimaryKeyOfDocumentData`'s role for the meta schema
/// (`checkpoint.ts:22-28`, `meta-instance.ts:180-183`) with the meta schema's
/// `separator: '|'`.
pub fn checkpoint_doc_id(direction: Direction) -> String {
    format!("{}|1", direction.as_str())
}

/// The meta row id for a document's assumed-master state: `"{docId}|0"`.
pub fn meta_doc_id(document_id: &str) -> String {
    format!("{document_id}|0")
}

/// `RxStorageReplicationDirection`.
///
/// The persisted spellings are `"up"` and `"down"`, not `"UP"`/`"DOWN"`. Upstream's
/// `minItemIdLength = 4` comment mentions `'DOWN'` only as a length bound; the value
/// written into `itemId` is the lowercase literal (`replication-protocol.d.ts:298`
/// and every call site).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Fork to master.
    Up,
    /// Master to fork.
    Down,
}

impl Direction {
    /// The persisted spelling, which is also the checkpoint's `itemId`.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
        }
    }

    /// Both directions, for code that must set up both.
    pub fn all() -> [Direction; 2] {
        [Direction::Up, Direction::Down]
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The key every meta row's `_rev` is built from, and the tag written into fork
/// documents' `_meta`.
///
/// Ports `getCheckpointKey` (`checkpoint.ts:145-154`): a SHA-256 hex of
/// `identifier || databaseName || collectionName` prefixed with
/// `rx_storage_replication_`.
///
/// Upstream computes this asynchronously because `crypto.subtle` is async; here it is
/// synchronous, so the state holds a `String` rather than a future and every use site
/// is a plain borrow.
///
/// # The hash is not optional
///
/// The default hash function is SHA-256 (`utils-hash.ts:52`), lowercase hex, no
/// prefix, no truncation. It is part of a persisted name, so a different hash function
/// means a database a JS peer cannot read — the caller may supply its own, exactly as
/// upstream lets `createRxDatabase` take one, but the *default* has to match.
pub fn checkpoint_key(
    hash_function: &dyn HashFunction,
    identifier: &str,
    database_name: &str,
    collection_name: &str,
) -> String {
    // Upstream joins with the two-character `||`, not `-` and not `/`. The meta
    // collection name uses `-` instead (`plugins/replication/index.ts:51-54`), so the
    // two are distinguishable only by reading both call sites.
    let joined = format!("{identifier}||{database_name}||{collection_name}");
    format!("rx_storage_replication_{}", hash_function.hash(&joined))
}

/// The database's hash function, as the replication needs it.
///
/// Upstream's `HashFunction` takes `string | ArrayBuffer | Blob` and returns hex.
/// The protocol only ever hashes strings, so this is the narrow form; an attachment
/// digest is the caller's business (`dxdb-attachments`).
pub trait HashFunction {
    /// Hashes a UTF-8 string, returning lowercase hex.
    fn hash(&self, input: &str) -> String;
}

/// SHA-256, lowercase hex — upstream's `defaultHashSha256`
/// (`utils-hash.ts:32-52`).
///
/// Default for [`crate::state::ReplicationState`] so that a caller who does not care
/// gets the hash a JS peer also gets.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sha256HashFunction;

impl HashFunction for Sha256HashFunction {
    fn hash(&self, input: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Reading and writing the checkpoint row
// ---------------------------------------------------------------------------

/// Reads the checkpoint for `direction` from the meta instance, caching the whole row
/// in state.
///
/// Ports `getLastCheckpointDoc` (`checkpoint.ts:18-43`). Two details:
///
/// * The row is fetched with `with_deleted: false`, so a tombstoned checkpoint row
///   reads as "no checkpoint" — which is what upstream's `checkpointResult[0]` does.
/// * The cache is written even when nothing was found, so `last_checkpoint_doc`
///   means "what the meta instance held at the last read" rather than "the last
///   non-empty checkpoint".
pub async fn last_checkpoint<S>(
    state: &mut ReplicationState<S>,
    direction: Direction,
) -> Result<Option<Checkpoint>, StorageError>
where
    S: RxStorageInstance,
{
    let doc_id = checkpoint_doc_id(direction);
    let found = state
        .meta_instance()
        .find_documents_by_id(vec![doc_id], false)
        .await?;
    let row = found.into_iter().next();
    let checkpoint = row
        .as_ref()
        .and_then(|doc| doc.get("checkpointData").cloned())
        .map(Checkpoint::new)
        .transpose()
        // A malformed stored checkpoint is a corrupt meta row, not a caller error.
        // Upstream would hand the value back and fail later; failing here names the
        // row.
        .map_err(|_| {
            StorageError::InvalidDocument(format!(
                "checkpoint row for {direction} has a non-object checkpointData"
            ))
        })?;
    state.set_last_checkpoint_doc(direction, row);
    Ok(checkpoint)
}

/// Persists a checkpoint, stacking it onto the previous one and resolving 409s.
///
/// Ports `setCheckpoint` (`checkpoint.ts:50-143`). The steps, and why each is not
/// optional:
///
/// 1. **Skip when unchanged.** Compares serialisations (see
///    [`Checkpoint::same_serialised`]), so an idempotent pull does not write.
/// 2. **Skip when cancelled.** "If the replication is already canceled, we do not
///    write a checkpoint because that could mean we write a checkpoint for data that
///    has been fetched from the master but not been written to the child."
/// 3. **Stack with the previous.** Required for a storage whose change events carry a
///    partial checkpoint.
/// 4. **Fresh `_meta.lwt` and `_rev` on every attempt**, including the retry: a 409
///    means someone else advanced the row, so reusing the revision would 409 again.
/// 5. **On 409, adopt `documentInDb` as `previous` and retry.** Any other error is
///    returned: it is not a race, it is a bug.
///
/// The loop is bounded by cancellation only, as upstream's is.
pub async fn set_checkpoint<S>(
    state: &mut ReplicationState<S>,
    direction: Direction,
    checkpoint: Option<&Checkpoint>,
) -> Result<(), StorageError>
where
    S: RxStorageInstance,
{
    let Some(checkpoint) = checkpoint else {
        return Ok(());
    };
    let mut previous = state.last_checkpoint_doc(direction);

    // Upstream: `checkpoint && !canceled && (no previous || serialisation differs)`.
    if state.is_cancelled() {
        return Ok(());
    }
    if let Some(previous_row) = &previous {
        let previous_checkpoint = previous_row
            .get("checkpointData")
            .cloned()
            .map(Checkpoint::new)
            .transpose()
            .ok()
            .flatten();
        if let Some(previous_checkpoint) = previous_checkpoint {
            if previous_checkpoint.same_serialised(checkpoint) {
                return Ok(());
            }
        }
    }

    let checkpoint_key = state.checkpoint_key().to_string();
    let mut stacked = checkpoint.clone();

    loop {
        if state.is_cancelled() {
            return Ok(());
        }
        if let Some(previous_row) = &previous {
            if let Some(previous_checkpoint) = previous_row
                .get("checkpointData")
                .cloned()
                .map(Checkpoint::new)
                .transpose()
                .ok()
                .flatten()
            {
                stacked = previous_checkpoint.stack(&stacked);
            }
        }

        let mut row = build_checkpoint_row(direction, &stacked, state.lwt());
        row.set(
            "_rev",
            Value::String(dxdb_storage::create_revision(
                &checkpoint_key,
                previous.as_ref().and_then(DocumentData::rev),
            )),
        );

        let write_row = match &previous {
            Some(previous_row) => dxdb_storage::update_row(previous_row.clone(), row.clone()),
            None => dxdb_storage::insert_row(row.clone()),
        };

        let response = state
            .meta_instance()
            .bulk_write(vec![write_row], "replication-set-checkpoint".to_string())
            .await?;

        if response.is_ok() {
            state.set_last_checkpoint_doc(direction, Some(row));
            return Ok(());
        }

        match response.error.into_iter().next() {
            Some(dxdb_storage::WriteError::Conflict { document_in_db, .. }) => {
                // Upstream: `previousCheckpointDoc = ensureNotFalsy(error.documentInDb)`
                // then recompute the revision. An insert-conflict carries no in-DB
                // document, which upstream's `ensureNotFalsy` would throw on; surface
                // it as the malformed case it is rather than looping forever.
                let Some(in_db) = document_in_db else {
                    return Err(StorageError::InvalidDocument(format!(
                        "checkpoint write for {direction} conflicted against an insert \
                         with no document in the database"
                    )));
                };
                previous = Some(in_db);
            }
            Some(other) => {
                return Err(StorageError::Unsupported(format!(
                    "writing the {direction} checkpoint failed with status {}",
                    other.status()
                )));
            }
            None => {
                return Err(StorageError::Unsupported(format!(
                    "writing the {direction} checkpoint reported no error and no success"
                )));
            }
        }
    }
}

/// The row upstream builds in `setCheckpoint` (`checkpoint.ts:76-89`).
///
/// Field set and order are the stored shape: `id`, `isCheckpoint`, `itemId`,
/// `_deleted`, `_attachments`, `checkpointData`, `_meta`, `_rev`. A reader diffing a
/// row written here against one written by a JS peer compares exactly these.
fn build_checkpoint_row(direction: Direction, checkpoint: &Checkpoint, lwt: f64) -> DocumentData {
    let mut map = Map::new();
    map.insert(
        "id".to_string(),
        Value::String(checkpoint_doc_id(direction)),
    );
    map.insert("isCheckpoint".to_string(), Value::String("1".to_string()));
    map.insert(
        "itemId".to_string(),
        Value::String(direction.as_str().to_string()),
    );
    map.insert("_deleted".to_string(), Value::Bool(false));
    map.insert("_attachments".to_string(), Value::Object(Map::new()));
    map.insert("checkpointData".to_string(), checkpoint.as_value().clone());
    map.insert("_meta".to_string(), meta_with_lwt(lwt));
    // Replaced by the caller, which has the previous revision.
    map.insert("_rev".to_string(), Value::String(String::new()));
    DocumentData::new(map)
}

/// `{"lwt": ...}`, the `_meta` a meta row carries.
fn meta_with_lwt(lwt: f64) -> Value {
    let mut meta = Map::new();
    if let Some(number) = crate::convert::json_number(lwt) {
        meta.insert("lwt".to_string(), number);
    }
    Value::Object(meta)
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The write row for a meta document, used by the meta module as well.
pub(crate) fn meta_write_row(
    previous: Option<DocumentData>,
    document: DocumentData,
) -> dxdb_storage::BulkWriteRow {
    match previous {
        Some(previous) => dxdb_storage::update_row(previous, document),
        None => dxdb_storage::insert_row(document),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(json: Value) -> Checkpoint {
        Checkpoint::new(json).expect("test checkpoints are objects")
    }

    #[test]
    fn stack_later_wins_and_keeps_first_insertion_order() {
        let a = checkpoint(dxdb_json::parse(r#"{"seq":1,"region":"eu"}"#).unwrap());
        let b = checkpoint(dxdb_json::parse(r#"{"region":"us","cursor":"c1"}"#).unwrap());
        let merged = a.stack(&b);
        assert_eq!(
            merged.to_js_json(),
            r#"{"seq":1,"region":"us","cursor":"c1"}"#,
            "region keeps its first position and takes the later value"
        );
    }

    #[test]
    fn stacking_an_empty_checkpoint_changes_nothing() {
        let a = checkpoint(dxdb_json::parse(r#"{"a":1}"#).unwrap());
        assert_eq!(a.stack(&Checkpoint::empty()).to_js_json(), r#"{"a":1}"#);
        assert_eq!(Checkpoint::empty().stack(&a).to_js_json(), r#"{"a":1}"#);
    }

    #[test]
    fn free_function_drops_absent_checkpoints() {
        let a = checkpoint(dxdb_json::parse(r#"{"a":1}"#).unwrap());
        let b = checkpoint(dxdb_json::parse(r#"{"b":2}"#).unwrap());
        let merged = stack_checkpoints(&[None, Some(a), None, Some(b)]);
        assert_eq!(merged.to_js_json(), r#"{"a":1,"b":2}"#);
        assert_eq!(
            stack_checkpoints(&[None, None]).to_js_json(),
            "{}",
            "all-absent stacks to the empty checkpoint, not an error"
        );
    }

    #[test]
    fn equality_is_serialisation_not_structure() {
        let a = checkpoint(dxdb_json::parse(r#"{"a":1,"b":2}"#).unwrap());
        let b = checkpoint(dxdb_json::parse(r#"{"b":2,"a":1}"#).unwrap());
        assert_eq!(a, b, "as JSON values they are equal");
        assert!(
            !a.same_serialised(&b),
            "but upstream compares JSON.stringify, and these serialise differently"
        );
    }

    #[test]
    fn js_number_formatting_reaches_the_bytes() {
        let a = checkpoint(dxdb_json::parse(r#"{"lwt":1.0}"#).unwrap());
        assert_eq!(
            a.to_js_json(),
            r#"{"lwt":1}"#,
            "a checkpoint must serialise like JSON.stringify, or a JS peer compares it as changed"
        );
    }

    #[test]
    fn a_non_object_is_rejected() {
        assert!(Checkpoint::new(Value::from(1)).is_err());
        assert!(Checkpoint::new(Value::Array(Vec::new())).is_err());
    }

    #[test]
    fn storage_checkpoint_round_trips() {
        let storage = dxdb_storage::Checkpoint::new("abc", 1.5);
        let protocol = Checkpoint::from_storage_checkpoint(&storage);
        assert_eq!(protocol.to_js_json(), r#"{"id":"abc","lwt":1.5}"#);
        assert_eq!(protocol.as_storage_checkpoint(), Some(storage));
    }

    #[test]
    fn checkpoint_key_uses_the_double_pipe_join_and_sha256() {
        let key = checkpoint_key(&Sha256HashFunction, "rxdbreplicationx", "db", "human");
        assert!(key.starts_with("rx_storage_replication_"));
        let hex = &key["rx_storage_replication_".len()..];
        assert_eq!(hex.len(), 64, "untruncated SHA-256 hex");
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

        // The join is `||`; the meta collection name joins with `-`, so this pins the
        // one that the *checkpoint key* uses.
        let expected_hash = Sha256HashFunction.hash("rxdbreplicationx||db||human");
        assert_eq!(hex, expected_hash);
    }

    #[test]
    fn directions_are_lowercase_and_ids_are_composed_with_pipe() {
        assert_eq!(Direction::Up.as_str(), "up");
        assert_eq!(Direction::Down.as_str(), "down");
        assert_eq!(checkpoint_doc_id(Direction::Down), "down|1");
        assert_eq!(meta_doc_id("passport-1"), "passport-1|0");
    }
}
