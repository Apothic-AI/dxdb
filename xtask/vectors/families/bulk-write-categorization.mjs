/**
 * Family: bulk-write categorisation.
 *
 * Upstream: `src/rx-storage-helper.ts:205` (`categorizeBulkWriteRows`).
 *
 * Every RxStorage implementation — memory, IndexedDB, Dexie, SQLite — funnels its
 * writes through this one pure function. PLAN.md 4.3 requires it to be ported as a
 * single pure function, and this family is how that port gets checked: given the
 * documents currently in the storage and a list of write rows, which rows become
 * inserts, which become updates, which become conflict errors, and exactly what
 * change events come out.
 *
 * The output has six parts and all six matter:
 *
 *   - `bulkInsertDocs` / `bulkUpdateDocs`: rows to persist, in input order;
 *   - `errors`: `status: 409` conflicts (revision mismatch, which includes
 *     "previous is missing") and `status: 510` attachment errors;
 *   - `eventBulk.events`: INSERT / UPDATE / DELETE, where a write that un-deletes
 *     a tombstoned document is an **INSERT**, not an UPDATE — the distinction
 *     downstream replications and observables depend on;
 *   - `newestRow`: the last successfully written row, used to stamp the event
 *     bulk's checkpoint;
 *   - `attachmentsAdd` / `attachmentsRemove` / `attachmentsUpdate`.
 *
 * The generator drives it against the real in-memory storage instance state, so
 * the `docsInDb` map and the `schema.attachments` flag are the ones a storage
 * would have, not a hand-built stub.
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

import { loadOracle } from '../vendor.mjs';

const FAMILY = 'bulk-write-categorization';

const PRIMARY = 'passportId';

/** Build a stored document the way RxDB stores it (meta fields included). */
function storedDoc(id, rev, lwt, overrides = {}) {
  return {
    passportId: id,
    firstName: 'first-' + id,
    lastName: 'last-' + id,
    age: 20,
    _deleted: false,
    _rev: rev,
    _meta: { lwt },
    _attachments: {},
    ...overrides,
  };
}

function writeRow(document, previous) {
  const row = { document };
  if (previous !== undefined) {
    row.previous = previous;
  }
  return row;
}

/**
 * Scenarios, each an initial storage state plus the write rows to categorise.
 * They are ordered by the property they pin; the labels are what a failing Rust
 * test will print.
 */
const SCENARIOS = [
  {
    why: 'fresh-insert',
    docsInDb: {},
    rows: [writeRow(storedDoc('a', '1-aaa', 1000))],
  },
  {
    why: 'insert-already-deleted-document',
    // Legal on replication: a tombstone arriving for a document we never had.
    docsInDb: {},
    rows: [writeRow(storedDoc('a', '1-aaa', 1000, { _deleted: true }))],
  },
  {
    why: 'update-existing',
    docsInDb: { a: storedDoc('a', '1-aaa', 1000) },
    rows: [
      writeRow(storedDoc('a', '2-bbb', 2000, { age: 30 }), storedDoc('a', '1-aaa', 1000)),
    ],
  },
  {
    why: 'update-conflict-stale-previous',
    docsInDb: { a: storedDoc('a', '2-bbb', 2000) },
    rows: [
      writeRow(storedDoc('a', '2-ccc', 3000, { age: 30 }), storedDoc('a', '1-aaa', 1000)),
    ],
  },
  {
    why: 'update-without-previous-is-conflict',
    docsInDb: { a: storedDoc('a', '1-aaa', 1000) },
    rows: [writeRow(storedDoc('a', '2-bbb', 2000, { age: 30 }))],
  },
  {
    why: 'delete-existing',
    docsInDb: { a: storedDoc('a', '1-aaa', 1000) },
    rows: [
      writeRow(
        storedDoc('a', '2-bbb', 2000, { _deleted: true, _attachments: {} }),
        storedDoc('a', '1-aaa', 1000)
      ),
    ],
  },
  {
    why: 'revive-deleted-document-is-insert-event',
    // The property that trips people up: previous._deleted true, document false
    // yields operation INSERT even though the row is an update.
    docsInDb: { a: storedDoc('a', '2-bbb', 2000, { _deleted: true }) },
    rows: [
      writeRow(
        storedDoc('a', '3-ccc', 3000),
        storedDoc('a', '2-bbb', 2000, { _deleted: true })
      ),
    ],
  },
  {
    why: 'mixed-batch-one-of-each',
    docsInDb: {
      a: storedDoc('a', '1-aaa', 1000),
      b: storedDoc('b', '1-bbb', 1100),
    },
    rows: [
      writeRow(storedDoc('c', '1-ccc', 1200)), // insert
      writeRow(storedDoc('a', '2-ddd', 1300, { age: 31 }), storedDoc('a', '1-aaa', 1000)), // update
      writeRow(storedDoc('b', '2-eee', 1400, { _deleted: true }), storedDoc('b', '1-bbb', 1100)), // delete
    ],
  },
  {
    why: 'duplicate-ids-in-one-batch',
    // The second row for the same id is still judged against the *storage* state,
    // not against the first row in the batch: the first has not been persisted
    // yet when categorisation runs.
    docsInDb: {},
    rows: [writeRow(storedDoc('a', '1-aaa', 1000)), writeRow(storedDoc('a', '1-bbb', 1100))],
  },
  {
    why: 'empty-batch',
    docsInDb: {},
    rows: [],
  },
  {
    why: 'tombstone-for-unknown-document-inserts',
    // Not a conflict: the conflict check only runs when a document is already in
    // the storage. A tombstone for an id we never had is inserted (this is how a
    // deletion replicates to a peer that never saw the insert), and emits no
    // event because there is nothing that observed the document appear.
    docsInDb: {},
    rows: [writeRow(storedDoc('a', '1-aaa', 1000, { _deleted: true }))],
  },
  {
    why: 'delete-existing-with-correct-previous',
    docsInDb: { a: storedDoc('a', '2-bbb', 2000) },
    rows: [
      writeRow(
        storedDoc('a', '3-ccc', 3000, { _deleted: true }),
        storedDoc('a', '2-bbb', 2000)
      ),
    ],
  },
  {
    why: 'many-inserts-order-preserved',
    docsInDb: {},
    rows: Array.from({ length: 8 }, (_, i) =>
      writeRow(storedDoc('doc-' + i, '1-r' + i, 1000 + i))
    ),
  },
];

/**
 * Attachment scenarios use their own schema (`attachments: {}`), because whether
 * the attachment branch runs at all depends on `storageInstance.schema.attachments`.
 */
const ATTACHMENT_SCENARIOS = [
  {
    why: 'insert-with-attachment-data',
    docsInDb: {},
    rows: [
      writeRow(
        storedDoc('a', '1-aaa', 1000, {
          _attachments: {
            'file.txt': {
              digest: 'sha256-abc',
              length: 3,
              revpos: 1,
              data: 'aGk=',
            },
          },
        })
      ),
    ],
  },
  {
    why: 'insert-attachment-without-data-is-510',
    docsInDb: {},
    rows: [
      writeRow(
        storedDoc('a', '1-aaa', 1000, {
          _attachments: {
            'file.txt': { digest: 'sha256-abc', length: 3, revpos: 1 },
          },
        })
      ),
    ],
  },
  {
    why: 'update-adds-attachment',
    docsInDb: { a: storedDoc('a', '1-aaa', 1000) },
    rows: [
      writeRow(
        storedDoc('a', '2-bbb', 2000, {
          _attachments: {
            'file.txt': { digest: 'sha256-abc', length: 3, revpos: 2, data: 'aGk=' },
          },
        }),
        storedDoc('a', '1-aaa', 1000)
      ),
    ],
  },
  {
    why: 'update-removes-attachment',
    docsInDb: {
      a: storedDoc('a', '1-aaa', 1000, {
        _attachments: { 'file.txt': { digest: 'sha256-abc', length: 3, revpos: 1 } },
      }),
    },
    rows: [
      writeRow(
        storedDoc('a', '2-bbb', 2000, { _attachments: {} }),
        storedDoc('a', '1-aaa', 1000, {
          _attachments: { 'file.txt': { digest: 'sha256-abc', length: 3, revpos: 1 } },
        })
      ),
    ],
  },
  {
    why: 'delete-document-with-attachments-removes-them',
    docsInDb: {
      a: storedDoc('a', '1-aaa', 1000, {
        _attachments: { 'file.txt': { digest: 'sha256-abc', length: 3, revpos: 1 } },
      }),
    },
    rows: [
      writeRow(
        storedDoc('a', '2-bbb', 2000, { _deleted: true, _attachments: {} }),
        storedDoc('a', '1-aaa', 1000, {
          _attachments: { 'file.txt': { digest: 'sha256-abc', length: 3, revpos: 1 } },
        })
      ),
    ],
  },
  {
    why: 'update-attachment-changed-digest',
    docsInDb: {
      a: storedDoc('a', '1-aaa', 1000, {
        _attachments: { 'file.txt': { digest: 'sha256-old', length: 3, revpos: 1 } },
      }),
    },
    rows: [
      writeRow(
        storedDoc('a', '2-bbb', 2000, {
          _attachments: {
            'file.txt': { digest: 'sha256-new', length: 3, revpos: 2, data: 'bmV3' },
          },
        }),
        storedDoc('a', '1-aaa', 1000, {
          _attachments: { 'file.txt': { digest: 'sha256-old', length: 3, revpos: 1 } },
        })
      ),
    ],
  },
];

/**
 * Reduce a categorisation result to the observable parts, with the write row
 * echoed back by *value* (upstream stores the caller's row object in errors,
 * which is not JSON-stable if the same object appears twice).
 */
function summarize(result) {
  return {
    bulkInsertDocs: result.bulkInsertDocs.map((r) => ({
      document: r.document,
      previous: r.previous ?? null,
    })),
    bulkUpdateDocs: result.bulkUpdateDocs.map((r) => ({
      document: r.document,
      previous: r.previous ?? null,
    })),
    newestRow: result.newestRow ? result.newestRow.document : null,
    errors: result.errors.map((e) => ({
      isError: e.isError,
      status: e.status,
      documentId: e.documentId,
      attachmentId: e.attachmentId ?? null,
      documentInDb: e.documentInDb ?? null,
      writeRowDocument: e.writeRow ? e.writeRow.document : null,
    })),
    events: result.eventBulk.events.map((e) => ({
      documentId: e.documentId,
      operation: e.operation,
      documentData: e.documentData,
      previousDocumentData: e.previousDocumentData ?? null,
    })),
    eventBulk: {
      // `id` is `randomToken()` output, deterministic only because the harness
      // replaced Math.random. The context is the caller's label.
      id: result.eventBulk.id,
      context: result.eventBulk.context,
      checkpoint: result.eventBulk.checkpoint,
    },
    attachmentsAdd: result.attachmentsAdd.map((a) => ({
      documentId: a.documentId,
      attachmentId: a.attachmentId,
      digest: a.digest,
    })),
    attachmentsRemove: result.attachmentsRemove.map((a) => ({
      documentId: a.documentId,
      attachmentId: a.attachmentId,
      digest: a.digest,
    })),
    attachmentsUpdate: result.attachmentsUpdate.map((a) => ({
      documentId: a.documentId,
      attachmentId: a.attachmentId,
      digest: a.digest,
    })),
  };
}

/**
 * Deterministic pseudo-random write sequences.
 *
 * The scripted scenarios above pin the named branches. They do not reach the
 * combinations — a conflict sandwiched between two successful updates, a revive
 * in the same batch as a delete, the same id written three times — which is where
 * an ordering bug in a port actually shows up. These sequences are generated from
 * a fixed seed so the file is reproducible, and they carry the same vocabulary of
 * write shapes as a real replication: insert, update, delete, revive, conflict.
 */
function createSequencePrng(seed = 0xb01c0de) {
  let state = seed >>> 0;
  return () => {
    state ^= state << 13;
    state >>>= 0;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    return state / 0x100000000;
  };
}

/** Revisions are height-prefixed in RxDB, so a plausible rev encodes a height. */
function rev(height, token) {
  return height + '-' + token;
}

function generateSequences({ count, seed, ids, batchSize }) {
  const next = createSequencePrng(seed);
  const pick = (list) => list[Math.floor(next() * list.length) % list.length];
  const sequences = [];

  for (let seq = 0; seq < count; seq++) {
    // Each sequence starts from a fresh storage with a seeded subset present.
    const docsInDb = {};
    const heights = {};
    for (const id of ids) {
      if (next() < 0.5) continue;
      const height = 1;
      const deleted = next() < 0.25;
      docsInDb[id] = storedDoc(id, rev(height, 'gen' + seq + id), 1000 + Math.floor(next() * 1000), {
        _deleted: deleted,
        age: Math.floor(next() * 100),
      });
      heights[id] = height;
    }

    const rows = [];
    for (let i = 0; i < batchSize; i++) {
      const id = pick(ids);
      const inDb = docsInDb[id];
      const height = heights[id] || 0;
      const roll = next();

      if (!inDb) {
        // Insert, or insert a tombstone for a document this storage never had.
        const deleted = roll < 0.2;
        const document = storedDoc(id, rev(1, 'gen' + seq + 'i' + i), 2000 + i, {
          _deleted: deleted,
          age: Math.floor(next() * 100),
        });
        rows.push(writeRow(document));
        continue;
      }

      if (roll < 0.55) {
        // Correct update: next height, previous echoes what is in the storage.
        const newHeight = height + 1;
        const document = storedDoc(id, rev(newHeight, 'gen' + seq + 'u' + i), 3000 + i, {
          age: Math.floor(next() * 100),
          _deleted: false,
        });
        rows.push(writeRow(document, inDb));
        continue;
      }

      if (roll < 0.75) {
        // Conflict: previous names a revision the storage does not hold.
        const document = storedDoc(id, rev(height + 1, 'gen' + seq + 'c' + i), 4000 + i, {
          age: Math.floor(next() * 100),
        });
        rows.push(
          writeRow(
            document,
            storedDoc(id, rev(Math.max(1, height - 1), 'stale' + seq + i), 500, {
              _deleted: false,
            })
          )
        );
        continue;
      }

      if (roll < 0.9) {
        // Update with no previous at all: also a conflict when the doc exists.
        rows.push(
          writeRow(
            storedDoc(id, rev(height + 1, 'gen' + seq + 'n' + i), 6000 + i, { age: 1 })
          )
        );
        continue;
      }

      // Delete, or revive a tombstone.
      const newHeight = height + 1;
      if (inDb._deleted) {
        rows.push(
          writeRow(
            storedDoc(id, rev(newHeight, 'gen' + seq + 'r' + i), 7000 + i, { _deleted: false }),
            inDb
          )
        );
      } else {
        rows.push(
          writeRow(
            storedDoc(id, rev(newHeight, 'gen' + seq + 'd' + i), 8000 + i, { _deleted: true }),
            inDb
          )
        );
      }
    }
    sequences.push({ seq, docsInDb, rows });
  }
  return sequences;
}

export async function run({ check, repo }) {
  const oracle = await loadOracle();
  const { storageHelper, memory, rxSchema, utilsHash, fixtures } = oracle.modules;

  const outFile = join(repo, 'testdata', 'vectors', `${FAMILY}.json`);

  const baseSchema = JSON.parse(JSON.stringify(fixtures.human));
  const attachSchema = JSON.parse(JSON.stringify(fixtures.human));
  attachSchema.attachments = {};

  const plainSchema = rxSchema.createRxSchema(baseSchema, utilsHash.defaultHashSha256).jsonSchema;
  const withAttachments = rxSchema.createRxSchema(attachSchema, utilsHash.defaultHashSha256).jsonSchema;

  let instanceCounter = 0;
  /** A real memory storage instance; only used for its `schema`, as upstream does. */
  async function makeInstance(schema) {
    instanceCounter += 1;
    return memory.getRxStorageMemory().createStorageInstance({
      databaseInstanceToken: 'vec-token',
      databaseName: 'vec-db-' + instanceCounter,
      collectionName: 'vec-col-' + instanceCounter,
      schema,
      options: {},
      multiInstance: false,
      devMode: false,
    });
  }

  const cases = [];
  const runScenario = async (scenario, schema, extra = {}) => {
    const instance = await makeInstance(schema);
    const docsInDb = new Map(Object.entries(scenario.docsInDb));
    const context = extra.context || 'vectors';
    const result = storageHelper.categorizeBulkWriteRows(
      instance,
      PRIMARY,
      docsInDb,
      scenario.rows,
      context
    );
    cases.push({
      schema: extra.schemaLabel || 'human',
      why: scenario.why,
      context,
      docsInDb: scenario.docsInDb,
      writeRows: scenario.rows,
      ...summarize(result),
    });
    await instance.close();
  };

  for (const scenario of SCENARIOS) {
    await runScenario(scenario, plainSchema);
  }
  for (const scenario of ATTACHMENT_SCENARIOS) {
    await runScenario(scenario, withAttachments, { schemaLabel: 'human+attachments' });
  }

  /**
   * Context is echoed into the event bulk and into every error, and storages use
   * it to tell their own writes from a replication's. It must survive verbatim,
   * including the collection plugin's special value.
   */
  const contexts = [
    'vectors',
    storageHelper.RX_COLLECTION_BULK_INSERT_CONTEXT,
    storageHelper.INTERNAL_STORAGE_NAME,
    '',
  ];
  for (const context of contexts) {
    await runScenario(
      {
        why: 'context-is-echoed',
        docsInDb: {},
        rows: [writeRow(storedDoc('a', '1-aaa', 1000))],
      },
      plainSchema,
      { context }
    );
  }

  /**
   * 24 generated sequences, each 6 rows over 4 ids. Chosen so the family stays a
   * few hundred KB and a human can still diff a change, while every row shape
   * (insert, tombstone-insert, update, conflict-by-stale-previous,
   * conflict-by-missing-previous, delete, revive) appears many times in many
   * orders.
   */
  const generated = generateSequences({
    count: 24,
    seed: 0xb01c0de,
    ids: ['a', 'b', 'c', 'd'],
    batchSize: 6,
  });
  for (const sequence of generated) {
    await runScenario(
      { why: 'generated-sequence-' + String(sequence.seq).padStart(2, '0'), docsInDb: sequence.docsInDb, rows: sequence.rows },
      plainSchema,
      { context: 'generated' }
    );
  }

  const doc = {
    ...oracle.header(
      'Outputs of upstream `categorizeBulkWriteRows` (src/rx-storage-helper.ts:205) for ' +
        'generated write sequences against the real in-memory storage instance state ' +
        '(PLAN.md 4.3 and 7.2). Both sides of a write are recorded: the rows to persist ' +
        'and the change events to emit, including the INSERT-for-revived-document and ' +
        '409-conflict cases.'
    ),
    format: 'docsInDb+writeRows->{bulkInsertDocs, bulkUpdateDocs, errors, events}',
    determinism: {
      ...oracle.determinism,
      eventBulkId: 'randomToken(10) output, deterministic because Math.random is seeded',
      context: 'echoed from the caller, not generated',
    },
    primaryPath: PRIMARY,
    caseCount: cases.length,
    cases,
  };

  const json = JSON.stringify(doc, null, 2) + '\n';

  if (check) {
    let committed;
    try {
      committed = readFileSync(outFile, 'utf8');
    } catch {
      console.error(`${FAMILY} vectors missing at ${outFile}; run \`make vectors\``);
      return { ok: false, cases: cases.length };
    }
    const strip = (text) => {
      const parsed = JSON.parse(text);
      delete parsed.node;
      return JSON.stringify(parsed);
    };
    if (strip(committed) !== strip(json)) {
      console.error(`${FAMILY} vectors drifted from the current Node output:`);
      console.error(`  regenerate with \`make vectors\` and review the diff at ${outFile}`);
      return { ok: false, cases: cases.length };
    }
    console.log(`${FAMILY} vectors OK (${cases.length} cases)`);
    return { ok: true, cases: cases.length };
  }

  mkdirSync(dirname(outFile), { recursive: true });
  writeFileSync(outFile, json);
  console.log(`wrote ${outFile} (${cases.length} cases, node ${process.version})`);
  return { ok: true, cases: cases.length, bytes: json.length };
}
