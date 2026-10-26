/**
 * Provision the upstream RxDB oracle used by every vector family.
 *
 * The brief says the vectors must come from "real upstream RxDB in Node", at the
 * pin recorded in NOTICE.md. Two traps make that harder than `npm i rxdb`:
 *
 *   1. The published tarball's `src/` is NOT the pinned commit. At 17.6.0 five
 *      files in our import closure differ from the git pin (`rx-query-mingo.ts`,
 *      `rx-query-helper.ts`, `rx-storage-helper.ts`, `plugins/attachments/index.ts`,
 *      `plugins/storage-memory/rx-storage-instance-memory.ts`). Those are exactly
 *      the files whose behaviour these vectors are supposed to pin, so the npm
 *      copy is not an acceptable oracle. We fetch the commit itself.
 *   2. Upstream `src/` imports TypeScript with `.ts` specifiers and only the
 *      built `dist/` is resolvable by Node. The dist is a Babel/TS build of the
 *      same source, but the source is what we are porting and what the vectors
 *      must describe, so we bundle the `.ts` sources and run that.
 *
 * So: fetch the pinned commit into a cache directory *outside the repo* (a 58 MB
 * checkout inside the tree would be rsynced to the build box by
 * cargo-remote-3000 on every `make check`), bundle the modules we need with the
 * pinned esbuild, and record provenance next to the bundle.
 *
 * The cache makes this idempotent: a second run verifies the checkout's HEAD and
 * reuses the bundle unless `package-lock.json` changed.
 *
 *   node xtask/vectors/vendor.mjs          # provision now (the dispatcher does this too)
 *   node xtask/vectors/vendor.mjs --print  # print the provenance as JSON
 */
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

/** The pin. Mirrors NOTICE.md; `--print` reports it so a drift is visible. */
export const PIN = {
  repo: 'https://github.com/pubkey/rxdb',
  commit: '3b763ee17d62bc96cfae556ea0ccf92f9891c8c1',
  version: '17.6.0',
};

export const cacheRoot =
  process.env.DXDB_VECTORS_CACHE || join(homedir(), '.cache', 'dxdb-vectors');

const checkoutDir = join(cacheRoot, 'upstream-' + PIN.commit.slice(0, 12));

/** Modules every family may import. Keep this list small: it is the oracle surface. */
const ORACLE_MODULES = {
  customIndex: 'custom-index.ts',
  queryPlanner: 'query-planner.ts',
  queryHelper: 'rx-query-helper.ts',
  schemaHelper: 'rx-schema-helper.ts',
  rxSchema: 'rx-schema.ts',
  storageHelper: 'rx-storage-helper.ts',
  replication: 'replication-protocol/index.ts',
  memory: 'plugins/storage-memory/index.ts',
  fixtures: 'plugins/test-utils/schemas.ts',
  schemaObjects: 'plugins/test-utils/schema-objects.ts',
  // Named explicitly: the utils barrel only reaches this module through
  // tree-shaken re-exports, so importing it through `schemaHelper` yields
  // undefined. Schema hashing must come from upstream, not from our copy.
  utilsHash: 'plugins/utils/utils-hash.ts',
};

function log(msg) {
  process.stderr.write(`[vendor] ${msg}\n`);
}

/**
 * The pin above is only meaningful if it is the same pin the project documents.
 *
 * `NOTICE.md` is the root of truth: it is the statement of which upstream this work
 * derives from, and it is the thing a human reads. `PIN` here is what the generator
 * actually fetches. If those two disagree, every vector in the tree silently describes
 * a commit nobody wrote down, and consumers keep passing against an oracle that moved.
 *
 * Checked at provisioning time rather than only in CI, because the generator is where
 * a new pin would be introduced. `crates/dxdb-vectors/tests/provenance_pin.rs` checks
 * the same NOTICE text against the crate constant and the committed files, so all
 * three legs of the triangle are covered and none of them can be updated alone.
 */
export function noticePin(noticePath = join(here, '..', '..', 'NOTICE')) {
  let text;
  try {
    text = readFileSync(noticePath, 'utf8');
  } catch (err) {
    throw new Error(
      `cannot read ${noticePath}: ${err.message}\n` +
        'NOTICE.md documents the upstream pin; the generator cannot verify what it fetches without it.'
    );
  }
  const commit = text
    .split(/\s+/)
    .find((word) => word.length === 40 && /^[0-9a-f]+$/.test(word));
  const versionMatch = text.match(/version\s+(\d+\.\d+\.\d+)/);
  if (!commit || !versionMatch) {
    throw new Error(
      `could not parse the upstream pin out of ${noticePath}; ` +
        'the provenance checks read this file and need updating if its wording changed'
    );
  }
  return { commit, version: versionMatch[1] };
}

export function assertPinMatchesNotice() {
  const documented = noticePin();
  if (documented.commit !== PIN.commit || documented.version !== PIN.version) {
    throw new Error(
      `the generator pin does not match NOTICE.md:\n` +
        `  vendor.mjs PIN : ${PIN.commit} (${PIN.version})\n` +
        `  NOTICE.md      : ${documented.commit} (${documented.version})\n` +
        'Update NOTICE.md, PIN here, and Provenance::EXPECTED_RXDB_COMMIT in ' +
        'crates/dxdb-vectors in the same commit, then regenerate with `make vectors`.'
    );
  }
  return documented;
}

function run(cmd, args, cwd) {
  return execFileSync(cmd, args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
}

/**
 * Clone only the pinned commit. `--filter=blob:none --no-checkout` first keeps
 * the transfer to the commit we need instead of the full history.
 *
 * A cached checkout is verified twice: HEAD must be the pin, and the work tree
 * must be clean. The second check matters because these files are the oracle. An
 * edit to a cached checkout would leave HEAD at the pin and silently change every
 * vector generated afterwards, which is precisely the failure the pin exists to
 * prevent.
 */
function ensureCheckout() {
  if (existsSync(join(checkoutDir, '.git'))) {
    const head = run('git', ['rev-parse', 'HEAD'], checkoutDir).trim();
    const dirty = run('git', ['status', '--porcelain'], checkoutDir).trim();
    if (head === PIN.commit && dirty === '') {
      return { dir: checkoutDir, fetched: false };
    }
    if (head === PIN.commit && dirty !== '') {
      log('cached upstream checkout has local modifications; discarding it');
    } else {
      log(`cached checkout is at ${head}, expected ${PIN.commit}; refetching`);
    }
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
    throw new Error(`upstream checkout landed on ${head}, expected the pin ${PIN.commit}`);
  }
  const dirty = run('git', ['status', '--porcelain'], checkoutDir).trim();
  if (dirty !== '') {
    throw new Error(`freshly fetched upstream checkout is unexpectedly modified:\n${dirty}`);
  }
  return { dir: checkoutDir, fetched: true };
}

/** The upstream generator deps are pinned in package.json; install them on demand. */
function ensureNodeModules() {
  const marker = join(here, 'package-lock.json');
  const stamp = join(here, 'node_modules', '.dxdb-vectors-stamp');
  const lockHash = createHash('sha256').update(readFileSync(marker)).digest('hex');
  if (existsSync(stamp) && readFileSync(stamp, 'utf8').trim() === lockHash) {
    return false;
  }
  log('installing pinned generator dependencies (npm ci)');
  execFileSync('npm', ['ci', '--no-audit', '--no-fund', '--loglevel=error'], {
    cwd: here,
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  writeFileSync(stamp, lockHash + '\n');
  return true;
}

function depsFingerprint() {
  return createHash('sha256')
    .update(readFileSync(join(here, 'package-lock.json')))
    .digest('hex')
    .slice(0, 12);
}

async function bundleOracle(upstreamDir) {
  const esbuild = await import(join(here, 'node_modules', 'esbuild', 'lib', 'main.js'));
  const outFile = join(cacheRoot, `oracle-${PIN.commit.slice(0, 12)}-${depsFingerprint()}.mjs`);
  if (existsSync(outFile)) {
    return { bundlePath: outFile, built: false };
  }
  const entryFile = join(cacheRoot, 'oracle-entry.mjs');
  const imports = Object.entries(ORACLE_MODULES)
    .map(([name, rel]) => `import * as ${name} from ${JSON.stringify(join(upstreamDir, 'src', rel))};`)
    .join('\n');
  const exports = Object.keys(ORACLE_MODULES).join(', ');
  writeFileSync(entryFile, `${imports}\nexport { ${exports} };\n`);

  log('bundling upstream src with esbuild');
  const result = await esbuild.build({
    entryPoints: [entryFile],
    bundle: true,
    platform: 'node',
    format: 'esm',
    target: 'node22',
    outfile: outFile,
    logLevel: 'warning',
    metafile: true,
    // Bare specifiers inside upstream src resolve against our pinned deps.
    nodePaths: [join(here, 'node_modules')],
    // mingo/rxjs ship CJS in places; the bundle is ESM so give it a require.
    banner: {
      js: "import { createRequire as __dxdbCreateRequire } from 'node:module';\nconst require = __dxdbCreateRequire(import.meta.url);",
    },
  });

  // Record which upstream files actually ended up in the bundle. This is the
  // "which code produced these vectors" answer, and it is what makes a silent
  // version swap visible.
  // esbuild reports inputs relative to the output directory ("../../../.cache/..."),
  // so key off the checkout directory name and slice off everything up to `src/`.
  const checkoutMarker = '/src/';
  const checkoutName = upstreamDir.split('/').pop();
  const upstreamInputs = Object.keys(result.metafile.inputs)
    .filter((p) => p.includes('/' + checkoutName + '/src/'))
    .map((p) => p.slice(p.lastIndexOf(checkoutMarker) + checkoutMarker.length))
    .sort();
  writeFileSync(
    outFile + '.inputs.json',
    JSON.stringify({ upstreamInputs, inputs: Object.keys(result.metafile.inputs).length }, null, 2) + '\n'
  );
  return { bundlePath: outFile, built: true };
}

/**
 * Provision everything and return the module namespace plus provenance.
 * Cached: repeated calls in one process are free.
 */
let cached;
export async function loadOracle() {
  if (cached) {
    return cached;
  }
  // Verify the pin we are about to fetch is the one the project documents.
  assertPinMatchesNotice();
  const installed = ensureNodeModules();
  const { dir, fetched } = ensureCheckout();
  const { bundlePath, built } = await bundleOracle(dir);

  const deps = JSON.parse(readFileSync(join(here, 'package.json'), 'utf8'));
  const provenance = {
    rxdbVersion: PIN.version,
    rxdbCommit: PIN.commit,
    rxdbRepo: PIN.repo,
    // Informational only; --check ignores it so a Node upgrade is not "drift".
    node: process.version,
    generatorDeps: {
      ...(deps.dependencies || {}),
      ...(deps.devDependencies || {}),
    },
  };
  const bundleInputs = JSON.parse(readFileSync(bundlePath + '.inputs.json', 'utf8'));

  if (process.env.DXDB_VECTORS_VERBOSE) {
    log(`checkout ${dir} (${fetched ? 'fetched' : 'cached'})`);
    log(`bundle ${bundlePath} (${built ? 'built' : 'cached'})`);
    log(`deps installed this run: ${installed}`);
  }

  const modules = await import(bundlePath);
  cached = {
    provenance,
    modules,
    bundlePath,
    bundleInputs,
    /**
     * Every vector file carries this header. `--check` compares everything
     * except `node`, so an upgrade that changes nothing is not a failure.
     */
    header(comment) {
      return {
        $comment: comment,
        generator: 'xtask/vectors/generate.mjs',
        rxdbVersion: provenance.rxdbVersion,
        rxdbCommit: provenance.rxdbCommit,
        node: provenance.node,
      };
    },
  };
  return cached;
}

export function printProvenance() {
  const { provenance, bundleInputs } = cached || {};
  process.stdout.write(
    JSON.stringify({ pin: PIN, provenance, ...(bundleInputs ? { bundle: bundleInputs } : {}) }, null, 2) + '\n'
  );
}

if (process.argv[1] && process.argv[1] === fileURLToPath(import.meta.url)) {
  const oracle = await loadOracle();
  process.stdout.write(
    JSON.stringify(
      { pin: PIN, provenance: oracle.provenance, bundle: oracle.bundleInputs, bundlePath: oracle.bundlePath },
      null,
      2
    ) + '\n'
  );
}
