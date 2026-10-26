# `dxdb-worker`: behavioural specification

| | |
| --- | --- |
| Stage | B3 (CLEANROOM.md section 7) |
| Crate | `crates/cleanroom/dxdb-worker` |
| Documented upstream modules | `storage-worker`, and `storage-worker`'s SharedWorker mode (CLEANROOM.md section 4 rows 8 and 9) |
| Spec author read | `docs-src/docs/rx-storage-worker.md`, `docs-src/docs/rx-storage-shared-worker.md`; the Apache-2.0 `src/plugins/storage-remote/*` (the transport this builds on); the MDN pages for `Worker`, `SharedWorker` and `MessagePort`; PLAN.md 3.4 and 5.2; the landed `dxdb-storage` traits |
| Spec author did not read | `rxdb-premium` in any form (source, bundles, `.d.ts`, source maps, installer, payloads) |
| Full provenance | [`READING-LIST.md`](./READING-LIST.md), row `dxdb-worker` |
| Gap rows this spec depends on | G-001, G-002, G-006, G-048, G-049, G-050, G-051, G-052, G-053 |
| Status | spec ready for review; implementation not started |

## 1. The model decision, stated first

`PLAN.md` 5.2 says Wasm threads need either `wasm32-wasip1-threads` or nightly
`-Z build-std`, because the prebuilt `wasm32-unknown-unknown` std is not compiled with
atomics. That raised the question of whether this crate is blocked on that.

**It is not, and the reason is that a worker is not a thread.** This crate implements
**message-passing isolates**: a browser `Worker`, a browser `SharedWorker`, and Node's
`worker_threads`. Each is a separate JavaScript realm with **its own linear memory**.

| Property | Wasm threads | Message-passing isolates (this crate) |
| --- | --- | --- |
| Memory | shared, `SharedArrayBuffer` | separate, per isolate |
| Synchronisation | atomics, locks | message passing, no shared mutable state |
| Needs `atomics`/`bulk-memory` | yes | **no** |
| Needs `wasm32-wasip1-threads` or nightly | yes | **no** |
| Runs on stable `wasm32-unknown-unknown` | no | **yes** |
| Failure mode | torn reads, deadlock | a message that never arrives |

So the whole of this crate's contract — the typed protocol, the handshake, backpressure,
and supervision of a worker that dies mid-write — lands on **stable**
`wasm32-unknown-unknown`, and `make wasm` gates it like every other crate. No nightly, no
threads target, no shared memory.

The cost, stated because it is a real trade rather than a win:

- **No zero-copy document transfer.** Every call and every result is serialised across
  the boundary. A `query` returning 10 000 documents copies them. A threads-based
  storage could hand over a shared buffer; this one cannot.
- **No synchronous completion.** Every storage method becomes a round trip, adding at
  least one message hop of latency to every operation.
- **Structured-clone constraints.** Whatever crosses the boundary must be
  structured-cloneable. The port's `DocumentData` is JSON, so documents are fine;
  nothing may smuggle a `Map`, a typed-array view into another isolate's memory, or a
  closure.

That trade is right for this crate's purpose. The documented point of the worker plugin
is to take CPU load off the main thread — "you can take CPU load from the main process
and move it into the worker's process which can improve the perceived performance of your
application". That benefit comes from *where the work runs*, not from sharing memory.
Paying copy costs to move encryption, query scanning and index maintenance off the main
thread is a good trade; a threads-based design would buy the copy away at the price of
nightly-only builds and torn-read bugs. Recorded in G-048.

### 1.1 What "SharedWorker mode" adds, and why it is one crate

> "Compared to a normal WebWorker, the SharedWorker is created exactly once, even when
> there are multiple browser tabs opened. Because of having exactly one worker, multiple
> performance optimizations can be done because the storage itself does not have to
> handle multiple opened database connections."
> — `docs-src/docs/rx-storage-shared-worker.md`

Observable: with SharedWorker mode, N tabs share one worker and therefore one storage
instance. With normal worker mode, each tab gets its own worker and its own instance. The
difference is instance *multiplicity*, not protocol.

So both modes are one crate, and the mode is one enum field, because the protocol,
handshake, backpressure and supervision are identical. Only the connection-acquisition
step differs: `Worker` constructs a new one per `workerInput`; `SharedWorker` retrieves
or creates the one for that URL and then opens a `MessagePort` on it. That is why this
spec does not split into two documents (CLEANROOM.md section 4's grouping note).

## 2. Observable behaviour

### 2.1 The storage keeps working, from further away

> "you can put the RxStorage of your database inside of a WebWorker (in browsers) or a
> Worker Thread (in node.js). By doing so, you can take CPU load from the main process
> and move it into the worker's process"

Observable: every operation of the storage trait behaves identically whether the storage
is in-process or behind a worker. Same documents, same order, same counts, same events.
The only observable differences are latency and the arrival times of change events.

This is the same shape of invariant as sharding's, and for the same reason: a user must
not have to know where their storage lives.

### 2.2 The worker is started by the caller, not by us

> "`workerInput`: contains any value that can be used as parameter to the Worker
> constructor of thread.js. Most likely you want to put the path to the worker.js file in
> here."

Observable: dxdb does not bundle or locate a worker file. The caller supplies either a
URL/path or a constructor that returns a worker instance. Consequence, worth stating:
**dxdb ships no pre-built worker bundle**, unlike upstream's `dist/workers`. A Rust
crate cannot ship a JavaScript bundle meaningfully, and the JS surface crate's bundled
artefact belongs to that crate, not this one.

### 2.3 Change events cross the boundary

Observable: `change_stream()` on a remote instance yields `EventBulk`s. Two properties
matter and both are testable:

- **no loss**: an event emitted while a subscriber is attached is delivered;
- **no duplicate delivery from our own transport**: the inner storage's semantics about
  duplicate events (a storage that forwards other instances' changes is at-least-once,
  as `EventBulk::id` documents) are preserved rather than made worse.

### 2.4 A worker that dies must not look like a slow call

This is the part with real failure modes, and the part a test can catch. Observable:

- Every in-flight call at the moment the worker dies **fails with a specific error**,
  not a hang.
- A call issued *after* the death fails immediately with the same specific error.
- The failure is *reported once* per call; a caller that retries gets a fresh attempt,
  and this crate does not silently retry underneath a `bulk_write` (see section 3.6).

### 2.5 Backpressure is observable as latency, never as corruption

Observable: if the transport's queue is full, calls wait. They do not interleave
partially, they do not reorder, and they do not drop. A caller sees slower calls, never a
wrong answer.

## 3. Design

### 3.1 The protocol: request/response with monotonic ids

```text
request  { id: u64, kind: RequestKind, payload: <per-kind> }
response { id: u64, result: Ok(<per-kind>) | Err(RemoteError) }
event    { id: 0,  kind: Event,  payload: EventBulk }      // id 0 is reserved for pushes
```

- **`id` is a monotonic `u64` allocated by the caller.** It is the only correlation
  mechanism. A response whose `id` is unknown is dropped and counted (see 3.7) rather
  than being assumed to belong to the oldest in-flight call — that assumption is how
  request/response transports pair the wrong results under reordering.
- **`id: 0` is reserved for the initial handshake and for pushes.** A storage call never
  uses it, so a storage response can never be mistaken for a handshake reply.
- Every `RequestKind` maps one-to-one onto a storage trait method. The enum is
  exhaustive and the crate is built so a new trait method is a compile error rather than
  an unhandled message.

### 3.2 The handshake negotiates a version and an identity

The first exchange, before any storage call:

```text
caller -> worker   { id: 0, kind: Hello, payload: { protocol: 1, min_supported: 1 } }
worker -> caller   { id: 0, kind: HelloReply, payload: { protocol: 1,
                                                        storage_name: "...",
                                                        instance_token: "..." } }
```

- **Version negotiation is a range, not an equality.** A caller offering
  `[min_supported, protocol]` and a worker replying with its own range must overlap, and
  the agreed version is the highest in the intersection. Equality-only negotiation makes
  every minor addition a hard break; a range makes an additive change compatible.
- If the ranges do not overlap, the connection fails with `ProtocolMismatch { worker,
  caller }` naming both versions. This is the same reasoning as the encryption
  envelope's version byte: a build that meets a newer peer should say so, not fail
  mysteriously.
- `instance_token` is echoed so a caller that reconnects to a *different* worker process
  can tell. Without it, a reconnection after a worker restart would look like the same
  instance, and in-flight assumptions about revisions would be wrong.

### 3.3 Backpressure: bounded queue, no unbounded growth

- The caller's send queue is **bounded** (default 64 in-flight requests). A call beyond
  the bound awaits a slot rather than allocating.
- Why bounded: an unbounded queue converts a slow worker into an out-of-memory crash on
  the main thread, which is the worse failure. A bounded queue converts it into
  latency, which is what the user can see and what section 2.5 promises.
- Ordering: requests are sent in the order the caller issued them, and results are
  matched by `id`, so a worker that completes out of order still pairs correctly. This is
  why the `id` design matters — the alternative (assume FIFO responses) breaks the moment
  a storage answers a `count` faster than the `query` before it.

### 3.4 Supervision: the part with real failure modes

Four distinct failures, four distinct responses, because conflating them is how a
transport turns a recoverable error into a hang:

| Failure | Detection | Response |
| --- | --- | --- |
| **Worker terminates** (crash, `terminate()`, OOM) | the transport's error/exit callback, plus a heartbeat timeout | every in-flight call fails `WorkerDied`; the instance is marked dead; later calls fail immediately |
| **Worker alive but unresponsive** | heartbeat: ping every 5 s, expect a reply within 15 s | after the timeout, treated as died: `WorkerUnresponsive`, in-flight calls fail, instance marked dead |
| **Worker alive, one call hangs** | per-call deadline, default 30 s | that call fails `CallTimeout`, **the instance stays alive**: one hung call is not evidence the worker is gone |
| **Worker replies with an error** | the `Err` variant | the error is propagated as `StorageError` with the remote message; the instance stays alive |

The three-way split is the design claim. A single "worker failed" bucket would either
tear down a healthy instance because one query was slow, or keep using a dead one
because its last error was ordinary. Tests for each are named in section 6.

**Mid-write death is the case that must be got exactly right.** `bulk_write` is
per-document atomic inside the storage (PLAN.md 4.2), but across a worker boundary the
caller cannot know which rows were applied when the worker died. So:

- The wrapper **never retries a `bulk_write`** after `WorkerDied`. A retry could apply
  rows twice, and rows carry revisions, so a double apply would surface as a conflict
  the user did not cause — or, worse, as a successful duplicate write for a caller that
  generated revisions itself.
- The error kind is `WorkerDiedDuringWrite { context }`, distinct from `WorkerDied`, so a
  caller can reason about it: the correct recovery is to re-read the affected documents
  and reconcile, which is `dxdb-core`'s business, not the transport's.
- The `context` string the caller passed to `bulk_write` is included, because it is how
  the caller identifies which of its own operations was in flight.

This is a divergence-shaped decision worth recording: an in-process `bulk_write` cannot
fail this way at all, so the worker wrapper adds a failure mode the storage contract does
not have. G-049.

### 3.5 Change events: one subscription channel, no per-event round trip

- Events are pushed from worker to caller as `id: 0` `Event` messages.
- A caller's first `change_stream()` sends a `Subscribe`; dropping the stream sends
  `Unsubscribe`. One worker-side subscription exists while at least one caller stream is
  alive, and is dropped when the last one ends.
- Events are **not** requested individually. A per-event round trip would add a hop of
  latency per change and would make a busy collection's event stream self-limiting under
  backpressure, which is a liveness bug rather than a performance one.
- If the caller's event queue is full, events are **coalesced by dropping the oldest
  unread bulks and setting a flag**, not by dropping the newest and not by unbounded
  growth. Rationale: a live query re-runs against current state when it sees a
  checkpoint gap, so a dropped old event is recoverable and a dropped new one is not.
  The `EventBulk` contract already carries `checkpoint` for exactly this.

### 3.6 What is *not* retried, and why

Nothing is retried automatically, in either direction. Stated explicitly because
"transport-level retries" is the obvious thing to add and it is wrong here:

- Retrying a **write** after an unknown outcome risks a double apply (3.4).
- Retrying a **read** is safe in isolation but changes the timing guarantees a caller's
  own deadline logic depends on, and it hides the fact that the worker died.
- Retrying the **handshake** is the one exception: reconnecting to a restarted worker is
  a new instance with a new `instance_token`, and the caller is told so rather than
  silently continuing against different state.

### 3.7 Counters, so a silently-dropping transport is observable

Dropping an unknown-id response is correct but must not be silent, or a protocol bug
looks like a missing result. The transport exposes counters: `requests_sent`,
`responses_matched`, `responses_dropped_unknown_id`, `events_delivered`,
`events_dropped_backpressure`, `heartbeats_missed`. A test asserts
`responses_matched == requests_sent` at quiescence, which turns "no mismatched
correlation" into an assertion rather than a hope.

## 4. Invariants

**INV-1 — transparency.** Every storage operation returns exactly what the same operation
returns in-process: same value, same order, same error kind. Only latency and event
arrival timing differ.
*Enforced by* one serialisation path per operation and one deserialisation path, with the
protocol enum exhaustive over the trait. *Proved by* `inv_01_transparency_over_transport`.

**INV-2 — the handshake establishes a version both sides implement.** A connection is
usable only if the offered and supported ranges overlap; the agreed version is the
highest in the intersection. A non-overlapping pair fails with `ProtocolMismatch`
carrying both versions, and no storage call is sent.
*Enforced by* the range-intersection check before any request. *Proved by*
`inv_02_handshake_negotiates_an_intersection`.

**INV-3 — request/response correlation is by id, not by order.** With responses arriving
in an order different from the requests, every response is matched to its own request.
*Enforced by* the id table, with `id: 0` reserved for handshake and pushes so a storage
response cannot be mistaken for either. *Proved by*
`inv_03_responses_pair_by_id_under_reordering`, which drives a worker that replies in
reverse order, and asserts `responses_dropped_unknown_id == 0`.

**INV-4 — the queue is bounded and nothing is dropped for capacity.** Offering more
concurrent calls than the bound blocks new sends; every call still completes.
*Enforced by* the semaphore. *Proved by*
`inv_04_backpressure_blocks_and_never_drops`.

**INV-5 — a dead worker fails fast and never hangs.** After the worker terminates, every
in-flight call and every subsequent call fails with `WorkerDied` within one heartbeat
interval, and none remains pending.
*Enforced by* the transport's exit callback marking the instance dead and draining the
in-flight table. *Proved by* `inv_05_dead_worker_fails_every_call`.

**INV-6 — an unresponsive worker is detected.** A worker that stops replying without
exiting is failed with `WorkerUnresponsive` after the heartbeat deadline, and its
in-flight calls fail with it.
*Enforced by* the heartbeat timer. *Proved by*
`inv_06_unresponsive_worker_is_detected`.

**INV-7 — one hung call does not kill the instance.** A single call exceeding its
deadline fails with `CallTimeout` and the instance remains usable, proven by a subsequent
call succeeding.
*Enforced by* per-call deadlines tracked separately from the instance's liveness.
*Proved by* `inv_07_a_hung_call_does_not_kill_the_instance`.

**INV-8 — a write is never retried across a worker death.** A `bulk_write` in flight when
the worker dies fails with `WorkerDiedDuringWrite` carrying the caller's context, and the
transport issues no second write.
*Enforced by* the write path having no retry and by the death path completing writes with
the distinct error kind. *Proved by*
`inv_08_write_is_never_retried_after_death`, which counts the requests the fake worker
received and asserts exactly one write was attempted.

**INV-9 — change events are neither lost nor duplicated by the transport.** Every event
the inner storage emits while a subscriber is attached reaches the subscriber once.
*Enforced by* one push channel per subscription and an unsubscribe on last-drop.
*Proved by* `inv_09_change_events_are_delivered_once`.

**INV-10 — the instance token is reported and changes on reconnect.** A reconnection after
a worker restart yields a different `instance_token`, so a caller cannot mistake a new
worker for the old one.
*Enforced by* echoing the worker's token in every handshake reply. *Proved by*
`inv_10_reconnect_reports_a_new_instance_token`.

**INV-11 — no unbounded memory growth under load.** The caller's queue is bounded and its
event buffer is bounded and coalescing; a long run under sustained load keeps both
bounded.
*Enforced by* both bound values. *Proved by*
`inv_11_queues_stay_bounded_under_sustained_load`.

## 5. Failure modes

| Failure | What the caller sees | What the transport does |
| --- | --- | --- |
| Worker file missing / constructor throws | `WorkerStartFailed` at construction | no instance is created |
| Protocol ranges do not overlap | `ProtocolMismatch { worker, caller }` | connection closed, no call sent |
| Worker terminates mid-call | `WorkerDied` for every in-flight call | instance dead, later calls fail fast |
| Worker terminates mid-`bulk_write` | `WorkerDiedDuringWrite { context }` | instance dead, **no retry** |
| Worker alive, silent | `WorkerUnresponsive` after the heartbeat deadline | instance dead |
| One call exceeds its deadline | `CallTimeout` | instance alive |
| Worker returns a storage error | the error, with the remote message | instance alive |
| Serialisation fails on the caller side | `SerializationFailed` for that call | instance alive; the call never reached the worker |
| Worker returns an unparseable or unknown-id response | counted in `responses_dropped_unknown_id` | the waiting call times out via INV-7 rather than hanging forever |
| Caller drops the storage instance | streams closed, in-flight calls cancelled | `Unsubscribe` sent for live subscriptions |
| SharedWorker: the shared worker already has a storage for this collection | a second instance over the same worker | allowed; the inner storage's multi-instance handling applies, unchanged |
| SharedWorker: caller sets `multiInstance: false` | documented as the fast path when the database only ever lives in the worker | passed through; this crate does not reinterpret it |

Error kinds, complete list: `WorkerStartFailed`, `WorkerDied`,
`WorkerDiedDuringWrite { context }`, `WorkerUnresponsive`, `CallTimeout`,
`ProtocolMismatch { worker, caller }`, `SerializationFailed`, `DeserializationFailed`,
`Remote(StorageError)`, `Closed`. Ten; nothing else is produced by this crate.

## 6. Test list

The tests that matter, each named after the invariant it proves. `inv_*` are invariant
tests; `acc_*` cover the observable behaviour in section 2.

| Test | Proves | How |
| --- | --- | --- |
| `inv_01_transparency_over_transport` | INV-1 | every trait method, in-process vs over a fake transport, results compared exactly |
| `inv_02_handshake_negotiates_an_intersection` | INV-2 | exhaustive over (caller range, worker range) pairs, including no-overlap |
| `inv_03_responses_pair_by_id_under_reordering` | INV-3 | a fake worker that replies in reverse; assert zero dropped-unknown-id |
| `inv_04_backpressure_blocks_and_never_drops` | INV-4 | offer > bound concurrent calls; all complete, peak queue == bound |
| `inv_05_dead_worker_fails_every_call` | INV-5 | kill the fake worker with N in flight; all N fail, none pending |
| `inv_06_unresponsive_worker_is_detected` | INV-6 | fake worker stops replying but stays alive; assert `WorkerUnresponsive` |
| `inv_07_a_hung_call_does_not_kill_the_instance` | INV-7 | one call exceeds its deadline; the next call succeeds |
| `inv_08_write_is_never_retried_after_death` | INV-8 | kill during `bulk_write`; assert exactly one write request reached the worker |
| `inv_09_change_events_are_delivered_once` | INV-9 | emit M events; assert M received, once each |
| `inv_10_reconnect_reports_a_new_instance_token` | INV-10 | two handshakes from different worker instances |
| `inv_11_queues_stay_bounded_under_sustained_load` | INV-11 | sustained load; assert queue and event buffer peaks |
| `acc_01_storage_is_usable_over_a_worker` | 2.1 | full read/write/query cycle through the transport |
| `acc_02_caller_supplies_the_worker` | 2.2 | a constructor-based `workerInput` is used and no bundled path is assumed |
| `acc_03_change_events_cross_the_boundary` | 2.3 | subscribe, write, observe |
| `acc_04_worker_death_is_reported_not_hung` | 2.4 | as INV-5/INV-8, from the caller's viewpoint with a wall-clock bound |
| `acc_05_sharedworker_shares_one_instance` | 1.1 | two callers on one shared transport see one storage instance |

Test seam: the transport is written against a `WorkerTransport` trait (send a message,
receive messages, observe termination), so all of the above run against a **fake
transport** in-process, on stable, with no browser and no Node. That is what makes the
supervision cases testable at all; a test that needed a real browser worker could not
simulate a mid-write death deterministically. The browser and Node adapters are thin
implementations of the same trait and are tested separately at the JS boundary.

## 7. Non-goals

- **No shared memory, no `SharedArrayBuffer`, no atomics.** That is the model decision
  (G-048); it would require nightly or a threads target and buys nothing this crate needs.
- **No pre-built worker bundle.** The caller supplies the worker (2.2).
- **No automatic retries.** Except the handshake, and it reports the new instance token
  (3.6).
- **No request pipelining optimisations that reorder a caller's calls.** Ordering is the
  caller's, and results pair by id regardless.
- **No transport-level encryption or authentication.** A worker is same-origin and
  in-process; encrypting field values is `dxdb-encryption`'s job and it composes *inside*
  or *outside* the worker boundary rather than being duplicated here.
- **No cross-origin worker support.** Out of scope and a browser policy question.
- **No `RxDB_UTILS_GLOBAL`/`PREMIUM_FLAG_HASH` machinery, no limits, no phone-home.**
  G-006.
- **No compatibility with upstream's worker protocol.** G-002: the wire format is ours,
  and an upstream worker cannot talk to a dxdb caller. Stated in the crate docs.

## 8. Performance targets

All **unmeasured**, and this crate is the one where the interesting number is a *cost*
rather than a gain.

| Target | Value | Why |
| --- | --- | --- |
| Round-trip latency added per call | **unmeasured**; target < 1 ms on the same machine | one message hop; a threads design would add ~0 but is not available (G-048) |
| Throughput for a 10 000-document `query` | **unmeasured**; the copy is expected to dominate | the honest cost of message passing, and the number that decides whether wrapping a fast storage in a worker is worth it |
| Main-thread time freed | **unmeasured** | the actual product benefit; needs a browser measurement, not a Rust benchmark |
| Memory per in-flight call | **unmeasured**; bounded by the queue at 64 | a number, not an intention |

The expectation, recorded before measuring so it cannot be retrofitted: for a *fast*
storage the worker wrapper is a net loss, and for a *slow* one it is a net gain on
perceived latency. `docs/BENCHMARKS.md` must show which, and the doc must say which.

## 8a. Verification record (partial — read the scope note first)

### Scope: what is built and what is not

This tranche lands **the protocol, the supervision state machine, and the transport seam**
— the parts that hold every testable failure mode. It does **not** land the
`RxStorageInstance` wrapper or the platform adapters. That split is stated rather than left
to be discovered:

| Piece | Status |
| --- | --- |
| `protocol` — `RequestKind`, `ToWorker`, `FromWorker`, version negotiation | **built, tested** |
| `supervision` — liveness, correlation, four-way failure split, counters | **built, tested** |
| `WorkerTransport` trait + fake transport | **built, tested** |
| `RxStorageInstance` wrapper over the transport | **not built** |
| Browser `Worker` / `SharedWorker` adapter | **not built** |
| Node `worker_threads` adapter | **not built** |

The wrapper is not a formality: it is where INV-1's full claim becomes assertable against
`dxdb-memory`. What `inv_01_transparency_over_transport` proves today is the weaker but real
statement that the *protocol* does not alter an operation — every `RequestKind` reaches the
worker intact with the caller's values, and the worker's answer is what the caller receives.
**Do not report INV-1 as fully proven until the wrapper exists and the in-process comparison
runs.**

All four workspace gates green (`check`, `clippy`, `test`, `wasm` all exit 0).

### Invariants proven

| Invariant | Test | Status |
| --- | --- | --- |
| INV-1 transparency | `inv_01_transparency_over_transport` | **partial** — protocol-level only |
| INV-2 handshake negotiates an intersection | `inv_02_handshake_negotiates_an_intersection` | proven, over a grid of ranges |
| INV-3 correlation by id, not order | `inv_03_responses_pair_by_id_under_reordering` | proven |
| INV-4 queue bounded, nothing dropped | `inv_04_backpressure_blocks_and_never_drops` | proven |
| INV-5 dead worker fails fast | `inv_05_dead_worker_fails_every_call` | proven |
| INV-6 unresponsive worker detected | `inv_06_unresponsive_worker_is_detected` | proven |
| INV-7 a hung call does not kill the instance | `inv_07_a_hung_call_does_not_kill_the_instance` | proven |
| INV-8 a write is never retried after death | `inv_08_write_is_never_retried_after_death` | proven |
| INV-9 events neither lost nor duplicated | `inv_09_change_events_are_delivered_once` | proven |
| INV-10 reconnect reports a new instance token | `inv_10_reconnect_reports_a_new_instance_token` | proven |
| INV-11 queues stay bounded | `inv_11_queues_stay_bounded_under_sustained_load` | proven |

Plus three supporting tests: the reserved id is never allocated, an unknown response is
counted rather than guessed at, and a remote error leaves the instance alive.

### The design bug the invariants caught

**The default settings were self-contradictory, and it mattered.** Section 3.4 specified a
15 s heartbeat timeout and a 30 s per-call deadline. With those numbers a call overrunning
its deadline had *already* tripped the heartbeat first, so `CallTimeout` was unreachable and
every slow call would have been reported as a dead worker — tearing down a healthy
instance, which is exactly the failure the four-way split exists to prevent. `inv_07`
caught it by asserting the relationship between the two values.

Fixed in both directions:

- the **evaluation order** in the state machine now checks per-call deadlines *before* the
  liveness check, so a call that overran is attributed to the call;
- the **defaults** now have `heartbeat_timeout_ms (45 s) > call_timeout_ms (30 s)`, giving a
  slow call its full deadline before the connection is suspect.

The values remain acknowledged guesses (G-050). What is no longer a guess is the **ordering
relationship**, which is now an invariant with a test.

`inv_06` then failed for the *right* reason: with ordering fixed, the unresponsive-worker
test tripped its per-call deadline first, because its one in-flight call had the shorter
deadline. The test is isolated by giving that call a deadline longer than the heartbeat
window, separating the two branches; the shipped relationship is asserted separately.

## 9. Open questions this spec does not settle

1. **Which JS adapter lands first.** The Rust transport can be built and tested against
   the fake in this tranche; the browser `Worker` adapter and the Node `worker_threads`
   adapter are thin but not free, and the Node one is testable in CI while the browser one
   needs Playwright (which the port's gates already use for `dxdb-js`). Implementing
   transport-first and adapting second is the plan; whether both adapters land in B3 or
   Node lands in B3 and browser in B4 is not decided here.
2. **The heartbeat and deadline values.** 5 s / 15 s / 30 s are starting points chosen so
   a slow phone does not trip them. They are configuration in the crate, and this spec
   does not claim they are right until a real device says otherwise.
3. **Whether a `SharedWorker` mode needs anything beyond instance naming in Rust.** If the
   JS boundary owns the `MessagePort` acquisition, the Rust side may be identical for both
   modes, in which case section 1.1's enum is a JS-side concept and this crate does not
   need it. Resolve during implementation rather than pre-emptively adding a field.
