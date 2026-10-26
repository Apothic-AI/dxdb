//! Ported from `test/unit/rx-state.test.ts` (782 lines) at upstream `3b763ee17`.
//!
//! Each test names the upstream `it(...)` and its line. The cases that are about the op
//! log are ported; the ones that need a live collection, a change stream, or the JS
//! `Proxy` are listed in the crate docs as not ported, with the reason.
//!
//! The two that carry the most weight are
//! [`upstream_85_ids_increment_in_sort_order`] (the padding is the whole reason ids are
//! strings) and [`upstream_150_many_writes_coalesce_into_one_row`] (which is what makes
//! the op log usable rather than one row per `set`).

use dxdb_plugin_state::{
    merge_operations, next_state_id, state_schema, Error, InsertOutcome, State, StateOperation,
    StateRow, StateStore, CLEANUP_MINIMUM_ROWS, ID_LENGTH, INSTANCE_ID_LENGTH, SCHEMA_TITLE,
};
use serde_json::{json, Value};

const INSTANCE: &str = "abcdefghij";

fn state() -> State {
    State::new("", INSTANCE).expect("state")
}

/// An in-memory log that reports conflicts, which is what a storage does.
#[derive(Default)]
struct Log {
    rows: Vec<StateRow>,
}

impl StateStore for Log {
    fn last_id(&mut self) -> Result<Option<String>, Error> {
        Ok(self.rows.iter().map(|row| row.id.clone()).max())
    }

    fn insert(&mut self, row: &StateRow) -> Result<InsertOutcome, Error> {
        if self.rows.iter().any(|existing| existing.id == row.id) {
            return Ok(InsertOutcome::Conflict);
        }
        self.rows.push(row.clone());
        self.rows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(InsertOutcome::Written)
    }

    fn rows_through(&mut self, up_to_id: &str) -> Result<Vec<StateRow>, Error> {
        Ok(self
            .rows
            .iter()
            .filter(|row| row.id.as_str() <= up_to_id)
            .cloned()
            .collect())
    }

    fn delete_through(&mut self, up_to_id: &str) -> Result<usize, Error> {
        let before = self.rows.len();
        self.rows.retain(|row| row.id.as_str() > up_to_id);
        Ok(before - self.rows.len())
    }
}

// ---------------------------------------------------------------------------
// `rx-state.test.ts:85` — should increment in sort order
// ---------------------------------------------------------------------------

#[test]
fn upstream_85_ids_increment_in_sort_order() {
    // The upstream case generates 1000 ids and asserts `ids` equals `ids.sort()`. A
    // thousand is cheap, and the padding is the entire reason ids are strings, so the
    // full 1000 runs rather than a sample.
    let mut ids = Vec::with_capacity(1_001);
    let mut current = next_state_id(None).expect("first");
    ids.push(current.clone());
    for _ in 0..1_000 {
        current = next_state_id(Some(&current)).expect("next");
        ids.push(current.clone());
    }

    assert_eq!(ids.len(), 1_001);
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "id order must equal string order");
    assert!(ids.iter().all(|id| id.len() == ID_LENGTH));
    assert_eq!(ids[1_000], format!("{:0width$}", 1_000, width = ID_LENGTH));
}

// ---------------------------------------------------------------------------
// `rx-state.test.ts:118` — should write without error
// ---------------------------------------------------------------------------

#[test]
fn upstream_118_writing_a_scalar_works() {
    let mut state = state();
    state.set("foo", |_| json!("bar")).expect("set");
    assert_eq!(state.get_path("foo"), Some(&json!("bar")));
}

// ---------------------------------------------------------------------------
// `rx-state.test.ts:124` — write multiple times at once
// ---------------------------------------------------------------------------

#[test]
fn upstream_124_ten_concurrent_modifiers_accumulate() {
    // `state.set('a', plusOne)` ten times, then `state.get('a') === 10`. Upstream relies
    // on the write queue serialising them; here `set` applies immediately in call order,
    // which is the same fold.
    let mut state = state();
    state.set("a", |_| json!(0)).expect("set");
    for _ in 0..10 {
        state
            .set("a", |current| {
                let value = current.as_i64().unwrap_or(0);
                json!(value + 1)
            })
            .expect("set");
    }
    assert_eq!(state.get_path("a"), Some(&json!(10)));
}

// ---------------------------------------------------------------------------
// `rx-state.test.ts:133` — update nested
// ---------------------------------------------------------------------------

#[test]
fn upstream_133_nested_writes_create_then_replace() {
    let mut state = state();
    state.set("nes", |_| json!({})).expect("set");
    state.set("nes.ted", |_| json!("foo")).expect("set");
    assert_eq!(state.get_path("nes.ted"), Some(&json!("foo")));
    state.set("nes.ted", |_| json!("foo2")).expect("set");
    assert_eq!(state.get_path("nes.ted"), Some(&json!("foo2")));
}

// ---------------------------------------------------------------------------
// `rx-state.test.ts:142` — update complete state at once
// ---------------------------------------------------------------------------

#[test]
fn upstream_142_the_empty_path_replaces_the_whole_state() {
    // `::140-148`: `state.set('', () => ({foo: 'bar2'}))` discards everything else.
    let mut state = state();
    state.set("foo", |_| json!("bar")).expect("set");
    state.set("other", |_| json!(1)).expect("set");
    let ops = state.set("", |_| json!({ "foo": "bar2" })).expect("set");
    assert_eq!(state.get(), &json!({ "foo": "bar2" }));
    assert_eq!(
        ops,
        vec![StateOperation::whole(json!({ "foo": "bar2" }))],
        "the whole-state write uses the empty path"
    );
}

// ---------------------------------------------------------------------------
// `rx-state.test.ts:150` — many writes end up as a single storage write
// ---------------------------------------------------------------------------

#[test]
fn upstream_150_many_writes_coalesce_into_one_row() {
    // Upstream: 100 `state.set('a', ..)` calls produce exactly two rows in the collection
    // (the initial write plus one coalesced row). The mechanism is that `_nonPersisted`
    // accumulates and the write loop drains it into one row (`rx-state.ts:167-170`), so
    // this test is about `take_pending`, not about timing.
    let mut state = state();
    let mut log = Log::default();

    // First write on its own, as upstream's `await state.set('a', () => 0)`. The drain
    // has to be explicit: `set` both applies the modifier and queues the op, and
    // `take_pending` is what empties the queue (`rx-state.ts:167-170`).
    state.set("a", |_| json!(0)).expect("set");
    let ops = state.take_pending();
    write_once(&mut state, &mut log, ops).expect("write");
    assert!(
        state.take_pending().is_empty(),
        "the first write was drained"
    );

    // Then 100 more with no intervening drain: they all land in one row.
    for index in 0..100 {
        state.set("a", |_| json!(index)).expect("set");
    }
    let pending = state.take_pending();
    assert!(state.take_pending().is_empty(), "drained once, empty after");
    assert_eq!(pending.len(), 100, "every op is queued");
    write_once(&mut state, &mut log, pending).expect("write");

    assert_eq!(log.rows.len(), 2, "100 sets produced one additional row");
    assert_eq!(state.get_path("a"), Some(&json!(99)), "the last write wins");
}

/// Drains `ops` into one row, retrying on conflict the way the write loop does.
fn write_once(state: &mut State, log: &mut Log, ops: Vec<StateOperation>) -> Result<(), Error> {
    if ops.is_empty() {
        return Ok(());
    }
    for _ in 0..16 {
        let next_id = next_state_id(log.last_id()?.as_deref())?;
        let row = state.row_for(next_id, ops.clone())?;
        match log.insert(&row)? {
            InsertOutcome::Written => return Ok(()),
            InsertOutcome::Conflict => continue,
        }
    }
    Err(Error::ConflictLivelock {
        id: "?".to_string(),
        attempts: 16,
    })
}

// ---------------------------------------------------------------------------
// `rx-state.test.ts:167` / `:174` / `:184` — .get()
// ---------------------------------------------------------------------------

#[test]
fn upstream_167_get_root_state() {
    let mut state = state();
    state.set("foo", |_| json!("bar")).expect("set");
    assert_eq!(state.get(), &json!({ "foo": "bar" }));
}

#[test]
fn upstream_174_get_the_updated_value() {
    let mut state = state();
    state.set("foo", |_| json!("bar")).expect("set");
    assert_eq!(state.get_path("foo"), Some(&json!("bar")));
    state.set("foo", |_| json!("bar2")).expect("set");
    assert_eq!(state.get_path("foo"), Some(&json!("bar2")));
}

#[test]
fn upstream_184_get_nested_values() {
    let mut state = state();
    state.set("nes", |_| json!({ "ted": "foo" })).expect("set");
    assert_eq!(state.get_path("nes"), Some(&json!({ "ted": "foo" })));
    assert_eq!(state.get_path("nes.ted"), Some(&json!("foo")));

    state.set("nes.ted", |_| json!("foo2")).expect("set");
    assert_eq!(state.get_path("nes"), Some(&json!({ "ted": "foo2" })));
    assert_eq!(state.get_path("nes.ted"), Some(&json!("foo2")));
}

#[test]
fn upstream_200_get_does_not_throw_on_a_missing_value() {
    // `:200-206` — reading an unset path is `undefined`, not an error.
    let state = state();
    assert_eq!(state.get_path("nope"), None);
    assert_eq!(state.get_path("nope.deeper"), None);
}

// ---------------------------------------------------------------------------
// `rx-state.test.ts:295` — cleanup merges the rows into one
// ---------------------------------------------------------------------------

#[test]
fn upstream_295_cleanup_folds_the_log_into_one_row() {
    // `:295-323`. Build more than five rows, then compact: the whole state is written as
    // one `k: ''` op and the older rows are deleted.
    let mut state = state();
    let mut log = Log::default();
    for index in 0..8 {
        state.set("counter", |_| json!(index)).expect("set");
        let ops = state.take_pending();
        write_once(&mut state, &mut log, ops).expect("write");
    }
    assert_eq!(log.rows.len(), 8);

    let first = log.rows.first().expect("first").id.clone();
    let last = log.rows.last().expect("last").id.clone();
    assert!(State::cleanup_needed(&first, &last).expect("check"));

    // The compaction: write the fold, then delete everything up to `last`.
    let current = state.get().clone();
    state.set("", |_| current).expect("whole write");
    let ops = state.take_pending();
    write_once(&mut state, &mut log, ops).expect("write");
    let deleted = log.delete_through(&last).expect("delete");
    assert_eq!(deleted, 8, "every row seen before the compaction goes");

    assert_eq!(log.rows.len(), 1, "one row left: the compaction itself");
    // And replaying what is left reproduces the state.
    let replayed = State::replay(&log.rows).expect("replay");
    assert_eq!(replayed, json!({ "counter": 7 }));
}

// ---------------------------------------------------------------------------
// Replay, which is how a second instance joins (`:325-345`)
// ---------------------------------------------------------------------------

#[test]
fn a_later_instance_replays_the_log_into_the_same_state() {
    let mut writer = State::new("", INSTANCE).expect("state");
    let mut log = Log::default();
    writer.set("a", |_| json!(1)).expect("set");
    let ops = writer.take_pending();
    write_once(&mut writer, &mut log, ops).expect("write");
    writer.set("nested.b", |_| json!("x")).expect("set");
    let ops = writer.take_pending();
    write_once(&mut writer, &mut log, ops).expect("write");

    let replayed = State::replay(&log.rows).expect("replay");
    assert_eq!(
        replayed,
        *writer.get(),
        "replay reproduces the writer's state"
    );
}

#[test]
fn a_conflict_retries_onto_the_next_id() {
    // `rx-state.ts:210-218`: another instance took the id, so re-read and retry.
    let mut log = Log::default();
    let other = StateRow::new(
        "00000000000000",
        INSTANCE,
        vec![StateOperation::at("a", json!(1))],
    )
    .expect("row");
    log.insert(&other).expect("insert");

    let mut state = state();
    state.set("b", |_| json!(2)).expect("set");
    let ops = state.take_pending();
    let first_attempt = next_state_id(log.last_id().expect("last").as_deref()).expect("id");
    assert_eq!(
        first_attempt, "00000000000001",
        "the retry computes from the new last id, not the stale one"
    );
    write_once(&mut state, &mut log, ops).expect("write");
    assert_eq!(log.rows.len(), 2);
    assert_eq!(
        State::replay(&log.rows).expect("replay"),
        json!({ "a": 1, "b": 2 }),
        "both instances' writes survive"
    );
}

// ---------------------------------------------------------------------------
// The schema and the row shape
// ---------------------------------------------------------------------------

#[test]
fn the_schema_is_a_legal_rxdb_schema() {
    // `helpers.ts:8-57`, and it has to pass the schema layer or the collection cannot be
    // created — which is the point of asserting it here rather than trusting the literal.
    let schema = state_schema().expect("schema");
    assert_eq!(schema["title"], json!(SCHEMA_TITLE));
    assert_eq!(schema["primaryKey"], json!("id"));
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // Upstream's literal lists three; `fill_with_default_settings` appends the four meta
    // fields, which is what makes the document storable.
    assert_eq!(
        required,
        vec![
            "id",
            "sId",
            "ops",
            "_deleted",
            "_rev",
            "_meta",
            "_attachments"
        ]
    );
    // `fill_with_default_settings` injected the meta fields.
    assert!(schema["properties"].get("_deleted").is_some());
    // And the whole thing validates.
    dxdb_schema::create_rx_schema(&schema).expect("the state schema must be valid");
}

#[test]
fn a_row_validates_against_the_schema() {
    let schema = state_schema().expect("schema");
    let row = StateRow::new(
        "00000000000000",
        INSTANCE,
        vec![StateOperation::at("a", json!(1))],
    )
    .expect("row");
    // `to_document_data` emits the plugin's own fields; `_deleted`/`_rev`/`_meta`/
    // `_attachments` are assigned by the write path (`dxdb-core::write`), which is what
    // the schema requires. Model that here rather than weakening the schema.
    let mut stored = row.to_document_data();
    let map = stored.as_object_mut().expect("object");
    map.insert("_deleted".to_string(), json!(false));
    map.insert("_rev".to_string(), json!("1-abc"));
    map.insert("_meta".to_string(), json!({ "lwt": 1 }));
    map.insert("_attachments".to_string(), json!({}));
    let errors = dxdb_schema::validate_document(&schema, &stored).expect("validation");
    assert!(errors.is_empty(), "row is invalid: {errors:?}");
}

#[test]
fn a_row_needs_at_least_one_operation() {
    // The schema's `minItems: 1` (`helpers.ts:31`).
    assert!(StateRow::new("00000000000000", INSTANCE, Vec::new()).is_err());
}

#[test]
fn the_instance_id_must_be_the_schema_length() {
    let error = State::new("", "short").expect_err("short instance id");
    assert!(matches!(error, Error::InvalidId(_)));
    assert_eq!(INSTANCE.len(), INSTANCE_ID_LENGTH);
}

#[test]
fn the_collection_name_is_prefixed() {
    // `rx-state.ts:321`: `'rx-state-' + prefix`.
    let state = State::new("", INSTANCE).expect("state");
    assert_eq!(state.collection_name(), "rx-state-");
    let state = State::new("foobar", INSTANCE).expect("state");
    assert_eq!(state.collection_name(), "rx-state-foobar");
}

#[test]
fn an_operation_round_trips_through_json() {
    let operation = StateOperation::at("nested.value", json!([1, 2]));
    assert_eq!(
        StateOperation::from_json(&operation.to_json()).expect("parse"),
        operation
    );
}

#[test]
fn merging_operations_in_order_means_later_ones_win() {
    let mut state = json!({});
    merge_operations(
        &mut state,
        &[
            StateOperation::at("a", json!(1)),
            StateOperation::at("a", json!(2)),
            StateOperation::at("b.c", json!(3)),
        ],
    )
    .expect("merge");
    assert_eq!(state, json!({ "a": 2, "b": { "c": 3 } }));
}

#[test]
fn the_cleanup_threshold_is_five_rows() {
    // `rx-state.ts:300`, exposed as a constant so the rule is not a magic number.
    assert_eq!(CLEANUP_MINIMUM_ROWS, 5);
}
