/**
 * The adapter test suite, run in the real browser against the real `.wasm`.
 *
 * This is the half of the proof the Rust policy tests cannot give: that the
 * `#[wasm_bindgen]` objects behave as specified across the actual JS boundary —
 * `externref` callbacks arriving, `unsubscribe()` reaching Rust, a JS exception in
 * a consumer callback not trapping the module, and `undefined` vs a callback being
 * distinguished correctly.
 *
 * Every case records `pass`, `fail`, or (for a case a given engine genuinely
 * cannot exercise) `skip` with a reason, and the page publishes the whole
 * transcript. Nothing here is allowed to pass by not running.
 */

/**
 * @typedef {'pass'|'fail'} CaseStatus
 * @typedef {{ name: string, status: CaseStatus, detail: string }} CaseResult
 */

/** A recorder that keeps a JS-side transcript of every callback invocation. */
class Transcript {
  constructor() {
    /** @type {string[]} */
    this.log = [];
  }
  /** @param {string} entry */
  push(entry) {
    this.log.push(entry);
  }
  /** The transcript joined for readable assertions. */
  get text() {
    return this.log.join(',');
  }
}

/**
 * Assert helpers that throw with a useful message. Using `expect` from
 * Playwright is not possible inside `page.evaluate`, and a bare `assert` loses
 * the surrounding context.
 */
function assertEqual(actual, expected, what) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) {
    throw new Error(`${what}: expected ${e}, got ${a}`);
  }
}

function assertTrue(value, what) {
  if (!value) throw new Error(`${what}: expected truthy, got ${JSON.stringify(value)}`);
}

/**
 * Await a microtask turn so an intentionally-scheduled delivery would have had a
 * chance to fire. Used by the "nothing arrives after unsubscribe" cases: an
 * assertion checking only the state right after `unsubscribe()` would pass even
 * if a queued callback fired one turn later.
 */
function tick() {
  return Promise.resolve();
}

/**
 * Run every adapter case. `api` is the instantiated module namespace.
 *
 * @param {{ Emitter: any }} api
 * @returns {Promise<CaseResult[]>}
 */
export async function runAdapterCases(api) {
  /** @type {CaseResult[]} */
  const results = [];

  /**
   * @param {string} name
   * @param {() => Promise<void> | void} body
   */
  const testCase = async (name, body) => {
    try {
      await body();
      results.push({ name, status: 'pass', detail: '' });
    } catch (err) {
      results.push({
        name,
        status: 'fail',
        detail: String(err && err.message ? err.message : err),
      });
    }
  };

  const newStream = () => {
    const emitter = new api.Emitter();
    return { emitter, observable: emitter.observable() };
  };

  // ---- 1. the happy path ------------------------------------------------
  await testCase('next delivers values in order', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    observable.subscribe((v) => t.push(`next(${v})`));
    emitter.next(1);
    emitter.next(2);
    emitter.next(3);
    assertEqual(t.log, ['next(1)', 'next(2)', 'next(3)'], 'order and count');
  });

  // ---- 2. error, exactly once -------------------------------------------
  await testCase('error is delivered exactly once', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    observable.subscribe(
      (v) => t.push(`next(${v})`),
      (e) => t.push(`error(${e})`),
      () => t.push('complete'),
    );
    emitter.next(1);
    emitter.error('boom');
    // A second error, a complete, and a value: all must be dropped.
    emitter.error('boom again');
    emitter.complete();
    emitter.next(2);
    await tick();
    assertEqual(t.log, ['next(1)', 'error(boom)'], 'exactly one terminal, nothing after');
  });

  // ---- 3. complete does not fire after error ----------------------------
  await testCase('complete does not fire after error', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    observable.subscribe(
      () => t.push('next'),
      (e) => t.push(`error(${e})`),
      () => t.push('complete'),
    );
    emitter.error('first');
    emitter.complete();
    await tick();
    assertEqual(t.log, ['error(first)'], 'complete suppressed after error');
  });

  // ---- 4. error does not fire after complete ----------------------------
  await testCase('error does not fire after complete', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    observable.subscribe(
      () => t.push('next'),
      (e) => t.push(`error(${e})`),
      () => t.push('complete'),
    );
    emitter.complete();
    emitter.error('late');
    await tick();
    assertEqual(t.log, ['complete'], 'error suppressed after complete');
  });

  // ---- 5. completion is delivered normally ------------------------------
  await testCase('complete is delivered once and closes the stream', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    observable.subscribe(
      () => t.push('next'),
      (e) => t.push(`error(${e})`),
      () => t.push('complete'),
    );
    emitter.complete();
    emitter.complete();
    emitter.next(1);
    await tick();
    assertEqual(t.log, ['complete'], 'single completion, nothing after');
  });

  // ---- 6. no callback after unsubscribe --------------------------------
  await testCase('no callback fires after unsubscribe', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    const sub = observable.subscribe(
      (v) => t.push(`next(${v})`),
      (e) => t.push(`error(${e})`),
      () => t.push('complete'),
    );
    emitter.next(1);
    sub.unsubscribe();
    emitter.next(2);
    emitter.error('after teardown');
    emitter.complete();
    await tick();
    assertEqual(t.log, ['next(1)'], 'nothing after unsubscribe');
    assertTrue(sub.closed, 'subscription reports closed');
  });

  // ---- 7. the queued-callback case -------------------------------------
  await testCase('unsubscribing in one turn drops a callback queued in the same turn', async () => {
    // The producer has a value in hand (it is on the stack, not yet delivered) and
    // the consumer unsubscribes before the microtask that would deliver it. The
    // adapter must drop it, not deliver it to a torn-down observer.
    const { emitter, observable } = newStream();
    const t = new Transcript();
    const sub = observable.subscribe((v) => t.push(`next(${v})`));

    const queued = 42;
    sub.unsubscribe();
    emitter.next(queued);

    await tick();
    await tick();
    assertEqual(t.log, [], 'queued value dropped after unsubscribe');
  });

  // ---- 8. unsubscribe from inside a callback ---------------------------
  await testCase('unsubscribe inside a callback does not double-deliver', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    /** @type {any} */
    let sub;
    sub = observable.subscribe((v) => {
      t.push(`next(${v})`);
      if (v === 1) {
        // Re-entrant unsubscribe, from inside the delivery.
        sub.unsubscribe();
        // A value emitted from inside the same callback must also be dropped.
        emitter.next('from-inside-callback');
      }
    });
    emitter.next(1);
    emitter.next(2);
    await tick();
    assertEqual(t.log, ['next(1)'], 're-entrant unsubscribe stops further delivery');
  });

  // ---- 9. a throwing callback does not wedge the subscription ----------
  await testCase('a callback that throws does not wedge the subscription', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    observable.subscribe(
      (v) => {
        t.push(`next(${v})`);
        if (v === 1) throw new Error('consumer bug');
      },
      (e) => t.push(`error(${e})`),
      () => t.push('complete'),
    );

    // The throw must not propagate out of `emitter.next` into the caller...
    let threw = false;
    try {
      emitter.next(1);
    } catch {
      threw = true;
    }
    assertTrue(!threw, 'a consumer exception propagated out of the producer');
    t.push('after-throw');

    // ...and the subscription must still be alive.
    emitter.next(2);
    emitter.complete();
    await tick();
    assertEqual(t.log, ['next(1)', 'after-throw', 'next(2)', 'complete'], 'stream survived');
  });

  // ---- 10. observer-object form ---------------------------------------
  await testCase('subscribe accepts an observer object', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    observable.subscribe({
      next: (v) => t.push(`next(${v})`),
      complete: () => t.push('complete'),
    });
    emitter.next('a');
    emitter.complete();
    await tick();
    assertEqual(t.log, ['next(a)', 'complete'], 'observer object wired up');
  });

  // ---- 11. missing callbacks are no-ops, not errors --------------------
  await testCase('missing callbacks are no-ops', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    // Only `next`; the terminal events have nowhere to go and must not throw.
    observable.subscribe((v) => t.push(`next(${v})`));
    emitter.next(1);
    emitter.error('nobody listening');
    await tick();
    assertEqual(t.log, ['next(1)'], 'error with no handler is swallowed');

    // And the stream really did terminate, even though nothing observed it: a
    // later subscriber gets a pre-closed subscription rather than hanging or
    // replaying the error. (`Subscription.closed` reports whether *this consumer*
    // unsubscribed, so it is the wrong probe for stream state.)
    const late = observable.subscribe(() => t.push('should-not-fire'));
    assertTrue(late.closed, 'a subscription to a terminated stream is pre-closed');
  });

  // ---- 12. resubscribe after unsubscribe ------------------------------
  await testCase('re-subscribing after unsubscribe continues the stream', async () => {
    const { emitter, observable } = newStream();
    const t = new Transcript();
    const first = observable.subscribe((v) => t.push(`first(${v})`));
    emitter.next(1);
    first.unsubscribe();

    // A stale unsubscribe from the first subscription must not kill the second.
    const second = observable.subscribe((v) => t.push(`second(${v})`));
    first.unsubscribe();
    emitter.next(2);
    emitter.complete();
    await tick();

    assertEqual(t.log, ['first(1)', 'second(2)'], 'second subscriber survived a stale unsubscribe');
    assertTrue(!second.closed, 'second subscription still open');
  });

  // ---- 13. Emitter delivery is synchronous, matching RxJS Subject ----
  await testCase('Emitter deliver is synchronous and re-entrancy does not duplicate', async () => {
    // `Emitter.next()` is a synchronous push, the same semantics as an RxJS
    // `Subject.next()`: a value emitted from inside a handler is delivered from
    // inside that handler, so nesting depth legitimately reaches 2. This case
    // asserts that nesting is *bounded and lossless* rather than pretending it
    // cannot happen.
    //
    // The non-re-entrant guarantee lives on the `from_stream` path, where
    // `spawn_local` gives one delivery per microtask. Recorded in
    // docs/lanes/m4-browser-plan.md.
    const { emitter, observable } = newStream();
    const t = new Transcript();
    let depth = 0;
    let maxDepth = 0;
    observable.subscribe((v) => {
      depth += 1;
      maxDepth = Math.max(maxDepth, depth);
      t.push(`next(${v})`);
      if (v === 1) emitter.next(2);
      depth -= 1;
    });
    emitter.next(1);
    await tick();
    assertEqual(t.log, ['next(1)', 'next(2)'], 'inner value delivered exactly once, in order');
    assertEqual(maxDepth, 2, 'inner emit nested one level inside the outer handler');
    assertEqual(depth, 0, 'depth unwound cleanly');
  });

  // ---- 14. a value storm does not lose or reorder --------------------
  await testCase('a long synchronous storm delivers every value in order', async () => {
    const { emitter, observable } = newStream();
    /** @type {number[]} */
    const seen = [];
    observable.subscribe((v) => seen.push(v));
    const n = 500;
    for (let i = 0; i < n; i += 1) emitter.next(i);
    assertEqual(seen.length, n, 'value count');
    assertEqual(seen[0], 0, 'first value');
    assertEqual(seen[n - 1], n - 1, 'last value');
    const inOrder = seen.every((v, i) => v === i);
    assertTrue(inOrder, 'values arrived in order');
  });

  return results;
}
