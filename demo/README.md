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
| `wasm module instantiates` | the release `.wasm` instantiates and `abiVersion()` returns the crate version |
| `calling across the boundary works` | `echo()` round-trips UTF-16-sensitive text through wasm |
| `async boundary works` | the exported `async fn` resolves through a real JS `Promise` |
| `storage capability probe` | reports which browser APIs each engine actually exposes (see below) |
| `conformance suite` | **skipped** until `dxdb-testkit` is published by lane D |

The last one is a `test.skip` with the reason inline rather than a
half-implemented assertion, so the moment the suite exists the run turns red and
the wiring gets finished instead of quietly passing.

## Layout

```
demo/
  index.html              # the harness page; one <script type="module">
  app.js                  # page bootstrap: instantiate wasm, publish results
  harness/conformance.js  # the suite runner (stub with an explicit contract today)
  scripts/build-wasm.sh   # remote build via ../../Makefile, then wasm-bindgen
  scripts/serve.mjs       # static server; .wasm gets application/wasm
  tests/harness.spec.js   # Playwright spec, three browser projects
  playwright.config.js    # chromium + firefox + webkit projects
  pkg/                    # build output (gitignored)
```

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
