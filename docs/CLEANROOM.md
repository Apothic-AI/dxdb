# Clean-room plan: RxDB's paid and closed-source features

Companion to [PLAN.md](./PLAN.md). Read section 1 and 3 of that document first.

Status: draft for review. No clean-room code exists yet.
Date: 2026-10-25

## 1. The problem

RxDB's core is Apache-2.0 and lives in the public repo we are porting (`~/Code/rxdb`, 247
TypeScript files). RxDB's paid features do not. They ship as `rxdb-premium` on npm. Three facts,
from public sources only:

- The npm README states the package "downloads+decrypts the actual code in the `postinstall`
  hook", and the published package metadata is consistent with that: `postinstall: node
  ./scripts/postinstall.js && node ./scripts/installer.js`. We take the README at its word and
  do not inspect the installer to find out what it fetches.
- The package metadata declares **no `license` field** (checked against the npm registry for
  version 17.6.0). There is no published license we could comply with even if we wanted to copy.
- The paid tier also holds back behaviour in the open code by checking a flag:
  `src/plugins/utils/utils-premium.ts` compares `sha256(RXDB_UTILS_GLOBAL.premium)` against a
  hardcoded hash, and `NON_PREMIUM_COLLECTION_LIMIT = 13` closes collections once 13 are open.

Together those three make the clean-room route the only defensible one, not merely the cautious
one.

So there are two distinct jobs, and they need different methods:

1. **Port the Apache-2.0 code.** Read it, translate it. Legal, intended, and the source of truth
   for behaviour. This is PLAN.md.
2. **Reimplement the paid features.** We cannot read that code, and we are not going to obtain
   it improperly. So it must be built from a specification written by someone who has not read
   it, from public documentation and observable behaviour.

That second job is what this document governs.

## 2. What "clean room" means here, precisely

Clean room is a process claim, not a marketing word. The claim only holds if it is auditable, so:

**Two roles, separated by knowledge, not just by intention.**

- **Specification author** reads only: public documentation, the Apache-2.0 source, published
  benchmarks, the public issue tracker, blog posts, and public API surfaces. Produces a written
  behavioural specification in `specs/`. Writes no implementation code.
- **Implementer** reads only: the specification, dxdb's own code, and generic prior art
  (crate documentation, RFCs, papers, standards). Never reads `rxdb-premium`, its minified
  bundles, its TypeScript declarations, or its source maps.

The separation is only real if the artifacts carry it. Therefore:

- Clean-room work lives in its own crate tree (`crates/cleanroom/*`) with its own
  `CODEOWNERS`, in a separate git history if reviewers prefer, so a diff cannot accidentally
  straddle the boundary.
- Every clean-room crate carries `specs/<feature>.md` as a header comment, and the PR template
  asks the implementer to confirm they only read that spec.
- If an implementer has already seen the closed source (for any reason, including having worked
  with RxDB Premium), they do not work on the matching crate. They can work on the port.

**What we will not do**, stated so nobody drifts into it:
- No installing `rxdb-premium` to inspect it, no decompiling its bundles, no reading its `.d.ts`,
  no fetching its source maps, no scraping its encrypted payloads.
- No copying of constants, magic numbers, index layouts, key derivation parameters, or string
  formats out of the paid code. If a value is not derivable from public docs or from the
  Apache-2.0 source, we choose our own value and document that we did.
- No using the `PREMIUM_FLAG_HASH` or `RXDB_UTILS_GLOBAL.premium` mechanism. We are not
  circumventing a license check to unlock someone else's plugin; we are writing a different
  plugin. Different thing, and the distinction has to stay visible in the code.

**What we will do:** if a license is purchased later, black-box observation becomes available
(run it, feed it input, record output) under a documented protocol, without reading source. That
is still clean room in the ordinary sense: specification from behaviour. Until then, the specs
are thinner and the risk of behavioural mismatch is higher. Section 6 says how we handle that.

## 3. Honest framing of the license boundary

Two facts that shape the whole plan, stated plainly:

**Fact one: the gates are Apache-2.0.** `utils-premium.ts`, the 13-collection limit, the
`hasPremiumFlag` check, the SQLite "trial" cap (no indexes, no attachments, ~500 documents, per
the warning text in `src/plugins/storage-sqlite/sqlite-storage-instance.ts:138`), and the
dev-mode tracking iframe are all in the public Apache-2.0 tree. Legally we could port them
verbatim. We will not. A port that reproduces another vendor's paywall is a bad look and a
support burden, and we are not the licensor of these features. Decision: **dxdb ships no
artificial limits and no phone-home.** `docs/` records this as an intentional divergence from
upstream with its rationale, and the parity matrix marks those specific behaviours as
"intentionally not ported".

**Fact two: premium features are not wire-compatible, and cannot be made so.** We cannot know the
on-disk format of premium OPFS or premium SQLite or WebCrypto-encrypted documents without
reading the closed code. So dxdb's clean-room equivalents will read and write their own formats.
Consequences to design around, not to hide:

- A database created by upstream premium SQLite cannot be opened by `dxdb-sqlite` directly. It
  can be migrated through the documented, portable path (export documents, import documents), and
  that path is tested.
- Premium-encrypted documents cannot be decrypted by dxdb and vice versa. dxdb defines its own
  documented envelope. If interoperability ever becomes a requirement, the only honest route is
  a specification from the vendor, which we would then have to obtain properly.

Both facts go in the README. Users forgive a documented difference; they do not forgive a silent
one.

## 4. The target list

Enumerated from the public installation docs and the premium page, which name 17 premium module
paths. `shared` is the flag setter, not a feature, so 16 real features.

| # | Upstream module | Documented behaviour | Clean-room crate | Tier shown |
| --- | --- | --- | --- | --- |
| 1 | `storage-indexeddb` | plain IndexedDB, custom index strings, batched cursors, write-ahead log | `dxdb-indexeddb` | Pro |
| 2 | `storage-opfs` | OPFS, sync access handles in a worker | `dxdb-opfs` | Pro |
| 3 | `storage-sqlite` | SQLite over a `sqliteBasics` adapter | `dxdb-sqlite` | Pro |
| 4 | `storage-filesystem-node` | one JSON file per collection/segment, multi-process safe | `dxdb-filesystem` | Pro |
| 5 | `storage-filesystem-expo` | same, over expo-file-system | `dxdb-filesystem` (RN adapter) | Pro |
| 6 | `encryption-web-crypto` | per-field encryption with WebCrypto | `dxdb-encryption` | Pro |
| 7 | `flexsearch` | fulltext index over writes, via RxPipeline | `dxdb-fulltext` | Pro |
| 8 | `storage-worker` | storage inside a WebWorker / worker thread | `dxdb-worker` | Pro Plus |
| 9 | `storage-worker` (shared) | SharedWorker, one instance across tabs | `dxdb-worker` | Pro Plus |
| 10 | `storage-sharding` | split one collection across N shards, merge on read | `dxdb-sharding` | Pro Plus |
| 11 | `storage-memory-mapped` | in-memory hot copy, persisted underneath | `dxdb-memory-tier` | Pro Plus |
| 12 | `storage-memory-synced` | in-memory copy replicated to the underlying storage | `dxdb-memory-tier` | not on the pricing page |
| 13 | `storage-localstorage-meta-optimizer` | keep collection metadata in localStorage to cut startup | `dxdb-meta-cache` | Pro Plus |
| 14 | `query-optimizer` | build-time `findBestIndex` over a real dataset | `dxdb-query-optimizer` | Pro Plus |
| 15 | `server-adapter-fastify` | CRUD + replication endpoints on Fastify | `dxdb-server` | Pro Plus |
| 16 | `server-adapter-koa` | same on Koa | `dxdb-server` | Pro Plus |
| 17 | `logger` | log every storage operation, Sentry compatible | `dxdb-logger` | Pro Plus |

The tier column is what the pricing page shows. Three notes on the table:

- 8 and 9 are one upstream module, and 11 and 12 are two different wrappers over the same idea.
  Grouping them into `dxdb-worker` and `dxdb-memory-tier` is our choice, based on shared
  machinery. It is a design decision the specification author makes, not something inherited.
- `storage-memory-synced` is documented as a premium plugin (`<PremiumBlock />` on its page, and
  it is imported from `rxdb-premium/plugins/storage-memory-synced`), but it does not appear in
  any tier's bullet list while `storage-memory-mapped` does. That is a documentation
  inconsistency upstream, not ours to resolve. The spec author notes it and moves on.
- `storage-localstorage-meta-optimizer` is on the Pro Plus list but has no dedicated tier
  wording beyond "Localstorage Meta Optimizer", so its spec leans on the plugin page rather than
  the pricing page.

Two features are documented as part of the paid tier that deserve a note:

- **Memory-synced vs memory-mapped** are documented as separate products with different
  trade-offs (replicated vs hot-copy-plus-persist). If a reader cannot tell them apart from the
  public docs, that is a gap in the public docs, and the spec must say which behaviour we chose
  and why. Do not invent a distinction we cannot source.
- **The logger** is documented as "compatible with Sentry". Compatibility here means the shape of
  the events it can emit, which is a public Sentry API, not RxDB-specific knowledge.

## 5. Per-feature specification briefs

The specification author expands each of these into `specs/<feature>.md` with: observable
behaviour, public API shape (from docs), invariants, failure modes, performance targets,
non-goals, and the tests that prove it. The notes below are the starting brief, not the spec.

### 5.1 `dxdb-opfs` (from `storage-opfs`)

Publicly known: OPFS gives byte-level file access; the synchronous `read`/`write` on
`FileSystemSyncAccessHandle` exist only inside a dedicated worker; the plugin claims 3-4x
IndexedDB. Independent design questions the spec must answer: how documents are laid out across
files (one file per collection as an append log? a segmented log? an index plus segments?), how
compaction works, how a checkpoint is made durable on a filesystem with no fsync-primitive
exposed, and what the failure story is when a write is interrupted. These are ordinary storage
engineering decisions and we make them ourselves, on evidence from our own benchmarks.

### 5.2 `dxdb-indexeddb` (from `storage-indexeddb`)

Publicly known: plain IndexedDB, custom index strings instead of native indexes, batched cursors,
write-ahead log for write latency, smallest bundle and fastest startup in their comparison.
Independent design: one object store per collection with a primary-key compound, an index-string
column for range scans, and a WAL store flushed on a schedule; batching cursor reads with
`getAll` over explicit key ranges. Everything here follows from the IndexedDB API plus the port's
own index-string format.

### 5.3 `dxdb-sqlite` (from `storage-sqlite`)

Publicly known: works across Node, Electron, React Native, Capacitor, and browser-via-wasm,
through a pluggable `sqliteBasics` adapter. Independent design: our own schema DDL, our own
prepared statements, our own adapter trait (`SqliteDriver`) with implementations for `rusqlite`
and `sqlite-wasm-rs`. Strictly better position than upstream: we own both sides, so no adapter
soup.

### 5.4 `dxdb-filesystem` (from `storage-filesystem-node`, `filesystem-expo`)

Publicly known: plain files, faster than SQLite in their measurement, multi-process safe when
`multiInstance: true`. Independent design: this is where `redb` earns its place on native
(pure Rust, MVCC, crash safe), with a documented JSON-file export path for portability and for
the Expo target where a real kv engine is unavailable. Multi-process safety comes from the engine,
not from hand-rolled locking.

### 5.5 `dxdb-encryption` (from `encryption-web-crypto`)

Publicly known: password-based, per-field, `encrypted: string[]` in the schema, works across
browser/RN/Node. Independent design: we are not bound to WebCrypto. Use RustCrypto
XChaCha20-Poly1305 with an Argon2id KDF (or `argon2` crate defaults), a documented versioned
envelope, a per-database salt, an explicit key-rotation path, and AAD binding the document id plus
schema hash so ciphertext cannot be moved between documents. Compare against WebCrypto AES-GCM as
an option when running in a browser, chosen by benchmark, not by habit.

### 5.6 `dxdb-fulltext` (from `flexsearch`)

Publicly known: an indexer runs on each write via RxPipeline, and queries support multi-field,
partial matching and "complex" operations. Independent design: do not port FlexSearch. Build an
inverted index in Rust with the tokenizer of our choice (documented, with Unicode segmentation),
and pick between a segmented immutable index with a small in-memory delta (good wasm size) or a
`tantivy` backend for native users who want ranking. Ranking model, tokenization, and update
strategy go in the spec with measurements.

### 5.7 `dxdb-worker` (from `storage-worker`, `storage-shared-worker`)

Publicly known: storage runs off the main thread; SharedWorker is created once across tabs, which
removes multi-connection handling. Independent design: the port already has `dxdb-remote`
(the open-source message-channel transport) to build on. The clean-room part is the transport
and lifecycle: a typed message protocol, handshake with version negotiation, backpressure, and a
supervision story for a worker that dies mid-write. On threads, this crate is the one that needs
`atomics` + shared memory (PLAN.md 5.2), so it is also where the wasm threading decision lands.

### 5.8 `dxdb-sharding` (from `storage-sharding`)

Publicly known: a wrapper over any storage, creates N shards, splits writes, merges reads,
claims 30-50% read and 25% write gains on slow IndexedDB. The interesting problem is not splitting,
it is **queries that span shards**: sort order across shards, `limit` with `skip`, and `count`
must all be exact, not approximate. The spec states the invariant: sharded results are
indistinguishable from unsharded results for every query the unsharded storage supports. That
invariant is testable and is the acceptance criterion.

### 5.9 `dxdb-memory-tier` (from `storage-memory-mapped`, `storage-memory-synced`)

Publicly known: a hot in-memory instance in front of a persistent one. Independent design: this is
a cache-coherence problem, so it gets a real coherence protocol (change-stream driven, with
checkpoint reconciliation on startup) rather than best-effort replication. The hard cases are
documented in the spec: a second process writing to the same storage, a memory copy that fell
behind a checkpoint, and a crash between the memory write and the persisted write.

### 5.10 `dxdb-meta-cache` (from `storage-localstorage-meta-optimizer`)

Publicly known: keeps the key-value metadata RxDB needs at startup in localStorage, saving about
200ms with more than 4 collections, browser only. Independent design: an explicit, versioned
metadata snapshot with a validity check against the underlying storage (so a stale snapshot
cannot resurrect a dropped collection), and a documented invalidation path.

### 5.11 `dxdb-query-optimizer` (from `query-optimizer`)

Publicly known: `findBestIndex({schema, storage, queries})` finds the best index for a query at
build time, because a client-side app cannot afford background measurement. Independent design:
run candidate indexes against real data in the test/build environment, measure scan lengths, and
emit a recommendation that can be pasted into the schema's `indexes`. Our version can go further
because it is native: a small offline harness that replays a query corpus and reports the actual
index strings visited. Output format is ours.

### 5.12 `dxdb-server` (from `server-adapter-fastify`, `server-adapter-koa`)

Publicly known: a server on top of an RxDB database exposing CRUD REST and realtime replication
endpoints, usable standalone or mounted on an existing HTTP server. Independent design:
`axum`/`hyper` on native, and a WASI `wasi:http` build for the component target. The wire
behaviour that matters is the **open-source replication protocol** (already ported in
`dxdb-replication`), not the adapter API. Adapters for Fastify and Koa are JS-side conveniences;
we provide a Rust-native server plus a thin JS shim if demand exists. Do not promise a
drop-in Fastify plugin: it would require reimplementing a third-party framework's plugin contract,
which is unrelated work.

### 5.13 `dxdb-logger` (from `logger`)

Publicly known: logs every storage-layer operation, aimed at APMs including Sentry. Independent
design: tap the `dxdb-storage` trait wrappers (exactly like `dxdb-encryption` does) and emit
structured events through `tracing`. Sentry compatibility means a documented field set that maps
onto Sentry's span/transaction API. This is the cheapest clean-room item and a good first one to
prove the process.

## 6. Verification without access to the thing being replaced

This is the weakest link in any clean room, so it gets a concrete answer rather than a shrug.

- **Behavioural acceptance is written from the public docs**, not from the closed implementation.
  Each spec ends with a numbered list of observable statements ("with 4 collections open, a cold
  start does not read the collection metadata from the primary storage more than once"). Tests
  reference those numbers.
- **Invariants we can prove without a reference implementation.** Sharding: results equal
  unsharded. Memory tier: after reconciliation, memory equals storage. Encryption: ciphertext
  never appears in plaintext after decrypt, AAD substitution is rejected, rotation preserves
  plaintext. Meta cache: a stale snapshot is always detected. Those are property tests against
  our own contract, and they catch the failure modes users care about.
- **Cross-check against the open-source baseline.** For every clean-room storage, the same
  conformance suite that `dxdb-memory` and `dxdb-filesystem` run must pass. The storage contract
  is defined by the Apache-2.0 side, so this is legitimate and strong.
- **Performance claims get measured, or they get dropped.** Their published numbers (3-4x, 30-50%
  reads, 25% writes, 200ms startup) are targets to benchmark against on our own hardware, not
  facts to repeat. Any doc sentence of the form "faster than X" needs a number from
  `docs/BENCHMARKS.md`.
- **Gap register.** When the public docs are insufficient, the spec records the gap and the
  decision we made. `docs/CLEANROOM-GAPS.md` is the running list. Publishing it is a feature:
  it tells users exactly where behaviour may differ.
- **If a license is later purchased**, black-box observation is added under a written protocol
  (inputs, recorded outputs, no source access) and the affected specs get revised. That is an
  upgrade to verification, not a change of method.

## 7. Sequencing

Clean room is track B, starting once the port passes M1 (schema + query), because every clean-room
storage needs a working core to test against.

| Stage | Work | Exit gate |
| --- | --- | --- |
| B0 (week 6) | process: `specs/` layout, `CODEOWNERS`, PR template, gap register, reading list | a reviewer can tell, from artifacts alone, who read what |
| B1 (week 8) | `dxdb-logger` spec + implementation | proves the pipeline end to end; smallest feature, real value |
| B2 (week 12) | `dxdb-encryption`, `dxdb-filesystem` | envelope and storage conformance; the two features with the clearest public contracts |
| B3 (week 16) | `dxdb-indexeddb`, `dxdb-opfs`, `dxdb-worker` | browser parity with upstream's premium story, which is the commercial core |
| B4 (week 20) | `dxdb-sqlite`, `dxdb-memory-tier`, `dxdb-sharding`, `dxdb-meta-cache` | full storage matrix |
| B5 (week 24) | `dxdb-fulltext`, `dxdb-query-optimizer`, `dxdb-server` | the extras, once the storage story stands |

Rationale for the order: value per unit of risk. Logger and encryption have sharp, verifiable
contracts. The browser storages are the commercial heart and the hardest to get right, so they
get a milestone of their own.

## 8. Nomenclature and IP hygiene

- **Do not use RxDB's premium names as ours.** `dxdb-opfs`, not "OPFS RxStorage". Descriptive
  names ("OPFS-backed storage") are fine; product names are not. `dxdb` is our name; "RxDB" appears
  only in factual statements ("a Rust port of RxDB's open-source core").
- **No 👑, no "Premium", no tier language in dxdb.** Everything we ship is one license, stated
  once.
- **LICENSE and NOTICE.** Apache-2.0 for the ported code, with the upstream NOTICE text preserved
  (`Copyright 2015-2025 Netflix, Inc., Microsoft Corp. and contributors`). Clean-room crates are
  authored by us and carry the same license with a provenance note: "independently specified from
  public documentation; no upstream premium source was consulted."
- **Commit provenance.** Clean-room commits say `[cleanroom]` in the subject and name the spec
  file. A reviewer can audit the boundary with `git log --grep`.
- **No vendor-lock language, no comparative marketing** until the benchmarks exist to back it.

## 9. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Unknowable behaviour (closed source) | feature feels different from the paid original | gap register, invariants-first testing, publish the differences |
| Contamination (someone reads the premium bundle) | the clean-room claim is void | role separation, `CODEOWNERS`, provenance trailers, honest disclosure if it happens |
| Trademark friction | rename late, expensive | neutral naming from day one; no premium terminology |
| Clean room drifts into "just port it" | legal exposure | PR template question, spec-first rule, review gate |
| Effort is larger than it looks (16 features) | stalls | B1 first (logger) to prove the pipeline, then value-ordered stages |
| Wire incompatibility disappoints users | churn, bad reviews | state it in the README and in each storage's docs, provide a tested migration path |

## 10. Immediate next steps for track B

1. Create `specs/` with a template that forces the six sections (behaviour, invariants, API,
   failures, performance targets, non-goals) and a numbered acceptance list.
2. Write `specs/logger.md` and `specs/encryption.md`. Both are writable today from public docs.
3. Add `CODEOWNERS`, the PR template question, and `docs/CLEANROOM-GAPS.md`.
4. Add the provenance note to the workspace README so the boundary is visible to anyone who
   clones the repo, not just to the people who read this file.
