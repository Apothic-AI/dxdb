/**
 * Generates a migration fixture from **real upstream RxDB**, in a real browser.
 *
 * The audit item is an interop claim: "a store written by upstream RxDB (Dexie) can
 * be migrated into dxdb's equivalent storage and back without semantic loss". A
 * fixture we fabricate to look like a Dexie store would make that claim untestable
 * while making the audit item look green. So this script produces the fixture the
 * only way that means anything:
 *
 *  1. bundle the **real** `rxdb@17.6.0` + `getRxStorageDexie()` with esbuild (a
 *     bundler is the right tool here; a browser has no node module resolution, and
 *     hand-serving rxdb's transitive ESM graph through mingo and rxjs is exactly the
 *     job bundlers exist to do);
 *  2. run it in Chromium through Playwright, write documents with the public RxDB
 *     API;
 *  3. dump the **raw IndexedDB contents** — every object store Dexie created, with
 *     every record — plus the schema hash from RxDB's own `RxSchema`;
 *  4. write a fixture JSON that a Rust test loads.
 *
 * Nothing here imports our code. If our reader disagrees with this file, ours is
 * wrong.
 *
 * Usage:
 *   DEXFIX_MODULES=/tmp/dexfix/node_modules node scripts/make-dexie-fixture.mjs [out]
 */

import { chromium } from 'playwright-core';
import { createReadStream, existsSync, mkdirSync, statSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { dirname, extname, join, normalize, resolve, sep } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = resolve(process.argv[2] ?? join(HERE, '..', 'fixtures', 'dexie-store.json'));
const MODULES = resolve(process.env.DEXFIX_MODULES ?? '/tmp/dexfix/node_modules');
const WORK = resolve(process.env.DEXFIX_WORK ?? '/tmp/dexfix');
const BUNDLE = join(WORK, 'rxdb-bundle.mjs');

/** A string primary key, a number with `minimum`/`maximum`/`multipleOf` (the index
 *  encoder reads all three), one secondary index, and an index on a string whose
 *  values include astral-plane characters. */
const SCHEMA = {
  title: 'dexie migration probe',
  version: 0,
  primaryKey: 'passportId',
  type: 'object',
  properties: {
    passportId: { type: 'string', maxLength: 100 },
    firstName: { type: 'string', maxLength: 100 },
    age: { type: 'number', minimum: 0, maximum: 150, multipleOf: 1 },
  },
  // `age` must be required: Dexie only supports indexes on required fields (RxDB
  // DXE1, "non-required index fields are not possible with the dexie.js RxStorage").
  // This is a real constraint of the storage under test, not a preference.
  required: ['passportId', 'firstName', 'age'],
  indexes: ['firstName', 'age'],
};

/** Values chosen to catch formatting and ordering divergence: an astral-plane
 *  string, a schema maximum, a zero, and a plain one. */
const DOCUMENTS = [
  { passportId: 'h1', firstName: 'Alice', age: 30 },
  { passportId: 'h2', firstName: 'Bob 🦀', age: 41 },
  { passportId: 'h3', firstName: 'Ünïcodé 世界', age: 150 },
  { passportId: 'h4', firstName: 'Zero', age: 0 },
];

const MIME = { '.js': 'text/javascript', '.mjs': 'text/javascript' };

/** Bundles the fixture entry point once, so the page has a single module to load. */
function bundle() {
  if (existsSync(BUNDLE) && !process.env.DEXFIX_REBUNDLE) return;
  const esbuild = join(WORK, 'node_modules', '.bin', 'esbuild');
  if (!existsSync(esbuild)) {
    throw new Error(
      `esbuild not found at ${esbuild}; run: cd ${WORK} && npm install esbuild`,
    );
  }
  const entry = join(WORK, 'fixture-entry.js');
  if (!existsSync(entry)) {
    writeFileSync(
      entry,
      [
        "import * as rxdb from 'rxdb';",
        "import { getRxStorageDexie } from 'rxdb/plugins/storage-dexie';",
        'window.__rxdb = rxdb;',
        'window.__getRxStorageDexie = getRxStorageDexie;',
        '',
      ].join('\n'),
    );
  }
  execFileSync(
    esbuild,
    [entry, '--bundle', '--format=esm', '--platform=browser', `--outfile=${BUNDLE}`],
    { stdio: 'inherit' },
  );
}

/** The page the browser loads. Served over HTTP: an import map or module script
 *  injected with `setContent` does not run for module loads the page then starts. */
const PAGE = `<!doctype html>
<title>dexie fixture</title>
<script type="module">
  import '${'/rxdb-bundle.mjs'}';
  window.__ready = true;
</script>
<body></body>`;

async function serve() {
  const server = createServer((req, res) => {
    const url = new URL(req.url ?? '/', 'http://localhost');
    if (url.pathname === '/' || url.pathname === '/index.html') {
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
      res.end(PAGE);
      return;
    }
    const requested = normalize(join(WORK, decodeURIComponent(url.pathname)));
    const path = [requested, `${requested}.js`, join(requested, 'index.js')].find(
      (candidate) => existsSync(candidate) && statSync(candidate).isFile(),
    );
    if (!requested.startsWith(WORK + sep) || !path) {
      res.writeHead(404).end('not found');
      return;
    }
    res.writeHead(200, {
      'content-type': MIME[extname(path)] ?? 'application/octet-stream',
      'cache-control': 'no-store',
    });
    createReadStream(path).pipe(res);
  });
  return new Promise((resolveServer) => {
    server.listen(0, '127.0.0.1', () => resolveServer({ server, port: server.address().port }));
  });
}

async function main() {
  if (!existsSync(join(MODULES, 'rxdb'))) {
    throw new Error(`no rxdb install at ${MODULES}; set DEXFIX_MODULES`);
  }
  bundle();

  const { server, port } = await serve();
  const base = `http://127.0.0.1:${port}`;
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  const failed = [];
  page.on('pageerror', (error) => errors.push(String(error)));
  page.on('response', (response) => {
    if (response.status() === 404) failed.push(response.url());
  });

  await page.goto(`${base}/`);
  try {
    await page.waitForFunction(() => window.__ready === true, null, { timeout: 30_000 });
  } catch (error) {
    console.error('the RxDB bundle did not load');
    for (const url of [...new Set(failed)].slice(0, 10)) console.error('  404 ' + url);
    for (const message of errors.slice(0, 10)) console.error('  ' + message);
    throw error;
  }

  const result = await page.evaluate(async ({ schema, documents }) => {
    const {
      __rxdb: rxdb,
      __getRxStorageDexie: getRxStorageDexie,
      __defaultHashSha256: defaultHashSha256,
    } = window;

    const db = await rxdb.createRxDatabase({
      name: 'dexieprobe',
      storage: getRxStorageDexie(),
      multiInstance: false,
      // No `ignoreDuplicate`: DB9 forbids it outside dev-mode, and a fresh browser
      // context has no duplicate to ignore.
    });
    await db.addCollections({ humans: { schema } });
    await db.humans.bulkInsert(documents);
    await new Promise((r) => setTimeout(r, 300));

    const collection = db.humans;
    const schemaObject = collection.schema;
    const filledSchema = schemaObject.jsonSchema ?? null;

    // `RxSchema.hash` is a getter that hashes `JSON.stringify(this.jsonSchema)` with
    // the collection's hash function. Both the resolved hash and the exact bytes it
    // hashed are captured: the hash is the interop claim, and the bytes are what makes
    // a mismatch diagnosable (a different key order in the filled schema produces a
    // different hash even when the schemas are semantically equal).
    let schemaHash = null;
    let schemaHashError = null;
    let hashInput = null;
    try {
      hashInput = JSON.stringify(schemaObject.jsonSchema);
      // RxDB's `RxSchema.hash` getter calls an *async* hash function
      // (crypto.subtle.digest), so the getter yields a Promise and awaiting the
      // getter is not the same as awaiting the hash. `defaultHashSha256` is the
      // exact function upstream uses by default, so calling it directly on the same
      // input string reproduces upstream's number rather than approximating it.
      schemaHash = await defaultHashSha256(hashInput);
    } catch (error) {
      schemaHashError = String(error);
    }

    const documentsAsRxdbSeesThem = (await collection.find().exec())
      .map((doc) => doc.toJSON(true))
      .sort((a, b) => String(a.passportId).localeCompare(String(b.passportId)));

    const databases = [];
    for (const info of await indexedDB.databases()) {
      if (!info.name) continue;
      const handle = await new Promise((ok, bad) => {
        const request = indexedDB.open(info.name);
        request.onerror = () => bad(request.error);
        request.onsuccess = () => ok(request.result);
      });
      const storeNames = Array.from(handle.objectStoreNames);
      const stores = {};
      for (const storeName of storeNames) {
        const store = handle.transaction(storeName, 'readonly').objectStore(storeName);
        const [values, keys] = await Promise.all([
          new Promise((ok, bad) => {
            const request = store.getAll();
            request.onerror = () => bad(request.error);
            request.onsuccess = () => ok(request.result);
          }),
          new Promise((ok, bad) => {
            const request = store.getAllKeys();
            request.onerror = () => bad(request.error);
            request.onsuccess = () => ok(request.result);
          }),
        ]);
        stores[storeName] = values.map((value, index) => ({ key: keys[index], value }));
      }
      const version = handle.version;
      handle.close();
      databases.push({ name: info.name, version, storeNames, stores });
    }

    await db.close();
    return {
      generatedBy: 'upstream RxDB via getRxStorageDexie(), in Chromium',
      rxdbVersion: rxdb.RXDB_VERSION ?? null,
      schema,
      filledSchema,
      schemaHash: typeof schemaHash === 'string' ? schemaHash : null,
      schemaHashError,
      hashInput,
      documentsInput: documents,
      documentsAsRxdbSeesThem,
      databases,
    };
  }, { schema: SCHEMA, documents: DOCUMENTS });

  await browser.close();
  server.close();

  const real = errors.filter((e) => !/dev-mode/i.test(e));
  if (real.length) {
    console.error('page errors:');
    for (const message of real.slice(0, 10)) console.error('  ' + message);
    process.exitCode = 1;
  }

  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, `${JSON.stringify(result, null, 2)}\n`);
  console.log(`wrote ${OUT}`);
  console.log(`  rxdb ${result.rxdbVersion}`);
  console.log(`  schemaHash: ${JSON.stringify(result.schemaHash)}`);
  console.log(
    `  databases: ${result.databases
      .map((d) => `${d.name}@${d.version} [${d.storeNames.join(',')}]`)
      .join('; ')}`,
  );
  console.log(`  documents: ${result.documentsAsRxdbSeesThem.length}`);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
