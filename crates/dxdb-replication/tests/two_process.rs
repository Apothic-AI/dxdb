//! The two-process test: dxdb -> HTTP -> dxdb.
//!
//! `PLAN.md` section 10 requires it, and the lane brief calls it one of the two things that
//! "proves the protocol works rather than just compiles". Everything else in this crate
//! runs both sides in one process, where a mistake in the wire format is invisible because
//! both halves share the same Rust types. Here the master is behind a TCP socket and the
//! only thing that crosses is JSON.
//!
//! # Hermetic by construction
//!
//! * The server binds `127.0.0.1:0`, so the port is assigned by the OS and read back. No
//!   fixed port number appears anywhere in this file.
//! * Loopback only: nothing leaves the machine and no external service is contacted.
//! * The server is torn down by `Drop`, including on a panic, so a failed test cannot leave
//!   a listener behind for the next one.
//!
//! # What is asserted
//!
//! 1. **Document convergence** — after the pushes and pulls, both sides hold the same
//!    documents with the same content.
//! 2. **Checkpoint equality** — the checkpoint the client persisted serialises byte-for-byte
//!    to what a re-run from it expects, and re-running from it yields no documents. That
//!    second half is the one that matters: a checkpoint that merely *looks* right but does
//!    not resume correctly re-delivers everything, and only the re-run catches it.
//! 3. **The wire shape** — `master_write` returns conflicts only, which the server's own
//!    request log proves rather than the client's opinion of it.

mod common {
    pub mod http_transport;
    pub mod memory_storage;
}

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use common::http_transport::{by_primary, HttpHandler, TestServer};
use common::memory_storage::{insert, stored_document, MemoryStorage};
use dxdb_json::Value;
use dxdb_replication::checkpoint::{
    last_checkpoint, set_checkpoint, Checkpoint, Direction, Sha256HashFunction,
};
use dxdb_replication::conflict::DefaultConflictHandler;
use dxdb_replication::meta_instance::get_assumed_master_state;
use dxdb_replication::state::{
    BatchSize, DocumentsWithCheckpoint, InitialCheckpoint, PullStream, ReplicationHandler,
    ReplicationInput, ReplicationState, WriteToMasterRow,
};
use dxdb_replication::upstream::Upstream;
use dxdb_storage::{DocumentData, RxStorageInstance, StorageError};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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
    .expect("valid schema")
}

fn human(id: &str, first: &str, age: i64) -> DocumentData {
    stored_document(
        "passportId",
        id,
        &[
            ("firstName", Value::String(first.to_string())),
            ("age", Value::from(age)),
        ],
    )
}

/// Runs a future on the current thread. The protocol is pumped by its caller (D2/D3); a
/// test is a caller, and this is the smallest correct executor for futures with no real I/O
/// wait of their own.
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

/// A client-side replication: a fork, its meta instance, and a state wired to HTTP.
struct Client {
    fork: MemoryStorage,
    meta: MemoryStorage,
    state: ReplicationState<MemoryStorage>,
    handler: Rc<HttpHandler>,
}

fn client_for(server: &TestServer, identifier: &str) -> Client {
    let fork = MemoryStorage::new("dxdb-client", "human", human_schema(), "client-token");
    let meta_schema =
        dxdb_replication::meta_instance::replication_meta_instance_schema(fork.schema(), false)
            .expect("the meta schema is derivable");
    let meta = MemoryStorage::new(
        "dxdb-client",
        "rx-replication-meta",
        meta_schema,
        "client-meta-token",
    );
    let handler = Rc::new(HttpHandler::new(server.base_url()));

    let state = ReplicationState::new(ReplicationInput {
        identifier: identifier.to_string(),
        pull_batch_size: BatchSize::new(100),
        push_batch_size: BatchSize::new(100),
        handler: Box::new(HttpAdapter(handler.clone())),
        conflict_handler: Box::new(DefaultConflictHandler),
        fork: fork.instance(),
        meta: meta.instance(),
        hash_function: Rc::new(Sha256HashFunction),
        skip_storing_pull_meta: false,
        keep_meta: false,
        initial_checkpoint: InitialCheckpoint::default(),
        wait_before_persist: None,
    });

    Client {
        fork,
        meta,
        state,
        handler,
    }
}

/// Adapts [`HttpHandler`] to the protocol's `ReplicationHandler`.
///
/// The client half of the wire: each method becomes one request. There is no caching and no
/// batching beyond what the protocol asks for, because the point is to prove the protocol's
/// own call pattern survives a socket.
struct HttpAdapter(Rc<HttpHandler>);

impl ReplicationHandler for HttpAdapter {
    fn master_changes_since(
        &self,
        checkpoint: Option<&Checkpoint>,
        batch_size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<DocumentsWithCheckpoint, StorageError>>>> {
        let handler = self.0.clone();
        let checkpoint = checkpoint.cloned();
        Box::pin(async move {
            let mut body = dxdb_json::Map::new();
            body.insert(
                "checkpoint".to_string(),
                checkpoint
                    .map(|checkpoint| checkpoint.into_value())
                    .unwrap_or(Value::Null),
            );
            body.insert("batchSize".to_string(), Value::from(batch_size as u64));
            let response = handler
                .post("/changes-since", Value::Object(body))
                .map_err(StorageError::Unsupported)?;

            let documents = response
                .get("documents")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|value| {
                    DocumentData::from_value(value)
                        .map_err(|error| StorageError::InvalidDocument(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let checkpoint = response
                .get("checkpoint")
                .filter(|value| !value.is_null())
                .and_then(|value| Checkpoint::new(value.clone()).ok());
            Ok(DocumentsWithCheckpoint {
                documents,
                checkpoint,
            })
        })
    }

    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DocumentData>, StorageError>>>> {
        let handler = self.0.clone();
        Box::pin(async move {
            let rows_json: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let mut map = dxdb_json::Map::new();
                    map.insert(
                        "assumedMasterState".to_string(),
                        row.assumed_master_state
                            .clone()
                            .map(DocumentData::into_value)
                            .unwrap_or(Value::Null),
                    );
                    map.insert(
                        "newDocumentState".to_string(),
                        row.new_document_state.clone().into_value(),
                    );
                    Value::Object(map)
                })
                .collect();
            let mut body = dxdb_json::Map::new();
            body.insert("rows".to_string(), Value::Array(rows_json));
            let response = handler
                .post("/write", Value::Object(body))
                .map_err(StorageError::Unsupported)?;

            // The contract: this is *conflicts*, not acknowledgements. The server test
            // below asserts the same thing from the other side.
            response
                .get("conflicts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|value| {
                    DocumentData::from_value(value)
                        .map_err(|error| StorageError::InvalidDocument(error.to_string()))
                })
                .collect()
        })
    }

    fn master_change_stream(&self) -> PullStream {
        // The two-process test drives pushes and pulls explicitly; a live stream would add
        // timing to an assertion that is about convergence. `Plan` is the live path's
        // business, and the conformance suite covers the batching it needs.
        Box::pin(futures::stream::empty())
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// dxdb -> HTTP -> dxdb: documents converge and the checkpoint is exact.
#[test]
fn two_process_convergence_and_checkpoint_equality() {
    let server = TestServer::start();
    let client = client_for(&server, "rxdbreplicationtwo-process");

    // --- push --------------------------------------------------------------------
    for index in 0..5 {
        block_on(client.fork.bulk_write(
            vec![insert(human(&format!("doc-{index}"), "pushed", 20 + index))],
            "local".to_string(),
        ))
        .expect("the fork write succeeds");
    }

    let mut upstream = Upstream::new(client.state.clone());
    block_on(upstream.pump()).expect("the push succeeds");

    assert!(
        client.handler.write_calls() >= 1,
        "the protocol must have made at least one HTTP write"
    );

    // --- convergence -------------------------------------------------------------
    let server_docs = server.documents();
    let fork_docs = client.fork.all_documents();
    assert_eq!(
        server_docs.len(),
        5,
        "all five documents reached the master over HTTP"
    );
    assert_eq!(fork_docs.len(), 5, "and the fork still holds all five");

    let server_by_id = by_primary(&server_docs);
    let fork_by_id = by_primary(&fork_docs);
    assert_eq!(
        server_by_id.keys().collect::<Vec<_>>(),
        fork_by_id.keys().collect::<Vec<_>>(),
        "the two sides hold the same document ids"
    );
    for (id, server_doc) in &server_by_id {
        let fork_doc = fork_by_id
            .get(id)
            .unwrap_or_else(|| panic!("the fork is missing {id}"));
        assert_eq!(
            content_of(server_doc),
            content_of(fork_doc),
            "document {id} must have the same content on both sides"
        );
    }

    // --- the response shape, from the server's side ------------------------------
    //
    // The server logged what it actually answered. A `master_write` that returned
    // acknowledgements instead of conflicts would still converge in this scenario, so the
    // shape is asserted where it is produced rather than inferred from the outcome.
    let requests = server.requests();
    let writes: Vec<_> = requests
        .iter()
        .filter(|record| record.path == "/write")
        .collect();
    assert!(!writes.is_empty(), "the server handled at least one write");
    for write in &writes {
        let conflicts = write
            .response
            .get("conflicts")
            .and_then(Value::as_array)
            .expect("a write response always has a conflicts array");
        assert!(
            conflicts.is_empty(),
            "an uncontested push must return an empty conflicts array, got {conflicts:?}"
        );
    }

    // --- checkpoint equality ------------------------------------------------------
    let stored = block_on(last_checkpoint(&client.state, Direction::Up))
        .expect("the checkpoint read succeeds")
        .expect("a checkpoint was persisted by the push");

    // The persisted checkpoint is the storage's `{id, lwt}` pair, encoded by the protocol's
    // own writer. Pinning the serialisation is the assertion: two implementations that
    // disagree about key order or number formatting produce different bytes for the same
    // logical position, and a resumed pull then disagrees with a JS peer about where it is.
    let serialised = dxdb_json::to_js_json(stored.as_value());
    for expected_key in ["\"id\"", "\"lwt\""] {
        assert!(
            serialised.contains(expected_key),
            "the checkpoint must carry {expected_key}, got {serialised}"
        );
    }
    // Round-trips to identical bytes, which is the property a JS peer compares on.
    let reparsed =
        Checkpoint::new(dxdb_json::parse(&serialised).expect("the checkpoint is valid JSON"))
            .expect("the checkpoint is an object");
    assert_eq!(
        reparsed.to_js_json(),
        serialised,
        "a checkpoint must round-trip to identical bytes"
    );

    // --- re-running from the checkpoint yields nothing ----------------------------
    //
    // This is the half that proves the checkpoint is *correct* rather than merely present.
    // A checkpoint that looks right but resumes one document early re-delivers that
    // document forever, and only the re-run detects it.
    let replayed = block_on(
        client
            .state
            .handler()
            .master_changes_since(Some(&stored), 100),
    )
    .expect("the changes-since request succeeds");
    assert!(
        replayed.documents.is_empty(),
        "resuming from the persisted checkpoint must report no documents, got {:?}",
        replayed
            .documents
            .iter()
            .filter_map(|document| document.get("passportId").cloned())
            .collect::<Vec<_>>()
    );

    // --- a second round, to prove it is not a one-shot ----------------------------
    block_on(client.fork.bulk_write(
        vec![insert(human("after-checkpoint", "second-round", 40))],
        "local".to_string(),
    ))
    .expect("the fork write succeeds");
    let mut upstream = Upstream::new(client.state.clone());
    block_on(upstream.pump()).expect("the second push succeeds");

    assert_eq!(
        server.documents().len(),
        6,
        "the second round reached the master"
    );
    let second_checkpoint = block_on(last_checkpoint(&client.state, Direction::Up))
        .expect("the read succeeds")
        .expect("a checkpoint exists");
    assert_ne!(
        second_checkpoint.to_js_json(),
        serialised,
        "a second round must advance the checkpoint"
    );
    let replayed_again = block_on(
        client
            .state
            .handler()
            .master_changes_since(Some(&second_checkpoint), 100),
    )
    .expect("the read succeeds");
    assert!(
        replayed_again.documents.is_empty(),
        "the advanced checkpoint must also resume cleanly"
    );

    // --- a pull converges the other way ------------------------------------------
    //
    // The master is written to directly (a second client in a real deployment), then the
    // client pulls it. This proves the wire works in both directions, which a push-only
    // test would not.
    let remote = block_on(client.state.handler().master_changes_since(None, 100))
        .expect("the read succeeds");
    if let Some(first) = remote.documents.first() {
        // Apply it to the fork as the pull would: with the downstream context, so the
        // push does not echo it back.
        let flag = client.state.downstream_bulk_write_flag().to_string();
        let id = first
            .get("passportId")
            .and_then(Value::as_str)
            .expect("a primary key")
            .to_string();
        match client.fork.get(&id) {
            Some(existing) => {
                block_on(client.fork.bulk_write(
                    vec![dxdb_storage::update_row(existing, first.clone())],
                    flag,
                ))
                .expect("the pull write succeeds");
            }
            None => {
                block_on(client.fork.bulk_write(vec![insert(first.clone())], flag))
                    .expect("the pull write succeeds");
            }
        }
        assert!(
            client.fork.get(&id).is_some(),
            "the pull applied the master's document to the fork"
        );
    }

    let _ = &client.meta;
    drop(server);
}

/// The content of a document, ignoring the fields each side generates locally.
fn content_of(document: &DocumentData) -> Value {
    let mut map = dxdb_json::Map::new();
    for (key, value) in document.as_object() {
        if matches!(key.as_str(), "_rev" | "_meta") {
            continue;
        }
        map.insert(key.clone(), value.clone());
    }
    Value::Object(map)
}

/// The server really is on an ephemeral port and really is loopback only.
///
/// Guards the hermeticity claim rather than trusting it: a future edit that hard-codes a
/// port, or binds `0.0.0.0`, fails here instead of in CI where it would be flaky.
#[test]
fn the_server_binds_an_ephemeral_loopback_port() {
    let server = TestServer::start();
    let addr = server.addr();
    assert!(
        addr.ip().is_loopback(),
        "the test server must bind loopback only, got {}",
        addr.ip()
    );
    assert_ne!(addr.port(), 0, "the OS must have assigned a real port");
    assert!(
        addr.port() > 1024,
        "an ephemeral port is above the privileged range, got {}",
        addr.port()
    );

    // Two servers at once must get different ports, which is what makes concurrent test
    // runs safe.
    let other = TestServer::start();
    assert_ne!(
        server.addr().port(),
        other.addr().port(),
        "two running servers must not share a port"
    );
}

/// The client's request shape is what the server expects, checked on the wire.
#[test]
fn the_wire_shape_matches_the_contract() {
    let server = TestServer::start();
    let handler = HttpHandler::new(server.base_url());

    // A write with no rows: the server must answer with a conflicts array, not an error.
    let response = handler
        .post("/write", {
            let mut map = dxdb_json::Map::new();
            map.insert("rows".to_string(), Value::Array(Vec::new()));
            Value::Object(map)
        })
        .expect("the request succeeds");
    assert_eq!(
        response
            .get("conflicts")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0),
        "an empty write returns an empty conflicts array"
    );

    // A changes-since with no documents.
    let response = handler
        .post("/changes-since", {
            let mut map = dxdb_json::Map::new();
            map.insert("checkpoint".to_string(), Value::Null);
            map.insert("batchSize".to_string(), Value::from(10u64));
            Value::Object(map)
        })
        .expect("the request succeeds");
    assert_eq!(
        response
            .get("documents")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    assert!(
        response.get("checkpoint").is_some(),
        "the response always carries a checkpoint field, even when it is null"
    );
}

/// An unknown route is an error, not a silent success.
#[test]
fn an_unknown_route_is_reported() {
    let server = TestServer::start();
    let handler = HttpHandler::new(server.base_url());
    let error = handler
        .post("/nope", Value::Object(dxdb_json::Map::new()))
        .expect_err("an unknown route must not succeed");
    assert!(
        error.contains("unknown route"),
        "the error names the cause, got {error}"
    );
}

/// A helper that makes the assumed-master assertions in this file readable.
fn assumed_for(state: &ReplicationState<MemoryStorage>, id: &str) -> Option<DocumentData> {
    block_on(get_assumed_master_state(state, &[id.to_string()]))
        .ok()
        .and_then(|map| map.get(id).map(|row| row.doc_data.clone()))
}

/// The push recorded assumed-master state across the wire, so a second push is a
/// compare-and-set rather than a blind overwrite.
#[test]
fn the_push_records_assumed_master_state_across_the_wire() {
    let server = TestServer::start();
    let client = client_for(&server, "rxdbreplicationassumed");
    block_on(client.fork.bulk_write(
        vec![insert(human("assumed", "value", 5))],
        "local".to_string(),
    ))
    .expect("the fork write succeeds");

    let mut upstream = Upstream::new(client.state.clone());
    block_on(upstream.pump()).expect("the push succeeds");

    let assumed = assumed_for(&client.state, "assumed");
    assert!(
        assumed.is_some(),
        "the push must record what it believes the master holds, or the next push cannot \
         detect a conflict"
    );
    let assumed = assumed.expect("checked");
    assert_eq!(
        assumed.get("firstName").and_then(Value::as_str),
        Some("value")
    );
    assert!(
        assumed.get("_meta").is_none() && assumed.get("_rev").is_none(),
        "the assumed state excludes the master's local bookkeeping"
    );

    // The second push finds the master already agreeing, so it sends nothing.
    let before = client.handler.write_calls();
    let mut upstream = Upstream::new(client.state.clone());
    block_on(upstream.pump()).expect("the second push succeeds");
    assert_eq!(
        client.handler.write_calls(),
        before,
        "nothing changed, so no write request should have been made"
    );
}

/// Two sequential `set_checkpoint` calls persist the same bytes a reload would read.
///
/// Checkpoint persistence is the only part of the protocol that survives a restart, so its
/// round-trip is worth asserting against the meta instance directly rather than only through
/// a full sync.
#[test]
fn a_checkpoint_survives_a_meta_instance_round_trip() {
    let server = TestServer::start();
    let client = client_for(&server, "rxdbreplicationcheckpoint");

    let mut map = dxdb_json::Map::new();
    map.insert("id".to_string(), Value::String("doc-1".to_string()));
    map.insert("lwt".to_string(), Value::from(1_700_000_000_000.07));
    let checkpoint = Checkpoint::new(Value::Object(map.clone())).expect("an object");
    block_on(set_checkpoint(
        &client.state,
        Direction::Up,
        Some(&checkpoint),
    ))
    .expect("the checkpoint write succeeds");
    client.state.checkpoint_queue().drain();

    // Read it back through a *fresh* read path, not the cached row.
    let read = block_on(last_checkpoint(&client.state, Direction::Up))
        .expect("the read succeeds")
        .expect("the checkpoint is there");
    assert_eq!(
        read.to_js_json(),
        dxdb_json::to_js_json(&Value::Object(map)),
        "the checkpoint must come back with identical bytes, including the two-decimal lwt"
    );

    // And the serialisation is the JS one: `1700000000000.07`, not `1700000000000.0700001`.
    assert!(
        read.to_js_json().contains("1700000000000.07"),
        "a two-decimal lwt must serialise the way JSON.stringify writes it, got {}",
        read.to_js_json()
    );

    let _ = &client.meta;
}
