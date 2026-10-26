/**
 * `$options` / `$regex` flag corpus, from upstream.
 *
 * Run:  node crates/dxdb-query/tests/support/regex-flags.mjs [--check]
 *
 * # Why this is a corpus and not a unit test
 *
 * JavaScript regex flags are a `Flags` struct in the port, not inline `(?i)`, and `$options` is
 * where a caller supplies them. The question "does `$options: 'i'` behave the same as upstream"
 * cannot be answered by our reading of the source — it needs upstream's answers. So the
 * expectations are generated here by running mingo (which calls `new RegExp(pattern, options)`)
 * over a document corpus, and the Rust test asserts agreement.
 *
 * # The two failures this targets
 *
 * 1. **A silently ignored flag.** If `i` were dropped, `{name: {$regex: '^alice$', $options:
 *    'i'}}` would stop matching `ALICE` — wrong documents, and it looks like missing data
 *    rather than a bug.
 * 2. **A flag string upstream rejects and we accept.** That is the worse direction: a caller
 *    learns their query works here and it fails in a browser. Every invalid string in the
 *    corpus must be an *error* in the port, never a quiet no-match.
 *
 * # The flag set, measured
 *
 *   ""  i  m  s  u  v  g  y  d          accepted
 *   gimsuy, dv, du, ims                 accepted in combination
 *   ii  uu  gg  dd  mm  ss  yy          THROW (repeated flag)
 *   uv  vu                              THROW (u and v are mutually exclusive)
 *   x  X  I  z                          THROW (no `x` flag exists in ECMAScript)
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const VECTORS_DIR = join(here, '..', '..', '..', '..', 'xtask', 'vectors');
const OUT = join(here, 'regex-flags.json');

/**
 * Documents chosen so each flag has an observable effect.
 *
 * Every document has the same key `f`, so a pattern's behaviour is the only variable.
 */
const DOCUMENTS = [
  { id: 'plain', f: 'alice' },
  { id: 'upper', f: 'ALICE' },
  { id: 'mixed', f: 'Alice' },
  { id: 'multiline', f: 'a\nb' },
  { id: 'multiline-b-end', f: 'x\nb' },
  { id: 'mid-dot', f: 'a\nc' },
  { id: 'newline-only', f: '\n' },
  { id: 'unicode-word', f: 'héllo' },
  { id: 'accent-cap', f: 'HÉLLO' },
  { id: 'greek', f: 'αβγ' },
  { id: 'emoji', f: '😀' },
  { id: 'digits', f: '123' },
  { id: 'empty', f: '' },
  { id: 'spaces', f: '  a  ' },
  { id: 'brackets', f: '[a]' },
  { id: 'caret', f: '^a$' },
];

/**
 * Cases: a pattern plus an options string.
 *
 * Grouped by the flag whose behaviour is under test, so a failure names the flag.
 */
const CASES = [
  // --- baseline: no flags ---
  { group: 'none', pattern: 'a', options: '' },
  { group: 'none', pattern: '^a', options: '' },
  { group: 'none', pattern: 'a$', options: '' },
  { group: 'none', pattern: 'a.c', options: '' }, // `.` must NOT cross a newline
  { group: 'none', pattern: '^b$', options: '' }, // `^`/`$` are whole-string only

  // --- `i`: case folding ---
  { group: 'i', pattern: '^alice$', options: 'i' },
  { group: 'i', pattern: 'alice', options: 'i' },
  { group: 'i', pattern: '^ALICE$', options: 'i' },
  { group: 'i', pattern: 'héllo', options: 'i' },

  // --- `m`: `^`/`$` at line boundaries ---
  { group: 'm', pattern: '^b$', options: 'm' },
  { group: 'm', pattern: '^b', options: 'm' },
  { group: 'm', pattern: 'a$', options: 'm' },
  { group: 'm', pattern: '^a$', options: 'm' },

  // --- `s`: `.` matches a newline ---
  { group: 's', pattern: 'a.c', options: 's' },
  { group: 's', pattern: '^a.c$', options: 's' },
  { group: 's', pattern: 'a.c', options: '' }, // contrast, same pattern without the flag

  // --- `u`: unicode property escapes and surrogate handling ---
  { group: 'u', pattern: '\\p{Letter}+', options: 'u' },
  { group: 'u', pattern: '^\\p{Letter}+$', options: 'u' },
  { group: 'u', pattern: '.', options: 'u' },
  // Without `u`, `\p` is not a property escape; the pattern still compiles in Annex B mode and
  // matches a literal `p`, which is a good discriminator.
  { group: 'u', pattern: '\\p{Letter}+', options: '' },

  // --- `d`: accepted, no observable effect on matching ---
  { group: 'd', pattern: 'a', options: 'd' },
  { group: 'd', pattern: '^alice$', options: 'd' },
  { group: 'd', pattern: 'a', options: 'di' },
  { group: 'd', pattern: '^ALICE$', options: 'di' },

  // --- `g` and `y`: accepted, and must not change a single `test` result ---
  { group: 'g-y', pattern: 'a', options: 'g' },
  { group: 'g-y', pattern: 'a', options: 'y' },
  { group: 'g-y', pattern: '^a', options: 'y' },
  { group: 'g-y', pattern: 'a', options: 'gy' },

  // --- combinations, which must behave as the union of their parts ---
  { group: 'combined', pattern: '^alice$', options: 'im' },
  { group: 'combined', pattern: '^b$', options: 'im' },
  { group: 'combined', pattern: 'a.c', options: 'is' },
  { group: 'combined', pattern: '^A.C$', options: 'is' },
  { group: 'combined', pattern: '^b$', options: 'ms' },
  { group: 'combined', pattern: '^b$', options: 'ims' },
  { group: 'combined', pattern: 'a', options: 'gimsuy' },
  { group: 'combined', pattern: 'a', options: 'du' },
  { group: 'combined', pattern: 'a', options: 'dv' },
  { group: 'combined', pattern: 'héllo', options: 'iu' },

  // --- invalid strings, every one of which must be an ERROR ---
  { group: 'invalid', pattern: 'a', options: 'x' },
  { group: 'invalid', pattern: 'a', options: 'X' },
  { group: 'invalid', pattern: 'a', options: 'I' },
  { group: 'invalid', pattern: 'a', options: 'z' },
  { group: 'invalid', pattern: 'a', options: 'ii' },
  { group: 'invalid', pattern: 'a', options: 'uu' },
  { group: 'invalid', pattern: 'a', options: 'vv' },
  { group: 'invalid', pattern: 'a', options: 'gg' },
  { group: 'invalid', pattern: 'a', options: 'dd' },
  { group: 'invalid', pattern: 'a', options: 'mm' },
  { group: 'invalid', pattern: 'a', options: 'ss' },
  { group: 'invalid', pattern: 'a', options: 'yy' },
  { group: 'invalid', pattern: 'a', options: 'uv' },
  { group: 'invalid', pattern: 'a', options: 'vu' },
  { group: 'invalid', pattern: 'a', options: 'imsuygdv' },
];

// `installDeterminism` before the bundle is imported: fixtures read Math.random/Date.now while
// the module evaluates.
const harness = await import(join(VECTORS_DIR, 'harness.mjs'));
harness.installDeterminism();
const { loadOracle } = await import(join(VECTORS_DIR, 'vendor.mjs'));
const oracle = await loadOracle();
// The mingo wiring lives in the vendored bundle's own module surface. `queryHelper` exposes
// `getQueryMatcher`, which is the same path a real query takes — preferred over reaching for
// mingo directly, because it is what RxDB actually does.
const { queryHelper, rxSchema, utilsHash, fixtures } = oracle.modules;
const humanSchema = rxSchema.createRxSchema(
  JSON.parse(JSON.stringify(fixtures.human)),
  utilsHash.defaultHashSha256,
).jsonSchema;

const cases = [];
const rejections = [];
for (const spec of CASES) {
  const selector = { f: { $regex: spec.pattern, $options: spec.options } };
  let matcher;
  try {
    matcher = queryHelper.getQueryMatcher(humanSchema, {
      selector,
      sort: [{ passportId: 'asc' }],
    });
  } catch (error) {
    // An invalid `$options` throws at query-construction time, which is the behaviour the port
    // must reproduce.
    rejections.push({
      group: spec.group,
      why: `options ${JSON.stringify(spec.options)}`,
      pattern: spec.pattern,
      options: spec.options,
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
    why: `regex ${JSON.stringify(spec.pattern)} with options ${JSON.stringify(spec.options)}`,
    pattern: spec.pattern,
    options: spec.options,
    matchingIds,
    matchCount: matchingIds.length,
  });
}

const doc = {
  $comment:
    'ECMAScript regex flag corpus for $regex/$options. Each case is a pattern plus an options ' +
    'string evaluated against a document corpus by upstream mingo (which calls new RegExp), ' +
    'so the expected matches come from JavaScript itself. Cases whose options string is ' +
    'invalid upstream are recorded as refusals, because the port must raise an error rather ' +
    'than silently evaluate to no matches -- a silently dropped `i` returns the wrong ' +
    'documents and looks like missing data, and accepting a flag upstream rejects teaches a ' +
    'caller a query that fails in a browser. There is no `x` flag in ECMAScript.',
  generator: 'crates/dxdb-query/tests/support/regex-flags.mjs',
  rxdbVersion: oracle.provenance.rxdbVersion,
  rxdbCommit: oracle.provenance.rxdbCommit,
  node: process.version,
  format: 'documents+(pattern,options)->matchingIds',
  determinism: harness.determinismHeader(),
  documentCount: DOCUMENTS.length,
  caseCount: cases.length + rejections.length,
  comparisonCount: cases.length,
  rejectionCount: rejections.length,
  groups: [...new Set(CASES.map((spec) => spec.group))],
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
  process.stderr.write(`[regex-flags] check ok (${doc.caseCount} cases)\n`);
} else {
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, text);
  process.stderr.write(
    `[regex-flags] wrote ${doc.caseCount} cases over ${doc.documentCount} documents ` +
      `(${doc.comparisonCount} compared, ${doc.rejectionCount} refusals) in ${doc.groups.length} groups\n`,
  );
}
