/**
 * Family: index strings.
 *
 * Upstream: `src/custom-index.ts` (`getIndexableStringMonad`, `getIndexStringPart`,
 * `getNumberIndexString`, `getStartIndexStringFromLowerBound`,
 * `getStartIndexStringFromUpperBound`, `getIndexStringLength`).
 *
 * This is the family where a port goes wrong silently. There are three separate
 * places a value is transformed and only the outer one is obvious:
 *
 *   1. `getIndexMeta` builds the index *string* for a document (`_deleted` and
 *      the primary key are part of the index after defaulting, so the index used
 *      here is the defaulted one, not what the fixture declared);
 *   2. `getNumberIndexString` **clamps** out-of-range values to the schema's
 *      min/max before writing digits, which is how a query with `$gt: -1000`
 *      still lands on a valid range in a storage that iterates by string;
 *   3. the bound helpers map `INDEX_MIN`/`INDEX_MAX`/`null`/`undefined` onto
 *      fill characters (`\x00`, space, `9`, `0`, `1`) rather than onto values.
 *
 * Cases are emitted for every numeric, string and boolean index field found in
 * upstream's fixture schemas, at and beyond the declared boundaries, plus the
 * bound helpers for each index, plus compound indexes so concatenation order is
 * pinned.
 */
import { join } from 'node:path';

import { loadOracle } from '../vendor.mjs';
import { determinismHeader, emitFamily } from '../harness.mjs';

const FAMILY = 'index-strings';
const MAX_CASES = 900;

/** Values every numeric field is probed with, split by intent. */
function numericProbes(minimum, maximum, multipleOf) {
  const step = typeof multipleOf === 'number' && multipleOf > 0 ? multipleOf : 1;
  const span = maximum - minimum;
  const mid = minimum + Math.floor(span / 2);
  const quarter = minimum + Math.floor(span / 4);
  const clampBelow = Math.min(minimum - 1, minimum - step * 3 - 1000);
  const clampAbove = Math.max(maximum + 1, maximum + step * 3 + 1000);
  const probes = [
    // Exactly at the schema boundaries.
    { why: 'at-minimum', value: minimum },
    { why: 'at-maximum', value: maximum },
    // Just inside, to catch off-by-one in the digit padding.
    { why: 'min-plus-step', value: minimum + step },
    { why: 'max-minus-step', value: maximum - step },
    // Interior positions, so the leading-zero padding is exercised at a digit
    // boundary and away from one.
    { why: 'midpoint', value: mid },
    { why: 'quarter', value: quarter },
    // Clamping: below minimum and above maximum. `getNumberIndexString` rewrites
    // these to the boundary, so the string must equal the at-minimum /
    // at-maximum case. A port that forgets to clamp produces a shorter or
    // negative string here and the diff shows it.
    { why: 'below-minimum-clamped', value: clampBelow },
    { why: 'above-maximum-clamped', value: clampAbove },
    { why: 'far-below-minimum-clamped', value: -1_000_000 },
    { why: 'far-above-maximum-clamped', value: 1_000_000 },
    // `undefined` is normalised to 0 inside the number branch, before clamping.
    { why: 'undefined-becomes-zero', value: undefined },
  ];
  // Fractional values only matter when the schema declares decimals; upstream
  // still runs them through Math.floor otherwise, which is worth pinning.
  if (typeof multipleOf === 'number' && String(multipleOf).includes('.')) {
    probes.push(
      { why: 'fractional-at-minimum', value: minimum + multipleOf / 2 },
      { why: 'fractional-near-maximum', value: maximum - multipleOf / 2 },
      { why: 'rounding-at-multiple', value: minimum + step * 1.5 }
    );
  }
  return probes;
}

function stringProbes(maxLength) {
  return [
    { why: 'empty', value: '' },
    { why: 'single-char', value: 'a' },
    { why: 'exactly-max-length', value: 'a'.repeat(maxLength) },
    { why: 'longer-than-max-length', value: 'a'.repeat(maxLength + 5) },
    { why: 'max-length-uppercase', value: 'Z'.repeat(maxLength) },
    // Sorting-relevant: space is the pad character, and these are the values
    // that sort around it. A port that pads with '\x00' instead of ' ' moves
    // these cases.
    { why: 'leading-space', value: ' ab' },
    { why: 'trailing-space-inside', value: 'ab c' },
    { why: 'null-char', value: 'a\x00b' },
    { why: 'tab', value: 'a\tb' },
    { why: 'non-ascii', value: 'äöü' },
    { why: 'emoji', value: '😊' },
    { why: 'digits', value: '0123' },
    // Falsy values take the `if (!fieldValue) fieldValue = ''` branch.
    { why: 'missing-field-becomes-empty', value: undefined },
    { why: 'null-becomes-empty', value: null },
  ];
}

function booleanProbes() {
  return [
    { why: 'true', value: true },
    { why: 'false', value: false },
    { why: 'missing-becomes-false', value: undefined },
  ];
}

/**
 * Upstream refuses four fixture indexes. These are not gaps in coverage: they are
 * upstream's validation behaviour, and PLAN.md 4.3 makes schema/index validation
 * part of the contract. A port that accepts any of these silently is wrong, so
 * each refusal is recorded as a first-class case carrying its observable error
 * identity (the RxDB error code where there is one, the thrown class where the
 * failure is an unguarded property access) plus the raw message.
 *
 * Note that each of these fixtures still produces *some* valid indexes: the
 * `_meta.lwt` index is always well formed. So a refusal is per-index, not
 * per-schema, and is recorded as such.
 */
const REFUSAL_CLASSES = {
  /** `getIndexMeta` calls `.replace()` on the index entry, which must be a string. */
  nonStringIndexEntry: {
    class: 'upstream-rejects-non-string-index-entry',
    detail:
      'The index array contains a non-string entry (a literal number). ' +
      '`getIndexMeta` treats each entry as a field path and calls a string ' +
      'method on it, so it throws before any schema lookup happens.',
    rustExpectation:
      'reject at schema/index validation with a typed error, not a panic',
  },
  /** `getStringLengthOfIndexNumber` reads `schemaPart.minimum` unconditionally. */
  missingNumericBounds: {
    class: 'upstream-requires-numeric-index-bounds',
    detail:
      'A numeric or object-typed field is used in an index without declaring ' +
      '`minimum` and `maximum`. Those two numbers are what sizes the fixed-width ' +
      'digit budget, so there is no valid index string to produce and upstream ' +
      'dereferences the absent bounds instead of validating them.',
    rustExpectation:
      'reject at schema validation with a typed error naming the field',
  },
  /** The one refusal that is already a proper typed RxDB error. */
  missingIndexedField: {
    class: 'upstream-rejects-missing-indexed-field',
    detail:
      'An index names a field path the schema does not declare. Upstream raises ' +
      'RxDB error CI1 with the offending path in `parameters.fieldName`.',
    rustExpectation: 'must be a typed CI1-equivalent error, never a panic',
  },
};

/**
 * Which refusal a given (schema, index) is expected to hit. A `null` means the
 * index is expected to work, so an unexpected throw is a generator bug and is
 * reported as such instead of being filed as upstream behaviour.
 */
function expectedRefusal(name, index) {
  const hasNonString = index.some((entry) => typeof entry !== 'string');
  if (hasNonString) return REFUSAL_CLASSES.nonStringIndexEntry;
  if (name === 'noStringIndex') return REFUSAL_CLASSES.missingNumericBounds;
  if (name === 'notExistingIndex') return REFUSAL_CLASSES.missingIndexedField;
  return null;
}

/**
 * Turn a caught error into the observable identity a Rust test can assert on.
 * `code` is only present when upstream raised a real RxDB error; a bare
 * `TypeError` is still observable behaviour and is recorded as its class.
 */
function describeError(err) {
  const errorName = err && err.constructor ? err.constructor.name : typeof err;
  return {
    errorName,
    errorCode: err && typeof err.code === 'string' ? err.code : null,
    rxdbError: !!(err && err.rxdb),
    errorUrl: err && typeof err.url === 'string' ? err.url : null,
    parameters: err && err.parameters ? { ...err.parameters } : null,
    // The raw message is kept because for the untyped cases it is the only hint
    // about which property access failed.
    rawMessage: String((err && err.message) || err)
      .split('\n')
      .map((line) => line.trim())
      .filter(Boolean)
      .join(' '),
  };
}

/** Ask the oracle for the leaf schema part of a dotted path, as upstream does. */
function schemaPartAt(schema, path) {
  return path.split('.').reduce((part, seg) => {
    if (!part) return undefined;
    return part.properties ? part.properties[seg] : undefined;
  }, schema);
}

function indexFieldInfo(oracle, schema, path) {
  const part = schemaPartAt(schema, path);
  if (!part) {
    throw new Error(`fixture index ${path} is not in the defaulted schema properties`);
  }
  return { path, part, type: part.type };
}

/**
 * Build the input document for one index: the index fields get the probe value,
 * everything else gets a type-plausible filler so the monad can walk the path.
 */
function docFor(entries) {
  const doc = {};
  for (const { path, value } of entries) {
    const segments = path.split('.');
    let cursor = doc;
    for (let i = 0; i < segments.length - 1; i++) {
      cursor[segments[i]] = cursor[segments[i]] || {};
      cursor = cursor[segments[i]];
    }
    cursor[segments[segments.length - 1]] = value;
  }
  return doc;
}

export async function run({ check, repo }) {
  const oracle = await loadOracle();
  const { customIndex, queryPlanner, rxSchema, utilsHash, fixtures } = oracle.modules;
  const { INDEX_MAX, INDEX_MIN } = queryPlanner;

  const outFile = join(repo, 'testdata', 'vectors', `${FAMILY}.json`);

  /**
   * Every fixture schema upstream ships, keyed by its export name. The defaulted
   * form (`createRxSchema().jsonSchema`) is what index building actually sees:
   * `_deleted` is prepended to each index, the primary key appended, and the
   * `_meta.lwt` index added.
   */
  const fixtureSchemas = Object.entries(fixtures)
    .filter(([, value]) => value && typeof value === 'object' && value.primaryKey)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));

  /**
   * Cases are grouped per (schema, index) so the cap below can take a fair share
   * of each group instead of truncating the tail of a flat list. A flat cap
   * silently drops whole schemas, which reads as "covered" when it is not.
   */
  const groups = [];
  const rejectionGroups = [];
  let currentGroup = null;
  /** Refusals, split so an unexpected one cannot hide among the expected ones. */
  const rejections = [];
  const unexpectedRejections = [];

  for (const [name, literal] of fixtureSchemas) {
    let schema;
    try {
      schema = rxSchema.createRxSchema(literal, utilsHash.defaultHashSha256).jsonSchema;
    } catch (err) {
      rejectionGroups.push({
        schema: name,
        index: null,
        cases: [
          {
            schema: name,
            index: null,
            kind: 'rejection',
            why: 'upstream-rejects-schema',
            rejected: true,
            ...describeError(err),
          },
        ],
      });
      unexpectedRejections.push({ schema: name, index: null, ...describeError(err) });
      continue;
    }

    // `schema.indexes` on the defaulted schema is exactly what storages build.
    const indexes = (schema.indexes || []).map((idx) => (Array.isArray(idx) ? idx.slice() : [idx]));

    for (const index of indexes) {
      currentGroup = { schema: name, index, cases: [] };
      groups.push(currentGroup);
      // One monad per index; the case list is the cross product of the probes
      // for each field, but kept small by probing one field at a time against
      // neutral fillers. That is the shape a Rust test wants: hold everything
      // else fixed, move one field, compare the whole index string.
      /**
       * Probe every entry point upstream offers for this index. A refusal at any
       * of them is recorded as a case rather than skipped: "upstream threw here"
       * is behaviour the port has to match, and it is the only evidence we have
       * that upstream rejects the shape at all.
       */
      let monad;
      let infos;
      let refusal = null;
      try {
        monad = customIndex.getIndexableStringMonad(schema, index);
        // `getIndexStringLength` is a separate entry point with its own failure
        // mode (it is what reads `.minimum` for the digit budget), so it is
        // probed explicitly rather than inferred from the monad succeeding.
        customIndex.getIndexStringLength(schema, index);
        infos = index.map((path) => indexFieldInfo(oracle, schema, path));
      } catch (err) {
        refusal = { ...describeError(err), atConstructor: monad ? 'getIndexStringLength' : 'getIndexableStringMonad' };
      }

      if (refusal) {
        const expected = expectedRefusal(name, index);
        currentGroup.cases.push({
          schema: name,
          index,
          kind: 'rejection',
          why: expected ? expected.class : 'UNEXPECTED-REJECTION',
          // The full list of entry points that were tried, so the case says what
          // was attempted and not only what failed.
          entryPointsTried: ['getIndexableStringMonad', 'getIndexStringLength', 'getSchemaByObjectPath'],
          rejected: true,
          ...refusal,
          upstreamBehaviour: expected
            ? { class: expected.class, detail: expected.detail }
            : null,
          rustExpectation: expected ? expected.rustExpectation : 'investigate: this index was expected to work',
        });
        if (!expected) {
          unexpectedRejections.push({ schema: name, index, ...refusal });
        } else {
          rejections.push({ schema: name, index, class: expected.class, errorCode: refusal.errorCode, errorName: refusal.errorName });
        }
        continue;
      }
      const neutral = infos.map((info) => {
        if (info.type === 'number' || info.type === 'integer') {
          return {
            path: info.path,
            value: typeof info.part.minimum === 'number' ? info.part.minimum : 0,
          };
        }
        if (info.type === 'boolean') {
          return { path: info.path, value: false };
        }
        return { path: info.path, value: '' };
      });

      const record = (why, overrides) => {
        const entries = neutral.map((n) =>
          Object.prototype.hasOwnProperty.call(overrides, n.path)
            ? { path: n.path, value: overrides[n.path] }
            : n
        );
        const doc = docFor(entries);
        let indexString;
        try {
          indexString = monad(doc);
        } catch (err) {
          indexString = { threw: String(err && err.message) };
        }
        currentGroup.cases.push({
          schema: name,
          index,
          field: Object.keys(overrides)[0] || null,
          kind: 'index-string',
          why,
          // Inputs are recorded as written; `stableValue` in the writer turns
          // undefined into an absent key, which is what the monad sees too.
          document: doc,
          indexString,
        });
      };

      for (const info of infos) {
        const probes =
          info.type === 'boolean'
            ? booleanProbes()
            : info.type === 'string'
              ? stringProbes(info.part.maxLength)
              : numericProbes(
                  info.part.minimum,
                  info.part.maximum,
                  info.part.multipleOf
                );
        for (const probe of probes) {
          record(`${info.path}:${probe.why}`, { [info.path]: probe.value });
        }
      }

      // The whole index at once, at both extremes, so concatenation and the
      // field order are pinned in one case rather than inferred from N.
      const allAtMinimum = {};
      const allAtMaximum = {};
      for (const info of infos) {
        if (info.type === 'boolean') {
          allAtMinimum[info.path] = false;
          allAtMaximum[info.path] = true;
        } else if (info.type === 'string') {
          allAtMinimum[info.path] = '';
          allAtMaximum[info.path] = 'Z'.repeat(info.part.maxLength);
        } else {
          allAtMinimum[info.path] = info.part.minimum;
          allAtMaximum[info.path] = info.part.maximum;
        }
      }
      record('all-fields-at-minimum', allAtMinimum);
      record('all-fields-at-maximum', allAtMaximum);

      // Declared index string length. Storages size buffers with it; a port that
      // computes it differently will disagree on padding above.
      currentGroup.cases.push({
        schema: name,
        index,
        kind: 'index-string-length',
        length: customIndex.getIndexStringLength(schema, index),
      });

      /**
       * Bound helpers. These are the other half of the contract: a query is
       * translated into start/end index strings, and the sentinels are the part
       * that cannot be discovered from a document alone.
       */
      const boundValues = (info) => {
        if (info.type === 'boolean') {
          return [
            { why: 'lower-true', value: true },
            { why: 'lower-false', value: false },
            { why: 'lower-null', value: null },
            { why: 'lower-INDEX_MIN', value: INDEX_MIN },
            { why: 'lower-INDEX_MAX', value: INDEX_MAX },
            { why: 'lower-undefined', value: undefined },
          ];
        }
        if (info.type === 'string') {
          return [
            { why: 'lower-typed', value: 'm' },
            { why: 'lower-empty', value: '' },
            { why: 'lower-null', value: null },
            { why: 'lower-INDEX_MIN', value: INDEX_MIN },
            { why: 'lower-INDEX_MAX', value: INDEX_MAX },
            { why: 'lower-undefined', value: undefined },
          ];
        }
        return [
          { why: 'lower-number', value: Math.floor((info.part.minimum + info.part.maximum) / 2) },
          { why: 'lower-null', value: null },
          { why: 'lower-INDEX_MIN', value: INDEX_MIN },
          { why: 'lower-INDEX_MAX', value: INDEX_MAX },
          { why: 'lower-out-of-range-low', value: -1_000_000 },
          { why: 'lower-out-of-range-high', value: 1_000_000 },
        ];
      };

      // Bounds are positional: the helpers take one entry per index field. Hold
      // the other fields at their neutral value and walk one field at a time.
      for (let fieldIndex = 0; fieldIndex < infos.length; fieldIndex++) {
        const info = infos[fieldIndex];
        for (const probe of boundValues(info)) {
          const lower = neutral.map((n, i) =>
            i === fieldIndex ? probe.value : n.type === 'boolean' ? false : n.value
          );
          const upper = lower.slice();
          let lowerString;
          let upperString;
          try {
            lowerString = customIndex.getStartIndexStringFromLowerBound(schema, index, lower);
          } catch (err) {
            lowerString = { threw: String(err && err.message) };
          }
          try {
            upperString = customIndex.getStartIndexStringFromUpperBound(schema, index, upper);
          } catch (err) {
            upperString = { threw: String(err && err.message) };
          }
          currentGroup.cases.push({
            schema: name,
            index,
            field: info.path,
            kind: 'bounds',
            why: probe.why,
            bound: probe.value,
            lowerBoundString: lowerString,
            upperBoundString: upperString,
          });
        }
      }
    }
  }

  /**
   * Bounded, deterministic, and honest about what got dropped.
   *
   * Two rounds of fairness, because both failures are invisible in a flat list:
   * a flat cap would cover only the alphabetically first schemas, and a plain
   * round-robin over per-schema groups would drop every `bounds` case (they are
   * appended last within each group, so the cap would keep only index strings
   * and look complete). So: stratify by (kind, schema) first, then round-robin
   * across the strata.
   */
  /**
   * Rejection cases are never subject to the cap. They are few, they are the only
   * evidence of upstream's validation behaviour, and dropping one would silently
   * turn "upstream rejects this" into "we never checked".
   */
  const allGroups = [...groups, ...rejectionGroups];
  const totalCases = allGroups.reduce((n, g) => n + g.cases.length, 0);
  const strata = new Map();
  for (const group of allGroups) {
    for (const one of group.cases) {
      const key = `${group.schema}\u0000${one.kind}`;
      if (!strata.has(key)) strata.set(key, []);
      strata.get(key).push(one);
    }
  }
  const strataList = [...strata.entries()]
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(([, list]) => list);
  const rejectionCases = strataList
    .flat()
    .filter((one) => one.kind === 'rejection');
  const producibleStrata = strataList
    .map((list) => list.filter((one) => one.kind !== 'rejection'))
    .filter((list) => list.length > 0);
  const budget = MAX_CASES - rejectionCases.length;
  const kept = [];
  for (let round = 0; kept.length < budget; round++) {
    let added = 0;
    for (const list of producibleStrata) {
      if (round < list.length && kept.length < budget) {
        kept.push(list[round]);
        added += 1;
      }
    }
    if (added === 0) break;
  }
  // Emit in a stable order: producible cases then rejections, each already
  // ordered by (schema, kind) strata, so the file diff is readable.
  kept.push(...rejectionCases);
  const dropped = totalCases - kept.length;

  const doc = {
    ...oracle.header(
      'Index strings produced by upstream src/custom-index.ts for every index of every ' +
        'upstream fixture schema, including values at and beyond the schema-declared ' +
        'minimum/maximum so the clamping behaviour is pinned (PLAN.md 7.2). ' +
        '`index` is the defaulted index (prepends _deleted, appends the primary key) ' +
        'because that is what the storage layer builds. `indexString` is the raw ' +
        'string; a `{ "threw": ... }` value means upstream threw for that input.'
    ),
    format: 'schema+index+document->indexString',
    determinism: determinismHeader(),
    /**
     * `caseCount` counts every case in `cases`, including `kind: "rejection"`
     * entries. Those are not missing coverage: they are upstream refusing an
     * index (a non-string index entry, a numeric index field with no declared
     * bounds, an index naming a field the schema does not have). PLAN.md 4.3
     * makes that validation part of the contract, so each refusal is recorded
     * with its observable error identity: the RxDB code (`CI1`) where upstream
     * raises a typed error, and the thrown class plus raw message where it does
     * not (`TypeError` from an unguarded property access).
     */
    caseCount: kept.length,
    producedCaseCount: kept.length - rejectionCases.length,
    rejectionCaseCount: rejectionCases.length,
    droppedCaseCount: dropped,
    /** Roll-up of the `kind: "rejection"` cases, by refusal class. */
    rejections,
    /** Empty means every refusal was the expected one. */
    unexpectedRejections,
    cases: kept,
  };

  return emitFamily({ outFile, doc, check, label: FAMILY, caseCount: kept.length });
}
