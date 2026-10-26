/**
 * Operator-interaction corpus: compound selectors on the same field, from upstream.
 *
 * Run:  node crates/dxdb-query/tests/support/operator-interactions.mjs [--check]
 *
 * # Why a second axis on top of `matcher-fuzz.json`
 *
 * `matcher-fuzz.json` generates one operator (or one nested clause) per field and asks which
 * documents match. That covers the operator *vocabulary*. It does not systematically cover
 * operators *composed on the same field and the same document*, which is where the semantics
 * interact rather than merely intersect:
 *
 *   - `$gt` + `$lte` on one field must behave as a half-open interval, and the boundary
 *     matters: `{f: {$gt: 1, $lte: 3}}` must exclude 1 and include 3.
 *   - `$in` intersected with a range must be the intersection, not either side alone.
 *   - `$elemMatch` + `$size` on one array: both must hold for the *same* document, and
 *     `$size` constrains the array while `$elemMatch` constrains an element.
 *   - `$not` wrapping a compound selector is a negation of the conjunction, not of one arm.
 *   - `$exists` + `$eq` on the same field, where the field's presence and its value interact.
 *   - `$regex` + `$options` alongside a range on the same field.
 *
 * # The absent / null / undefined three-way
 *
 * This is the axis the leader predicted a bug on, and it is the case where JavaScript's type
 * model and Rust's differ most. Mango has *three* states for a field and treats them
 * differently per operator:
 *
 *   ABSENT     the key is not in the document object
 *   NULL       the key is present with value `null`
 *   UNDEFINED  a JavaScript value with no JSON representation at all
 *
 * `undefined` cannot be written in JSON, so the corpus records the two states JSON can
 * express and, separately, records a probe whose *intent* is undefined so the port's handling
 * of "the third state cannot arrive from JSON" is documented rather than assumed. Upstream's
 * matrix, which the Rust test asserts against:
 *
 *   selector                 absent   null
 *   $exists:true               no     yes
 *   $exists:false             yes     no
 *   $eq:null                  yes     yes
 *   $ne:null                   no     no
 *   $type:'null'               no     yes
 *   $type:'undefined'         yes     yes
 *   $gte:null                  no     yes
 *   $in:[null]                yes     yes
 *   $not:{$exists:true}       yes     no
 *
 * Each of those is a separate case below, and each is combined with a second operator where
 * the combination is what makes the distinction observable.
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const VECTORS_DIR = join(here, '..', '..', '..', '..', 'xtask', 'vectors');
const OUT = join(here, 'operator-interactions.json');

/**
 * The document corpus.
 *
 * Hand-built rather than random, because the axis under test is the *state* of one field,
 * not the shape of the document. Every document differs only in how `f` is populated, so a
 * divergence localises to the state rather than to some unrelated property.
 *
 * `$state` records the intended state (absent / null / value), because JSON cannot express
 * "the key is absent" once the key is gone and the test must be able to say what it expected.
 */
const DOCUMENTS = [
  { id: 'absent', $state: 'absent' },
  { id: 'null', $state: 'null', f: null },
  { id: 'zero', $state: 'value', f: 0 },
  { id: 'one', $state: 'value', f: 1 },
  { id: 'two', $state: 'value', f: 2 },
  { id: 'three', $state: 'value', f: 3 },
  { id: 'four', $state: 'value', f: 4 },
  { id: 'neg', $state: 'value', f: -1 },
  { id: 'false', $state: 'value', f: false },
  { id: 'true', $state: 'value', f: true },
  { id: 'empty', $state: 'value', f: '' },
  { id: 'str-a', $state: 'value', f: 'a' },
  { id: 'str-alice', $state: 'value', f: 'alice' },
  { id: 'str-ALICE', $state: 'value', f: 'ALICE' },
  { id: 'str-bob', $state: 'value', f: 'bob' },
  { id: 'str-num', $state: 'value', f: '7' },
  // Array-valued, for $elemMatch / $size interactions.
  { id: 'arr-empty', $state: 'value', f: [] },
  { id: 'arr-one', $state: 'value', f: [1] },
  { id: 'arr-two', $state: 'value', f: [1, 2] },
  { id: 'arr-three', $state: 'value', f: [1, 2, 3] },
  { id: 'arr-strs', $state: 'value', f: ['a', 'b'] },
  { id: 'arr-mixed', $state: 'value', f: ['a', 2] },
  { id: 'arr-null', $state: 'value', f: [null] },
];

/**
 * The selector list: compound selectors on the single field `f`.
 *
 * Grouped by the interaction under test, so a failure names the semantic rather than a
 * generated index.
 */
const SELECTOR_GROUPS = [
  {
    group: 'interval',
    why: '$gt + $lte on one field is a half-open interval; both ends matter',
    selectors: [
      { f: { $gt: 1, $lte: 3 } },
      { f: { $gte: 1, $lt: 3 } },
      { f: { $gt: 0, $lt: 2 } },
      { f: { $gte: 2, $lte: 2 } },
      { f: { $gt: 2, $lt: 2 } },
      { f: { $gte: 0, $lte: 4 } },
      // An inverted interval matches nothing, and must not fall back to either bound.
      { f: { $gt: 3, $lt: 1 } },
      // Three bounds at once: the tightest wins on each side.
      { f: { $gt: 0, $gte: 2, $lte: 4 } },
    ],
  },
  {
    group: 'in-with-range',
    why: '$in intersected with a range operator on the same field',
    selectors: [
      { f: { $in: [1, 2, 3], $gt: 1 } },
      { f: { $in: [1, 2, 3], $lte: 2 } },
      { f: { $in: [0, 1], $gt: 0 } },
      { f: { $in: ['a', 'bob'], $gt: 'a' } },
      { f: { $in: [null, 1], $gt: 0 } },
      { f: { $in: [1, 2], $nin: [2] } },
    ],
  },
  {
    group: 'elemMatch-with-size',
    why: '$elemMatch constrains an element while $size constrains the array',
    selectors: [
      { f: { $elemMatch: { $gt: 0 }, $size: 2 } },
      { f: { $elemMatch: { $gt: 1 }, $size: 2 } },
      { f: { $elemMatch: { $gt: 2 }, $size: 2 } },
      { f: { $elemMatch: { $eq: 1 }, $size: 1 } },
      { f: { $elemMatch: { $eq: 1 }, $size: 3 } },
      { f: { $elemMatch: { $eq: 2 }, $size: 0 } },
      // $size alone, to show the difference.
      { f: { $size: 2 } },
      { f: { $elemMatch: { $gt: 0 } } },
    ],
  },
  {
    group: 'not-compound',
    why: '$not wraps a conjunction, so it negates the whole thing',
    selectors: [
      { f: { $not: { $gt: 1, $lt: 3 } } },
      { f: { $not: { $gte: 1, $lte: 2 } } },
      { f: { $not: { $in: [1, 2] } } },
      { f: { $not: { $exists: true } } },
      { f: { $not: { $eq: null } } },
      { f: { $not: { $type: 'null' } } },
      { f: { $not: { $size: 2 } } },
      { f: { $not: { $elemMatch: { $gt: 1 } } } },
      { f: { $not: { $not: { $eq: 1 } } } },
    ],
  },
  {
    group: 'three-way-state',
    why: 'absent vs null vs a real value, per operator',
    selectors: [
      { f: { $exists: true } },
      { f: { $exists: false } },
      { f: { $exists: true, $eq: null } },
      { f: { $exists: false, $eq: null } },
      { f: { $eq: null } },
      { f: { $ne: null } },
      { f: { $type: 'null' } },
      { f: { $type: 'undefined' } },
      { f: { $type: 'null', $exists: true } },
      { f: { $gte: null } },
      { f: { $lte: null } },
      { f: { $gt: null } },
      { f: { $lt: null } },
      { f: { $in: [null] } },
      { f: { $nin: [null] } },
      { f: { $in: [null], $exists: true } },
      { f: { $ne: null, $exists: true } },
      { f: { $not: { $eq: null } } },
      { f: { $not: { $exists: true } } },
      { f: { $exists: true, $gte: 0 } },
      // A missing-field equality against a concrete value: must not match the absent doc.
      { f: { $eq: 1 } },
      { f: { $ne: 1 } },
    ],
  },
  {
    group: 'regex-with-range',
    why: '$regex with $options alongside a range on the same field',
    selectors: [
      { f: { $regex: '^a', $options: 'i', $gt: 'a' } },
      { f: { $regex: '^a', $options: 'i', $lt: 'b' } },
      { f: { $regex: 'a', $options: '', $gt: 'a' } },
      { f: { $regex: '[0-9]', $options: '', $gte: '0' } },
      { f: { $regex: '^$', $options: '', $lte: 'a' } },
      { f: { $regex: '^a', $options: 'i', $in: ['alice', 'ALICE', 'bob'] } },
      { f: { $regex: '^a', $options: 'i', $exists: true } },
    ],
  },
  {
    group: 'cross-type-ordering',
    why: 'ordering comparisons require the same type, so mixed corpora must not coerce',
    selectors: [
      { f: { $gt: 0 } },
      { f: { $gte: 0 } },
      { f: { $lt: 1 } },
      { f: { $lte: 0 } },
      { f: { $gt: '', } },
      { f: { $lt: 'b' } },
      { f: { $eq: 0 } },
      { f: { $eq: '0' } },
      { f: { $eq: false } },
      { f: { $eq: '' } },
      { f: { $in: [0, 1] } },
      { f: { $in: [0, '0'] } },
    ],
  },
];

// `installDeterminism` must run before the bundle is imported: fixtures read
// `Math.random()`/`Date.now()` while the module evaluates.
const harness = await import(join(VECTORS_DIR, 'harness.mjs'));
harness.installDeterminism();
const { loadOracle } = await import(join(VECTORS_DIR, 'vendor.mjs'));
const oracle = await loadOracle();
const { queryHelper, rxSchema, utilsHash, fixtures } = oracle.modules;

const humanSchema = rxSchema.createRxSchema(
  JSON.parse(JSON.stringify(fixtures.human)),
  utilsHash.defaultHashSha256,
).jsonSchema;

const cases = [];
const rejections = [];
for (const spec of SELECTOR_GROUPS) {
  for (const selector of spec.selectors) {
    let normalized;
    let matcher;
    try {
      normalized = queryHelper.normalizeMangoQuery(humanSchema, { selector }).selector;
      matcher = queryHelper.getQueryMatcher(humanSchema, {
        selector: normalized,
        sort: [{ passportId: 'asc' }],
      });
    } catch (error) {
      // Upstream refuses this selector. Recorded so the port must refuse it too.
      rejections.push({
        group: spec.group,
        selector,
        kind: 'rejection',
        rejected: String((error && error.message) || error),
      });
      continue;
    }

    const matchingIds = [];
    for (const document of DOCUMENTS) {
      let matched = false;
      try {
        matched = matcher(document) === true;
      } catch {
        matched = false;
      }
      if (matched) matchingIds.push(document.id);
    }
    cases.push({
      group: spec.group,
      why: spec.why,
      selector,
      normalizedSelector: normalized,
      matchingIds,
      matchCount: matchingIds.length,
    });
  }
}

const doc = {
  $comment:
    'Operator-interaction corpus for the M1 selector matcher: compound selectors on one ' +
    'field, evaluated by upstream RxDB in Node. This is the second axis on top of ' +
    'matcher-fuzz.json: that family covers the operator vocabulary over random documents, ' +
    'this one covers operators COMPOSED on the same field and the same document, plus the ' +
    'absent-vs-null-vs-undefined three-way that JSON cannot express directly ' +
    '($state records the intended state per document). Expected answers came from upstream, ' +
    'never from the Rust matcher.',
  generator: 'crates/dxdb-query/tests/support/operator-interactions.mjs',
  rxdbVersion: oracle.provenance.rxdbVersion,
  rxdbCommit: oracle.provenance.rxdbCommit,
  node: process.version,
  format: 'documents+compound-selector->matchingIds',
  determinism: harness.determinismHeader(),
  documentCount: DOCUMENTS.length,
  caseCount: cases.length + rejections.length,
  comparisonCount: cases.length,
  rejectionCount: rejections.length,
  groups: [...new Set(SELECTOR_GROUPS.map((spec) => spec.group))],
  documents: DOCUMENTS,
  cases: [...cases, ...rejections],
};

const text = JSON.stringify(doc, null, 1) + '\n';
if (process.argv.includes('--check')) {
  let existing;
  try {
    existing = readFileSync(OUT, 'utf8');
  } catch {
    throw new Error(`${OUT} does not exist; run without --check to generate it`);
  }
  if (existing !== text) {
    throw new Error(`${OUT} is out of date; regenerate it`);
  }
  process.stderr.write(`[operator-interactions] check ok (${doc.caseCount} cases)\n`);
} else {
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, text);
  process.stderr.write(
    `[operator-interactions] wrote ${doc.caseCount} cases over ${doc.documentCount} documents ` +
      `(${doc.comparisonCount} compared, ${doc.rejectionCount} refusals) in ${doc.groups.length} groups\n`,
  );
}
