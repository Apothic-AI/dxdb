# M6 plugin port: scope, classification, and the Hooks API design

Lane H. Upstream `~/Code/rxdb` at `3b763ee17` (RxDB 17.6.0).

This document answers two questions that M6's scope was previously dishonest
about:

1. Which of the 52 plugin directories are actually this lane's, and what happens
   to the rest.
2. What the hooks/plugin-injection model becomes in Rust, exactly, including the
   24 hook names and their ordering.

Everything below was counted from the pinned checkout, not estimated. The
counting command is `xtask/status.py`'s `plugin_lines` for the per-module totals,
and `find src/plugins -name '*.ts' | xargs cat | wc -l` for the tree total.

## 1. The headline

| | Count | Lines |
| --- | --- | --- |
| Upstream plugin directories | 52 | 37,698 |
| **Portable plugin behaviour this lane builds** | **21** | **9,900** |
| Storage implementations (other lanes, or omitted with reason) | 9 | 8,178 |
| Replication adapters (lane E's protocol; adapters sequenced last) | 11 | 11,038 |
| Attachments + replication core (lane E by path ownership) | 2 | 1,419 |
| JS-ecosystem bindings (omitted with reason) | 7 | 1,586 |
| Not plugins (`utils`, `test-utils`) | 2 | 5,579 |

M6 is **26% of the plugin tree**. The brief's "52 plugin directories, 37,698
lines" is the size of the directory, not the size of the milestone. Reporting a
completion percentage against 37,698 would be a number that can never be earned.

## 2. Classification of all 52 modules

Class key:

- **P** portable behaviour — this lane, `crates/plugins/<module>/`.
- **S** storage plugin — belongs to a storage crate (other lane) or omitted.
- **R** replication adapter — `dxdb-replication` (lane E) plus adapter crates.
- **E** owned by lane E by path (`docs/WORKTREES.md`).
- **J** JS-ecosystem binding — omitted, justified in
  `crates/plugins/README.md`.
- **N** not a plugin — re-homed by concern.

| Module | Lines | Class | dxdb home |
| --- | ---: | --- | --- |
| `attachments` | 397 | E | `crates/dxdb-attachments/` (lane E) |
| `attachments-compression` | 236 | P | `crates/plugins/attachments-compression/` |
| `backup` | 413 | P | `crates/plugins/backup/` |
| `cleanup` | 291 | P | `crates/plugins/cleanup/` |
| `crdt` | 577 | P | `crates/plugins/crdt/` |
| `dev-mode` | 3,088 | P | `crates/plugins/dev-mode/` |
| `electron` | 125 | J | omitted — Electron IPC transport |
| `encryption-crypto-js` | 233 | P | `crates/plugins/encryption/` (see note) |
| `flutter` | 65 | J | omitted — Flutter platform channel |
| `json-dump` | 236 | P | `crates/plugins/json-dump/` |
| `key-compression` | 249 | P | `crates/plugins/key-compression/` |
| `leader-election` | 132 | P | `crates/plugins/leader-election/` |
| `local-documents` | 776 | P | `crates/plugins/local-documents/` |
| `migration-schema` | 1,187 | P | `crates/plugins/migration-schema/` |
| `migration-storage` | 255 | P | `crates/plugins/migration-storage/` |
| `pipeline` | 497 | P | `crates/plugins/pipeline/` |
| `query-builder` | 749 | P | `crates/plugins/query-builder/` |
| `react` | 469 | J | omitted — React hooks |
| `reactivity-angular` | 32 | J | omitted — Angular signals |
| `reactivity-preact-signals` | 90 | J | omitted — Preact signals |
| `reactivity-vue` | 43 | J | omitted — Vue refs |
| `replication` | 1,022 | E | `crates/dxdb-replication/` (lane E) |
| `replication-appwrite` | 338 | R | adapter crate, after `dxdb-replication` |
| `replication-couchdb` | 475 | R | adapter crate, after `dxdb-replication` |
| `replication-firestore` | 538 | R | adapter crate |
| `replication-google-drive` | 2,421 | R | adapter crate (OAuth + REST client) |
| `replication-graphql` | 901 | R | adapter crate |
| `replication-microsoft-onedrive` | 2,053 | R | adapter crate (OAuth + REST client) |
| `replication-mongodb` | 578 | R | adapter crate |
| `replication-nats` | 347 | R | adapter crate |
| `replication-supabase` | 435 | R | adapter crate |
| `replication-webrtc` | 2,314 | R | adapter crate (needs a peer stack) |
| `replication-websocket` | 638 | R | adapter crate |
| `state` | 577 | P | `crates/plugins/state/` |
| `storage-denokv` | 696 | S | omitted — Deno host API (see README) |
| `storage-dexie` | 1,041 | S | omitted — Dexie is a JS IndexedDB wrapper |
| `storage-foundationdb` | 736 | S | omitted — out-of-tree trait impl |
| `storage-localstorage` | 794 | S | `crates/storages/dxdb-localstorage` (lane F) |
| `storage-memory` | 1,387 | S | `crates/storages/dxdb-memory` (lane D) |
| `storage-mongodb` | 699 | S | omitted — out-of-tree trait impl |
| `storage-remote` | 827 | S | `crates/storages/dxdb-remote` (lane F) |
| `storage-remote-websocket` | 252 | S | `crates/storages/dxdb-remote` feature (lane F) |
| `storage-sqlite` | 1,746 | S | `crates/storages/dxdb-sqlite` (lane G) |
| `test-utils` | 3,762 | N | `crates/dxdb-testkit/` (lane D) |
| `update` | 83 | P | `crates/plugins/update/` |
| `utils` | 1,817 | N | split by concern (README lists the mapping) |
| `validate-ajv` | 70 | P | `crates/plugins/validate-ajv/` |
| `validate-ajv-standalone` | 126 | P | `crates/plugins/validate-standalone/` |
| `validate-is-my-json-valid` | 30 | P | `crates/plugins/validate-is-my-json-valid/` |
| `validate-z-schema` | 58 | P | `crates/plugins/validate-z-schema/` |
| `vector` | 37 | P | `crates/plugins/vector/` |
| `webmcp` | 762 | J | omitted — `navigator.modelContext` |

Class totals: **P 21 (9,900)** · **S 9 (8,178)** · **R 11 (11,038)** ·
**E 2 (1,419)** · **J 7 (1,586)** · **N 2 (5,579)**. The classes are a verified
partition: 52 distinct directories, each assigned exactly once, summing to the
same 37,700 lines `wc -l` reports. The counting scripts' 37,698 is 2 lines lower
because `replication-graphql/helper.ts` and `webmcp/nosql-query-schema.ts` have
no trailing newline and `wc -l` counts newlines.

### Notes on the four judgement calls

**`encryption-crypto-js` is portable, but not as a crypto-js port.** Upstream
delegates to crypto-js's `AES.encrypt(value, password)`
(`src/plugins/encryption-crypto-js/index.ts:35`), which is OpenSSL `EVP_BytesToKey`
(MD5, one iteration) feeding AES-256-CBC. That is a password-based KDF with no
work factor. PLAN.md section 9 already chooses the clean-room envelope
(chacha20poly1305 + argon2) and lists `aes-gcm` only as a WebCrypto-compatible
option. So the portable behaviour is the *shape*: a storage wrapper that strips
`encrypted` paths out of the schema, encrypts those paths on write and decrypts
on read (`:93-190`). The cipher is a dxdb choice, recorded as an intentional
divergence: ciphertext written by upstream's plugin will not decrypt here, and
that is the point. Name it `dxdb-plugin-encryption` rather than
`-crypto-js`, because the module name would otherwise promise an interop it
deliberately does not have.

**`attachments-compression` is portable and is 236 lines of `CompressionStream`.**
It is a storage-wrapper plugin, not a Blob-serialisation plugin, so it depends on
`dxdb-storage` traits and a compressor that works on both native and wasm
(`flate2` on native, `CompressionStream` via `web-sys` on wasm, behind a
feature). It is a distinct crate from lane E's `dxdb-attachments`: lane E owns
the attachment *data model* (digest, `_attachments`, size accounting), this lane
owns the *storage wrapper* that compresses attachment bodies on the way in.
That split must be agreed with lane E before either side writes code.

**The four validator plugins are one behaviour with four backends.** Upstream
ships four because it wants to let users pick a JSON-Schema engine by install
size. In Rust there is one engine (`jsonschema` 0.60.1, already chosen in PLAN.md
9) plus a compile-time precompiled variant. Port the shared part once
(`wrappedValidateStorageFactory`, `src/plugin-helpers.ts:55`) as
`dxdb-plugin-validate`, then:
`validate-ajv` → `validate` (runtime compilation),
`validate-ajv-standalone` → `validate` with a `precompiled` feature,
`validate-is-my-json-valid` and `validate-z-schema` → omitted, because the
difference is which JS library does the validating and there is only one library
here. Do **not** ship four crates that differ by one type alias; that is
structure, not behaviour, and the brief says port behaviour.

**`vector`'s `helper.ts` is empty and `types.ts` is one line.** 37 lines total,
of which 34 are three distance functions (`euclideanDistance`,
`manhattanDistance`, `cosineSimilarity`). It is portable in an afternoon and it
depends on nothing but `dxdb-jsnum` for number semantics.

## 3. The Hooks API design

PLAN.md 4.3 requires "the same hook names" and "explicit ordering". This section
is the concrete contract. **This is a design, not an implementation**: lane D owns
`dxdb-core`, where the registry lives, so the code lands there once that lane
publishes. This lane's contribution is the contract and the evidence that the
contract is complete.

### 3.1 The facts the design is built on

- `src/hooks.ts:5-103` declares **24** hook arrays. Counted and names verified.
- `src/plugin.ts:35` mutates five prototypes: `RxSchema`, `RxDocument`,
  `RxQuery`, `RxCollection`, `RxDatabase`.
- `src/plugin.ts:103-114` is the ordering rule: `hooks[name].after` is **pushed**
  (appended), `hooks[name].before` is **unshifted** (prepended). So `before`
  handlers run newest-first and `after` handlers run oldest-first. This
  asymmetry is observable and is the source of upstream's own warning
  (`src/types/rx-plugin.d.ts:56-59`): "the encryption plugin must run encryption
  before the key-compression changes the fieldnames."
- `src/hooks.ts:105` (`runPluginHooks`) is synchronous and ignores return values.
  `src/hooks.ts:117` (`runAsyncPluginHooks`) awaits handlers **sequentially**, with
  the deliberate comment "We do intentionally not run the hooks in parallel
  because that makes stuff unpredictable" (`:112-116`).
- The sync variant reads a handler's *mutations of the input object*. That is how
  `preCreateRxDatabase` and `prePrepareQuery` work: they receive a mutable object
  and mutate it. Rust cannot reproduce "pass a `&mut` and hope", so every
  mutating hook becomes an explicit return value (see 3.4).
- Two hooks are declared but never invoked anywhere in `src/`:
  `preMigrateDocument` and `postMigrateDocument`. Verified: the only occurrences
  are the declaration (`hooks.ts:72,76`), the plugin's registration
  (`src/plugins/attachments/index.ts:306`), and the typings
  (`types/rx-plugin.d.ts:152-153`). `migrateDocumentData`
  (`migration-schema/migration-helpers.ts:60`) never runs them. `createRxQuery`
  is in the typings but not in `HOOKS`, so registering it upstream would throw on
  `HOOKS[name].push`. Also verified.

### 3.2 The 24 hook names, and the call site for each

Names are exact and are the public contract. `→` gives the upstream call site and
sync/async. The Rust payload type is named in 3.3.

| # | Hook | Site | Mode |
| ---: | --- | --- | --- |
| 1 | `preAddRxPlugin` | `src/plugin.ts:54` | sync |
| 2 | `preCreateRxDatabase` | `src/rx-database.ts:839` | sync, mutating |
| 3 | `createRxDatabase` | `src/rx-database.ts:918` | async |
| 4 | `preCreateRxCollection` | `src/rx-database.ts:402` | sync, mutating |
| 5 | `createRxCollection` | `src/rx-collection.ts:1321` | sync |
| 6 | `createRxState` | `src/plugins/state/rx-state.ts:397` | sync |
| 7 | `postCloseRxCollection` | `src/rx-collection.ts:1141` | async |
| 8 | `postRemoveRxCollection` | `src/rx-collection-helper.ts:225` | async |
| 9 | `preCreateRxSchema` | `src/rx-schema.ts:270` | sync, mutating |
| 10 | `createRxSchema` | `src/rx-schema.ts:278` | sync |
| 11 | `prePrepareRxQuery` | `src/rx-collection.ts:842, 866` | sync, mutating |
| 12 | `preCreateRxQuery` | `src/rx-query.ts:659` | sync, mutating |
| 13 | `prePrepareQuery` | `src/rx-query.ts:497` | sync, mutating |
| 14 | `createRxDocument` | `src/rx-document.ts:480` | sync |
| 15 | `postCreateRxDocument` | `src/rx-document-prototype-merge.ts:116` | sync (typing says sync, `:55`) |
| 16 | `preCreateRxStorageInstance` | `src/rx-collection.ts:1282`, `src/rx-database.ts:429` | sync, mutating |
| 17 | `preStorageWrite` | `src/rx-storage-helper.ts:702` | sync, mutating |
| 18 | `preMigrateDocument` | **no call site** | — |
| 19 | `postMigrateDocument` | **no call site** | — |
| 20 | `preCloseRxDatabase` | `src/rx-database.ts:619` | async |
| 21 | `postRemoveRxDatabase` | `src/rx-database.ts:984` | async |
| 22 | `postCleanup` | `src/plugins/cleanup/cleanup.ts:117` | async |
| 23 | `preReplicationMasterWrite` | `src/plugins/replication/index.ts:356` | async |
| 24 | `preReplicationMasterWriteDocumentsHandle` | `src/plugins/replication/index.ts:434` | async |

Registering, in terms of the 24: sixteen hooks are registered by at least one of
the 52 plugins. Those sixteen are `preCreateRxSchema`, `preCreateRxDatabase`,
`preCreateRxCollection`, `preCreateRxQuery`, `prePrepareRxQuery`,
`prePrepareQuery`, `preStorageWrite`, `createRxDatabase`, `createRxCollection`,
`createRxState`, `createRxDocument`, `postCloseRxCollection`,
`postRemoveRxCollection`, `postRemoveRxDatabase`, `preCloseRxDatabase` and
`preMigrateDocument`. The other eight — `createRxSchema`, `postCreateRxDocument`,
`preAddRxPlugin`, `preCreateRxStorageInstance`, `postMigrateDocument`,
`postCleanup`, `preReplicationMasterWrite` and
`preReplicationMasterWriteDocumentsHandle` — exist for third-party plugins and are
nonetheless invoked by core or by another plugin. They must all be present,
because a plugin written against upstream registers against all 24.

### 3.3 Rust shapes

One module, `dxdb_core::hooks`. The registry is owned by the database, not
global: upstream's `ADDED_PLUGINS` is module-global
(`src/plugin.ts:43-44`), which is why adding a plugin twice is a no-op and why
two databases in one process share a hook list. In Rust the global form is
available but it makes tests order-dependent, so the design has **both**: a
process-wide `Registry` for parity, and a per-database `Hooks` handle that is what
the database actually consults. A test that wants isolation constructs its own.

```rust
/// The 24 upstream hook names, in declaration order (`src/hooks.ts:5`).
///
/// Not a `String` and not an extension point: the set is closed because the call
/// sites are closed. A plugin cannot add a hook, which matches upstream (the
/// `HOOKS` object is not extensible in practice — `runPluginHooks` takes
/// `keyof typeof HOOKS`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Hook {
    PreAddRxPlugin,
    PreCreateRxDatabase,
    CreateRxDatabase,
    PreCreateRxCollection,
    CreateRxCollection,
    CreateRxState,
    PostCloseRxCollection,
    PostRemoveRxCollection,
    PreCreateRxSchema,
    CreateRxSchema,
    PrePrepareRxQuery,
    PreCreateRxQuery,
    PrePrepareQuery,
    CreateRxDocument,
    PostCreateRxDocument,
    PreCreateRxStorageInstance,
    PreStorageWrite,
    PreMigrateDocument,
    PostMigrateDocument,
    PreCloseRxDatabase,
    PostRemoveRxDatabase,
    PostCleanup,
    PreReplicationMasterWrite,
    PreReplicationMasterWriteDocumentsHandle,
}
```

Payloads are one enum rather than 24 generic parameters, because a single
`Registry` has to hold all of them and a plugin registers several at once:

```rust
pub enum HookPayload<'a> {
    PreAddRxPlugin(&'a PluginDescriptor),
    PreCreateRxDatabase(DatabaseCreatorParams),
    CreateRxDatabase { database: &'a DbHandle, creator: &'a DatabaseCreator },
    // ... one variant per Hook variant
    PreStorageWrite(StorageWriteRows<'a>),
    Unsupported(&'a Hook),  // see 3.5: hooks with no call site yet
}
```

Two payloads can now name real types, because lane D published
`dxdb-storage` (`d136a7a`) while this lane was writing:

- `preCreateRxStorageInstance` carries `dxdb_storage::InstanceCreationParams`
  (13 fields, `traits.rs:133`).
- `preStorageWrite` carries
  `{ storage_instance: &dyn RxStorageInstance, rows: &mut [BulkWriteRow] }`
  (`write.rs:43`).

**One field is missing and it matters.** Upstream's payload for
`preCreateRxStorageInstance` includes `password`
(`src/rx-collection.ts:1268`, `src/rx-database.ts:429`), and
`dxdb_storage::InstanceCreationParams` deliberately does not, with the reason
recorded in its own doc comment: encryption is a wrapper, so a plain storage never
sees a password and one that could see it would be a place to leak it. So the hook
payload cannot simply be `InstanceCreationParams`; it has to be

```rust
pub struct PreCreateRxStorageInstancePayload {
    pub params: dxdb_storage::InstanceCreationParams,
    /// Upstream passes this. A wrapping storage needs it; `params` must not gain
    /// it, because a plain storage must never be able to read it.
    pub password: Option<SecretString>,
}
```

This is the one place where the storage layer's improved security contract and
the hook contract disagree, and the resolution is a separate payload type rather
than widening `InstanceCreationParams`. Recorded here because it is invisible
from either side alone.

**The mutating hooks return, they do not take `&mut`.** Upstream passes a mutable
object and reads it back after the run. In Rust that becomes:

```rust
/// `preCreateRxDatabase` / `preCreateRxCollection` / `preCreateRxSchema` /
/// `preCreateRxQuery` / `prePrepareRxQuery` / `prePrepareQuery` /
/// `preCreateRxStorageInstance` / `preStorageWrite`.
///
/// Same rule as upstream, now enforced by the compiler: the value passed in is
/// what the next handler sees, the value returned is what the caller proceeds
/// with, and a handler that wants to change something must say so.
pub trait MutatingHook<P>: Send + Sync {
    fn run(&self, input: P) -> Result<P, HookError>;
}
```

A synchronous hook that needs no mutation (the `create*` and `post*` families)
returns `Result<(), HookError>`. The async families are declared on the same
trait via `dynosaur` (PLAN.md 3.3) so both a `Box<dyn Hook>` and a generic
`impl Hook` call site work without `async_trait`.

### 3.4 Ordering, exactly

Upstream's semantics, re-stated because they are not obvious:

```
for each plugin, in add order:
    if plugin.hooks[name].after:  HOOKS[name].push(after)
    if plugin.hooks[name].before: HOOKS[name].unshift(before)
```

Re-expression in Rust:

- Every registration carries an `Order`:
  ```rust
  pub enum Order { First, Before, After, Last }
  ```
- `Before` and `First` prepend; `After` and `Last` append. `First` and `Last` are
  additions, not translations: upstream can only prepend/append, so a plugin that
  needs "before everything, including other before-handlers" has no way to say
  it. `First`/`Last` cost one enum variant and remove a class of ordering bugs.
- Registration sequence is a `u32` monotonically increasing within a `Registry`,
  so ordering is **total and deterministic**: `(tier, registration_seq)` where
  tier is `First=0, Before=1, Normal=2, After=3, Last=4`. No two handlers are
  ever ambiguous. This is what "explicit ordering" in PLAN.md 4.3 has to mean,
  otherwise the same plugin set can produce different results on different runs,
  which is the bug upstream's push/unshift asymmetry causes.
- Handlers are invoked **sequentially, never concurrently**, for the async family
  too. This is upstream's deliberate choice (`src/hooks.ts:112-116`) and it is
  the only defensible one when handlers mutate shared state.
- A handler error aborts the hook run and propagates. Upstream has no error
  handling here at all: `forEach(fun => fun(obj))` lets a throw escape, and
  nothing distinguishes "handler failed" from "hook absent". Rust returns
  `Result`, and the db operation fails with the handler's error attached.
- **Duplicate registration is a no-op on identity, an error on name.** Upstream
  (`src/plugin.ts:56-71`) dedupes by object identity and throws `PL3` on a
  duplicate *name*. Keep both rules.

### 3.5 The three gaps between upstream's model and what a Rust port can honour

These are the honest costs of "re-expression", and they need a decision rather
than a silence.

1. **`preMigrateDocument` / `postMigrateDocument` have no call site upstream.** A
   faithful port registers them and never fires them, exactly as upstream. That is
   the correct parity behaviour and it must be written down, because a reviewer
   will otherwise read "24 hooks implemented" as "24 hooks working". Checked
   against the pinned source: no invocation exists.

2. **`createRxQuery` is in the typings but not in `HOOKS`.** It is safe to omit.
   Drop it; do not add a 25th hook for a name that would throw upstream.

3. **Prototype mutation has no equivalent.** Upstream's five prototypes
   (`RxSchema`, `RxDocument`, `RxQuery`, `RxCollection`, `RxDatabase`) are
   monkey-patched by 12 of the 52 plugins — `attachments`, `backup`, `cleanup`,
   `crdt`, `json-dump`, `local-documents`, `migration-schema`, `pipeline`,
   `query-builder`, `state`, `update`, `webmcp` — adding methods like
   `doc.update()`, `doc.incrementalUpdate()`, `query.update()`,
   `db.dump()`, `collection.addPipeline()`, `db.addState()`. `RxSchema` is
   patchable upstream but no plugin patches it, so the schema trait is a design
   obligation with no M6 caller; it can be the last of the five to land. The plan's answer
   (PLAN.md 4.3) is extension traits plus a type-indexed map on the handle. Two
   consequences worth stating now:

   - The map must be keyed by `TypeId` on a boxed `dyn Any` with a
     `downcast_ref` accessor, and it must be `Send + Sync` so a database can cross
     a thread. A plugin that stores a non-`Send` value is a compile error instead
     of a runtime surprise.
   - An extension method is only callable when its trait is in scope. Upstream's
     `doc.update()` works after `addRxPlugin(RxDBUpdatePlugin)` whether or not you
     imported anything. Rust users will have to `use dxdb_plugin_update::Update`.
     That is a real ergonomic regression and it is the price of the type system;
     it belongs in each crate's docs, not in a footnote here.

## 4. This lane's build plan

Order from the lane brief, filtered by the classification above (storage plugins
and JS bindings removed, `attachments` deferred to lane E). Each step lands with
its gate green before the next, and each names the upstream test file it ports.

| # | Crate | Upstream lines | Test corpus to port | Blocked on |
| ---: | --- | ---: | --- | --- |
| 0 | *(skeletons: `query-builder`, `update`)* | — | — | nothing — **this commit** |
| 1 | `query-builder` | 749 | `test/unit/query-builder.test.ts` (229 lines, 11 cases) | `dxdb-query` (B) for `MangoQuery`, `dxdb-core` (D) for `RxQuery` |
| 2 | `update` | 83 | `test/unit/rx-document.test.ts`'s update cases | `dxdb-core` (D) document write path |
| 3 | `key-compression` | 249 | `test/unit/key-compression.test.ts` | `dxdb-storage` traits (D) |
| 4 | `migration-schema` | 1,187 | `test/unit/migration.test.ts` | D schema + document |
| 5 | `migration-storage` | 255 | `test/unit/migration-storage.test.ts` | D + a concrete storage |
| 6 | `local-documents` | 776 | `test/unit/local-documents.test.ts` | D |
| 7 | `leader-election` | 132 | `test/unit/leader-election.test.ts` | D (multi-instance) |
| 8 | `cleanup` | 291 | `test/unit/cleanup.test.ts` | D |
| 9 | `json-dump` | 236 | `test/unit/json-dump.test.ts` | D |
| 10 | `backup` | 413 | `test/unit/backup.test.ts` | D |
| 11 | `crdt` | 577 | `test/unit/crdt.test.ts` | D |
| 12 | `state` | 577 | `test/unit/rx-state.test.ts` | D |
| 13 | `pipeline` | 497 | `test/unit/pipeline.test.ts` | D |
| 14 | `dev-mode` | 3,088 | `test/unit/dev-mode.test.ts`, `hooks.test.ts` | D + most of the above |
| 15 | `validate` (+ standalone feature) | 70 + 126 | `test/unit/validate.test.ts` | D storage wrapper |
| 16 | `encryption`, `attachments-compression` | 233 + 236 | round-trip suites in `rx-storage-implementations` | D + lane E's attachments |
| 17 | `vector` | 37 | none upstream (three functions) | nothing |
| — | replication adapters | 11,038 | — | `dxdb-replication` (E) stable |
| — | `hooks.test.ts`, `plugin.test.ts` | 825 | `crates/dxdb-core` tests | lane D's implementation of §3 |

Steps 1 and 2 are gated on lane D publishing `dxdb-core` and (for anything below
step 3) `dxdb-storage`; step 17 is not gated on anything and is available now if
this lane needs filler while waiting.

`hooks.test.ts` (616 lines) and `plugin.test.ts` (209 lines) port to
`dxdb-core`, not here: they test the registry, and the registry is the database's.

### 4.1 Tranche 2: what actually landed

Steps 1 and 2 are done, against `dxdb-storage`'s published surface (lane D,
`d136a7a`). The numbers are measured, not estimated.

| Crate | Upstream lines ported | Tests | Gate |
| --- | ---: | ---: | --- |
| `dxdb-plugin-query-builder` | 749 (631 builder + 81 wrapper + 37 merge helpers) | 29: 11 name an upstream line, 12 cover behaviour upstream tests only incidentally, 4 cross-check the oracle, 2 internal | green |
| `dxdb-plugin-update` | 82, plus the 14 mingo modifier bodies it depended on | 40: 5 name an upstream line (4 of them modifier cases, 1 the document-layer boundary), 25 name the mingo file whose behaviour they assert, 8 cover the briefed invariants, 2 internal | green |

**Upstream test provenance.** `test/unit/query-builder.test.ts` is 229 lines and
11 `it(...)` cases, and **all 11 are ported**, each named `upstream_<line>_<slug>`:
`:24`, `:55`, `:79`, `:89`, `:113`, `:129`, `:142`, `:155`, `:172`, `:221`, and
`:189`. The last one needs a live collection, so it is ported at builder level
covering both merge directions rather than as an end-to-end query.

`test/unit/rx-document.test.ts` supplies six: the four modifier cases at `:346`,
`:362`, `:374` and `:388`, plus `:398`/`:433` as an explicit boundary test
asserting that the conflict and final-field guards are *not* this crate's job.

The remaining 25 update tests assert mingo behaviour that upstream never tests —
they name the mingo file whose behaviour they pin, because upstream has no test for
it and a port that got it wrong would pass upstream's own suite. Eight more cover
the invariants the brief calls out (unknown modifier must error, accepted set,
input not mutated, operator order, array-filter refusal, prototype-path refusal).

Total for the tranche: **69 tests, all passing** (29 + 40).

**Gate status, run in sequence as `for t in check clippy test wasm; do make $t; done`
after merging master:**

| Gate | Exit |
| --- | --- |
| `make check` | 0 |
| `make clippy` | 0 |
| `make test` | 0 |
| `make wasm` | 0 |
| `make fmt-check` | **2 — not this lane** |

`make fmt-check` is red on master independently of `crates/plugins/`: every diff is
in `crates/dxdb-storage/` (nine files, from lane D). `rustfmt --check` on the two
plugin crates and their test files exits 0. Reported rather than fixed, because
`crates/dxdb-storage/` is lane D's path; reformatting it from here would conflict
with their next commit.

**Not ported, and why it is not a gap to paper over.** The document layer that
`rx-document.test.ts` exercises around those four cases — revision bump, conflict
on a stale `_rev` (`:398`), the `final` field guard (`:433`), the query-level
effect of `_deleted` — belongs to lane D's write path. The modifier engine accepts
and applies `_rev` and final fields without complaint, because refusing them is
the caller's job; `upstream_398_and_433_are_document_layer_not_modifier_layer`
asserts that boundary so it stays machine-visible.

**Brief errata, verified against the pin.** The tranche-2 brief asked for
`query-builder`'s `.remove()` chain and for `update`'s `incrementalPatch`,
`incrementalModify` and `atomicUpdate`. None of those exist where the brief places
them:

- `remove()` is `RxQuery.remove` in core (`src/rx-query.ts:525`), not a
  query-builder method. The builder's authoritative method list is `where`,
  `equals`, `eq`, `or`, `nor`, `and`, `mod`, `exists`, `elemMatch`, `sort`, plus
  `limit`/`skip`/`maxScan`/`batchSize`/`comment` and the nine generated
  comparisons (`nosql-query-builder.ts:67-321`, `:459`, `:473`). All are ported
  except `maxScan`/`batchSize`/`comment`, which upstream accepts and then discards
  at projection time and this crate reproduces deliberately.
- `incrementalModify` is core `RxDocument`; `incrementalPatch` is defined by
  `local-documents` (`rx-local-document.ts:201`) and patched by `crdt`
  (`crdt/index.ts:393`); `atomicUpdate` does not exist anywhere in the repository
  at `3b763ee17`. The three entry points the update plugin actually defines are
  `update`, `incrementalUpdate` and `RxQueryUpdate` (`update/index.ts:15,25,34`).

**The modifier dependency decision, recorded in full in the crate docs.** Port the
operator set directly rather than depend on anything. Nothing off the shelf
implements mingo's quirks: `$mul` on a missing field yields `0` not `factor`,
`$inc` skips a non-numeric *target* but errors on a non-numeric *argument*,
`$unset` writes `null` into arrays, `$push`'s `$slice` keeps the first N when
positive and the last N when negative. Fourteen operators are accepted, matching
mingo 7.2.5 exactly; everything else, including the `$setOnInsert` that upstream's
*typings* advertise but mingo never shipped, is an error rather than a no-op.

## 5. What the workspace line had to be, and what master already resolved

**Resolution status.** The leader merged this lane's first two commits into
`master` and, in `d7f632b`, independently landed the same `crates/dxdb-*` form
(found from lane G after `make gate-storage` turned up dead). The current line is

```toml
members = ["crates/dxdb-*", "crates/storages/*", "crates/plugins/*", "xtask"]
```

with no `exclude`, and `cargo metadata` now lists `dxdb-memory` as a member. So
items 1 and 2 below are historical: they record what the constraint was and why
the brief's literal one-line instruction was unsatisfiable, which is worth
keeping because the brief still says it.

1. **The workspace glob was not a one-line change in the sense the brief meant.**
   Measured with `cargo metadata`, not assumed: the moment `crates/plugins/`
   exists, `crates/*` matches it, and cargo fails with "failed to read
   `crates/plugins/Cargo.toml`". A member glob cannot coexist with a grouping
   directory of the same name.

   The obvious fix, `exclude = ["crates/plugins"]`, is worse than the disease: an
   `exclude` on a grouping directory silently removes every path underneath it, so
   `crates/plugins/*` would match nothing and the crates would vanish from the
   workspace while the gate stayed green. That is exactly the defect that had
   `dxdb-memory` outside `make check`.

   The form that works is `crates/dxdb-*` in place of `crates/*`, which cannot
   match a grouping directory at all. One changed line, no `exclude` entry, and
   the reason for the `exclude` disappears rather than moving.

2. **`exclude = ["crates/storages"]` silently excluded
   `crates/storages/dxdb-memory` from the workspace.** Verified by
   `cargo metadata` listing six packages with no storage crate, so `make check`
   and `make test` never saw the parity oracle. `xtask/status.py`'s greps still
   found the files, so nothing failed loudly. Fixed on master by `d7f632b`; it is
   recorded here because this lane found it from the M6 side and the general rule
   ("`exclude` wins over `members` for everything underneath") is the reason to
   prefer a narrower `members` glob over an `exclude` in future lanes.

## 6. Open questions that need a decision, not more typing

3. **`attachments` path ownership conflicts with the lane brief.** The brief lists
   `attachments` first in the build order; `docs/WORKTREES.md` assigns
   `crates/dxdb-attachments/` to lane E. Filenames say E, the brief says here.
   This lane is treating E as authoritative and will not create the crate.
4. **The cipher choice in `encryption` is a parity break by design.** Confirm the
   divergence is acceptable: upstream ciphertext will not be readable by dxdb. If
   the answer is "no", the crate needs an `upstream-compat` feature that
   implements `EVP_BytesToKey`, which is a deliberate use of a weak KDF and needs
   a written justification in PLAN.md 9.
5. **Four validator crates become one.** `validate-ajv`, `validate-ajv-standalone`,
   `validate-is-my-json-valid`, `validate-z-schema` differ only in which JS
   library compiles the schema. Confirm the collapse to `validate` (runtime) plus
   `validate`'s `precompiled` feature is the intended reading of "port behaviour,
   not structure".
