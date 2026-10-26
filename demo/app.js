/**
 * Harness page bootstrap.
 *
 * Order matters and is the reason this is one module rather than inline script:
 *
 *  1. probe capabilities (must work even when the wasm module is missing, so a
 *     build failure still yields a useful capability record);
 *  2. import the wasm-bindgen module (a *dynamic* import, so a missing `pkg/`
 *     produces a reported failure instead of a blank page);
 *  3. exercise the boundary;
 *  4. run the conformance suite;
 *  5. publish everything as data attributes + a JSON blob.
 *
 * Step 5 is the contract with Playwright: `body[data-status="done"]` means "the
 * page finished", whatever the outcome. A page that never reaches it is a
 * harness bug and the spec reports it as such.
 */

import { runCapabilityProbe } from './harness/capabilities.js';
import { runAdapterCases } from './harness/adapter-cases.js';
import { runConformanceSuite, summarize } from './harness/conformance.js';

const results = {
  status: 'pending',
  environment: {},
  capabilities: [],
  /** @type {Record<string, unknown>} */
  wasm: { available: false, error: null },
  adapter: [],
  conformance: [],
  consoleErrors: [],
};

// Capture console errors from the very start: a wasm trap or a failed module
// fetch shows up here and nowhere else, and "no console errors" is one of the
// things the spec asserts.
window.addEventListener('error', (ev) => {
  results.consoleErrors.push(`error: ${ev.message}`);
});
window.addEventListener('unhandledrejection', (ev) => {
  results.consoleErrors.push(`unhandledrejection: ${String(ev.reason)}`);
});
{
  const originalError = console.error.bind(console);
  console.error = (...args) => {
    results.consoleErrors.push(`console.error: ${args.map(String).join(' ')}`);
    originalError(...args);
  };
}

function renderRow(tableId, cells) {
  const tbody = document.querySelector(`#${tableId} tbody`);
  if (!tbody) return;
  const tr = document.createElement('tr');
  for (const [i, cell] of cells.entries()) {
    const td = document.createElement('td');
    td.textContent = String(cell);
    if (i === 1 && typeof cell === 'string') {
      td.className = cell === 'pass' || cell === 'present' ? 'pass'
        : cell === 'skip' ? 'skip' : 'fail';
    }
    tr.appendChild(td);
  }
  tbody.appendChild(tr);
}

function publish() {
  document.body.dataset.status = results.status;
  const statusEl = document.getElementById('status');
  statusEl.textContent = results.status;
  statusEl.dataset.state = results.status;

  const blob = document.getElementById('results');
  blob.textContent = JSON.stringify(results, null, 2);
  // Also expose it as a property so the spec can read it without parsing DOM
  // text; both paths are cheap and the property is the sturdier one.
  window.__dxdbHarness = results;
}

async function main() {
  try {
    // ---- 1. capabilities ------------------------------------------------
    const { probes, environment } = await runCapabilityProbe();
    results.environment = environment;
    results.capabilities = probes;
    for (const p of probes) renderRow('capabilities', [p.name, p.ok ? 'present' : 'absent', p.detail]);

    // ---- 2. wasm module -------------------------------------------------
    let module;
    try {
      module = await import('../pkg/dxdb_js.js');
      // `--target web` emits a module whose exports are unusable until the
      // default init function has fetched and instantiated the `.wasm`.
      // Forgetting this is the classic "__wbindgen_malloc is undefined" trap,
      // so it is done here, immediately after the import.
      await module.default();
      results.wasm.available = true;
    } catch (err) {
      results.wasm.error = String(err);
      // Keep going: capabilities are still worth reporting, and the suite is
      // independently skippable. The final status records the failure below.
    }

    // ---- 3. boundary exercise -------------------------------------------
    if (module) {
      // `wasm-bindgen` keeps the Rust identifier unless `js_name` is given, so
      // these are snake_case. Renaming them with `#[wasm_bindgen(js_name = …)]`
      // is the plan's call for the real surface (docs/lanes/m4-browser-plan.md
      // §2.1 lists camelCase public names); this smoke surface stays honest to
      // what it actually generates so a rename is a deliberate change.
      const abiVersion = module.abi_version();
      const echoed = module.echo('héllo — 世界 🦀');
      const asyncValue = await module.resolve_after_tick(41);

      results.wasm.abiVersion = abiVersion;
      results.wasm.echo = echoed;
      results.wasm.asyncValue = asyncValue;
      results.wasm.asyncResolved = asyncValue === 42;

      // ---- 3b. the Observable adapter, in this engine -------------------
      // Written to a separate table because it is a different thing from a
      // capability: these are assertions about our own code.
      const adapter = await runAdapterCases(module);
      results.adapter = adapter;
      for (const c of adapter) renderRow('adapter', [c.name, c.status, c.detail]);
    }

    // ---- 4. conformance suite -------------------------------------------
    const conformance = await runConformanceSuite();
    results.conformance = conformance;
    for (const c of conformance) renderRow('suite', [c.storage, c.status, c.detail]);

    // ---- 5. final status ------------------------------------------------
    const suiteStatus = summarize(conformance);
    const adapterFailed = results.adapter.some((c) => c.status !== 'pass');
    if (!results.wasm.available) {
      results.status = 'failed';
    } else if (results.wasm.asyncResolved !== true) {
      results.status = 'failed';
    } else if (adapterFailed) {
      // The adapter is our own code under assertion, so a failure here is a
      // failure of the harness, unambiguously.
      results.status = 'failed';
    } else if (results.consoleErrors.length > 0) {
      results.status = 'failed';
    } else {
      // `skipped`/`partial` are honest non-green states: the suite has not run.
      results.status = suiteStatus === 'passed' ? 'passed' : suiteStatus;
    }
  } catch (err) {
    results.status = 'failed';
    results.fatal = String(err && err.stack || err);
  } finally {
    publish();
  }
}

main();
