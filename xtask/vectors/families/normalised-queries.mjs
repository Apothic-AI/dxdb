/**
 * Family: normalised queries.
 *
 * Upstream: `src/rx-query-helper.ts` (`normalizeMangoQuery`,
 * `getSortComparator`, `getQueryMatcher`), `src/query-planner.ts`
 * (`getQueryPlan`, `isSelectorSatisfiedByIndex`, `INDEX_MIN`, `INDEX_MAX`).
 *
 * `normalizeMangoQuery` is the contract between "what the user wrote" and
 * "what the storage sees". Four things in it are easy to get wrong and are the
 * whole reason this family exists:
 *
 *   1. Selector shorthand (`{ field: value }`) is rewritten to
 *      `{ field: { $eq: value } }` recursively through `$and`/`$or`/`$nor`/`$not`
 *      and `$elemMatch`, but *not* into operator payloads like `$regex` options.
 *   2. The primary key is appended to an explicit `index`, and to `sort`, unless
 *      it is already there. `desc` anywhere forces `sortSatisfiedByIndex: false`,
 *      which is why a `desc` query re-sorts in memory even on a perfect index.
 *   3. With no sort and no index, the sort is *derived* from the schema indexes
 *      by counting how many selector fields each index covers, then falling back
 *      to the first index, then to the primary key. That is not the identity and
 *      not insertion order.
 *   4. `INDEX_MIN` is `Number.MIN_SAFE_INTEGER` (not `-Infinity`) specifically so
 *      that a plan survives `JSON.stringify` when it is shipped to a remote
 *      storage. The round-trip is recorded below.
 *
 * Each case records the input query, the normalised query, and the resulting plan
 * (`startKeys`, `endKeys`, inclusiveness, `sortSatisfiedByIndex`,
 * `selectorSatisfiedByIndex`) so a port can be checked level by level.
 */
import { join } from 'node:path';

import { loadOracle } from '../vendor.mjs';
import { determinismHeader, emitFamily } from '../harness.mjs';

const FAMILY = 'normalised-queries';

/** Fixture schemas upstream cannot plan a query for, and why. */
const KNOWN_BROKEN_SCHEMAS = {
  compoundIndexNoString: 'its declared index contains the literal number 10 instead of a field path',
};

/** Fixture schemas with enough shape to exercise interesting plans. */
const QUERY_SCHEMAS = [
  'human',
  'humanAgeIndex',
  'humanWithAllIndex',
  'compoundIndex',
  'compoundIndexNoString',
  'humanSubIndex',
  'nestedHuman',
  'deepNestedHuman',
  'noIndexHuman',
  'humanWithTimestamp',
  'humanWithTimestampAllIndex',
  'humanWithSimpleAndCompoundIndexes',
  'humanIdAndAgeIndex',
  'point',
  'refHuman',
  'humanCompositePrimary',
  'simpleHumanV3',
  'heroArray',
  'averageSchema',
];

/**
 * Selector/query corpus. Every entry is a query *shape*, applied to whichever
 * fixture schemas have the fields it mentions; fields that do not exist in a
 * schema are dropped by `fieldsOf()` below rather than producing an invalid case.
 */
const QUERY_SHAPES = [
  { why: 'empty-query', query: {} },
  { why: 'empty-selector', query: { selector: {} } },
  { why: 'shorthand-equality', query: { selector: { firstName: 'alice' } } },
  { why: 'explicit-eq', query: { selector: { firstName: { $eq: 'alice' } } } },
  { why: 'numeric-range', query: { selector: { age: { $gt: 20 } } } },
  { why: 'numeric-inclusive-range', query: { selector: { age: { $gte: 20, $lte: 40 } } } },
  { why: 'numeric-lt', query: { selector: { age: { $lt: 40 } } } },
  { why: 'numeric-exclusive-out-of-range', query: { selector: { age: { $lt: -100 } } } },
  { why: 'numeric-above-maximum', query: { selector: { age: { $gt: 1_000_000 } } } },
  { why: 'boolean-eq', query: { selector: { _deleted: { $eq: false } } } },
  { why: 'two-fields-and-implicit', query: { selector: { age: { $gt: 20 }, firstName: { $eq: 'alice' } } } },
  { why: '$and', query: { selector: { $and: [{ age: { $gt: 20 } }, { firstName: { $eq: 'alice' } }] } } },
  { why: '$or', query: { selector: { $or: [{ age: { $gt: 20 } }, { age: { $lt: 5 } }] } } },
  { why: '$nor', query: { selector: { $nor: [{ age: { $gt: 20 } }] } } },
  { why: '$not', query: { selector: { age: { $not: { $gt: 20 } } } } },
  { why: '$in-strings', query: { selector: { firstName: { $in: ['alice', 'bob'] } } } },
  { why: '$in-numbers', query: { selector: { age: { $in: [10, 20, 30] } } } },
  { why: '$in-empty', query: { selector: { age: { $in: [] } } } },
  { why: '$in-mixed-types', query: { selector: { age: { $in: [10, 'twenty'] } } } },
  { why: '$elemMatch', query: { selector: { list: { $elemMatch: { deep1: 'x' } } } } },
  { why: '$regex', query: { selector: { firstName: { $regex: '^a' } } } },
  { why: '$regex-with-options', query: { selector: { firstName: { $regex: '^a', $options: 'i' } } } },
  { why: '$exists', query: { selector: { firstName: { $exists: true } } } },
  { why: '$ne', query: { selector: { firstName: { $ne: 'alice' } } } },
  { why: '$nin', query: { selector: { firstName: { $nin: ['alice'] } } } },
  { why: '$size', query: { selector: { list: { $size: 1 } } } },
  { why: '$type', query: { selector: { firstName: { $type: 'string' } } } },
  { why: '$mod', query: { selector: { age: { $mod: [2, 0] } } } },
  // Sorts: ascending, descending, multi-field, and one that includes the pk.
  { why: 'sort-asc-one-field', query: { selector: {}, sort: [{ firstName: 'asc' }] } },
  { why: 'sort-desc-one-field', query: { selector: {}, sort: [{ firstName: 'desc' }] } },
  { why: 'sort-asc-two-fields', query: { selector: {}, sort: [{ firstName: 'asc' }, { age: 'asc' }] } },
  { why: 'sort-mixed-directions', query: { selector: {}, sort: [{ firstName: 'asc' }, { age: 'desc' }] } },
  { why: 'sort-including-pk', query: { selector: {}, sort: [{ firstName: 'asc' }, { passportId: 'asc' }] } },
  // Explicit index, with and without the primary key.
  { why: 'index-without-pk', query: { selector: {}, index: ['firstName'] } },
  { why: 'index-with-pk', query: { selector: {}, index: ['firstName', 'passportId'] } },
  { why: 'index-and-sort', query: { selector: {}, index: ['age'], sort: [{ age: 'asc' }] } },
  { why: 'index-unknown-field', query: { selector: {}, index: ['notAField'] } },
  // skip/limit permutations.
  { why: 'skip-only', query: { selector: {}, skip: 5 } },
  { why: 'limit-only', query: { selector: {}, limit: 5 } },
  { why: 'skip-and-limit', query: { selector: {}, skip: 5, limit: 10 } },
  { why: 'limit-zero', query: { selector: {}, limit: 0 } },
  { why: 'skip-zero', query: { selector: {}, skip: 0 } },
];

const FIELD_HINTS = ['firstName', 'lastName', 'age', 'passportId', 'name', 'id', 'deep', 'list', 'var1', 'var2'];

/** Drop selector fields the schema does not declare, so cases stay valid. */
function pruneToSchema(schema, query) {
  const has = (path) => {
    const part = path.split('.').reduce((acc, seg) => (acc && acc.properties ? acc.properties[seg] : undefined), schema);
    return !!part;
  };
  const pruneSelector = (selector) => {
    const out = {};
    for (const [field, matcher] of Object.entries(selector || {})) {
      if (field.startsWith('$')) {
        if (Array.isArray(matcher)) {
          out[field] = matcher.map(pruneSelector);
        } else if (matcher && typeof matcher === 'object') {
          out[field] = pruneSelector(matcher);
        } else {
          out[field] = matcher;
        }
        continue;
      }
      if (!has(field)) continue;
      // `$elemMatch` payloads address fields inside arrays, which we cannot
      // resolve by path here; keep them only when the field exists.
      out[field] = matcher;
    }
    return out;
  };
  const pruned = JSON.parse(JSON.stringify(query));
  if (pruned.selector) {
    pruned.selector = pruneSelector(pruned.selector);
  }
  return pruned;
}

export async function run({ check, repo }) {
  const oracle = await loadOracle();
  const { queryHelper, queryPlanner, rxSchema, utilsHash, fixtures } = oracle.modules;
  const { INDEX_MAX, INDEX_MIN } = queryPlanner;

  const outFile = join(repo, 'testdata', 'vectors', `${FAMILY}.json`);

  // `averageSchema` is a function, the rest are objects.
  const schemaByName = (name) => {
    const value = fixtures[name];
    return typeof value === 'function' ? value() : value;
  };

  const cases = [];
  const failures = [];

  for (const schemaName of QUERY_SCHEMAS) {
    const literal = schemaByName(schemaName);
    if (!literal) {
      failures.push({ schema: schemaName, reason: 'fixture export not found' });
      continue;
    }
    const schema = rxSchema.createRxSchema(JSON.parse(JSON.stringify(literal)), utilsHash.defaultHashSha256).jsonSchema;

    for (const shape of QUERY_SHAPES) {
      const query = pruneToSchema(schema, shape.query);
      let normalized;
      let plan;
      let error;
      try {
        normalized = queryHelper.normalizeMangoQuery(schema, query);
      } catch (err) {
        error = String((err && err.message) || err).split('\n')[0];
      }
      if (!error) {
        try {
          plan = queryPlanner.getQueryPlan(schema, normalized);
        } catch (err) {
          error = String((err && err.message) || err).split('\n')[0];
        }
      }

      if (error) {
        // Upstream rejects some of these (an index on a field that is not in the
        // schema, for example). Recorded rather than dropped, and marked when the
        // schema is one upstream cannot plan for at all, so a future reader does
        // not mistake 38 identical errors for a generator defect.
        cases.push({
          schema: schemaName,
          why: shape.why,
          query,
          rejected: error,
          expectedUpstreamFailure: KNOWN_BROKEN_SCHEMAS[schemaName] !== undefined,
        });
        failures.push({
          schema: schemaName,
          why: shape.why,
          reason: error,
          expected: KNOWN_BROKEN_SCHEMAS[schemaName] || null,
        });
        continue;
      }

      // Does the plan survive a trip through JSON? This is the `INDEX_MIN` reason.
      const planJson = JSON.stringify(plan);
      const planRoundTrip = JSON.parse(planJson);
      const roundTripEqual = JSON.stringify(planRoundTrip) === planJson;

      cases.push({
        schema: schemaName,
        why: shape.why,
        query,
        normalizedQuery: normalized,
        plan: {
          index: plan.index,
          startKeys: plan.startKeys,
          endKeys: plan.endKeys,
          inclusiveStart: plan.inclusiveStart,
          inclusiveEnd: plan.inclusiveEnd,
          sortSatisfiedByIndex: plan.sortSatisfiedByIndex,
          selectorSatisfiedByIndex: plan.selectorSatisfiedByIndex,
        },
        planJson,
        planRoundTripEqual: roundTripEqual,
      });
    }
  }

  /**
   * Sentinels and the JSON boundary, recorded explicitly because a Rust port has
   * to pick the same representation for "no lower bound".
   */
  const sentinels = {
    INDEX_MIN,
    INDEX_MAX,
    INDEX_MAXCodePoints: [...INDEX_MAX].map((c) => c.codePointAt(0)),
    indexMinJson: JSON.stringify(INDEX_MIN),
    indexMinRoundTrips: JSON.parse(JSON.stringify(INDEX_MIN)) === INDEX_MIN,
    // The value a naive `-Infinity` choice would produce, and why that breaks:
    minusInfinityJson: JSON.stringify(-Infinity),
    minusInfinityRoundTrips: JSON.parse(JSON.stringify(-Infinity)) === -Infinity,
    isMinSafeInteger: INDEX_MIN === Number.MIN_SAFE_INTEGER,
  };

  /**
   * The sort comparator and matcher are part of the same contract: a normalised
   * query is useless if both sides do not agree on what "sorted" and "matching"
   * mean. A small fixed document set makes the comparator observable.
   */
  const docs = [
    { passportId: 'a', firstName: 'bob', age: 30, _deleted: false },
    { passportId: 'b', firstName: 'alice', age: 30, _deleted: false },
    { passportId: 'c', firstName: 'alice', age: 20, _deleted: true },
    { passportId: 'd', firstName: '', age: 0, _deleted: false },
  ];
  const humanSchema = rxSchema.createRxSchema(
    JSON.parse(JSON.stringify(fixtures.human)),
    utilsHash.defaultHashSha256
  ).jsonSchema;

  const orderings = [];
  for (const shape of [
    { why: 'asc-firstName', sort: [{ firstName: 'asc' }] },
    { why: 'desc-firstName', sort: [{ firstName: 'desc' }] },
    { why: 'desc-age-then-asc-firstName', sort: [{ age: 'desc' }, { firstName: 'asc' }] },
    { why: 'asc-missing-field', sort: [{ notInDocs: 'asc' }] },
  ]) {
    const normalized = queryHelper.normalizeMangoQuery(humanSchema, { selector: {}, sort: shape.sort });
    const comparator = queryHelper.getSortComparator(humanSchema, normalized);
    const sortedIds = docs
      .slice()
      .sort(comparator)
      .map((d) => d.passportId);
    orderings.push({ why: shape.why, normalizedSort: normalized.sort, sortedIds });
  }

  const matchers = [];
  for (const shape of [
    { why: 'empty', selector: {} },
    { why: 'eq-string', selector: { firstName: 'alice' } },
    { why: 'gt-number', selector: { age: { $gt: 20 } } },
    { why: 'deleted-flag', selector: { _deleted: { $eq: false } } },
    { why: 'and', selector: { $and: [{ firstName: 'alice' }, { age: { $gt: 25 } }] } },
    { why: 'or', selector: { $or: [{ age: { $lt: 25 } }, { firstName: 'bob' }] } },
    { why: 'regex', selector: { firstName: { $regex: '^a' } } },
  ]) {
    const normalized = queryHelper.normalizeMangoQuery(humanSchema, { selector: shape.selector });
    const matcher = queryHelper.getQueryMatcher(humanSchema, normalized);
    matchers.push({
      why: shape.why,
      normalizedSelector: normalized.selector,
      matchingIds: docs.filter((d) => matcher(d)).map((d) => d.passportId),
    });
  }

  const doc = {
    ...oracle.header(
      'Normalised mango queries, their plans, and the sort/matcher behaviour they imply, ' +
        'produced by upstream src/rx-query-helper.ts and src/query-planner.ts ' +
        '(PLAN.md 7.2). `normalizedQuery` is `normalizeMangoQuery()` output; `plan` is ' +
        '`getQueryPlan()` on it. `planJson`/`planRoundTripEqual` record whether a plan ' +
        'survives JSON, which is why INDEX_MIN is MIN_SAFE_INTEGER and not -Infinity.'
    ),
    format: 'schema+query->{normalizedQuery, plan}',
    determinism: determinismHeader(),
    sentinels,
    caseCount: cases.length,
    failures,
    cases,
    orderings,
    matchers,
  };

  return emitFamily({ outFile, doc, check, label: FAMILY, caseCount: cases.length });
}
