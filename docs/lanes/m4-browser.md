# Lane brief: M4 browser storages and the JS binding

You own `crates/dxdb-js/`, `crates/storages/dxdb-indexeddb/`,
`crates/storages/dxdb-opfs/`, `crates/storages/dxdb-localstorage/`,
`crates/storages/dxdb-remote/`, and `demo/`.

## Read first

1. `docs/PLAN.md` sections 3.3 (async traits / streams), 3.4 and 5 (the WASM
   strategy, in full — it is the whole point of this lane), 6 M4, 5.3 (budgets),
   5.4 (the storage availability matrix).
2. `docs/GATES.md`.
3. `docs/WORKTREES.md` — you own only the paths above.

Upstream at `~/Code/rxdb` (pin `3b763ee17`).

## What to build

### `dxdb-js`: the wasm-bindgen surface

- `wasm-bindgen` 0.2.129, `wasm-bindgen-futures`, `tsify-next` for typed options
  (`Cargo.toml` already pins the versions in `[workspace.dependencies]` where it
  can; add crate-local ones as needed).
- **The Observable bridge.** PLAN.md 3.3: "Do not leak RxJS. JS consumers get an
  adapter that turns the stream into an `Observable` at the boundary, in the same
  way `wasm-bindgen` adapts a `Promise`." Implement a minimal, correct `Observable`
  adapter: `subscribe(next, error, complete)` returning an object with
  `unsubscribe`, with `error` delivered on the error path and no double-callback
  after unsubscribe.
- A hand-written `.d.ts` a real app can compile against. No `any` in the public
  surface.

### The storages

- `dxdb-indexeddb`: IndexedDB via `web-sys`/`idb`. Custom index strings, not
  native indexes; batched cursor reads with `getAll` over explicit key ranges.
- `dxdb-opfs`: OPFS. **`createSyncAccessHandle` exists only in a dedicated
  worker**, so the fast path is inherently a worker story (PLAN.md 5.4). Handle
  the main-thread case by falling back to the async API, and document which path
  is in use.
- `dxdb-localstorage`: localStorage / any kv.
- `dxdb-remote`: the message-channel transport, mirroring upstream
  `storage-remote`.

Every one of them implements `dxdb-storage`'s traits and must pass the ported
conformance suite (lane D's `dxdb-testkit`). That is the acceptance criterion, and
it is what makes "this storage behaves like RxDB's" a fact rather than a claim.

### `demo/`

A real app, built from the repo, that runs the conformance suite in a browser via
Playwright on Chromium, Firefox, and WebKit. PLAN.md M4's gate is "a demo app
... that runs the same conformance suite in a browser".

## Size budgets (PLAN.md 5.3)

| Artifact | Budget |
| --- | --- |
| `dxdb-js` core only | 250 KiB brotli |
| `+ memory storage` | 320 KiB brotli |
| `+ OPFS storage` | 400 KiB brotli |
| `+ SQLite storage` | 900 KiB brotli |

Measure with the release profile already in `Cargo.toml` (`opt-level="s"`,
`lto="fat"`, `codegen-units=1`, `panic="abort"`). Record actual numbers in
`docs/BENCHMARKS.md` (leader-owned file; request the edit or report the numbers).
**Budgets are aspirational until measured, and they get revised with numbers, not
vibes.**

## Capability rules from PLAN.md 3.4 and 5.2

These are not suggestions; they are why the WASM plan is credible:

- `wasm32-unknown-unknown` **on stable 1.95.0** is the primary target and must
  build. Nothing in the core may hard-require a nightly-only feature.
- Safe on stable: `multivalue`, `reference-types`, `tail-call`, `simd128`,
  `relaxed-simd`, `extended-const`, `bulk-memory`, `nontrapping-fptoint`.
- Allowed but fragile (pin the toolchain, gate in CI): `exception-handling`, `gc`,
  `wide-arithmetic`, `atomics`.
- **Do not use**: `multimemory`, `relaxed-atomics` (unknown and unstable on the
  pinned toolchain).
- **`relaxed-simd` must never touch anything that becomes an index string or a
  hash.** Results there have to be deterministic. SIMD is for scanning.
- Threads need `wasm32-wasip1-threads` or nightly `-Z build-std`; the prebuilt
  `wasm32-unknown-unknown` std has no atomics. Do not make threads a default.
- **JSPI** is not a dependency but is the documented fallback for the OPFS
  synchronous-handle story. Note it where you rely on the shape of that fallback.

## Your gate

```bash
make check && make clippy && make test && make wasm
```

plus the demo app passing the conformance suite in all three browsers via
Playwright, with size measurements recorded. Report browser versions used.

## Constraints

- No `unsafe`.
- No `tokio`. `wasm-bindgen-futures` supplies the executor.
- Do not edit lane-owned files of other lanes. If you need a trait change in
  `dxdb-storage`, request it; do not edit it.

## Reporting

Report the gates with exit statuses, the three browser runs with versions, the
measured bundle sizes against the budgets, and any capability you had to gate.
