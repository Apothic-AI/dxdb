#!/usr/bin/env node
/**
 * Report whether the oracle is still describing current upstream.
 *
 * The vector families are pinned to one upstream commit (`NOTICE.md`, and `PIN` in
 * `vendor.mjs`). Nothing in the repo notices when upstream moves on, so the oracle can
 * describe behaviour that no longer exists and the only way anyone finds out is by
 * accident. This task answers the question directly: what has upstream done since the
 * pin, and did any of it touch a file that feeds a family?
 *
 * It **does not** regenerate anything and it does not move the pin. Re-pinning is a
 * deliberate decision with its own review (regenerate every family, re-verify every
 * consumer); this report only supplies the evidence for that decision.
 *
 * The definition of "feeds a family" matters, so it is not a glob over `src/`:
 *
 *  - the eleven modules `vendor.mjs` actually bundles (`ORACLE_MODULES`), plus
 *  - everything those modules import, transitively, at the pinned commit.
 *
 * The second half is why this uses the bundled closure rather than the entry list: a
 * change to `rx-error.ts` or `utils-time.ts` alters family output without appearing in
 * `ORACLE_MODULES`. `vendor.mjs` already records that closure (`.inputs.json` next to
 * the bundle), and this reads it, so the report and the generator cannot disagree about
 * what the oracle contains.
 *
 *   node xtask/vectors/upstream-drift.mjs           # human-readable report
 *   node xtask/vectors/upstream-drift.mjs --json     # machine-readable
 *   node xtask/vectors/upstream-drift.mjs --strict   # exit 1 if any oracle file changed
 *
 * `--strict` is deliberately not wired into `make check`: the pin is expected to lag and
 * a red gate for "upstream moved" would train everyone to ignore it. It exists for when
 * someone wants to assert freshness explicitly, for example before a release.
 */
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, '..', '..');

const PIN = '3b763ee17d62bc96cfae556ea0ccf92f9891c8c1';
const UPSTREAM = 'https://github.com/pubkey/rxdb';

/** A blobless mirror of upstream, kept outside the repo: it is a report input, not code. */
const mirror =
  process.env.DXDB_UPSTREAM_MIRROR || join(process.env.HOME || '/tmp', '.cache', 'dxdb-vectors', 'upstream-history.git');

function log(msg) {
  process.stderr.write(`[upstream-drift] ${msg}\n`);
}

function git(args, cwd = mirror) {
  return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
}

/**
 * Fetch upstream history if the mirror is absent or stale.
 *
 * `--filter=blob:none` keeps this to ~21 MB for the whole history: the report needs
 * commit metadata and path lists, never file contents. The pin itself is fetched into
 * the existing checkout cache by `vendor.mjs`, so this only has to cover the graph.
 */
function ensureMirror({ refresh }) {
  if (!existsSync(join(mirror, 'HEAD'))) {
    log(`cloning ${UPSTREAM} history (blobless, one-time)`);
    execFileSync('git', ['clone', '--bare', '--filter=blob:none', '--quiet', UPSTREAM, mirror], {
      stdio: ['ignore', 'ignore', 'inherit'],
    });
  } else if (refresh) {
    log('refreshing upstream history');
    git(['fetch', '--quiet', '--filter=blob:none', 'origin', '+refs/heads/*:refs/heads/*', '+refs/tags/*:refs/tags/*']);
  }
  // The pin must be present before anything else can be asked of the graph.
  try {
    git(['cat-file', '-e', PIN + '^{commit}']);
  } catch {
    log(`pin ${PIN.slice(0, 12)} not in mirror yet; fetching it`);
    git(['fetch', '--quiet', '--filter=blob:none', 'origin', PIN]);
  }
}

/**
 * Every upstream file the oracle depends on: the explicit entry modules plus their
 * transitive import closure at the pin.
 *
 * Read from the bundle metadata `vendor.mjs` writes, so this cannot drift from what the
 * generator actually bundled. Falls back to the entry list alone, with a warning, if no
 * bundle has been built yet — better a partial report than none, but it is said out loud.
 */
function oracleSurface() {
  // `vendor.mjs` does not export its module map, so parse the entry list out of it: the
  // file is the single source of truth and duplicating it here would let the two drift.
  const vendorSrc = readFileSync(join(here, 'vendor.mjs'), 'utf8');
  const entryBlock = vendorSrc.slice(
    vendorSrc.indexOf('const ORACLE_MODULES = {'),
    vendorSrc.indexOf('};', vendorSrc.indexOf('const ORACLE_MODULES = {'))
  );
  const entries = [...entryBlock.matchAll(/:\s*'([^']+\.ts)'/g)].map((m) =>
    // Entries are relative to `src/`.
    m[1]
  );

  // The transitive closure from the most recent bundle, if one exists.
  let closure = [];
  const cacheRoot = join(process.env.HOME || '/tmp', '.cache', 'dxdb-vectors');
  if (existsSync(cacheRoot)) {
    // Newest `*.inputs.json`, chosen in JS rather than by shelling out to `ls -t`:
    // the shell call interpolated a path into a command string, which breaks on a
    // cache path containing spaces or shell metacharacters, and `readdirSync` also
    // avoids depending on `ls`'s output format.
    const candidates = readdirSync(cacheRoot)
      .filter((name) => name.startsWith('oracle-') && name.endsWith('.inputs.json'))
      .map((name) => ({ name, mtime: statSync(join(cacheRoot, name)).mtimeMs }))
      .sort((a, b) => b.mtime - a.mtime)
      .map((entry) => join(cacheRoot, entry.name))[0];
    if (candidates) {
      try {
        const meta = JSON.parse(readFileSync(candidates, 'utf8'));
        closure = meta.upstreamInputs || [];
      } catch (err) {
        log(`could not read ${candidates}: ${err.message}`);
      }
    }
  }
  if (closure.length === 0) {
    log(
      'WARNING: no bundle metadata found, so only the entry modules are checked. Run ' +
        '`node xtask/vectors/vendor.mjs` first for the full import closure.'
    );
    closure = entries;
  } else {
    // The closure is authoritative, but assert the entries are in it: if an entry is
    // missing, the bundle is stale relative to `vendor.mjs`.
    const missing = entries.filter((e) => !closure.includes(e));
    if (missing.length > 0) {
      log(
        `WARNING: bundle metadata does not include ${missing.join(', ')}; it is stale ` +
          'relative to vendor.mjs. The report may understate the oracle surface.'
      );
    }
  }
  return { entries: [...new Set(entries)].sort(), closure: [...new Set(closure)].sort() };
}

/** Commits after the pin that touched any file in `surface`. */
function commitsTouching(surface, since) {
  if (surface.length === 0) return [];
  // `--no-renames` so a file moved out of the oracle surface still shows as a change to
  // both paths, rather than a rename that looks like a deletion plus an unrelated add.
  const out = git([
    'log',
    '--no-renames',
    '--name-only',
    '--format=__C__%H%x09%aI%x09%s',
    `${since}..HEAD`,
    '--',
    ...surface.map((p) => `src/${p}`),
  ]);
  if (!out) return [];

  const commits = [];
  let current = null;
  for (const line of out.split('\n')) {
    if (line.startsWith('__C__')) {
      const [hash, date, subject] = line.slice(5).split('\t');
      current = { hash, date, subject, files: [] };
      commits.push(current);
    } else if (line.trim() && current) {
      current.files.push(line.trim().replace(/^src\//, ''));
    }
  }
  return commits;
}

/** Tags (releases) between `since` and HEAD, oldest first, with their oracle-surface churn. */
function releasesSince(surface, since) {
  const tagList = git(['tag', '--sort=v:refname', '--merged', 'HEAD']).split('\n').filter(Boolean);
  const after = [];
  for (const tag of tagList) {
    const tagCommit = git(['rev-list', '-n', '1', tag]);
    // Reachable from the pin's future? `merge-base --is-ancestor` answers whether the tag
    // is at or before the pin, which is exactly the cut.
    let isAfter = true;
    try {
      execFileSync('git', ['merge-base', '--is-ancestor', PIN, tagCommit], {
        cwd: mirror,
        stdio: 'ignore',
      });
      // Pin is an ancestor of the tag => the tag is at or after the pin.
      isAfter = tagCommit !== PIN;
    } catch {
      isAfter = false;
    }
    if (!isAfter) continue;

    const changed = surface.filter((p) => {
      try {
        execFileSync('git', ['diff', '--quiet', PIN, tagCommit, '--', `src/${p}`], {
          cwd: mirror,
          stdio: 'ignore',
        });
        return false; // identical
      } catch {
        return true; // differs
      }
    });
    after.push({ tag, commit: tagCommit, oracleFilesChanged: changed });
  }
  return after;
}

async function main() {
  const args = process.argv.slice(2);
  const asJson = args.includes('--json');
  const strict = args.includes('--strict');
  const refresh = !args.includes('--no-refresh');

  ensureMirror({ refresh });

  if (git(['rev-parse', '--is-shallow-repository']) === 'true') {
    throw new Error('the mirror is shallow; the commit graph since the pin would be wrong');
  }

  const head = git(['rev-parse', 'HEAD']);
  const headDate = git(['log', '-1', '--format=%aI', 'HEAD']);
  const pinDate = git(['log', '-1', '--format=%aI', PIN]);
  const pinVersion = JSON.parse(git(['show', `${PIN}:package.json`])).version;
  const headVersion = JSON.parse(git(['show', `HEAD:package.json`])).version;

  const surface = oracleSurface();
  const commitsSince = git(['rev-list', '--count', `${PIN}..HEAD`]);
  const touching = commitsTouching(surface.closure, PIN);
  const releases = releasesSince(surface.closure, PIN);

  const changedFiles = [...new Set(touching.flatMap((c) => c.files))].sort();

  const report = {
    upstream: UPSTREAM,
    pin: { commit: PIN, date: pinDate, version: pinVersion },
    head: { commit: head, date: headDate, version: headVersion },
    commitsSincePin: Number(commitsSince),
    oracleSurface: {
      entryModules: surface.entries.length,
      totalFiles: surface.closure.length,
    },
    verdict:
      changedFiles.length === 0
        ? 'fresh'
        : 'stale',
    changedOracleFiles: changedFiles,
    commitsTouchingOracle: touching,
    releasesSincePin: releases,
  };

  if (asJson) {
    process.stdout.write(JSON.stringify(report, null, 2) + '\n');
  } else {
    const lines = [];
    lines.push(`upstream      ${UPSTREAM}`);
    lines.push(`pin           ${PIN.slice(0, 12)}  ${pinDate.slice(0, 10)}  v${pinVersion}`);
    lines.push(`upstream HEAD ${head.slice(0, 12)}  ${headDate.slice(0, 10)}  v${headVersion}`);
    lines.push(`commits since pin: ${commitsSince}`);
    lines.push(
      `oracle surface: ${surface.entries.length} entry modules, ${surface.closure.length} files in their import closure`
    );
    lines.push('');

    if (changedFiles.length === 0) {
      lines.push(`VERDICT: fresh — no upstream file the oracle reads has changed since the pin.`);
    } else {
      lines.push(
        `VERDICT: stale — ${changedFiles.length} oracle file(s) changed in ${touching.length} commit(s):`
      );
      for (const file of changedFiles) lines.push(`  ${file}`);
      lines.push('');
      lines.push('Commits:');
      for (const c of touching) {
        lines.push(`  ${c.hash.slice(0, 12)}  ${c.date.slice(0, 10)}  ${c.subject}`);
        for (const f of c.files) lines.push(`      ${f}`);
      }
    }

    lines.push('');
    if (releases.length === 0) {
      lines.push('Releases since the pin: none (no tag on this branch is newer than the pin).');
    } else {
      lines.push('Releases since the pin:');
      for (const r of releases) {
        lines.push(
          `  ${r.tag.padEnd(10)} ${r.commit.slice(0, 12)}  ${
            r.oracleFilesChanged.length === 0
              ? 'no oracle files changed'
              : `${r.oracleFilesChanged.length} oracle file(s) changed`
          }`
        );
        for (const f of r.oracleFilesChanged) lines.push(`      ${f}`);
      }
    }

    process.stdout.write(lines.join('\n') + '\n');
  }

  if (strict && report.verdict !== 'fresh') {
    process.stderr.write(
      `\nupstream-drift: --strict and the oracle is stale.\n` +
        `Re-pinning is a deliberate decision: it means regenerating every family and ` +
        `re-verifying every consumer. See docs/PLAN.md section 7.2.\n`
    );
    return 1;
  }
  return 0;
}

process.exitCode = await main();
