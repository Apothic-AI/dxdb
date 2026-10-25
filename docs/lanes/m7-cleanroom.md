# Lane brief: M7 clean room (track B)

You own `crates/cleanroom/` and `specs/`. You are two roles in one lane, and the
separation is the whole point, so hold it explicitly.

## Read first, in this order

1. `docs/CLEANROOM.md` — all of it. It is the governing document for this lane and
   it is not background reading.
2. `docs/PLAN.md` sections 3, 4.2 (the wrapper pattern and why it is the clean-room
   boundary), 6 M7, 7, 9.
3. `docs/WORKTREES.md`, `docs/GATES.md`.

## The rule you must not break

**You will not read `rxdb-premium`.** Not its source, not its minified bundles, not
its `.d.ts`, not its source maps, not its encrypted payloads, not its installer.
`docs/CLEANROOM.md` section 2 states this and section 9 lists contamination as the
top risk to the claim. You may read:

- public documentation (RxDB's docs site content, plugin pages, pricing page)
- the Apache-2.0 source at `~/Code/rxdb`
- generic prior art: crate docs, RFCs, papers, standards
- the public issue tracker and blog posts

If you find yourself about to inspect anything under a premium path, stop and say
so in your report.

## What the plan explicitly forbids (CLEANROOM 2 and 3)

- No copying constants, magic numbers, index layouts, key-derivation parameters, or
  string formats out of the paid code. If a value is not derivable from public docs
  or the Apache-2.0 source, **choose our own value and document that we did**.
- No using the `PREMIUM_FLAG_HASH` or `RXDB_UTILS_GLOBAL.premium` mechanism. We are
  not circumventing a license check; we are writing a different plugin.
- **dxdb ships no artificial limits and no phone-home.** The gates are Apache-2.0
  and could be ported verbatim; we will not. Record this as an intentional
  divergence in `docs/CLEANROOM-GAPS.md`.
- **We are not wire-compatible with premium on-disk formats and cannot be.** State
  it plainly in each storage's docs and provide the tested portable migration path
  (export documents, import documents).

## Sequencing (CLEANROOM 7)

| Stage | Work | Exit gate |
| --- | --- | --- |
| B0 | `specs/` layout, `CODEOWNERS`, PR template question, gap register, reading list | a reviewer can tell, from artifacts alone, who read what |
| B1 | `dxdb-logger` spec + implementation | proves the pipeline end to end |
| B2 | `dxdb-encryption`, `dxdb-filesystem` | envelope + storage conformance |
| B3 | `dxdb-indexeddb`, `dxdb-opfs`, `dxdb-worker` | browser parity |
| B4 | `dxdb-sqlite`, `dxdb-memory-tier`, `dxdb-sharding`, `dxdb-meta-cache` | full storage matrix |
| B5 | `dxdb-fulltext`, `dxdb-query-optimizer`, `dxdb-server` | the extras |

Note a naming collision you must resolve, not ignore: B2-B5 name crates that lane F
and lane G are building as **ports** (`dxdb-filesystem`, `dxdb-sqlite`,
`dxdb-indexeddb`, `dxdb-opfs`) from CLEANROOM's own table, where those POs are the
clean-room targets for premium modules. Resolve it this way:

- Lane F owns `crates/storages/dxdb-indexeddb` and `dxdb-opfs`; lane G owns
  `dxdb-filesystem` and `dxdb-sqlite`. Those are the **open-source port** of the
  Apache-2.0 storages that share those names upstream.
- Your clean-room crates live under `crates/cleanroom/` with their own names from
  the CLEANROOM table (`dxdb-logger`, `dxdb-encryption`, `dxdb-worker`,
  `dxdb-sharding`, `dxdb-memory-tier`, `dxdb-meta-cache`, `dxdb-fulltext`,
  `dxdb-query-optimizer`, `dxdb-server`).
- Where CLEANROOM lists a crate name that the port also uses, the clean-room
  **capability** is a wrapper crate in `crates/cleanroom/` (e.g. the clean-room
  browser storage wrapper), and the port crate provides the underlying storage.
  Document the split in `specs/README.md`. Do not duplicate a storage engine.

Start at B0+B1. Do not open B2 until the logger is real and gated.

## The spec template (CLEANROOM 6 and 10)

Every `specs/<feature>.md` has these six sections plus a numbered acceptance list:

1. Observable behaviour
2. Public API shape (from docs)
3. Invariants
4. Failure modes
5. Performance targets
6. Non-goals

The acceptance list is numbered and each test references its numbers. That is how
behavioural acceptance is written from public docs rather than from an
implementation.

## Verification without the reference implementation (CLEANROOM 6)

- **Invariants we can prove against our own contract.** Sharding: results equal
  unsharded. Memory tier: after reconciliation, memory equals storage. Encryption:
  ciphertext never appears in plaintext after decrypt, AAD substitution is rejected,
  rotation preserves plaintext. Meta cache: a stale snapshot is always detected.
- **Cross-check against the open-source baseline.** Every clean-room storage runs
  the same conformance suite as `dxdb-memory` and `dxdb-filesystem`. The contract is
  defined by the Apache-2.0 side, so this is legitimate and strong.
- **Performance claims get measured or dropped.** Their published numbers (3-4x, 30-50%
  reads, 25% writes, 200ms startup) are targets to benchmark against on our own
  hardware, not facts to repeat. Any "faster than X" sentence needs a number.
- **Gap register.** `docs/CLEANROOM-GAPS.md` records every place the public docs
  were insufficient and what we decided. Publishing it is a feature.

## Provenance mechanics (CLEANROOM 8)

- Commit subjects start with `[cleanroom]` and name the spec file.
- Each clean-room crate's header comment cites `specs/<feature>.md`.
- Naming: no RxDB premium product names, no crown emoji, no tier language, no
  "Premium". Descriptive names only. `dxdb-*` is ours.
- No comparative marketing until benchmarks exist.

## Workspace mechanics

`crates/cleanroom/*` is not in the workspace members glob yet (cargo fails on an
empty glob). Your first commit adds the single glob line and nothing else:
`workspace: add the cleanroom member glob`.

## Your gate

```bash
make check && make clippy && make test && make wasm
```

per stage, plus the invariants tests for whatever stage you reached. Report per
stage: spec written, invariants tested, gate status. Report B0 artifacts
(`CODEOWNERS`, PR template, gap register, reading list) as files with paths.

## Reporting

Report which stage you reached, the specs written, the invariants proven (naming
the test), gates with exit statuses, and every gap decision you made. If you could
not complete B5, say which stage stopped and why — do not round up.
