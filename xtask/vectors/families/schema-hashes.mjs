/**
 * Family: schema hashes.
 *
 * Upstream: `src/rx-schema.ts` (`createRxSchema`, the cached `hash` getter),
 * `src/rx-schema-helper.ts` (`fillWithDefaultSettings`, `normalizeRxJsonSchema`),
 * `src/plugins/utils/utils-hash.ts` (`nativeSha256`).
 *
 * A schema hash is an identity: storages are keyed by it, replication uses it to
 * decide that two sides agree, and the internal store keys collections by it. So
 * the hash has to be reproduced from the same bytes, which means the steps before
 * the hash matter as much as the hash:
 *
 *   1. `fillWithDefaultSettings` injects `_rev`, `_attachments`, `_deleted`,
 *      `_meta`, forces `additionalProperties: false`, defaults `keyCompression`,
 *      `version`, `required` and `encrypted`, and rewrites **every** index to
 *      prepend `_deleted` and append the primary key. It also appends the
 *      `_meta.lwt` index every schema needs for `getChangedDocumentsSince`.
 *   2. `normalizeRxJsonSchema` deep-sorts the object keys.
 *   3. `JSON.stringify` of that object is hashed with SHA-256, hex encoded.
 *
 * Because of (1) and (2), two schemas that differ only in key order or in
 * redundant `required` entries hash identically, and a schema that omits an index
 * does not hash like one that declares the same index explicitly. Vectors record
 * the input, the fully defaulted schema, and the hash, so a Rust port can be
 * checked at whichever of those three levels it disagrees on.
 */
import { dirname, join } from 'node:path';

import { loadOracle } from '../vendor.mjs';
import { determinismHeader, emitFamily } from '../harness.mjs';

const FAMILY = 'schema-hashes';

/**
 * Extra inputs that are not fixture exports, chosen because each one isolates a
 * step in the pipeline above. Each is a real schema shape from upstream's docs.
 */
const SYNTHETIC = {
  'synthetic:key-order-swapped': {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    properties: {
      age: { maximum: 150, minimum: 0, type: 'integer', multipleOf: 1 },
      id: { maxLength: 100, type: 'string' },
    },
    required: ['id'],
    title: 'key order test',
  },
  'synthetic:explicit-default-index': {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    properties: {
      id: { type: 'string', maxLength: 100 },
    },
    required: ['id'],
    indexes: [['_deleted', 'id']],
  },
  'synthetic:no-index-at-all': {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    properties: {
      id: { type: 'string', maxLength: 100 },
    },
    required: ['id'],
  },
  'synthetic:redundant-required': {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    properties: {
      id: { type: 'string', maxLength: 100 },
    },
    // Duplicated and out of order on purpose: `required` is de-duplicated and
    // meta fields are appended, so this must hash like the schema above.
    required: ['id', 'id'],
  },
  'synthetic:key-compression-on': {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    keyCompression: true,
    properties: {
      id: { type: 'string', maxLength: 100 },
      longFieldName: { type: 'string' },
    },
    required: ['id'],
  },
  'synthetic:composite-primary': {
    version: 0,
    type: 'object',
    primaryKey: { key: 'id', fields: ['firstName', 'lastName'], separator: '|' },
    properties: {
      id: { type: 'string', maxLength: 200 },
      firstName: { type: 'string', maxLength: 100 },
      lastName: { type: 'string', maxLength: 100 },
    },
    required: ['firstName', 'lastName'],
  },
  'synthetic:attachments-enabled': {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    attachments: {},
    properties: {
      id: { type: 'string', maxLength: 100 },
    },
    required: ['id'],
  },
  'synthetic:encrypted-field': {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    properties: {
      id: { type: 'string', maxLength: 100 },
      secret: { type: 'string' },
    },
    required: ['id'],
    encrypted: ['secret'],
  },
  'synthetic:version-3': {
    version: 3,
    type: 'object',
    primaryKey: 'id',
    properties: {
      id: { type: 'string', maxLength: 100 },
    },
    required: ['id'],
  },
};

export async function run({ check, repo }) {
  const oracle = await loadOracle();
  const { rxSchema, utilsHash, fixtures } = oracle.modules;

  const outFile = join(repo, 'testdata', 'vectors', `${FAMILY}.json`);

  const fixtureSchemas = Object.entries(fixtures).filter(
    ([, value]) => value && typeof value === 'object' && value.primaryKey
  );

  const inputs = [...fixtureSchemas.map(([name, schema]) => ({ name, schema, origin: 'fixture' }))];
  for (const [name, schema] of Object.entries(SYNTHETIC)) {
    inputs.push({ name, schema, origin: 'synthetic' });
  }
  inputs.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));

  const cases = [];
  const failures = [];

  for (const { name, schema, origin } of inputs) {
    // Deep clone: `fillWithDefaultSettings` mutates its argument's nested objects
    // and the fixtures are frozen in dev mode, so passing the export directly
    // would either throw or poison the next reader of `fixtures`.
    const literal = JSON.parse(JSON.stringify(schema));
    let defaulted;
    let hash;
    let error;
    try {
      const rx = rxSchema.createRxSchema(literal, utilsHash.defaultHashSha256);
      hash = await rx.hash;
      defaulted = rx.jsonSchema;
    } catch (err) {
      error = String((err && err.message) || err).split('\n')[0];
    }

    if (error) {
      // A schema upstream cannot create is data too; a port must reject it.
      cases.push({ name, origin, input: literal, rejected: error });
      failures.push(name);
      continue;
    }

    // The hash is of `JSON.stringify(defaulted)`. Record that exact string's
    // length and the defaulted object, so a mismatch can be localised to
    // defaulting versus hashing instead of only "hash differs".
    const hashInput = JSON.stringify(defaulted);

    cases.push({
      name,
      origin,
      input: literal,
      hash,
      hashInputLength: hashInput.length,
      // Full defaulted schema. This is what the hash covers, and it is the part
      // that pins `fillWithDefaultSettings` (injected meta properties, forced
      // additionalProperties, rewritten indexes) which a hash alone would only
      // catch as a mismatch.
      defaultedSchema: defaulted,
    });
  }

  /**
   * Hash stability properties. These are the invariants M1's schema layer is
   * expected to satisfy, and stating them as vectors means a port that hashes
   * key order into the identity is caught here instead of in a storage test.
   */
  const relations = [];

  const hashOf = async (literal) => {
    const rx = rxSchema.createRxSchema(JSON.parse(JSON.stringify(literal)), utilsHash.defaultHashSha256);
    return rx.hash;
  };
  const plain = {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    properties: { id: { type: 'string', maxLength: 100 } },
    required: ['id'],
  };
  const plainHash = await hashOf(plain);
  const reordered = {
    required: ['id'],
    primaryKey: 'id',
    properties: { id: { maxLength: 100, type: 'string' } },
    type: 'object',
    version: 0,
  };
  relations.push({
    why: 'key-order-does-not-change-hash',
    a: 'plain',
    b: 'reordered',
    equal: plainHash === (await hashOf(reordered)),
    hash: plainHash,
  });

  const explicitIndex = { ...plain, indexes: [['_deleted', 'id']] };
  relations.push({
    why: 'explicit-default-index-equals-implicit-default',
    a: 'plain',
    b: 'explicitIndex',
    equal: plainHash === (await hashOf(explicitIndex)),
    hash: plainHash,
  });

  const redundantRequired = { ...plain, required: ['id', 'id'] };
  relations.push({
    why: 'redundant-required-does-not-change-hash',
    a: 'plain',
    b: 'redundantRequired',
    equal: plainHash === (await hashOf(redundantRequired)),
    hash: plainHash,
  });

  // The counterpart: a semantic change MUST change the hash, otherwise the
  // identity is too coarse and two different schemas share a storage.
  const differentMaxLength = {
    ...plain,
    properties: { id: { type: 'string', maxLength: 99 } },
  };
  relations.push({
    why: 'max-length-change-changes-hash',
    a: 'plain',
    b: 'differentMaxLength',
    equal: plainHash === (await hashOf(differentMaxLength)),
    hash: await hashOf(differentMaxLength),
  });

  const versionBump = { ...plain, version: 1 };
  relations.push({
    why: 'version-change-changes-hash',
    a: 'plain',
    b: 'versionBump',
    equal: plainHash === (await hashOf(versionBump)),
    hash: await hashOf(versionBump),
  });

  // Non-string inputs reach the hasher too (replication hashes a joined string,
  // not a schema). Recorded so a port cannot assume UTF-8-only ASCII.
  const hashStringInputs = [
    '',
    'abc',
    'äöü',
    '😊',
    'a'.repeat(1000),
  ];
  const hashStringCases = [];
  for (const input of hashStringInputs) {
    hashStringCases.push({ input, sha256: await utilsHash.nativeSha256(input) });
  }

  const doc = {
    ...oracle.header(
      'Schema hashes from upstream src/rx-schema.ts for every upstream fixture schema and ' +
        'a set of synthetic schemas that isolate one defaulting step each (PLAN.md 7.2, ' +
        'GATES tier 2). `hash` is the value of `RxSchema.hash`; `defaultedSchema` is the ' +
        'exact object that was JSON.stringify-ed and hashed, so a mismatch can be ' +
        'attributed to schema defaulting or to hashing instead of just "hash differs".'
    ),
    format: 'schema->{defaultedSchema, sha256}',
    determinism: determinismHeader(),
    algorithm: 'sha256 hex of JSON.stringify(defaultedSchema)',
    caseCount: cases.length,
    failedSchemas: failures,
    cases,
    relations,
    hashStringCases,
  };

  return emitFamily({ outFile, doc, check, label: FAMILY, caseCount: cases.length });
}
