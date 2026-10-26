//! The GraphQL adapter against a local double.
//!
//! Upstream has no GraphQL server of its own — `replicateGraphQL` is a client for *your* server
//! (`plugins/replication-graphql/index.ts:90`), and its schema/query builders only help you
//! author one. So there is no upstream test to cite beyond the shared suite's shapes, and this
//! file is explicit about that: it tests the *adapter's* contract, which is the part that has to
//! match upstream.
//!
//! # The point of this adapter: the checkpoint is the caller's
//!
//! Every other adapter in this crate has a checkpoint shape it chose. GraphQL has none: upstream
//! hands the previous checkpoint to the caller's `queryBuilder` and takes whatever the caller's
//! resolver returns at `data.checkpoint` (`index.ts:124-153`). So these tests use **three
//! different checkpoint shapes** across cases — `{updatedAt}`, `{offset}`, `{cursor}` — and the
//! adapter is unchanged for all three. That is D7 being exercised rather than asserted.
//!
//! # What the double models
//!
//! * **`errors` inside a 200.** The defining GraphQL behaviour: a resolver failure arrives with
//!   status 200 and an `errors` array (`helper.ts:40-48`, checked at `index.ts:129-131`). The
//!   double can be armed to return one, and a case asserts the adapter fails rather than
//!   returning an empty batch.
//! * **A configurable root field.** The default `dataPath` reaches for
//!   `Object.keys(data)[0]` (`helper.ts:44-45`), so the double's root field is deliberately
//!   *not* named after the collection; a case asserts the default finds it anyway.
//! * **An explicit path into a nested array**, because `getProperty` accepts a numeric element
//!   (`helper.ts:47`).
//! * **Caller-defined checkpoints**, as above.
//! * **A push payload as a conflicts array**, and as `{conflicts: [...]}` (the shape this
//!   crate's other adapters use).
//!
//! # What it does NOT model
//!
//! * **graphql-ws subscriptions** (`graphql-websocket.ts`, 62 lines). The adapter's change
//!   stream is empty; live behaviour is unported and untested.
//! * **A real GraphQL parser or schema.** The double matches on the query text's first
//!   meaningful token rather than parsing GraphQL. A malformed query would be handled by the
//!   real server, not here.
//! * **`credentials`/cookie auth**, and the schema/query builders (655 lines of server-authoring
//!   helpers), which are not client transport.
//! * **Partial errors** — a response with both `data` and `errors`, where GraphQL permits
//!   returning partially successful data. The adapter treats any errors array as a failure,
//!   which matches upstream (`index.ts:129-131`) but means a caller relying on partial success
//!   is not modelled.
//! * **Batching semantics beyond one request per call.**

mod common {
    pub mod memory_storage;
}

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use common::memory_storage::MemoryStorage;
use dxdb_json::Value;
use dxdb_replication::adapters::graphql::{
    GraphQlClient, GraphQlHandler, GraphQlRequest, PullQueryBuilder, PushQueryBuilder,
    ResponseDirection, ResponseModifier,
};
use dxdb_replication::adapters::TransportError;
use dxdb_replication::checkpoint::Sha256HashFunction;
use dxdb_replication::conflict::DefaultConflictHandler;
use dxdb_replication::downstream::Downstream;
use dxdb_replication::state::{
    BatchSize, InitialCheckpoint, ReplicationHandler, ReplicationInput, ReplicationState,
    WriteToMasterRow,
};
use dxdb_replication::upstream::Upstream;
use dxdb_storage::{DocumentData, RxStorageInstance, StorageError};

/// A GraphQL double: records requests, answers from a queue of canned responses.
#[derive(Default)]
struct GraphQlDouble {
    responses: RefCell<Vec<Value>>,
    /// Responses keyed by a substring of the query text.
    routed: RefCell<Vec<(String, Value)>>,
    requests: RefCell<Vec<Value>>,
}

impl GraphQlDouble {
    fn new() -> Rc<Self> {
        Rc::new(GraphQlDouble::default())
    }

    /// Queues the next response body.
    fn respond(&self, body: Value) {
        self.responses.borrow_mut().push(body);
    }

    /// Queues a response used only for requests whose query contains `needle`.
    ///
    /// A double that answers strictly from a queue breaks as soon as the protocol makes a call
    /// the test did not anticipate -- which is exactly what happened here: a push also triggers
    /// an initial-sync read. Routing by query kind makes the double behave like a server (it
    /// knows which resolver it is answering) instead of like a script.
    fn respond_to(&self, needle: &str, body: Value) {
        self.routed.borrow_mut().push((needle.to_string(), body));
    }

    /// The next answer for a body: a routed response if one matches, else the queue head.
    fn answer(&self, body: &Value) -> Option<Value> {
        let query = body
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // The `position` borrow must end before the `remove` borrow starts: holding both on
        // one RefCell panics at runtime ("RefCell already borrowed"), which is what happened
        // the first time this double was used for a two-call test.
        let index = self.routed.borrow().iter().position(|(needle, _)| {
            query
                .to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase())
        });
        if let Some(index) = index {
            return Some(self.routed.borrow_mut().remove(index).1);
        }
        if self.responses.borrow().is_empty() {
            // A push is retried until a pass reports no conflict writes
            // (`upstream.ts:176-186`), so "no conflicts" is the termination condition rather
            // than an empty queue. A server answers every push; the double does too.
            if query.to_ascii_lowercase().contains("pushhuman") {
                return Some(dxdb_json::parse(r#"{"data":{"pushHuman":[]}}"#).expect("valid json"));
            }
            None
        } else {
            Some(self.responses.borrow_mut().remove(0))
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.borrow().clone()
    }
}

/// Newtype: `Rc<GraphQlDouble>` is foreign, so the orphan rule needs a wrapper.
struct DoubleClient(Rc<GraphQlDouble>);

impl GraphQlClient for DoubleClient {
    fn post(
        &self,
        body: Value,
    ) -> dxdb_replication::state::BoxFuture<'_, Result<Value, TransportError>> {
        self.0.requests.borrow_mut().push(body.clone());
        let next = self.0.answer(&body);
        let query = body
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("<no query>")
            .to_string();
        Box::pin(async move {
            next.ok_or_else(|| {
                TransportError::Unreachable(format!(
                    "the double had no response queued for query: {query}"
                ))
            })
        })
    }
}

/// A pull builder that encodes the checkpoint as `{updatedAt}` — shape 1.
struct UpdatedAtPull;

impl PullQueryBuilder for UpdatedAtPull {
    fn build(&self, checkpoint: Option<&Value>, batch_size: usize) -> GraphQlRequest {
        let since = checkpoint
            .and_then(|checkpoint| checkpoint.get("updatedAt"))
            .cloned()
            .unwrap_or(Value::from(0u64));
        let mut variables = dxdb_json::Map::new();
        variables.insert("updatedAt".to_string(), since.clone());
        variables.insert("limit".to_string(), Value::from(batch_size as u64));
        GraphQlRequest::new(
            format!(
                "query PullHuman($updatedAt: Float!, $limit: Int!) {{ feedHuman(updatedAt: $updatedAt, limit: $limit) {{ documents checkpoint {{ updatedAt }} }} }} // since={}",
                dxdb_json::to_js_json(&since)
            ),
            Value::Object(variables),
        )
    }
}

/// A pull builder that encodes the checkpoint as `{offset}` — shape 2, to prove the adapter does
/// not assume anything about the key.
struct OffsetPull;

impl PullQueryBuilder for OffsetPull {
    fn build(&self, checkpoint: Option<&Value>, batch_size: usize) -> GraphQlRequest {
        let offset = checkpoint
            .and_then(|checkpoint| checkpoint.get("offset"))
            .cloned()
            .unwrap_or(Value::from(0u64));
        let mut variables = dxdb_json::Map::new();
        variables.insert("offset".to_string(), offset);
        variables.insert("limit".to_string(), Value::from(batch_size as u64));
        GraphQlRequest::new("query PullByOffset { feed }", Value::Object(variables))
    }
}

/// A push builder that wraps rows in the documented shape.
struct RowsPush;

impl PushQueryBuilder for RowsPush {
    fn build(&self, rows: &[WriteToMasterRow]) -> GraphQlRequest {
        let rows: Vec<Value> = rows
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
        let mut variables = dxdb_json::Map::new();
        variables.insert("rows".to_string(), Value::Array(rows));
        GraphQlRequest::new(
            "mutation PushHuman($rows: [HumanInput!]!) { pushHuman(rows: $rows) }",
            Value::Object(variables),
        )
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

fn state_with<H: ReplicationHandler + 'static>(
    handler: H,
    identifier: &str,
) -> ReplicationState<MemoryStorage> {
    let fork = MemoryStorage::new("client", "human", human_schema(), "fork-token");
    let meta_schema =
        dxdb_replication::meta_instance::replication_meta_instance_schema(fork.schema(), false)
            .expect("meta schema");
    let meta = MemoryStorage::new("client", "rx-replication-meta", meta_schema, "meta-token");
    ReplicationState::new(ReplicationInput {
        identifier: identifier.to_string(),
        pull_batch_size: BatchSize::new(10),
        push_batch_size: BatchSize::new(10),
        handler: Box::new(handler),
        conflict_handler: Box::new(DefaultConflictHandler),
        fork,
        meta: meta.instance(),
        hash_function: Rc::new(Sha256HashFunction),
        skip_storing_pull_meta: false,
        keep_meta: false,
        initial_checkpoint: InitialCheckpoint::default(),
        wait_before_persist: None,
    })
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// A pull works with a `{updatedAt}` checkpoint, and the checkpoint round-trips into the next
/// request's variables.
///
/// Cite: upstream's pull handler passes `lastPulledCheckpoint` to the caller's `queryBuilder` and
/// returns `data.checkpoint` as the new one (`index.ts:126-152`).
#[test]
fn a_pull_uses_a_caller_defined_updated_at_checkpoint() {
    let double = GraphQlDouble::new();
    // Root field name is deliberately not derived from the collection: the default dataPath
    // takes the first key, so this is what proves that default works (`helper.ts:44-45`).
    double.respond(dxdb_json::parse(r#"{"data":{"feedHuman":{"documents":[{"passportId":"a","firstName":"Alice","age":30,"_deleted":false,"_attachments":{},"_rev":"1-x"}],"checkpoint":{"updatedAt":1234}}}}"#).expect("json"));

    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        Some(UpdatedAtPull),
        None::<RowsPush>,
    );
    let state = state_with(handler, "graphql-pull-1");

    let mut downstream = Downstream::new(state.clone(), state.identifier_hash());
    block_on(downstream.resync()).expect("pull");

    let documents = block_on(
        state
            .fork()
            .find_documents_by_id(vec!["a".to_string()], false),
    )
    .expect("query");
    assert_eq!(documents.len(), 1, "the document must have been pulled");
    assert_eq!(
        documents[0].get("firstName").and_then(Value::as_str),
        Some("Alice")
    );

    // The persisted checkpoint is the server's `{updatedAt}` object, untouched.
    let checkpoint = block_on(dxdb_replication::checkpoint::last_checkpoint(
        &state,
        dxdb_replication::checkpoint::Direction::Down,
    ))
    .expect("read")
    .expect("a checkpoint was persisted");
    assert_eq!(
        checkpoint.to_js_json(),
        r#"{"updatedAt":1234}"#,
        "the adapter must persist the caller's checkpoint verbatim"
    );
}

/// The *same adapter* works with an `{offset}` checkpoint, unchanged.
///
/// This is the case that makes the point: no code in `graphql.rs` mentions `updatedAt` or
/// `offset`, and both work. If the adapter had a checkpoint shape, one of these would fail.
#[test]
fn the_same_adapter_works_with_an_offset_checkpoint() {
    let double = GraphQlDouble::new();
    double.respond(dxdb_json::parse(r#"{"data":{"feed":{"documents":[{"passportId":"b","firstName":"Bob","age":41,"_deleted":false,"_attachments":{},"_rev":"1-y"}],"checkpoint":{"offset":7}}}}"#).expect("json"));

    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        Some(OffsetPull),
        None::<RowsPush>,
    );
    let state = state_with(handler, "graphql-pull-offset");

    let mut downstream = Downstream::new(state.clone(), state.identifier_hash());
    block_on(downstream.resync()).expect("pull");

    let checkpoint = block_on(dxdb_replication::checkpoint::last_checkpoint(
        &state,
        dxdb_replication::checkpoint::Direction::Down,
    ))
    .expect("read")
    .expect("checkpoint");
    assert_eq!(
        checkpoint.to_js_json(),
        r#"{"offset":7}"#,
        "an offset checkpoint must round-trip exactly like an updatedAt one"
    );
}

/// An `errors` array inside a 200 fails the pull instead of reading as an empty batch.
///
/// The distinguishing GraphQL behaviour (`index.ts:129-131`). Without this check a resolver
/// failure looks like "no documents", and a replication silently stops advancing.
#[test]
fn graphql_errors_in_a_200_fail_the_pull() {
    let double = GraphQlDouble::new();
    double.respond(
        dxdb_json::parse(
            r#"{"errors":[{"message":"Field 'feedHuman' is not defined"}],"data":null}"#,
        )
        .expect("json"),
    );

    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        Some(UpdatedAtPull),
        None::<RowsPush>,
    );
    let state = state_with(handler, "graphql-errors");
    let mut downstream = Downstream::new(state.clone(), state.identifier_hash());

    let error = block_on(downstream.resync()).expect_err("an errors array must fail the pull");
    let message = format!("{error:?}");
    assert!(
        message.contains("not defined"),
        "the server's message must survive, got {message}"
    );
    assert!(
        message.contains("errors"),
        "the failure must name the errors array, got {message}"
    );
}

/// A push sends the caller's mutation and returns the server's conflicts.
///
/// Cite: `index.ts:159-172`, the push handler's shape.
#[test]
fn a_push_sends_the_mutation_and_reads_conflicts() {
    let double = GraphQlDouble::new();
    // The push resolver reports one conflict.
    double.respond_to(
        "pushHuman",
        dxdb_json::parse(r#"{"data":{"pushHuman":[{"passportId":"dup","firstName":"Server","age":99,"_deleted":false,"_attachments":{},"_rev":"2-server"}]}}"#).expect("json"),
    );
    // And the pull resolver has nothing, because a push also runs an initial-sync read.
    double.respond_to(
        "feedHuman",
        dxdb_json::parse(r#"{"data":{"feedHuman":{"documents":[],"checkpoint":{"updatedAt":0}}}}"#)
            .expect("json"),
    );

    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        None::<OffsetPull>,
        Some(RowsPush),
    );
    let state = state_with(handler, "graphql-push-1");

    // The server holds a divergent state, so the push is refused.
    // (The double answers from its queue; the conflict list is what matters.)
    block_on(state.fork().bulk_write(
        vec![dxdb_storage::insert_row(human("dup", "Client", 10))],
        "local".to_string(),
    ))
    .expect("fork write");

    let mut upstream = Upstream::new(state.clone());
    block_on(upstream.pump()).expect("push");

    // The request carried the mutation text and the rows.
    let requests = double.requests();
    let push_requests: Vec<&Value> = requests
        .iter()
        .filter(|body| {
            body.get("query")
                .and_then(Value::as_str)
                .is_some_and(|query| query.contains("pushHuman"))
        })
        .collect();
    assert!(
        !push_requests.is_empty(),
        "at least one push request must have been made"
    );
    let body = push_requests[0];
    assert!(
        body.get("query")
            .and_then(Value::as_str)
            .is_some_and(|query| query.contains("pushHuman")),
        "the caller's mutation text must be sent: {}",
        dxdb_json::to_js_json(body)
    );
    assert!(
        body.get("variables")
            .and_then(|variables| variables.get("rows"))
            .and_then(Value::as_array)
            .is_some_and(|rows| rows.len() == 1),
        "the rows must be in the variables: {}",
        dxdb_json::to_js_json(body)
    );

    // And the conflict was resolved by keeping the server's state.
    let resolved = state.events().resolved_conflicts();
    assert_eq!(resolved.len(), 1, "the server's conflict must be resolved");
    assert_eq!(
        resolved[0]
            .input
            .real_master_state
            .get("firstName")
            .and_then(Value::as_str),
        Some("Server")
    );
}

/// A push payload of `{conflicts: [...]}` is accepted too.
///
/// Not upstream's shape, but the one this crate's other adapters use, and a caller should not
/// need a response modifier to normalise it.
#[test]
fn a_push_accepts_the_conflicts_object_shape() {
    let double = GraphQlDouble::new();
    double.respond(dxdb_json::parse(r#"{"data":{"pushHuman":{"conflicts":[]}}}"#).expect("json"));

    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        None::<OffsetPull>,
        Some(RowsPush),
    );
    let state = state_with(handler, "graphql-push-obj");

    block_on(state.fork().bulk_write(
        vec![dxdb_storage::insert_row(human("fresh", "New", 5))],
        "local".to_string(),
    ))
    .expect("fork write");

    let mut upstream = Upstream::new(state.clone());
    block_on(upstream.pump()).expect("push");

    assert!(
        state.events().resolved_conflicts().is_empty(),
        "an empty conflicts array means success"
    );
}

/// A `dataPath` can reach into a nested list.
///
/// `getProperty` supports numeric elements (`helper.ts:47`), so a server that returns
/// `{data: {feed: [{...}]}}` must work when the caller configures the path.
#[test]
fn an_explicit_data_path_can_index_into_an_array() {
    let double = GraphQlDouble::new();
    double.respond(dxdb_json::parse(r#"{"data":{"feed":[{"documents":[{"passportId":"c","firstName":"Cara","age":20,"_deleted":false,"_attachments":{},"_rev":"1-z"}],"checkpoint":{"offset":3}}]}}"#).expect("json"));

    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        Some(OffsetPull),
        None::<RowsPush>,
    )
    .with_data_path(vec![
        Value::String("data".to_string()),
        Value::String("feed".to_string()),
        Value::from(0u64),
    ]);
    let state = state_with(handler, "graphql-path");
    let mut downstream = Downstream::new(state.clone(), state.identifier_hash());
    block_on(downstream.resync()).expect("pull");

    let documents = block_on(
        state
            .fork()
            .find_documents_by_id(vec!["c".to_string()], false),
    )
    .expect("query");
    assert_eq!(
        documents.len(),
        1,
        "an explicit path with a numeric element must reach the payload"
    );
}

/// A response modifier can reshape the payload before the adapter reads it.
///
/// Cite: `pull.responseModifier(data, 'handler', checkpoint)` (`index.ts:134-141`).
#[test]
fn a_response_modifier_can_reshape_the_payload() {
    struct Wrap;
    impl ResponseModifier for Wrap {
        fn build(
            &self,
            direction: ResponseDirection,
            payload: Value,
            _checkpoint: Option<&Value>,
        ) -> Value {
            assert_eq!(direction, ResponseDirection::Pull);
            // The server returned `{items, cursor}`; reshape it to what the protocol reads.
            let mut map = dxdb_json::Map::new();
            map.insert(
                "documents".to_string(),
                payload
                    .get("items")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new())),
            );
            map.insert(
                "checkpoint".to_string(),
                payload.get("cursor").cloned().unwrap_or(Value::Null),
            );
            Value::Object(map)
        }
    }

    let double = GraphQlDouble::new();
    double.respond(dxdb_json::parse(r#"{"data":{"feedHuman":{"items":[{"passportId":"d","firstName":"Dan","age":50,"_deleted":false,"_attachments":{},"_rev":"1-w"}],"cursor":{"cursor":"abc"}}}}"#).expect("json"));

    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        Some(UpdatedAtPull),
        None::<RowsPush>,
    )
    .with_response_modifier(Box::new(Wrap));
    let state = state_with(handler, "graphql-modifier");
    let mut downstream = Downstream::new(state.clone(), state.identifier_hash());
    block_on(downstream.resync()).expect("pull");

    let documents = block_on(
        state
            .fork()
            .find_documents_by_id(vec!["d".to_string()], false),
    )
    .expect("query");
    assert_eq!(documents.len(), 1, "the modifier's documents must be used");

    let checkpoint = block_on(dxdb_replication::checkpoint::last_checkpoint(
        &state,
        dxdb_replication::checkpoint::Direction::Down,
    ))
    .expect("read")
    .expect("checkpoint");
    assert_eq!(
        checkpoint.to_js_json(),
        r#"{"cursor":"abc"}"#,
        "the modifier's checkpoint -- shape 3 -- must be persisted"
    );
}

/// A pull-only handler returns no conflicts and makes no push request.
#[test]
fn a_pull_only_handler_does_not_push() {
    let double = GraphQlDouble::new();
    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        Some(UpdatedAtPull),
        None::<RowsPush>,
    );
    let state = state_with(handler, "graphql-pull-only");

    block_on(state.fork().bulk_write(
        vec![dxdb_storage::insert_row(human("x", "X", 1))],
        "local".to_string(),
    ))
    .expect("fork write");

    let mut upstream = Upstream::new(state.clone());
    block_on(upstream.pump()).expect("push");

    assert!(
        double.requests().is_empty(),
        "a caller with no push builder must not cause a request, got {:?}",
        double.requests()
    );
}

/// A malformed payload is an error, not a silent empty batch.
#[test]
fn a_payload_that_is_not_the_expected_shape_is_an_error() {
    let double = GraphQlDouble::new();
    // `checkpoint` is a string where the protocol needs an object.
    double
        .respond(dxdb_json::parse(r#"{"data":{"pushHuman":{"conflicts":"nope"}}}"#).expect("json"));

    let handler = GraphQlHandler::new(
        DoubleClient(double.clone()),
        None::<OffsetPull>,
        Some(RowsPush),
    );
    let state = state_with(handler, "graphql-bad-payload");
    block_on(state.fork().bulk_write(
        vec![dxdb_storage::insert_row(human("bad", "Bad", 1))],
        "local".to_string(),
    ))
    .expect("fork write");

    let mut upstream = Upstream::new(state.clone());
    let result = block_on(upstream.pump());
    assert!(
        result.is_err(),
        "a payload of the wrong shape must fail rather than read as no conflicts"
    );
}

/// An unreachable server surfaces as an error.
#[test]
fn an_unreachable_server_is_an_error() {
    struct Dead;
    impl GraphQlClient for Dead {
        fn post(
            &self,
            _body: Value,
        ) -> dxdb_replication::state::BoxFuture<'_, Result<Value, TransportError>> {
            Box::pin(async { Err(TransportError::Unreachable("no route".to_string())) })
        }
    }
    let handler = GraphQlHandler::new(Dead, Some(UpdatedAtPull), None::<RowsPush>);
    let error = block_on(handler.master_changes_since(None, 10)).expect_err("must fail");
    assert!(
        matches!(error, StorageError::Unsupported(_)),
        "a transport failure surfaces as Unsupported, got {error:?}"
    );
}
