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
import { join } from 'node:path';

import { loadOracle } from '../vendor.mjs';
import { determinismHeader, emitFamily } from '../harness.mjs';

const FAMILY = 'schema-hashes';

/**
 * Extra inputs that are not fixture exports, chosen because each one isolates a
 * step in the pipeline above. Each is a real schema shape from upstream's docs.
 */
const SYNTHETIC = {
  /**
   * The SAME schema as `synthetic:no-index-at-all`, with keys inserted in a different
   * order, and nothing else changed.
   *
   * It previously carried an extra `title` plus a second `properties.age` entry, which
   * made it a different schema, so `key-order-does-not-change-hash` was false as
   * recorded. A permutation has to be a permutation: same key set, same values, only
   * insertion order differs. `normalizeRxJsonSchema` deep-sorts keys, so the hash must
   * be identical to `no-index-at-all`'s.
   */
  'synthetic:key-order-swapped': {
    required: ['id'],
    properties: { id: { maxLength: 100, type: 'string' } },
    primaryKey: 'id',
    type: 'object',
    version: 0,
  },
  /**
   * The negative counterpart for `maxLength`: one digit lower, everything else the
   * same. Emitted as a case so `max-length-change-changes-hash` can resolve both of its
   * endpoints, which it could not before.
   */
  'synthetic:max-length-changed': {
    version: 0,
    type: 'object',
    primaryKey: 'id',
    properties: { id: { type: 'string', maxLength: 99 } },
    required: ['id'],
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

  /**
   * Relations are derived from cases, not from separate inline literals.
   *
   * The previous version built its own `plain` / `reordered` schemas, hashed them, and
   * recorded the result as a relation naming endpoints that were never emitted as
   * cases. That produced a block which could not be checked against the file it lived
   * in, and it had silently accumulated two defects:
   *
   *   - every endpoint name (`plain`, `reordered`, ...) resolved to nothing in
   *     `cases[]`, so a consumer reading `relation.a` found no such case;
   *   - `key-order-swapped` was not a permutation at all. It added a `title` and a
   *     second property, so `key-order-does-not-change-hash` was asserting equality
   *     between two genuinely different schemas, recording `equal: true` alongside a
   *     hash that belonged to only one of them.
   *
   * Resolving both by name from `hashesByName` means a relation can only exist if both
   * of its endpoints were actually emitted, and the assertion at the end of this
   * function fails the generator if anything disagrees.
   */
  const hashesByName = new Map(cases.filter((c) => c.hash).map((c) => [c.name, c.hash]));

  /** Record a relation between two emitted cases, computing `equal` from their hashes. */
  const relate = (why, a, b) => {
    const hashA = hashesByName.get(a);
    const hashB = hashesByName.get(b);
    if (hashA === undefined || hashB === undefined) {
      throw new Error(
        `relation ${why} names a case that was not emitted as a case: ` +
          `${hashA === undefined ? a : b} (have: ${[...hashesByName.keys()].join(', ')})`
      );
    }
    relations.push({
      why,
      a,
      b,
      equal: hashA === hashB,
      // The hash of each endpoint, so a reader never has to guess which side a single
      // `hash` field referred to.
      hashA,
      hashB,
    });
  };

  // Order-insensitivity: the same schema with its keys inserted in a different order.
  relate(
    'key-order-does-not-change-hash',
    'synthetic:no-index-at-all',
    'synthetic:key-order-swapped'
  );
  // An explicitly declared default index must not change the identity.
  relate(
    'explicit-default-index-equals-implicit-default',
    'synthetic:no-index-at-all',
    'synthetic:explicit-default-index'
  );
  // `required` is de-duplicated, so a repeated entry must not change it either.
  relate(
    'redundant-required-does-not-change-hash',
    'synthetic:no-index-at-all',
    'synthetic:redundant-required'
  );
  // The negative counterparts: a semantic change MUST change the identity, otherwise
  // two different schemas share a storage and the hash is too coarse to be useful.
  relate(
    'max-length-change-changes-hash',
    'synthetic:no-index-at-all',
    'synthetic:max-length-changed'
  );
  relate(
    'version-change-changes-hash',
    'synthetic:no-index-at-all',
    'synthetic:version-3'
  );
  relate(
    'key-compression-change-changes-hash',
    'synthetic:no-index-at-all',
    'synthetic:key-compression-on'
  );

  /**
   * Self-check. A relation block that cannot be checked against the cases it lives
   * with is worse than no relation block, because it looks like evidence. This fails
   * the generator rather than emitting a file whose relations do not resolve.
   */
  for (const relation of relations) {
    if (typeof relation.a !== 'string' || typeof relation.b !== 'string') {
      throw new Error(`relation ${relation.why} has a non-string endpoint`);
    }
    if (!hashesByName.has(relation.a)) {
      throw new Error(`relation ${relation.why}: endpoint a=${relation.a} is not a declared case`);
    }
    if (!hashesByName.has(relation.b)) {
      throw new Error(`relation ${relation.why}: endpoint b=${relation.b} is not a declared case`);
    }
    const expectedEqual = hashesByName.get(relation.a) === hashesByName.get(relation.b);
    if (relation.equal !== expectedEqual) {
      throw new Error(
        `relation ${relation.why} claims equal=${relation.equal} but the recorded ` +
          `hashes differ: ${relation.a}=${hashesByName.get(relation.a)} vs ` +
          `${relation.b}=${hashesByName.get(relation.b)}`
      );
    }
    if (relation.hashA !== hashesByName.get(relation.a) || relation.hashB !== hashesByName.get(relation.b)) {
      throw new Error(`relation ${relation.why} recorded a hash that is not its case's hash`);
    }
  }
  // The positive relations must be genuinely positive, and the negative ones genuinely
  // negative; otherwise a whole class of property silently stops being tested.
  const positives = relations.filter((r) => r.why.includes('does-not-change') || r.why.includes('equals-implicit'));
  const negatives = relations.filter((r) => r.why.includes('changes-hash'));
  if (positives.length === 0 || positives.some((r) => !r.equal)) {
    throw new Error('a "does not change the hash" relation is not actually equal');
  }
  if (negatives.length === 0 || negatives.some((r) => r.equal)) {
    throw new Error('a "changes the hash" relation is actually equal');
  }

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
