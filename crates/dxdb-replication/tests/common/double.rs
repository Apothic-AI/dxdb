//! THROWAWAY in-memory endpoint double.
//!
//! DELETE-WHEN: `dxdb-memory` (lane D) publishes an `RxStorageInstance`
//! implementation, and this lane's protocol exists. Then the two `DocStore`s below
//! are replaced by two `dxdb-memory` instances and only the adapter keeps its shape.
//! No test body changes.
//!
//! # Why it exists
//!
//! The suite is gated on an [`EndpointAdapter`]. To prove the gate is real — rather
//! than a `todo!()` pretending to be one — the suite is run against this double. It
//! is a plain ordered-JSON document store with the three behaviours the suite's
//! assertions are actually sensitive to:
//!
//! 1. **Soft deletes.** Documents are never removed; `_deleted` is set. The suite's
//!    delete assertion branches on this, and the double takes the `softDeletes: true`
//!    branch (the harder one).
//! 2. **Monotonic last-write time.** `_meta.lwt` uses the shape upstream's `now()`
//!    produces (`ms + k*0.01`, `k` in 1..=99, two decimals), because
//!    `getChangedDocumentsSince` orders by `(lwt, primary)` and a whole-millisecond
//!    clock would tie.
//! 3. **Revisions.** `{height}-{token}`, height 1 on insert and `previous + 1` after;
//!    a `previous._rev` mismatch is a conflict that carries the in-DB document, which
//!    is the contract every layer above storage assumes (`PLAN.md` 4.2).
//!
//! # What it deliberately does not do
//!
//! - Not a storage: no indexes, no query language, no schema validation beyond the
//!   fields the suite touches.
//! - Not a network: the master is an in-process struct, so nothing here exercises a
//!   transport. The two-process test is a later gate.
//! - No changefeeds: `changed_since` is a pull over the same `(lwt, primary)` order,
//!   which is the fallback upstream defines when a storage lacks a native
//!   `getChangedDocumentsSince` (`rx-storage-helper.ts:936-960`).
//!
//! Anything this double gets wrong is a bug in a fixture, not a parity bug. That is
//! why it is marked throwaway and why every behaviour above cites the upstream
//! `file:line` it copies.

#![allow(dead_code)]

use serde_json::Value;

use dxdb_replication::conformance::{
    fixture_string, human_data, human_primary_path, EndpointAdapter, ReplicationHandle,
};

/// Batch size a pull may return, mirroring upstream's `pullBatchSize` default of 100
/// (`plugins/replication/index.ts:462`).
pub const BATCH_SIZE: usize = 100;

/// How many push-then-pull passes the double allows before declaring divergence.
///
/// A correct fixpoint needs at most a handful: each pass propagates every difference in
/// one direction. The bound exists so a bug in this fixture fails loudly instead of
/// hanging the suite.
pub const MAX_SYNC_PASSES: usize = 8;

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Deterministic monotonic clock producing upstream's `lwt` shape.
///
/// Upstream (`plugins/utils/utils-time.ts:24-41`) returns `ms + k*0.01` with `k` in
/// `1..=99`, bumping the millisecond when 99 is exceeded, so a value is never
/// returned twice. The values differ per run upstream; the shape is wire behaviour
/// (DECISIONS.md D4) and is reproduced. The start millisecond is fixed so a failing
/// case is replayable.
pub struct TestClock {
    ms: i64,
    sub: i64,
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl TestClock {
    pub fn new() -> Self {
        // 2026-10-25T00:00:00Z, an arbitrary but fixed epoch.
        Self {
            ms: 1_793_001_600_000,
            sub: 0,
        }
    }

    /// Next `_meta.lwt`. Same branch structure as upstream: the sub-millisecond
    /// counter advances first and carries into the millisecond.
    pub fn now(&mut self) -> f64 {
        self.sub += 1;
        if self.sub == 100 {
            self.ms += 1;
            self.sub = 1;
        }
        self.ms as f64 + self.sub as f64 * 0.01
    }
}

// ---------------------------------------------------------------------------
// Revisions
// ---------------------------------------------------------------------------

/// Upstream `createRevision` (`plugins/utils/utils-revision.ts:59-65`).
pub fn create_revision(token: &str, previous: Option<&Value>) -> String {
    let height = match previous {
        None => 1,
        Some(doc) => {
            get_height_of_revision(
                doc.get("_rev")
                    .and_then(Value::as_str)
                    .expect("a stored document has a _rev"),
            ) + 1
        }
    };
    format!("{height}-{token}")
}

/// Upstream `getHeightOfRevision` (`utils-revision.ts:38-53`): the integer before the
/// first `-`.
pub fn get_height_of_revision(revision: &str) -> i64 {
    let dash = revision
        .find('-')
        .unwrap_or_else(|| panic!("malformatted revision: {revision}"));
    revision[..dash]
        .parse::<i64>()
        .unwrap_or_else(|_| panic!("malformatted revision: {revision}"))
}

// ---------------------------------------------------------------------------
// JSON number helpers
// ---------------------------------------------------------------------------

/// A JSON number for an `f64` that is known to be finite.
pub fn json_number(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .expect("lwt is always finite")
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// The result of a write.
pub enum WriteOutcome {
    Written(Value),
    /// The row lost: the payload is the document that is in the store, which is what
    /// a 409 must carry (`rx-error.d.ts:186-196`, `documentInDb`).
    Conflict(Value),
}

/// One endpoint's document set: a fork, or the master.
pub struct DocStore {
    docs: Vec<Value>,
    clock: TestClock,
    token: String,
}

impl DocStore {
    pub fn new(token: &str) -> Self {
        Self {
            docs: Vec::new(),
            clock: TestClock::new(),
            token: token.to_string(),
        }
    }

    pub fn all(&self) -> &[Value] {
        &self.docs
    }

    pub fn non_deleted(&self) -> Vec<Value> {
        self.docs
            .iter()
            .filter(|d| !is_deleted(d))
            .cloned()
            .collect()
    }

    pub fn find(&self, id: &str) -> Option<&Value> {
        self.docs
            .iter()
            .find(|d| d.get(human_primary_path()).and_then(Value::as_str) == Some(id))
    }

    /// Insert or update, mirroring `bulkWrite`'s per-document atomicity: on a
    /// `previous._rev` mismatch the row is not applied and the in-DB state is
    /// returned.
    ///
    /// `previous` follows `BulkWriteRow.previous`: `None` means "this is an insert",
    /// and a conflict is still possible then.
    pub fn write(&mut self, doc: &Value, previous: Option<&Value>) -> WriteOutcome {
        let id = doc
            .get(human_primary_path())
            .and_then(Value::as_str)
            .expect("a document has a primary key")
            .to_string();
        let existing = self.find(&id).cloned();

        match (&existing, previous) {
            (Some(in_db), None) => return WriteOutcome::Conflict(in_db.clone()),
            (Some(in_db), Some(prev)) => {
                if in_db.get("_rev") != prev.get("_rev") {
                    return WriteOutcome::Conflict(in_db.clone());
                }
            }
            (None, Some(_)) => return WriteOutcome::Conflict(Value::Null),
            (None, None) => {}
        }

        let lwt = self.clock.now();
        let rev = create_revision(&self.token, existing.as_ref());
        let mut new_doc = doc.clone();
        let map = new_doc
            .as_object_mut()
            .expect("a document is a JSON object");
        map.insert("_meta".to_string(), build_meta(lwt));
        map.insert("_rev".to_string(), Value::String(rev));
        map.entry("_deleted".to_string())
            .or_insert(Value::Bool(false));
        map.entry("_attachments".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));

        match existing {
            Some(_) => {
                let slot = self
                    .docs
                    .iter_mut()
                    .find(|d| {
                        d.get(human_primary_path()).and_then(Value::as_str) == Some(id.as_str())
                    })
                    .expect("found above");
                *slot = new_doc.clone();
            }
            None => self.docs.push(new_doc.clone()),
        }
        WriteOutcome::Written(new_doc)
    }

    /// Upstream `getChangedDocumentsSince` order: `(_meta.lwt, primaryKey)`
    /// ascending (`rx-storage-helper.ts:889-934`).
    ///
    /// Tombstones are included: a pull that cannot see them can never replicate a
    /// deletion.
    pub fn changed_since(&self, checkpoint: Option<&Value>) -> Vec<Value> {
        let mut sorted: Vec<&Value> = self.docs.iter().collect();
        sorted.sort_by(|a, b| {
            lwt_of(a)
                .partial_cmp(&lwt_of(b))
                .expect("lwt is finite")
                .then_with(|| {
                    let ka = a
                        .get(human_primary_path())
                        .map(dxdb_json::to_js_json)
                        .unwrap_or_default();
                    let kb = b
                        .get(human_primary_path())
                        .map(dxdb_json::to_js_json)
                        .unwrap_or_default();
                    dxdb_jsstr::compare_js(&ka, &kb)
                })
        });

        sorted
            .into_iter()
            .filter(|doc| match checkpoint {
                None => true,
                Some(cp) => {
                    let cp_lwt = lwt_of(cp);
                    let doc_lwt = lwt_of(doc);
                    let cp_id = cp
                        .get(human_primary_path())
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let doc_id = doc
                        .get(human_primary_path())
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if doc_lwt > cp_lwt {
                        true
                    } else if doc_lwt == cp_lwt {
                        dxdb_jsstr::compare_js(doc_id, cp_id) == std::cmp::Ordering::Greater
                    } else {
                        false
                    }
                }
            })
            .take(BATCH_SIZE)
            .cloned()
            .collect()
    }

    /// The checkpoint a pull over this store would return after `docs`.
    ///
    /// Upstream's checkpoint is `{id, lwt}` by default (`rx-storage.d.ts:310-313`) and
    /// resumes "exactly after the last document that was returned". Returning the last
    /// document's own key makes that observable, which is what a checkpoint-equality
    /// assertion needs.
    pub fn checkpoint_after(&self, docs: &[Value]) -> Option<Value> {
        let last = docs.last()?;
        let mut map = serde_json::Map::new();
        map.insert(
            human_primary_path().to_string(),
            last.get(human_primary_path())
                .cloned()
                .unwrap_or(Value::Null),
        );
        map.insert("lwt".to_string(), json_number(lwt_of(last)));
        Some(Value::Object(map))
    }
}

pub fn is_deleted(doc: &Value) -> bool {
    doc.get("_deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn lwt_of(doc: &Value) -> f64 {
    doc.get("_meta")
        .and_then(|m| m.get("lwt"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

/// `_meta` as upstream builds it (`utils-document.ts:17-30`): `lwt` plus an open
/// index signature for plugin tags. The double never writes a tag, so the map is
/// just `lwt` — which is also what a document looks like before any replication has
/// touched it.
fn build_meta(lwt: f64) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("lwt".to_string(), json_number(lwt));
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

/// A payload plus the stub that carries it across the wire.
///
/// Upstream keeps payloads out of the document: `_attachments` holds
/// `{length, digest, type}` and readers resolve bytes by digest
/// (`plugins/attachments/index.ts:72-84`, `rx-storage.d.ts:115-139`).
pub struct AttachmentCreator {
    pub id: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl AttachmentCreator {
    pub fn new(id: &str, mime: &str, bytes: &[u8]) -> Self {
        Self {
            id: id.to_string(),
            mime: mime.to_string(),
            bytes: bytes.to_vec(),
        }
    }

    /// Default digest: SHA-256, lowercase hex, untruncated — upstream's
    /// `defaultHashSha256` (`utils-hash.ts:32-52`).
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&self.bytes);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The stub stored in `_attachments`.
    pub fn stub(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("length".to_string(), json_number(self.bytes.len() as f64));
        map.insert("digest".to_string(), Value::String(self.digest()));
        map.insert("type".to_string(), Value::String(self.mime.clone()));
        Value::Object(map)
    }
}

// ---------------------------------------------------------------------------
// Collections and the endpoint pair
// ---------------------------------------------------------------------------

/// A handle the suite passes around in place of `RxCollection<any>`.
#[derive(Clone, Copy, Debug)]
pub struct DoubleCollection {
    index: usize,
}

/// One collection's fork, plus the flags the suite branches on.
pub struct CollectionState {
    pub name: String,
    pub fork: DocStore,
    pub has_attachments: bool,
    /// What this collection believes the master holds, per document id.
    ///
    /// This is the meta instance's `docData` half (`meta-instance.ts:107-144`), reduced to
    /// the one field the conflict path needs. Without it a push cannot say "I am writing
    /// on top of state X", so every push looks like an insert and the master silently
    /// overwrites instead of reporting a conflict. Upstream stores it in a real storage
    /// instance because it must survive a restart; the double keeps it in memory, which is
    /// the whole difference between a fixture and `dxdb-memory`.
    pub assumed_master: Vec<(String, Value)>,
    /// True once a live replication was started for this collection.
    pub live: bool,
}

impl CollectionState {
    fn assumed(&self, id: &str) -> Option<&Value> {
        self.assumed_master
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, value)| value)
    }

    fn set_assumed(&mut self, id: &str, state: Value) {
        match self.assumed_master.iter_mut().find(|(key, _)| key == id) {
            Some(slot) => slot.1 = state,
            None => self.assumed_master.push((id.to_string(), state)),
        }
    }
}

/// The throwaway adapter: an in-process fork plus an in-process master.
///
/// One master per adapter, because upstream's suite requires a shared endpoint:
/// "Must target a shared server endpoint so that multiple collections can replicate
/// to the same backend" (`replication-base-test-suite.ts:25-27`). That is what makes
/// the "two collections" case meaningful.
pub struct DoubleEndpoint {
    master: DocStore,
    collections: Vec<CollectionState>,
}

impl Default for DoubleEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl DoubleEndpoint {
    pub fn new() -> Self {
        Self {
            master: DocStore::new("master-token"),
            collections: Vec::new(),
        }
    }

    /// Create a collection with `size` pre-inserted humans, mirroring
    /// `humansCollection.create(size, undefined, false)` — the third argument is
    /// `multiInstance: false` at every call site in the suite, so it is fixed.
    pub fn create_collection(&mut self, size: usize, with_attachments: bool) -> DoubleCollection {
        let index = self.collections.len();
        let mut fork = DocStore::new(&format!("fork-token-{}", index + 1));
        // Upstream draws these from `Math.random()`, so every document differs. The
        // deterministic port salts by (collection, ordinal) so a failing case replays.
        for ordinal in 0..size {
            let passport = fixture_string(&format!("passport-{index}-{ordinal}"), 10);
            let doc = human_data(Some(&passport), None, None);
            match fork.write(&doc, None) {
                WriteOutcome::Written(_) => {}
                WriteOutcome::Conflict(in_db) => panic!(
                    "fixture insert conflicted for {passport}; in-db rev {}",
                    in_db
                        .get("_rev")
                        .map(dxdb_json::to_js_json)
                        .unwrap_or_default()
                ),
            }
        }
        self.collections.push(CollectionState {
            name: format!("human-{}", index + 1),
            fork,
            has_attachments: with_attachments,
            assumed_master: Vec::new(),
            live: false,
        });
        DoubleCollection { index }
    }

    fn state(&self, collection: &DoubleCollection) -> &CollectionState {
        &self.collections[collection.index]
    }

    /// The `syncOnce` body: push the fork to the master, then pull the master back.
    ///
    /// This is *not* the protocol: no checkpoints, no meta instance, no conflict
    /// handler. It is the smallest shape that makes the suite's one-shot cases
    /// converge, and it exists so that an adapter method which silently did nothing
    /// could not pass the "delete one" and "conflict" assertions.
    fn sync_once_impl(&mut self, collection: &DoubleCollection) {
        // Upstream's one-shot sync runs the protocol to an idle state, not one
        // push-then-pull. A single pass converges only when one side is empty, which is
        // why the double loops: each pass either moves a document or changes nothing, and
        // a pass with no change is the fixpoint.
        for _ in 0..MAX_SYNC_PASSES {
            if !self.sync_pass(collection) {
                return;
            }
        }
        panic!(
            "one-shot sync did not reach a fixpoint after {MAX_SYNC_PASSES} passes;              the double is divergent"
        );
    }

    /// One push-then-pull pass. Returns true when anything changed.
    ///
    /// "Changed" is decided by **content**, not by `_rev`. The fork and the master keep
    /// independent revision chains with different tokens and different heights, so
    /// comparing `_rev` across the two would report every document as different forever
    /// and the pass would never reach a fixpoint. That is not a shortcut: the protocol's
    /// own "are these the same state" test is `conflictHandler.isEqual` over document
    /// states (`default-conflict-handler.ts:9-26`), and revisions are local bookkeeping.
    fn sync_pass(&mut self, collection: &DoubleCollection) -> bool {
        let mut changed = false;
        // Push, applying upstream's `masterWrite` rules
        // (`rxStorageInstanceToReplicationHandler`, `index.ts:61-134`):
        //
        //   no master state                  -> insert
        //   master state, no assumed state   -> conflict
        //   assumed state == master state    -> write

        //   otherwise                        -> conflict
        //
        // The assumed state is what makes the difference between "my write is on top of
        // what the master has" and "someone else moved it first". Without this rule a
        // conflicting push silently clobbers the master, and the conformance suite's
        // conflict case cannot distinguish a correct default handler from a last-writer
        // -wins fixture.
        let fork_docs: Vec<Value> = self.collections[collection.index].fork.all().to_vec();
        for doc in fork_docs {
            let id = primary_of(&doc);
            let master_state = self.master.find(&id).cloned();
            let assumed = self.collections[collection.index].assumed(&id).cloned();
            let outcome = match &master_state {
                None => self.master.write(&strip_local_meta(&doc), None),
                Some(master_doc) => {
                    let already_same = same_content(master_doc, &doc);
                    let assumed_matches = assumed
                        .as_ref()
                        .map(|assumed| same_content(assumed, master_doc))
                        .unwrap_or(false);
                    if already_same {
                        continue;
                    }
                    if !assumed_matches {
                        // The master moved (or was never read). Report the real master
                        // state instead of writing.
                        WriteOutcome::Conflict(master_doc.clone())
                    } else {
                        self.master.write(&strip_local_meta(&doc), Some(master_doc))
                    }
                }
            };
            match outcome {
                WriteOutcome::Written(written) => {
                    changed = true;
                    // The assumed master is the state just written, minus local
                    // bookkeeping: `getMetaWriteRow(..., docData = masterState)`
                    // (`meta-instance.ts:147-195`).
                    let state = strip_local_meta(&written);
                    self.collections[collection.index].set_assumed(&id, state);
                }
                WriteOutcome::Conflict(real_master) => {
                    // A conflict is resolved by keeping the master and re-pulling it. The
                    // resolved write is `resolveConflictError`'s job
                    // (`conflicts.ts:22-53`): the fork takes the master's content with a
                    // fresh local revision. Recording that as the assumed state is the
                    // same thing without the revision bookkeeping.
                    let state = strip_local_meta(&real_master);
                    self.collections[collection.index].set_assumed(&id, state.clone());
                    if let Some(fork_doc) =
                        self.collections[collection.index].fork.find(&id).cloned()
                    {
                        if !same_content(&fork_doc, &real_master) {
                            match self.collections[collection.index]
                                .fork
                                .write(&state, Some(&fork_doc))
                            {
                                WriteOutcome::Written(_) => changed = true,
                                WriteOutcome::Conflict(_) => {}
                            }
                        }
                    }
                }
            }
        }

        // Pull: apply every master document back onto the fork.
        let master_docs: Vec<Value> = self.master.all().to_vec();
        let fork = &mut self.collections[collection.index].fork;
        for doc in master_docs {
            let id = primary_of(&doc);
            let fork_state = fork.find(&id).cloned();
            let outcome = match fork_state {
                None => fork.write(&strip_local_meta(&doc), None),
                Some(fork_doc) => {
                    if same_content(&fork_doc, &doc) {
                        continue;
                    }
                    fork.write(&strip_local_meta(&doc), Some(&fork_doc))
                }
            };
            match outcome {
                WriteOutcome::Written(_) => changed = true,
                WriteOutcome::Conflict(in_db) => panic!(
                    "pull conflicted for {id}; fork holds rev {}",
                    in_db
                        .get("_rev")
                        .map(dxdb_json::to_js_json)
                        .unwrap_or_default()
                ),
            }
        }
        // Whatever the fork now holds, it believes the master holds the same. This is the
        // pull's `getMetaWriteRow(state, masterState, ...)` call.
        let master_docs: Vec<Value> = self.master.all().to_vec();
        for doc in master_docs {
            let id = primary_of(&doc);
            let state = strip_local_meta(&doc);
            self.collections[collection.index].set_assumed(&id, state);
        }
        changed
    }

    /// Runs a sync pass for every collection with a live replication.
    ///
    /// Upstream's live replication converges on its own: the fork's change stream feeds
    /// the push and the master's change stream feeds the pull. A cross-collection
    /// dependency (collection A must observe what collection B pushed) therefore arrives
    /// without anyone calling `syncOnce`. The double has no background task, so a case
    /// body's own sync calls drive the fan-out — which is why this is called from
    /// `sync_once` and from the fixture writes.
    ///
    /// A fixture that skipped this would let `two collections` pass or fail on *when*
    /// the body happened to call sync, which is the flakiness upstream's own `reSync()`
    /// calls exist to avoid.
    fn service_live_collections(&mut self) {
        let live: Vec<usize> = self
            .collections
            .iter()
            .enumerate()
            .filter(|(_, state)| state.live)
            .map(|(index, _)| index)
            .collect();
        // Round-robin until a *whole round* changes nothing. Fixpointing each collection
        // in turn is not enough: the last collection to sync publishes state the earlier
        // ones have already passed, so they would remain a round behind. Upstream has no
        // such ordering because its change streams all point at the same master.
        for _ in 0..MAX_SYNC_PASSES {
            let mut round_changed = false;
            for index in &live {
                if self.sync_pass(&DoubleCollection { index: *index }) {
                    round_changed = true;
                }
            }
            if !round_changed {
                return;
            }
        }
        panic!(
            "live fan-out did not reach a fixpoint after {MAX_SYNC_PASSES} rounds; \
             the double is divergent"
        );
    }

    // -- fixture writes, used by case bodies ---------------------------------

    /// Upstream `collection.insert(doc)`.
    pub fn insert(&mut self, collection: DoubleCollection, doc: &Value) -> Value {
        let store = &mut self.collections[collection.index].fork;
        match store.write(doc, None) {
            WriteOutcome::Written(d) => d,
            WriteOutcome::Conflict(in_db) => panic!(
                "insert conflicted; in-db rev {}",
                in_db
                    .get("_rev")
                    .map(dxdb_json::to_js_json)
                    .unwrap_or_default()
            ),
        }
    }

    /// Upstream `collection.bulkInsert(docs)`.
    pub fn bulk_insert(&mut self, collection: DoubleCollection, docs: &[Value]) {
        for doc in docs {
            self.insert(collection, doc);
        }
    }

    /// Runs the live fan-out. Called by the case bodies where upstream would rely on the
    /// change stream having propagated by the time `awaitInSync()` returns.
    pub fn propagate_live(&mut self) {
        self.service_live_collections();
    }

    /// Upstream `doc.incrementalPatch({field: value})`: read-modify-write against the
    /// current state. That is what makes the conflict case's two divergent local
    /// writes possible, since each side patches its own fork.
    pub fn patch(
        &mut self,
        collection: DoubleCollection,
        id: &str,
        field: &str,
        value: Value,
    ) -> Value {
        let current = self.collections[collection.index]
            .fork
            .find(id)
            .cloned()
            .unwrap_or_else(|| panic!("patch on a missing document {id}"));
        let mut next = current.clone();
        next.as_object_mut()
            .expect("a document is a JSON object")
            .insert(field.to_string(), value);
        let store = &mut self.collections[collection.index].fork;
        match store.write(&next, Some(&current)) {
            WriteOutcome::Written(d) => d,
            WriteOutcome::Conflict(in_db) => panic!(
                "patch conflicted on {id}; in-db rev {}",
                in_db
                    .get("_rev")
                    .map(dxdb_json::to_js_json)
                    .unwrap_or_default()
            ),
        }
    }

    /// Upstream `doc.remove()`: a soft delete, so the revision advances and the
    /// tombstone replicates.
    pub fn remove(&mut self, collection: DoubleCollection, id: &str) -> Value {
        let current = self.collections[collection.index]
            .fork
            .find(id)
            .cloned()
            .unwrap_or_else(|| panic!("remove on a missing document {id}"));
        let mut next = current.clone();
        next.as_object_mut()
            .expect("a document is a JSON object")
            .insert("_deleted".to_string(), Value::Bool(true));
        let store = &mut self.collections[collection.index].fork;
        match store.write(&next, Some(&current)) {
            WriteOutcome::Written(d) => d,
            WriteOutcome::Conflict(_) => panic!("remove conflicted on {id}"),
        }
    }

    /// The primary key of the single document a fork holds, for cases that start from
    /// one document. Panics if the fork does not hold exactly one — which is a
    /// clearer failure than picking one arbitrarily.
    pub fn only_document(&self, collection: DoubleCollection) -> Value {
        let docs = self.collections[collection.index].fork.non_deleted();
        assert_eq!(
            docs.len(),
            1,
            "expected exactly one live document in collection {}",
            self.collections[collection.index].name
        );
        docs.into_iter().next().expect("checked above")
    }
}

fn primary_of(doc: &Value) -> String {
    doc.get(human_primary_path())
        .and_then(Value::as_str)
        .expect("a document has a primary key")
        .to_string()
}

/// True when two stored documents hold the same replicated content.
///
/// Ignores `_rev` and `_meta` (local bookkeeping with independent chains on each side)
/// and compares everything else, including `_deleted` and attachment stubs. This is the
/// double's stand-in for `conflictHandler.isEqual`, which is what the real protocol uses
/// to decide whether a document needs syncing at all.
fn same_content(a: &Value, b: &Value) -> bool {
    fn projection(doc: &Value) -> Value {
        let Value::Object(map) = doc else {
            return doc.clone();
        };
        let mut out = serde_json::Map::new();
        for (key, value) in map {
            if matches!(key.as_str(), "_rev" | "_meta") {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }
        Value::Object(out)
    }
    projection(a) == projection(b)
}

/// Strips the fields a fork adds locally before a document goes to the master.
///
/// Upstream's `writeDocToDocState(doc, keepAttachments, keepMeta)` deletes `_meta` and
/// `_rev`, and `_attachments` when attachments are off (`helper.ts:53-68`).
fn strip_local_meta(doc: &Value) -> Value {
    let Value::Object(map) = doc else {
        return doc.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, value) in map {
        if matches!(key.as_str(), "_meta" | "_rev") {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Adapter implementation
// ---------------------------------------------------------------------------

/// The inert replication handle the double returns.
///
/// A replication handle that reports the call and does nothing. Kept next to the
/// double rather than in the lib so its throwaway-ness lives in one file. Every case
/// that depends on live replication waiting for something is marked deferred in
/// [`dxdb_replication::conformance::UPSTREAM_CASES`].
pub struct InertReplication;

impl ReplicationHandle for InertReplication {
    fn await_initial_replication(&mut self) {}
    fn await_in_sync(&mut self) {}
    fn re_sync(&mut self) {}
    fn take_errors(&mut self) -> Vec<String> {
        Vec::new()
    }
}

impl dxdb_replication::conformance::LiveFanout for DoubleEndpoint {
    fn fan_out_live(&mut self) {
        self.service_live_collections();
    }
}

impl EndpointAdapter for DoubleEndpoint {
    type Collection = DoubleCollection;
    type Handle = InertReplication;

    fn start_replication(&mut self, collection: &Self::Collection) -> Self::Handle {
        self.collections[collection.index].live = true;
        InertReplication
    }

    fn sync_once(&mut self, collection: &Self::Collection) {
        self.sync_once_impl(collection);
        self.service_live_collections();
    }

    fn all_server_docs(&mut self) -> Vec<Value> {
        self.master.all().to_vec()
    }

    fn clean_up_server(&mut self) {
        // A fresh master with the same token, so checkpoint-ish state cannot leak
        // between cases. The collections keep their forks, matching upstream, where
        // `cleanUpServer()` only clears the server.
        self.master = DocStore::new("master-token");
    }

    fn soft_deletes(&self) -> bool {
        // The double keeps tombstones, so the suite's delete assertion takes the
        // harder branch: the master still holds 3 documents and one is flagged
        // deleted.
        true
    }

    fn is_deleted(&self, server_doc: &Value) -> bool {
        is_deleted(server_doc)
    }

    fn primary_of_server_doc(&self, server_doc: &Value) -> String {
        primary_of(server_doc)
    }

    fn fork_documents(&self, collection: &Self::Collection) -> Vec<Value> {
        self.state(collection).fork.non_deleted()
    }

    fn sync_once_with_attachments(&mut self, collection: &Self::Collection) -> Option<()> {
        if !self.state(collection).has_attachments {
            return None;
        }
        self.sync_once_impl(collection);
        Some(())
    }

    fn sync_once_with_attachments_disabled(&mut self, collection: &Self::Collection) -> Option<()> {
        if !self.state(collection).has_attachments {
            return None;
        }
        self.sync_once_impl(collection);
        Some(())
    }
}
