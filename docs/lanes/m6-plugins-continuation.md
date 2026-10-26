# Lane H (m6-plugins) — continuation

Overwrite of the previous brief; that one described a state three tranches old. Dense on
purpose — `docs/DECISIONS.md`, `docs/STATUS.md` and `docs/lanes/m6-port-plan.md` carry
everything else.

## 0. Orientation

- Worktree `/home/bitnom/Code/dxdb-worktrees/m6-plugins`, branch `lane/m6-plugins`.
- Own `crates/plugins/`, `crates/plugins/README.md`, `docs/lanes/m6-port-plan.md`.
- **Never merge to master** — the leader does. Never edit leader-owned files
  (`Cargo.toml`, `Makefile`, `docs/PLAN.md`, `docs/STATUS.md`, `.github/`). The one
  exception, the `members` glob line, was taken long ago and is settled.
- Build only via `make check|clippy|test|wasm`. Never build locally. Never wrap a make
  target in `flock` (the Makefile's `xtask/with-build-lock.sh` is reentrant; an outer
  flock deadlocked the fleet for 75 minutes once). A route fallback is automatic; on
  "No route to host" / "connection refused" / `rsync exited with 255`, report it and do
  non-build work.
- Never `cargo fmt --all`. Use `rustfmt --edition 2021 <files>`.
- **Stage only `crates/plugins/` plus `Cargo.lock`** — never `git add -A` (leader's
  instruction this tranche).
- Commit one plugin per commit, as it goes green. No `Co-Authored-By` ever.

## 1. Merged to master

All of this lane's work is **merged**. Verified with
`git merge-base --is-ancestor <sha> master` for each — every one `MERGED`:

| commit | what |
| --- | --- |
| `b59f18c` | workspace member glob + first two skeletons + `docs/lanes/m6-port-plan.md` |
| `856c514`, `d3e8fe1` | docs corrections (counted figures; workspace-line history) |
| `8fc7c0e` | query-builder + update (69 tests) |
| `66db5ac`, `0881a76` | Cargo.lock; gate results in the plan |
| `e4bdf72` | key-compression |
| `91bbd7b` | migration-schema |
| `6358c9f` | migration-storage |
| `68629d3` | local-documents |
| `523aa15` | leader-election |
| `6fff9b4` | cleanup |
| `e2223ab` | **core: plugin model + hook ordering tiers** |
| `907c19f`, `2bdb35c` | migrate query-builder/update/cleanup onto the registry; count fix |
| `2e3250e` | state |
| `5ebc851` | pipeline |
| `76647c9` | json-dump |
| `63b7da0` | backup |
| `61ebd56` | crdt |

`master` was at `d1dcaea` when this was written; `git merge --no-edit master` is clean.

**14 plugin crates** on disk, all merged except dev-mode (below):
query-builder (33 tests), update (44), key-compression (18), migration-schema (25),
migration-storage (16), local-documents (21), leader-election (26), cleanup (23), state
(26), pipeline (18), json-dump (19), backup (15), crdt (31) — plus `dev-mode` (29) in
progress.

## 2. In progress: `dev-mode`, uncommitted, one verification step short

`crates/plugins/dev-mode/` — `Cargo.toml`, `src/lib.rs`, `tests/dev_mode_rules.rs`. Untracked.
`git status`: ` M Cargo.lock` and `?? crates/plugins/dev-mode/` only.

**What is known green and what is not:**

- `make check` = 0, `make test` = 0 (`/tmp/dm-gates.log`: 23 integration tests + 6 lib
  tests passed). That was the state **before** three cosmetic edits.
- Since then, **unverified by a compiler**: two `clippy::collapsible_match` rewrites in
  `src/lib.rs` (the `Value::Array(fields)` arm of the SC13 check and the `Some("string")`
  arm of the SC34 check, both converted to guards), three doc-comment indentation fixes
  (`//!    ` → `//!  ` on continuation lines — `clippy::doc_overindented_list_items`),
  and removing an unused `Plugin`/`HookName` import from the test file.
- **clippy has never been green for dev-mode.** `make clippy` was `Killed` twice
  (box-side, mid-run) and the collapsible-match fixes were applied last, unverified.

So: `dev-mode` almost certainly passes; it needs one `make check && make clippy &&
make test && make wasm` before committing. If any of the three cosmetic edits broke
something it will be a compile error, not a behavioural one.

## 3. Exact next task

**Step 1 — verify and commit `dev-mode`** (after RESUME):

```
cd /home/bitnom/Code/dxdb-worktrees/m6-plugins
git merge --no-edit master            # pick up d1dcaea or later
( for t in check clippy test wasm; do echo "=== $t ==="; make $t; echo "=== $t EXIT=$? ==="; done ) >/tmp/dm-verify.log 2>&1
grep -E "=== .* EXIT=" /tmp/dm-verify.log
```

On four greens: `git add crates/plugins/dev-mode Cargo.lock` then commit with the
message drafted at the bottom of this file (§7). If only clippy fails on more
`collapsible_match`/`doc_overindented` lints, fix them and re-run — they are the same two
lint families, nothing structural.

**Step 2 — update the sequenced table** in `docs/lanes/m6-port-plan.md` (todo #37, the
last open item from the previous instruction). Section `## 7. The portable remainder,
sequenced` does **not exist yet** — §1–§6 do. Add it with one row per remaining module:
crate name, upstream lines, hard dependency, what makes it non-trivial, and a Done column.
Current truth to encode:

| module | lines | state |
| --- | ---: | --- |
| query-builder | 749 | **done** |
| update | 83 | **done** |
| key-compression | 249 | **done** |
| migration-schema | 1,187 | **done** |
| migration-storage | 255 | **done** |
| local-documents | 776 | **done** |
| leader-election | 132 | **done** |
| cleanup | 291 | **done** |
| state | 577 | **done** |
| pipeline | 497 | **done** |
| json-dump | 236 | **done** |
| backup | 413 | **done** |
| crdt | 577 | **done** |
| dev-mode | 3,088 | **done** (once §3 step 1 lands) |
| vector | 37 | remaining — 3 distance fns, only `dxdb-jsnum`, **no lane dependency, buildable now** |
| attachments-compression | 236 | remaining — storage wrapper; blocked on `DynRxStorageInstance` |
| encryption-crypto-js | 233 | remaining — storage wrapper; same blocker + cipher decision (§5) |
| validate-ajv | 70 | remaining — the collapse decision (§5) |
| validate-ajv-standalone | 126 | remaining — same crate, `precompiled` feature |
| validate-z-schema | 58 | remaining — recommend **omit** |
| validate-is-my-json-valid | 30 | remaining — recommend **omit** |

So the honest number is **14 of 21 done** (dev-mode makes 14), with 7 left: 1 free
(`vector`), 2 blocked on the dyn twin, 4 that are really 1 crate plus 2 omissions.

**Step 3 — then, in order:** `vector` (unblocked, tiny, pick it up any time), then
`attachments-compression` and `encryption` together once lane D ships
`DynRxStorageInstance`, then the `validate` collapse.

## 4. Not written down in the repo — traps, upstream behaviours, decisions

**The dyn twin is the single biggest unblocker for this lane.** `dxdb_storage::
RxStorageInstance` uses `impl Future` in eight methods and is therefore not
dyn-compatible. A storage *wrapper* returns `Box<dyn RxStorageInstance>` by definition, so
no wrapper plugin can exist until lane D adds the `DynRxStorageInstance` twin (PLAN §3.3,
`dynosaur`). That gates `key-compression`'s wrapper (already written and waiting),
`encryption`, and `attachments-compression` — two of the seven remaining. Say this
prominently whenever it is asked why 21 is not 21.

**Upstream behaviours that surprised me, all verified at `3b763ee17`:**

- **local-documents isolation is structural, not a flag.** Local docs live in a separate
  storage instance named `plugin-local-documents-<collection>`
  (`local-documents-helper.ts:80-82`; empty name for a database parent, `:22`).
  Replication binds to a collection's storage instance `changeStream()`
  (`replication-protocol/index.ts:200`, `upstream.ts:95`), so it cannot see that instance.
  `isLocal: true` serves a *different* consumer: it keeps events out of the change-event
  buffer (`change-event-buffer.ts:54`) and out of pipelines. Do not conflate them.
- **A fresh local document has `_rev: ''` on purpose** (`utils-document.ts:34-40`) so a
  storage that forgets to assign a revision throws. It *fails* `minLength: 1` before the
  write and passes after. A test must assert both stages.
- **leader-election has no liveness check.** A suspended tab never calls `die()`, so
  `is_leader` stays true while others elect a replacement: two leaders on resume. Upstream
  only mitigates with `responseTime * 4` on fallback rounds.
- **leader-election's listener is edge-triggered** — attached inside a round and removed
  after (`leader-election.js:145,168`). A buffering transport must clear at round start, or
  a stale `apply` from a dead peer makes every survivor stand down. This was a real bug in
  my first port.
- **In a synchronous harness rounds do not overlap**, so N sequential rounds elect N
  leaders. `Transport::drain_messages_within` exists for that; contention is modelled by
  injecting on the first *in-round* drain.
- **migration-schema's delete sentinel is `null`, not `false`**
  (`migration-helpers.ts:58`, `rx-migration-state.ts:600-604`). `false` has no meaning.
  `_deleted`/`_meta` are stripped before the first strategy and restored after, unguarded,
  so a document with no `_deleted` gains an explicit `null`.
- **`preMigrateDocument` / `postMigrateDocument` have no call site anywhere in `src/`.**
  Declared (`hooks.ts:72,76`) and registered by attachments, never fired. A faithful port
  registers and never fires. `createRxQuery` is in the typings but not in `HOOKS`.
- **key-compression: `keyCompression` is part of the schema hash**, so enabling it later
  changes identity and is **DB6**, not a migration (`rx-database.ts:481`). Names of length
  ≤ 3 never compress; the composite `primaryKey` is `{key, fields, separator}`, not an
  array.
- **migration-storage swallows new-storage write conflicts** — the check at `:185-188` is a
  commented-out TODO. The batch query's `_deleted: false` filter is load-bearing: progress
  comes from tombstones, never a growing `skip`.
- **cleanup's boundary is strict**: `lwt < now - retention`, so a tombstone aged exactly
  the retention survives, and `minimumDeletedTime: 0` does not purge one deleted at `now`.
  `saturating_sub` is wrong here (clamps at 0 where JS goes negative) — the safe form is
  `last >= first + 5`.
- **state is an append-only op log**, not a map: id = zero-padded counter, so ordering is
  *string* ordering and the 14-digit padding is the whole reason ids are strings. 100 `set`
  calls coalesce into one row. `cleanup_needed`'s upstream comment is off by one against
  its code; the code is the contract.
- **pipeline's re-entrancy detection is a JS stack-trace hack** (`flagged-functions.ts:3-27`,
  20 marker functions matched by prefix; its own comment calls it "the most hacky thing we
  do"). Unportable and unnecessary — this port passes an explicit `PipelineToken`. Strictly
  better, and it covers the two-pipelines-one-destination case the hack was retrofitted for.
- **crdt's `operations` is an array of arrays**, one inner array per write, and the merge
  pairs **by index**. A flat list collapses concurrent writes instead of uniting them. The
  hash covers **creators only**, not bodies. Upstream's `.updateCRDT()` describe block is
  preceded by a bare `return;` (`:329`), so those two cases do not run upstream either.
- **json-dump and backup handle attachments differently on purpose**: base64 inline for a
  dump (must be one JSON document) versus raw bytes for a backup (a folder tree). Not an
  inconsistency.
- **backup clears a document's folder before rewriting** (`index.ts:50`); without it a
  dropped attachment survives on disk and a restore attaches bytes the document no longer
  references.

**Dev-mode: my answer to the recorded divergence — I agree, with a sharper reason, and I
found evidence the boundary had never been audited.**

The recorded decision is "dev-mode is a deliberate divergence". I agree it should not be
ported wholesale, but the argument is not "it is a JS-ism". It is that **most of its schema
checks are already unconditional in `dxdb-schema`**, so re-implementing them would be two
implementations of one rule — which is how they drift. I enumerated 12 such codes (SC1's
neighbours, SC7, SC8, SC18–SC20, SC24, SC29, SC30, SC32, SC33, CI1, CI2) and made each a
test that asserts `dxdb-schema` rejects the schema, so if a rule ever moves, the test fails.

That audit turned up **three codes `dxdb-schema` declares in `SchemaErrorCode` but never
raises**:

- **`SC1`** (field-name pattern) — `Sc1` is in the catalogue and `as_str` renders it, but
  nothing constructs it. A schema with field `"Bad Name"` passes `create_rx_schema`.
- **`SC11`** (missing/invalid `version`) — `version` is *defaulted* to 0 (`lib.rs:332,416`)
  and neither a string nor a negative value is rejected.
- **`SC39`** — enforced, but **lazily**: raised by `Schema::primary_key_max_length()`
  (`lib.rs:834`), not at creation. Fine, but it means "creation validates SC39" is false.

`SC1` therefore lives in `dev-mode` as `Error::FieldNamePattern` (plus `SC23`,
`properties` as a field name), and there is a test,
`dxdb_schema_declares_sc1_but_cannot_raise_it`, that asserts `dxdb-schema` still accepts a
bad name — so the day it is fixed, the test fails and the rule moves. **This is worth
telling the leader: it is a defect in lane D's crate, found from this lane, not something
I should fix from here.**

The genuinely portable dev-mode rules that are *not* in `dxdb-schema`, and which are
therefore what the crate implements: `SC10` (_rev declared), `SC13` (primary in indexes,
including the compound form), `SC14` (primary `unique`), `SC15` (primary encrypted), `SC16`
(primary not a string), `SC17` (field shadows an RxDocument member), `SC34`/`SC35` (indexed
field bounds), `DB2` (`_`-prefixed collection name), `COL12` (strategy count).

Also not ported from dev-mode, recorded: `deepFreezeWhenDevMode` (a Rust value cannot be
frozen after the fact; `Document::ensure_revision` is the equivalent guarantee, and the
`Overwritables` flag is deliberately left false rather than promised), `dev-mode-tracking.ts`
(iframe beacon), the 1,433-line error-message table (the *mechanism* is kept as
`expanded_error_message`, 12 codes of rule-statements rather than upstream's prose), and
`check-orm.ts` (no object-literal methods to check).

**Test-provenance convention, applied throughout:** `upstream_<line>_<slug>` for a case
ported from an upstream test file; `mingo_*` for behaviour pinned from a dependency's
source; `source_<line or code>_<slug>` where upstream has **no** test file for the plugin
or the case. Three plugins have no upstream test file at all — **json-dump, backup-less
parts, and dev-mode** — and their crates say so explicitly and **claim no ported count**.
If a plugin has no ported tests, say so; do not label inferred tests as ported.

## 5. Blocked / questions

1. **`validate-*`: my recommendation is one crate plus two omissions, and I would argue
   against porting any backend mechanically.** The four plugins differ only in *which JS
   library compiles the JSON Schema*: `ajv` (+`ajv-formats`), ajv precompiled, `is-my-json-
   valid`, `z-schema`. dxdb has one engine (`jsonschema` 0.60.1, chosen in PLAN §9) and
   already uses it in `dxdb-schema`. So:
   - `validate` — one crate: the `wrappedValidateStorageFactory` shape from
     `plugin-helpers.ts:55`, which strips the read-only/meta properties before validation
     and reports `RxStorageWriteError` 422s. This has behaviour worth having independent
     of the engine (it is how a storage reports a validation failure).
   - `validate` with a `precompiled` feature — the analogue of `validate-ajv-standalone`:
     compile the schema once at build time instead of per instance. Real behaviour (bundle
     size, start cost).
   - **Omit `validate-z-schema` and `validate-is-my-json-valid`.** Their only difference is
     the library; there is no second engine here, so porting them would produce two crates
     that call the same function. If the leader wants them for parity of *name*, say so and
     they become two one-line re-exports over `validate`.
   **Question: confirm the collapse, and confirm the two omissions.**

2. **`encryption` cipher divergence** (still open from two tranches ago). Upstream is
   crypto-js AES with `EVP_BytesToKey`/MD5/1 iteration. PLAN §9 picks chacha20poly1305 +
   argon2, so upstream ciphertext would be unreadable by dxdb. I intend to name the crate
   `dxdb-plugin-encryption`, not `-crypto-js`, so the name does not promise interop we do
   not have. **Confirm the divergence is acceptable**, or ask for an `upstream-compat`
   feature (which needs a written justification in PLAN §9 because it deliberately uses a
   weak KDF).

3. **`attachments` path ownership**, carried but now settled in practice — lane E built
   `crates/dxdb-attachments`. `attachments-compression` (the storage wrapper) is mine and
   waits on the dyn twin; no action needed unless the leader disagrees.

4. **`DynRxStorageInstance`** — not a question, a dependency: ask lane D for it whenever it
   is cheap to ask. It converts two of the seven remaining modules from blocked to trivial.

## 6. Standing verification recipe

```
( for t in check clippy test wasm; do echo "=== $t ==="; make $t; echo "=== $t EXIT=$? ==="; done ) >/tmp/<name>-gates.log 2>&1
grep -E "=== .* EXIT=" /tmp/<name>-gates.log
```

A `Killed` with no compiler output is the box, not the code (seen twice this tranche).
`rsync exited with 255` / "No route to host" is the route, not the code. Report either;
never work around them.

## 7. Drafted commit message for dev-mode

```
plugins: dev-mode, scoped to the rules dxdb-schema does not enforce

dxdb-plugin-dev-mode: src/plugins/dev-mode/ (3088 lines). 29 tests (6 lib, 23 integration).

I agree with the recorded divergence, but for a sharper reason than "it is a JS-ism":
MOST of dev-mode's schema checks are already UNCONDITIONAL in dxdb-schema, because
upstream gates them behind dev mode and dxdb validates always. Re-implementing them would
be two implementations of one rule, which is how they drift. Twelve codes are therefore
deliberately NOT ported (SC7, SC8, SC18-20, SC24, SC29, SC30, SC32, SC33, CI1, CI2 and
their neighbours), and each has a test asserting dxdb-schema rejects the schema on its own
-- so if a rule ever moves, the test fails and this crate adopts it.

That audit found three codes dxdb-schema DECLARES BUT NEVER RAISES:
- SC1 (field-name pattern): SchemaErrorCode::Sc1 exists and as_str renders it, but nothing
  constructs it. A schema with a field named "Bad Name" passes create_rx_schema.
- SC11 (version): version is defaulted to 0 (lib.rs:332,416) and neither a string nor a
  negative value is rejected.
- SC39: enforced, but LAZILY by Schema::primary_key_max_length (lib.rs:834), not at
  creation.
SC1 therefore lives here (with SC23, `properties` as a field name), and there is a test --
dxdb_schema_declares_sc1_but_cannot_raise_it -- that asserts dxdb-schema still accepts a
bad name, so the day it is fixed the test fails and the rule moves. Reported as a defect
in lane D's crate found from this lane, not fixed from here.

Ported, because genuinely unenforced: SC10 (_rev declared), SC13 (primary in indexes,
including the compound form), SC14 (primary unique), SC15 (primary encrypted), SC16
(primary not a string), SC17 (field shadows an RxDocument member), SC34/SC35 (indexed
field bounds), DB2 (underscore collection name), COL12 (strategy count).

Not ported, recorded with reasons: deepFreezeWhenDevMode (a Rust value cannot be frozen
after the fact; Document::ensure_revision is the guarantee, and the Overwritables flag is
left false rather than promised), dev-mode-tracking.ts (iframe beacon), the 1433-line
error-message table (the MECHANISM is kept as expanded_error_message with 12 codes of
rule-statements rather than new prose), check-orm.ts (no object-literal methods to check).

Upstream has no test/unit/dev-mode.test.ts: the plugin is installed globally in
test/helper/parallel-setup.ts:27, so its checks run as a precondition of every other suite
and a regression would surface elsewhere or not at all. Tests are therefore labelled by
source line (source_sc13_...) and this crate claims no ported-test count.
```
