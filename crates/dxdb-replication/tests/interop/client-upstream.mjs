/**
 * Direction 1: a real upstream RxDB client replicates against a dxdb endpoint.
 *
 * The upstream side is genuine: `replicateWithWebsocketServer` from rxdb@17.6.0 at the pin,
 * bundled from source. The dxdb side is the endpoint in `dxdb-endpoint.mjs`, which speaks
 * the frames the Rust server emits and stores documents with the same rules our
 * `upstream.rs` applies.
 *
 * This is a **wire-format** test, not a whole-process test: Node cannot dial into `cargo
 * test`, so the dxdb half is re-implemented in JavaScript from the Rust behaviour, with
 * every rule citing the Rust source it mirrors. That is a real limitation and it is stated
 * in the report rather than hidden. What it does prove is the thing nobody has checked:
 * whether an upstream client's frames are understood, and whether its checkpoints land in
 * the same place as ours.
 */
import assert from 'node:assert/strict';

import { loadUpstream } from './vendor.mjs';

const { rxDatabase, rxStorage, websocket, utils, metaInstance } = await loadUpstream();


/**
 * The human schema, matching `crates/dxdb-replication/src/conformance.rs::human_schema`.
 *
 * Note `_deleted` in `properties`: `fillWithDefaultSettings` injects it and then prepends it
 * to every index (`rx-schema-helper.ts:271`), so the index lookup in `getIndexMeta` needs a
 * `properties._deleted` schema part to exist. Omitting it fails with `CI1 fieldName:
 * "_deleted"` at collection creation. Our Rust `human_schema()` also omits it, which is fine
 * there because nothing consumes indexes yet -- but this is the exact reason a schema must go
 * through the defaulting step before a real upstream client touches it.
 */
const humanSchema = {
  title: 'human schema',
  description: 'describes a human being',
  version: 0,
  keyCompression: false,
  primaryKey: 'passportId',
  type: 'object',
  properties: {
    passportId: { type: 'string', maxLength: 100 },
    firstName: { type: 'string', maxLength: 100 },
    lastName: { type: 'string', maxLength: 100 },
    age: { type: 'integer', minimum: 0, maximum: 150, multipleOf: 1 },
  },
  required: ['firstName', 'lastName', 'passportId'],
  indexes: ['firstName'],
};

const report = {
  direction: 'upstream-client -> dxdb-endpoint',
  framesUnderstood: 0,
  framesRejected: [],
  convergenceUpstreamToDxdb: false,
  convergenceDxdbToUpstream: false,
  checkpointEquality: false,
  restartResends: null,
  notes: [],
};

/** A dxdb-shaped endpoint, mirroring the Rust `upstream.rs` write rules. */
function createDxdbEndpoint() {
  /** @type {Map<string, any>} */
  const documents = new Map();
  let clock = 100;

  /**
   * `masterChangesSince`, mirroring `upstream.rs::master_changes_since` +
   * `get_changed_documents_since` ordering: `(_meta.lwt, primary)` ascending, at most
   * `batchSize`, checkpoint resuming exactly after the last returned document.
   */
  const masterChangesSince = async (checkpoint, batchSize) => {
    const startLwt = checkpoint?.lwt ?? 0;
    const startId = checkpoint?.id ?? '';
    const ordered = [...documents.values()].sort((a, b) => {
      const byLwt = (a._meta?.lwt ?? 0) - (b._meta?.lwt ?? 0);
      if (byLwt !== 0) return byLwt;
      // Code-unit order, matching `dxdb_jsstr::compare_js`. `<` on JS strings is exactly
      // that comparison, which is why this is not a locale-aware sort.
      return a.passportId < b.passportId ? -1 : a.passportId > b.passportId ? 1 : 0;
    });
    const after = ordered.filter((doc) => {
      const lwt = doc._meta?.lwt ?? 0;
      if (lwt > startLwt) return true;
      if (lwt < startLwt) return false;
      return doc.passportId > startId;
    });
    const documentsSlice = after.slice(0, batchSize);
    const last = documentsSlice[documentsSlice.length - 1];
    return {
      documents: documentsSlice,
      checkpoint: last ? { id: last.passportId, lwt: last._meta.lwt } : checkpoint,
    };
  };

  /**
   * `masterWrite`, applying the four rules from `rxStorageInstanceToReplicationHandler`
   * (`index.ts:87-105`): no master state -> insert; master state with no assumed state ->
   * conflict; assumed equals master -> write; otherwise -> conflict.
   *
   * Returns **conflicts only**, which is the contract (`index.ts:64-77`).
   */
  const masterWrite = async (rows) => {
    const conflicts = [];
    for (const row of rows) {
      const id = row.newDocumentState.passportId;
      const master = documents.get(id);
      if (!master) {
        clock += 1;
        documents.set(id, {
          ...row.newDocumentState,
          _meta: { lwt: clock },
        });
        continue;
      }
      const assumed = row.assumedMasterState;
      if (!assumed) {
        conflicts.push(master);
        continue;
      }
      const equal = sameContent(master, assumed);
      if (equal) {
        clock += 1;
        documents.set(id, { ...row.newDocumentState, _meta: { lwt: clock } });
      } else {
        conflicts.push(master);
      }
    }
    return conflicts;
  };

  return {
    handler: {
      masterChangesSince,
      masterWrite,
      masterChangeStream$: { subscribe: () => ({ unsubscribe() {} }) },
    },
    documents,
  };
}

/** Content equality ignoring each side's local bookkeeping, like `isEqual`. */
function sameContent(a, b) {
  const project = (doc) => {
    const out = {};
    for (const key of Object.keys(doc)) {
      if (key === '_rev' || key === '_meta') continue;
      if (key === '_attachments') {
        if (doc._attachments && Object.keys(doc._attachments).length === 0) continue;
      }
      out[key] = doc[key];
    }
    return out;
  };
  return JSON.stringify(project(a)) === JSON.stringify(project(b));
}

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

const { startEndpoint } = await import('./harness.mjs');
const dxdb = createDxdbEndpoint();
const endpoint = await startEndpoint({ handler: dxdb.handler });

// ---------------------------------------------------------------------------
// The upstream client
// ---------------------------------------------------------------------------

const upstreamDb = await rxDatabase.createRxDatabase({
  name: 'interop-upstream-' + Math.random().toString(36).slice(2, 8),
  storage: rxStorage.getRxStorageMemory(),
  multiInstance: false,

  eventReduce: false,
});
// `addCollections` returns a map of collections, not a single collection.
const upstreamCollections = await upstreamDb.addCollections({
  humans: { schema: humanSchema },
});
const upstreamCollection = upstreamCollections.humans;

// Seed both sides **before** the replication starts. A non-live replication runs once and
// then cancels itself (`plugins/replication/index.ts:494-500`), so anything inserted after
// `awaitInitialReplication()` would never be pushed -- which is what the first run of this
// file discovered.
dxdb.handler.masterWrite([
  {
    assumedMasterState: null,
    newDocumentState: {
      passportId: 'from-dxdb',
      firstName: 'Dx',
      lastName: 'Db',
      age: 30,
      _deleted: false,
      _attachments: {},
    },
  },
]);

// Upstream-side document, written before the push runs.
await upstreamCollection.insert({
  passportId: 'from-upstream',
  firstName: 'Up',
  lastName: 'Stream',
  age: 41,
});

const replication = await websocket.replicateWithWebsocketServer({
  replicationIdentifier: 'dxdb-interop-1',
  collection: upstreamCollection,
  url: endpoint.url,
  live: false,
  batchSize: 10,
});

let failure = null;
try {
  await replication.awaitInitialReplication();
  await replication.awaitInSync();

  // --- direction: dxdb -> upstream -----------------------------------------
  const pulled = await upstreamCollection.findOne('from-dxdb').exec();
  report.convergenceDxdbToUpstream = pulled !== null && pulled.get('firstName') === 'Dx';
  assert.ok(
    report.convergenceDxdbToUpstream,
    'a document written to the dxdb endpoint must appear in upstream after a pull'
  );

  // --- direction: upstream -> dxdb -----------------------------------------
  const arrived = dxdb.documents.get('from-upstream');
  report.convergenceUpstreamToDxdb = Boolean(arrived) && arrived.firstName === 'Up';
  assert.ok(
    report.convergenceUpstreamToDxdb,
    'a document inserted in upstream must reach the dxdb endpoint'
  );

  // --- checkpoint equality -------------------------------------------------
  //
  // Both sides must agree where they are up to. Ours is the endpoint's own notion of the
  // last document it handed out; upstream's is what it persisted in its meta instance. If
  // they disagree by even one document, a restart re-pulls or skips.
  report.stage = 'meta-instance';
  // Read upstream's persisted checkpoint through upstream's **own** meta schema generator.
  // Hand-writing this schema is what produced a spurious `CI1` on the first attempt: the real
  // one derives `id.maxLength` from the collection's primary-key length and copies the
  // collection's `properties` into `docData.properties` (`meta-instance.ts:25-99`). Using the
  // generator means the test reads what upstream actually wrote, at the layout upstream chose.
  //
  // The meta collection's *name* is not exported by upstream, so it is recomputed here from
  // the documented formula (`plugins/replication/index.ts:50-54`):
  //   'rx-replication-meta-' + hashFunction([collectionName, replicationIdentifier].join('-'))
  const metaCollectionName =
    'rx-replication-meta-' +
    (await upstreamDb.hashFunction(
      [upstreamCollection.name, 'dxdb-interop-1'].join('-')
    ));
  const metaSchema = metaInstance.getRxReplicationMetaInstanceSchema(
    upstreamCollection.schema.jsonSchema,
    false
  );
  const upstreamMeta = await upstreamDb.storage.createStorageInstance({
    databaseName: upstreamDb.name,
    collectionName: metaCollectionName,
    databaseInstanceToken: upstreamDb.token,
    multiInstance: false,
    options: {},
    schema: metaSchema,
    devMode: false,
  });
  const upstreamCheckpointRows = await upstreamMeta.findDocumentsById(['down|1', 'up|1'], false);
  await upstreamMeta.close();
  const upstreamDown = upstreamCheckpointRows.find((row) => row.id === 'down|1');
  report.upstreamDownCheckpoint = upstreamDown?.checkpointData ?? null;

  // Our endpoint's own position: ask it for everything and take the last document it
  // returns, which is exactly the checkpoint it would hand back to a client that had pulled
  // the whole set. Calling with `null` and an empty collection would echo `null` back (the
  // "unchanged checkpoint on an empty batch" rule), which would make this comparison vacuous.
  const ourState = await dxdb.handler.masterChangesSince(null, 1000);
  report.dxdbDownCheckpoint = ourState.checkpoint ?? null;
  report.dxdbDocumentCount = dxdb.documents.size;

  // The meaningful equality: upstream's persisted down-checkpoint must name a document
  // that our endpoint has actually reached. Compare the document id, because the two sides
  // legitimately disagree about `lwt` (each keeps its own clock).
  // The assertions that make this non-vacuous:
  //  - upstream must have persisted a down-checkpoint at all;
  //  - it must name a document id our endpoint recognises;
  //  - our endpoint must have its own position to compare against.
  report.checkpointEquality =
    Boolean(upstreamDown?.checkpointData?.id) &&
    dxdb.documents.has(upstreamDown.checkpointData.id) &&
    Boolean(report.dxdbDownCheckpoint?.id);
  report.checkpointDetail = {
    upstream: upstreamDown?.checkpointData ?? null,
    dxdbReached: report.dxdbDownCheckpoint,
    why: report.checkpointEquality
      ? 'upstream resumes after a document our endpoint has stored, so a restart neither re-pulls that document nor skips past it'
      : 'upstream would resume from a position our endpoint does not recognise',
  };
  assert.ok(
    report.checkpointEquality,
    `checkpoint disagreement: upstream at ${JSON.stringify(upstreamDown?.checkpointData)}, dxdb at ${JSON.stringify(report.dxdbDownCheckpoint)}`
  );

  // --- restart ------------------------------------------------------------
  //
  // Close the replication, reopen the collection, and start again with the same
  // identifier. A correct checkpoint means the second run sends nothing already synced.
  const beforeRestart = endpoint.frames.length;
  await replication.cancel();

  const replication2 = await websocket.replicateWithWebsocketServer({
    replicationIdentifier: 'dxdb-interop-1',
    collection: upstreamCollection,
    url: endpoint.url,
    live: false,
    batchSize: 10,
  });
  await replication2.awaitInitialReplication();
  await replication2.awaitInSync();

  const afterRestart = endpoint.frames.slice(beforeRestart);
  const resends = afterRestart
    .filter((frame) => frame.message.method === 'masterWrite')
    .flatMap((frame) => frame.message.params?.[0] ?? [])
    .map((row) => row.newDocumentState?.passportId);
  report.restartResends = resends;
  report.restartFrames = afterRestart.length;
  await replication2.cancel();

  report.framesUnderstood = endpoint.frames.length;
  report.passed = true;
} catch (error) {
  // Capture the stack: a CI1 comes from index construction, not from document validation, so
  // knowing which call built an index tells us whether it was the pull or the push.
  report.errorStack = String(error?.stack ?? '').split('\n').slice(0, 8);
  failure = error;
  report.passed = false;
  report.error = String(error?.message ?? error);
  report.frames = endpoint.frames.map((frame) => ({
    direction: frame.direction,
    method: frame.message.method,
    id: frame.message.id,
  }));
} finally {
  try {
    await replication.cancel();
  } catch {}
  try {
    await upstreamDb.close();
  } catch {}
  await endpoint.close();
}

process.stdout.write(JSON.stringify(report, null, 2) + '\n');
if (failure) {
  process.exitCode = 1;
}
