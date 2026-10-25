# Clean-room gap register

Every place where public documentation was insufficient to write a specification,
what we did instead, and how a reader can tell. CLEANROOM.md section 6 makes this
a deliverable rather than an apology: the differences between dxdb and the paid
upstream features are documented here, so nobody has to discover them in
production.

Append-only. A decision is never edited away; a superseded decision keeps its id
and gains a "superseded by" note. Codes are `G-nnn`.

## How to add a row

1. You were writing a spec and the public docs did not answer a design question.
2. Add a row with: the question, the state of the public evidence, the decision,
   the risk if we are wrong, and what would resolve it.
3. Reference the row from the spec section it affects.
4. Do not guess an upstream constant or format to fill the hole. Choose our own
   value, say that we chose it, and record it here.

## Standing decisions that apply to every clean-room crate

### G-001: no read access to the reference implementation

**Question.** What does the paid `logger` (or any other premium module) actually
emit, byte for byte?

**Public evidence.** The plugin pages describe which operations are logged, the
option names, and the callbacks. They do not describe the emitted format, the
log line encoding, the timing mechanism's precision, or the error object shape.

**Decision.** We do not reproduce the upstream output format. `dxdb-logger` emits
dxdb's own documented event schema (fields in `specs/logger.md` section 2),
mapped onto Sentry's public span vocabulary. A user who needs the old log lines
needs a formatter we have not written.

**Risk if wrong.** Someone who grep-parses upstream's log lines has to change a
log parser. This is a small, visible difference, and it is stated in the crate
docs.

**Would resolve it.** A purchased license plus the black-box protocol in
CLEANROOM.md section 6 (record outputs, do not read source). Not available today.

### G-002: the paid features are not wire-compatible, by construction

**Question.** Can dxdb read a database written by the paid storage modules?

**Public evidence.** No format documentation exists. The Apache-2.0 source covers
only the open storages.

**Decision.** No. Every clean-room storage and every clean-room envelope reads
and writes its own documented format. The supported migration path is the
portable one: export documents, import documents. Each affected crate's docs say
this in its first paragraph, per CLEANROOM.md section 3.

**Risk if wrong.** A user expecting to point dxdb at an existing paid-tier
database gets a documented refusal instead of silent corruption. That is the
better failure.

**Would resolve it.** A published format specification from the vendor, obtained
properly.

### G-003: performance numbers are targets, not facts

**Question.** Are upstream's published numbers (3-4x on OPFS, 30-50% read and
25% write gains on sharding, ~200 ms startup saving for the meta cache)
reproducible?

**Public evidence.** Aggregate percentages published without hardware, dataset,
or measurement method.

**Decision.** They are recorded as *targets* in each spec's section 5 and are
never restated as our results. Any comparative sentence ships only with a number
from `docs/BENCHMARKS.md` (PLAN.md 7.5). Where our measurement disagrees, our
number wins and the divergence is noted.

**Risk if wrong.** We under-promise. Acceptable.

**Would resolve it.** Our own benchmarks, one per storage, run on named hardware.

### G-004: the public API shape of our crates is ours, not upstream's

**Question.** Should dxdb's API mirror the upstream option object exactly, or
follow the wrapper pattern the port already uses?

**Public evidence.** The plugin pages show the JS option object
(`wrappedLoggerStorage({storage, settings, onOperationStart, ...})`). That shape
reflects a JS object-literal idiom and a callback trio, not a design requirement.

**Decision.** The documented *capability* is preserved (every listed operation
can be switched on and off; there is a prefix; there is a timing switch; there
is a meta-storage filter; there are start/end/error hooks). The *shape* follows
dxdb's Rust conventions: a typed settings struct with defaults, and a trait for
the hooks. `specs/<feature>.md` maps documented JS option names onto the Rust
fields one-to-one in a table, so a reader can check coverage by reading a table.

**Risk if wrong.** A JS consumer porting code from the paid plugin needs a thin
adapter at the dxdb JS boundary. That adapter is cheap and belongs to the JS
surface crate, not to us.

**Would resolve it.** Nothing; this is a deliberate divergence in favour of the
host language.

### G-005: `info` is documented but absent from the public storage interface

**Question.** The logger plugin's public settings list an `info` operation, but
the Apache-2.0 `RxStorageInstance` has no `info()` method
(`src/types/rx-storage.interface.d.ts`, methods at `:164`, `:180`, `:208`,
`:220`, `:227`, `:246`, `:288`, `:308`, `:314`).

**Public evidence.** The logger docs list `info: true` among the operation
toggles. The open interface has no such method. Either the operation is a
premium-side concept, or the docs are stale, or a newer upstream release adds it.

**Decision.** `dxdb-logger` defines the "operation name" enum as an open set that
includes `info` so the documented toggle exists, and the wrapper logs it only if
the underlying storage exposes an informational call through dxdb's own
`SpecEnvelope`-style extension point. Nothing is invented on the open trait.

**Risk if wrong.** An operation the user expected to see in the log is missing.
Visible in a test, listed here.

**Would resolve it.** A public upstream release note describing the method.

### G-006: no artificial limits, no phone-home, no flag check

**Question.** The Apache-2.0 tree contains the premium gates
(`utils-premium.ts` `PREMIUM_FLAG_HASH`, the 13-collection
`NON_PREMIUM_COLLECTION_LIMIT`, the SQLite trial caps, the dev-mode tracking
iframe). They are readable and could be ported verbatim. Do we?

**Public evidence.** All four are in the open tree and are described in
CLEANROOM.md section 3.

**Decision.** No. dxdb ships no artificial limits and no phone-home, and no code
on this lane references `PREMIUM_FLAG_HASH`, `RXDB_UTILS_GLOBAL`,
`hasPremiumFlag`, or the collection limit. This is an intentional divergence
from upstream, and `docs/STATUS.md` parity rows mark those specific behaviours as
"intentionally not ported".

**Risk if wrong.** Users get more capability than the paid tier for free; no
technical risk to us.

**Would resolve it.** Nothing. It is a product decision, made in CLEANROOM.md
section 3, and it is not ours to revisit per crate.

### G-007: two storages are shipped as one merged clean-room module upstream

**Question.** `storage-memory-mapped` and `storage-memory-synced` are documented
as separate products with different trade-offs, and the CLEANROOM table groups
them into one `dxdb-memory-tier`. Which behaviour do we implement first?

**Public evidence.** `docs-src/docs/rx-storage-memory-mapped.md` and
`rx-storage-memory-synced.md` (public pages exist for both). The pricing page
lists mapped but not synced, which is an upstream documentation inconsistency.

**Decision.** Deferred to B4, where the spec must state which of the two it
specifies and why, per CLEANROOM.md section 4 ("do not invent a distinction we
cannot source"). Recorded here now so it is not lost.

**Risk if wrong.** Deferred work, no rework.

**Would resolve it.** The B4 spec, which must cite the public pages.

## B1: `dxdb-logger`

### G-020: `dxdb-storage` publishes no storage traits yet

**Question.** The logger is defined as a storage wrapper, but the trait it wraps
did not exist when B1 started: `crates/dxdb-storage/src/lib.rs` was a
nine-line scaffold ("the traits land in M2"), owned by lane D.

**Public evidence.** The Apache-2.0 interface is fully documented
(`src/types/rx-storage.interface.d.ts`), and PLAN.md 4.2 fixes the trait shape.
So the *contract* is known; only the code is missing.

**Decision.** `dxdb-logger` defines the trait it wraps as its own explicit
specification envelope, in `dxdb_logger::storage`, mirroring the documented
method set and its argument and return shapes one-to-one. The crate has no
dependency on `dxdb-storage`, so there is no cross-lane file conflict and no
placeholder that pretends to be the real trait. When lane D lands the real
traits, a bridge module implements them over the envelope (or the envelope over
them) as a mechanical change, and this row gains a "resolved" note with the
commit that did it. The point of doing it this way is that the logger proves the
clean-room *pipeline* end to end, which is B1's actual exit gate, without
blocking on another lane.

**Risk if wrong.** A bridging commit later, roughly a page of delegation code.
The alternative, waiting, blocks B1 indefinitely on lane D.

**Would resolve it.** Lane D landing `RxStorageInstance`-equivalent traits in
`crates/dxdb-storage/`.

### G-021: the public docs do not say what "log" means

**Question.** "It will log all operations" could mean structured events, console
lines, timing marks, or APM spans. The option `times: true` "will log timings
with `console.time()` and `console.timeEnd()`" implies console lines alongside
whatever else is emitted.

**Public evidence.** `docs-src/docs/logger.md`, quoted above. The page names
Sentry and other APMs as the *purpose*, which implies structured events rather
than console text, but never says so.

**Decision.** dxdb emits **structured events through `tracing`**, one event per
operation at start and one at end (or error), with the documented field set in
`specs/logger.md` section 2. `tracing`'s own `tracing-subscriber` ecosystem
covers the console case (`fmt` layer) and the Sentry case (a span exporter
layer), so `times: true` maps to "emit the two events that carry start and
duration" rather than to a `console.time` call. No `console.time` anywhere: it
is a JS browser API with no Rust equivalent, and inventing one would be worse
than documenting the mapping.

**Risk if wrong.** Console output looks different. The structured event carries
strictly more information than a timing line, so nothing is lost.

**Would resolve it.** A public example of the emitted output. There is none.

### G-022: timing on `wasm32-unknown-unknown` has no clock in `std`

**Question.** Duration measurement needs a clock, and
`std::time::Instant::now()` aborts on `wasm32-unknown-unknown` ("time not
implemented on this platform", see the static-library implementation note in the
Rust std `sys/unsupported/time.rs` and the discussion in rust-lang internals
"is `std::time::Instant` on webassembly possible?"). The workspace gates require
`make wasm` (`cargo check --workspace --target wasm32-unknown-unknown`) for
every crate, including this one.

**Public evidence.** The panic is documented behaviour of the target, not a bug,
and `SystemTime::now()` has the same problem. In a browser the correct clock is
`performance.now()`, which dxdb reaches through `web-sys`/`js-sys` in the JS
surface crate, not here.

**Decision.** `dxdb-logger` never calls a clock itself. It takes a `Clock`
abstraction (`fn now_ns(&self) -> u64`, a monotonic nanosecond reading) with two
implementations: `MonotonicClock`, which uses `Instant` on every target where a
clock exists, and which is therefore *not* compiled for
`wasm32-unknown-unknown`; and a caller-supplied clock, which is what the wasm
build uses and what tests use to make time deterministic. The crate compiles for
`wasm32-unknown-unknown` with the `std` clock absent, and asks the host for one
at construction. This also makes the timing tests exact instead of flaky.

**Risk if wrong.** A wasm user must pass a clock, which is one line and is
already required to get real timings in a browser.

**Would resolve it.** Nothing; the target has no clock and that is not going to
change.

### G-023: the docs name an operation the open trait calls something else

**Question.** The settings list has `info`, `getChangedDocumentsSince`,
`cleanup`, `close`, `remove`, `bulkWrite`, `findDocumentsById`, `query`, `count`,
`getAttachmentData`. Nine of those ten exist on the open interface; `info` does
not (G-005). `getChangedDocumentsSince` is *optional* upstream
(`src/types/rx-storage.interface.d.ts:246`) and RxDB supplies a query-based
fallback (`src/rx-storage-helper.ts:936-960`), so a wrapper that has no inner
implementation must not fabricate one.

**Public evidence.** The interface and the helper, both Apache-2.0, both cited.

**Decision.** The wrapper forwards `get_changed_documents_since` only when the
inner storage reports support (`supports_get_changed_documents_since()`), and
returns the documented not-supported answer otherwise, so the upstream-style
fallback stays above us. The operation-name enum has both names, and the
acceptance list tests the optional-operation behaviour explicitly.

**Risk if wrong.** A caller relying on a fabricated fallback gets an explicit
answer instead of a silently slow one.

**Would resolve it.** Nothing; this is the correct reading of the open contract.

### G-024: "meta storage instances" has no public marker in dxdb

**Question.** `metaStorageInstances: false` suppresses "meta storage instances
like used in replication". What identifies one?

**Public evidence.** Upstream names them by collection name:
`INTERNAL_STORAGE_NAME = '_rxdb_internal'` (`src/rx-storage-helper.ts:51`, used at
`src/rx-database.ts:769`) and `RX_DATABASE_LOCAL_DOCS_STORAGE_NAME =
'rxdatabase_storage_local'` (`src/rx-storage-helper.ts:52`). Those two strings are
the only public signal, and they are Apache-2.0 constants, so reading them is
allowed. The replication plugin's meta instances reuse `_rxdb_internal`.

**Decision.** dxdb detects meta storage instances by collection name against a
published list of two names, which the crate re-exports as
`storage::meta::INTERNAL_COLLECTION_NAME` and `LOCAL_DOCUMENTS_COLLECTION_NAME`,
with a doc comment citing the upstream constant's origin. Because the string
values are fixed by the SQLite/JSON on-disk naming *of the open tree*, not by the
paid plugin, taking them from the Apache-2.0 source is legitimate; this row
records that we did.

**Risk if wrong.** A meta instance the user cared about is filtered out of the
log. It is one settings toggle away from being logged again.

**Would resolve it.** A dxdb-side explicit marker, which is a future change to
the port, not to this crate.

### G-025: the docs do not say whether a failing operation logs an end event

**Question.** With `onOperationError` and `onOperationEnd` both provided, does a
failed operation produce both?

**Public evidence.** The callback list implies three terminal outcomes are
possible, but the page does not say whether `end` follows `error`.

**Decision.** Exactly one terminal event per operation: `end` on success, `error`
on failure, never both. The start event is always paired with exactly one
terminal event, which is stated as an invariant (`INV-2` in
`specs/logger.md`) and property-tested. Rationale: a caller registering only
`onOperationEnd` and measuring elapsed time would otherwise double-count, and a
caller registering only `onOperationError` would miss nothing.

**Risk if wrong.** A `console.timeEnd`-style consumer sees one fewer line.
Documented, and the invariant is the more defensible reading.

**Would resolve it.** A public example showing both callbacks firing.

### G-026: Sentry compatibility is a field set, not an SDK dependency

**Question.** "Compatible with Sentry" could mean emit Sentry envelopes, or emit
data that maps onto Sentry's model.

**Public evidence.** The Sentry span interface is a public, stable JSON contract
(required `span_id`, `trace_id`, `start_timestamp`, `timestamp`; optional
`status`, `description`, `data`, `tags`, `origin`). The docs page for the logger
names Sentry as one of several APM targets, not as a dependency.

**Decision.** Compatibility is a **documented field set** and a conversion
function, `SentrySpan`, in `dxdb_logger::sentry`, which maps one dxdb operation
event (or a start/end pair) onto the Sentry span JSON shape with the public
status vocabulary. No Sentry SDK dependency, no network, no API key, no
transport. Users wire it to their own Sentry client. This satisfies "no
phone-home" for free and keeps the wasm build small.

**Risk if wrong.** A user wanting a built-in transport writes one. The field
mapping is already tested for shape.

**Would resolve it.** Nothing; adding an SDK later is a feature request, not a
spec change.

## Closed by fact, not by decision

### G-027: which upstream modules are in the Apache-2.0 tree (recorded, not decided)

**Question.** How much of the CLEANROOM table can be read as Apache-2.0 source?

**Public evidence, checked against the pinned upstream tree
(`pubkey/rxdb` @ `3b763ee`, v17.6.0, 2026-10-25).** Of the 17 modules in the
CLEANROOM table, exactly **one** exists in the open tree: `storage-sqlite`
(`src/plugins/storage-sqlite`). The other 16 module paths
(`storage-indexeddb`, `storage-opfs`, `storage-filesystem-node`,
`storage-filesystem-expo`, `encryption-web-crypto`, `flexsearch`,
`storage-worker`, `storage-sharding`, `storage-memory-mapped`,
`storage-memory-synced`, `storage-localstorage-meta-optimizer`,
`query-optimizer`, `server-adapter-fastify`, `server-adapter-koa`, `logger`,
`shared`) are **not** present as directories in the open tree. The open tree
does ship different, unrelated storages under similar names (`storage-dexie`,
`storage-localstorage`, `storage-memory`, `storage-remote`, `storage-denokv`,
`storage-foundationdb`, `storage-mongodb`).

**Consequence for this lane, and it is the important one.** CLEANROOM.md section
1 says the port reads the Apache-2.0 core; for the paid feature modules there is
nothing to read even for the *public* behaviour beyond the docs pages. That makes
the specs thinner than they look, makes G-001 permanent for B1-B5, and means the
"cross-check against the open-source baseline" technique in CLEANROOM.md section
6 only works where an open storage implements the same contract (`dxdb-memory`,
`crates/storages/*`) — it cannot be used to check a premium-only behaviour.

**Where the documentation lives, then.** The behaviour of the 16 premium-only
modules is documented on the public docs site *independently of the source tree*:
`docs-src/docs/rx-storage-{indexeddb,opfs,sqlite,filesystem-node,filesystem-expo,
worker,shared-worker,sharding,memory-mapped,memory-synced,localstorage-meta-optimizer}.md`,
`docs-src/docs/{logger,encryption,fulltext-search,query-optimizer}.md`. That is
the corpus the specs are written from. It is prose, it is thin, and it is what
G-001 is about. There is **no page for the server adapters** (Fastify/Koa) in the
pinned tree, which is a B5 problem recorded now rather than discovered later.

**Also recorded:** `storage-sqlite` is *both* an Apache-2.0 port target for lane
G and a paid-tier module name for lane I. `specs/README.md` resolves that split:
the engine is lane G's `crates/storages/dxdb-sqlite`, and the clean-room crate is
a capability wrapper with no engine inside it.

**Nothing to decide.** This row exists so a reviewer can see that the shape of
the plan rests on the pinned tree, not on an assumption about it.
