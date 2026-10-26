# Lane H (m6-plugins) — continuation

Written to be pasted verbatim into a fresh session. Dense on purpose; the repo and
`docs/lanes/m6-port-plan.md` carry the rest.

## 0. Orientation

- Worktree: `/home/bitnom/Code/dxdb-worktrees/m6-plugins`, branch `lane/m6-plugins`.
- Own: `crates/plugins/` (including `crates/plugins/README.md`) and
  `docs/lanes/m6-port-plan.md`. Never merge to master; the leader does.
- Never edit leader-owned files (`Cargo.toml`, `Makefile`, `docs/PLAN.md`,
  `docs/STATUS.md`, `docs/WORKTREES.md`, `.github/`). The one exception was already
  taken: the `members` glob line, now
  `members = ["crates/dxdb-*", "crates/storages/*", "crates/plugins/*", "xtask"]`
  with **no** `exclude` (leader's `d7f632b` removed it).
- Build discipline: `make check|clippy|test|wasm` only. Never build locally. Never
  wrap a make target in `flock` — the Makefile uses a reentrant
  `xtask/with-build-lock.sh` and an outer flock deadlocked the fleet for 75 minutes
  once. A route fallback (`xtask/remote-route.sh` → `flyctl proxy`) is automatic; if
  you see "No route to host" / "connection refused" / `rsync exited with 255`, do not
  debug it, do not build locally, do not skip the gate — report it and do non-build
  work.
- Never `cargo fmt --all` (reformats other lanes' crates). Use
  `rustfmt --edition 2021 <your files>`.
- Commit each plugin as it goes green. Never add `Co-Authored-By`.

## 1. Completed and merged to master

Merged (leader verified): tranche 1 (classification + Hooks design + first two crate
skeletons), tranche 2 (query-builder, update), tranche 3 (key-compression,
migration-schema, migration-storage).

Committed on `lane/m6-plugins`, **not yet merged** (master is `986d02c`, my HEAD has
these on top):

| commit | crate | upstream lines | tests |
| --- | --- | ---: | ---: |
| `68629d3` | `local-documents` | 776 | 21 (3 lib + 18 integration) |
| `523aa15` | `leader-election` | 132 + `broadcast-channel@7` | 26 (3 + 23) |
| `6fff9b4` | `cleanup` | 291 | 17 (3 + 14) |

All three were green on `check`/`clippy`/`test`/`wasm` before being committed.

Earlier lane commits still on the branch and already merged by the leader:
`b59f18c` (workspace glob), `856c514`, `d3e8fe1` (docs corrections), `8fc7c0e`
(query-builder+update), `66db5ac` (Cargo.lock), `0881a76` (gate results in docs),
`e4bdf72` (key-compression), `91bbd7b` (migration-schema), `6358c9f`
(migration-storage).

Crates on disk in `crates/plugins/`: `query-builder`, `update`, `key-compression`,
`migration-schema`, `migration-storage`, `local-documents`, `leader-election`,
`cleanup`, plus `README.md`. **192 tests total** (lib + integration).

## 2. In progress: nothing

`git status --short` is clean. The working tree compiles and all four gates are green
on HEAD (`6fff9b4`): check 0, clippy 0, test 0, wasm 0.

The only uncommitted-at-time-of-writing artefact is this file.

## 3. Exact next task

**Write the sequenced remainder table into `docs/lanes/m6-port-plan.md`** (the user
called it "the more important ask"), then report.

It goes as a new section after §6, titled roughly
`## 7. The portable remainder, sequenced`. Columns, one row per crate:
**crate name | upstream lines | hard dependencies (crate that must exist first) |
what makes it non-trivial**. Make the sequence defensible and state *why* the order
is what it is (unblocking value vs risk), and say plainly how many are gated on other
lanes versus buildable now.

The 13 portable modules left in scope (measured with
`find <dir> -name '*.ts' | xargs cat | wc -l` at `3b763ee17`):

| module | lines | notes for the table |
| --- | ---: | --- |
| `dev-mode` | 3,088 | Largest. Registers **10** of the 24 hooks (`preCreateRxDatabase`, `preCreateRxCollection`, `preCreateRxSchema`, `preCreateRxQuery`, `prePrepareRxQuery`, `prePrepareQuery`, `preStorageWrite`, `createRxDatabase`, `createRxCollection`, `createRxDocument`). Owns the overwritables `isDevMode`/`deepFreezeWhenDevMode`/`tunnelErrorMessage`. Also holds `check-migration-strategies.ts` (the COL11/12/13 validators) and `check-orm.ts` (COL14). Non-trivial: needs almost every other crate, and the overwritable seam must be designed (Rust has no `Object.assign` on a module-level object). |
| `crdt` | 577 | Needs a CRDT op log, `_meta` handling, and it **patches `incrementalPatch`** (`crdt/index.ts:393`) which is `local-documents`' method — so it depends on local-documents landing first. Two hooks, two prototypes. |
| `state` | 577 | `RxState` with a `$` stream, `createRxState` hook, and a `_cleanup()` that the cleanup plugin's state path calls (`cleanup-state.ts`). Touches the cleanup crate. |
| `pipeline` | 497 | `addPipeline` on collections, filters on `eventBulks$` with `!bulk.isLocal` (`rx-pipeline.ts:89,305`) — i.e. it consumes the same `isLocal` distinction local-documents established. Needs the collection event stream (lane D). |
| `backup` | 413 | Two hooks (`preCloseRxDatabase` cancel). Needs a storage-agnostic dump plus a re-import path; flag the encryption/compressed-schema interaction. |
| `json-dump` | 236 | `db.dump()` / import. Needs the document cache and a storage read; overlaps `backup` — state the difference so a reader is not confused by two dump plugins. |
| `attachments-compression` | 236 | Storage wrapper over `dxdb-storage` traits; needs a compressor that works on wasm (`CompressionStream` via `web-sys`) and native (`flate2`), behind a feature. Pairs with lane E's `dxdb-attachments` (data model), which is theirs. |
| `encryption-crypto-js` | 233 | Storage wrapper. **Cipher decision already recorded**: upstream uses crypto-js AES (`EVP_BytesToKey`, MD5, 1 iteration, AES-256-CBC); PLAN.md §9 picks chacha20poly1305 + argon2. So it is a deliberate parity break — upstream ciphertext will not be readable by dxdb. Needs a `dxdb-plugin-encryption` name (not `-crypto-js`, which would promise interop we do not have). |
| `validate-ajv` | 70 | Collapse to one `validate` crate with a `precompiled` feature — see §6.5 of the plan; needs the user's decision. |
| `validate-ajv-standalone` | 126 | Same crate, `precompiled` feature. |
| `validate-z-schema` | 58 | Omit — the difference is which JS library compiles the schema, and there is one engine here (`jsonschema` 0.60.1). |
| `validate-is-my-json-valid` | 30 | Omit, same reason as above. |
| `vector` | 37 | Three distance functions (`euclidean`, `manhattan`, `cosineSimilarity`); depends only on `dxdb-jsnum`. **Buildable right now, unblocked.** |

Key sequencing facts to state honestly:
- `vector` and the `validate` collapse are the only two with **no lane dependency**.
- `dev-mode` is the long pole and gates the "full ported test corpus" gate in M6, but it
  depends on nearly everything else, so it must come **last**.
- `crdt` depends on `local-documents` (already done) and on `state`-style `_meta` flows.
- `pipeline` and `state` both need lane D's collection event stream.
- `attachments-compression` and `encryption` are storage wrappers and share a blocker:
  `dxdb_storage::RxStorageInstance` is **not dyn-compatible** (`impl Future` in 8
  methods), so no wrapper can exist until lane D ships the `DynRxStorageInstance` twin
  (PLAN.md §3.3, `dynosaur`). State this once, prominently — it blocks three plugins.

Then report to the user: the three tranche-4 plugins' ported test counts with upstream
citations, gate statuses, deliberately-unreproduced upstream behaviours, and the table.

## 4. Not written down in the repo — traps and learned facts

**Upstream behaviours that surprised me (all verified against the pinned checkout):**

- **local-documents isolation is structural, not a flag.** Local docs live in a
  separate storage instance named `plugin-local-documents-<collection>`
  (`local-documents-helper.ts:80-82`; empty name for a database parent, `:22`).
  Replication binds to a collection's storage instance `changeStream()`
  (`replication-protocol/index.ts:200`, `upstream.ts:95`), so it cannot see that
  instance. `isLocal: true` (`rx-local-document.ts:366,374`) serves a *different*
  consumer: it keeps events out of the change-event buffer (`change-event-buffer.ts:54`)
  and out of pipelines. Do not conflate the two.
- **A fresh local document has `_rev: ''` on purpose** (`utils-document.ts:34-40`) so a
  storage that forgets to assign a revision throws. It therefore *fails* the
  `minLength: 1` schema check before the write and passes after. A test must assert
  both stages.
- **leader-election has no liveness check.** A suspended tab never calls `die()`, so
  `is_leader` stays true and others elect a replacement: two leaders on resume. Upstream
  only mitigates via `responseTime * 4` on fallback rounds, documented as making
  duplicates "less likely". The port reproduces this and adds opt-in
  `resume_after_suspension` / `observe_foreign_claim`.
- **leader-election's listener is edge-triggered**: attached inside a round and removed
  after (`leader-election.js:145,168`), so a message sent between rounds is never seen.
  A buffering transport must clear at round start or a stale `apply` from a dead peer
  makes every survivor stand down. This was a real bug in my first port.
- **In a synchronous harness, rounds do not overlap**, so three sequential rounds elect
  three leaders. That is the transport's job (`drain_messages_within` must actually
  wait). Contention has to be modelled by injecting on the *first in-round drain*.
- **migration-schema's delete sentinel is `null`, not `false`.** `migrateDocumentData` is
  documented "return final object or null if migrationStrategy deleted it"
  (`migration-helpers.ts:58`), converted at `rx-migration-state.ts:600-604`. `false` has
  no meaning anywhere. Also: `_deleted`/`_meta` are stripped before the first strategy
  and restored after, and the restore is unguarded so a document with no `_deleted`
  gains an explicit `null`.
- **`preMigrateDocument` / `postMigrateDocument` have no call site anywhere in `src/`.**
  They are declared (`hooks.ts:72,76`) and registered by attachments
  (`attachments/index.ts:306`) but never fired. A faithful port registers and never
  fires them. `createRxQuery` is in the typings but not in `HOOKS` — registering it
  would throw. Do not add a 25th hook.
- **Hook ordering is asymmetric**: `after` is pushed (FIFO), `before` is unshifted
  (LIFO among befores) (`plugin.ts:103-114`). Async hooks run sequentially on purpose
  (`hooks.ts:112-116`).
- **key-compression: `keyCompression` is part of the schema hash**, so enabling it on an
  existing collection changes identity and is **DB6**, not a migration
  (`rx-database.ts:481`). Verified by hashing the same schema with the flag flipped.
  Also: names of length ≤ 3 are never compressed; the primary field and
  `_rev/_attachments/_deleted/_meta` are ignored; the composite `primaryKey` shape is
  `{key, fields, separator}`, **not** an array.
- **migration-storage swallows new-storage write conflicts** — the check at `:185-188`
  is a commented-out TODO. A re-run after a crash between copy and tombstone therefore
  "succeeds" silently with duplicates. The progress tally counts them so a caller can
  see it. The batch query's `_deleted: false` filter is load-bearing: progress comes from
  tombstones, never from a growing `skip`.
- **cleanup's boundary is strict**: `lwt < now - retention`, so a tombstone aged exactly
  the retention survives, and `minimumDeletedTime: 0` does **not** purge a tombstone
  deleted at exactly `now`.

**Port-specific decisions:**

- **No dependency on a storage crate from a plugin crate** — only `dxdb-storage` traits.
  Where a wrapper was impossible I said so and left the transformation layer complete
  and tested (key-compression's `KeyCompressionStorage`, migration-storage's
  `MigrationSource`/`MigrationSink`).
- **`update` ports all 14 mingo operators directly** rather than depending on a patcher.
  Nothing off the shelf reproduces `$mul`-missing→0, `$inc` skipping a non-numeric
  *target* but erroring on a non-numeric *argument*, `$unset` writing `null` into
  arrays, or `$push`'s `$slice` keeping first-N/last-N. `$setOnInsert` is an **error**
  (mingo 7.2.5 never shipped it despite upstream's typings).
- **`CleanupGroup` is an addition, not a port** — max retention across collections
  sharing a storage. It is opt-in so the default path stays upstream's.
- **Test provenance convention**: `upstream_<line>_<slug>` for cases ported from an
  upstream test file; `mingo_*` for behaviour pinned from a dependency's source;
  everything else names the upstream file:line it asserts in a doc comment or the test
  body. Where a Rust type makes an upstream negative case unrepresentable (e.g.
  migration COL11/COL13 array/non-function strategies), the test says which upstream
  case it stands in for rather than claiming coverage.

**Brief errata already found and reported** (do not re-litigate, but do not silently
"fix" the brief either): query-builder has no `.remove()`; update has no
`incrementalPatch`/`incrementalModify`/`atomicUpdate`; upstream uses `mingo`, not
`modifyjs`.

## 5. Blocked / questions for the user

Nothing hard-blocks the worktree: HEAD is green and committed.

Carried questions that need a **decision**, listed as §6.3–6.5 of
`docs/lanes/m6-port-plan.md`:

1. **`attachments` path ownership** — the lane brief lists it first in the build order,
   but `docs/WORKTREES.md` assigns `crates/dxdb-attachments/` to lane E. I am treating
   E as authoritative and have not created the crate. Confirm.
2. **Encryption cipher divergence** — chacha20poly1305 + argon2 (PLAN.md §9) instead of
   upstream's crypto-js AES means upstream ciphertext is unreadable. Confirm that is
   acceptable; if not, an `upstream-compat` feature needs a written justification in
   PLAN.md §9 because it deliberately uses a weak KDF.
3. **Collapse the four validator plugins to one** `validate` crate + `precompiled`
   feature, omitting `validate-z-schema` and `validate-is-my-json-valid` (they differ
   only in which JS library compiles the schema). Confirm that is the intended reading
   of "port behaviour, not structure".

Also worth telling the user when reporting: **the `dyn` twin is the single biggest
unblocker for this lane** — it gates `key-compression`'s wrapper (already written,
waiting), `encryption`, and `attachments-compression`, i.e. three of the remaining
thirteen.
