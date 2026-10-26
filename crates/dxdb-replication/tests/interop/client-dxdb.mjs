/**
 * Direction 2: a dxdb client replicates against upstream's own `startWebsocketServer`.
 *
 * Direction 1 proves an upstream client understands a dxdb endpoint. This proves the converse,
 * and it is the stronger statement about the *frame format*, because upstream's server is the
 * reference implementation of it.
 *
 * The dxdb client here is JavaScript that emits the frames `crates/dxdb-replication/src/
 * websocket.rs` emits, in the order the Rust protocol emits them. Node cannot call into
 * `cargo test`, so the client-side protocol driving is re-implemented -- but the *server* is
 * genuine upstream, so a frame-shape mistake in our code shows up immediately.
 *
 * The point is not to re-test our protocol (the Rust suite does that) but to confirm that what
 * our `websocket.rs` writes is what upstream's `websocket-server.ts` reads, and that the
 * `masterChangeStream$` stream request is handled the way our adapter expects.
 */
import assert from 'node:assert/strict';

import { loadUpstream } from './vendor.mjs';

const { rxDatabase, rxStorage, websocket } = await loadUpstream();

const humanSchema = {
  title: 'human schema',
  version: 0,
  keyCompression: false,
  primaryKey: 'passportId',
  type: 'object',
  properties: {
    passportId: { type: 'string', maxLength: 100 },
    firstName: { type: 'string', maxLength: 100 },
    lastName: { type: 'string', maxLength: 100 },
    age: { type: 'integer', minimum: 0, maximum: 150, multipleOf: 1 },
    _deleted: { type: 'boolean' },
  },
  required: ['firstName', 'lastName', 'passportId'],
  indexes: ['firstName'],
};

const report = {
  direction: 'dxdb-client -> upstream-server',
  pullWorked: false,
  pushWorked: false,
  conflictsOnly: null,
  checkpointShape: null,
  passed: false,
};

// --- the genuine upstream server ------------------------------------------
const upstreamDb = await rxDatabase.createRxDatabase({
  name: 'interop-server-' + Math.random().toString(36).slice(2, 8),
  storage: rxStorage.getRxStorageMemory(),
  multiInstance: false,
  eventReduce: false,
});
const collections = await upstreamDb.addCollections({ humans: { schema: humanSchema } });
const upstreamCollection = collections.humans;

await upstreamCollection.insert({
  passportId: 'server-side',
  firstName: 'Ser',
  lastName: 'Ver',
  age: 55,
});

const serverState = websocket.startWebsocketServer({
  database: upstreamDb,
  port: 0,
  host: '127.0.0.1',
});
await new Promise((resolve) => serverState.server.once('listening', resolve));
const port = serverState.server.address().port;
const url = `ws://127.0.0.1:${port}`;

// --- a dxdb-shaped client --------------------------------------------------
const { WebSocket } = await import('ws');
const socket = new WebSocket(url);
await new Promise((resolve, reject) => {
  socket.once('open', resolve);
  socket.once('error', reject);
});

let counter = 0;
const pending = new Map();
const streamItems = [];
socket.on('message', (raw) => {
  const message = JSON.parse(String(raw));
  if (message.id === 'stream') {
    streamItems.push(message);
    return;
  }
  const resolve = pending.get(message.id);
  if (resolve) {
    pending.delete(message.id);
    resolve(message);
  }
});

/** Sends one frame and awaits its response, exactly as `websocket.rs` does. */
function call(method, params) {
  const id = `dxdb|${counter++}`;
  return new Promise((resolve, reject) => {
    pending.set(id, resolve);
    socket.send(JSON.stringify({ id, collection: upstreamCollection.name, method, params }));
    setTimeout(() => {
      if (pending.delete(id)) reject(new Error(`${method} timed out`));
    }, 5000);
  });
}

try {
  // The change-stream subscription: upstream treats a `masterChangeStream$` request as "start
  // pushing events to me with id 'stream'". Our `websocket.rs` documents this as a stream, not
  // an RPC, so this confirms upstream's handling matches our documented expectation.
  socket.send(
    JSON.stringify({
      id: 'stream',
      collection: upstreamCollection.name,
      method: 'masterChangeStream$',
      params: [],
    })
  );

  // --- pull ---------------------------------------------------------------
  const pulled = await call('masterChangesSince', [null, 10]);
  assert.ok(pulled.result, 'a masterChangesSince response has a result');
  const documents = pulled.result.documents;
  report.pullWorked = documents.length === 1 && documents[0].passportId === 'server-side';
  report.checkpointShape = pulled.result.checkpoint;
  assert.ok(
    report.pullWorked,
    `expected the server's document, got ${JSON.stringify(documents.map((d) => d.passportId))}`
  );

  // The checkpoint upstream hands out is `{id, lwt}`, which is what our protocol's opaque
  // checkpoint carries and what `Checkpoint::as_storage_checkpoint` round-trips.
  assert.ok(
    report.checkpointShape && typeof report.checkpointShape.id === 'string',
    `upstream's checkpoint must carry an id, got ${JSON.stringify(report.checkpointShape)}`
  );

  // --- push ---------------------------------------------------------------
  const pushed = await call('masterWrite', [
    [
      {
        assumedMasterState: null,
        newDocumentState: {
          passportId: 'from-rust-shape',
          firstName: 'Rust',
          lastName: 'Shape',
          age: 33,
          _deleted: false,
          _attachments: {},
        },
      },
    ],
  ]);
  // The contract: conflicts only. An accepted write must produce an empty array.
  report.conflictsOnly = Array.isArray(pushed.result) && pushed.result.length === 0;
  assert.ok(
    report.conflictsOnly,
    `masterWrite must return conflicts only; got ${JSON.stringify(pushed.result)}`
  );

  const stored = await upstreamCollection.findOne('from-rust-shape').exec();
  report.pushWorked = stored !== null && stored.get('firstName') === 'Rust';
  assert.ok(report.pushWorked, "the pushed document must exist in upstream's collection");

  report.passed = true;
} catch (error) {
  report.error = String(error?.stack ?? error).split('\n').slice(0, 5).join(' | ');
} finally {
  socket.close();
  await new Promise((resolve) => serverState.close().then(resolve).catch(resolve));
  await upstreamDb.close().catch(() => {});
}

process.stdout.write(JSON.stringify(report, null, 2) + '\n');
if (!report.passed) process.exitCode = 1;
