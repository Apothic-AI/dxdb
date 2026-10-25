/**
 * Dedicated worker whose only job is to answer one question:
 *
 *   is `FileSystemFileHandle.prototype.createSyncAccessHandle` exposed here?
 *
 * It is not exposed to the main thread, an iframe, or a SharedWorker — only to
 * a dedicated worker (PLAN.md 5.4, and the OPFS storage docs upstream). This
 * worker measures the worker side of that difference in each engine the demo
 * runs in, so the plan's OPFS decision rests on a measurement rather than a
 * doc quote.
 *
 * The probe does not call `createSyncAccessHandle()`: it checks the method's
 * presence and, when present, that it is callable. Calling it would require a
 * real file handle and would create state; presence is what decides the
 * strategy.
 */

/** @returns {{ ok: boolean, detail: string }} */
function probe() {
  const proto =
    typeof FileSystemFileHandle !== 'undefined'
      ? FileSystemFileHandle.prototype
      : undefined;

  if (!proto) {
    return { ok: false, detail: 'FileSystemFileHandle is not defined in this worker' };
  }

  if (typeof proto.createSyncAccessHandle !== 'function') {
    return { ok: false, detail: 'FileSystemFileHandle present, createSyncAccessHandle absent' };
  }

  // `createWritable` is the async path. Reporting both makes it obvious when an
  // engine has the worker-only API but not the main-thread one, or vice versa.
  const hasWritable = typeof proto.createWritable === 'function';
  return {
    ok: true,
    detail: `createSyncAccessHandle present (createWritable ${hasWritable ? 'also ' : 'NOT '}present)`,
  };
}

try {
  self.postMessage(probe());
} catch (err) {
  self.postMessage({ ok: false, detail: `probe threw: ${String(err && err.message || err)}` });
}
