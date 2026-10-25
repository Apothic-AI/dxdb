/**
 * The conformance-suite runner.
 *
 * The suite itself is lane D's `dxdb-testkit` crate (PLAN.md M4: "a demo app
 * that runs the same conformance suite in a browser"). It does not exist yet,
 * so this module fixes the *shape* the page calls and nothing else. That way
 * the page, the spec and the build script are all finished and proven now, and
 * landing the suite is a change to exactly one function.
 *
 * ## Contract
 *
 * `runConformanceSuite(storages)` resolves to one `StorageSuiteResult` per
 * storage, in the order given. It never rejects for a *failing storage* — a
 * storage that fails is a `status: 'failed'` result, because one broken storage
 * must not hide the state of the others. It rejects only when the harness
 * itself is broken (module failed to load, wasm trap).
 *
 * @typedef {'passed'|'failed'|'skipped'} SuiteStatus
 *
 * @typedef {object} StorageSuiteResult
 * @property {string} storage        storage name as exported by dxdb, e.g. "indexeddb"
 * @property {SuiteStatus} status
 * @property {number} assertions     how many assertions ran
 * @property {string[]} failures     one line per failure, empty when passed
 * @property {string} detail         human-readable summary; the skip reason when skipped
 *
 * @typedef {object} ConformanceOptions
 * @property {string[]} [only]       run only these storages; default: all available
 * @property {boolean} [fastMode]    mirror upstream's `isFastMode` (fewer iterations)
 */

/**
 * Storages this harness intends to cover, in the order they should be reported.
 *
 * `memory` is the parity oracle (PLAN.md 4.1) and is lane D's; the rest are this
 * lane's. The list is declared here rather than discovered so a storage that
 * fails to *build* shows up as a missing row instead of silently vanishing from
 * a green run.
 */
export const STORAGES_UNDER_TEST = /** @type {const} */ ([
  'memory',
  'indexeddb',
  'opfs-worker',
  'opfs-main-thread',
  'localstorage',
]);

/**
 * @param {ConformanceOptions} [options]
 * @returns {Promise<StorageSuiteResult[]>}
 */
export async function runConformanceSuite(options = {}) {
  const only = options.only ?? STORAGES_UNDER_TEST;

  // One row per storage, all skipped, with the blocking reason stated. This is
  // deliberate: the harness reports "not yet runnable" as data, so the day the
  // suite lands the results change meaning without the page changing shape.
  return only.map((storage) => ({
    storage,
    status: /** @type {SuiteStatus} */ ('skipped'),
    assertions: 0,
    failures: [],
    detail:
      'blocked: dxdb-testkit (lane D) has not published the storage conformance ' +
      'suite yet; see docs/lanes/m4-browser-plan.md, "Dependencies".',
  }));
}

/**
 * Reduce suite results to the single state the page publishes on `<body>`.
 *
 * `failed` dominates, then `skipped`, then `passed` — a run is only green if
 * something actually ran and nothing failed. This keeps the "all skipped"
 * tranche-1 state from ever reading as a pass.
 *
 * @param {StorageSuiteResult[]} results
 * @returns {'passed'|'failed'|'partial'|'skipped'}
 */
export function summarize(results) {
  if (results.some((r) => r.status === 'failed')) return 'failed';
  if (results.every((r) => r.status === 'skipped')) return 'skipped';
  if (results.some((r) => r.status === 'skipped')) return 'partial';
  return 'passed';
}
