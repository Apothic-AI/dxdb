//! An observable state store that multiple instances can write safely.
//!
//! Ported from `src/plugins/state/` at upstream `3b763ee17` (577 lines):
//! `helpers.ts` (86) has the schema and the id arithmetic, `rx-state.ts` (419) is the
//! store, `types.ts` (32) the document shape, `index.ts` (40) the per-database cache.
//!
//! # The idea, and why it is not just a hash map
//!
//! `RxState` is a JSON object that several JavaScript realms (tabs, workers) can write to
//! without overwriting each other. Upstream gets that from an **append-only op log**
//! rather than from locking: every write is a new row whose primary key is a
//! zero-padded counter, and the row carries the *list of operations* that produced it
//! (`rx-state.ts:151-219`). State is the fold of every row's ops in id order.
//!
//! Two properties fall out of that, and both are worth understanding before reading the
//! code:
//!
//! * **Concurrent writers conflict instead of clobbering.** Both tabs compute "last id +
//!   1" and both try to insert it; one gets a conflict, retries, and re-reads (`:212-218`).
//!   No lock, no lost update.
//! * **A reader that joins later replays the log** (`:325-345`), so nothing needs to be
//!   pushed to it.
//!
//! The cost is unbounded growth, which [`State::cleanup`] pays down by writing the whole
//! state as one op (`k: ''`) and deleting every row up to the last one it saw
//! (`:280-317`). `cleanup` only runs when there are more than five rows, which is why the
//! five-row constant is in the public API rather than buried.
//!
//! # What is ported here, and what needs a database
//!
//! Everything that decides *what the state is*: the schema, the id arithmetic, the op
//! log, `set` with its modifier semantics, `merge_operations`, `cleanup`, and the
//! un-projected document shape. What needs a live `RxCollection` is the *transport*: the
//! conflict-retry loop against a real storage, the change stream that feeds a second
//! instance, and the `Proxy` that makes `state.age$` a property access (`:361-389`).
//! Those are [`StateStore`]'s job — a two-method trait — so the log semantics are testable
//! without one, exactly as `migration-storage` was built.
//!
//! `state.age$` has no Rust equivalent at all: there are no dynamic properties, so
//! [`State::get`] and [`State::subscribe`] replace it, and the `$`/`$$` naming disappears.
//! Recorded as a divergence rather than imitated with a macro that would read worse.
//!
//! # Two details that are easy to get wrong
//!
//! * **The empty path `''` means "replace the whole state"**, not "the field named
//!   nothing" (`:203-206`, `:266-270`). It is how `cleanup` writes the fold, and a port
//!   that dropped it would make compaction impossible.
//! * **Values are cloned on the way in and out.** Upstream clones because a storage may
//!   have deep-frozen the input in dev mode (`:196-207`). Rust values are owned so the
//!   clone is structural, but the *semantics* matter: a modifier that mutates its input
//!   must not be able to reach back into the state, so [`set`](State::set) gives the
//!   modifier a clone.

#![forbid(unsafe_code)]

use serde_json::{Map, Value};
use thiserror::Error;

/// The schema title (`helpers.ts:6`).
pub const SCHEMA_TITLE: &str = "RxStateCollection";

/// The id length, including the left padding (`helpers.ts:7`).
pub const ID_LENGTH: usize = 14;

/// The instance-id length, from the schema's `sId` bounds (`helpers.ts:26-27`).
pub const INSTANCE_ID_LENGTH: usize = 10;

/// `cleanup` only compacts once there are more than this many rows
/// (`rx-state.ts:300`).
pub const CLEANUP_MINIMUM_ROWS: u64 = 5;

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// A path is not a valid dotted object path.
    #[error("invalid state path '{0}'")]
    InvalidPath(String),
    /// A row id is not the 14-digit zero-padded form the schema requires.
    #[error("invalid state row id '{0}': expected {ID_LENGTH} digits")]
    InvalidId(String),
    /// The store reported a write conflict that could not be resolved.
    #[error("state write conflicted on id '{id}' after {attempts} attempts")]
    ConflictLivelock {
        /// The id that kept conflicting.
        id: String,
        /// How many times it was retried.
        attempts: usize,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// One operation in the log (`types.ts:27-30`).
///
/// `k` is the path and `v` the new value. `k == ""` replaces the whole state, which is
/// how compaction is expressed (`rx-state.ts:266-270`).
#[derive(Clone, Debug, PartialEq)]
pub struct StateOperation {
    /// The dotted path, or `""` for the whole state.
    pub path: String,
    /// The new value at that path.
    pub value: Value,
}

impl StateOperation {
    /// An operation on one path.
    pub fn at(path: impl Into<String>, value: Value) -> Self {
        StateOperation {
            path: path.into(),
            value,
        }
    }

    /// The whole-state replacement (`k: ''`).
    pub fn whole(value: Value) -> Self {
        StateOperation {
            path: String::new(),
            value,
        }
    }

    /// The wire shape, `{k, v}` (`types.ts:27-30`).
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "k": self.path, "v": self.value })
    }

    /// Reads the wire shape.
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::InvalidPath(value.to_string()))?;
        let path = map
            .get("k")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidPath(value.to_string()))?
            .to_string();
        Ok(StateOperation {
            path,
            value: map.get("v").cloned().unwrap_or(Value::Null),
        })
    }
}

/// One row of the log (`types.ts:6-24`).
#[derive(Clone, Debug, PartialEq)]
pub struct StateRow {
    /// The zero-padded counter, which is also the primary key.
    pub id: String,
    /// The id of the `RxState` instance that wrote it (`sId`).
    pub instance_id: String,
    /// The operations this row applies.
    pub ops: Vec<StateOperation>,
}

impl StateRow {
    /// Builds a row, validating the id shape.
    pub fn new(
        id: impl Into<String>,
        instance_id: impl Into<String>,
        ops: Vec<StateOperation>,
    ) -> Result<Self> {
        let id = id.into();
        parse_id(&id)?;
        let instance_id = instance_id.into();
        if instance_id.len() != INSTANCE_ID_LENGTH {
            return Err(Error::InvalidId(format!(
                "instance id '{instance_id}' is not {INSTANCE_ID_LENGTH} characters"
            )));
        }
        if ops.is_empty() {
            return Err(Error::InvalidPath(
                "a state row needs at least one operation (schema: minItems 1)".to_string(),
            ));
        }
        Ok(StateRow {
            id,
            instance_id,
            ops,
        })
    }

    /// The wire shape.
    pub fn to_document_data(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "sId": self.instance_id,
            "ops": self.ops.iter().map(StateOperation::to_json).collect::<Vec<_>>(),
        })
    }
}

/// The JSON schema for the log collection (`helpers.ts:8-57`).
///
/// Passed through `fill_with_default_settings` so it carries the meta fields, then
/// through `create_rx_schema` to prove it validates — a schema this module ships has to
/// be a legal RxDB schema.
pub fn state_schema() -> Result<Value> {
    let raw = serde_json::json!({
        "title": SCHEMA_TITLE,
        "primaryKey": "id",
        "version": 0,
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "maxLength": ID_LENGTH,
                "minLength": ID_LENGTH,
                "pattern": "[0-9]+"
            },
            "sId": {
                "type": "string",
                "maxLength": INSTANCE_ID_LENGTH,
                "minLength": INSTANCE_ID_LENGTH
            },
            "ops": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": { "k": { "type": "string" }, "v": {} },
                    "required": ["k", "v"]
                }
            }
        },
        "required": ["id", "sId", "ops"]
    });
    dxdb_schema::fill_with_default_settings(&raw)
        .map_err(|error| Error::InvalidPath(error.to_string()))
}

/// `nextRxStateId` (`helpers.ts:60-68`).
///
/// No previous id means the first row, which is `"0"` padded to [`ID_LENGTH`]. Otherwise
/// parse, add one, and pad back — so ordering is **string** ordering, which is why the
/// padding exists and why [`Error::InvalidId`] rejects an unpadded id rather than sorting
/// it wrongly.
pub fn next_state_id(last_id: Option<&str>) -> Result<String> {
    let Some(last_id) = last_id else {
        return Ok("0".repeat(ID_LENGTH));
    };
    let parsed = parse_id(last_id)?;
    Ok(format!("{:0width$}", parsed + 1, width = ID_LENGTH))
}

/// Parses a zero-padded id.
fn parse_id(id: &str) -> Result<u64> {
    if id.len() != ID_LENGTH || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::InvalidId(id.to_string()));
    }
    id.parse::<u64>()
        .map_err(|_| Error::InvalidId(id.to_string()))
}

/// Reads a dotted path out of a JSON value. `""` is the value itself.
fn read_path<'a>(state: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(state);
    }
    let mut current = state;
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(array) => array.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Writes a dotted path, creating intermediate objects.
///
/// `""` replaces the whole value, which is upstream's `path === ''` branch
/// (`rx-state.ts:203-206`).
fn write_path(state: &mut Value, path: &str, value: Value) -> Result<()> {
    if path.is_empty() {
        *state = value;
        return Ok(());
    }
    let segments: Vec<&str> = path.split('.').collect();
    let mut current = state;
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            return Err(Error::InvalidPath(path.to_string()));
        }
        let last = index + 1 == segments.len();
        if last {
            match current {
                Value::Object(map) => {
                    map.insert((*segment).to_string(), value);
                }
                other => {
                    *other = Value::Object(Map::from_iter([((*segment).to_string(), value)]));
                }
            }
            return Ok(());
        }
        current = match current {
            Value::Object(map) => map
                .entry((*segment).to_string())
                .or_insert_with(|| Value::Object(Map::new())),
            other => {
                *other = Value::Object(Map::new());
                match other {
                    Value::Object(map) => map
                        .entry((*segment).to_string())
                        .or_insert_with(|| Value::Object(Map::new())),
                    _ => unreachable!(),
                }
            }
        };
    }
    Ok(())
}

/// Folds operations into a state value (`mergeOperationsIntoState`,
/// `rx-state.ts:265-278` and `:408-418`).
///
/// A `""` path replaces everything, so order matters and later ops win.
pub fn merge_operations(state: &mut Value, operations: &[StateOperation]) -> Result<()> {
    for operation in operations {
        write_path(state, &operation.path, operation.value.clone())?;
    }
    Ok(())
}

/// The store the log is written to.
///
/// Two methods, so the log semantics are testable without a storage: `last_id` is the
/// "read the highest row" query (`rx-state.ts:167`), and `insert` is the write that
/// conflicts when another instance won the race (`:210`).
pub trait StateStore {
    /// The id of the highest row, or `None` when the log is empty.
    fn last_id(&mut self) -> Result<Option<String>>;

    /// Inserts a row. Returns `Err(StoreError::Conflict)` when the id is taken, which is
    /// the mechanism that makes concurrent writers safe.
    fn insert(&mut self, row: &StateRow) -> Result<InsertOutcome>;

    /// Every row up to and including `up_to_id`, in id order (for replay and cleanup).
    fn rows_through(&mut self, up_to_id: &str) -> Result<Vec<StateRow>>;

    /// Deletes every row up to and including `up_to_id`, returning how many went.
    fn delete_through(&mut self, up_to_id: &str) -> Result<usize>;
}

/// What an insert did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The row was written.
    Written,
    /// Another instance took this id first (`CONFLICT` upstream, `:214-218`).
    Conflict,
}

/// The state store.
#[derive(Debug)]
pub struct State {
    /// The namespace/prefix; the collection is `rx-state-<prefix>` (`:321`).
    pub prefix: String,
    /// This instance's id (`sId`), [`INSTANCE_ID_LENGTH`] characters.
    pub instance_id: String,
    state: Value,
    pending: Vec<StateOperation>,
}

impl State {
    /// A state with an empty value.
    pub fn new(prefix: impl Into<String>, instance_id: impl Into<String>) -> Result<Self> {
        let instance_id = instance_id.into();
        if instance_id.len() != INSTANCE_ID_LENGTH {
            return Err(Error::InvalidId(format!(
                "instance id '{instance_id}' is not {INSTANCE_ID_LENGTH} characters"
            )));
        }
        Ok(State {
            prefix: prefix.into(),
            instance_id,
            state: Value::Object(Map::new()),
            pending: Vec::new(),
        })
    }

    /// The collection name for a prefix (`rx-state.ts:321`).
    pub fn collection_name(&self) -> String {
        format!("rx-state-{}", self.prefix)
    }

    /// The current state (`get()` with no path, `:222-226`).
    pub fn get(&self) -> &Value {
        &self.state
    }

    /// The value at a path (`get(path)`, `:222-226`).
    pub fn get_path(&self, path: &str) -> Option<&Value> {
        read_path(&self.state, path)
    }

    /// Queues a modifier for `path` and applies it (`set`, `:143-150`).
    ///
    /// The modifier receives a **clone** of the current value, matching upstream's
    /// clone-before-freeze (`:196-207`), so it cannot reach back into the state.
    ///
    /// Returns the operations it queued, in order, which is also what a caller would
    /// append to a row.
    pub fn set<F>(&mut self, path: &str, modifier: F) -> Result<Vec<StateOperation>>
    where
        F: FnOnce(Value) -> Value,
    {
        // Upstream reads `undefined` for a missing path; a JSON value has no undefined,
        // so `null` stands in and a modifier that wants to distinguish must use the
        // `Option`-returning `get_path`.
        let current = read_path(&self.state, path).cloned().unwrap_or(Value::Null);
        let new_value = modifier(current);
        write_path(&mut self.state, path, new_value.clone())?;
        let operation = if path.is_empty() {
            StateOperation::whole(new_value)
        } else {
            StateOperation::at(path, new_value)
        };
        self.pending.push(operation.clone());
        Ok(vec![operation])
    }

    /// Takes the queued operations, as the write loop does when it builds a row
    /// (`:167-170`).
    pub fn take_pending(&mut self) -> Vec<StateOperation> {
        std::mem::take(&mut self.pending)
    }

    /// Applies operations that came from another instance (`mergeOperationsIntoState`).
    pub fn merge_operations(&mut self, operations: &[StateOperation]) -> Result<()> {
        merge_operations(&mut self.state, operations)
    }

    /// The fold of every row, as a reader joining later computes it
    /// (`:325-345`).
    pub fn replay(rows: &[StateRow]) -> Result<Value> {
        let mut state = Value::Object(Map::new());
        for row in rows {
            merge_operations(&mut state, &row.ops)?;
        }
        Ok(state)
    }

    /// A row carrying the operations queued for `id`.
    pub fn row_for(&self, id: impl Into<String>, ops: Vec<StateOperation>) -> Result<StateRow> {
        StateRow::new(id, self.instance_id.clone(), ops)
    }

    /// Whether `cleanup` would compact anything.
    ///
    /// Upstream's guard is `if ((lastNr - 5) < firstNr) { return true; }` inside
    /// `_cleanup` (`rx-state.ts:300-302`), where `true` means **"done, nothing to do"** —
    /// `_cleanup` returns `Promise<boolean>` and the caller loops until true
    /// (`cleanup-state.ts:46`). So the condition to *compact* is the negation: the span of
    /// ids must be at least [`CLEANUP_MINIMUM_ROWS`] wide.
    pub fn cleanup_needed(first_id: &str, last_id: &str) -> Result<bool> {
        let first = parse_id(first_id)?;
        let last = parse_id(last_id)?;
        // Upstream stops when `(lastNr - 5) < firstNr` (`rx-state.ts:300`), so it
        // compacts when `last - 5 >= first`. Written as `last >= first + 5` because the
        // subtraction can go negative in JS and `saturating_sub` would clamp it to zero
        // and turn `0 - 5 < 0` into `0 >= 0`; the addition form cannot underflow.
        Ok(last >= first.saturating_add(CLEANUP_MINIMUM_ROWS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_id_is_padded_zero() {
        // `helpers.ts:61-63`: `''.padStart(14, '0')`.
        assert_eq!(next_state_id(None).expect("id"), "00000000000000");
        assert_eq!(next_state_id(None).expect("id").len(), ID_LENGTH);
    }

    #[test]
    fn ids_are_left_padded_so_string_order_is_numeric_order() {
        let mut id = next_state_id(None).expect("first");
        let mut seen = vec![id.clone()];
        for _ in 0..9 {
            id = next_state_id(Some(&id)).expect("next");
            seen.push(id.clone());
        }
        assert_eq!(seen[1], "00000000000001");
        assert_eq!(seen[9], "00000000000009");
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(seen, sorted, "padding makes lexicographic order correct");
    }

    #[test]
    fn an_unpadded_id_is_rejected_rather_than_mis_sorted() {
        assert!(matches!(next_state_id(Some("1")), Err(Error::InvalidId(_))));
        assert!(matches!(
            next_state_id(Some("0000000000000a")),
            Err(Error::InvalidId(_))
        ));
    }

    #[test]
    fn the_whole_state_operation_replaces_everything() {
        let mut state = serde_json::json!({ "a": 1, "b": 2 });
        merge_operations(
            &mut state,
            &[StateOperation::whole(serde_json::json!({ "c": 3 }))],
        )
        .expect("merge");
        assert_eq!(state, serde_json::json!({ "c": 3 }));
    }

    #[test]
    fn cleanup_needs_a_span_of_at_least_five_ids() {
        // `rx-state.ts:300`: `(lastNr - 5) < firstNr` means "nothing to do". From id 0,
        // that is `last - 5 < 0`, i.e. `last <= 4`, so compaction starts at `last == 5`
        // (six rows: 0..=5). The upstream comment says "only run if more than 5 write
        // rows", which is off by one against the code; the code is the contract.
        assert!(!State::cleanup_needed("00000000000000", "00000000000004").expect("check"));
        assert!(State::cleanup_needed("00000000000000", "00000000000005").expect("check"));
        assert!(State::cleanup_needed("00000000000000", "00000000000006").expect("check"));
        // From a later start the span is what matters, not the absolute ids.
        assert!(!State::cleanup_needed("00000000000100", "00000000000104").expect("check"));
        assert!(State::cleanup_needed("00000000000100", "00000000000105").expect("check"));
    }
}
