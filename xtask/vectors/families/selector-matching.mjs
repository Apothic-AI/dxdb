/**
 * Family: selector matching.
 *
 * Upstream: `src/rx-query-helper.ts` (`getQueryMatcher`) and
 * `src/rx-query-mingo.ts` (`getMingoQuery`), which configures a mingo `Query`
 * with exactly eighteen operators.
 *
 * This is the differential oracle for the M1 matcher, and the property test in
 * M1 consumes it: given a document corpus and a selector, which document ids
 * match. It is deliberately *not* a test of our reading of Mango semantics — the
 * expected answers come from mingo through upstream's own wiring.
 *
 * Two upstream details the vectors capture, both of which a from-scratch matcher
 * tends to get wrong:
 *
 *   - The operator set is closed. `$where`, `$text`, `$all`, `$bitsAllSet` and
 *     friends are *not* registered, so they behave differently from MongoDB
 *     rather than throwing. Those are recorded as behaviour, because "agrees with
 *     mingo as configured by RxDB" is the claim, not "agrees with MongoDB".
 *   - Selectors reach mingo *after* `normalizeMangoQuery`, so shorthand equality
 *     has already been rewritten to `$eq` and the matcher sees the normalised
 *     form. Both forms are recorded where they differ in meaning.
 *
 * The corpus is generated from a fixed seed and includes the awkward values:
 * empty strings, zero, negatives, unicode, arrays, nested objects, deleted flags,
 * and missing fields (absent, not `undefined`), because "field is absent" versus
 * "field is null" is the classic divergence.
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

import { loadOracle } from '../vendor.mjs';

const FAMILY = 'selector-matching';

/** Seeded PRNG: the corpus must be byte-identical on every run. */
function createPrng(seed = 0x5e1ec7) {
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

const next = createPrng();
const pick = (list) => list[Math.floor(next() * list.length) % list.length];
const chance = (p) => next() < p;

/**
 * Value pools. The presence of both `''` and `null` and *absent* is the point:
 * `$exists`, `$eq: null` and `$ne` all disagree about those three.
 */
const FIRST_NAMES = ['alice', 'bob', 'carol', '', 'Alice', 'äöü', '😊', 'a b', 'null'];
const LAST_NAMES = ['smith', 'jones', '', 'Smith-Jr', 'o\'neil', 'z'];
const AGES = [0, 1, 20, 21, 40, 150, -1, 3.5, 1000];
const TAGS = [[], ['a'], ['a', 'b'], ['b', 'c'], ['ä']];
const NESTED_STREETS = ['main', 'oak', '', 'side'];
const BOOLS = [true, false];

function generateDocs(count) {
  const docs = [];
  for (let i = 0; i < count; i++) {
    const doc = { id: 'doc-' + String(i).padStart(3, '0') };

    // Each optional field is included or omitted independently, so the corpus
    // contains every combination of present/absent for the fields selectors touch.
    if (!chance(0.12)) doc.firstName = pick(FIRST_NAMES);
    if (!chance(0.15)) doc.lastName = pick(LAST_NAMES);
    if (!chance(0.1)) doc.age = pick(AGES);
    if (!chance(0.2)) doc.tags = pick(TAGS);
    if (!chance(0.25)) doc.address = { street: pick(NESTED_STREETS), number: Math.floor(next() * 5) };
    if (!chance(0.3)) doc.active = pick(BOOLS);
    // Deliberate member of the "explicitly null" class, distinct from absent.
    if (chance(0.15)) doc.firstName = null;

    doc._deleted = chance(0.2);
    doc._rev = '1-' + doc.id;
    doc._meta = { lwt: 1000 + i };
    doc._attachments = {};
    docs.push(doc);
  }
  return docs;
}

/**
 * Selectors are grouped by the operator they exercise, so a failure names the
 * operator rather than just "some selector". Values are drawn from the same pools
 * as the corpus so that some match and some do not; a selector that matches
 * everything or nothing proves very little.
 */
function selectors() {
  const list = [];
  const add = (why, selector) => list.push({ why, selector });

  // Equality, including the absent/null boundary.
  add('eq-field', { firstName: { $eq: 'alice' } });
  add('eq-null', { firstName: { $eq: null } });
  add('eq-zero', { age: { $eq: 0 } });
  add('eq-empty-string', { firstName: { $eq: '' } });
  add('eq-boolean', { active: { $eq: true } });
  add('eq-deleted-flag', { _deleted: { $eq: false } });
  add('eq-missing-field', { notAField: { $eq: 1 } });
  add('eq-nested-path', { 'address.street': { $eq: 'main' } });
  add('eq-array', { tags: { $eq: ['a'] } });

  // Comparison.
  add('gt-age', { age: { $gt: 20 } });
  add('gte-age', { age: { $gte: 20 } });
  add('lt-age', { age: { $lt: 20 } });
  add('lte-age', { age: { $lte: 20 } });
  add('gt-negative', { age: { $gt: -1 } });
  add('gt-float', { age: { $gt: 3.4 } });
  add('range-both', { age: { $gte: 1, $lte: 40 } });
  add('range-inverted', { age: { $gte: 40, $lte: 1 } });
  add('gt-empty-string', { firstName: { $gt: '' } });

  // Negation.
  add('ne-string', { firstName: { $ne: 'alice' } });
  add('ne-null', { firstName: { $ne: null } });
  add('nin-list', { firstName: { $nin: ['alice', 'bob'] } });
  add('not-gt', { age: { $not: { $gt: 20 } } });
  add('not-eq', { firstName: { $not: { $eq: 'alice' } } });

  // Membership.
  add('in-strings', { firstName: { $in: ['alice', 'bob', ''] } });
  add('in-numbers', { age: { $in: [0, 20, 150] } });
  add('in-with-null', { firstName: { $in: [null] } });
  add('in-empty', { age: { $in: [] } });
  add('in-mixed', { age: { $in: [20, 'twenty'] } });

  // Existence and type.
  add('exists-true', { firstName: { $exists: true } });
  add('exists-false', { firstName: { $exists: false } });
  add('exists-nested', { 'address.street': { $exists: true } });
  add('type-string', { firstName: { $type: 'string' } });
  add('type-number', { age: { $type: 'number' } });
  add('type-null', { firstName: { $type: 'null' } });
  add('type-array', { tags: { $type: 'array' } });
  add('type-boolean', { active: { $type: 'bool' } });

  // Arrays.
  add('size-one', { tags: { $size: 1 } });
  add('size-two', { tags: { $size: 2 } });
  add('size-zero', { tags: { $size: 0 } });
  add('elemMatch', { tags: { $elemMatch: { $eq: 'a' } } });
  // An equality against a scalar matches an array containing that scalar; this is
  // the mingo/MongoDB array-member rule and a common port bug.
  add('array-contains-scalar', { tags: { $eq: 'a' } });

  // Regular expressions, including a case-insensitive one.
  add('regex-prefix', { firstName: { $regex: '^a' } });
  add('regex-ci', { firstName: { $regex: '^A', $options: 'i' } });
  // Must match something: an empty match set proves almost nothing. `s` appears in
  // several surname pool entries and in none of the nested/array values.
  add('regex-substring', { lastName: { $regex: 's' } });
  add('regex-unicode', { firstName: { $regex: '^ä' } });

  // Arithmetic.
  add('mod-even', { age: { $mod: [2, 0] } });
  add('mod-odd', { age: { $mod: [2, 1] } });

  // Logical composition, including nesting.
  add('and-two', { $and: [{ age: { $gt: 20 } }, { active: { $eq: true } }] });
  add('or-two', { $or: [{ firstName: { $eq: 'alice' } }, { active: { $eq: false } }] });
  add('nor', { $nor: [{ firstName: { $eq: 'alice' } }] });
  add('and-inside-or', {
    $or: [{ $and: [{ age: { $gt: 20 } }, { active: { $eq: true } }] }, { firstName: { $eq: null } }],
  });
  add('or-inside-and', {
    $and: [{ $or: [{ age: { $gt: 100 } }, { age: { $lt: 2 } }] }, { _deleted: { $eq: false } }],
  });
  add('and-empty', { $and: [] });
  add('or-empty', { $or: [] });
  add('not-outside-field', { $not: { age: { $gt: 20 } } });

  // Both-deleted case: storage queries almost always constrain this, and getting
  // it backwards silently includes tombstones in every result.
  add('deleted-tombstones', { _deleted: { $eq: true } });

  // The selector replication itself issues, from `getChangedDocumentsSinceQuery`:
  // an `$or` over `_meta.lwt` with the primary key as the tiebreaker, plus a hint
  // field outside the `$or`. A matcher that gets the tiebreaker wrong silently
  // skips documents with an equal lwt, which is a data-loss bug in M3.
  add('replication-changed-since', {
    $or: [{ '_meta.lwt': { $gt: 1020 } }, { '_meta.lwt': { $eq: 1020 }, id: { $gt: 'doc-010' } }],
    '_meta.lwt': { $gte: 1020 },
  });

  // Absence semantics on a nested path, where "parent object missing" and
  // "parent present but child missing" must agree.
  add('exists-false-nested', { 'address.street': { $exists: false } });
  add('eq-nested-missing-number', { 'address.number': { $eq: 0 } });

  // An `$and` arm that can never match, combined with one that can: a matcher
  // that short-circuits incorrectly returns everything or nothing here.
  add('and-with-impossible-arm', {
    $and: [{ age: { $gt: 1000 } }, { _deleted: { $eq: false } }],
  });

  // `$elemMatch` with a comparison operator, not just equality.
  add('elemMatch-comparison', { tags: { $elemMatch: { $gt: 'a' } } });

  // Operators upstream does NOT register. Recorded because "behaves like mingo as
  // configured by RxDB" is the contract, and these must not be assumed to work.
  add('unregistered-$all', { tags: { $all: ['a'] } });
  add('unregistered-$where', { $where: 'true' });

  return list;
}

/**
 * Selectors upstream refuses, and why. A `null` lookup means the selector was
 * expected to work, so an unexpected throw is reported rather than filed as
 * behaviour.
 */
const EXPECTED_SELECTOR_REJECTIONS = {
  'not-outside-field': {
    class: 'upstream-rejects-negative-logical-operator',
    detail:
      '$not is a field-level operator in Mango, not a top-level one. Upstream ' +
      'passes the selector to mingo unmodified, which rejects it as an unknown ' +
      'top-level operator.',
    rustExpectation: 'reject with a typed error naming the operator',
  },
  'unregistered-$all': {
    class: 'upstream-does-not-register-operator',
    detail:
      '$all exists in MongoDB but RxDB registers a closed set of eighteen ' +
      'operators in src/rx-query-mingo.ts. $all is not among them, so mingo ' +
      'throws "unknown query operator" instead of behaving like MongoDB.',
    rustExpectation: 'reject with a typed error, do not silently implement MongoDB parity',
  },
  'unregistered-$where': {
    class: 'upstream-does-not-register-operator',
    detail:
      '$where evaluates a JavaScript expression in MongoDB. Registering it would ' +
      'mean executing caller-supplied code, so RxDB does not, and mingo rejects it.',
    rustExpectation: 'reject with a typed error; never evaluate code from a selector',
  },
};

export async function run({ check, repo }) {
  const oracle = await loadOracle();
  const { queryHelper, rxSchema, utilsHash, fixtures } = oracle.modules;

  const outFile = join(repo, 'testdata', 'vectors', `${FAMILY}.json`);

  const schema = rxSchema.createRxSchema(
    JSON.parse(JSON.stringify(fixtures.human)),
    utilsHash.defaultHashSha256
  ).jsonSchema;

  const docs = generateDocs(64);

  const cases = [];
  const unexpectedRejections = [];
  for (const { why, selector } of selectors()) {
    let normalized;
    let matchingIds;
    let error;
    try {
      normalized = queryHelper.normalizeMangoQuery(schema, { selector });
      // `getQueryMatcher` requires a sort even though matching does not use it;
      // that requirement is itself part of the contract.
      const matcher = queryHelper.getQueryMatcher(schema, normalized);
      matchingIds = docs.filter((doc) => matcher(doc)).map((doc) => doc.id);
    } catch (err) {
      error = err;
    }

    if (error) {
      /**
       * Upstream rejects two families of selector here and the distinction
       * matters: `$not` is not a top-level operator at all (it is field-level
       * only), and `$all` / `$where` are real MongoDB operators that RxDB simply
       * does not register in `getMingoQuery`. Both are recorded as first-class
       * cases so the port reproduces the refusal rather than guessing at MongoDB
       * parity it does not have.
       */
      const message = String((error && error.message) || error)
        .split('\n')
        .map((line) => line.trim())
        .filter(Boolean)
        .join(' ');
      const expected = EXPECTED_SELECTOR_REJECTIONS[why] || null;
      cases.push({
        why,
        selector,
        kind: 'rejection',
        rejected: true,
        errorName: error && error.constructor ? error.constructor.name : typeof error,
        errorCode: error && typeof error.code === 'string' ? error.code : null,
        rxdbError: !!(error && error.rxdb),
        rawMessage: message,
        upstreamBehaviour: expected,
        rustExpectation: expected ? expected.rustExpectation : 'investigate: this selector was expected to match',
      });
      if (!expected) {
        unexpectedRejections.push({ why, rawMessage: message });
      }
      continue;
    }

    cases.push({
      why,
      selector,
      // What mingo actually received. For shorthand selectors this differs from
      // the input, which is the point of recording it.
      normalizedSelector: normalized.selector,
      matchingIds,
      matchCount: matchingIds.length,
    });
  }

  /**
   * The same corpus is also exported as documents, so a Rust property test can
   * run its own matcher over exactly these inputs without regenerating them.
   */
  const doc = {
    ...oracle.header(
      'Selector matching from upstream `getQueryMatcher` (mingo configured by ' +
        'src/rx-query-mingo.ts) over a generated document corpus (PLAN.md 7.2). ' +
        '`matchingIds` is the differential oracle for the M1 matcher and the input to ' +
        'its property test. `corpus` is the exact document set, so the Rust side does ' +
        'not have to reproduce the generator.'
    ),
    format: 'documents+selector->matchingIds',
    determinism: {
      ...oracle.determinism,
      corpus: 'generated from xorshift128 seeded with 0x5e1ec7, 64 documents',
    },
    /** The closed operator set from src/rx-query-mingo.ts, readable at a glance. */
    registeredOperators: [
      '$elemMatch', '$eq', '$nor', '$exists', '$regex', '$and', '$gt', '$gte',
      '$in', '$lt', '$lte', '$ne', '$nin', '$mod', '$not', '$or', '$size', '$type',
    ],
    corpus: docs,
    /**
     * `caseCount` includes `kind: "rejection"` cases: upstream refusing a
     * selector is behaviour the port must reproduce, not missing coverage.
     */
    caseCount: cases.length,
    matchingCaseCount: cases.length - cases.filter((c) => c.kind === 'rejection').length,
    rejectionCaseCount: cases.filter((c) => c.kind === 'rejection').length,
    unexpectedRejections,
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
