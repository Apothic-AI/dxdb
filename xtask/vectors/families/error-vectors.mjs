/**
 * Family: error vectors.
 *
 * Every other family records what upstream *produces*. This one records how upstream
 * *refuses*, because a port diverges quietly on error paths: a success is compared
 * field by field, while an error is usually just "it failed", so returning the wrong
 * code, or a panic instead of a typed error, goes unnoticed for months.
 *
 * Two layers are recorded, and the difference between them is the point.
 *
 * 1. **Raised codes** — inputs driven through the bundled upstream, with the error's
 *    observable identity: `code`, `rxdb` marker, `url`, `parameters` keys, and the
 *    thrown constructor. These are the codes a port can be diffed against directly.
 *
 * 2. **Declared codes** — the full `ERROR_MESSAGES` vocabulary from upstream's
 *    dev-mode plugin (219 entries at this pin), with each code's message and docs URL.
 *    That table is the authority on which codes *exist* and what they mean, and it is
 *    cheap to record exhaustively.
 *
 * What is deliberately NOT claimed: that a declared code is reachable from here. Many
 * are raised only by the dev-mode checkers (`SC20`), or by code paths that need a full
 * database, a browser, or a storage driver. `check-schema.ts` raises `SC20` directly
 * but imports `entity-properties.ts`, which imports the whole collection/document
 * layer and its runtime dependencies (`event-reduce-js`, `custom-idle-queue`,
 * `oblivious-set`) — bundling that to reach one checker would make the oracle's
 * dependency surface larger than the library it checks. So each declared code carries
 * `reachable` from this generator, set honestly to `false` where we could not raise it,
 * and a reader must not treat a declared-but-unreachable code as verified behaviour.
 *
 * The codes the task named explicitly (`CI1`, `SC20`, `COL22`, `DXE1`, and the `DB`
 * family) are recorded in both layers where possible, and each carries a note saying
 * exactly why it is or is not in the raised layer.
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

import { loadOracle } from '../vendor.mjs';
import { determinismHeader, emitFamily } from '../harness.mjs';

const FAMILY = 'error-vectors';

/** A minimal valid schema, used as the base for inputs that break one thing. */
const BASE_SCHEMA = () => ({
  version: 0,
  type: 'object',
  primaryKey: 'id',
  properties: { id: { type: 'string', maxLength: 10 } },
  required: ['id'],
});

/**
 * Inputs that make upstream raise, each pinned to the code it is expected to raise.
 *
 * `expect` is asserted, not merely recorded: if a probe stops raising the expected
 * code, the generator fails rather than emitting a case that silently proves nothing.
 */
const RAISED = [
  {
    why: 'primary-key-without-maxLength',
    code: 'SC39',
    /// Upstream requires `maxLength` on the primary key because index strings are
    /// fixed width and sized from it.
    detail:
      'The primary key has no `maxLength`. Index strings are fixed width and the ' +
      'width comes from it, so upstream rejects the schema rather than guessing.',
    rustExpectation: 'SC39-equivalent typed error naming the schema',
    run: (m) => {
      const schema = BASE_SCHEMA();
      delete schema.properties.id.maxLength;
      m.rxSchema.createRxSchema(schema, m.utilsHash.defaultHashSha256);
    },
  },
  {
    why: 'index-names-a-field-not-in-schema',
    code: 'CI1',
    detail:
      'An index names a field path the schema does not declare. `parameters.fieldName` ' +
      'carries the offending path, which is what makes the error actionable.',
    rustExpectation: 'CI1-equivalent typed error with the field name',
    run: (m) => {
      const schema = m.rxSchema.createRxSchema(BASE_SCHEMA(), m.utilsHash.defaultHashSha256).jsonSchema;
      m.customIndex.getIndexStringLength(schema, ['nope']);
    },
  },
  {
    why: 'lower-bound-on-unencodable-type',
    code: 'CI2',
    detail:
      'A bound helper is asked for the index string of a field whose schema type has ' +
      'no index encoding. `parameters.type` carries the type, which is the whole ' +
      'diagnosis.',
    rustExpectation: 'CI2-equivalent typed error naming the type',
    run: (m) => {
      const schema = BASE_SCHEMA();
      schema.properties.weird = { type: 'object' };
      const built = m.rxSchema.createRxSchema(schema, m.utilsHash.defaultHashSha256).jsonSchema;
      m.customIndex.getStartIndexStringFromLowerBound(built, ['weird', 'id'], ['x', 'y']);
    },
  },
  {
    why: 'upper-bound-on-unencodable-type',
    code: 'CI2',
    detail: 'The upper-bound helper has its own copy of the type switch.',
    rustExpectation: 'CI2-equivalent typed error naming the type',
    run: (m) => {
      const schema = BASE_SCHEMA();
      schema.properties.weird = { type: 'object' };
      const built = m.rxSchema.createRxSchema(schema, m.utilsHash.defaultHashSha256).jsonSchema;
      m.customIndex.getStartIndexStringFromUpperBound(built, ['weird', 'id'], ['x', 'y']);
    },
  },
  {
    why: 'unknown-mango-operator',
    // No RxDB code at all: mingo throws its own error. Recorded because a port that
    // returns a typed RxDB error here would be *more* structured than upstream, which
    // is a divergence a caller comparing codes would notice.
    code: null,
    expectClass: 'MingoError2',
    detail:
      'A selector uses an operator outside the eighteen RxDB registers in ' +
      '`getMingoQuery`. mingo throws its own `MingoError2`, which carries no RxDB code ' +
      'and no `rxdb` marker. There is no QU* code for this upstream.',
    rustExpectation: 'an error distinguishable from a typed RxDB error, not a QU* code',
    run: (m) => {
      const schema = m.rxSchema.createRxSchema(BASE_SCHEMA(), m.utilsHash.defaultHashSha256).jsonSchema;
      const q = m.queryHelper.normalizeMangoQuery(schema, { selector: { id: { $frobnicate: 1 } } });
      m.queryHelper.getQueryMatcher(schema, q);
    },
  },
  {
    why: 'top-level-negative-operator',
    code: null,
    expectClass: 'MingoError2',
    detail:
      '`$not` is field-level in Mango, not a top-level operator. mingo rejects it before ' +
      'RxDB can see it, so again there is no RxDB code.',
    rustExpectation: 'an error distinguishable from a typed RxDB error',
    run: (m) => {
      const schema = m.rxSchema.createRxSchema(BASE_SCHEMA(), m.utilsHash.defaultHashSha256).jsonSchema;
      const q = m.queryHelper.normalizeMangoQuery(schema, { selector: { $not: { id: { $eq: 'a' } } } });
      m.queryHelper.getQueryMatcher(schema, q);
    },
  },
  {
    why: 'invalid-regex-pattern',
    code: null,
    expectClass: 'SyntaxError',
    detail:
      'A `$regex` whose source the ECMAScript grammar rejects surfaces as a plain ' +
      '`SyntaxError` from `RegExp`, not an RxDB error.',
    rustExpectation: 'an error distinguishable from a typed RxDB error',
    run: (m) => {
      const schema = m.rxSchema.createRxSchema(BASE_SCHEMA(), m.utilsHash.defaultHashSha256).jsonSchema;
      const q = m.queryHelper.normalizeMangoQuery(schema, { selector: { id: { $regex: '[' } } });
      m.queryHelper.getQueryMatcher(schema, q);
    },
  },
  {
    why: 'prepare-query-without-sort',
    code: 'SNH',
    detail:
      '`prepareQuery` requires a sort. This is the "should never happen" code, which ' +
      'is exactly the kind a port tends to replace with a panic — upstream still ' +
      'returns a typed error carrying the query.',
    rustExpectation: 'SNH-equivalent typed error, not a panic',
    run: (m) => {
      const schema = m.rxSchema.createRxSchema(BASE_SCHEMA(), m.utilsHash.defaultHashSha256).jsonSchema;
      m.queryHelper.prepareQuery(schema, { selector: {}, skip: 0 });
    },
  },
];

/**
 * Codes the task named, with an honest note about whether this generator can raise
 * them. These entries exist so the "why is X missing from the raised layer" question
 * has an answer in the data rather than only in a commit message.
 */
const NAMED = {
  CI1: { raised: true },
  CI2: { raised: true },
  SC39: { raised: true },
  SNH: { raised: true },
  SC20: {
    raised: false,
    why: 'Raised by `plugins/dev-mode/check-schema.ts:376`, which imports ' +
      '`entity-properties.ts` -> `rx-collection`/`rx-database`/`rx-document`. ' +
      'Bundling that layer costs event-reduce-js, custom-idle-queue and oblivious-set. ' +
      'The code and message are recorded in the declared layer; the raiser is not ' +
      'reachable from the core oracle. A port must still return SC20 for a non-string ' +
      'index entry, and `index-strings` records upstream refusing that shape.',
  },
  COL22: {
    raised: false,
    why: 'Raised by `.bulkInsert()`/`.bulkUpsert()` inside a RxCollection, which is ' +
      'the database layer rather than the storage layer. The storage-level equivalent ' +
      '(a 409 conflict for a duplicate id) is covered by bulk-write-categorization.',
  },
  DXE1: {
    raised: false,
    why: 'Raised by the Dexie storage plugin, which needs a browser IndexedDB. ' +
      'Not reachable in Node without a fake IndexedDB, and the plugin is not part of ' +
      'the bundled oracle surface.',
  },
  'DB family': {
    raised: false,
    why: 'Raised by RxDatabase setup (collection limits, addCollections conflicts, ' +
      'prepare() state). Needs a live database; out of scope for a data-file oracle.',
  },
};

export async function run({ check, repo }) {
  const oracle = await loadOracle();
  const outFile = join(repo, 'testdata', 'vectors', `${FAMILY}.json`);

  // --- Layer 1: codes actually raised by upstream, with full observable identity.
  const raisedCases = [];
  for (const probe of RAISED) {
    let observed = null;
    try {
      probe.run(oracle.modules);
    } catch (err) {
      observed = {
        errorName: err && err.constructor ? err.constructor.name : typeof err,
        errorCode: err && typeof err.code === 'string' ? err.code : null,
        rxdbError: !!(err && err.rxdb),
        errorUrl: err && typeof err.url === 'string' ? err.url : null,
        // `parameters` is upstream's structured payload. Keys and values are recorded
        // because a port that returns the right code with the wrong parameters is
        // still wrong for any caller that reads them.
        parameters: err && err.parameters ? err.parameters : null,
        parameterKeys: err && err.parameters ? Object.keys(err.parameters).sort() : [],
        rawMessage: String((err && err.message) || err).slice(0, 400),
      };
    }

    if (!observed) {
      throw new Error(
        `probe ${probe.why} was expected to raise ${probe.code ?? probe.expectClass} but did not. ` +
          'A case that records no error would claim the input is valid, which is worse ' +
          'than omitting it.'
      );
    }
    if (probe.code === null) {
      // No RxDB code exists upstream for this failure. Assert the thrown class and that
      // it is genuinely untyped, so the "no code" claim is checked rather than assumed.
      if (observed.errorName !== probe.expectClass) {
        throw new Error(
          `probe ${probe.why} threw ${observed.errorName}, expected ${probe.expectClass}`
        );
      }
      if (observed.rxdbError || observed.errorCode !== null) {
        throw new Error(
          `probe ${probe.why} unexpectedly produced a typed RxDB error ` +
            `(${observed.errorCode}); it was recorded as untyped`
        );
      }
    } else if (observed.errorCode !== probe.code) {
      throw new Error(
        `probe ${probe.why} raised ${observed.errorCode || observed.errorName}, ` +
          `expected ${probe.code}. The upstream contract changed, or the probe is wrong.`
      );
    }

    raisedCases.push({
      why: probe.why,
      expectedCode: probe.code,
      detail: probe.detail,
      rustExpectation: probe.rustExpectation,
      ...observed,
    });
  }

  // --- Layer 2: the declared vocabulary, exhaustively.
  const messages = oracle.modules.devModeErrorMessages.ERROR_MESSAGES;
  const raisedCodes = new Set(
    raisedCases.map((c) => c.errorCode).filter((code) => code !== null)
  );
  const declaredCodes = Object.keys(messages)
    .sort()
    .map((code) => {
      const entry = messages[code] || {};
      return {
        code,
        message: typeof entry.message === 'string' ? entry.message : null,
        docs: typeof entry.docs === 'string' ? entry.docs : null,
        // `true` only for the codes this generator actually raised, i.e. the ones a
        // port can be diffed against from this file alone.
        reachable: raisedCodes.has(code),
      };
    });

  // Every raised code must exist in the declared vocabulary, or one of the two layers
  // is describing a different upstream.
  for (const code of raisedCodes) {
    if (!Object.prototype.hasOwnProperty.call(messages, code)) {
      throw new Error(
        `raised code ${code} is not in upstream's ERROR_MESSAGES table (${declaredCodes.length} entries). ` +
          'The two layers disagree about which codes exist.'
      );
    }
  }

  const doc = {
    ...oracle.header(
      'Upstream error behaviour: the codes RxDB raises for invalid operations, with the ' +
        'observable identity of each error (code, rxdb marker, docs URL, parameters), plus ' +
        'the full declared error vocabulary from the dev-mode plugin. Every other family ' +
        'records successful output; error paths are where a port diverges quietly, because ' +
        '"it failed" is usually accepted as good enough. See `namedCodes` for why some ' +
        'well-known codes (SC20, COL22, DXE1, DB*) are declared but not raised here.'
    ),
    format: 'invalid-operation->error{code,url,parameters}',
    determinism: determinismHeader(),
    raisedCaseCount: raisedCases.length,
    /** Raised cases that carry a real RxDB code, i.e. a port can be diffed by code. */
    typedCaseCount: raisedCases.filter((c) => c.errorCode !== null).length,
    /** Raised cases where upstream throws an untyped error (no RxDB code exists). */
    untypedCaseCount: raisedCases.filter((c) => c.errorCode === null).length,
    declaredCodeCount: declaredCodes.length,
    /** Codes from the task's list, with an honest reachability note for each. */
    namedCodes: NAMED,
    /** Codes this generator raised, with their error identity. */
    raisedCases,
    /** The whole vocabulary, so a port can be checked for returning a *known* code. */
    declaredCodes,
  };

  return emitFamily({
    outFile,
    doc,
    check,
    label: FAMILY,
    caseCount: doc.raisedCaseCount,
  });
}
