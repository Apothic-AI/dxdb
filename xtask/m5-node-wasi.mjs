#!/usr/bin/env node
// node:wasi host for the wasip1 build of the conformance suite.
//
// Used by xtask/m5-conformance.sh. Kept as a file rather than an inline `-e`
// string so it can be read, tested and reported on.
//
// Flags: none beyond the standard runtime. `WASI` is experimental in Node 22, so a
// warning goes to stderr unless `--no-warnings` is passed; this script does not
// pass it, because hiding the warning would hide the fact that the API is
// experimental. Callers that want quiet can add the flag themselves.
//
// Usage: node m5-node-wasi.mjs <wasm> <guest-dir> <host-dir> [filter]
//
//   guest-dir / host-dir implement the same capability the Wasmtime `--dir`
//   mapping grants: the guest can see <guest-dir> and nothing else. Passing the
//   host path as the guest dir is the trap; they are deliberately separate
//   arguments so a caller has to think about both.

import { WASI } from 'node:wasi';
import { readFile } from 'node:fs/promises';

const [wasmPath, guestDir, hostDir, filter] = process.argv.slice(2);
if (!wasmPath || !guestDir || !hostDir) {
  console.error('usage: m5-node-wasi.mjs <wasm> <guest-dir> <host-dir> [filter]');
  process.exit(2);
}

const args = ['dxdb-conformance', '--dir', guestDir];
if (filter) args.push('--filter', filter);

const wasi = new WASI({
  version: 'preview1',
  args,
  preopens: { [guestDir]: hostDir },
  returnOnExit: true,
});

const bytes = await readFile(wasmPath);
const module = await WebAssembly.compile(bytes);
const instance = await WebAssembly.instantiate(module, wasi.getImportObject());

let code = 0;
try {
  code = wasi.start(instance) ?? 0;
} catch (error) {
  // A proc_exit surfaces as an exception carrying the code. Anything else is a
  // real failure and must not be reported as an exit code the suite "chose".
  if (typeof error?.code === 'number') {
    code = error.code;
  } else {
    console.error(`wasm trap: ${error?.message ?? error}`);
    code = 1;
  }
}
process.exit(code);
