/**
 * Family: replication transcripts.
 *
 * Upstream: `src/replication-protocol/` (`replicateRxStorageInstance`,
 * `rxStorageInstanceToReplicationHandler`, `getRxReplicationMetaInstanceSchema`,
 * `awaitRxStorageReplicationFirstInSync`, `awaitRxStorageReplicationIdle`).
 *
 * A replication is not a pure function, so it cannot be pinned by one input and
 * one output. What it can be pinned by is a **transcript**: a scripted sequence
 * of writes and pulls, and after each step the observable state of three things —
 * the fork (client), the master, and the meta instance that holds the checkpoints.
 * That is what this family records, and it feeds M3.
 *
 * The meta instance is the part people underestimate. Every pull and push writes a
 * checkpoint document whose id is `<itemId>|<flag>` (`down|1`, `up|1`), whose
 * revision is derived from a SHA-256 of the checkpoint key, and whose `_meta.lwt`
 * comes from `now()`. All three are recorded, because a port that computes the
 * checkpoint key differently will silently re-pull from the beginning.
 *
 * Determinism: `now()` is frozen by the harness (so `_meta.lwt` values are
 * reproducible), and `Math.random()` is seeded (so the `randomToken()` instance
 * tokens inside thrown-together replicas are too). Document revisions that depend
 * on a database instance token are recorded verbatim rather than normalised, so a
 * port must produce the same token-to-revision derivation.
 */
import { join } from 'node:path';

import { loadOracle } from '../vendor.mjs';
import { determinismHeader, emitFamily } from '../harness.mjs';

const FAMILY = 'replication-transcripts';
const PRIMARY = 'passportId';

/** A conflict handler that records what it was asked and resolves by age. */
function higherAgeConflictHandler(onCall) {
  return {
    isEqual(a, b) {
      return JSON.stringify(a) === JSON.stringify(b);
    },
    resolve(input) {
      onCall && onCall(input);
      const docA = input.newDocumentState;
      const docB = input.realMasterState;
      // Deleted state wins on disagreement, matching upstream's default handler
      // in intent; the point is that the resolution is recorded, not inferred.
      if (!!docA._deleted !== !!docB._deleted) {
        return input.newDocumentState;
      }
      const ageA = typeof docA.age === 'number' ? docA.age : 0;
      const ageB = typeof docB.age === 'number' ? docB.age : 0;
      return ageA >= ageB ? docA : docB;
    },
  };
}

function document(id, overrides = {}) {
  return {
    passportId: id,
    firstName: 'first-' + id,
    lastName: 'last-' + id,
    age: 20,
    _deleted: false,
    _rev: '1-' + id,
    _meta: { lwt: 1000 },
    _attachments: {},
    ...overrides,
  };
}

export async function run({ check, repo }) {
  const oracle = await loadOracle();
  const { replication, memory, queryHelper, rxSchema, utilsHash, fixtures } = oracle.modules;

  const outFile = join(repo, 'testdata', 'vectors', `${FAMILY}.json`);
  const schema = rxSchema.createRxSchema(
    JSON.parse(JSON.stringify(fixtures.human)),
    utilsHash.defaultHashSha256
  ).jsonSchema;
  const metaSchema = replication.getRxReplicationMetaInstanceSchema(schema, false);

  const storage = memory.getRxStorageMemory();
  let instanceSeq = 0;
  const createInstance = async (label, useSchema) => {
    instanceSeq += 1;
    return storage.createStorageInstance({
      // Deterministic tokens: the harness froze Math.random, and a fixed token
      // makes the derived revision hashes reproducible and checkable.
      databaseInstanceToken: 'token-' + label,
      databaseName: 'db-' + label + '-' + instanceSeq,
      collectionName: 'col-' + label,
      schema: useSchema,
      options: {},
      multiInstance: false,
      devMode: false,
    });
  };

  /** Read every document in a deterministic order, stripped of nothing. */
  const snapshot = async (instance) => {
    const prepared = queryHelper.prepareQuery(
      instance.schema,
      queryHelper.normalizeMangoQuery(instance.schema, {
        selector: {},
        sort: [{ [PRIMARY]: 'asc' }],
      })
    );
    const result = await instance.query(prepared);
    return result.documents.map((doc) => ({
      [PRIMARY]: doc[PRIMARY],
      _rev: doc._rev,
      _deleted: doc._deleted,
      _meta: doc._meta,
      age: doc.age,
      firstName: doc.firstName,
    }));
  };

  /** The meta instance is the checkpoint record; read it as the storage holds it. */
  const snapshotMeta = async (meta) => {
    const prepared = queryHelper.prepareQuery(
      meta.schema,
      queryHelper.normalizeMangoQuery(meta.schema, { selector: {}, sort: [{ id: 'asc' }] })
    );
    const result = await meta.query(prepared);
    return result.documents.map((doc) => ({
      id: doc.id,
      itemId: doc.itemId,
      isCheckpoint: doc.isCheckpoint,
      _rev: doc._rev,
      _meta: doc._meta,
      // The field is `checkpointData` (see `getRxReplicationMetaInstanceSchema`).
      // This is the actual checkpoint payload: id + lwt of the last applied
      // document, which is what a resumed replication continues from.
      checkpointData: doc.checkpointData ?? null,
      // The fork's recorded assumption about the master's document state, used
      // for conflict detection on the next push.
      docData: doc.docData ? { _rev: doc.docData._rev, _deleted: doc.docData._deleted, age: doc.docData.age } : null,
      isResolvedConflict: doc.isResolvedConflict ?? null,
    }));
  };

  const transcripts = [];

  /**
   * Each scenario builds a fresh master/fork/meta trio, runs its script, and
   * snapshots after every labelled step. `steps` is the transcript.
   */
  const runScenario = async ({ why, script }) => {
    const master = await createInstance('master', schema);
    const fork = await createInstance('fork', schema);
    const meta = await createInstance('meta', metaSchema);

    const conflictCalls = [];
    const errors = [];
    const steps = [];

    const record = async (label, extra = {}) => {
      steps.push({
        step: label,
        master: await snapshot(master),
        fork: await snapshot(fork),
        meta: await snapshotMeta(meta),
        ...extra,
      });
    };

    let state;
    const start = async () => {
      state = replication.replicateRxStorageInstance({
        identifier: 'vec-replication',
        replicationHandler: replication.rxStorageInstanceToReplicationHandler(
          master,
          higherAgeConflictHandler((input) => conflictCalls.push(input)),
          'token-master-handler'
        ),
        forkInstance: fork,
        metaInstance: meta,
        skipStoringPullMeta: false,
        pullBatchSize: 100,
        pushBatchSize: 100,
        conflictHandler: higherAgeConflictHandler((input) => conflictCalls.push(input)),
        hashFunction: utilsHash.defaultHashSha256,
      });
      state.events.error.subscribe((err) => {
        errors.push(String((err && err.message) || err).split('\n')[0]);
      });
      await replication.awaitRxStorageReplicationFirstInSync(state);
      await replication.awaitRxStorageReplicationIdle(state);
    };

    const sync = async () => {
      await replication.awaitRxStorageReplicationIdle(state);
    };

    const context = { master, fork, meta, record, start, sync, errors, conflictCalls };
    await script(context);

    // Cancel before closing so the replication does not act on closed instances.
    if (state) {
      await replication.awaitRxStorageReplicationInSync(state);
      await replication.cancelRxStorageReplication(state);
    }
    await Promise.all([master.close(), fork.close(), meta.close()]);

    transcripts.push({
      why,
      steps,
      recordedConflictResolutions: conflictCalls.length,
      errors,
      finalForkState: steps.length > 0 ? steps[steps.length - 1].fork : [],
      finalMasterState: steps.length > 0 ? steps[steps.length - 1].master : [],
      finalMetaState: steps.length > 0 ? steps[steps.length - 1].meta : [],
    });
  };

  // 1. The empty case: both sides empty, nothing to transfer but checkpoints.
  await runScenario({
    why: 'empty-master-and-fork',
    script: async ({ record, start }) => {
      // Recorded before and after, so the transcript shows that a sync over two
      // empty sides still *writes checkpoints* -- the case where "nothing synced"
      // and "nothing to sync" look identical in the document snapshots.
      await record('before-start');
      await start();
      await record('after-start');
    },
  });

  // 2. Master already has documents when the replication starts: pure pull.
  await runScenario({
    why: 'pull-existing-documents',
    script: async ({ master, record, start }) => {
      await master.bulkWrite(
        [document('a', { age: 10 }), document('b', { age: 40 })].map((doc) => ({ document: doc })),
        'setup'
      );
      await record('master-populated');
      await start();
      await record('after-pull');
    },
  });

  // 3. Only the fork has documents: pure push.
  await runScenario({
    why: 'push-existing-documents',
    script: async ({ fork, record, start }) => {
      await fork.bulkWrite(
        [document('c', { age: 33 })].map((doc) => ({ document: doc })),
        'setup'
      );
      await record('fork-populated');
      await start();
      await record('after-push');
    },
  });

  // 4. Both sides have the same id with different content: conflict resolution.
  await runScenario({
    why: 'conflict-both-sides-differ',
    script: async ({ master, fork, record, start }) => {
      // Different revisions on each side means `isEqual` is false and `resolve`
      // runs; the recorded resolution says which state survived on both sides.
      await master.bulkWrite([{ document: document('d', { age: 50 }) }], 'setup');
      await fork.bulkWrite([{ document: document('d', { age: 10, _rev: '1-fork-d' }) }], 'setup');
      await record('both-sides-populated');
      await start();
      await record('after-conflict');
    },
  });

  // 5. Deletion propagates as a tombstone, not as absence.
  await runScenario({
    why: 'deletion-propagates',
    script: async ({ master, record, start, sync }) => {
      await master.bulkWrite([{ document: document('e', { age: 22 }) }], 'setup');
      await start();
      await record('after-initial-pull');
      await master.bulkWrite(
        [
          {
            document: document('e', { age: 22, _deleted: true, _rev: '2-deleted-e', _meta: { lwt: 2000 } }),
            previous: document('e', { age: 22 }),
          },
        ],
        'change'
      );
      await sync();
      await record('after-master-delete');
    },
  });

  // 6. A second write after the initial sync: incremental pull, new checkpoint.
  await runScenario({
    why: 'incremental-pull-after-first-sync',
    script: async ({ master, record, start, sync }) => {
      await master.bulkWrite([{ document: document('f', { age: 25 }) }], 'setup');
      await start();
      await record('after-first-sync');
      await master.bulkWrite(
        [
          {
            document: document('g', { age: 26, _rev: '1-g', _meta: { lwt: 3000 } }),
          },
        ],
        'change'
      );
      await sync();
      await record('after-second-doc');
    },
  });

  // 7. Batch boundary: more documents than `pullBatchSize=100` would not be
  // practical to commit, so the boundary is exercised with a small explicit
  // replication instance instead (see `batchBoundary` below).

  /**
   * Batch boundary, with a deliberately small pull batch. This is the case where
   * the checkpoint has to advance exactly once per batch and never skip the last
   * document of a full batch; a port that checkpoints before applying, or that
   * checkpoints with the last *returned* rather than the last *applied* document,
   * loses documents here and nowhere else.
   */
  const batchBoundary = [];
  {
    const master = await createInstance('batchmaster', schema);
    const fork = await createInstance('batchfork', schema);
    const meta = await createInstance('batchmeta', metaSchema);
    const docs = Array.from({ length: 7 }, (_, i) =>
      document('batch-' + i, { _meta: { lwt: 1000 + i * 10 } })
    );
    await master.bulkWrite(docs.map((doc) => ({ document: doc })), 'setup');

    const state = replication.replicateRxStorageInstance({
      identifier: 'vec-batch',
      replicationHandler: replication.rxStorageInstanceToReplicationHandler(
        master,
        higherAgeConflictHandler(),
        'token-batch-handler'
      ),
      forkInstance: fork,
      metaInstance: meta,
      skipStoringPullMeta: false,
      pullBatchSize: 3,
      pushBatchSize: 3,
      conflictHandler: higherAgeConflictHandler(),
      hashFunction: utilsHash.defaultHashSha256,
    });
    const errors = [];
    state.events.error.subscribe((err) => errors.push(String((err && err.message) || err).split('\n')[0]));
    await replication.awaitRxStorageReplicationFirstInSync(state);
    await replication.awaitRxStorageReplicationIdle(state);
    batchBoundary.push({
      why: 'pull-batch-size-3-with-7-documents',
      pullBatchSize: 3,
      documentsInMaster: docs.length,
      forkIds: (await snapshot(fork)).map((d) => d[PRIMARY]),
      meta: await snapshotMeta(meta),
      errors,
    });
    await replication.awaitRxStorageReplicationInSync(state);
    await replication.cancelRxStorageReplication(state);
    await Promise.all([master.close(), fork.close(), meta.close()]);
  }

  const doc = {
    ...oracle.header(
      'Replication transcripts from upstream src/replication-protocol/ against the ' +
        'in-memory storage (PLAN.md 7.2, feeds M3). Each transcript is a scripted ' +
        'sequence of writes and syncs, and after every step it records the observable ' +
        'state of the fork, the master, and the replication meta instance (where the ' +
        'checkpoints and their derived revisions live).'
    ),
    format: 'scripted-steps->{fork, master, meta} snapshots',
    determinism: {
      ...determinismHeader(),
      note:
        'now() is frozen so _meta.lwt and checkpoint revisions are reproducible; ' +
        'document revisions are derived from the database instance token, which is ' +
        'fixed per instance here rather than randomToken-ed.',
    },
    transactionCount: transcripts.length,
    caseCount:
      transcripts.reduce((n, t) => n + t.steps.length, 0) + batchBoundary.length,
    transcripts,
    batchBoundary,
  };

  return emitFamily({ outFile, doc, check, label: FAMILY, caseCount: doc.caseCount });
}
