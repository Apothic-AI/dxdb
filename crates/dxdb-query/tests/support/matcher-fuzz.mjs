/**
 * Differential fuzz corpus for the M1 selector matcher (PLAN.md 7.3).
 *
 * Run:  node crates/dxdb-query/tests/support/matcher-fuzz.mjs [--check]
 *
 * # Why this lives in dxdb-query rather than xtask/vectors
 *
 * `xtask/vectors/` and `testdata/vectors/` are lane C's paths (docs/WORKTREES.md). This
 * generator *imports* lane C's vendoring machinery — `xtask/vectors/vendor.mjs`, which
 * clones the pinned upstream commit and bundles its TypeScript with the pinned esbuild —
 * so the oracle is the same object every committed family uses. What it does not do is
 * write into lane C's directory. Its output is committed under
 * `crates/dxdb-query/tests/support/` instead, next to the test that consumes it.
 *
 * # What makes this a differential and not a self-fulfilling test
 *
 * Every `matchingIds` value below comes from `getQueryMatcher(...)(document)` running in
 * upstream's own code. Nothing here consults the Rust matcher. A corpus generated from our
 * own implementation would launder a bug into a test and prove nothing.
 *
 * # Why the corpus is committed rather than generated at test time
 *
 * Generating documents and selectors is easy in Rust; obtaining upstream's answer for them
 * needs Node plus the pinned checkout. So the pair list is produced here, once, and
 * committed. The seed is fixed so it is reproducible, and the case count is asserted in
 * Rust so a truncated file fails instead of passing against a smaller corpus.
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const VECTORS_DIR = join(here, '..', '..', '..', '..', 'xtask', 'vectors');
const OUT = join(here, 'matcher-fuzz.json');

/** Corpus sizes. Big enough to find things; small enough that `make test` stays quick. */
const DOCUMENT_COUNT = 160;
const SELECTOR_COUNT = 240;

/**
 * xorshift128, the same generator lane C's harness uses, so the corpus is reproducible
 * from the same seed on any machine.
 */
function createPrng(seed) {
  let a = seed >>> 0;
  let b = 0x1b873593;
  let c = 0x85ebca6b;
  let d = 0xc2b2ae35;
  return function next() {
    const t = a ^ (a << 11);
    a = b;
    b = c;
    c = d;
    d = (d ^ (d >>> 19) ^ (t ^ (t >>> 8))) >>> 0;
    return d / 0x100000000;
  };
}

/**
 * One generated document.
 *
 * Fields are sometimes *absent* rather than null, and `$present` records which, because
 * "field is missing" versus "field is null" is the classic Mango divergence and JSON alone
 * cannot express the difference once a key is gone.
 */
function makeDocument(rand, index) {
  const pick = (values) => values[Math.floor(rand() * values.length)];
  const maybe = (value) => (rand() < 0.22 ? undefined : value);
  const doc = { id: `doc-${String(index).padStart(3, '0')}` };

  const num = maybe(pick([0, 1, 2, 5, 10, 20, 21, 40, 100, -5, 3.5, 0.5, 1e6, null]));
  if (num !== undefined) doc.num = num;

  const text = maybe(pick(['', 'a', 'alice', 'bob', 'Z', 'zzz', '\uFFFF', '\u{1F600}']));
  if (text !== undefined) doc.text = text;

  const flag = maybe(pick([true, false]));
  if (flag !== undefined) doc.flag = flag;

  const tags = maybe(
    pick([[], ['a'], ['a', 'b'], ['b', 'a'], [1, 2], ['a', 1], [[1], 2], [{ x: 1 }], null]),
  );
  if (tags !== undefined) doc.tags = tags;

  const nested = maybe(
    pick([{ value: 1 }, { value: 10 }, { value: 'a' }, { value: null }, {}, null, [{ value: 1 }]]),
  );
  if (nested !== undefined) doc.nested = nested;

  const mixed = maybe(
    pick([0, 1, -1, 3.5, '', 'a', true, false, null, 'alice', '\uFFFF', 1e6]),
  );
  if (mixed !== undefined) doc.mixed = mixed;

  doc._deleted = rand() < 0.15;
  doc._rev = `1-${index}`;
  doc.$present = ['num', 'text', 'flag', 'tags', 'nested', 'mixed'].filter((field) =>
    Object.hasOwn(doc, field),
  );
  return doc;
}

/** Field paths a selector may address, including dotted and array-index forms. */
const PATHS = [
  'num', 'text', 'flag', 'tags', 'nested', 'mixed', '_deleted',
  'nested.value', 'tags.0', 'tags.1', 'nope', 'nested.missing',
];

/** Operand values, weighted toward the boundaries where semantics change. */
const OPERANDS = [
  0, 1, 2, 20, 21, 40, 100, -1, -5, 3.5, 0.5, 1e6,
  '', 'a', 'alice', 'bob', 'Z', '\uFFFF', '\u{1F600}',
  true, false, null,
  [], ['a'], ['a', 'b'], [1, 2], { value: 1 }, { x: 1 },
];

const SCALAR_OPERATORS = ['$eq', '$ne', '$gt', '$gte', '$lt', '$lte'];
const SET_OPERATORS = ['$in', '$nin'];

function makeSelector(rand) {
  const pick = (values) => values[Math.floor(rand() * values.length)];
  const operand = () => pick(OPERANDS);
  const fieldSelector = () => {
    switch (Math.floor(rand() * 10)) {
      case 0:
      case 1:
      case 2:
        return { [pick(SCALAR_OPERATORS)]: operand() };
      case 3:
        return { [pick(SET_OPERATORS)]: [operand(), operand()] };
      case 4:
        return { $exists: rand() < 0.5 };
      case 5:
        return {
          $type: pick(['null', 'boolean', 'number', 'string', 'array', 'object', 'undefined']),
        };
      case 6:
        return { $mod: [pick([2, 3, 10]), pick([0, 1, 2])] };
      case 7:
        return { $size: pick([0, 1, 2]) };
      case 8:
        return { $elemMatch: { [pick(SCALAR_OPERATORS)]: operand() } };
      default:
        return { $regex: pick(['^a', 'b', '^$', '[0-9]', 'a.c']), $options: pick(['', 'i']) };
    }
  };

  const clauseCount = 1 + Math.floor(rand() * 3);
  const clauses = {};
  for (let index = 0; index < clauseCount; index++) {
    clauses[pick(PATHS)] = fieldSelector();
  }

  switch (Math.floor(rand() * 6)) {
    case 0:
      return { $and: [clauses] };
    case 1:
      return { $or: [clauses, { [pick(PATHS)]: fieldSelector() }] };
    case 2:
      return { $nor: [clauses] };
    case 3: {
      const path = pick(PATHS);
      return { [path]: { $not: fieldSelector() } };
    }
    default:
      return clauses;
  }
}

// `installDeterminism` must run before the oracle bundle is imported: fixture construction
// draws on `Math.random()` and `Date.now()` while the module is being evaluated, so
// importing first would bake in real randomness and make the corpus unreproducible.
const harness = await import(join(VECTORS_DIR, 'harness.mjs'));
harness.installDeterminism();
const { loadOracle } = await import(join(VECTORS_DIR, 'vendor.mjs'));
const oracle = await loadOracle();
const { queryHelper, rxSchema, utilsHash, fixtures } = oracle.modules;

const humanSchema = rxSchema.createRxSchema(
  JSON.parse(JSON.stringify(fixtures.human)),
  utilsHash.defaultHashSha256,
).jsonSchema;

const rand = createPrng(0x5eed1e55 ^ 0xf0f0f0);
const documents = [];
for (let index = 0; index < DOCUMENT_COUNT; index++) {
  documents.push(makeDocument(rand, index));
}

const cases = [];
const rejections = [];
for (let index = 0; index < SELECTOR_COUNT; index++) {
  const selector = makeSelector(rand);
  let normalized;
  let matcher;
  try {
    // Normalise exactly as RxDB does, so the matcher sees what a real query would.
    normalized = queryHelper.normalizeMangoQuery(humanSchema, { selector }).selector;
    matcher = queryHelper.getQueryMatcher(humanSchema, {
      selector: normalized,
      sort: [{ passportId: 'asc' }],
    });
  } catch (error) {
    // A generated selector can be one upstream refuses. Record it rather than dropping it:
    // the port must refuse it too, and a dropped case is a silently-uncovered input.
    rejections.push({
      why: `generated-${index}`,
      selector,
      kind: 'rejection',
      rejected: String((error && error.message) || error),
    });
    continue;
  }

  const matchingIds = [];
  for (const document of documents) {
    // A document whose shape the matcher cannot handle upstream is a `false`, not a crash:
    // the query path never throws per document, so neither should the expectation.
    let matched = false;
    try {
      matched = matcher(document) === true;
    } catch {
      matched = false;
    }
    if (matched) matchingIds.push(document.id);
  }
  cases.push({
    why: `generated-${index}`,
    selector,
    normalizedSelector: normalized,
    matchingIds,
    matchCount: matchingIds.length,
  });
}

const doc = {
  $comment:
    'Differential fuzz corpus for the M1 selector matcher (PLAN.md 7.3). Documents and ' +
    'selectors are generated from a fixed seed and every expected answer comes from ' +
    'upstream RxDB in Node, never from the Rust matcher. `$present` records which fields ' +
    'are actually present on each document, because absent-versus-null is the classic ' +
    'Mango divergence and JSON alone cannot express it once a key is gone.',
  generator: 'crates/dxdb-query/tests/support/matcher-fuzz.mjs',
  rxdbVersion: oracle.provenance.rxdbVersion,
  rxdbCommit: oracle.provenance.rxdbCommit,
  node: process.version,
  format: 'documents+selector->matchingIds',
  determinism: harness.determinismHeader(),
  seed: '0x5eed1e55 ^ 0xf0f0f0',
  documentCount: documents.length,
  selectorCount: cases.length,
  rejectionCount: rejections.length,
  caseCount: cases.length + rejections.length,
  documents,
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
    throw new Error(`${OUT} is out of date; regenerate it with node ${process.argv[1]}`);
  }
  process.stderr.write(`[matcher-fuzz] check ok (${doc.caseCount} cases)\n`);
} else {
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, text);
  process.stderr.write(
    `[matcher-fuzz] wrote ${doc.caseCount} cases: ${doc.documentCount} documents, ` +
      `${doc.selectorCount} selectors, ${doc.rejectionCount} refusals\n`,
  );
}
