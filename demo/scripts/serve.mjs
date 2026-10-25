/**
 * Static file server for the harness page.
 *
 * Small on purpose — three behaviours are load-bearing and a generic dev server
 * does not guarantee any of them:
 *
 *  1. `.wasm` is served as `application/wasm`. Without it, `WebAssembly.
 *     instantiateStreaming` refuses the response, and the failure reads like a
 *     wasm bug rather than a header bug.
 *  2. `.js` is served as `text/javascript`, because ES module loading is
 *     strict about the MIME type in all three engines.
 *  3. No caching. The harness rebuilds `pkg/`, and a cached module would make a
 *     green run meaningless.
 *
 * Usage: node scripts/serve.mjs [port]
 */

import { createServer } from 'node:http';
import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import { extname, join, normalize, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const DEMO_DIR = resolve(fileURLToPath(new URL('..', import.meta.url)));
const PORT = Number(process.argv[2] ?? process.env.DXDB_DEMO_PORT ?? 4173);

const MIME = new Map(
  Object.entries({
    '.html': 'text/html; charset=utf-8',
    '.js': 'text/javascript; charset=utf-8',
    '.mjs': 'text/javascript; charset=utf-8',
    '.json': 'application/json; charset=utf-8',
    '.wasm': 'application/wasm',
    '.css': 'text/css; charset=utf-8',
    '.map': 'application/json; charset=utf-8',
    // The hand-written declarations; served so a real TS app can be built
    // against the demo's own output.
    '.d.ts': 'text/plain; charset=utf-8',
    '.ts': 'text/plain; charset=utf-8',
  }),
);

function mimeFor(path) {
  // `.d.ts` must be checked before `.ts`.
  if (path.endsWith('.d.ts')) return MIME.get('.d.ts');
  return MIME.get(extname(path)) ?? 'application/octet-stream';
}

const server = createServer(async (req, res) => {
  try {
    const url = new URL(req.url ?? '/', 'http://localhost');
    let pathname = decodeURIComponent(url.pathname);
    if (pathname.endsWith('/')) pathname += 'index.html';

    // Refuse anything that escapes the demo directory. `normalize` collapses
    // `..`, and the prefix check catches the rest.
    const filePath = normalize(join(DEMO_DIR, pathname));
    if (!filePath.startsWith(DEMO_DIR + sep) && filePath !== DEMO_DIR) {
      res.writeHead(403, { 'content-type': 'text/plain' }).end('forbidden');
      return;
    }

    const info = await stat(filePath).catch(() => null);
    if (!info || !info.isFile()) {
      res.writeHead(404, { 'content-type': 'text/plain' }).end('not found');
      return;
    }

    res.writeHead(200, {
      'content-type': mimeFor(filePath),
      'content-length': info.size,
      'cache-control': 'no-store',
    });
    createReadStream(filePath).pipe(res);
  } catch (err) {
    res.writeHead(500, { 'content-type': 'text/plain' }).end(String(err));
  }
});

server.listen(PORT, '127.0.0.1', () => {
  // Playwright's webServer waits on this line only indirectly (it polls the
  // port), but a human running `npm run serve` needs the URL.
  console.log(`dxdb demo harness: http://127.0.0.1:${PORT}/`);
});

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
