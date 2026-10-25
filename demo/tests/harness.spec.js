// Harness spec.
//
// This file's job in tranche 1 is to prove the *wiring*: a real browser loads
// the real wasm module, crosses the boundary, and reports capabilities. The
// conformance assertions are explicitly skipped rather than absent, so that:
//
//   - a green run today means "the harness works", not "the storages work";
//   - the day dxdb-testkit lands, the skip turns into a failure that names what
//     to wire up, instead of the suite quietly staying green.

import { test, expect } from '@playwright/test';

/** @type {import('@playwright/test').Page} */
let page;

test.beforeEach(async ({ page: p }) => {
  page = p;
  await page.goto('/');
  // The page publishes `data-status="done"` in a `finally`, so this resolves for
  // failures too — the assertions below are what decide pass/fail.
  await expect(page.locator('body')).toHaveAttribute('data-status', /^(?!pending).+$/, {
    timeout: 30_000,
  });
});

/** Read the harness result object the page publishes. */
async function harness() {
  return page.evaluate(() => {
    if (!window.__dxdbHarness) {
      throw new Error('window.__dxdbHarness missing: the page did not publish results');
    }
    return window.__dxdbHarness;
  });
}

test('page loads and reports a terminal state', async () => {
  const result = await harness();
  expect(result).toHaveProperty('status');
  // The status must be one we know how to interpret; a typo'd state would
  // otherwise read as "not failed" and pass silently.
  expect(['passed', 'failed', 'skipped', 'partial']).toContain(result.status);
  expect(result.consoleErrors, 'page reported console errors').toEqual([]);
});

test('wasm module instantiates and the ABI version crosses the boundary', async () => {
  const result = await harness();
  expect(result.wasm.error, 'wasm import failed').toBeNull();
  expect(result.wasm.available).toBe(true);
  // dxdb-js is 0.0.0 until the workspace version moves; asserting the exact
  // value would make every unrelated version bump a test failure.
  expect(result.wasm.abiVersion).toMatch(/^\d+\.\d+\.\d+/);
});

test('calling across the boundary preserves UTF-16-sensitive text', async () => {
  const result = await harness();
  // Astral-plane characters and non-ASCII are the case that breaks when a
  // binding round-trips through a byte-oriented string type instead of UTF-16
  // (PLAN.md 3.2 is the same class of bug one layer down).
  expect(result.wasm.echo).toBe('héllo — 世界 🦀');
});

test('async exports resolve through a real JS Promise', async () => {
  const result = await harness();
  expect(result.wasm.asyncResolved).toBe(true);
  expect(result.wasm.asyncValue).toBe(42);
});

test('capability probe produces a complete record', async () => {
  const result = await harness();
  expect(Array.isArray(result.capabilities)).toBe(true);

  const byName = Object.fromEntries(result.capabilities.map((c) => [c.name, c]));
  const required = [
    'indexedDB',
    'localStorage',
    'BroadcastChannel',
    'navigator.storage.getDirectory (OPFS)',
    'createSyncAccessHandle on main thread',
    'createSyncAccessHandle in dedicated worker',
    'createWritable on main thread',
    'wasm baseline',
  ];
  for (const name of required) {
    // Pass the name in an array: `toHaveProperty` parses a plain string as a
    // dotted path, and these names contain dots (`navigator.storage…`).
    expect(byName, `missing capability probe: ${name}`).toHaveProperty([name]);
  }

  // Every engine must have IndexedDB and localStorage for the conformance suite
  // to be runnable at all.
  expect(byName['indexedDB'].ok, 'IndexedDB unavailable').toBe(true);
  expect(byName['localStorage'].ok, 'localStorage unavailable').toBe(true);

  // PLAN.md 5.4's load-bearing claim, measured per engine: the synchronous OPFS
  // handle is NOT on the main thread but IS in a dedicated worker. If a browser
  // starts exposing it on the main thread, this assertion is what tells us the
  // OPFS plan can be simplified.
  expect(
    byName['createSyncAccessHandle on main thread'].ok,
    'createSyncAccessHandle unexpectedly exposed on the main thread',
  ).toBe(false);

  // The worker half is asserted only where OPFS exists; WebKit historically
  // gated OPFS behind a flag, and a missing FileSystemFileHandle there is a
  // real, reportable browser difference rather than a harness bug.
  const opfs = byName['navigator.storage.getDirectory (OPFS)'].ok;
  if (opfs) {
    expect(
      byName['createSyncAccessHandle in dedicated worker'].ok,
      `OPFS is present but the worker-only sync handle is missing: ${byName['createSyncAccessHandle in dedicated worker'].detail}`,
    ).toBe(true);
  }

  // The shipped core may use these unconditionally (PLAN.md 3.4), so all three
  // engines must agree.
  expect(byName['wasm baseline'].ok, 'WebAssembly unavailable').toBe(true);
  expect(byName['wasm simd128'].ok, 'simd128 unavailable').toBe(true);

  // Attach the full record to the report: this is the evidence a run leaves
  // behind, and it is what makes "which engine had which capability" checkable
  // after the fact.
  await test.info().attach('capabilities.json', {
    contentType: 'application/json',
    body: JSON.stringify(
      { environment: result.environment, capabilities: result.capabilities },
      null,
      2,
    ),
  });
});

test('conformance suite runs against every declared storage', async () => {
  // Blocked on lane D. When dxdb-testkit publishes the storage conformance
  // suite (and dxdb-storage publishes the traits this lane's storages implement
  // against), this assertion becomes:
  //
  //   const result = await harness();
  //   expect(result.status).toBe('passed');
  //   for (const row of result.conformance) {
  //     expect(row.status, `${row.storage}: ${row.detail}`).toBe('passed');
  //   }
  //
  // and this skip is deleted. See docs/lanes/m4-browser-plan.md,
  // "Dependencies and the unblock condition".
  test.skip(true, 'blocked: dxdb-testkit storage conformance suite not published by lane D');

  const result = await harness();
  expect(result.status).toBe('passed');
  for (const row of result.conformance) {
    expect(row.status, `${row.storage}: ${row.detail}`).toBe('passed');
  }
});
