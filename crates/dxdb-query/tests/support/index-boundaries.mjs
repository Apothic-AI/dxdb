/**
 * Index-string numeric-boundary corpus, generated against upstream.
 *
 * Run:  node crates/dxdb-query/tests/support/index-boundaries.mjs [--check]
 *
 * # Why a second generator instead of extending lane C's family
 *
 * The committed `index-strings` family probes each index field with a handful of values
 * (`lower-number`, `lower-INDEX_MIN`, out-of-range high/low). That is enough to pin the
 * clamping *branches*. It is not enough to pin the *arithmetic*: the encoding is zero-padded
 * around a schema-declared min/max, so the failures cluster at exact boundaries and at
 * widths that interact with decimal places.
 *
 * This generator builds schemas specifically designed to put those boundary values in play,
 * and for each one records upstream's index string, its length, and both bound strings for a
 * systematic value list. The interesting values are:
 *
 *   - exactly `minimum` and exactly `maximum`
 *   - one representable step inside each (and one outside, which must clamp)
 *   - values far outside the range in both directions
 *   - negative ranges, and ranges crossing zero
 *   - widths that are powers of ten (`max - min` = 9, 99, 999, ...), where the digit budget
 *     gains a character and an off-by-one in the width calculation shows up immediately
 *   - `multipleOf` of 1, 0.1, 0.01, 0.001 and 0.5, and a `multipleOf` that does not divide
 *     the range evenly
 *   - fractional values whose decimal part rounds up at the last digit, where the carry must
 *     be clamped to `multiplier - 1` rather than widening the field
 *
 * Output lives beside this file rather than in `testdata/vectors/`, which is lane C's path.
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const VECTORS_DIR = join(here, '..', '..', '..', '..', 'xtask', 'vectors');
const OUT = join(here, 'index-boundaries.json');

/**
 * The schemas under test.
 *
 * Each is a minimal single-field schema plus the `_deleted`/`_meta.lwt` prefix and the
 * primary-key suffix that `fillWithDefaultSettings` adds, so the index being encoded has
 * realistic company around the field under test.
 */
const SCHEMAS = [
  // Zero-based, integer, span 99 -> "99" is two characters.
  { name: 'int-0-99', minimum: 0, maximum: 99, multipleOf: 1 },
  // A width that is a power of ten: span 100 needs three characters.
  { name: 'int-0-100', minimum: 0, maximum: 100, multipleOf: 1 },
  { name: 'int-0-999', minimum: 0, maximum: 999, multipleOf: 1 },
  { name: 'int-0-1000', minimum: 0, maximum: 1000, multipleOf: 1 },
  // Negative range, entirely below zero.
  { name: 'int-negative', minimum: -100, maximum: -1, multipleOf: 1 },
  // Negative range crossing zero.
  { name: 'int-crossing', minimum: -50, maximum: 50, multipleOf: 1 },
  // A non-zero minimum, so the offset is not the identity.
  { name: 'int-offset', minimum: 1000, maximum: 1099, multipleOf: 1 },
  // Fractional ranges, at each decimal budget.
  { name: 'dec-1', minimum: 0, maximum: 10, multipleOf: 0.1 },
  { name: 'dec-2', minimum: 0, maximum: 10, multipleOf: 0.01 },
  { name: 'dec-3', minimum: 0, maximum: 10, multipleOf: 0.001 },
  // A fractional step that is not a power of ten.
  { name: 'dec-half', minimum: 0, maximum: 10, multipleOf: 0.5 },
  // Negative fractional, where floor() and the offset interact.
  { name: 'dec-negative', minimum: -10, maximum: 10, multipleOf: 0.01 },
  // A large maximum, where the span's digit count is dominated by the magnitude.
  { name: 'big', minimum: 0, maximum: 1000000000000000, multipleOf: 0.01 },
  // A maximum that is not an integer, so `ceil` matters.
  { name: 'ceil-max', minimum: 0.5, maximum: 10.5, multipleOf: 0.1 },
  // `minimum` and `maximum` both negative and fractional.
  { name: 'neg-frac', minimum: -2.5, maximum: -0.5, multipleOf: 0.1 },
];

/** Build the schema literal for one numeric field spec. */
function schemaLiteral(spec) {
  return {
    title: `boundary ${spec.name}`,
    version: 0,
    primaryKey: 'id',
    type: 'object',
    properties: {
      id: { type: 'string', maxLength: 10 },
      value: {
        type: 'number',
        minimum: spec.minimum,
        maximum: spec.maximum,
        multipleOf: spec.multipleOf,
      },
    },
    required: ['id', 'value'],
    indexes: [['value']],
  };
}

/**
 * The values to encode for one schema.
 *
 * Deliberately systematic rather than random: the point is to hit every boundary of THIS
 * schema, so the list is derived from its own min/max/multipleOf rather than drawn from a
 * global pool.
 */
function probeValues(spec) {
  const { minimum, maximum, multipleOf } = spec;
  const step = multipleOf;
  const values = [];
  const push = (label, value) => values.push({ label, value });

  push('exact-min', minimum);
  push('exact-max', maximum);
  push('inside-min', minimum + step);
  push('inside-max', maximum - step);
  push('outside-low', minimum - step);
  push('outside-high', maximum + step);
  push('far-low', minimum - 1000);
  push('far-high', maximum + 1000);

  push('midpoint', minimum + Math.floor((maximum - minimum) / 2));
  push('zero', 0);

  // The decimal carry: a value whose fractional part rounds up to the full multiplier.
  const decimals = String(multipleOf).includes('.') ? String(multipleOf).split('.')[1].length : 0;
  if (decimals > 0) {
    const multiplier = Math.pow(10, decimals);
    const carryBase = Math.floor((minimum + maximum) / 2);
    push('carry', carryBase + (multiplier - 1) / multiplier);
    push('carry-exact', carryBase + (multiplier - 0.5) / multiplier);
    push('one-lsb-inside', minimum + 1 / multiplier);
  }

  // A value off the `multipleOf` grid, which upstream encodes by rounding.
  push('off-grid', minimum + step * 1.5);

  return values;
}

// `installDeterminism` before the bundle is imported: fixtures read Math.random/Date.now
// while the module evaluates.
const harness = await import(join(VECTORS_DIR, 'harness.mjs'));
harness.installDeterminism();
const { loadOracle } = await import(join(VECTORS_DIR, 'vendor.mjs'));
const oracle = await loadOracle();
const { customIndex, queryPlanner, schemaHelper, utilsHash, rxSchema } = oracle.modules;

const cases = [];
for (const spec of SCHEMAS) {
  const literal = schemaLiteral(spec);
  const filled = schemaHelper.fillWithDefaultSettings(JSON.parse(JSON.stringify(literal)));
  // The filled form, which is what every encoder call must use.
  const schema = rxSchema.createRxSchema(
    JSON.parse(JSON.stringify(literal)),
    utilsHash.defaultHashSha256,
  ).jsonSchema;

  // The index under test is the one the fill produced that ends with our field.
  const index = filled.indexes.find((candidate) => candidate.includes('value'));
  if (!index) {
    throw new Error(`${spec.name}: fill produced no index containing 'value'`);
  }

  const length = customIndex.getIndexStringLength(schema, index);
  const parsed = customIndex.getStringLengthOfIndexNumber(schema.properties.value);

  cases.push({
    schema: spec.name,
    spec,
    index,
    length,
    digitBudget: {
      nonDecimals: parsed.nonDecimals,
      decimals: parsed.decimals,
      minimum: parsed.minimum,
      maximum: parsed.maximum,
    },
    values: probeValues(spec).map((probe) => {
      // Encode one document holding this value.
      const document = {
        id: 'x',
        value: probe.value,
        _deleted: false,
        _rev: '1-x',
        _meta: { lwt: 1 },
        _attachments: {},
      };
      let indexString;
      try {
        indexString = customIndex.getIndexableStringMonad(schema, index)(document);
      } catch (error) {
        indexString = { threw: String((error && error.message) || error) };
      }
      // And the bound strings, with this value in this field's position.
      const position = index.indexOf('value');
      const neutral = index.map((field, at) => {
        if (at === position) return probe.value;
        const part = schema.properties[field];
        if (part && (part.type === 'number' || part.type === 'integer')) {
          return typeof part.minimum === 'number' ? part.minimum : 0;
        }
        if (part && part.type === 'boolean') return false;
        return '';
      });
      let lowerBoundString;
      let upperBoundString;
      try {
        lowerBoundString = customIndex.getStartIndexStringFromLowerBound(
          schema,
          index,
          neutral,
        );
      } catch (error) {
        lowerBoundString = { threw: String((error && error.message) || error) };
      }
      try {
        upperBoundString = customIndex.getStartIndexStringFromUpperBound(
          schema,
          index,
          neutral,
        );
      } catch (error) {
        upperBoundString = { threw: String((error && error.message) || error) };
      }
      return { ...probe, indexString, lowerBoundString, upperBoundString };
    }),
  });
}

const doc = {
  $comment:
    'Numeric-boundary corpus for the index-string encoder. Each schema is chosen to put a ' +
    'boundary in play (power-of-ten widths, negative ranges, ranges crossing zero, each ' +
    'decimal budget) and each probe value is derived from that schema own min/max/multipleOf: ' +
    'exact bounds, one step inside, one step outside (which must clamp), far outside, and ' +
    'the decimal-carry case. Expected strings come from upstream custom-index.ts in Node.',
  generator: 'crates/dxdb-query/tests/support/index-boundaries.mjs',
  rxdbVersion: oracle.provenance.rxdbVersion,
  rxdbCommit: oracle.provenance.rxdbCommit,
  node: process.version,
  format: 'schema+value->{indexString, lowerBoundString, upperBoundString}',
  determinism: harness.determinismHeader(),
  schemaCount: cases.length,
  valueCount: cases.reduce((sum, entry) => sum + entry.values.length, 0),
  cases,
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
  process.stderr.write(`[index-boundaries] check ok (${doc.valueCount} values)\n`);
} else {
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, text);
  process.stderr.write(
    `[index-boundaries] wrote ${doc.schemaCount} schemas, ${doc.valueCount} values\n`,
  );
}
