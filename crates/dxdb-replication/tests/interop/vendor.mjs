/**
 * Provision real upstream RxDB for the interop test.
 *
 * This is the same technique the vector generator uses (`xtask/vectors/vendor.mjs`), and
 * for the same reasons — see that file for the long version. Short version:
 *
 *   1. `npm i rxdb@17.6.0` is not the pin. Five files in the import closure differ between
 *      the published tarball and the git commit, and they are exactly the ones whose
 *      replication behaviour this test exists to check.
 *   2. Upstream `src/` is TypeScript with `.ts` specifiers, which Node cannot resolve, so we
 *      bundle the sources with the pinned esbuild and run the bundle.
 *
 * The upstream checkout is reused from the vector cache when present, so this cost is paid
 * once for the whole repository.
 *
 * What this harness adds beyond the vector generator: `ws` and the
 * `replication-websocket` plugin, because the thing under test is a real upstream client
 * talking to a dxdb endpoint over a socket.
 */
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

/** The pin. Mirrors NOTICE.md; reported so a drift is visible rather than silent. */
export const PIN = {
  repo: 'https://github.com/pubkey/rxdb',
  commit: '3b763ee17d62bc96cfae556ea0ccf92f9891c8c1',
  version: '17.6.0',
};

const cacheRoot =
  process.env.DXDB_VECTORS_CACHE || join(homedir(), '.cache', 'dxdb-vectors');
const checkoutDir = join(cacheRoot, 'upstream-' + PIN.commit.slice(0, 12));

/**
 * The upstream modules this harness needs.
 *
 * Deliberately narrow. `replicateRxCollection` is the real driver, not the protocol layer:
 * the test must exercise upstream's *client*, including its checkpoint handling, because
 * that is what a JavaScripe peer in production runs.
 */
const MODULES = {
  rxDatabase: 'rx-database.ts',
  rxStorage: 'plugins/storage-memory/index.ts',
  replication: 'plugins/replication/index.ts',
  websocket: 'plugins/replication-websocket/index.ts',
  utils: 'plugins/utils/index.ts',
  // Upstream's own websocket test imports its collections from here, and the module is why
  // it works: `plugins/test-utils/index` pulls in the plugin registration every RxDB feature
  // depends on. Creating a collection without it leaves `collection.onClose` undefined, which
  // is the failure that led to this import.
  testUtils: 'plugins/test-utils/index.ts',
  // Dev mode turns the anonymised RxError messages into real ones. Without it every failure
  // is "Error messages are not included in RxDB core to reduce build size", which is useless
  // when the point of the harness is to diagnose a disagreement.
  devMode: 'plugins/dev-mode/index.ts',
  plugin: 'plugin.ts',
  // Named directly: the replication-protocol barrel re-exports meta-instance, but esbuild
  // tree-shakes it away because nothing else in the entry reaches it.
  metaInstance: 'replication-protocol/meta-instance.ts',
};

function log(message) {
  process.stderr.write(`[interop] ${message}\n`);
}

function run(cmd, args, cwd) {
  return execFileSync(cmd, args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
}

/** Clone only the pinned commit, verifying the checkout is the pin and clean. */
function ensureCheckout() {
  if (existsSync(join(checkoutDir, '.git'))) {
    const head = run('git', ['rev-parse', 'HEAD'], checkoutDir).trim();
    const dirty = run('git', ['status', '--porcelain'], checkoutDir).trim();
    if (head === PIN.commit && dirty === '') {
      return checkoutDir;
    }
    log(`cached checkout is ${head}${dirty ? ' (dirty)' : ''}; refetching`);
    rmSync(checkoutDir, { recursive: true, force: true });
  }
  mkdirSync(dirname(checkoutDir), { recursive: true });
  log(`fetching ${PIN.repo} at ${PIN.commit.slice(0, 12)} (one-time, ~20s)`);
  run('git', ['init', '--quiet', checkoutDir], dirname(checkoutDir));
  run('git', ['remote', 'add', 'origin', PIN.repo], checkoutDir);
  run('git', ['fetch', '--depth', '1', '--filter=blob:none', 'origin', PIN.commit], checkoutDir);
  run('git', ['checkout', '--detach', '--quiet', 'FETCH_HEAD'], checkoutDir);
  const head = run('git', ['rev-parse', 'HEAD'], checkoutDir).trim();
  if (head !== PIN.commit) {
    throw new Error(`checkout landed on ${head}, expected ${PIN.commit}`);
  }
  const dirty = run('git', ['status', '--porcelain'], checkoutDir).trim();
  if (dirty !== '') {
    throw new Error(`freshly fetched checkout is modified:\n${dirty}`);
  }
  return checkoutDir;
}

function depsFingerprint() {
  return createHash('sha256')
    .update(readFileSync(join(here, 'package.json')))
    .digest('hex')
    .slice(0, 12);
}

async function bundle(upstreamDir) {
  const esbuild = await import(join(here, 'node_modules', 'esbuild', 'lib', 'main.js'));
  const outFile = join(cacheRoot, `interop-${PIN.commit.slice(0, 12)}-${depsFingerprint()}.mjs`);
  if (existsSync(outFile)) {
    return outFile;
  }

  const entryFile = join(cacheRoot, 'interop-entry.mjs');
  const imports = Object.entries(MODULES)
    .map(([name, rel]) => `import * as ${name} from ${JSON.stringify(join(upstreamDir, 'src', rel))};`)
    .join('\n');
  writeFileSync(entryFile, `${imports}\nexport { ${Object.keys(MODULES).join(', ')} };\n`);

  log('bundling upstream src with esbuild');
  await esbuild.build({
    entryPoints: [entryFile],
    bundle: true,
    platform: 'node',
    format: 'esm',
    target: 'node22',
    outfile: outFile,
    logLevel: 'warning',
    metafile: true,
    // Upstream's bare specifiers resolve against this harness's pinned deps.
    nodePaths: [join(here, 'node_modules')],
    // Some upstream deps ship CJS; the bundle is ESM so give it a require.
    banner: {
      js: "import { createRequire as __dxdbRequire } from 'node:module';\nconst require = __dxdbRequire(import.meta.url);",
    },
    // Bundle `ws` too. The output lives in the shared cache directory, not next to this
    // harness's `node_modules`, so an external `ws` would be unresolvable at import time.
    // Bundling is also the more honest provenance: the frame implementation is then part of
    // the recorded bundle rather than whatever happened to be on disk.
  });
  return outFile;
}

let cached;
/**
 * Loads the bundled upstream RxDB plus provenance.
 *
 * Cached per process: the second call is free, which matters because the test harness
 * creates several collections.
 */
export async function loadUpstream() {
  if (cached) {
    return cached;
  }
  const upstreamDir = ensureCheckout();
  const bundlePath = await bundle(upstreamDir);
  const namespace = await import(bundlePath);
  cached = {
    ...namespace,
    provenance: {
      rxdbVersion: PIN.version,
      rxdbCommit: PIN.commit,
      node: process.version,
      bundlePath,
    },
  };
  return cached;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const upstream = await loadUpstream();
  process.stdout.write(JSON.stringify(upstream.provenance, null, 2) + '\n');
  process.stdout.write(
    `exports: ${Object.keys(upstream).filter((k) => k !== 'provenance').join(', ')}\n`
  );
}
