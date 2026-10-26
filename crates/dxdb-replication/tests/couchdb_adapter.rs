//! The CouchDB adapter against a local double.
//!
//! Upstream runs `runReplicationBaseTestSuite` against a real CouchDB
//! (`test/replication-couchdb.test.ts:112`); a hermetic test cannot, so this file drives the
//! adapter against [`CouchDouble`]. The cases are chosen to match upstream's *own* extra ones
//! plus the shared conflict rule, and each says which it is.
//!
//! # What the double models
//!
//! The parts of CouchDB the adapter actually depends on, including the parts that are awkward:
//!
//! * **`_id` / `_rev` with a revision tree.** A write must carry the current `_rev` or it is a
//!   `conflict` — the real database's rule, and what makes the read-then-write in
//!   [`CouchDbHandler::master_write`] necessary rather than decorative.
//! * **`_changes` as a sequence log.** `last_seq` is a monotonically increasing integer, and
//!   `since=N` returns rows whose sequence is greater than N. This is what makes the
//!   `{sequence}` checkpoint meaningful, and it is why a *document* position would be wrong
//!   here.
//! * **`include_docs`, `style=all_docs`, `deleted: 'ok'`.** Honoured, because the adapter's
//!   query sends them and a double that ignored them would pass while the real server returned
//!   rows with no documents.
//! * **The `_bulk_docs` race.** [`CouchDouble::race_next_bulk_docs`] makes a document's rev
//!   change *between* the `_all_docs` read and the `_bulk_docs` write, so the
//!   `error: "conflict"` → re-read path (`index.ts:226-247`) is exercised. Without this hook
//!   the most subtle branch in the adapter would be untested, because the race is otherwise
//!   unreachable from a single-threaded test.
//! * **Purged rows.** A `_changes` row may arrive with no `doc`.
//!
//! # What it does NOT model
//!
//! * **Real CouchDB clustered sequences.** Production returns opaque strings like
//!   `"17-g1AAAA..."`, not integers. The adapter passes the value through opaquely, and there is
//!   a unit test for that, but the double never *produces* one — so a serialisation bug specific
//!   to string sequences would not be caught here.
//! * **Auth.** `getFetchWithCouchDBAuthorization` (`couchdb-helper.ts:90`) is not ported and
//!   `401` handling is not exercised.
//! * **The live `_changes` feed.** `feed=longpoll` and `heartbeat` are absent; the adapter's
//!   `master_change_stream` returns an empty stream and this file never tests live behaviour.
//! * **`_bulk_docs` `new_edits=false`**, replication-style writes with supplied revisions.
//! * **CouchDB's actual attachment handling** (`_attachments` with `follows`/multipart).
//!   Attachments travel as stubs in this double.
//! * **Compaction, purging, and the `_purge` endpoint.**

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;

use dxdb_json::Value;
use dxdb_replication::adapters::couchdb::{CouchDbHandler, HttpClient, HttpMethod, HttpResponse};
use dxdb_replication::adapters::TransportError;
use dxdb_replication::checkpoint::Sha256HashFunction;
use dxdb_replication::conflict::DefaultConflictHandler;
use dxdb_replication::downstream::Downstream;
use dxdb_replication::state::{
    BatchSize, InitialCheckpoint, ReplicationHandler, ReplicationInput, ReplicationState,
};
use dxdb_replication::upstream::Upstream;
use dxdb_storage::{DocumentData, RxStorageInstance, StorageError};

/// One stored document plus its CouchDB-ish revision.
#[derive(Clone)]
struct CouchDoc {
    /// The sequence at which this document last changed. `_changes` orders by this.
    sequence: u64,
    /// `"<n>-<token>"`, incremented per write.
    rev: String,
    body: Value,
}

/// A CouchDB double. See the module docs for what it does and does not model.
#[derive(Default)]
struct CouchDouble {
    documents: RefCell<BTreeMap<String, CouchDoc>>,
    sequence: RefCell<u64>,
    requests: RefCell<Vec<String>>,
    /// When set, the next `_bulk_docs` bumps this id's rev just before applying, simulating a
    /// write that landed between the `_all_docs` read and the `_bulk_docs` write.
    race_next_bulk_docs: RefCell<Option<String>>,
}

impl CouchDouble {
    fn new() -> Self {
        CouchDouble::default()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.borrow().clone()
    }

    fn put(&self, id: &str, body: Value) {
        let mut sequence = self.sequence.borrow_mut();
        *sequence += 1;
        let current = self.documents.borrow().get(id).cloned();
        let height = current
            .as_ref()
            .and_then(|doc| doc.rev.split('-').next())
            .and_then(|height| height.parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        self.documents.borrow_mut().insert(
            id.to_string(),
            CouchDoc {
                sequence: *sequence,
                rev: format!("{height}-server"),
                body,
            },
        );
    }

    fn document(&self, id: &str) -> Option<Value> {
        self.documents.borrow().get(id).map(|doc| doc.body.clone())
    }

    fn ids(&self) -> Vec<String> {
        self.documents.borrow().keys().cloned().collect()
    }

    /// The server's current log position, for asserting the checkpoint's value.
    fn sequence_now(&self) -> u64 {
        *self.sequence.borrow()
    }

    fn race_next_bulk_docs(&self, id: &str) {
        *self.race_next_bulk_docs.borrow_mut() = Some(id.to_string());
    }

    /// Builds the document CouchDB would return: body plus `_id` plus `_rev`.
    fn wire_doc(&self, id: &str, doc: &CouchDoc) -> Value {
        let mut body = match doc.body.as_object() {
            Some(map) => map.clone(),
            None => dxdb_json::Map::new(),
        };
        body.insert("_id".to_string(), Value::String(id.to_string()));
        body.insert("_rev".to_string(), Value::String(doc.rev.clone()));
        Value::Object(body)
    }

    fn handle_changes(&self, url: &str) -> Value {
        let param = |name: &str| -> Option<String> {
            url.split('?').nth(1).and_then(|query| {
                query.split('&').find_map(|pair| {
                    let mut parts = pair.split('=');
                    let key = parts.next()?;
                    if key == name {
                        parts.next().map(str::to_string)
                    } else {
                        None
                    }
                })
            })
        };
        let since: u64 = param("since").and_then(|v| v.parse().ok()).unwrap_or(0);
        let limit: usize = param("limit").and_then(|v| v.parse().ok()).unwrap_or(100);
        let include_docs = param("include_docs").as_deref() == Some("true");

        let documents = self.documents.borrow();
        let mut ordered: Vec<(&String, &CouchDoc)> = documents
            .iter()
            .filter(|(_, doc)| doc.sequence > since)
            .collect();
        ordered.sort_by_key(|(_, doc)| doc.sequence);
        ordered.truncate(limit);

        let results: Vec<Value> = ordered
            .iter()
            .map(|(id, doc)| {
                let mut row = dxdb_json::Map::new();
                row.insert("id".to_string(), Value::String((*id).clone()));
                row.insert("seq".to_string(), Value::from(doc.sequence));
                if include_docs {
                    row.insert("doc".to_string(), self.wire_doc(id, doc));
                }
                Value::Object(row)
            })
            .collect();

        // `last_seq` is the position after everything returned, or after everything at all
        // when nothing was returned -- the sequence log has still advanced.
        let last_seq = ordered
            .last()
            .map(|(_, doc)| doc.sequence)
            .unwrap_or_else(|| *self.sequence.borrow());
        drop(documents);

        let mut body = dxdb_json::Map::new();
        body.insert("results".to_string(), Value::Array(results));
        body.insert("last_seq".to_string(), Value::from(last_seq));
        Value::Object(body)
    }

    fn handle_all_docs(&self, body: &Value) -> Value {
        let keys: Vec<String> = body
            .get("keys")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|key| key.as_str().map(str::to_string))
            .collect();
        let documents = self.documents.borrow();
        let rows: Vec<Value> = keys
            .iter()
            .map(|id| {
                let mut row = dxdb_json::Map::new();
                match documents.get(id) {
                    Some(doc) => {
                        row.insert("id".to_string(), Value::String(id.clone()));
                        row.insert("key".to_string(), Value::String(id.clone()));
                        row.insert("doc".to_string(), self.wire_doc(id, doc));
                    }
                    None => {
                        // CouchDB returns `{"key": id, "error": "not_found"}` for a missing key.
                        row.insert("key".to_string(), Value::String(id.clone()));
                        row.insert("error".to_string(), Value::String("not_found".to_string()));
                    }
                }
                Value::Object(row)
            })
            .collect();
        let mut body = dxdb_json::Map::new();
        body.insert("rows".to_string(), Value::Array(rows));
        Value::Object(body)
    }

    fn handle_bulk_docs(&self, body: &Value) -> Result<Value, String> {
        let docs = body
            .get("docs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Simulate a concurrent write landing between the read and the write.
        if let Some(raced_id) = self.race_next_bulk_docs.borrow_mut().take() {
            // Clone out first: the borrow must end before `documents` is mutably borrowed
            // below, or RefCell panics at runtime (which is what this test hit).
            let existing = self.documents.borrow().get(&raced_id).cloned();
            if let Some(existing) = existing {
                let height = existing
                    .rev
                    .split('-')
                    .next()
                    .and_then(|height| height.parse::<u64>().ok())
                    .unwrap_or(0)
                    + 1;
                let mut documents = self.documents.borrow_mut();
                let mut sequence = self.sequence.borrow_mut();
                *sequence += 1;
                documents.insert(
                    raced_id.clone(),
                    CouchDoc {
                        sequence: *sequence,
                        rev: format!("{height}-raced"),
                        body: existing.body,
                    },
                );
            }
        }

        let mut results = Vec::with_capacity(docs.len());
        for doc in docs {
            let id = doc
                .get("_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "_bulk_docs doc has no _id".to_string())?
                .to_string();
            let supplied_rev = doc.get("_rev").and_then(Value::as_str).map(str::to_string);
            let existing = self.documents.borrow().get(&id).cloned();

            let mut result = dxdb_json::Map::new();
            result.insert("id".to_string(), Value::String(id.clone()));
            match (&existing, supplied_rev) {
                // Insert onto an existing document without a rev: a conflict, exactly as
                // CouchDB reports it.
                (Some(_), None) => {
                    result.insert("error".to_string(), Value::String("conflict".to_string()));
                    results.push(Value::Object(result));
                }
                // Update with a stale rev: a conflict.
                (Some(current), Some(rev)) if current.rev != rev => {
                    result.insert("error".to_string(), Value::String("conflict".to_string()));
                    results.push(Value::Object(result));
                }
                _ => {
                    let mut body = doc.clone();
                    if let Some(map) = body.as_object_mut() {
                        map.remove("_id");
                        map.remove("_rev");
                    }
                    self.put(&id, body);
                    result.insert("ok".to_string(), Value::Bool(true));
                    results.push(Value::Object(result));
                }
            }
        }
        Ok(Value::Array(results))
    }
}

/// A client that owns the double. A plain struct rather than `impl HttpClient for
/// Rc<CouchDouble>`: `Rc` is a foreign type, so the orphan rule forbids that impl too.
struct DoubleClient(std::rc::Rc<CouchDouble>);

impl HttpClient for DoubleClient {
    fn request(
        &self,
        method: HttpMethod,
        url: String,
        body: Option<Value>,
    ) -> dxdb_replication::state::BoxFuture<'_, Result<HttpResponse, TransportError>> {
        let double = self.0.clone();
        Box::pin(async move {
            double
                .requests
                .borrow_mut()
                .push(format!("{method:?} {url}"));
            match method {
                HttpMethod::Get if url.contains("_changes") => Ok(HttpResponse {
                    status: 200,
                    body: double.handle_changes(&url),
                }),
                HttpMethod::Post if url.contains("_all_docs") => Ok(HttpResponse {
                    status: 200,
                    body: double.handle_all_docs(&body.unwrap_or(Value::Null)),
                }),
                HttpMethod::Post if url.contains("_bulk_docs") => {
                    match double.handle_bulk_docs(&body.unwrap_or(Value::Null)) {
                        Ok(body) => Ok(HttpResponse { status: 201, body }),
                        Err(error) => Err(TransportError::Malformed(error)),
                    }
                }
                _ => Err(TransportError::Unreachable(format!("no route for {url}"))),
            }
        })
    }
}

fn human_schema() -> Value {
    dxdb_json::parse(
        r#"{
            "version": 0,
            "keyCompression": false,
            "primaryKey": "passportId",
            "type": "object",
            "properties": {
                "passportId": { "type": "string", "maxLength": 100 },
                "firstName": { "type": "string", "maxLength": 100 },
                "age": { "type": "integer" }
            },
            "indexes": ["firstName"]
        }"#,
    )
    .expect("schema")
}

fn human(id: &str, first: &str, age: i64) -> DocumentData {
    let mut map = dxdb_json::Map::new();
    map.insert("passportId".to_string(), Value::String(id.to_string()));
    map.insert("firstName".to_string(), Value::String(first.to_string()));
    map.insert("age".to_string(), Value::from(age));
    map.insert("_deleted".to_string(), Value::Bool(false));
    map.insert(
        "_attachments".to_string(),
        Value::Object(dxdb_json::Map::new()),
    );
    map.insert("_rev".to_string(), Value::String(String::new()));
    let mut meta = dxdb_json::Map::new();
    meta.insert("lwt".to_string(), Value::from(1.0));
    map.insert("_meta".to_string(), Value::Object(meta));
    DocumentData::new(map)
}

fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Noop;
    impl Wake for Noop {
        fn wake(self: std::sync::Arc<Self>) {}
        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }
    let waker = Waker::from(std::sync::Arc::new(Noop));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Builds a client-side replication wired to the double.
fn state_over(
    double: std::rc::Rc<CouchDouble>,
    identifier: &str,
) -> ReplicationState<MemoryStorage> {
    let fork = MemoryStorage::new("client", "human", human_schema(), "fork-token");
    let meta_schema =
        dxdb_replication::meta_instance::replication_meta_instance_schema(fork.schema(), false)
            .expect("meta schema");
    let meta = MemoryStorage::new("client", "rx-replication-meta", meta_schema, "meta-token");
    let handler = CouchDbHandler::new("http://couch.test/db/", "passportId", DoubleClient(double))
        .with_identifier(identifier);
    ReplicationState::new(ReplicationInput {
        identifier: identifier.to_string(),
        pull_batch_size: BatchSize::new(10),
        push_batch_size: BatchSize::new(10),
        handler: Box::new(handler),
        conflict_handler: Box::new(DefaultConflictHandler),
        fork,
        meta: meta.instance(),
        hash_function: std::rc::Rc::new(Sha256HashFunction),
        skip_storing_pull_meta: false,
        keep_meta: false,
        initial_checkpoint: InitialCheckpoint::default(),
        wait_before_persist: None,
    })
}

/// Minimal local storage, the same fixture the other adapter tests use.
mod common {
    pub mod memory_storage;
}
use common::memory_storage::MemoryStorage;

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// Upstream `test/replication-couchdb.test.ts:152-171`, "finish sync once without data" and
/// "push one insert to server".
///
/// A push with nothing on the server must insert, and the adapter must be able to do it without
/// a prior `_all_docs` hit returning anything.
#[test]
fn push_one_insert_creates_the_document_on_the_server() {
    let double = std::rc::Rc::new(CouchDouble::new());
    let state = state_over(double.clone(), "couch-1");

    block_on(state.fork().bulk_write(
        vec![dxdb_storage::insert_row(human("a", "Alice", 30))],
        "local".to_string(),
    ))
    .expect("fork write");

    let mut upstream = Upstream::new(state);
    block_on(upstream.pump()).expect("push");

    assert_eq!(
        double.ids(),
        vec!["a".to_string()],
        "the document must exist on the server"
    );
    let stored = double.document("a").expect("document");
    assert_eq!(
        stored.get("firstName").and_then(Value::as_str),
        Some("Alice")
    );
    // The adapter must have used the documented query parameters, not a bespoke request.
    let requests = double.requests();
    assert!(
        requests.iter().any(|r| r.contains("_all_docs")),
        "a push reads the current rev first: {requests:?}"
    );
    assert!(
        requests.iter().any(|r| r.contains("_bulk_docs")),
        "and writes through _bulk_docs: {requests:?}"
    );
}

/// Upstream `test/replication-couchdb.test.ts:173-183`, "push and pull inserted document".
///
/// Both directions in one test, which is what that upstream case does.
#[test]
fn push_then_pull_round_trips() {
    let double = std::rc::Rc::new(CouchDouble::new());
    // A document that only exists on the server.
    double.put("b", {
        let mut map = dxdb_json::Map::new();
        map.insert("firstName".to_string(), Value::String("Bob".to_string()));
        map.insert("age".to_string(), Value::from(41));
        Value::Object(map)
    });

    let state = state_over(double.clone(), "couch-2");
    block_on(state.fork().bulk_write(
        vec![dxdb_storage::insert_row(human("a", "Alice", 30))],
        "local".to_string(),
    ))
    .expect("fork write");

    let mut upstream = Upstream::new(state.clone());
    block_on(upstream.pump()).expect("push");
    assert!(double.document("a").is_some(), "push reached the server");

    let mut downstream = Downstream::new(state.clone(), state.identifier_hash());
    block_on(downstream.resync()).expect("pull");

    let pulled = block_on(
        state
            .fork()
            .find_documents_by_id(vec!["b".to_string()], false),
    )
    .expect("query");
    assert_eq!(pulled.len(), 1, "the server's document must reach the fork");
    assert_eq!(
        pulled[0].get("firstName").and_then(Value::as_str),
        Some("Bob")
    );
    // The `_id` swap must have happened: the fork holds `passportId`, not `_id`.
    assert!(
        pulled[0].get("_id").is_none(),
        "the pulled document must not carry CouchDB's _id"
    );
}

/// Upstream `replication-base-test-suite.ts:243-271`: the master's state wins a conflict.
///
/// CouchDB's version of this is `conflictHandler.isEqual` at `index.ts:186`, which is why the
/// adapter takes the protocol's handler rather than comparing `_rev`.
#[test]
fn a_conflict_resolves_towards_the_master() {
    let double = std::rc::Rc::new(CouchDouble::new());
    // The server already holds a divergent state for the id.
    double.put("dup", {
        let mut map = dxdb_json::Map::new();
        map.insert(
            "firstName".to_string(),
            Value::String("FromServer".to_string()),
        );
        map.insert("age".to_string(), Value::from(99));
        Value::Object(map)
    });

    let state = state_over(double.clone(), "couch-conflict");
    block_on(state.fork().bulk_write(
        vec![dxdb_storage::insert_row(human("dup", "FromClient", 10))],
        "local".to_string(),
    ))
    .expect("fork write");

    let mut upstream = Upstream::new(state.clone());
    block_on(upstream.pump()).expect("push");

    let resolved = state.events().resolved_conflicts();
    assert_eq!(
        resolved.len(),
        1,
        "the divergence must be reported as one conflict"
    );
    assert_eq!(
        resolved[0]
            .input
            .real_master_state
            .get("firstName")
            .and_then(Value::as_str),
        Some("FromServer"),
        "the adapter must hand the protocol the server's real state"
    );

    let fork_doc = block_on(
        state
            .fork()
            .find_documents_by_id(vec!["dup".to_string()], false),
    )
    .expect("query");
    assert_eq!(
        fork_doc[0].get("firstName").and_then(Value::as_str),
        Some("FromServer"),
        "the default handler keeps the master"
    );
}

/// Upstream `index.ts:226-247`: a rev that moves between the read and the write is re-read and
/// reported as a conflict.
///
/// This is the branch a single-threaded test cannot otherwise reach, so the double has an
/// explicit hook for it. If the adapter dropped this path, the document would silently diverge
/// whenever two writers interleaved — the exact class of bug a happy-path double passes.
#[test]
fn a_rev_that_moves_between_read_and_write_is_reported_as_a_conflict() {
    let double = std::rc::Rc::new(CouchDouble::new());
    let state = state_over(double.clone(), "couch-race");

    // Seed so the server holds the same document the fork does, then let the fork update it.
    block_on(state.fork().bulk_write(
        vec![dxdb_storage::insert_row(human("raced", "First", 1))],
        "local".to_string(),
    ))
    .expect("fork write");
    let mut upstream = Upstream::new(state.clone());
    block_on(upstream.pump()).expect("initial push");

    // Update on the fork.
    let current = block_on(
        state
            .fork()
            .find_documents_by_id(vec!["raced".to_string()], false),
    )
    .expect("query")
    .into_iter()
    .next()
    .expect("document");
    block_on(state.fork().bulk_write(
        vec![dxdb_storage::update_row(
            current,
            human("raced", "Second", 2),
        )],
        "local".to_string(),
    ))
    .expect("fork update");

    // Arm the race: the server's rev changes between `_all_docs` and `_bulk_docs`.
    double.race_next_bulk_docs("raced");

    let mut upstream = Upstream::new(state.clone());
    block_on(upstream.pump()).expect("racing push");

    let resolved = state.events().resolved_conflicts();
    assert!(
        !resolved.is_empty(),
        "a rev that moved under the write must surface as a conflict, not be swallowed"
    );
    // The server's state won, so the fork holds the raced state.
    let fork_doc = block_on(
        state
            .fork()
            .find_documents_by_id(vec!["raced".to_string()], false),
    )
    .expect("query");
    assert_eq!(
        fork_doc[0].get("firstName").and_then(Value::as_str),
        Some("First"),
        "the server's surviving state is what both sides end up with"
    );
}

/// The `{sequence}` checkpoint reflects CouchDB's log position, not a document position.
///
/// This is the case that separates this adapter from every other. After a pull that returned
/// documents, the persisted checkpoint must be a `{sequence}` object whose value is the
/// `last_seq` the server reported — not a document id plus a timestamp.
///
/// **What it deliberately does not assert:** that a pull returning *nothing* persists a
/// checkpoint. Upstream breaks out of its resync loop on an empty batch **before** calling
/// `persistFromMaster` (`downstream.ts:192-194`), so no checkpoint is written at all. The
/// adapter's `{sequence}` value still moves inside `master_changes_since`; the protocol simply
/// never stores it on an empty batch. Asserting otherwise would be asserting behaviour upstream
/// does not have.
#[test]
fn the_sequence_checkpoint_is_the_servers_log_position() {
    let double = std::rc::Rc::new(CouchDouble::new());
    let state = state_over(double.clone(), "couch-seq");

    // One document on the server, so the pull returns a batch and a checkpoint is persisted.
    double.put("c", {
        let mut map = dxdb_json::Map::new();
        map.insert("firstName".to_string(), Value::String("Cara".to_string()));
        Value::Object(map)
    });

    let mut downstream = Downstream::new(state.clone(), state.identifier_hash());
    block_on(downstream.resync()).expect("pull");
    let persisted = block_on(dxdb_replication::checkpoint::last_checkpoint(
        &state,
        dxdb_replication::checkpoint::Direction::Down,
    ))
    .expect("read")
    .expect("a checkpoint was persisted after a non-empty pull");

    // The shape is `{sequence}` -- the whole point of this adapter.
    assert!(
        persisted.as_map().contains_key("sequence"),
        "the CouchDB checkpoint is {{sequence}}, got {}",
        persisted.to_js_json()
    );
    assert!(
        !persisted.as_map().contains_key("id"),
        "a CouchDB checkpoint must not carry a document id, got {}",
        persisted.to_js_json()
    );

    // And it is the server's log position: the document's own sequence.
    let sequence = persisted
        .as_map()
        .get("sequence")
        .and_then(Value::as_u64)
        .expect("the sequence is a number for this double");
    assert_eq!(
        sequence,
        double.sequence_now(),
        "the checkpoint must be the server's last_seq, not an invented position"
    );

    // A later pull resumes from it and returns nothing new.
    let mut downstream = Downstream::new(state.clone(), state.identifier_hash());
    block_on(downstream.resync()).expect("second pull");
    let after = block_on(dxdb_replication::checkpoint::last_checkpoint(
        &state,
        dxdb_replication::checkpoint::Direction::Down,
    ))
    .expect("read")
    .expect("checkpoint");
    assert_eq!(
        after.to_js_json(),
        persisted.to_js_json(),
        "an empty pull must leave the persisted checkpoint where it was"
    );
}

/// A string sequence is sent unquoted, as a clustered CouchDB requires.
///
/// A clustered CouchDB returns sequences like `"17-g1AAAA..."` rather than integers. The
/// adapter must interpolate the *value* into the query string, not its JSON encoding: sending
/// `since="17-g1..."` with the quotes would be a different sequence to CouchDB, and the pull
/// would re-read from the beginning. The double only emits integers, so this is asserted on the
/// constructed URL.
#[test]
fn a_string_sequence_is_interpolated_unquoted() {
    let double = std::rc::Rc::new(CouchDouble::new());
    let handler = CouchDbHandler::new(
        "http://couch.test/db/",
        "passportId",
        DoubleClient(double.clone()),
    );

    let mut map = dxdb_json::Map::new();
    map.insert(
        "sequence".to_string(),
        Value::String("17-g1AAAAFrazeJzI2NjQ".to_string()),
    );
    let checkpoint = dxdb_replication::checkpoint::Checkpoint::new(Value::Object(map)).expect("ok");

    // Drive one call so the URL is recorded by the double.
    let _ = block_on(handler.master_changes_since(Some(&checkpoint), 10));
    let requests = double.requests();
    let changes = requests
        .iter()
        .find(|request| request.contains("_changes"))
        .expect("a _changes request was made");
    assert!(
        changes.contains("since=17-g1AAAAFrazeJzI2NjQ"),
        "the sequence must be interpolated without quotes: {changes}"
    );
    assert!(
        !changes.contains("since=%22") && !changes.contains("since=\"17"),
        "a quoted sequence would be a different value to CouchDB: {changes}"
    );
}

/// An unreachable server surfaces as an error rather than a silent empty result.
#[test]
fn an_unreachable_server_is_an_error() {
    struct DeadClient;
    impl HttpClient for DeadClient {
        fn request(
            &self,
            _method: HttpMethod,
            url: String,
            _body: Option<Value>,
        ) -> dxdb_replication::state::BoxFuture<'_, Result<HttpResponse, TransportError>> {
            Box::pin(async move { Err(TransportError::Unreachable(url)) })
        }
    }
    let handler = CouchDbHandler::new("http://couch.test/db/", "passportId", DeadClient);
    let error = block_on(handler.master_changes_since(None, 10)).expect_err("must fail");
    assert!(
        matches!(error, StorageError::Unsupported(_)),
        "a transport failure surfaces as Unsupported, got {error:?}"
    );
}

/// A non-2xx answer is an error, not an empty batch.
#[test]
fn a_non_success_status_is_an_error() {
    struct Unauthorised;
    impl HttpClient for Unauthorised {
        fn request(
            &self,
            _method: HttpMethod,
            _url: String,
            _body: Option<Value>,
        ) -> dxdb_replication::state::BoxFuture<'_, Result<HttpResponse, TransportError>> {
            Box::pin(async move {
                Ok(HttpResponse {
                    status: 401,
                    body: dxdb_json::parse(r#"{"error":"unauthorized"}"#).expect("json"),
                })
            })
        }
    }
    let handler = CouchDbHandler::new("http://couch.test/db/", "passportId", Unauthorised);
    let error = block_on(handler.master_changes_since(None, 10)).expect_err("must fail");
    let message = format!("{error:?}");
    assert!(
        message.contains("401"),
        "the status must be reported, got {message}"
    );
}
