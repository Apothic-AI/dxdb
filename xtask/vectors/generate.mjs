#!/usr/bin/env node
/**
 * Generate (or verify) the cross-language oracle vectors from upstream RxDB.
 *
 * This is PLAN.md section 7.2. Each family module below runs real upstream RxDB
 * in Node and writes what it observed to `testdata/vectors/<family>.json`. Those
 * files are the specification the Rust port is tested against, so they must come
 * from upstream and never from our own implementation.
 *
 *   node xtask/vectors/generate.mjs           # regenerate every family
 *   node xtask/vectors/generate.mjs --check    # fail if any committed file drifted
 *   node xtask/vectors/generate.mjs jsnum      # run only the named families
 *
 * `--check` is what `make vectors-check` runs, and what CI runs. It compares
 * parsed content with the informational `node` field removed: an upgrade that
 * changes nothing is not drift, a change in any recorded observation is.
 *
 * Upstream is provisioned by `vendor.mjs`, which fetches the pinned commit into
 * `~/.cache/dxdb-vectors/` and bundles its TypeScript sources with esbuild. See
 * that file for why the npm tarball and the built `dist/` are both rejected.
 */
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { installDeterminism } from './harness.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, '..', '..');

/**
 * Order matters only for reporting: jsnum is first because it is the oldest and
 * smallest family and a failure there means something is wrong with Node
 * itself, not with RxDB.
 */
const FAMILIES = [
  { name: 'jsnum', module: './jsnum.mjs', argvRunner: true },
  { name: 'index-strings', module: './families/index-strings.mjs' },
  { name: 'schema-hashes', module: './families/schema-hashes.mjs' },
  { name: 'normalised-queries', module: './families/normalised-queries.mjs' },
  { name: 'bulk-write-categorization', module: './families/bulk-write-categorization.mjs' },
  { name: 'selector-matching', module: './families/selector-matching.mjs' },
  { name: 'replication-transcripts', module: './families/replication-transcripts.mjs' },
];

function usage() {
  console.log('usage: node xtask/vectors/generate.mjs [--check] [family ...]');
  console.log('families: ' + FAMILIES.map((f) => f.name).join(', '));
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--help') || argv.includes('-h')) {
    usage();
    return 0;
  }
  const check = argv.includes('--check');
  const requested = argv.filter((a) => !a.startsWith('-'));
  const unknown = requested.filter((name) => !FAMILIES.some((f) => f.name === name));
  if (unknown.length > 0) {
    console.error(`unknown family: ${unknown.join(', ')}`);
    usage();
    return 2;
  }
  const selected = requested.length > 0 ? FAMILIES.filter((f) => requested.includes(f.name)) : FAMILIES;

  /**
   * jsnum needs none of this, but it is harmless for it and it must be installed
   * before *any* oracle module is imported, because upstream's `randomToken()`
   * and `now()` bake their results into the fixture objects at import time.
   */
  installDeterminism();

  if (!check) {
    // Regenerating without the pinned upstream would silently produce vectors
    // from whatever is on this machine. Provision (or verify) it up front.
    const { loadOracle } = await import('./vendor.mjs');
    await loadOracle();
  }

  const results = [];
  for (const family of selected) {
    if (family.argvRunner) {
      // jsnum.mjs is standalone by design (it predates the dispatcher and has its
      // own --check). Run it in-process so one process covers every family.
      const mod = await import(family.module);
      const result = await mod.run({ check, repo });
      results.push({ name: family.name, ...result });
      continue;
    }
    const mod = await import(family.module);
    const result = await mod.run({ check, repo, here });
    results.push({ name: family.name, ...result });
  }

  const failed = results.filter((r) => !r.ok);
  const totalCases = results.reduce((sum, r) => sum + (r.cases || 0), 0);

  if (!check) {
    const bytes = results.reduce((sum, r) => sum + (r.bytes || 0), 0);
    console.log(`\n${results.length} families, ${totalCases} cases, ${bytes} bytes written`);
  }
  if (failed.length > 0) {
    console.error(`\n${failed.length} of ${results.length} families failed: ${failed.map((f) => f.name).join(', ')}`);
    return 1;
  }
  return 0;
}

process.exitCode = await main();
