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

### G-028: Rust futures can be cancelled; a JavaScript promise cannot

**Question.** The documented callback trio is `onOperationStart`,
`onOperationEnd`, `onOperationError`. What should happen when the caller drops the
operation before it finishes?

**Public evidence.** Nothing, because the question cannot arise upstream: a
JavaScript `Promise` has no cancellation, and the paid plugin's callback set is
exactly three, so the situation it does not model could not have been observed.

**Decision.** In Rust the situation is routine (`select!`, a timeout wrapper, a
cancelled query), so `dxdb-logger` adds a fourth phase, `Phase::Cancelled`, and a
fourth hook, `OperationHooks::on_operation_cancelled`, whose default is a no-op.
The terminal-record guard emits `Cancelled` from `Drop` when the future is dropped
mid-flight. Without this, a start record would be left unpaired and `INV-2` would
be false exactly where interruption is most likely. The phase maps onto the public
Sentry status `cancelled`, so nothing is invented at the APM boundary.

**Risk if wrong.** A consumer with an exhaustive match on the record phases needs
a new arm. That is a compile error, not a silent behaviour change, and the default
hook means a caller that never cancels sees nothing new.

**Would resolve it.** Nothing; the two languages have different concurrency
models and the divergence is the correct one.

### G-029: a start record belongs to first poll, not to future construction

**Question.** When a caller builds a storage-operation future and polls it later —
or never — at what moment should the start record be emitted?

**Public evidence.** Nothing directly, because the question does not exist upstream:
a JavaScript `Promise` begins executing when it is constructed, so "the call
started" and "the future was created" are the same instant, and the paid plugin's
three callbacks never had to distinguish them.

**Decision.** The start record, the log id allocation, and the first clock read all
happen **inside** the returned future, at its first poll. Three consequences, all
deliberate:

- An operation that is never polled emits nothing and reads no clock. Constructing
  a future is free.
- Log ids increase in **poll** order, not call order (`INV-4`).
- A caller that builds two futures and lets the first run to completion before
  polling the second will see pair A complete before pair B opens. That is the
  caller's schedule, not the logger's, and `INV-7` is written about pairs rather
  than about globals for exactly this reason.

The alternative — emitting at construction — was rejected because it would make an
unpolled future cost a clock read and, worse, would emit a start record that can
never be closed if the future is dropped before its first poll, breaking `INV-2` in
the ordinary `select!` case.

**Risk if wrong.** A user correlating log order with their own call order sees pair
B open after pair A closes, even though B was constructed first. Documented in
`specs/logger.md` INV-7 and in the crate docs.

**Would resolve it.** Nothing; this follows from `INV-2` plus the Rust execution
model. Recorded because it was a genuine spec/implementation mismatch caught at the
merge gate, and the resolution is a spec correction, not a test weakening.

## B2: `dxdb-encryption`

### G-030: the Apache-2.0 `crypto-js` envelope is readable and deliberately not reproduced

**Question.** The Apache-2.0 tree ships `src/plugins/encryption-crypto-js/index.ts`, a
233-line encryption plugin. It is readable. Should `dxdb-encryption` be
compatible with it?

**Public evidence.** The whole plugin is Apache-2.0 and was read. What it does:
`AES.encrypt(value, password)` (`:29`) and `AES.decrypt` (`:34`) from crypto-js,
called with the raw password string. CryptoJS's password path derives the key with
OpenSSL's `EVP_BytesToKey` — a single MD5-based pass with no iteration count — and
the value is a JSON-stringified field. There is no AAD, no version byte, no algorithm
identifier, and no parameters in the ciphertext. It also uses AES in default CBC mode,
which is unauthenticated.

**Decision.** Compatibility is **possible and refused**. Reproducing that envelope
would mean shipping a fast, non-memory-hard KDF and an unauthenticated cipher mode in
a format we then have to keep supporting for every existing database. Our envelope is
`DXE1` (XChaCha20-Poly1305 + Argon2id + AAD), `specs/encryption.md` section 3. A user
with a `crypto-js`-encrypted database has one migration path: open it with the open
plugin, export documents in plaintext, import them into dxdb. There is no in-place
path and we are not building one.

**Risk if wrong.** A `crypto-js` user must export/import. That is a documented,
scriptable step, and the alternative is shipping a KDF we would have to describe as a
security floor.

**Would resolve it.** Nothing. This is the CLEANROOM.md section 3 framing applied to
the one paid feature whose open sibling we *can* read: being able to copy it is not a
reason to.

### G-031: encrypted paths in queries — upstream ignores them, we reject them

**Question.** The docs say encrypted fields "cannot be used as operators in queries".
What does "cannot" mean operationally: no matches, or an error?

**Public evidence.** `docs-src/docs/encryption.md`: "you cannot perform queries like
'find all documents where the encrypted field equals a certain value.' RxDB does not
expose the encrypted data in a way that allows direct querying based on the encrypted
content." The open plugin's read path decrypts after the storage has already filtered
(`modifyFromStorage` in `encryption-crypto-js/index.ts`), so upstream's storage sees
ciphertext and the matcher compares against it, which yields **no matches** rather than
an error.

**Decision.** dxdb **rejects** a selector or sort that names an encrypted path with
`EncryptedPathInQuery`, at query preparation, before the storage is called
(`specs/encryption.md` INV-12). Silent no-matches is the worst possible outcome: the
caller believes their search works and gets an empty list forever, with no signal.

**Risk if wrong.** A caller porting code that silently returned nothing now gets an
error they must handle. That error is the bug report they needed.

**Would resolve it.** A public statement from upstream that "cannot" means "returns an
error". None exists.

### G-032: the documented minimum password length is 8, and we reject that number

**Question.** `MINIMUM_PASSWORD_LENGTH = 8`
(`encryption-crypto-js/index.ts:32`) with `EN1`/`EN2` errors around it. Do we adopt it?

**Public evidence.** The constant is in the Apache-2.0 source and is a documented
behaviour: passwords shorter than 8 throw. It is also below every current guideline —
NIST SP 800-63B's direction is length-based with no composition rules and a much
higher verifier-side floor, and 8 characters is trivially within offline-guess reach
for a memory-hard KDF or otherwise.

**Decision.** dxdb does **not** impose a hard 8-character minimum. It computes an
entropy estimate and refuses only trivially guessable passwords
(`PasswordTooWeak`), while allowing long passphrases with no composition rules. The
floor is a property of the *password*, not its length. Rationale: Argon2id at 64 MiB
makes a genuine 8-character random password survivable and an 8-character dictionary
password not; a length check cannot tell them apart and gives users a false sense of a
security floor.

**Risk if wrong.** A password that upstream would reject, dxdb accepts — in the
permissive direction, which is the direction that does not lock anyone out of their
data.

**Would resolve it.** Nothing; the constant is a legacy default.

### G-033: key rotation does not exist upstream, and we are adding it

**Question.** The docs say "it is not possible to change the password of a database",
and offer two workarounds (migrate, or a meta-password in a local document). Neither
lets a user invalidate a *leaked derived key* while keeping their password.

**Public evidence.** `docs-src/docs/encryption.md`, "Changing the password", quoted in
`specs/encryption.md` 1.6.

**Decision.** Add `rotate()`: derive generation `n+1` from the same password with a new
KDF salt, re-encrypt every field, keep both generations readable during the rewrite,
and drop generation `n`'s salt on completion (`specs/encryption.md` 2.6, INV-7). This
is an addition, not a compatiblity issue: it changes no format an upstream user has,
because no upstream ciphertext is readable by us anyway.

**Risk if wrong.** Extra surface (resumable rewrite, two-generation read path) for a
capability users may not ask for. The alternative is telling a user whose device was
stolen that their only remedy is a full migration.

**Would resolve it.** Nothing; it is a gap in the paid product that we are choosing to
close.

### G-034: no AES-GCM option in v1, despite CLEANROOM 5.5 allowing one

**Question.** CLEANROOM.md 5.5 says to compare XChaCha20-Poly1305 against WebCrypto
AES-GCM "when running in a browser, chosen by benchmark, not by habit". Do we ship
both?

**Public evidence.** The paid plugin documents `algorithm: 'AES-CTR' | 'AES-CBC' |
'AES-GCM'` as a user choice (`docs-src/docs/encryption.md`, WebCrypto section). Note
that two of those three options are not AEAD at all.

**Decision.** v1 ships XChaCha20-Poly1305 only. Reasons: a 96-bit nonce needs a
persisted counter or a per-message uniqueness discipline that our multi-tab,
restore-from-backup world cannot guarantee (2.2); a second cipher doubles the envelope
test surface and creates a second way to produce unreadable data; and no benchmark
exists on either side, so "chosen by benchmark" cannot be done yet. Reopening
condition: a measured browser throughput gap that matters to a real user, plus a
nonce-uniqueness argument that survives a restore-from-backup.

**Risk if wrong.** WASM AES-GCM via AES-NI could be several times faster for large
fields in browsers that expose it. That is a performance gap, not a correctness gap.

**Would resolve it.** `docs/BENCHMARKS.md` numbers for both ciphers on our targets.

### G-035: Argon2 `p = 1` where both RFC 9106 options say `p = 4`

**Question.** RFC 9106 §4's two recommended options both use `p = 4`. We specify
`p = 1`. Justify or change it.

**Public evidence.** RFC 9106 §4 (both FIRST and SECOND RECOMMENDED), and §3.1's
constraint that `m >= 8p`. PLAN.md 3.4: threads on wasm need either
`wasm32-wasip1-threads` or `-Z build-std`, so a single thread is the default build
that the workspace gate compiles.

**Decision.** `p = 1` at `m = 64 MiB, t = 3`
(`specs/encryption.md` 2.3). `p` is a parallel-speed parameter once memory is fixed; on
a single-threaded wasm build, `p = 4` is `p = 1` with three idle lanes and identical
security. A build-dependent `p` would derive the same key differently on two targets,
so we fix one value for every target.

**Risk if wrong.** On a many-core native build we leave unlock-time speedup on the
table — a one-time ~200 ms cost, not a per-operation one.

**Would resolve it.** A decision to make `-threads` a first-class target and to raise
`p` for every target at once, which changes no stored data (the parameters live in the
internal store per generation) but must ship as a versioned parameter change.

### G-036: key-generation binding defeats ciphertext replay, but not revision forgery

**Question.** `specs/encryption.md` 2.4 puts the document's `_rev` in the AAD so a
replayed older ciphertext fails. Is that sufficient rollback protection?

**Public evidence.** The Apache-2.0 revision format
(`src/plugins/utils/utils-revision.ts`, `parseRevision`: `"<height>-<hash>"`) is
public, so an attacker with storage write access knows it. Upstream treats `_rev` as
trusted storage data.

**Decision.** Bind `_rev` in the AAD (defeats partial rollback, which is the realistic
attack: swapping one field's ciphertext for an older one) and **document the residual
gap**: an attacker who rewrites the ciphertext *and* `_rev` consistently, and who knows
the revision format, can present a consistent older `(ciphertext, revision)` pair. Also
documented: a schema migration that changes the schema hash invalidates every
ciphertext (INV-4), so a migration on an encrypted collection must re-encrypt and the
migration path must refuse otherwise.

**Risk if wrong.** The first is a documented limit of what AAD binding can do without
a tamper-evident version vector, which is a storage-layer feature, not an envelope
one. The second is a data-loss-shaped event if a migration is allowed to proceed
naively; the spec's interim position is to refuse.

**Would resolve it.** A monotonic, attacker-unrollbackable version per document at the
storage layer. That is a port-level design question, out of scope for B2.

## B2: `dxdb-sharding`

### G-037: sharding config is in the schema hash, which encryption binds into AAD

**Question.** `shards` and `mode` are declared in the schema, so they are part of
`schemaHash`. `dxdb-encryption` binds `schema_hash` into its AAD. Do the two wrappers
interact correctly, and in what order may they be stacked?

**Public evidence.** The upstream docs say changing `shards` "will lose access to your
data" (`rx-storage-sharding.md`), which is consistent with the value being part of the
schema identity. The open source computes `RxSchema.hash` from
`JSON.stringify(schema)` (`src/rx-schema.ts:113`), so any schema block affects it.

**Decision.** The wrappers compose in either order, but **changing `shards` on an
encrypted collection invalidates every ciphertext** (because the schema hash changes),
which on top of the documented data loss makes it doubly forbidden. `dxdb-sharding`
refuses at open on a config mismatch (INV-7) *before* any decryption is attempted, so
the user gets one clear error rather than a cascade of authentication failures. This
is recorded because it is the kind of ordering bug that only appears when two
clean-room wrappers meet.

**Risk if wrong.** A user stacking sharding inside encryption and changing `shards`
sees "config mismatch" instead of "authentication failed" — the better of the two.

**Would resolve it.** Nothing; the interaction is inherent to putting config in the
schema.

### G-038: the docs give no default `shards` and no default `mode`

**Question.** The page says "10 Shards is a good value to start with" and lists two
modes without saying which is the default. What does dxdb default to?

**Public evidence.** `docs-src/docs/rx-storage-sharding.md`, quoted in
`specs/sharding.md` 2.

**Decision.** **No default.** `ShardingConfig` has no `Default` impl and `mode` must be
chosen; "10" stays what the docs say it is — a starting recommendation — and does not
become a magic value we ship. A schema with no `sharding` block on a sharded wrapper is
unsharded.

**Risk if wrong.** A caller must write two fields instead of one. In exchange, no
database is silently sharded 10 ways because someone copied a snippet.

**Would resolve it.** An upstream default documented on the page. There is none.

### G-039: the shard-routing hash is ours, because nothing public specifies one

**Question.** Which hash maps a primary key to a shard? No public source says.

**Public evidence.** None. The docs describe splitting and merging but not routing.

**Decision.** FNV-1a 64-bit over the UTF-8 bytes of the primary key string, then
`mod N`, specified in `specs/sharding.md` 3.4 with a committed `(key -> shard)` vector
file so the hash cannot drift. Chosen because it is stable by construction (unlike
`DefaultHasher`, whose output is explicitly unspecified across releases), trivially
reproducible in every target including wasm, and shard assignment is not an attack
surface. Recorded as a value we chose, per CLEANROOM.md section 2's rule about values
that are not derivable from public sources.

**Risk if wrong.** Our shard layout differs from upstream's. That is already true and
unavoidable (G-002): upstream's sharded databases are not readable by us either way,
so the hash choice costs nothing and buys determinism.

**Would resolve it.** Nothing public. If a reader needs to locate a document by hand,
the vector file and the one-paragraph spec are the answer.

### G-040: `count` under sharding is a clamp, not a sum, and the docs never say so

**Question.** With `limit` in a query, is the sharded `count` the sum of per-shard
counts?

**Public evidence.** The port's own contract answers it
(`crates/dxdb-storage/src/traits.rs`, `count`): `count` must equal
`query(...).documents.len()`, including with a `limit` — which upstream states as
"`.count({limit: 3})` must return the same as `.query({limit: 3}).documents.length`".
The sharding docs say nothing about `count`.

**Decision.** Not a sum. Per-shard counts are each clamped to `limit`, and the sum is
clamped to `limit` (`specs/sharding.md` 3.3). A plain sum returns up to `N * limit` and
is silently wrong, which is the one sharding bug a user would blame on data loss.

**Risk if wrong.** None; the derivation is from the inner contract, and INV-2 asserts
`count == query length` for every query, which would catch a mistake here rather than
reporting a plausible-looking number.

**Would resolve it.** Nothing; it is determined by the open contract.

## B2: `dxdb-meta-cache`

### G-041: `localStorage` durability is not guaranteed, and the docs assume it is

**Question.** The documented design replaces the internal storage instance with a
`localStorage`-backed one. Is `localStorage` durable enough to be authoritative?

**Public evidence.** The page says the optimizer "will then automatically detect if an
RxDB internal storage instance is created, and replace that with a localstorage based
instance" (`rx-storage-localstorage-meta-optimizer.md`). The Web Storage API and
browser engine policies are public: Safari caps script-writable storage at roughly
seven days of no interaction, private modes discard on session end, users and "clear
site data" flows erase it, and quota pressure can evict it. It is also readable by any
script on the origin.

**Decision.** dxdb does **not** replace the internal instance. The internal store stays
in the real storage and `localStorage` holds a **validated snapshot** of it
(`specs/meta-cache.md` 1.4). Rationale: if `localStorage` held the only copy of the
collection registry, an eviction would leave the user's documents intact and
unopenable, and any XSS could rewrite which collections exist. A cache that can be
erased is a cache; a registry that can be erased is a data-loss bug.

**Risk if wrong.** We do not get the full saving upstream claims, because we still read
one small document from the real store at open (INV-9). We convert N reads into 1
instead of into 0. That is the price of the registry surviving a cleared cache, and it
is the right trade.

**Would resolve it.** Nothing; the durability of `localStorage` is an engine policy we
do not control.

### G-042: how to validate N cached documents without reading N documents

**Question.** A snapshot of the internal store is only useful if validating it is
cheaper than reading the store. What is the check?

**Public evidence.** None from upstream: the page describes replacing the instance, not
validating a copy, so there is no documented mechanism to borrow.

**Decision.** A **generation document** in the real internal store, bumped by every
internal-store mutation, plus an `instance_id` distinguishing a recreated database.
The snapshot is tagged with both, and the check is one read plus a synchronous
`localStorage` read (`specs/meta-cache.md` 3). The read-first/tag-early ordering rule
(3.3) makes the only possible error direction "discard a good snapshot", never "accept
a stale one". A checksum was considered and rejected: verifying it requires reading the
documents, which is the cost being removed.

**Risk if wrong.** A bug in the bump discipline leaves snapshots valid across a silent
change; INV-6's per-path test and the single-mutation-helper requirement are the
mitigation. Also: the bump hook must live inside `dxdb-core`'s internal-store writer,
which is another lane's crate — a cross-lane dependency that must be resolved before
implementation.

**Would resolve it.** A monotonic store-level sequence number exposed by the port, which
would be strictly better than a document inside the store. Revisit if `dxdb-core` grows
one.

### G-043: the generation document's id, key and placement are values we chose

**Question.** Nothing public specifies where a generation counter lives, or what it is
called. Same for the `localStorage` key.

**Public evidence.** The internal store's document shape is public
(`InternalStoreDocType { id, key, context, data }`,
`src/types/rx-database-internal-store.d.ts:6`), and the storage-token document's id and
key are public constants. The generation document is not upstream's at all.

**Decision.** We choose: id `"dxdb-meta-cache-generation"`, `localStorage` key
`"dxdb-meta-cache/v1/<database_name>"`, `format` byte, and the JSON field names in
`specs/meta-cache.md` 3.1. Recorded because CLEANROOM.md section 2 requires any value
not derivable from public sources to be chosen and documented. `instance_id` is 16
random bytes rendered as hex.

**Risk if wrong.** A future upstream mechanism with a different name cannot be
interoperated with — already impossible (G-002), and a rename is a `format` bump.

**Would resolve it.** Nothing public.

### G-044: "about 200 ms" and "more than 4 collections" are targets, not measurements

**Question.** The page quantifies the saving and the threshold at which the wrapper is
worth using. Should we repeat those figures?

**Public evidence.** `rx-storage-localstorage-meta-optimizer.md`: "this can save about
200 milliseconds on the initial page load", "recommended to use this when you create
more than 4 RxCollections". No hardware, dataset, browser, or measurement method.

**Decision.** Both are recorded as **targets** in `specs/meta-cache.md` section 6 and
may not appear as claims until `docs/BENCHMARKS.md` has our numbers. The crossover
collection count is explicitly something we measure at 1, 2, 4, 8, 12 and 20, and if
our crossover is 7 then the doc says 7 rather than 4 (G-003 applied to this crate).

**Risk if wrong.** We under-promise. Acceptable, and the alternative is repeating a
number we cannot reproduce.

**Would resolve it.** Our own bench, run per `docs/BENCHMARKS.md`.

### G-045: attachment ciphertexts are not revision-bound, because a read has no document

**Question.** `specs/encryption.md` INV-6 binds a field ciphertext to the document's
`_rev`, which defeats replaying an older value. Can attachments get the same binding?

**Public evidence.** None: the port's `get_attachment_data(document_id,
attachment_id, digest)` returns bytes and has no document, so there is no `_rev` to
bind at read time.

**Decision.** An attachment's AAD binds the document id and the attachment id, and
carries `revision: None` on both the seal and the open path. Consequence, stated
plainly: rollback protection covers encrypted document fields and **not** attachment
payloads. An attacker with storage write access can replay an older attachment
ciphertext for the same document and attachment, and it will open.

**Risk if wrong.** A replayed attachment returns stale bytes. It cannot return another
document's or another attachment's bytes, and it cannot be forged, because the AEAD tag
still covers the content.

**Would resolve it.** Carrying the document's revision into the attachment read, which
means either changing the trait signature (a port change, `dxdb-storage`) or storing
the revision alongside the attachment in `_attachments`. Both are port decisions, not
envelope decisions, so this is recorded rather than invented here.

### G-046: the KDF cost was 15x the estimate until it was measured

**Question.** `specs/encryption.md` section 3.3 argued that Argon2id at
`m = 64 MiB, t = 3, p = 1` costs "on the order of 200 ms" on a machine sustaining
~1 GB/s of per-core Argon2 throughput, and section 8 recorded that as an unmeasured
target. What is it actually?

**Public evidence.** RFC 9106 gives parameter recommendations but no throughput
figure; the 200 ms estimate was arithmetic from an assumed throughput.

**Decision.** Measured on the shared build box, three runs each: **289 ms in release**,
**2.93 s in debug**. The release figure is inside the target, so the parameters stand —
but the estimate was right by coincidence, not by method, and the assumed 1 GB/s is
really ~0.7 GB/s. More importantly, a debug build is ~10x slower, which on a slower
CPU would be several seconds and would read to a user as a hang. So the crate now
carries a test that asserts a **profile-dependent** budget (8 s debug, 500 ms release)
and prints its samples, turning "how slow is unlock" into something a gate answers.

**Risk if wrong.** The budget is calibrated to one machine. A target much slower than
this box (a low-end phone) could exceed the release budget, at which point the choice
is between lowering `t`/`m` — which weakens T1 — and accepting a multi-second unlock.
That trade belongs in a decision, not in a silent retune, so the test failing is the
intended prompt to have it.

**Would resolve it.** A measurement on the slowest target we intend to support. Until
then the numbers above are the only ones that may appear in prose.

### G-047: `dxdb-memory` is still a scaffold, so the wrapper is tested against a double

**Question.** `specs/encryption.md`'s strongest verification claim is the cross-check
against the open-source baseline (CLEANROOM.md section 6): a clean-room storage runs
the same conformance suite as `dxdb-memory`. That suite does not exist yet, and
`crates/storages/dxdb-memory` is a ten-line scaffold.

**Public evidence.** The trait contract in `crates/dxdb-storage/src/traits.rs` is
complete and merged, including the documented `count`/`query` relationship and the
change-feed fallback; the engine is not.

**Decision.** The encryption tests run against a local recording double
(`tests/fake.rs`) that implements the trait contract faithfully — real storage by
primary key, real conflict detection, real `with_deleted` filtering, and a real
`(_meta.lwt, primary)` change feed. The last one was implemented rather than inherited
from the trait's default body, because the default runs a planned range query that the
double's `query` does not honour, and a double that quietly ignores a checkpoint makes
a multi-pass consumer look broken when the double is what is wrong.

**Risk if wrong.** The double could agree with a wrappper bug. That risk is real and is
why this row exists: **the conformance cross-check is an obligation for this crate, not
a finished claim.** When `dxdb-memory` lands, `dxdb-encryption` must be re-run against
it through `make gate-storage`.

**Would resolve it.** `dxdb-memory` becoming a real storage. Tracked here so the
obligation is not quietly forgotten when the tests go green against the double.

## B3: `dxdb-worker`

### G-048: a worker is not a thread, so this crate lands on stable

**Question.** PLAN.md 5.2 says Wasm threads need `wasm32-wasip1-threads` or nightly
`-Z build-std`, because the prebuilt `wasm32-unknown-unknown` std is not compiled with
atomics. Is `dxdb-worker` therefore blocked on a threads target?

**Public evidence.** CLEANROOM.md 5.7 says "on threads, this crate is the one that needs
`atomics` + shared memory (PLAN.md 5.2), so it is also where the wasm threading decision
lands". The MDN pages for `Worker` and `SharedWorker` document separate realms; Node's
`worker_threads` documents the same; neither shares linear memory by default.

**Decision.** This crate implements **message-passing isolates** and lands on stable
`wasm32-unknown-unknown`. A browser `Worker`, a `SharedWorker` and a Node `worker_thread`
each have their own memory and communicate by message passing, so nothing in the
protocol, handshake, backpressure or supervision needs atomics. Nothing in the crate uses
`SharedArrayBuffer`, and `make wasm` gates it like any other crate.

The cost is accepted and stated: no zero-copy transfer, so a `query` returning 10 000
documents copies them, and every call pays a message hop. The benefit the documented
plugin sells is *where the work runs* — taking CPU load off the main thread — not shared
memory, so paying a copy to move encryption and index maintenance off the main thread is
the right trade. A threads design would buy the copy away at the price of nightly-only
builds and torn-read bugs.

**Risk if wrong.** A caller pushing very large result sets through the boundary sees copy
cost. That is measurable and is section 8's second target; if it turns out to dominate,
the answer is a streaming protocol for large results, not shared memory.

**Would resolve it.** Nothing; the model follows from the targets we must support on
stable.

### G-049: a write interrupted by worker death is a failure mode the storage contract does not have

**Question.** `bulk_write` is per-document atomic inside a storage (PLAN.md 4.2). Across a
worker boundary, when the worker dies mid-call, the caller cannot know which rows were
applied. What must the transport do?

**Public evidence.** None, because an in-process `bulk_write` cannot fail this way: the
call either returns or the process is gone. Upstream's worker plugin documents nothing
about it.

**Decision.** The transport **never retries a write** after a worker death, and reports a
distinct kind, `WorkerDiedDuringWrite { context }`, carrying the caller's own context
string. The correct recovery is for the layer above to re-read the affected documents and
reconcile, which is `dxdb-core`'s business. A retry would risk applying rows twice, which
with revisions surfaces as a conflict the user did not cause, or as a silent duplicate
write for a caller that generates its own revisions.

**Risk if wrong.** A caller that expected transparent retry gets an error it must handle.
That error is the information it needed: the outcome is genuinely unknown, and pretending
otherwise is how a duplicate write ships.

**Would resolve it.** Nothing; it is inherent to crossing a process boundary.

### G-050: the heartbeat and call-deadline values are guesses until a device says otherwise

**Question.** Section 3.4 detects a dead worker by exit callback plus heartbeat (5 s
interval, 15 s timeout) and a hung call by a 30 s per-call deadline. Are those right?

**Public evidence.** None. No public source documents a worker-unresponsive deadline, and
the plausible range is wide: too short trips on a slow phone under a GC pause, too long
makes a dead worker look like a slow one.

**Decision.** Those are the defaults, they are configuration, and the spec does not claim
they are correct. The three-way split they implement (dead / unresponsive / one slow call)
is the part that matters and is independent of the numbers.

**Risk if wrong.** A too-short heartbeat fails a healthy instance; a too-long one delays
recovery. Both are visible in the counters this crate exposes (`heartbeats_missed`).

**Would resolve it.** A measurement on a slow device under a forced GC pause, which needs
a real browser rather than a Rust test.

### G-051: no pre-built worker bundle ships with dxdb

**Question.** Upstream ships ready-made worker files in
`node_modules/rxdb-premium/dist/workers`, and the docs describe copying one to a
webserver. Should dxdb?

**Public evidence.** `docs-src/docs/rx-storage-worker.md`, "Pre-build workers". The
section also says any valid worker file works for both Worker and SharedWorker, because
`exposeWorkerRxStorage` detects where it runs.

**Decision.** No. dxdb takes a `workerInput` that is either a URL or a constructor, and
ships no bundle. A Rust crate cannot meaningfully ship a JavaScript bundle, and the
bundled artefact for a JS consumer belongs to the JS surface crate (`dxdb-js`), not here.
This is the same separation that keeps the clean-room crates free of platform plumbing.

**Risk if wrong.** A JS user must supply a worker file. That is one line in the surface
crate's docs and it is where it belongs.

**Would resolve it.** Nothing; the split is deliberate.

### G-052: the change-event buffer coalesces by dropping the oldest, not the newest

**Question.** If a caller's event queue fills, which events should be dropped?

**Public evidence.** The `EventBulk` contract documents that a checkpoint is carried
"so a consumer can drop a duplicate" and that a live query re-runs on a checkpoint gap,
but says nothing about which end to drop under pressure.

**Decision.** Drop the **oldest unread** bulks and set a coalesced flag. Rationale: a
consumer that sees a checkpoint gap recovers by re-running against current state, so a
dropped old event is recoverable; a dropped *new* event stalls a live query with nothing
to indicate it should re-read. This is a liveness decision, not a memory decision, and
the counters expose `events_dropped_backpressure` so it is not silent.

**Risk if wrong.** A consumer that assumed it saw every event in order gets a gap. It is
told, via the flag and the counter, which is the difference between a coalescing buffer
and a bug.

**Would resolve it.** Nothing; it follows from what a checkpoint gap already means.

### G-053: the wire protocol is ours, so an upstream worker cannot talk to a dxdb caller

**Question.** Is the message protocol compatible with upstream's worker plugin?

**Public evidence.** Upstream's worker and shared-worker pages describe the
`exposeWorkerRxStorage` / `getRxStorageWorker` pair and the `workerInput` option. No
message format is documented, and the runtime that handles it is a paid module.

**Decision.** Not compatible and cannot be made so (G-001, G-002). dxdb defines its own
request/response protocol with a versioned handshake (section 3.1-3.2). A dxdb caller
needs a dxdb worker; an upstream worker cannot serve one, and mixing them is a
`ProtocolMismatch` at the handshake rather than a mysterious hang.

**Risk if wrong.** A user with an existing upstream worker bundle must build a dxdb one.
That is inevitable given the protocol is undocumented, and the handshake makes the
failure legible.

**Would resolve it.** A published protocol specification from the vendor, obtained
properly.

### G-054: `dxdb-core` has no internal store, so the metadata cache defines the ledger

**Question.** `specs/meta-cache.md` section 3.1 assumed RxDB's internal store
(`_rxdb_internal`, one document per collection plus a storage token) would exist in the
port, so the cache could sit in front of it. Does it?

**Public evidence.** The internal store is real upstream: `INTERNAL_STORAGE_NAME =
'_rxdb_internal'` (`src/rx-storage-helper.ts:51`), one collection document per collection
carrying `name`, `schema`, `schemaHash`, `version`, `connectedStorages`, and a
storage-token document carrying `passwordHash`
(`src/rx-database-internal-store.ts:152`, `src/types/rx-database-internal-store.d.ts:6,28`).
On the dxdb side, `dxdb-core::Database::create` takes a storage and `add_collections`
builds collections directly from it; `_rxdb_internal` appears nowhere in the workspace
outside this lane's own constants.

**Decision.** `dxdb-meta-cache` declares a `MetadataLedger` trait — list the metadata
documents, replace them, read and write a generation document, mint an incarnation
identity — and proves its invariants against a faithful implementation. When `dxdb-core`
grows an internal store it implements this trait and the proof carries over unchanged.
Recorded rather than silently substituted, for the same reason as G-020: the clean-room
crate states the contract it needs instead of blocking on a crate it does not own.

**Risk if wrong.** The trait is a shape the port might not adopt verbatim, in which case an
adapter is needed. That is a page of delegation, and the alternative — waiting on another
lane — blocks B4 indefinitely on work with no publish date.

**Would resolve it.** `dxdb-core` gaining an internal store.

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
