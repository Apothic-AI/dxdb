# dxdb browser demo / conformance harness

A real page, served from the repo, that loads the `dxdb-js` wasm module and runs
the storage conformance suite in a real browser. The suite itself
(`dxdb-testkit`, PLAN.md M4 acceptance criterion) is lane D's crate and lands
later; this harness is the wiring that will run it, and today it proves that
wiring end to end in **Chromium, Firefox and WebKit**.

## What runs today

| Test | Asserts |
| --- | --- |
| `page loads` | the harness page serves, the module graph parses, no console errors |
| `wasm module instantiates` | the release `.wasm` instantiates and `abi_version()` returns the crate version |
| `calling across the boundary works` | `echo()` round-trips UTF-16-sensitive text through wasm |
| `async boundary works` | the exported `async fn` resolves through a real JS `Promise` |
| `Observable adapter` | **14 cases**, all passing in this engine (see below) |
| `storage capability probe` | reports which browser APIs each engine actually exposes (see below) |
| `conformance suite` | **skipped** until `dxdb-testkit` is published |
| `typecheck` | `types/` compiles and matches the generated `.d.ts` (runs before the suite) |

The last one is a `test.skip` with the reason inline rather than a
half-implemented assertion, so the moment the suite exists the run turns red and
the wiring gets finished instead of quietly passing.

## Layout

```
demo/
  index.html                  # the harness page; one <script type="module">
  app.js                      # bootstrap: probe, instantiate wasm, adapter cases, suite
  harness/capabilities.js     # per-engine capability probe (main thread + worker)
  harness/sync-handle-worker.js  # the worker half of the OPFS evidence
  harness/adapter-cases.js    # 14 Observable-adapter cases, run in each engine
  harness/conformance.js      # storage suite runner (stub with an explicit contract)
  types/dxdb.d.ts             # the authored TypeScript surface (no `any`)
  types/consumer.ts           # compiles against it; carries @ts-expect-error negatives
  types/assignability.ts      # drift guard against the generated .d.ts
  types/tsconfig.json         # strict, noEmit
  scripts/build-wasm.sh       # remote build via ../../Makefile, then wasm-bindgen
  scripts/check-types.mjs     # stale-pkg guard + tsc --noEmit
  scripts/serve.mjs           # static server; .wasm gets application/wasm
  scripts/collect-capabilities.mjs  # prints each engine's probe + version
  tests/harness.spec.js       # Playwright spec, three browser projects
  playwright.config.js        # chromium + firefox + webkit projects
  pkg/                        # build output (gitignored)
```

## The two things worth knowing before you edit this

**`reuseExistingServer` is `false` on purpose.** A server left over from a previous
session serves the *old* `pkg/` and the old JS, and the resulting failure looks
like a wasm or adapter bug rather than a stale process. If you have
`npm run serve` running, stop it before `npm test`.

**`pkg/` is a build artifact and is checked for staleness.** `scripts/check-types.mjs`
fails if `pkg/dxdb_js.d.ts` is older than `pkg/dxdb_js_bg.wasm`, because the
assignability assertions compare the authored types against the generated ones and
a stale generated file makes them pass for the wrong reason.

## Running it

```bash
cd demo
npm ci                              # or npm install
npm run install-browsers            # chromium firefox webkit, once
npm test                            # build wasm + run all three browsers
npm run test:chromium               # one engine
```

`npm test` runs `scripts/build-wasm.sh` first. That script calls the workspace
`Makefile` target, which is the only sanctioned compile path — it goes to the
shared Fly build server and takes the flock. **Nothing is compiled locally.**

If the remote box is busy the build queues; that is expected, not a hang.

### Requirements

- `wasm-bindgen` CLI **exactly matching** the `wasm-bindgen` crate version
  (`0.2.129`). The script checks this and fails with the exact command to fix it,
  because the mismatch otherwise surfaces as an unrelated-looking trap at
  instantiation.
- `brotli` (the size gate in the harness page).

## The capability probe is evidence, not decoration

The page probes and reports, per engine:

- `navigator.storage.getDirectory` — OPFS present
- `FileSystemFileHandle.prototype.createSyncAccessHandle` — the **synchronous**
  handle. This is the API that decides the OPFS strategy, because it exists only
  in a dedicated worker (PLAN.md 5.4). The page also re-checks the same
  property from inside a worker (`harness/sync-handle-worker.js`) so the
  worker-vs-main-thread difference is measured in each engine rather than
  assumed from documentation.
- `indexedDB`, `localStorage`, `BroadcastChannel`, `navigator.locks`
- `WebAssembly` feature detection for the features PLAN.md 3.4 relies on:
  `simd128`, `bulk-memory`, `reference-types`, `tail-call`, `extended-const`,
  `nontrapping-fptoint`. These decide whether the same `.wasm` runs everywhere
  or needs a fallback build, so the harness records the answer per browser
  version instead of trusting a caniuse table.

The probe results are written to `probe.json` as a Playwright attachment, so a
run leaves a record of which browser build reported which capabilities. That is
the evidence the plan file cites.

## The Observable adapter cases

`harness/adapter-cases.js` is the browser half of the adapter proof. The Rust half
is `crates/dxdb-js/src/observable/state.rs` (14 host tests over the delivery
policy). The split is deliberate: the policy is an ordering discipline and is
tested exhaustively where that is cheap; the browser cases prove the
`#[wasm_bindgen]` shell faithfully implements it across the real boundary.

Each case reports `pass`/`fail` individually, and the spec asserts the **count**
as well as the outcomes, so a suite that silently shrinks is a failure rather
than a smaller green run.

The cases include the ones that are easy to get wrong and hard to notice: a
callback queued at the moment of `unsubscribe` must be dropped (asserted after
awaiting a microtask, since the bug only shows up a turn later); `unsubscribe`
called from *inside* a callback; and a consumer callback that throws, which must
not trap the module or wedge the subscription.

## Adding the conformance suite

`harness/conformance.js` exports one function and documents the contract. When
`dxdb-testkit` exists:

1. build its wasm test surface into `pkg/` (or import its wasm-bindgen module),
2. have `runConformanceSuite` call it per storage under test,
3. delete the `test.skip` in `tests/harness.spec.js` and assert the returned
   per-storage results.

The page is already structured for that: one storage per section, each section
reporting `passed/failed/skipped` independently, so one storage failing does not
hide the state of the others.
