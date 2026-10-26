# `dxdb-meta-cache`: behavioural specification

| | |
| --- | --- |
| Stage | B4 (CLEANROOM.md section 7) |
| Crate | `crates/cleanroom/dxdb-meta-cache` |
| Documented upstream module | `storage-localstorage-meta-optimizer` (CLEANROOM.md section 4 row 13) |
| Spec author read | `docs-src/docs/rx-storage-localstorage-meta-optimizer.md`; the Apache-2.0 internal-store bootstrap (`rx-database-internal-store.ts`, `rx-database.ts`), the internal-store document types, `INTERNAL_STORAGE_NAME`; the MB/MDN specifications for the Web Storage API |
| Spec author did not read | `rxdb-premium` in any form (source, bundles, `.d.ts`, source maps, installer, payloads) |
| Full provenance | [`READING-LIST.md`](./READING-LIST.md), row `dxdb-meta-cache` |
| Gap rows this spec depends on | G-001, G-002, G-003, G-006, G-041, G-042, G-043, G-044 |
| Status | spec ready for review; implementation gated on `dxdb-core` |

## 1. Observable behaviour

### 1.1 What it accelerates

> "The wrapper uses the original RxStorage for normal collection documents. But to
> optimize the initial page load time, it uses localstorage to store the plain
> key-value metadata that RxDB needs to create databases and collections."
> — `docs-src/docs/rx-storage-localstorage-meta-optimizer.md`

The metadata in question is the database's **internal store**: one collection named
`_rxdb_internal` (`src/rx-storage-helper.ts:51`), whose documents are not user data
but the bookkeeping RxDB needs before it can open anything — the storage token
(`src/rx-database-internal-store.ts:152` `ensureStorageTokenDocumentExists`), and one
document per collection carrying `name`, `schema`, `schemaHash`, `version` and
`connectedStorages` (`InternalStoreCollectionDocType`,
`src/types/rx-database-internal-store.d.ts:28`).

Observable: a database with many collections performs fewer reads of the internal
store at startup. Everything the user sees — documents, queries, replication — is
unchanged.

### 1.2 Browser only, and it degrades rather than fails

> "This plugin can only be used in browsers."

Observable: on a target without `localStorage` (native, WASI, a worker without DOM
access), the wrapper is either not constructed or is transparent. In a browser where
`localStorage` exists but throws on access — private browsing in some engines,
storage disabled by policy, or a full quota — the wrapper degrades to the unwrapped
behaviour and the database still opens. **A cache that can prevent a database from
opening is worse than no cache** (`INV-7`).

### 1.3 When it is worth using

> "Depending on your database usage and the collection amount, this can save about
> 200 milliseconds on the initial page load. It is recommended to use this when you
> create more than 4 `RxCollections`."

Observable: the saving scales with collection count, because the cost it removes is
per-collection internal-store reads. With one or two collections the wrapper is close
to neutral and may be a small loss (the generation read plus a localStorage read is
not free). The "~200 ms" and ">4 collections" figures are **targets** to measure or
drop (G-003, G-044); they were measured on someone else's hardware and dataset.

### 1.4 The critical difference from the documented upstream design

The page says the optimizer "will then automatically detect if an RxDB internal
storage instance is created, and **replace that with a localstorage based instance**".

dxdb does **not** replace the internal storage instance. It keeps the internal store
in the real storage and caches a **validated snapshot** of it in `localStorage`. This
is a deliberate, documented divergence and the single most important decision in this
spec:

- `localStorage` is **not durable**. Engines evict it: Safari's 7-day cap on
  script-writable storage, private-browsing sessions that discard on close, storage
  cleared by the user or by a "clear site data" flow, and quota eviction under
  pressure. A design whose *authoritative* copy of collection metadata lives only in
  `localStorage` loses the database's schema registry when the browser evicts it —
  and the user's documents are still there, orphaned, with no way to name or open
  them.
- It is also **same-origin and world-readable by any script on the origin**. Making it
  authoritative enlarges the blast radius of any XSS from "read the UI" to "rewrite
  the collection registry".

So: the real storage stays the source of truth, `localStorage` is a cache, and the
whole design is one question — *how does a cache of N documents get validated by
reading fewer than N?* Section 3 answers it. G-042 records the divergence.

## 2. Public API shape (from docs)

```ts
const optimizedRxStorage = getLocalstorageMetaOptimizerRxStorage({
    storage: getRxStorageIndexedDB()
});
```

Documented: one option, the inner storage; no settings object; the wrapper detects the
internal instance itself.

| Documented | dxdb | Note |
| --- | --- | --- |
| `{ storage }` | `MetaCacheStorage::new(inner, cache: Arc<dyn MetaSnapshotStore>)` | the snapshot store is injected so the crate compiles on targets without `localStorage` and so tests can drive eviction and corruption |
| "automatically detect … internal storage instance" | detection by collection name against `_rxdb_internal` | the same test `dxdb-logger` uses for its meta-instance filter; the two share the notion, not the code |
| no settings | `MetaCacheSettings` exists but all-optional: `enabled: bool` (default true) | added deliberately: a user needs a way to turn it off in the field without a rebuild |

There is no documented knob for cache lifetime or size, and dxdb does not invent one.
Eviction is the browser's business; the design assumes the cache can vanish at any
moment (`INV-7`).

## 3. The validity check, in full

This is the load-bearing section.

### 3.1 The two artefacts

**A. A generation document, in the real internal store.**

```text
collection: _rxdb_internal
id:         "dxdb-meta-cache-generation"      // dxdb-chosen id (G-043)
data: {
    generation: u64,        // monotonically increasing
    instance_id: string,    // random 16 bytes, hex; identity of this database incarnation
    format: u8,             // snapshot format this writer understands
}
```

Every mutation of the internal store — adding a collection, removing one, a schema
migration that rewrites a collection document, the storage-token document's creation
or replacement — bumps `generation` by one **in the same per-document atomic write**
that performs the mutation. The internal contract gives per-document atomicity and no
cross-document transaction (PLAN.md 4.2), so the generation bump cannot be atomic with
a *multi-document* change; section 3.4 says how that is handled.

`instance_id` exists for one failure mode, and it is the failure mode CLEANROOM.md 5.10
names: **a dropped-and-recreated database must not be confused with the old one.**
If a database is removed and created again under the same name, `generation` restarts
at 1 — and a surviving snapshot tagged `generation: 1` would validate against a new,
unrelated database. Its `instance_id` differs, so it is rejected.

**B. A snapshot, in `localStorage`.**

```text
key:   "dxdb-meta-cache/v1/<database_name>"
value: JSON {
    format: u8,              // snapshot layout version
    instance_id: string,
    generation: u64,
    documents: [ <internal-store documents, verbatim> ]
}
```

Written with a single `setItem` call, which the Web Storage specification makes atomic
per key: a reader sees either the old value or the new one, never a partial write.

### 3.2 The check

At internal-instance open:

1. Read the generation document from the **real** internal store. This is **one** read,
   and it is the entire cost of the optimisation.
2. Read the snapshot from `localStorage` (synchronous, no I/O in the storage sense).
3. Accept the snapshot only if **all** of:
   - the snapshot parses,
   - `snapshot.format == supported_format`,
   - `snapshot.instance_id == generation_doc.instance_id`,
   - `snapshot.generation == generation_doc.generation`.
4. On accept: serve the internal store's reads from `documents`, and do not read the
   real internal store's collection documents.
5. On any failure: discard the snapshot, read the internal store normally, and rewrite
   the snapshot — tagged as in 3.3.

The transformation that makes this worth doing: **N collection documents + 1 storage
token document become 1 generation document.** With 12 collections that is 13 reads
turned into 1. That is where a ~200 ms saving with >4 collections comes from, and it
is also why the wrapper is near-neutral at 1-2 collections.

### 3.3 The write ordering rule (this is where a hand-rolled version silently breaks)

**Read `generation` first, then the documents, and tag the snapshot with the
generation read first. Never read the generation after the documents and never
re-read it before tagging.**

Why, precisely: there is no cross-document transaction, so a document read is not a
consistent cut. Suppose a writer mutates the store between our generation read and our
document read. Then:

- Tagging with the **earlier** generation is **safe**: the snapshot may contain slightly
  new data under an old tag, and the next reader sees `generation_now > tag` and
  discards it. Conservative, never stale.
- Tagging with a **later** generation is **unsafe**: the snapshot may contain data from
  before the mutation while claiming to be after it, so the next reader's equality
  check passes and serves a snapshot missing a collection.

The rule exists so that the only possible error direction is "discard a good snapshot"
(cost: one slow open) and never "accept a stale one" (cost: a collection that appears
to have vanished, or a dropped one that reappears).

### 3.4 The invalidation path, enumerated

Invalidation is not a callback list; it is "every mutation bumps the generation". The
mutation paths that must bump it, each with the test that proves it:

| Mutation | Where | Test |
| --- | --- | --- |
| Storage-token document created | internal-store bootstrap | `inv_03_...` |
| Collection added | database `addCollections` | `inv_06_...` |
| Collection removed | collection `remove()` | `inv_06_...` |
| Collection document rewritten (schema migration) | migration | `inv_06_...` |
| Database `remove()` | database teardown | `inv_03_...`, `inv_04_...` |
| Recreated under the same name | bootstrap; new `instance_id` | `inv_04_...` |

A multi-document mutation (adding two collections in one call) bumps by two, or by one
and then one; either is fine, because what matters is that the value **differs from
any snapshot built before the call**. The invariant is not "bumps by exactly one", it
is "strictly increases on every mutation".

The generation document is not itself cached. It is always read fresh, which is the
one read the whole design spends.

**The failure the enumeration exists to prevent:** any mutation path that forgets to
bump leaves snapshots valid across a silent change, and then `INV-1` is violated for
that path only — a bug that shows up as "a collection I deleted came back" for some
users and not others. This spec requires the bump to live in **one** function that
every internal-store mutation calls, so there is one place to audit, mirroring the
single-filter-function discipline in `dxdb-logger`.

### 3.5 Why not checksum the snapshot against the store instead

A tempting alternative: store `SHA-256(documents)` in the generation document and
compare. It is rejected because **computing the comparison requires reading the
documents**, which is the cost being removed. A checksum you must read the data to
verify is not a cache. The generation counter is cheap because it is one small
document whose value changes without needing to read anything else.

### 3.6 Why the snapshot holds internal documents only

The snapshot contains the internal store's documents: collection names, schemas,
schema hashes, the storage token. It contains **no user documents and no encryption
key material**. This matters twice: `localStorage` is XSS-readable (1.4), and a
snapshot of user data would be a plaintext copy of the database outside the storage's
own access controls (`INV-10`).

It does contain the schema, which includes the `encrypted` field list — that is
metadata, it is already readable in the real store, and `dxdb-encryption`'s section 1.7
records that the schema is not secret.

## 4. Invariants

**INV-1 — a stale snapshot is always detected.** If the snapshot's contents differ in
any way from the internal store's current contents, the validity check rejects it. No
interleaving of any number of tabs or processes produces a served snapshot that is
missing a collection the store has, or contains one it does not.
*Enforced by* the generation counter bumped on every internal-store mutation (3.4),
tagged by the read-first rule (3.3), compared for equality at use (3.2).
*Proved by* `inv_01_stale_snapshot_is_always_detected` — an exhaustive
interleaving harness over {read gen, read docs, write snapshot, mutate, evict,
recreate} with one and two concurrent writers, asserting that any accepted snapshot is
byte-equal to a freshly-read internal store.

**INV-2 — the cache is never authoritative.** Every write goes to the real internal
store first; the snapshot is only ever written from data the real store supplied. If
the two disagree for any reason not covered above, the store wins.
*Enforced by* the write path having no branch that writes only to the cache, and by the
`documents` array being produced by a read of the store. *Proved by*
`inv_02_cache_is_never_authoritative`: with the snapshot deliberately corrupted to
claim a collection that does not exist, the open succeeds and reports the store's
collections, not the snapshot's.

**INV-3 — a dropped collection is never resurrected.** After a collection is removed,
no subsequent open serves it, whether or not the snapshot was updated, and whether or
not a snapshot from before the removal survives.
*Enforced by* the removal bumping the generation (3.4). *Proved by*
`inv_03_dropped_collection_is_never_resurrected`, which removes a collection without
rewriting the snapshot and asserts the next open does not list it. This is
CLEANROOM.md 5.10's named requirement, tested by name.

**INV-4 — a recreated database is never confused with its predecessor.** A database
removed and created again under the same name does not accept a snapshot from the old
incarnation, even when the generation numbers coincide.
*Enforced by* `instance_id` in both the generation document and the snapshot.
*Proved by* `inv_04_recreated_database_rejects_the_old_snapshot`.

**INV-5 — an accepted snapshot equals the store.** Whenever the snapshot is accepted,
its `documents` are exactly the internal store's documents: same set, same ids, same
content.
*Enforced by* INV-1 plus INV-2. *Proved by* `inv_05_accepted_snapshot_equals_the_store`,
which asserts byte equality against a fresh read on every acceptance in the
interleaving harness.

**INV-6 — invalidation is complete.** Every enumerated mutation path (3.4) changes the
generation, so no snapshot survives a mutation silently.
*Enforced by* one mutation-helper that every path calls. *Proved by*
`inv_06_every_mutation_path_invalidates`, which drives each path from the table and
asserts the generation strictly increased and the previously-valid snapshot is
rejected.

**INV-7 — the cache can never prevent a database from opening.** If `localStorage` is
absent, throws on read, throws on write, returns unparseable data, or is evicted
mid-session, the database opens from the real storage.
*Enforced by* every cache operation returning `Option`/`Result` that the open path
treats as "no cache", and by no cache error propagating out of the wrapper.
*Proved by* `inv_07_cache_failure_never_blocks_open`, driving a snapshot store that
panics-free-fails in six different ways.

**INV-8 — a format change invalidates by version, not by parse failure.** A snapshot
whose `format` is not the supported one is rejected **before** any document parsing is
attempted, so a future format cannot be half-read by an old build.
*Enforced by* checking `format` first, and by parsing documents only after acceptance.
*Proved by* `inv_08_unknown_snapshot_format_is_rejected_first`, exhaustive over every
`u8` format value.

**INV-9 — cost bound.** With a valid snapshot present, opening a database performs
**exactly one** read against the real internal store (the generation document) and no
further internal-store reads for the collection documents.
*Enforced by* the accepted path never issuing a second read. *Proved by*
`inv_09_valid_cache_costs_one_read`, counting reads through a recording storage.

**INV-10 — the snapshot carries no user data and no key material.** No snapshot field
contains a user document, an attachment, a password, a salt, or a derived key.
*Enforced by* the snapshot type being the internal-store document type, which has no
such field, rather than a generic JSON value. *Proved by*
`inv_10_snapshot_carries_no_user_data`, which writes user documents with recognisable
payloads and greps the snapshot for them.

## 5. Failure modes

| Failure | What the caller sees | What the cache does |
| --- | --- | --- |
| `localStorage` unavailable (native, worker, disabled) | database opens normally, no speedup | transparent; no error |
| `localStorage` throws on `getItem`/`setItem` (private mode, quota) | database opens normally | treats as "no snapshot"; the failed write is dropped, never retried in a loop |
| Snapshot absent | one slow open; snapshot written | rebuilds |
| Snapshot unparseable (truncated, user-edited, other app's key) | one slow open; snapshot rewritten | discards; never partially applies |
| Snapshot `format` unknown | one slow open | rejects before parsing (INV-8) |
| Generation mismatch | one slow open; snapshot rewritten | discards (INV-1) |
| `instance_id` mismatch (recreated database) | one slow open; snapshot rewritten | discards (INV-4) |
| Generation document missing (pre-cache database opened by a cache-aware build) | one slow open | creates it at 1, then snapshots; the database is otherwise untouched |
| Two tabs open concurrently, one mutates | both eventually consistent | the mutating tab bumps the generation; the other tab's next open rejects its snapshot. **The already-open tab is not invalidated live** — see the non-goal in 7 |
| Generation document corrupted (non-numeric) | one slow open, then the document is rewritten | treated as missing; a cache never gets to veto a real document permanently |
| `localStorage` cleared between sessions | one slow open | rebuilds |
| Cache key collision with another app on the same origin | none, key is namespaced `dxdb-meta-cache/v1/` + database name | a collision with another dxdb database name is impossible; collisions with other apps' keys are not our namespace |

Error kinds produced by this crate: none that reach the caller. Every cache failure is
absorbed into "no cache". A wrapper that can make a database unopenable because a
browser cleared a cache is a wrapper that will be removed in a hotfix (INV-7).

## 6. Performance targets

All **unmeasured**, and the crate may not be described as saving anything until
`docs/BENCHMARKS.md` has numbers.

| Target | Value | How measured |
| --- | --- | --- |
| Startup saving, 12 collections, IndexedDB | **unmeasured**; upstream target ~200 ms | bench `meta_cache_startup`, cold and warm, JSON result recorded |
| Startup saving, 1 collection | **unmeasured**; expected near-neutral or a small loss | same bench at 1, 2, 4, 8, 12, 20 collections, so the crossover is a measured number rather than the docs' "more than 4" |
| Reads of the internal store, warm open | exactly 1 (INV-9) | asserted by a counting test, not a bench |
| Snapshot size | **unmeasured**; must stay under 1 MiB for 20 collections or the crate must refuse to cache | bench records the JSON size per collection count |

The crossover point is the interesting number and the docs do not give it precisely.
Section 1.3 quotes ">4 collections" as a target to confirm or refute; if our measurement
says the crossover is at 7, that is what ships and the doc says 7.

## 7. Non-goals

- **Not authoritative storage.** The internal store stays in the real storage. This is
  the divergence in 1.4 and the reason the crate exists in this shape (G-042).
- **No live invalidation of an already-open tab.** The generation is checked at open.
  A second tab mutating collection metadata while a first tab is open does not
  retroactively break the first tab; the port's multi-instance handling covers
  cross-instance change propagation for user data, and extending that to collection
  metadata is a change to the port, not to this cache.
- **No cache of user documents, queries, or index strings.** Only the internal store's
  own metadata (INV-10). Caching query results is `dxdb-memory-tier`'s job.
- **No cross-origin, no cross-browser, no server-side cache.** `localStorage` is
  same-origin by construction.
- **No encryption of the snapshot.** It holds metadata that is already plaintext in
  the real store; encrypting one copy and not the other would be theatre. If the schema
  ever became secret, that is an encryption-spec change, not a cache change.
- **No `sessionStorage` / IndexedDB fallback for the cache itself.** `localStorage` is
  the only synchronous store available in a browser, and synchrony is the point: an
  async cache costs a round trip that eats the saving.
- **No Cache API / Service Worker integration.** Out of scope and unavailable in the
  targets that need this.
- **No `RxDB_UTILS_GLOBAL`/`PREMIUM_FLAG_HASH` machinery, no limits, no phone-home.**
  G-006.
- **No compatibility with upstream's meta-optimized database.** If upstream makes its
  internal store live in `localStorage`, a database created that way has no internal
  store in the real storage for us to cache; G-002's export/import path applies.

## Acceptance list

1. With a valid snapshot, opening a 12-collection database reads the real internal
   store exactly once. `[INV-9]`
2. With no snapshot, opening reads the internal store fully and writes a snapshot.
   `[INV-1]`
3. A snapshot whose `generation` is one lower than the store's is rejected, and the
   open still returns the store's collections. `[INV-1]`
4. A collection added by another writer, then an open: the new collection is present.
   `[INV-1]`
5. A collection removed without updating the snapshot, then an open: the removed
   collection is absent. `[INV-3]`
6. A database removed and recreated under the same name does not accept the old
   snapshot. `[INV-4]`
7. A snapshot edited to claim a collection that does not exist results in an open that
   reports the store's collections. `[INV-2]`
8. A snapshot with `format: 99` is rejected before any document parsing. `[INV-8]`
9. `localStorage` throwing on every operation still opens the database. `[INV-7]`
10. Garbage in the snapshot key still opens the database and rewrites the snapshot.
    `[INV-7]`
11. Every mutation path from the section 3.4 table strictly increases the generation.
    `[INV-6]`
12. The snapshot contains no user-document payload, no password, and no key material.
    `[INV-10]`
13. A pre-cache database (no generation document) opens, gains a generation document,
    and is unchanged otherwise. `[INV-1]`
14. A corrupted (non-numeric) generation document is treated as missing rather than as
    a permanent veto. `[INV-1]`
15. On any accepted snapshot, its documents equal a fresh read byte for byte. `[INV-5]`

## Test map

Tests live in `crates/cleanroom/dxdb-meta-cache/tests/`. The snapshot store is
injected, so "localStorage" in the tests is a controllable double that can be
evicted, made to throw, or corrupted.

| Acceptance | Test | File |
| --- | --- | --- |
| 1 | `acceptance_01_valid_cache_reads_the_store_once` | `tests/acceptance.rs` |
| 2 | `acceptance_02_cold_open_populates_the_snapshot` | `tests/acceptance.rs` |
| 3, 4 | `acceptance_03_04_generation_mismatch_is_rejected` | `tests/acceptance.rs` |
| 5 | `acceptance_05_removed_collection_does_not_return` | `tests/acceptance.rs` |
| 6 | `acceptance_06_recreated_database_rejects_old_snapshot` | `tests/acceptance.rs` |
| 7 | `acceptance_07_snapshot_cannot_invent_a_collection` | `tests/acceptance.rs` |
| 8 | `acceptance_08_unknown_format_rejected_before_parsing` | `tests/acceptance.rs` |
| 9, 10 | `acceptance_09_10_broken_localstorage_never_blocks_open` | `tests/acceptance.rs` |
| 11 | `acceptance_11_every_mutation_bumps_the_generation` | `tests/acceptance.rs` |
| 12 | `acceptance_12_snapshot_holds_no_user_data` | `tests/acceptance.rs` |
| 13, 14 | `acceptance_13_14_missing_or_corrupt_generation_is_tolerated` | `tests/acceptance.rs` |
| 15 | `acceptance_15_accepted_snapshot_equals_fresh_read` | `tests/acceptance.rs` |
| INV-1 | `inv_01_stale_snapshot_is_always_detected` | `tests/invariants.rs` |
| INV-2 | `inv_02_cache_is_never_authoritative` | `tests/invariants.rs` |
| INV-3 | `inv_03_dropped_collection_is_never_resurrected` | `tests/invariants.rs` |
| INV-4 | `inv_04_recreated_database_rejects_the_old_snapshot` | `tests/invariants.rs` |
| INV-5 | `inv_05_accepted_snapshot_equals_the_store` | `tests/invariants.rs` |
| INV-6 | `inv_06_every_mutation_path_invalidates` | `tests/invariants.rs` |
| INV-7 | `inv_07_cache_failure_never_blocks_open` | `tests/invariants.rs` |
| INV-8 | `inv_08_unknown_snapshot_format_is_rejected_first` | `tests/invariants.rs` |
| INV-9 | `inv_09_valid_cache_costs_one_read` | `tests/invariants.rs` |
| INV-10 | `inv_10_snapshot_carries_no_user_data` | `tests/invariants.rs` |

Search space note: `INV-1` is the one invariant here whose state space is genuinely
large (concurrent interleavings), so it is the one place a generated corpus is
justified. It uses a deterministic interleaving enumerator over a six-operation
alphabet restricted to the orderings that matter (mutate before/after each cache
step), which is a few hundred cases, not a random walk — reproducible failures beat
sample size for a cache-coherence protocol. `INV-8` is exhaustive over all 256 format
bytes.

## Verification record

Implemented as `crates/cleanroom/dxdb-meta-cache`. All four workspace gates green
(`check`, `clippy`, `test`, `wasm` all exit 0). Every invariant and acceptance number
above is implemented by a passing test with that number in its name.

### The one thing this spec got wrong, and how it was resolved

Section 3.1 assumed the port would have RxDB's internal store (`_rxdb_internal`) with its
collection documents, so the cache could sit in front of it. **It does not.**
`dxdb-core::Database::create` takes a storage and `add_collections` builds collections
directly from it; `_rxdb_internal` appears nowhere in the workspace outside this lane's own
constants (`docs/CLEANROOM-GAPS.md` G-054).

The resolution is a `MetadataLedger` trait: the contract the cache actually needs is "list
the metadata documents" / "replace them" / "read and write a generation document", and that
is what the trait declares. The invariants are therefore proven against a faithful
implementation rather than deferred until `dxdb-core` grows an internal store, and when it
does, it implements this trait and the proof carries over unchanged. That is the same
reasoning as `dxdb-logger`'s specification envelope (G-020), and it is recorded rather than
silently substituted.

### What the tests found

Three genuine implementation bugs, all caught by the invariants rather than by review:

- **INV-7 failed first, and it was the important one.** A snapshot-store write failure
  propagated out of `open`, so a browser at quota would have refused to open a database
  whose documents were perfectly readable. That is the exact failure the invariant exists
  to forbid. Fixed by making generation-document creation best-effort: the cache's job is
  to serve documents, and caching them is an optimisation.
- **INV-3 failed**, and the diagnosis mattered: it was a *test* bug, not a code bug. The
  test reopened twice and asserted both opens were misses; the second legitimately hits,
  because the first open rebuilds the snapshot from the post-mutation store. Asserting the
  second miss would have been asserting that the cache never works. The test now asserts a
  miss on the **first** open after a mutation — which is the assertion that actually
  catches a missing bump — and that the collection stays gone on later opens.
- **INV-1 failed** on a test fake whose `instance_id` was derived from the store's key
  count, so an ordinary open could hand out a new identity. That in turn broke INV-3's
  premise. Fixed by keying the identity on a recreation counter.

This is worth recording because two of the three were test bugs. An invariant that fails
is evidence of *a* disagreement, and the spec or the test is as likely to be wrong as the
code.

### INV-9, the cost bound, is measured not asserted

A hit performs **zero** metadata-document reads: the test seeds 12 collections, warms the
snapshot, and asserts `document_reads == 0` on the next open. The one read the design
spends is the generation document, which the ledger counts separately.

### Remaining unproven, stated rather than claimed

- **No performance number.** Section 6's targets are unmeasured; no bench exists. The
  crate may not be described as saving any milliseconds until `docs/BENCHMARKS.md` has one.
- **The browser path is untested here.** Every test uses `dxdb-localstorage`'s `MemoryKv`
  through the `Kv` trait, which is the same interface `WebStorage` implements, but the real
  `localStorage` — including its quota and eviction behaviour — has not been exercised. The
  `Kv` seam means it can be, at the JS boundary.

## Open questions this spec does not settle

1. **Where the bump hook attaches — RESOLVED.** Section 3.4 required one mutation helper
   inside the port's internal-store writer. Since `dxdb-core` has no internal store, the
   bump lives in [`MetaCache::write`], which is the *only* way to mutate metadata through
   this crate and therefore the only place the generation is touched. `INV-6` is thus a
   property of the API surface rather than of a callback registry — a stronger arrangement
   than the one the spec originally described, because there is no way to mutate metadata
   and forget to bump.
2. **Whether the generation document belongs in the internal store or in the cache-key
   namespace.** Putting it in the internal store costs one read (the design's whole
   cost) and makes it transactional with the data it describes; putting it outside
   would cost nothing but could diverge from the store. The spec chooses the internal
   store for the second reason and accepts the read.
