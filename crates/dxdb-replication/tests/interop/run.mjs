#!/usr/bin/env node
/**
 * Runs both interop directions and exits non-zero if either fails.
 *
 * Hermetic: every server binds `127.0.0.1:0`, so ports are ephemeral and nothing leaves the
 * machine. Upstream RxDB is provisioned from the pin by `vendor.mjs`, never from a checked-in
 * fixture.
 *
 *   node crates/dxdb-replication/tests/interop/run.mjs
 */
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

const SUITES = [
  {
    file: 'client-upstream.mjs',
    title: 'upstream client -> dxdb endpoint',
    expect: (report) =>
      report.convergenceDxdbToUpstream &&
      report.convergenceUpstreamToDxdb &&
      report.checkpointEquality &&
      Array.isArray(report.restartResends) &&
      report.restartResends.length === 0,
  },
  {
    file: 'client-dxdb.mjs',
    title: 'dxdb client -> upstream server',
    expect: (report) => report.pullWorked && report.pushWorked && report.conflictsOnly,
  },
];

let failed = false;
for (const suite of SUITES) {
  process.stdout.write(`\n=== ${suite.title} ===\n`);
  let output;
  try {
    output = execFileSync(process.execPath, [join(here, suite.file)], {
      cwd: here,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'inherit'],
      timeout: 120_000,
    });
  } catch (error) {
    process.stdout.write(String(error.stdout ?? '') + '\n');
    process.stderr.write(`${suite.title}: process failed\n`);
    failed = true;
    continue;
  }
  let report;
  try {
    report = JSON.parse(output);
  } catch {
    process.stdout.write(output + '\n');
    process.stderr.write(`${suite.title}: output was not JSON\n`);
    failed = true;
    continue;
  }
  process.stdout.write(JSON.stringify(report, null, 2) + '\n');
  if (!suite.expect(report)) {
    process.stderr.write(`${suite.title}: assertions did not hold\n`);
    failed = true;
  }
}

if (failed) {
  process.stderr.write('\ninterop: FAILED\n');
  process.exitCode = 1;
} else {
  process.stderr.write('\ninterop: both directions passed\n');
}
