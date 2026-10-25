# `crates/plugins/` — one crate per upstream plugin

Mirrors `~/Code/rxdb/src/plugins/*` at pin `3b763ee17`: 52 directories, 37,698
lines of TypeScript.

The classification of all 52 modules, the port order, and the Hooks API design
live in [`docs/lanes/m6-port-plan.md`](../../docs/lanes/m6-port-plan.md). This
file is the **omission register** that PLAN.md section 10 requires: every module
that is not ported here either names its dxdb home or carries a reviewed
justification, and nothing in `crates/plugins/` is allowed to be a plugin that
belongs to another lane without saying so.

## Rules this directory follows

- One crate per plugin, named `dxdb-plugin-<upstream-dir>`. No `dxdb-plugins`
  monolith (lane brief, "Rules").
- A crate may depend on `dxdb-storage` **traits**, never on a concrete storage
  crate.
- Every crate builds for `wasm32-unknown-unknown`. Anything filesystem- or
  network-specific gets a feature gate, not a broken build.
- `#![forbid(unsafe_code)]`.
- Ported tests name the upstream file and line they came from.
- Behaviour is ported; structure is not, where Rust forces a difference, and the
  difference is written into the crate's module docs.

## Where the 52 modules actually live

| Class | Modules | Lines | Home |
| --- | --- | --- | --- |
| Portable plugin behaviour | 21 | 9,900 | `crates/plugins/*`, this lane |
| Storage plugins | 9 | 8,178 | storage crates (lanes D, F, G), or justified omission |
| Replication adapters | 11 | 11,038 | `dxdb-replication` + adapter crates (lane E) |
| Owned by another lane | 2 | 1,419 | `attachments` → lane E, `replication` → lane E |
| JS-ecosystem bindings | 7 | 1,586 | omission, justified below |
| Not plugins | 2 | 5,579 | `utils` → merged by concern; `test-utils` → `dxdb-testkit` (lane D) |

Only the 21 in the first row are this lane's to build. That is the honest scope
of M6: **9,900 lines, 26% of the plugin tree**, not 37,698.

## Omissions and re-homings, with justification

Every row below is a module this lane will never port as a plugin crate. The
reason and the substitute user-facing answer are the deliverable.

### JS-ecosystem bindings (7 modules, 1,586 lines) — omitted

These exist to bind RxDB to a JavaScript runtime or framework. There is no Rust
counterpart to be behaviourally identical to, because the counterpart is the
host's own surface.

| Module | Lines | Why not ported | What a Rust user does instead |
| --- | --- | --- | --- |
| `react` | 469 | Nine files of React hooks (`use-rx-database`, `use-live-rx-query`, `use-replication-status`, ...) built on `useState`/`useEffect`. The product is a React component contract, not database behaviour. | Consume `dxdb-js`'s change-event stream from a React app and keep the hooks in an app-side `dxdb-react` package written in TypeScript against the generated `.d.ts` (lane F owns that surface). The live-query semantics the hooks wrap are already dxdb's, not the plugin's. |
| `flutter` | 65 | Registers a `process.init` bridge and a Dart `Lokijs` adapter. Depends on Flutter's platform channel. | Bind `dxdb` from Dart directly (FFI over the native build, or the WASM build in Flutter web). The plugin is a bridge to a specific transport we do not use. |
| `electron` | 125 | A `storage-remote` transport over Electron's `ipcMain`/`ipcRenderer`. Not database behaviour; a message channel we neither own nor can test. | `dxdb-remote` already exposes the transport-neutral half (`storage-remote`'s message protocol, lane F). An Electron app wires its own IPC callbacks into that, which is what the upstream plugin does in 125 lines. |
| `webmcp` | 762 | Exposes collections as browser tool-calling targets via `navigator.modelContext` / `document.modelContext` (`webmcp-tools.ts:15-18`). Tied to a browser API that is not stabilised, and the payload is a JSON-schema description of Mango queries. | Nothing to port: the same effect is a small adapter over `dxdb-js`. Note the part that *is* interesting (the Mango-query JSON schema it advertises, `nosql-query-schema.ts`) is a description of `dxdb-query`'s surface and would be generated from it if a tool-calling binding is ever wanted. |
| `reactivity-angular` | 32 | Wraps `toSignal`/`Injector` from `@angular/core`. | `dxdb-reactivity` (lane D) publishes the change streams; an Angular app adapts them with `toSignal` in three lines. |
| `reactivity-preact-signals` | 90 | Wraps Preact's `Signal`. | Same: adapt `dxdb-reactivity`'s streams to a `Signal` app-side. |
| `reactivity-vue` | 43 | Wraps Vue's `shallowRef`/`triggerRef`. | Same: adapt `dxdb-reactivity`'s streams to a `ref` app-side. |

The three reactivity adapters are 165 lines combined and exist because
TypeScript generics let a `ReactivityFactory` return a framework-native handle.
Rust's equivalent is the extension-trait/type-indexed-map pattern PLAN.md 4.3
requires for ORM methods; the framework half stays in the host language.

### Storage plugins (9 modules, 8,178 lines) — other crumbs' work

`dxdb-storage` is the trait spine and these are implementations of it. Lane
brief and `docs/WORKTREES.md` assign them elsewhere; porting them here would
duplicate another lane's work and break the "no concrete storage dependency"
rule in this directory.

| Module | Lines | dxdb home | Note |
| --- | --- | --- | --- |
| `storage-memory` | 1,387 | `crates/storages/dxdb-memory` | Lane D. The parity oracle. |
| `storage-sqlite` | 1,746 | `crates/storages/dxdb-sqlite` | Lane G. |
| `storage-dexie` | 1,041 | — | **Justified omission.** Dexie is a JS IndexedDB wrapper. Lane F's `dxdb-indexeddb` talks to IndexedDB directly; porting Dexie's caching layer would be porting a library, not a storage contract. Upstream's own IndexedDB performance problems are the reason dxdb does not use it. |
| `storage-localstorage` | 794 | `crates/storages/dxdb-localstorage` | Lane F. |
| `storage-remote` | 827 | `crates/storages/dxdb-remote` | Lane F. |
| `storage-remote-websocket` | 252 | `crates/storages/dxdb-remote` (feature) | Lane F. A `ws`-specific transport over `storage-remote`; the transport-neutral protocol is the portable part. |
| `storage-foundationdb` | 736 | — | **Justified omission.** FoundationDB is a native C client with its own cluster process. It is not a consumer storage for dxdb's targets (browser, Node, WASI); a user who wants it implements `dxdb-storage` traits over the FDB client crate, and the trait suite in `dxdb-testkit` is what proves it correct. |
| `storage-mongodb` | 699 | — | **Justified omission.** The upstream plugin is marked premium-adjacent and is a network client to MongoDB. Same answer as FoundationDB: an out-of-tree implementation of the published traits. |
| `storage-denokv` | 696 | — | **Justified omission.** Deno KV is a Deno-host API. The WASI(Node) target uses `dxdb-filesystem` (lane G); a Deno user reaches dxdb through the same napi/WASI surface. |

### Replication adapters (11 modules, 11,038 lines) — lane E's protocol, adapter crates later

`replication` (the protocol, 1,022 lines) and `dxdb-replication`'s core belong to
lane E. The adapters are thin, server-specific endpoint implementations
(`replicateCouchDB`, `replicateGraphQL`, `replicateNATS`, ...). They are
sequenced *last* in this lane's brief and are only meaningful once
`dxdb-replication` is stable, so they are declared out of scope for M6's first
pass and named here rather than silently dropped.

`replication-google-drive` (2,421) and `replication-microsoft-onedrive` (2,053)
are OAuth-and-HTTP clients for proprietary REST APIs; they are the largest single
chunk of the plugin tree and give the least portability. A Rust port is possible
and is the honest long-term answer, but it is a per-service integration project,
not plugin parity.

### Owned elsewhere (2 modules, 1,419 lines)

- `attachments` (397) — `crates/dxdb-attachments/`, lane E. This lane's brief
  lists it first in the build order for user unblocking, but PATH OWNERSHIP is
  `docs/WORKTREES.md`, and it assigns the crate to lane E. This lane consumes it;
  `crates/plugins/` will not contain an `attachments` crate.
- `replication` (1,022) — `crates/dxdb-replication/`, lane E.

The hooks this crate registers (`preMigrateDocument`,
`src/plugins/attachments/index.ts:306`) are still this lane's design
responsibility, because the hook registry is. See the plan's Hooks section.

### Not plugins (2 modules, 5,579 lines)

- `utils` (1,817, 22 files) — a grab bag that upstream keeps in a plugin
  directory for packaging reasons. Each file lands where its concern lives:
  `utils-revision.ts` → `dxdb-core` (revisions), `utils-base64.ts` /
  `utils-blob.ts` → `dxdb-attachments` (lane E), `utils-object-deep-equal.ts` /
  `utils-object-dot-prop.ts` → `dxdb-json`, `utils-hash.ts` → `dxdb-json`
  (schema hash), `utils-number.ts` → `dxdb-jsnum` (lane A). Porting it as a
  `dxdb-plugin-utils` crate would be a crate that every other crate depends on
  and that owns nothing.
- `test-utils` (3,762, 10 files) — not a plugin. Its ergonomics live in
  `dxdb-testkit` (lane D, per that lane's brief and PLAN.md M3).
