#!/usr/bin/env node
/**
 * The type-surface gate.
 *
 * Runs `tsc --noEmit` over `types/`, and — because TypeScript alone cannot catch
 * a *stale* declaration — checks that the generated `pkg/dxdb_js.d.ts` the
 * assignability assertions compare against is newer than the `.wasm` it came from.
 *
 * A `.d.ts` nobody compiles is a `.d.ts` that is wrong; a `.d.ts` compiled against
 * a stale `pkg/` is worse, because it is wrong *confidently*.
 *
 * Usage: node scripts/check-types.mjs
 */

import { spawnSync } from 'node:child_process';
import { existsSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const DEMO_DIR = fileURLToPath(new URL('..', import.meta.url));
const PKG = join(DEMO_DIR, 'pkg');
const GENERATED = join(PKG, 'dxdb_js.d.ts');
const WASM = join(PKG, 'dxdb_js_bg.wasm');

/** @param {string} message */
function fail(message) {
  console.error(`typecheck: ${message}`);
  process.exit(1);
}

if (!existsSync(GENERATED) || !existsSync(WASM)) {
  fail(
    `pkg/ is missing. The authored types are checked against the *generated* ones,\n` +
      `so the wasm bindings have to exist first:\n` +
      `  npm run build:wasm`,
  );
}

// `wasm-bindgen --out-dir` rewrites both files together, so equal mtimes are the
// normal state. Anything older than the wasm means pkg/ was hand-edited or
// partially restored, which is exactly when a silent drift would slip through.
if (statSync(GENERATED).mtimeMs < statSync(WASM).mtimeMs - 1000) {
  fail(
    `pkg/dxdb_js.d.ts is older than pkg/dxdb_js_bg.wasm — the generated types do\n` +
      `not describe the module in this directory. Re-run:\n` +
      `  npm run build:wasm`,
  );
}

const tsc = join(DEMO_DIR, 'node_modules', '.bin', 'tsc');
if (!existsSync(tsc)) {
  fail('tsc not installed. Run `npm ci` (typescript is a devDependency).');
}

const result = spawnSync(tsc, ['-p', join(DEMO_DIR, 'types', 'tsconfig.json')], {
  cwd: DEMO_DIR,
  stdio: 'inherit',
});

if (result.error) fail(`could not run tsc: ${result.error.message}`);
if (result.status !== 0) {
  // tsc has already printed the diagnostics.
  process.exit(result.status ?? 1);
}

console.log('typecheck: authored surface compiles, and matches the generated types');
