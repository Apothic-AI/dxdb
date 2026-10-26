#!/usr/bin/env node
// node:wasi host for the wasip1 build of the conformance suite.
//
// Usage: node m5-node-wasi.mjs <wasm> <guest-dir> <host-dir> [filter]
//
// <guest-dir> and <host-dir> are separate arguments on purpose: they implement the same
// capability the Wasmtime `--dir` mapping grants (the guest sees <guest-dir> and nothing
// else), and collapsing them into one is exactly the host-path-vs-guest-path mistake the
// suite's preopen check exists to catch.
//
// Node flags: none required. `node:wasi` is experimental in Node 22 and prints an
// ExperimentalWarning on stderr; this harness does not suppress it, because hiding it
// would hide that the API is experimental. Add `--no-warnings` if the noise matters.

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
  // A proc_exit surfaces as an exception carrying the code. Anything else is a real
  // failure and must not be reported as an exit code the suite "chose".
  if (typeof error?.code === 'number') {
    code = error.code;
  } else {
    console.error(`wasm trap: ${error?.message ?? error}`);
    code = 1;
  }
}
process.exit(code);
