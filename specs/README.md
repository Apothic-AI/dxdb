# `specs/`: clean-room specifications

Every file here is a behavioural specification for a paid upstream feature,
written from public documentation by someone who has not read the paid code. The
implementer's only allowed source for the matching crate is the spec plus
generic prior art (CLEANROOM.md section 2).

Start from [`_TEMPLATE.md`](./_TEMPLATE.md). Read [`READING-LIST.md`](./READING-LIST.md)
before you read anything else, and [`../docs/CLEANROOM-GAPS.md`](../docs/CLEANROOM-GAPS.md)
whenever the public docs run out.

## Two crate trees, and the naming collision this file resolves

CLEANROOM.md section 4 lists clean-room crates whose names collide with crates
that other lanes are porting from the **Apache-2.0** tree. The collision is real
and it is not resolved by renaming anybody's crate, because both sides need the
straightforward name:

- `dxdb-indexeddb`, `dxdb-opfs`, `dxdb-sqlite`, `dxdb-filesystem` are, upstream,
  **Apache-2.0 ports**. They are ported by lanes F and G from
  `~/Code/rxdb` and live at `crates/storages/dxdb-*`.
- The same four names appear in the CLEANROOM table as the targets for the paid
  **storage modules**, which are not in the Apache-2.0 tree at all.

The resolution, which this file is the authority for:

| Path | Owner | What it is |
| --- | --- | --- |
| `crates/storages/dxdb-indexeddb` | lane F | Apache-2.0 port of upstream `storage-dexie`+IndexedDB behaviour |
| `crates/storages/dxdb-opfs` | lane F | Apache-2.0 port, OPFS engine, `storage/` conformance |
| `crates/storages/dxdb-sqlite` | lane G | Apache-2.0 port of `src/plugins/storage-sqlite` |
| `crates/storages/dxdb-filesystem` | lane G | Apache-2.0 port, `redb` on native |
| `crates/cleanroom/dxdb-logger` | lane I | clean-room capability, wraps any storage |
| `crates/cleanroom/dxdb-encryption` | lane I | clean-room envelope + field encryption |
| `crates/cleanroom/dxdb-filesystem` | lane I | **wrapper** capability over the port crate, never a second engine |
| `crates/cleanroom/dxdb-indexeddb` | lane I | **wrapper** capability over the port crate |
| `crates/cleanroom/dxdb-opfs` | lane I | **wrapper** capability over the port crate |
| `crates/cleanroom/dxdb-sqlite` | lane I | **wrapper** capability over the port crate |

The rule that keeps this from duplicating work:

> A clean-room crate whose name is also a port crate's name is a **capability
> wrapper**: it implements behaviour on top of the port crate's storage engine,
> and it must not contain a storage engine of its own.

Concretely: `crates/cleanroom/dxdb-filesystem` may not write files. It may add
the clean-room behaviour that upstream sells on top of a filesystem storage
(premium multi-process semantics, layout choices we own) by wrapping whatever
`crates/storages/dxdb-filesystem` exposes. If the capability turns out to be
inseparable from the engine, the engine change belongs in lane G's crate, made
through the leader, and the spec records that split rather than forking an
engine. Duplicating a storage engine would create two on-disk formats we then
have to support forever.

## Crate names with no collision

These are clean-room capabilities with no Apache-2.0 counterpart and no port
crate, so they get the plain name in `crates/cleanroom/`:
`dxdb-logger`, `dxdb-encryption`, `dxdb-worker`, `dxdb-sharding`,
`dxdb-memory-tier`, `dxdb-meta-cache`, `dxdb-fulltext`, `dxdb-query-optimizer`,
`dxdb-server`.

## Stage map

| Stage | Crates | Spec files |
| --- | --- | --- |
| B1 | `dxdb-logger` | [`logger.md`](./logger.md) |
| B2 | `dxdb-encryption`, `dxdb-filesystem` (wrapper) | `encryption.md`, `filesystem-tier.md` |
| B3 | `dxdb-indexeddb` (wrapper), `dxdb-opfs` (wrapper), `dxdb-worker` | `browser-storage-tier.md`, `worker.md` |
| B4 | `dxdb-sqlite` (wrapper), `dxdb-memory-tier`, `dxdb-sharding`, `dxdb-meta-cache` | `sqlite-tier.md`, `memory-tier.md`, `sharding.md`, `meta-cache.md` |
| B5 | `dxdb-fulltext`, `dxdb-query-optimizer`, `dxdb-server` | `fulltext.md`, `query-optimizer.md`, `server.md` |

Only the stages with a spec file exist as work. A crate with no spec is not
started, and a spec with no acceptance list is not reviewed.

## What every spec must contain

Six sections in this order, plus a numbered acceptance list and a test map:

1. Observable behaviour
2. Public API shape (from docs)
3. Invariants
4. Failure modes
5. Performance targets
6. Non-goals

The acceptance list is numbered. Every test names its numbers, and a test that
proves an invariant repeats the invariant id. That is how a behavioural
acceptance is written from public documentation instead of from someone's
memory of the closed implementation.

## Provenance mechanics

- Commit subjects start with `[cleanroom]` and name the spec file, for example
  `[cleanroom] specs/logger.md: acceptance list and invariants`.
- Every clean-room crate's crate-level doc comment cites its `specs/<feature>.md`
  and states that it was written from that spec.
- No RxDB premium product names, no tier language, no crown emoji. Descriptive
  names only.
- No comparative marketing in a spec until a number from
  `docs/BENCHMARKS.md` supports it.
