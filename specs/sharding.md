# `dxdb-sharding`: behavioural specification

| | |
| --- | --- |
| Stage | B4 (CLEANROOM.md section 7) |
| Crate | `crates/cleanroom/dxdb-sharding` |
| Documented upstream module | `storage-sharding` (CLEANROOM.md section 4 row 10) |
| Spec author read | `docs-src/docs/rx-storage-sharding.md`; the Apache-2.0 storage interface and the query-planner/prepared-query contracts; `docs/GATES.md`; the landed `dxdb-storage` traits |
| Spec author did not read | `rxdb-premium` in any form (source, bundles, `.d.ts`, source maps, installer, payloads) |
| Full provenance | [`READING-LIST.md`](./READING-LIST.md), row `dxdb-sharding` |
| Gap rows this spec depends on | G-001, G-002, G-003, G-006, G-037, G-038, G-039, G-040 |
| Status | spec ready for review; implementation gated on `dxdb-core` and the port's query planner |

## 1. Observable behaviour

### 1.1 It is a transparent wrapper that splits one collection across N shards

> "The sharding plugin works as a wrapper around any other `RxStorage`. The sharding
> plugin will automatically create multiple shards per storage instance and it will
> merge and split read and write calls to it."
> — `docs-src/docs/rx-storage-sharding.md`

Observable: wrapping a storage with `shards: N` and creating a collection produces
`N` underlying storage instances for that one collection. Documents are spread
across them by primary key. The caller above the wrapper still sees exactly one
storage instance, with the same name, schema and collection identity.

The wrapper does not change the document format, the revision format, the index
strings, or the change-event shape. It redistributes whole documents and merges
whole documents.

### 1.2 The configuration is part of the schema, and changing it loses data

> "**IMPORTANT**: Changing the value of shards is not possible on an already
> existing database state, you will lose access to your data."

Observable: `shards` and `mode` are declared in the schema, so the shard count is
persisted in the schema hash and in the internal store. Opening an existing
database with a different `shards` value is a **hard error at open**
(`ShardingConfigMismatch`), not a silent reinterpretation.

dxdb states the mechanism instead of relying on "you will lose access to your
data": the shard a document lives in is a pure function of its primary key and `N`,
and `N` is in the schema hash (which `dxdb-encryption` also binds its AAD to — the
two interact, see G-037). Changing `N` means every document's shard address changes
at once, so the storage would appear empty rather than corrupted. Refusing at open
turns "you will lose access to your data" into "you cannot do this by accident".

### 1.3 Two modes

> "Sharding mode, you can either shard by collection or by database. For most cases
> you should use 'collection' which will shard on the collection level. For example
> with the IndexedDB RxStorage, it will then create multiple stores per IndexedDB
> database and not multiple IndexedDB databases, which would be slower."

Observable:

- `mode: "collection"` — the `N` shards are `N` storage instances *within* the same
  underlying database. On IndexedDB that is `N` object stores in one database.
- `mode: "database"` — the `N` shards are `N` separate underlying databases.

The mode changes only how shards are *named* when instances are created; it does not
change routing, merging, or any result. That is deliberate: it means the mode is not
part of the correctness surface, so `INV-1` is proven once, not twice.

### 1.4 What it claims to be for, and what that claim is worth

> "For example on slow IndexedDB, a performance gain of **30-50% on reads**, and
> **25% on writes** can be achieved by using multiple IndexedDB Stores instead of
> putting all documents into the same store."

Those numbers are **targets, not facts** (G-003). They were measured on someone
else's hardware, with an unknown dataset and an unknown query mix. Section 6 requires
our own measurement before any comparative sentence ships. Note also the qualifier
the page itself puts on it: "**some** `RxStorage` implementations" — a storage whose
bottleneck is not per-store (a single-file engine, a network transport) may see no
gain or a small loss from the merge cost.

## 2. Public API shape (from docs)

The documented JavaScript shape:

```ts
const shardedRxStorage = getRxStorageSharding({ storage: someStorage });

const schema = {
    /* ... */
    sharding: {
        shards: 10,
        mode: 'collection'   // or 'database'
    }
};
```

Documented defaults and constraints, and what dxdb does with each:

| Documented | dxdb | Note |
| --- | --- | --- |
| `sharding.shards` | `ShardingConfig::shards: NonZeroU32` | `NonZeroU32` makes `0` unrepresentable, so the division in routing cannot panic |
| `sharding.mode: 'collection' \| 'database'` | `ShardMode::{Collection, Database}` | no default is documented; there is no `Default` impl, so the caller must choose (G-038) |
| "10 Shards is a good value to start with" | guidance, not a default | we do not ship a magic default; a schema without a `sharding` block is not sharded |
| absent `sharding` block on a sharded wrapper | unsharded, single instance | the wrapper passes through; documenting this because "wrapped but not configured" is a real configuration |

`ShardingConfig` names no shard count that dxdb invented, and `mode` has no default
we invented. Both are recorded in G-038 because "the docs give a starting value and
no default" is a genuine gap that a reader could resolve in two different ways.

## 3. The invariant, stated precisely

Everything else in this spec exists to support one testable statement.

> **INV-1 — result equivalence.** Let `S` be any storage implementing the port's
> `RxStorageInstance` contract, and `S_N` the same storage wrapped with
> `shards = N` for any `N >= 1`. Let `C` be a collection whose documents are written
> through `S_N`. Then for every write sequence and every prepared query `q` that
> `S` supports, running `q` against `S_N` returns exactly what running `q` against an
> unsharded `S` holding the same documents returns:
>
> 1. `query(q).documents` — the same documents, **in the same order**;
> 2. `count(q).count` — the same number;
> 3. `find_documents_by_id(ids, with_deleted)` — the same documents, in the same
>    order as the unsharded storage returns them for the same `ids`;
> 4. `bulk_write(rows, ctx)` — the same set of per-row errors, and the same final
>    stored state;
> 5. `get_changed_documents_since(limit, cp)` — the same documents and a checkpoint
>    that continues correctly;
> 6. `change_stream()` — every change event still reaches every subscriber, once.

"Indistinguishable" here means **observationally equivalent through the storage
trait**. It does not mean identical internal layout, identical number of underlying
calls, or identical performance — the last of which is the entire point of sharding.

Note that `N = 1` is in scope and is not a degenerate case to skip: it is the
cheapest differential test, and it is where an off-by-one in routing shows up first.

### 3.1 Why sort order across shards is exact, not approximate

A shard is chosen by primary key, and the query's sort is not the primary key in
general. So `query(q)` on `S_N` is a **k-way merge** of `N` already-sorted streams.

The exactness argument has three parts, and all three are needed:

1. **Each shard returns its own results in `q`'s order.** That is the inner storage's
   contract: `query` returns documents in the query's sort order. The wrapper does
   not weaken it, because it passes `q` through unchanged.
2. **The merge is a comparison on the same total order the inner storage sorts by.**
   This is the part that would be approximate if we were careless. PLAN.md 3.2 fixes
   the order as **UTF-16 code-unit order** on index strings, not UTF-8 byte order,
   and it is not the same order. A merge that compared Rust `String`s would
   interleave results differently from an unsharded query for any document set with
   astral-plane characters in a sorted field — silently, and only for those users.
   The merge therefore compares with `dxdb-jsstr`'s code-unit comparator, and
   `INV-1`'s test corpus must include surrogate-pair keys for exactly this reason.
3. **The merge is over the full `q` after per-shard application of `q`.** A document
   that any shard would return is returned, and no document is returned twice
   because each document lives in exactly one shard (`INV-3`).

Given those, the k-way merge of sorted sequences under a shared total order is
sorted, and contains exactly the union. That is a proof, not an approximation, and
it is why the test can assert order equality rather than order-agnostic set equality.
The corollary matters: **if the port ever makes a sort order that is not a total
order** (a comparator that says two distinct documents are equal), the merge becomes
unstable and `INV-1`'s order clause needs revisiting. The spec's position is that the
storage contract's order is total, so the merge is total.

### 3.2 Why `limit` with `skip` is exact, not approximate

The naive sharded implementation is the wrong one: ask each shard for
`skip + limit` and then skip `skip` in the merged result. That is *correct* but
profligate; asking each shard for `limit` and merging is *wrong* (the first `limit`
documents overall may all come from one shard).

The rule, stated so it can be implemented and tested:

- Each shard is asked for `skip + limit` documents **when `limit` is `Some`**, because
  in the worst case every one of the global `skip` documents lives in that shard, so
  a shard must be prepared to supply that many before its contribution to the global
  window can be known.
- The merged stream is then advanced by exactly `skip` and truncated to exactly
  `limit`.
- **`skip` and `limit` are applied after the merge, never before**, except in the
  per-shard over-fetch above, which is an optimisation that cannot change the result
  because it only ever fetches *more* than the shard could contribute.
- When `limit` is `None`, each shard is asked without a limit and the merge is
  consumed in full.

Exactness argument: let `W` be the global window `[skip, skip + limit)` of the merged
sorted sequence. Any document in `W` has global rank `< skip + limit`, and its rank
within its own shard is at most its global rank, so it is among the first
`skip + limit` of its shard. Every document contributing to `W` is therefore fetched.
The merged prefix of length `skip + limit` is a prefix of the true merged sequence,
so taking `[skip, skip + limit)` of it is exactly `W`.

This is exact and cheap. The approximation people actually write — per-shard `limit`
then merge — is the one this paragraph exists to forbid.

### 3.3 Why `count` is exact, not approximate, and why it is not a sum

The port's `count` contract (`crates/dxdb-storage/src/traits.rs`, `count`) says it
"must equal the length of the equivalent `query()` — including with a `limit`, which
upstream spells out: `.count({limit: 3})` must return what
`.query({limit: 3}).documents.length` returns."

That single sentence makes `count` non-trivial under sharding, and it is the one
place a careless implementation is *silently wrong* rather than merely slow:

- With **no** `limit` and no `skip`, `count` is `sum over shards of count(shard)`.
  Each shard counts its own matches; the sums are disjoint (`INV-3`), so the total is
  the true count. Exact.
- With a **`limit`**, the plain sum is **wrong**. Each shard would return up to
  `limit`, so the sum would be up to `N * limit` and would exceed the true answer.
  The correct rule: `count`s here are `min(limit, merged_len)`, computed by taking
  the same merge as 3.2 and counting the documents in the window. Concretely, when
  `limit` is present the wrapper must either (a) run the merge and count, or (b) sum
  per-shard counts and then clamp to `limit` — and (b) is only valid because each
  per-shard count is already clamped to `limit`, so the sum clamped to `limit` equals
  `min(limit, total)` when `total >= limit`, and equals the true total otherwise.
  Clause (b) is exact and is the implementation of record, because it avoids
  materialising documents to answer a count.
- With **`skip`** and no `limit`, `count` is `max(0, total - skip)`.

All three follow from "count equals the length the equivalent query would return",
which is the inner contract, not an invention. Stated as the port's own rule so there
is no ambiguity: `count_N(q) == query_N(q).documents.len()` for every `q` (that is
`INV-2`).

### 3.4 Routing, and why it must be a pure function

A document goes to shard `hash(primary_key_string) mod N`.

Two properties are required, and both are testable:

- **Pure**: the same key always routes to the same shard, in every process, after
  every restart, on every platform. No `HashMap` iteration order, no `DefaultHasher`
  (whose output is explicitly unstable across releases), no address, no pointer, no
  random seed. A hash whose value differs between two processes would make two tabs
  disagree about where a document lives, which is data loss that looks like a missing
  document.
- **Stable across the wasm boundary**: the same key routes identically in a browser
  and in Node, because a database can be opened by both. The hash must therefore be
  specified, not inherited from a library whose algorithm may change: dxdb specifies
  **FNV-1a 64-bit over the UTF-8 bytes of the primary key string**, whose definition
  is stable by construction, and then `mod N`. Rationale for choosing our own value
  here is explicit: nothing in the public docs specifies a hash, so we pick one and
  record it (G-039). FNV-1a is not a cryptographic hash and does not need to be: shard
  assignment is not an attack surface (an attacker who can choose keys can already
  choose which shard to fill, and the storage does not care).

The hash is applied to the **primary key as a string**, not to the document bytes,
because the document changes on every write while the key does not. Routing on
anything mutable would move a document between shards on update, which `INV-3`
forbids.

## 4. Invariants

**INV-1 — result equivalence.** As stated in section 3, all six clauses, for every
`N >= 1`.
*Enforced by* per-shard pass-through of the query plus a k-way merge in the port's
code-unit order, with `skip`/`limit` applied post-merge (3.1, 3.2).
*Proved by* `inv_01_results_equal_unsharded` — a differential test run against
`dxdb-memory` as the unsharded reference, over a corpus that includes surrogate-pair
keys, duplicate sort values, `skip`/`limit` combinations including `limit: 0` and
`limit` > total, and both modes.

**INV-2 — count equals query length.** For every prepared query `q` and every `N`,
`count_N(q) == query_N(q).documents.len()`.
*Enforced by* the clamping rule in 3.3 and by a debug assertion in `count` that
re-runs the query in debug builds. *Proved by*
`inv_02_count_equals_query_length`.

**INV-3 — exactly one home.** Every document is stored in exactly one shard at any
time: a write lands in one shard, an update rewrites the same shard (routing is a pure
function of the primary key, 3.4), and no document appears in two shards' results.
*Enforced by* routing on the immutable primary key, plus a write path that writes to
exactly the routed shard. *Proved by* `inv_03_each_document_lives_in_one_shard`,
which writes a large corpus and asserts the sum of per-shard document counts equals
the total, and that `find_documents_by_id` over all ids yields no duplicates.

**INV-4 — routing is a pure, stable function.** For any primary key and any `N`,
`shard_of(key, N)` is the same value across calls, processes, restarts and targets,
and equals `fnv1a64(key.as_bytes()) mod N`.
*Enforced by* specifying FNV-1a rather than using a library hasher, and by taking the
primary key as a string. *Proved by*
`inv_04_routing_is_pure_and_matches_the_specification`, with a committed vector file
of (key -> shard) pairs so a change to the hash is a test failure rather than a
silent data migration.

**INV-5 — write atomicity is preserved per document.** A `bulk_write` row that fails
in an unsharded storage fails identically when sharded, and one that succeeds
succeeds; the per-row error list is the same set, independent of how the rows were
distributed across shards.
*Enforced by* forwarding each row to its own shard's `bulk_write` and concatenating
the per-row errors. *Proved by* `inv_05_per_row_errors_match_unsharded`.

**INV-6 — change streams lose nothing and duplicate nothing.** Every change emitted
by any shard reaches the wrapped instance's subscribers exactly once, and the
`EventBulk` stream is a valid sequence: each event's checkpoint advances.
*Enforced by* merging the `N` shard streams into one, with a filter so an event is
never emitted twice. *Proved by* `inv_06_change_stream_is_complete_and_unique`.

**INV-7 — configuration mismatch is refused, not reinterpreted.** Opening an existing
collection with a different `shards` or `mode` than it was created with returns
`ShardingConfigMismatch` at open and reads no documents.
*Enforced by* persisting the config in the internal store and comparing at open.
*Proved by* `inv_07_config_mismatch_is_refused`.

**INV-8 — `N = 1` is behaviourally identical to no wrapper.** With `shards: 1`, every
operation is observationally identical to the unwrapped storage, including the
number of underlying calls being exactly one per operation.
*Enforced by* routing to shard 0 and forwarding without a merge when `N == 1`.
*Proved by* `inv_08_single_shard_is_a_pass_through`.

**INV-9 — `cleanup` and `remove` are complete.** `cleanup(min_deleted_time)` calls
every shard and returns `false` if **any** shard reports more work remains; `remove()`
removes every shard, and a partial failure is reported rather than swallowed.
*Enforced by* folding the per-shard booleans with `&&` and collecting errors.
*Proved by* `inv_09_cleanup_and_remove_cover_every_shard`.

**INV-10 — attachments resolve on their document's shard.** An attachment read for a
document id goes to that id's shard, so attachments never need a cross-shard lookup
and never become unreachable after a re-shard.
*Enforced by* routing the attachment read by document id. *Proved by*
`inv_10_attachments_follow_their_document`.

## 5. Failure modes

| Failure | What the caller sees | What the wrapper does |
| --- | --- | --- |
| `shards: 0` in a schema | schema validation error at open | rejected by type (`NonZeroU32`) before it reaches storage |
| Schema has `sharding` but the storage was not wrapped | unsharded behaviour; the block is inert | documented; the wrapper is the only thing that reads the block |
| Config changed on an existing database | `ShardingConfigMismatch` at open | refuses; no document is read (INV-7) |
| One shard's `query` fails | the error, with the shard index in it | the whole query fails: a partial merge would silently return an incomplete result, which is worse than an error |
| One shard's `bulk_write` fails wholesale | that shard's rows all fail; other shards' rows still wrote | per-document atomicity is preserved; the error list names the rows. **Cross-shard writes are not atomic** and cannot be, given the inner contract's "no cross-document transaction" (PLAN.md 4.2) — stated in 6 |
| A shard is missing at open (deleted from storage) | `ShardMissing { index }` at open | refuses to serve an incomplete collection; a missing shard means documents are unreachable, and returning a subset would look like data loss to the user without being reported as such |
| `N` larger than the document count | works; most shards are empty | no special case; empty shards participate in the merge trivially |
| Duplicate primary keys across shards (corruption) | `inv_03` fails in tests; at runtime the merge emits both | not silently deduplicated: two documents claiming one id is corruption and the wrapper surfaces it rather than picking one |
| Change stream subscription after a shard's stream ended | the wrapped stream ends | the wrapper does not reconnect a shard silently; a storage that ends its change stream has ended it |

## 6. Performance targets

All **unmeasured**. The published "30-50% on reads, 25% on writes" is a target to
reproduce or refute on our hardware (G-003), and section 1.4 already notes the
upstream qualifier that only *some* storages benefit.

| Target | Value | How measured |
| --- | --- | --- |
| Read gain vs unsharded, slow-store surrogate (dxdb-filesystem, 10k docs, `N=10`) | **unmeasured**; upstream target 30-50% | `criterion` bench `sharding_read`, JSON result into `docs/BENCHMARKS.md` |
| Write gain, same setup | **unmeasured**; upstream target 25% | `criterion` bench `sharding_write` |
| Merge overhead when the inner storage is fast (dxdb-memory) | **unmeasured**; expected to be a **loss** | same bench, and the loss is reported, not hidden (PLAN section 11's "including where they lose") |
| Per-query cost of `skip + limit` over-fetch | **unmeasured** | derived from `sharding_read` with added skip values |

The honest expectation, recorded before measuring so it cannot be retrofitted: on a
fast in-memory storage the merge is pure overhead and sharding will be slower. The
feature earns its place only on stores whose per-store cost dominates, which is
exactly what upstream says and exactly what our bench must show or the claim gets
dropped.

## 7. Non-goals

- **No re-sharding.** `N` is fixed per schema. A migration is: export documents,
  create a new sharded database with the new `N`, import. Same portable path as every
  other storage divergence (G-002).
- **No cross-shard transactions.** The inner contract has no cross-document
  transaction, so the wrapper cannot invent one.
- **No sharding key other than the primary key.** A user-chosen shard key would be a
  new concept with its own migration story; it is not what upstream documents.
- **No query rewriting.** The wrapper passes the prepared query through unchanged. It
  does not re-plan, does not pick a better index per shard, and does not push
  aggregates down.
- **No partial results on shard failure.** See the failure table.
- **No replication awareness.** The port's replication runs against a storage
  instance; sharding is below it. Replicating from a sharded collection is not
  special-cased.
- **No `RxDB_UTILS_GLOBAL`/`PREMIUM_FLAG_HASH` machinery, no limits, no phone-home.**
  G-006.
- **No compatibility with upstream's sharded on-disk layout.** G-002. Upstream's
  storage names, shard naming scheme and any persisted metadata are unknown to us and
  we invent ours (G-039), so an upstream sharded database cannot be opened and the
  portable export/import path is the answer.

## Acceptance list

1. A collection written through `shards: 10` returns documents from `query` in the
   same order an unsharded `dxdb-memory` collection returns them, for a corpus that
   includes surrogate-pair sort values. `[INV-1]`
2. `limit` with `skip`, over a corpus where the global window's documents are
   distributed unevenly (including all in one shard), returns exactly the unsharded
   window. `[INV-1]`
3. `count` equals `query(...).documents.len()` for every prepared query, including
   with `limit` and `skip`, and including `limit: 0`. `[INV-2]`
4. `count` with a `limit` smaller than the total does not return the sum of per-shard
   counts. `[INV-2]`
5. Summing per-shard document counts equals the number of documents written, for a
   corpus larger than `N`. `[INV-3]`
6. `find_documents_by_id` over all ids returns no duplicates and the same values as
   unsharded. `[INV-3]`
7. Routing matches the committed `(key -> shard)` vector file for `N` in {1, 2, 3, 7,
   10, 64, 1000}. `[INV-4]`
8. A `bulk_write` with some conflicting rows produces the same per-row error set as
   unsharded, for the same rows in different orders. `[INV-5]`
9. Every change event from every shard reaches one subscriber exactly once.
   `[INV-6]`
10. Opening with a different `shards` value returns `ShardingConfigMismatch` and
    reads no documents. `[INV-7]`
11. Opening with a different `mode` returns `ShardingConfigMismatch`. `[INV-7]`
12. `shards: 1` makes exactly one underlying call per operation and returns
    unsharded results. `[INV-8]`
13. `cleanup` returns `false` when exactly one shard reports more work. `[INV-9]`
14. `remove()` issues a remove to every shard. `[INV-9]`
15. An attachment read resolves through the document's own shard. `[INV-10]`
16. `shards: 0` is rejected before reaching storage. `[INV-7]`
17. Both `mode` values produce identical results for the same corpus and `N`.
    `[INV-1]`
18. A missing shard at open returns `ShardMissing` naming the index. `[INV-7]`

## Test map

Tests live in `crates/cleanroom/dxdb-sharding/tests/`. The differential harness is a
shared fixture that runs the same corpus against `dxdb-memory` unwrapped and against
each `(N, mode)` combination.

| Acceptance | Test | File |
| --- | --- | --- |
| 1, 17 | `acceptance_01_17_query_order_matches_unsharded_in_both_modes` | `tests/acceptance.rs` |
| 2 | `acceptance_02_limit_with_skip_is_exact_under_skew` | `tests/acceptance.rs` |
| 3, 4 | `acceptance_03_04_count_equals_query_length_including_limit` | `tests/acceptance.rs` |
| 5, 6 | `acceptance_05_06_no_document_is_duplicated_or_lost` | `tests/acceptance.rs` |
| 7 | `acceptance_07_routing_matches_the_vector_file` | `tests/acceptance.rs` |
| 8 | `acceptance_08_per_row_errors_match_unsharded` | `tests/acceptance.rs` |
| 9 | `acceptance_09_change_stream_is_complete_and_unique` | `tests/acceptance.rs` |
| 10, 11, 16, 18 | `acceptance_10_11_16_18_config_errors_are_refused_at_open` | `tests/acceptance.rs` |
| 12 | `acceptance_12_single_shard_is_a_pass_through` | `tests/acceptance.rs` |
| 13, 14 | `acceptance_13_14_cleanup_and_remove_cover_every_shard` | `tests/acceptance.rs` |
| 15 | `acceptance_15_attachments_follow_their_document` | `tests/acceptance.rs` |
| INV-1 | `inv_01_results_equal_unsharded` | `tests/invariants.rs` |
| INV-2 | `inv_02_count_equals_query_length` | `tests/invariants.rs` |
| INV-3 | `inv_03_each_document_lives_in_one_shard` | `tests/invariants.rs` |
| INV-4 | `inv_04_routing_is_pure_and_matches_the_specification` | `tests/invariants.rs` |
| INV-5 | `inv_05_per_row_errors_match_unsharded` | `tests/invariants.rs` |
| INV-6 | `inv_06_change_stream_is_complete_and_unique` | `tests/invariants.rs` |
| INV-7 | `inv_07_config_mismatch_is_refused` | `tests/invariants.rs` |
| INV-8 | `inv_08_single_shard_is_a_pass_through` | `tests/invariants.rs` |
| INV-9 | `inv_09_cleanup_and_remove_cover_every_shard` | `tests/invariants.rs` |
| INV-10 | `inv_10_attachments_follow_their_document` | `tests/invariants.rs` |

Cross-check note: this crate also runs the port's storage conformance suites
(`make gate-storage`, `make gate-query-correctness`) with the sharded wrapper
configured, which is CLEANROOM.md section 6's "cross-check against the open-source
baseline". That is legitimate and strong here for the reason it is legitimate
everywhere in this lane: the contract is defined by the Apache-2.0 side, so a sharded
storage that passes the same suite as `dxdb-memory` is measurably indistinguishable
from it for every behaviour the suite covers. It is not a substitute for `INV-1`'s
ordering corpus, which is where the code-unit-order trap lives.
