/**
 * Sort tie-breaking and stability corpus, from upstream.
 *
 * Run:  node crates/dxdb-query/tests/support/sort-ties.mjs [--check]
 *
 * # What breaks without this
 *
 * Upstream sorts by index string and appends the primary key to every normalized sort, which
 * makes the order *total*: two documents with identical sort-field values still have a
 * deterministic relative order. RxDB's event-reduce and change-event comparison both depend
 * on that, so a tie broken differently is a divergence even though the *set* of results is
 * the same.
 *
 * Three tie shapes are easy to get wrong in a Rust port:
 *
 *   1. **Equal values across a re-sort.** A `desc` anywhere forces an in-memory re-sort
 *      (`query-planner.ts:48`). The tie-break must survive it — and note that upstream
 *      appends the primary key as **`asc`** regardless of the other parts' directions, so the
 *      secondary order is never inverted.
 *   2. **`1` versus `1.0`.** In JavaScript these are the same number, so any two documents
 *      holding them are tied. In Rust `serde_json::Value` equality distinguishes
 *      `Number(1u64)` from `Number(1.0f64)`, so a comparator that decides ties with `==` on
 *      `Value` treats them as different and falls through to a comparison that may order them
 *      by representation. This corpus pins it.
 *   3. **`_meta.lwt` sharing a millisecond with differing fractions.** `lwt` is
 *      `multipleOf: 0.01`, so `1000`, `1000.25` and `1000.0` are three distinct values that a
 *      storage must order — and `1000` and `1000.0` are the same value.
 *
 * Each case records the **full ordering**, not just the set, because that is the property
 * under test.
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const VECTORS_DIR = join(here, '..', '..', '..', '..', 'xtask', 'vectors');
const OUT = join(here, 'sort-ties.json');

/**
 * Document corpora.
 *
 * Each is a small, hand-built set where the *only* differences are the ones under test, so a
 * divergence localises to the tie shape rather than to some unrelated property. The JSON
 * source text is used for the `1` vs `1.0` corpus, because `JSON.parse` would normalise them
 * to the same number and the distinction would be lost before the comparator ever saw it.
 */
const CORPORA = {
  /** Identical string sort values, distinct primary keys. */
  stringTies: [
    { id: 'c', s: 'same' },
    { id: 'a', s: 'same' },
    { id: 'b', s: 'same' },
    { id: 'e', s: 'same' },
    { id: 'd', s: 'zzz' },
    { id: 'f', s: 'aaa' },
  ],
  /** Identical numeric sort values, including some written with a trailing `.0`. */
  numberTies: [
    { id: 'c', n: 5 },
    { id: 'a', n: 5 },
    { id: 'b', n: 5.0 },
    { id: 'd', n: 4 },
    { id: 'e', n: 6.0 },
  ],
  /** `multipleOf: 0.01` values sharing a millisecond, plus exact duplicates. */
  lwtFractions: [
    { id: 'p', lwt: 1000.0 },
    { id: 'q', lwt: 1000.5 },
    { id: 'r', lwt: 1000.25 },
    { id: 's', lwt: 1000 },
    { id: 't', lwt: 1000.5 },
    { id: 'u', lwt: 999.99 },
    { id: 'v', lwt: 1001 },
  ],
  /** Two sort fields with a tie on the first and a tie on the second. */
  compound: [
    { id: 'c', s: 'a', n: 1 },
    { id: 'a', s: 'a', n: 1 },
    { id: 'b', s: 'a', n: 2 },
    { id: 'd', s: 'b', n: 1 },
    { id: 'e', s: 'b', n: 1 },
  ],
  /** Booleans, where there are only two possible values and ties are guaranteed. */
  booleans: [
    { id: 'c', b: true },
    { id: 'a', b: false },
    { id: 'b', b: true },
    { id: 'd', b: false },
  ],
  /** A field absent on some documents: mingo sorts an absent path first. */
  missingField: [
    { id: 'c', m: 2 },
    { id: 'a' },
    { id: 'b', m: 1 },
    { id: 'd' },
  ],
  /** Unicode strings that differ only past the BMP, exercising code-unit ordering. */
  unicodeTies: [
    { id: 'b', s: '\u{1F600}' },
    { id: 'a', s: '\u{1F600}' },
    { id: 'c', s: '\uFFFF' },
    { id: 'd', s: '\u{1F600}' },
    { id: 'e', s: 'Z' },
  ],
};

/** The sorts to run against each corpus. Every one must end with the primary key. */
const SORTS = {
  stringTies: [[{ s: 'asc' }], [{ s: 'desc' }], [{ s: 'asc' }, { id: 'desc' }]],
  numberTies: [[{ n: 'asc' }], [{ n: 'desc' }]],
  lwtFractions: [[{ lwt: 'asc' }], [{ lwt: 'desc' }]],
  compound: [
    [{ s: 'asc' }, { n: 'asc' }],
    [{ s: 'desc' }, { n: 'asc' }],
    [{ s: 'asc' }, { n: 'desc' }],
    [{ s: 'desc' }, { n: 'desc' }],
  ],
  booleans: [[{ b: 'asc' }], [{ b: 'desc' }]],
  missingField: [[{ m: 'asc' }], [{ m: 'desc' }]],
  unicodeTies: [[{ s: 'asc' }], [{ s: 'desc' }]],
};

// `installDeterminism` before the bundle is imported: fixtures read Math.random/Date.now
// while the module evaluates.
const harness = await import(join(VECTORS_DIR, 'harness.mjs'));
harness.installDeterminism();
const { loadOracle } = await import(join(VECTORS_DIR, 'vendor.mjs'));
const oracle = await loadOracle();
const { queryHelper, schemaHelper, rxSchema, utilsHash } = oracle.modules;

const schemaLiteral = {
  version: 0,
  primaryKey: 'id',
  type: 'object',
  properties: {
    id: { type: 'string', maxLength: 10 },
    s: { type: 'string', maxLength: 20 },
    n: { type: 'number' },
    lwt: { type: 'number' },
    b: { type: 'boolean' },
    m: { type: 'number' },
  },
  required: ['id'],
  indexes: ['s'],
};
const schema = rxSchema.createRxSchema(
  JSON.parse(JSON.stringify(schemaLiteral)),
  utilsHash.defaultHashSha256,
).jsonSchema;

const cases = [];
for (const [corpusName, documents] of Object.entries(CORPORA)) {
  for (const sort of SORTS[corpusName]) {
    const normalized = queryHelper.normalizeMangoQuery(schema, {
      selector: {},
      sort,
    });
    const comparator = queryHelper.getSortComparator(schema, normalized);

    // Sort a copy, and record the full ordering as ids.
    const sortedIds = documents
      .slice()
      .sort(comparator)
      .map((document) => document.id);

    // Also record the pairwise comparison matrix, so a port can assert the *relation*
    // between two documents rather than only the total order. This is what catches "right
    // answer, wrong reason" — a comparator that is accidentally correct for this corpus but
    // orders the 1-vs-1.0 pair the wrong way shows up here.
    const comparisons = [];
    for (let i = 0; i < documents.length; i++) {
      for (let j = 0; j < documents.length; j++) {
        if (i === j) continue;
        const relation = comparator(documents[i], documents[j]);
        comparisons.push({
          a: documents[i].id,
          b: documents[j].id,
          // Normalise `undefined`/0 to 0, negative to -1, positive to 1.
          relation: relation === undefined ? 0 : Math.sign(relation),
        });
      }
    }

    cases.push({
      corpus: corpusName,
      why: `${corpusName}: ${JSON.stringify(sort)}`,
      sort,
      normalizedSort: normalized.sort,
      sortedIds,
      comparisons,
    });
  }
}

// A separate, minimal record of the `1` vs `1.0` question: the raw JSON source is needed so
// the two are not canonicalised before the comparator sees them.
const numberIdentity = {
  $comment:
    'Whether `{"n": 1}` and `{"n": 1.0}` are tied. In JavaScript they hold the same number, ' +
    'so a comparator must treat them as equal and fall through to the primary key. Recorded ' +
    'as raw JSON text because JSON.parse would otherwise make the two indistinguishable ' +
    'before the comparison happens, which is exactly the bug being tested for.',
  pair: [{ id: 'int', json: '{"id":"int","n":1}' }, { id: 'float', json: '{"id":"float","n":1.0}' }],
  tied: null,
  sortedIds: null,
};
{
  const intDoc = JSON.parse('{"id":"int","n":1}');
  const floatDoc = JSON.parse('{"id":"float","n":1.0}');

  // Measure the `n` comparison IN ISOLATION. A normalized sort appends the primary key, so
  // comparing the two under it would answer the tie-break question rather than the "are these
  // the same number" question. `getSortComparator` takes whatever sort it is given, so hand
  // it the single part.
  const nOnly = queryHelper.getSortComparator(schema, { selector: {}, sort: [{ n: 'asc' }] });
  const forward = nOnly(intDoc, floatDoc);
  numberIdentity.tied = forward === undefined || forward === 0;
  numberIdentity.forwardRelation = forward === undefined ? 'undefined' : Math.sign(forward);
  numberIdentity.reverseRelation = (() => {
    const back = nOnly(floatDoc, intDoc);
    return back === undefined ? 'undefined' : Math.sign(back);
  })();

  // And the full normalized order, where the primary key decides because `n` ties.
  const normalized = queryHelper.normalizeMangoQuery(schema, {
    selector: {},
    sort: [{ n: 'asc' }],
  });
  const comparator = queryHelper.getSortComparator(schema, normalized);
  numberIdentity.sortedIds = [intDoc, floatDoc].slice().sort(comparator).map((d) => d.id);
  // Reversed input, to show the order comes from the primary key and not insertion order.
  numberIdentity.sortedIdsReversed = [floatDoc, intDoc]
    .slice()
    .sort(comparator)
    .map((d) => d.id);
}

const doc = {
  $comment:
    'Sort tie-breaking and stability. Each case is a document corpus that differs only in the ' +
    'tie shape under test, a sort specification, the full normalized sort, and the complete ' +
    'expected ordering — plus a pairwise comparison matrix so a port can assert the relation ' +
    'between two documents rather than only the total order. Expected values come from ' +
    'upstream getSortComparator in Node. Ties matter because RxDB appends the primary key to ' +
    'every sort, which makes the order total, and event-reduce depends on that.',
  generator: 'crates/dxdb-query/tests/support/sort-ties.mjs',
  rxdbVersion: oracle.provenance.rxdbVersion,
  rxdbCommit: oracle.provenance.rxdbCommit,
  node: process.version,
  format: 'corpus+sort->{sortedIds, comparisons}',
  determinism: harness.determinismHeader(),
  corpora: Object.fromEntries(
    Object.entries(CORPORA).map(([name, documents]) => [name, documents]),
  ),
  caseCount: cases.length,
  comparisonCount: cases.reduce((sum, entry) => sum + entry.comparisons.length, 0),
  cases,
  numberIdentity,
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
  process.stderr.write(`[sort-ties] check ok (${doc.caseCount} cases)\n`);
} else {
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, text);
  process.stderr.write(
    `[sort-ties] wrote ${doc.caseCount} cases, ${doc.comparisonCount} pairwise comparisons\n`,
  );
}
