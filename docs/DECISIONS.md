# Decision log

Cross-cutting decisions that more than one lane depends on. Each entry records the
decision, the alternative that was rejected, and the reason — because the reason is
what a lane needs when its own case looks different.

A decision here is binding. Changing one requires the leader, because every lane that
reads it has built on it.

---

## D1: The document type lives in `dxdb-storage`, not `dxdb-core`

**Decided:** 2026-10-25. **Raised by:** lane E (replication), lane D (core).

`RxDocumentData` and its companions (`_rev`, `_deleted`, `_meta`, `_attachments`) are
defined in `dxdb-storage`.

**Rejected:** defining them in `dxdb-core`.

**Why:** the dependency direction is the whole point of PLAN.md 4.1. `dxdb-core`
depends on `dxdb-storage` traits and never on a storage crate. If the document type
lived in core, then `dxdb-memory`, `dxdb-indexeddb`, `dxdb-sqlite`, and every clean-room
storage wrapper would have to depend on `dxdb-core` to name the type they store — which
inverts the layering, creates a core↔storage cycle at the crate level, and means the
clean-room crates (which must not depend on the port's core) could not implement the
storage trait at all.

**Consequence:** `dxdb-storage` is the bottom of the graph. It may depend on
`dxdb-jsnum`, `dxdb-jsstr`, and `dxdb-json` (encoding), and on nothing else from this
workspace. Replication and core both depend on it.

---

## D2: Streams are `futures::Stream` plus a wake channel, never an `async next()` loop

**Decided:** 2026-10-25. **Raised by:** lane E.

Change streams, master change streams, and query results are `futures::Stream`. Where
the protocol needs push-delivery batching semantics, it uses a wake channel alongside
the stream.

**Rejected:** driving the protocol from an `async fn next()` loop over the stream.

**Why:** lane E's analysis of `downstream.ts`/`upstream.ts` found the queue-wake logic
is written against push delivery — `addNewTask` appends to a promise chain,
`processTasks` merges whatever is currently open, and there is a
`time < lastTimeMasterChangesRequested` drop rule. An `async next()` loop changes both
the batching and that drop rule: it would converge in the happy path and diverge under
contention. PLAN.md 3.3 already commits to `futures::Stream`; this entry records the
part that is easy to get subtly wrong.

**Consequence:** a lane that finds `next()` convenient must add the wake channel, not
change the delivery model.

---

## D3: Execution is injected through a `Spawner` capability, defaulting to sequential

**Decided:** 2026-10-25. **Raised by:** lane E. **Implementing lane:** D, in
`dxdb-storage`.

The core defines no runtime and spawns nothing itself. Where upstream relies on
"fire and forget, then await at the end" (`persistFromMaster`, the un-awaited
`setCheckpoint`), the code takes a `Spawner`:

- The **default** spawner keeps the future in the caller's queue. Sequential execution
  is therefore the default behaviour, and it is correct — just not optimal.
- **Bounded parallelism is opt-in**, supplied by the platform binding: `wasm-bindgen-futures`
  in JS, a WASI poll loop in WASI, an async runtime in native code.
- A `Spawner` implementation must be able to report whether it actually spawns, so a
  test can assert it exercised the sequential path.

**Rejected:** a hard `tokio::spawn`, and a `'static`-only bound that forces every caller
into a runtime.

**Why:** PLAN.md 3.3 and section 9 bar `tokio` from the core by policy — it compiles for
wasm32 but its timers and IO do not work there. A no-op-default spawner keeps the core
executor-neutral, keeps the wasm build honest, and makes the sequential path the one
that is tested by default rather than the one that is untested.

**Consequence for tests:** the conformance suites run on the sequential spawner, so
correctness is proven without a runtime. A separate test exercises a real parallel
spawner to prove the opt-in path.

---

## D4: Time is injected, and the default is process-global monotonic

**Decided:** 2026-10-25. **Raised by:** lane E.

`now()` is a capability on the replication state (and available to the document layer),
not a direct call to a clock. The default implementation reproduces upstream's
process-global monotonic clock; tests inject a deterministic one.

**Rejected:** calling the platform clock directly, and making every call site thread a
clock through.

**Why:** two reasons, both concrete. First, upstream's `now()` is a module-global, so two
replications in one process share it; a per-instance clock would change `_meta.lwt`
ordering in any process with two replications, and ordering is load-bearing for conflict
resolution. Second, `lwt` values appear in stored documents and in replication vectors, so
tests need to be able to pin them exactly.

**Upstream shape that must be reproduced, not just the value:** `lwt` is
milliseconds plus `k * 0.01` for `k` in 1..=99, two decimals, in upstream's expression
order, formatted by `dxdb-jsnum`. A whole-millisecond clock produces ties that the
ordering logic relies on being broken. Lane E recorded this; it is repeated here because
it is the kind of detail that looks like an implementation choice and is actually wire
behaviour.

---

## D5: `localeCompare` is not reproduced; code-unit order is the contract

**Decided:** 2026-10-25. **Required by:** PLAN.md 3.2.

All string ordering uses `dxdb_jsstr::compare_js` (UTF-16 code units). Upstream's
`localeCompare`-based sorts (`utils-object.ts:207`, `:220`) are not reproduced.

**Why:** ICU collation depends on a locale data version that changes between Node
releases. A "compatible" collation would match one specific ICU build and be quietly
wrong with the next. A stable, testable order that differs from upstream in an exotic
case is a better contract than an unstable one that matches today. None of the
data-integrity surfaces — index strings, revisions, storage order — use `localeCompare`,
so the divergence does not affect stored data.

**Consequence:** documented as an intentional divergence in `docs/STATUS.md`, so a
future reader does not find it as a bug.

---

## D6: No artificial limits and no phone-home, as an intentional divergence

**Decided:** 2026-10-25. **Required by:** CLEANROOM.md section 3.

The gates in the Apache-2.0 tree — `utils-premium.ts`, `NON_PREMIUM_COLLECTION_LIMIT`,
the SQLite trial caps, the dev-mode tracking iframe — are legally portable and are
deliberately not ported.

**Why:** a port that reproduces another vendor's paywall is a bad look and a support
burden, and we are not the licensor of those features.

**Consequence:** these are marked `not ported (intentional)` in `docs/STATUS.md` and are
not parity gaps. A reviewer comparing against upstream must not "fix" this.

---

## D7: The protocol's checkpoint is generic and opaque; the storage's is concrete

**Decided:** 2026-10-25. **Raised by:** lane E, reviewing the merged `dxdb-storage`.

`dxdb-storage` defines `Checkpoint { id: String, lwt: f64 }`. That is the right type
for *a storage's own change feed*, and its `to_json()` satisfies the round-trip
requirement (`lwt` goes through `dxdb_json`, so it is formatted with JavaScript number
semantics).

It is **not** the protocol's checkpoint type, and the replication protocol must not be
constrained to it. Upstream is explicit about why
(`src/replication-protocol/checkpoint.ts:92`):

> Instead of just storing the new checkpoint, we have to stack up the checkpoint with
> the previous one. This is required for plugins like the sharding RxStorage where the
> changeStream events only contain a Partial of the checkpoint.

Upstream's `CheckpointType` is a type parameter; `stackCheckpoints` is `Object.assign`
over arbitrary objects; a checkpoint is only ever handed back to the caller's own
`masterChangesSince`. A sharding checkpoint is not `{id, lwt}`, and a transport may use
`{seq}` or `{cursor, region}`. Stacking two `Checkpoint` structs would silently drop
every key that is not `id` or `lwt` — data loss in the one operation the stacking exists
to make safe.

**Decision:** the replication protocol is generic over `C: JsonCheckpoint` (any
ordered-JSON object), with `stack_checkpoints` as a shallow left-to-right merge where
later wins and key order is first-insertion. `dxdb_storage::Checkpoint` is one concrete
`C` that a caller may use, and the storage-to-storage adapter does exactly that. Nothing
in `dxdb-storage` changes.

**What `C` must satisfy**, asserted in one place:

1. It encodes to a JSON object, not a scalar or an array.
2. Its key order survives a round-trip, so ordered-JSON encoding is used on both sides.
3. Merging is shallow, left-to-right, later-wins, first-insertion key order.

**Rejected:** widening `dxdb_storage::Checkpoint` into a map, and making every storage
carry an opaque checkpoint. Both push the protocol's generality into a layer that does
not need it, and the storage's typed checkpoint is genuinely more useful to storage
implementors.

**Consequence:** a lane that finds `Checkpoint` inconvenient for the protocol has found
a real constraint, not a bug. The generic parameter is the answer.
