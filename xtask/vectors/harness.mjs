/**
 * Shared plumbing for the vector families.
 *
 * Two things in upstream RxDB are nondeterministic by design, and both leak into
 * the data these vectors record:
 *
 *   1. `Math.random()`. It seeds `randomToken()` (which becomes
 *      `eventBulk.id` in `categorizeBulkWriteRows`, and every database /
 *      collection / instance token in replication), and `randomString()` (which
 *      is inside `averageSchema()`'s title, so it feeds the schema hash).
 *   2. `Date.now()`, via `now()`. It sets `_meta.lwt` on replicated documents and
 *      on every checkpoint the replication protocol writes.
 *
 * Both are installed as deterministic stand-ins for the whole process before any
 * oracle module is used, and the fact is recorded in every file's header. The
 * clock and PRNG are reproducible from the file, so a Rust test can inject the
 * same clock and compare checkpoints bit for bit. Freezing them is also what
 * makes `make vectors` idempotent, which is the lane's gate.
 */
import { createHash } from 'node:crypto';

/**
 * xorshift128, seeded. Not cryptographic, not trying to be: it only has to be
 * identical on every run and every machine.
 */
export function createPrng(seed = 0x5eed1e55) {
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

/** The clock start used by the frozen `Date.now`. */
export const CLOCK_START = 1_700_000_000_000;

/** Human-readable description of the determinism stubs, for each file header. */
export const DETERMINISM = {
  mathRandom: 'xorshift128 seeded with 0x5eed1e55, 1 draw per call',
  dateNow: `frozen monotonic clock starting at ${CLOCK_START}, +1 ms per call`,
};

/**
 * Install the stubs. Must run before `loadOracle()` because `randomToken()` and
 * `now()` read their sources lazily but their *results* are baked into fixtures
 * the moment those fixtures are imported.
 */
export function installDeterminism() {
  const next = createPrng();
  Math.random = () => next();

  let tick = 0;
  const RealDate = Date;
  class FrozenDate extends RealDate {
    static now() {
      return CLOCK_START + tick++;
    }
  }
  // Keep everything else (constructors, parsing, `new Date()`) real.
  globalThis.Date = FrozenDate;
}

/** Sort object keys recursively, so key order in the output never drifts. */
export function sortKeysDeep(value) {
  if (Array.isArray(value)) {
    return value.map(sortKeysDeep);
  }
  if (value && typeof value === 'object') {
    const out = {};
    for (const key of Object.keys(value).sort()) {
      out[key] = sortKeysDeep(value[key]);
    }
    return out;
  }
  return value;
}

/** JSON with 2-space indent and a trailing newline, the jsnum.mjs convention. */
export function toJson(doc) {
  return JSON.stringify(doc, null, 2) + '\n';
}

/**
 * Normalise a value that came out of RxDB into something JSON-stable:
 * - `undefined` (and object keys whose value is undefined) are dropped by
 *   JSON.stringify anyway, but dropping them here makes the intent explicit;
 * - `-0` is written as the string `-0` so a reader cannot lose the sign;
 * - non-finite numbers are written by name.
 * Everything else passes through untouched. This is only ever applied to data
 * that RxDB produced, never to inputs we hand it.
 */
export function stableValue(value) {
  if (typeof value === 'number') {
    if (Number.isNaN(value)) return { $number: 'NaN' };
    if (value === Infinity) return { $number: 'Infinity' };
    if (value === -Infinity) return { $number: '-Infinity' };
    if (Object.is(value, -0)) return { $number: '-0' };
    return value;
  }
  if (Array.isArray(value)) {
    return value.map(stableValue);
  }
  if (value && typeof value === 'object') {
    const out = {};
    for (const key of Object.keys(value)) {
      if (typeof value[key] === 'undefined') continue;
      out[key] = stableValue(value[key]);
    }
    return out;
  }
  return value;
}

/** A short sha256 prefix, used to assert two runs agree where a value is opaque. */
export function shortHash(text) {
  return createHash('sha256').update(text).digest('hex').slice(0, 16);
}

/**
 * Write or verify one family file. Mirrors jsnum.mjs's `--check` contract:
 * compare parsed content with the informational `node` field removed, so a Node
 * upgrade that changes nothing is not reported as drift.
 */
export function emitFamily({ fs, outFile, doc, check, label, caseCount }) {
  const json = toJson(doc);
  if (check) {
    let committed;
    try {
      committed = fs.readFileSync(outFile, 'utf8');
    } catch {
      console.error(`${label} vectors missing at ${outFile}; run \`make vectors\``);
      return { ok: false };
    }
    const strip = (text) => {
      const parsed = JSON.parse(text);
      delete parsed.node;
      return JSON.stringify(parsed);
    };
    if (strip(committed) !== strip(json)) {
      console.error(`${label} vectors drifted from the current Node output:`);
      console.error(`  regenerate with \`make vectors\` and review the diff at ${outFile}`);
      return { ok: false };
    }
    console.log(`${label} vectors OK (${caseCount} cases)`);
    return { ok: true };
  }
  fs.mkdirSync(fs.dirname ? fs.dirname(outFile) : undefined, { recursive: true });
  fs.writeFileSync(outFile, json);
  console.log(`wrote ${outFile} (${caseCount} cases, node ${process.version})`);
  return { ok: true };
}
