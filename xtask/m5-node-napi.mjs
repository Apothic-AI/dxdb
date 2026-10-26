#!/usr/bin/env node
// Node host for the napi addon build of the conformance suite.
//
// Usage: node m5-node-napi.mjs <addon.node> <db-dir> [filter]
//
// Node flags: none. A `.node` addon is a plain `require`, so unlike the WASI path there
// is no experimental subsystem involved.
//
// <db-dir> is an ordinary host path: a native addon runs with the process's own
// filesystem access, so there is no preopen and no guest/host distinction. The storage
// underneath is the same code as the WASI paths, which is the point of running both.

import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const [addonPath, dbDir, filter] = process.argv.slice(2);

if (!addonPath || !dbDir) {
  console.error('usage: m5-node-napi.mjs <addon.node> <db-dir> [filter]');
  process.exit(2);
}

let addon;
try {
  addon = require(addonPath);
} catch (error) {
  console.error(`could not load addon ${addonPath}: ${error?.message ?? error}`);
  process.exit(1);
}

// Every export the addon must provide, checked explicitly: a partially loaded addon that
// answers the one call we make is not a passing target.
const required = ['runConformanceSuite', 'suiteCaseIds', 'exitCodeFor'];
const missing = required.filter((name) => typeof addon[name] !== 'function');
if (missing.length > 0) {
  console.error(`addon is missing exports: ${missing.join(', ')}`);
  process.exit(1);
}

const ids = addon.suiteCaseIds();
if (!Array.isArray(ids) || ids.length === 0) {
  console.error('addon reported no case ids');
  process.exit(1);
}

let result;
try {
  result = addon.runConformanceSuite(dbDir, filter ?? undefined);
} catch (error) {
  console.error(`runConformanceSuite threw: ${error?.message ?? error}`);
  process.exit(1);
}

for (const failure of result.failures ?? []) {
  console.log(failure);
}
console.log(`${result.passed}/${ids.length} passed, ${result.failed} failed, ${result.skipped} skipped`);

// The verdict comes from the addon so it uses the same rule as the Rust front ends
// rather than a second implementation here.
process.exit(addon.exitCodeFor(result.passed, result.failed));
