//! The ported upstream conformance suite, written once against an endpoint
//! adapter so it can run on any fork+master pair.
//!
//! Upstream ships this as `src/plugins/test-utils/replication-base-test-suite.ts`
//! (454 lines, pin `3b763ee17`): "the shared suite every replication plugin runs"
//! (`docs/GATES.md` tier 3, `docs/PLAN.md` section 6 M3). Upstream parameterises it
//! with six callbacks; so does this port, through a trait, so the case bodies are
//! written once and the adapter carries the whole difference between endpoints.
//!
//! # Why a lib module instead of a test helper
//!
//! Integration tests under `tests/` are separate crates: a helper module in
//! `tests/` cannot be imported by a sibling test binary. The shared pieces — the
//! config trait, the case table, the assertions — therefore live here, and the
//! test binaries pull them in with `use dxdb_replication::conformance::*`. This is
//! the conventional Rust arrangement, not an escape hatch.
//!
//! # What is real here and what is not
//!
//! Real: the case list, the operation order inside each case, the assertions, and
//! the mapping from each case to its upstream `file:line`. [`EndpointAdapter`] has
//! the same shape as upstream's config callbacks, so when `dxdb-memory` lands (lane
//! D) and the protocol lands (this lane), an adapter over them plugs in without
//! touching a case body.
//!
//! Not real: the protocol and the storage. The suite is gated on
//! [`ReplicationDriver`], which by default lets a case run its body against the
//! adapter and records every call. That is how the port is proven runnable and
//! correctly shaped before the protocol exists. Arming `DXDB_RUN_CONFORMANCE=1`
//! makes the driver fail, because there is no implementation yet — so the suite can
//! never quietly pass as if there were.
//!
//! # Case inventory
//!
//! Upstream defines 11 cases: 6 mandatory, 4 behind `syncOnceWithAttachments`, and
//! 1 behind `syncOnceWithAttachmentsDisabled`. The opt-in blocks are cases in their
//! own right, so they are deferred with the case name attached rather than dropped.
//! See [`UPSTREAM_CASES`].

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use serde_json::Value;

// ---------------------------------------------------------------------------
// Upstream case table
// ---------------------------------------------------------------------------

/// How a ported case is currently backed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// Ported and executable against the suite's adapter.
    Ported,
    /// Ported into the suite, but blocked on a capability that does not exist yet.
    /// The string names the gate that unblocks it.
    PortedDeferred(&'static str),
}

/// One case from the upstream suite, with the exact `it(...)` span so a reader can
/// diff the port against the original without searching for it.
#[derive(Clone, Copy, Debug)]
pub struct UpstreamCase {
    /// `file:line` of the upstream `it(...)`.
    pub span: &'static str,
    /// Upstream `describe` block path.
    pub block: &'static str,
    /// The upstream test name, character for character.
    pub name: &'static str,
    /// Whether the case only exists when an optional config callback is set.
    pub opt_in: bool,
    /// How this port currently backs it, and why.
    pub backing: Backing,
    /// The test function in `tests/replication_conformance.rs` that carries it.
    pub rust_test: &'static str,
}

/// Every case in `replication-base-test-suite.ts`, including the opt-in ones.
///
/// The opt-in cases are all attachment cases. Upstream gates them on callbacks
/// rather than on capabilities, but the reason they are optional is that a
/// replication plugin may not support attachments at all. The port keeps the same
/// gate shape — two `Option`-returning adapter methods — so an endpoint that gains
/// attachment support runs them unchanged.
pub const UPSTREAM_CASES: &[UpstreamCase] = &[
    UpstreamCase {
        span: "src/plugins/test-utils/replication-base-test-suite.ts:112",
        block: "base test suite > live replication",
        name: "push replication to client-server",
        opt_in: false,
        backing: Backing::Ported,
        rust_test: "push_replication_to_client_server",
    },
    UpstreamCase {
        span: "src/plugins/test-utils/replication-base-test-suite.ts:156",
        block: "base test suite > live replication",
        name: "two collections",
        opt_in: false,
        backing: Backing::Ported,
        rust_test: "two_collections",
    },
    UpstreamCase {
        span: "src/plugins/test-utils/replication-base-test-suite.ts:243",
        block: "base test suite > conflict handling",
        name: "should keep the master state as default conflict handler",
        opt_in: false,
        backing: Backing::Ported,
        rust_test: "should_keep_the_master_state_as_default_conflict_handler",
    },
    UpstreamCase {
        span: "src/plugins/test-utils/replication-base-test-suite.ts:276",
        block: "base test suite > attachment replication",
        name: "should replicate an inserted attachment",
        opt_in: true,
        backing: Backing::PortedDeferred(
            "attachment-capable double: bytes, digest and stub round-trip",
        ),
        rust_test: "attachment_inserted",
    },
    UpstreamCase {
        span: "src/plugins/test-utils/replication-base-test-suite.ts:301",
        block: "base test suite > attachment replication",
        name: "should replicate an updated attachment",
        opt_in: true,
        backing: Backing::PortedDeferred(
            "attachment-capable double: bytes, digest and stub round-trip",
        ),
        rust_test: "attachment_updated",
    },
    UpstreamCase {
        span: "src/plugins/test-utils/replication-base-test-suite.ts:336",
        block: "base test suite > attachment replication",
        name: "should replicate a deleted attachment",
        opt_in: true,
        backing: Backing::PortedDeferred(
            "attachment-capable double: bytes, digest and stub round-trip",
        ),
        rust_test: "attachment_deleted",
    },
    UpstreamCase {
        span: "src/plugins/test-utils/replication-base-test-suite.ts:369",
        block: "base test suite > attachment replication",
        name: "should keep the master attachment state on conflict",
        opt_in: true,
        backing: Backing::PortedDeferred(
            "protocol conflict resolution: needs the real upstream, not a double",
        ),
        rust_test: "attachment_master_state_on_conflict",
    },
    UpstreamCase {
        span: "src/plugins/test-utils/replication-base-test-suite.ts:424",
        block: "base test suite > attachment replication disabled",
        name: "should not replicate attachment data when attachments are disabled",
        opt_in: true,
        backing: Backing::PortedDeferred(
            "attachment-capable double that can also withhold payloads",
        ),
        rust_test: "attachment_disabled_binary_not_replicated",
    },
];

/// The upstream file this module ports.
pub const UPSTREAM_FILE: &str = "src/plugins/test-utils/replication-base-test-suite.ts";

/// Whole-file span, for coverage reporting.
pub const UPSTREAM_LINE_SPAN: (u32, u32) = (1, 454);

/// Upstream lines ported as machinery rather than as a case body, or deliberately
/// not ported at all. Recorded so "lines ported" is not a hand-wave: every
/// remaining upstream line is inside a case body mapped 1:1 by [`UPSTREAM_CASES`].
pub const MACHINERY_SPANS: &[(u32, u32, &str)] = &[
    (
        1,
        19,
        "file doc block, imports, and the `wait` helper import",
    ),
    (
        20,
        98,
        "ReplicationBaseTestSuiteConfig: ported as the EndpointAdapter trait, incl. doc comments",
    ),
    (
        100,
        107,
        "runReplicationBaseTestSuite signature + @example: ported as the Suite type",
    ),
    (
        110,
        111,
        "describe('base test suite') / describe('live replication')",
    ),
    (
        240,
        242,
        "close of live block, open of describe('conflict handling')",
    ),
    (
        272,
        275,
        "close of conflict block, open of the attachment describe",
    ),
    (
        419,
        423,
        "close of attachment block, open of the disabled describe",
    ),
    (
        451,
        454,
        "close of the disabled block, close of the outer describe and function",
    ),
];

// ---------------------------------------------------------------------------
// The human schema and fixtures
// ---------------------------------------------------------------------------

/// Upstream `humanSchemaLiteral` (`src/plugins/test-utils/schemas.ts:36-66`).
///
/// Reproduced verbatim: same `primaryKey`, same `required` order, same
/// `indexes: ['firstName']`, same `age` bounds with `multipleOf: 1`.
pub fn human_schema() -> Value {
    dxdb_json::parse(
        r#"{
            "title": "human schema",
            "description": "describes a human being",
            "version": 0,
            "keyCompression": false,
            "primaryKey": "passportId",
            "type": "object",
            "properties": {
                "passportId": { "type": "string", "maxLength": 100 },
                "firstName": { "type": "string", "maxLength": 100 },
                "lastName": { "type": "string", "maxLength": 100 },
                "age": {
                    "description": "age in years",
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 150,
                    "multipleOf": 1
                }
            },
            "required": ["firstName", "lastName", "passportId"],
            "indexes": ["firstName"]
        }"#,
    )
    .expect("the human schema literal is valid JSON")
}

/// The primary path of [`human_schema`], as upstream reads it from the schema.
pub fn human_primary_path() -> &'static str {
    "passportId"
}

/// Deterministic replacement for upstream's `randomStringWithSpecialChars`.
///
/// Upstream draws these from `Math.random()`, so its own suite is not reproducible:
/// a failing case can be re-run but not replayed. Here the value is a seeded
/// function of `salt`, so a failure is reproducible. The alphabet still includes
/// multi-byte code points and the characters that change an index string, because
/// that is what upstream's helper is exercising.
pub fn fixture_string(salt: &str, n: usize) -> String {
    const ALPHABET: &[char] = &[
        'a', 'b', 'c', 'X', 'Y', 'Z', '0', '1', '9', ' ', '-', '_', '.', ':', '~', '!', 'ä', 'ß',
        '中', 'Ｓ',
    ];
    let mut out = String::new();
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in salt.as_bytes() {
        state ^= *byte as u64;
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for _ in 0..n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(ALPHABET[(state % ALPHABET.len() as u64) as usize]);
    }
    out
}

/// Upstream `humanData(passportId, age, firstName)`
/// (`src/plugins/test-utils/schema-objects.ts:73-84`).
///
/// The upstream parameter order is **passportId, age, firstName** — not
/// passportId, firstName. Getting it wrong silently swaps two fields, so the
/// signature here is positional in the same order.
pub fn human_data(passport_id: Option<&str>, age: Option<i64>, first_name: Option<&str>) -> Value {
    let passport = passport_id
        .map(str::to_string)
        .unwrap_or_else(|| fixture_string("passportId", 10));
    let age = age.unwrap_or_else(|| {
        let digits = fixture_string(&format!("age{passport}"), 3);
        10 + (digits.bytes().map(u64::from).sum::<u64>() % 40) as i64
    });
    let first = first_name
        .map(str::to_string)
        .unwrap_or_else(|| fixture_string(&format!("firstName{passport}"), 10));
    let last = fixture_string(&format!("lastName{passport}"), 10);

    let mut map = serde_json::Map::new();
    map.insert("passportId".to_string(), Value::String(passport));
    map.insert("firstName".to_string(), Value::String(first));
    map.insert("lastName".to_string(), Value::String(last));
    map.insert("age".to_string(), json_integer(age));
    Value::Object(map)
}

/// A JSON integer, used for `age` so the fixture matches the schema's
/// `type: 'integer'` and prints as `42` rather than `42.0`.
pub fn json_integer(value: i64) -> Value {
    Value::Number(serde_json::Number::from(value))
}

// ---------------------------------------------------------------------------
// The adapter: upstream's six config callbacks, as a trait
// ---------------------------------------------------------------------------

/// Endpoints whose live replications can be pumped by the test.
///
/// Separate from [`EndpointAdapter`] so an endpoint that only supports one-shot sync (a
/// transport with no change feed, for instance) still runs the conflict and attachment
/// cases. Only the two live cases require this bound.
pub trait LiveFanout {
    /// Runs live propagation until nothing changes.
    fn fan_out_live(&mut self);
}

/// What an endpoint pair has to provide for the suite to run.
///
/// Method names follow upstream's config callback names exactly
/// (`replication-base-test-suite.ts:20-98`), so a reader diffing the two does not
/// have to translate. `Collection` is opaque to the suite for the same reason
/// `RxCollection<any>` is: the suite only ever hands it back to the adapter.
pub trait EndpointAdapter {
    /// The collection handle the suite passes around.
    type Collection;

    /// The replication handle [`Self::start_replication`] hands back.
    type Handle: ReplicationHandle;

    /// Upstream `startReplication(collection)` — a **live** replication.
    ///
    /// Must have the semantics the upstream doc comment demands: "Must target a
    /// shared server endpoint so that multiple collections can replicate to the
    /// same backend" (`replication-base-test-suite.ts:25-27`).
    fn start_replication(&mut self, collection: &Self::Collection) -> Self::Handle;

    /// Upstream `syncOnce(collection)` — one-shot sync against the same endpoint as
    /// [`Self::start_replication`], then quiesce.
    fn sync_once(&mut self, collection: &Self::Collection);

    /// Upstream `getAllServerDocs()` — every document the master holds, including
    /// soft-deleted ones when [`Self::soft_deletes`] is true.
    fn all_server_docs(&mut self) -> Vec<Value>;

    /// Upstream `cleanUpServer()` — called at the start of every case.
    fn clean_up_server(&mut self);

    /// Upstream `softDeletes?`.
    fn soft_deletes(&self) -> bool {
        false
    }

    /// Upstream `isDeleted?(serverDoc)`. Only called when [`Self::soft_deletes`].
    fn is_deleted(&self, _server_doc: &Value) -> bool {
        false
    }

    /// Upstream `getPrimaryOfServerDoc(serverDoc)`.
    fn primary_of_server_doc(&self, server_doc: &Value) -> String;

    /// Upstream `waitTime?`, in milliseconds. `0` means "not configured".
    fn wait_time_ms(&self) -> u64 {
        0
    }

    /// Upstream `collection.find().exec()` — the documents a fork currently holds,
    /// tombstones excluded.
    ///
    /// This is the one thing upstream's suite gets for free by holding real
    /// `RxCollection` objects, and the thing a Rust adapter has to be asked for. It
    /// is on this trait rather than a separate one because every adapter that can
    /// sync can also list what it synced, and a second trait bound on one method is
    /// worse than one required method.
    fn fork_documents(&self, collection: &Self::Collection) -> Vec<Value>;

    /// Upstream `syncOnceWithAttachments?`. `None` means "this endpoint does not do
    /// attachments", which is what gates the four attachment cases.
    fn sync_once_with_attachments(&mut self, collection: &Self::Collection) -> Option<()> {
        let _ = collection;
        None
    }

    /// Upstream `syncOnceWithAttachmentsDisabled?`. `None` gates the fifth opt-in
    /// case.
    fn sync_once_with_attachments_disabled(&mut self, collection: &Self::Collection) -> Option<()> {
        let _ = collection;
        None
    }
}

/// What `start_replication` hands back, mirroring `RxReplicationState`
/// (`src/plugins/replication/index.ts:81`) restricted to what the suite calls.
pub trait ReplicationHandle {
    /// Upstream `awaitInitialReplication()`.
    fn await_initial_replication(&mut self);
    /// Upstream `awaitInSync()`, including its twice-around idleness loop.
    fn await_in_sync(&mut self);
    /// Upstream `reSync()`.
    fn re_sync(&mut self);
    /// Upstream `ensureReplicationHasNoErrors(state)`, which subscribes to `error$`
    /// and throws. Drained here rather than subscribed so the suite stays
    /// executor-agnostic.
    fn take_errors(&mut self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// The driver: the seam where the protocol goes
// ---------------------------------------------------------------------------

/// One observable step a case asked of the endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    CleanUpServer,
    StartReplication,
    SyncOnce,
    SyncOnceWithAttachments,
    SyncOnceWithAttachmentsDisabled,
    AwaitInitialReplication,
    AwaitInSync,
    ReSync,
    GetAllServerDocs,
    Wait { ms: u64 },
}

/// Decides whether a case may run, and records what it asked for.
///
/// Selection is by environment variable rather than by a feature so one test binary
/// proves both properties:
///
/// - default: every case runs its whole body against the adapter and asserts on the
///   resulting call sequence, annotating itself as awaiting the protocol;
/// - `DXDB_RUN_CONFORMANCE=1`: the driver declares "the protocol is implemented" and
///   fails, because there is no implementation to run. That is the honest default
///   for a suite whose subject does not exist, and it makes arming the flag itself
///   an assertion.
pub struct ProtocolNotImplemented {
    enabled: bool,
    log: Vec<Call>,
}

/// Environment variable that arms [`ProtocolNotImplemented`].
pub const RUN_ENV: &str = "DXDB_RUN_CONFORMANCE";

/// A case's seam to whatever actually runs the replication.
pub trait ReplicationDriver {
    /// Record one step, before the adapter performs it, so the log reads in the
    /// same order as the case body.
    fn record(&mut self, call: Call);
    /// The steps recorded so far, in order. Lets a case assert the documented call
    /// sequence without naming the concrete driver.
    fn calls(&self) -> Vec<Call>;
    /// True when a real implementation is required.
    fn requires_implementation(&self) -> bool;
    /// Fail the current case with a message that names the gate.
    fn fail(&mut self, message: String) -> !;
}

impl Default for ProtocolNotImplemented {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolNotImplemented {
    pub fn new() -> Self {
        Self {
            enabled: std::env::var(RUN_ENV).is_ok_and(|v| v == "1"),
            log: Vec::new(),
        }
    }

    /// The calls the cases made, in order. Read by the call-sequence assertions.
    pub fn log(&self) -> &[Call] {
        &self.log
    }

    /// True when the process is armed to demand real behaviour.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl ReplicationDriver for ProtocolNotImplemented {
    fn record(&mut self, call: Call) {
        self.log.push(call);
    }

    fn calls(&self) -> Vec<Call> {
        self.log.clone()
    }

    fn requires_implementation(&self) -> bool {
        self.enabled
    }

    fn fail(&mut self, message: String) -> ! {
        panic!("{message}")
    }
}

/// Called at the top of every case body.
///
/// Unarmed, this is a no-op and the case runs. Armed via [`RUN_ENV`], it fails with
/// the gate's name, so a run can prove the suite is not passing against a stub.
pub fn gate(driver: &mut dyn ReplicationDriver, case_name: &str, gate_reason: &str) {
    if driver.requires_implementation() {
        driver.fail(format!(
            "{case_name}: the replication protocol is not implemented yet ({gate_reason}). \
             Unset {RUN_ENV} to run the case body against the documented call sequence."
        ));
    }
}

// ---------------------------------------------------------------------------
// The suite runner
// ---------------------------------------------------------------------------

/// A replication handle the suite is holding for a case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandleId(usize);

/// Everything a case body needs: one adapter, one driver, and the handles the case
/// started.
///
/// Owns its adapter, so a case body can interleave `all_server_docs()` with
/// `await_in_sync()` in the order upstream does without fighting the borrow checker
/// — the borrow problem is why the handle is an id rather than a `&mut`.
pub struct Suite<A: EndpointAdapter> {
    adapter: A,
    driver: Box<dyn ReplicationDriver>,
    handles: Vec<A::Handle>,
    /// Documents read from the master, kept so a case can inspect them after the
    /// adapter is borrowed again.
    server_docs: VecDeque<Vec<Value>>,
}

impl<A: EndpointAdapter> Suite<A> {
    pub fn new(adapter: A, driver: Box<dyn ReplicationDriver>) -> Self {
        Self {
            adapter,
            driver,
            handles: Vec::new(),
            server_docs: VecDeque::new(),
        }
    }

    /// Read-only access to the adapter, for endpoint-specific assertions (querying
    /// the fork for a document, for instance).
    pub fn adapter(&self) -> &A {
        &self.adapter
    }

    /// Mutable access, for endpoint-specific fixture writes.
    pub fn adapter_mut(&mut self) -> &mut A {
        &mut self.adapter
    }

    /// Read-only access to the driver's call log.
    pub fn calls(&self) -> Vec<Call> {
        self.driver.calls()
    }

    /// Upstream `config.cleanUpServer()` at the top of every case.
    pub fn begin(&mut self) {
        self.driver.record(Call::CleanUpServer);
        self.adapter.clean_up_server();
    }

    /// Upstream `await config.syncOnce(x)`.
    pub fn sync_once(&mut self, collection: &A::Collection) {
        self.driver.record(Call::SyncOnce);
        self.adapter.sync_once(collection);
    }

    /// Upstream `config.syncOnceWithAttachments!(x)`, returning whether the adapter
    /// has that capability — the port of `if (config.syncOnceWithAttachments)`.
    pub fn sync_once_with_attachments(&mut self, collection: &A::Collection) -> bool {
        self.driver.record(Call::SyncOnceWithAttachments);
        self.adapter
            .sync_once_with_attachments(collection)
            .is_some()
    }

    /// Upstream `config.syncOnceWithAttachmentsDisabled!(x)`.
    pub fn sync_once_with_attachments_disabled(&mut self, collection: &A::Collection) -> bool {
        self.driver.record(Call::SyncOnceWithAttachmentsDisabled);
        self.adapter
            .sync_once_with_attachments_disabled(collection)
            .is_some()
    }

    /// Upstream `await config.startReplication(x)`.
    pub fn start_replication(&mut self, collection: &A::Collection) -> HandleId {
        self.driver.record(Call::StartReplication);
        self.handles
            .push(self.adapter.start_replication(collection));
        HandleId(self.handles.len() - 1)
    }

    fn handle_mut(&mut self, id: HandleId) -> &mut A::Handle {
        &mut self.handles[id.0]
    }

    /// Upstream `ensureReplicationHasNoErrors(state)`.
    pub fn ensure_no_errors(&mut self, id: HandleId) {
        let errors = self.handle_mut(id).take_errors();
        assert!(
            errors.is_empty(),
            "ensureReplicationHasNoErrors(): the replication reported {:?}",
            errors
        );
    }

    /// Upstream `await replicationState.awaitInitialReplication()`.
    pub fn await_initial_replication(&mut self, id: HandleId) {
        self.ensure_no_errors(id);
        self.driver.record(Call::AwaitInitialReplication);
        self.handle_mut(id).await_initial_replication();
        self.ensure_no_errors(id);
    }

    /// Upstream `await replicationState.awaitInSync()`.
    pub fn await_in_sync(&mut self, id: HandleId) {
        self.ensure_no_errors(id);
        self.driver.record(Call::AwaitInSync);
        self.handle_mut(id).await_in_sync();
        self.ensure_no_errors(id);
    }

    /// Upstream `replicationState.reSync()`.
    pub fn re_sync(&mut self, id: HandleId) {
        self.driver.record(Call::ReSync);
        self.handle_mut(id).re_sync();
    }

    /// Drives live replication to a fixpoint.
    ///
    /// Upstream's live replication converges on its own and the suite only has to await
    /// it. This port has no background task (D3: no runtime), so a live case drives the
    /// endpoint's fan-out where upstream would await. Named after what it stands in for
    /// rather than `sync`, so a reader can see that the live cases are not secretly
    /// one-shot cases.
    pub fn drive_live(&mut self)
    where
        A: LiveFanout,
    {
        self.adapter.fan_out_live();
    }

    /// Upstream `config.getAllServerDocs()`, stashed so the case can assert on it
    /// after the adapter is needed again.
    pub fn fetch_server_docs(&mut self) {
        self.driver.record(Call::GetAllServerDocs);
        let docs = self.adapter.all_server_docs();
        self.server_docs.push_back(docs);
    }

    /// The most recently fetched master documents.
    pub fn server_docs(&self) -> &[Value] {
        self.server_docs
            .back()
            .map(Vec::as_slice)
            .expect("fetch_server_docs() was not called")
    }

    /// Upstream `if (waitTime) { await wait(waitTime); }`.
    pub fn wait_if_configured(&mut self) {
        let ms = self.adapter.wait_time_ms();
        if ms > 0 {
            self.driver.record(Call::Wait { ms });
        }
    }

    /// Upstream `awaitCollectionsHaveEqualState(a, b, context)`:
    /// `find()` both sides, project through `toJSON()`, compare.
    pub fn await_collections_have_equal_state(
        &mut self,
        left: &A::Collection,
        right: &A::Collection,
        context: &str,
    ) {
        let primary = human_primary_path();
        let left_docs = documents_by_primary(&self.adapter.fork_documents(left), primary);
        let right_docs = documents_by_primary(&self.adapter.fork_documents(right), primary);
        assert_eq!(
            left_docs.len(),
            right_docs.len(),
            "awaitCollectionsHaveEqualState({context}): document counts differ \
             (left {}, right {})",
            left_docs.len(),
            right_docs.len()
        );
        for (index, (l, r)) in left_docs.iter().zip(right_docs.iter()).enumerate() {
            assert_deep_eq(
                l,
                r,
                &format!("awaitCollectionsHaveEqualState({context}): document {index}"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Assertions (upstream uses `assert.strictEqual` / `assert.deepStrictEqual`)
// ---------------------------------------------------------------------------

/// Upstream `assert.strictEqual` for a JSON number.
pub fn assert_num_eq(actual: &Value, expected: f64, what: &str) {
    let got = actual.as_f64().unwrap_or_else(|| {
        panic!(
            "{what}: expected a number, got {}",
            dxdb_json::to_js_json(actual)
        )
    });
    assert_eq!(got, expected, "{what}");
}

/// Upstream `assert.strictEqual` for a JSON string.
pub fn assert_str_eq(actual: &Value, expected: &str, what: &str) {
    assert_eq!(
        dxdb_json::to_js_json(actual),
        dxdb_json::to_js_json(&Value::String(expected.to_string())),
        "{what}"
    );
}

/// Upstream `assert.deepStrictEqual` on two values.
///
/// `deepStrictEqual` compares own enumerable properties, so key order is not part of
/// equality; comparing `to_js_json` output would be stricter than upstream and would
/// fail on a legitimate key-order difference. `serde_json`'s object equality is
/// order-insensitive and its array equality is order-sensitive, which is the same
/// contract. The one known difference is JS `-0` versus `0`, recorded in the port
/// plan.
pub fn assert_deep_eq(actual: &Value, expected: &Value, what: &str) {
    assert_eq!(
        actual,
        expected,
        "{what}\n  actual:   {}\n  expected: {}",
        dxdb_json::to_js_json(actual),
        dxdb_json::to_js_json(expected)
    );
}

/// The projection upstream's state-equality assertion compares: `toJSON()` without
/// meta fields (`rx-document.ts:260-267`).
///
/// Upstream deletes `_rev`, `_attachments`, `_deleted`, `_meta` and keeps everything
/// else in place, so the remaining key order is the document's own.
pub fn to_json_without_meta_fields(doc: &Value) -> Value {
    let Value::Object(map) = doc else {
        return doc.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, value) in map {
        if matches!(key.as_str(), "_rev" | "_attachments" | "_deleted" | "_meta") {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

/// Every document's `toJSON()` projection, sorted by primary key.
///
/// Sorted because upstream's `find()` default query has no explicit sort, so its
/// `assert.deepStrictEqual` on the resulting array depends on index order; the
/// suite's intent is set equality, so the port states it. Primary-key order is
/// `dxdb_jsstr::compare_js`, the single ordering contract (DECISIONS.md D5).
pub fn documents_by_primary(docs: &[Value], primary_path: &str) -> Vec<Value> {
    let mut projected: Vec<Value> = docs
        .iter()
        .map(|doc| to_json_without_meta_fields(doc))
        .collect();
    projected.sort_by(|a, b| {
        let ka = a
            .get(primary_path)
            .map(dxdb_json::to_js_json)
            .unwrap_or_default();
        let kb = b
            .get(primary_path)
            .map(dxdb_json::to_js_json)
            .unwrap_or_default();
        dxdb_jsstr::compare_js(&ka, &kb)
    });
    projected
}
