//! Upstream's `test-utils` / `async-test-util` ergonomics, ported.
//!
//! Every function here exists because a ported test calls it. The point is that a
//! translation stays one-to-one and reviewable against the original: when a
//! ported case reads `randomToken(12)` upstream, it reads `random_token(12)`
//! here, with the same default, the same alphabet, and the same result shape.
//!
//! | upstream | here | upstream source |
//! | --- | --- | --- |
//! | `randomToken(len = 10)` | [`random_token`] | `plugins/utils/utils-string.ts:10` |
//! | `randomString(len = 8, charset = a-z)` | [`random_string`] | `async-test-util/dist/es/random-string.js` |
//! | `randomStringWithSpecialChars(min, max)` | [`random_string_with_special_chars`] | `test-utils/schema-objects.ts:25` |
//! | `randomNumber(min = 0, max = 1000)` | [`random_number`] | `plugins/utils/utils-number.ts:7` |
//! | `wait(ms = 0)` | [`wait`] | `async-test-util/dist/es/wait.js` |
//! | `waitUntil(fun, timeout = 0, interval = 20)` | [`wait_until`] | `async-test-util/dist/es/wait-until.js` |
//! | `nextTick()` | [`next_tick`] | `plugins/utils/utils-promise.ts:4` |
//! | `isFastMode()` | [`is_fast_mode`] | `test-utils/config.ts:57` |
//! | `clone(obj)` | [`clone_value`] | `plugins/utils/utils-object.ts:265` |
//!
//! # Time and randomness are injected, not ambient
//!
//! Upstream calls `Math.random()` and `setTimeout` directly. Both are ambient
//! state, and ambient state is what makes a conformance suite produce a failure
//! nobody can reproduce. So:
//!
//! * randomness goes through [`Rng`], seeded from [`seed`], and a test that fails
//!   can be re-run with `DXDB_TESTKIT_SEED=<printed seed>` to replay it exactly;
//! * waiting goes through [`Sleeper`]. The default [`YieldSleeper`] yields to the
//!   executor and counts polls instead of consulting a clock, which works on
//!   `wasm32-unknown-unknown` where there are no threads and (for the default
//!   build) no host clock callback. A platform binding that wants wall-clock
//!   time supplies its own `Sleeper`.
//!
//! This mirrors D3 (execution is injected, default sequential) and D4 (time is
//! injected) rather than inventing a third convention.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

// ---------------------------------------------------------------------------
// Randomness
// ---------------------------------------------------------------------------

/// A deterministic, seedable pseudo-random generator.
///
/// Not cryptographic and not `Math.random`-compatible — nothing here depends on
/// the *values*, only on being varied, which is what upstream's fixtures need
/// ("alternating string lengths can reproduce various problems"). Being seedable
/// is the property upstream lacks and a conformance suite wants.
///
/// `Cell<u64>` rather than `&mut self` so a fixture helper can draw from one
/// shared generator while handing out `&self` references, which is how upstream's
/// module-level `Math.random` behaves.
#[derive(Debug, Default)]
pub struct Rng {
    state: Cell<u64>,
}

impl Rng {
    /// A generator with an explicit seed.
    pub fn new(seed: u64) -> Self {
        // splitmix64's finalizer, so that a seed of 0 or a small counter still
        // produces a well-distributed first draw.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng {
            state: Cell::new(z | 1),
        }
    }

    /// The next raw 64-bit draw (xorshift64*).
    pub fn next_u64(&self) -> u64 {
        let mut x = self.state.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// The equivalent of `Math.random()`: a value in `[0, 1)`.
    pub fn next_f64(&self) -> f64 {
        // 53 bits of mantissa is exactly what a JS double can hold, and matches
        // V8's `Math.random` resolution.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// The equivalent of `Math.floor(Math.random() * n)` for `n > 0`.
    fn next_index(&self, n: usize) -> usize {
        debug_assert!(n > 0);
        ((self.next_f64() * n as f64) as usize).min(n - 1)
    }
}

thread_local! {
    /// The process-wide generator upstream gets from `Math.random()`.
    static GLOBAL_RNG: Rng = Rng::new(initial_seed());
    /// The seed the global generator was created with, for the failure message.
    static GLOBAL_SEED: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Reads `DXDB_TESTKIT_SEED`, falling back to a fixed default.
///
/// A fixed default means a failing suite is reproducible by default, which is the
/// opposite of upstream's `Math.random()` and the right trade: the fixtures only
/// need *varied* values, and reproducing a failure is worth more than an
/// unrepeatable draw.
fn initial_seed() -> u64 {
    let seed = std::env::var("DXDB_TESTKIT_SEED")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(0x5EED_D5DB_0071_2345);
    GLOBAL_SEED.with(|cell| cell.set(Some(seed)));
    seed
}

/// Reseeds the process-wide generator, so a failing run can be replayed.
pub fn seed(value: u64) {
    GLOBAL_RNG.with(|rng| rng.state.set(Rng::new(value).state.get()));
    GLOBAL_SEED.with(|cell| cell.set(Some(value)));
}

/// The seed in use, printed by a failing run's panic message.
pub fn current_seed() -> u64 {
    // Force initialisation so the value reported is the value in use.
    GLOBAL_RNG.with(|_| ());
    GLOBAL_SEED.with(|cell| cell.get()).unwrap_or(0)
}

/// Draws from the process-wide generator.
pub fn with_rng<T>(f: impl FnOnce(&Rng) -> T) -> T {
    GLOBAL_RNG.with(f)
}

/// `randomNumber(min = 0, max = 1000)`, inclusive at both ends.
///
/// `plugins/utils/utils-number.ts:7`:
/// `Math.floor(Math.random() * (max - min + 1)) + min`.
pub fn random_number(min: i64, max: i64) -> i64 {
    assert!(min <= max, "random_number: min {min} > max {max}");
    with_rng(|rng| rng.next_index((max - min + 1) as usize) as i64 + min)
}

/// `COUCH_NAME_CHARS` from `utils-string.ts:1`.
pub const COUCH_NAME_CHARS: &str = "abcdefghijklmnopqrstuvwxyz";

/// `randomToken(length = 10)` — `plugins/utils/utils-string.ts:10`.
///
/// Lowercase letters only, so the result is valid as a database name, a
/// collection name, *and* a JavaScript identifier. Ported exactly, because a
/// storage that behaves differently for a name with a hyphen is a real bug and
/// upstream's choice of alphabet is why it does not see one.
pub fn random_token(length: usize) -> String {
    random_string(length, COUCH_NAME_CHARS)
}

/// `randomString(length = 8, charset = 'a-z')` — `async-test-util/dist/es/random-string.js`.
pub fn random_string(length: usize, charset: &str) -> String {
    let chars: Vec<char> = charset.chars().collect();
    assert!(!chars.is_empty(), "random_string: empty charset");
    with_rng(|rng| {
        let mut text = String::new();
        for _ in 0..length {
            text.push(chars[rng.next_index(chars.len())]);
        }
        text
    })
}

/// `TEST_DATA_CHARSET` — `test-utils/schema-objects.ts:17`.
///
/// Upstream: "Some storages had problems with umlauts and other special chars.
/// So we add these to all test strings." The umlauts are load-bearing: a storage
/// that compares UTF-8 bytes instead of UTF-16 code units, or that mangles
/// multi-byte characters, only fails against a string containing one.
pub const TEST_DATA_CHARSET: &str =
    "0987654321ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyzäöüÖÄßÜ[]{}'";

/// The emoji upstream appends to the charset — `schema-objects.ts:19`.
pub const TEST_DATA_EMOJIS: [&str; 6] = ["😊", "💩", "👵", "🍌", "🏳️‍🌈", "😃"];

/// `TEST_DATA_CHARSET_LAST_SORTED` — `schema-objects.ts:18`.
///
/// The largest single UTF-16 code unit in [`TEST_DATA_CHARSET`], by code-unit
/// order (D5). Upstream computes it with `Array.prototype.sort()`, which is
/// code-unit order, so this is a faithful port and not a `localeCompare` sort.
pub fn test_data_charset_last_sorted() -> char {
    TEST_DATA_CHARSET
        .chars()
        .max_by_key(|c| {
            let mut buffer = [0u16; 2];
            c.encode_utf16(&mut buffer).first().copied().unwrap_or(0)
        })
        .expect("TEST_DATA_CHARSET is not empty")
}

/// UTF-16 code unit length, i.e. JavaScript's `String.prototype.length`.
///
/// Used by [`random_string_with_special_chars`], which must reproduce upstream's
/// emoji-driven length overrun. `dxdb-jsstr` owns the ordering contract (D5);
/// this is only the length, so it does not pull in a second dependency.
fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `randomStringWithSpecialChars(minLength, maxLength)` —
/// `test-utils/schema-objects.ts:25`.
///
/// Three behaviours are ported exactly, because each was added upstream to
/// reproduce a specific class of bug:
///
/// 1. **Variable length.** "It has shown that alternating string lengths can
///    reproduce various problems", so the length is drawn per call rather than
///    fixed.
/// 2. **No emoji in the first position.** Upstream notes foundationdb fails when
///    an index string *starts* with an emoji.
/// 3. **Recursive retry when the emoji overruns the length.** An emoji is two
///    UTF-16 code units, so the loop can overshoot; upstream detects
///    `text.length > length` and calls itself again with fresh randomness.
///    A naive truncation would silently produce a string shorter than
///    `minLength`, so the retry is the bug-fix, not a nicety.
///
/// # Panics
///
/// Panics where upstream throws: a missing or inverted length range.
pub fn random_string_with_special_chars(min_length: usize, max_length: usize) -> String {
    assert!(
        min_length > 0 && max_length > 0 && min_length <= max_length,
        "randomStringWithSpecialChars(): invalid length given {} {}",
        min_length,
        max_length
    );

    // Upstream: `baseChars = TEST_DATA_CHARSET.split('')` and
    // `allChars = baseChars.concat(someEmojisArr)`. The emoji entries are whole
    // *strings*, and one of them is a multi-scalar sequence, so the pool is a
    // pool of `&str` rather than of `char` — drawing a `char` would split the
    // flag emoji apart and produce a string that never occurs upstream.
    let base_chars: Vec<&str> = {
        let mut collected = Vec::new();
        let mut offset = 0usize;
        for ch in TEST_DATA_CHARSET.chars() {
            let end = offset + ch.len_utf8();
            collected.push(&TEST_DATA_CHARSET[offset..end]);
            offset = end;
        }
        collected
    };
    let mut all_chars: Vec<&str> = base_chars.clone();
    all_chars.extend(TEST_DATA_EMOJIS);

    let length = random_number(min_length as i64, max_length as i64) as usize;
    let mut text = String::new();
    while js_length(&text) < length {
        // First character never comes from the emoji pool (upstream's TODO about
        // foundationdb failing on an index string that starts with an emoji).
        let choice = if text.is_empty() {
            with_rng(|rng| base_chars[rng.next_index(base_chars.len())])
        } else {
            with_rng(|rng| all_chars[rng.next_index(all_chars.len())])
        };
        text.push_str(choice);
    }

    if js_length(&text) > length {
        // An emoji is two UTF-16 code units, so a draw can overshoot the target
        // length. Upstream retries with fresh randomness rather than truncating;
        // the retry is the bug-fix, because truncation would produce a string
        // shorter than `min_length`.
        return random_string_with_special_chars(min_length, max_length);
    }

    text
}

// ---------------------------------------------------------------------------
// Fast mode
// ---------------------------------------------------------------------------

/// `isFastMode()` — `test-utils/config.ts:57`.
///
/// Upstream: `ENV_VARIABLES.NODE_ENV === 'fast'`, wrapped in a try/catch that
/// returns `false` if the environment is unreadable. Three ported cases consult it
/// to scale their document counts (100 vs 200, 100 vs 10,000, 40 vs 90), so a
/// suite that ignores it takes 100x longer than it needs to without testing
/// anything more.
///
/// Reads `NODE_ENV` under the same name upstream uses, so a caller running the
/// Rust suite in CI with `NODE_ENV=fast` gets the same scaling as the JS suite
/// would. `DXDB_FAST_TEST_MODE` is accepted as an alias because setting
/// `NODE_ENV` for a Rust binary surprises people.
pub fn is_fast_mode() -> bool {
    let node_env = std::env::var("NODE_ENV").unwrap_or_default();
    node_env == "fast" || std::env::var_os("DXDB_FAST_TEST_MODE").is_some()
}

/// The document count to use, given upstream's fast-mode alternative.
///
/// Cleans up the `isFastMode() ? 100 : 200` idiom that appears three times in the
/// implementations suite, so the ported call sites are one expression.
pub fn fast_or(slow: usize, fast: usize) -> usize {
    if is_fast_mode() {
        fast
    } else {
        slow
    }
}

// ---------------------------------------------------------------------------
// clone
// ---------------------------------------------------------------------------

/// Deep clone of a JSON value — `plugins/utils/utils-object.ts:265` (`clone`).
///
/// `serde_json::Value` is already a tree of owned values, so a clone is a clone.
/// The function exists rather than inlining `.clone()` because upstream's
/// fixture code calls `clone(...)` at specific points *for a reason*: the write
/// path may mutate its input (upstream's `RX_COLLECTION_BULK_INSERT_CONTEXT`
/// documents that a wrapper may take ownership), so a test that passes the same
/// object twice without cloning is testing something different.
pub fn clone_value(value: &serde_json::Value) -> serde_json::Value {
    value.clone()
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

/// How a [`wait`] / [`wait_until`] call spends its interval.
///
/// The default implementation counts *polls* rather than milliseconds. That is a
/// deliberate difference from upstream, which sleeps on a real timer: a
/// conformance suite that sleeps on real timers is slow, flaky under load, and
/// impossible to run deterministically. What the suites actually need is "give the
/// storage's own background work a chance to run", and yielding to the executor is
/// what provides that on a single-threaded runtime.
///
/// A platform binding that wants real time supplies a different implementation;
/// nothing in the suite depends on which one is in use.
pub trait Sleeper {
    /// Prepares to spend one interval.
    fn sleep(&self) -> SleepFuture;
}

impl std::fmt::Debug for dyn Sleeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sleeper")
    }
}

/// The future [`Sleeper::sleep`] returns.
pub struct SleepFuture {
    remaining: usize,
}

impl SleepFuture {
    /// A future that is immediately ready.
    pub fn ready() -> Self {
        SleepFuture { remaining: 0 }
    }
}

impl Future for SleepFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.remaining == 0 {
            Poll::Ready(())
        } else {
            self.remaining -= 1;
            // Self-wake, so this terminates under any executor, including one
            // that would otherwise park forever because nothing else is ready.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// The default sleeper: yields to the executor a bounded number of times.
///
/// One yield is `remaining = 1`. Upstream's default `interval` is 20 ms; a yield
/// is the closest executor-neutral equivalent and is what makes `wait_until`
/// usable on `wasm32-unknown-unknown` without a timer capability.
#[derive(Debug, Clone, Copy)]
pub struct YieldSleeper {
    /// How many times to yield per interval.
    pub yields: usize,
}

impl Default for YieldSleeper {
    fn default() -> Self {
        YieldSleeper { yields: 1 }
    }
}

impl YieldSleeper {
    /// One yield per interval.
    pub fn new() -> Self {
        Self::default()
    }

    /// A sleeper that yields `yields` times per interval, for a test that needs
    /// to give a storage's background task more room.
    pub fn with_yields(yields: usize) -> Self {
        YieldSleeper { yields }
    }
}

impl Sleeper for YieldSleeper {
    fn sleep(&self) -> SleepFuture {
        SleepFuture {
            remaining: self.yields,
        }
    }
}

/// A sleeper that returns immediately. For a deterministic test that has already
/// driven the work to completion.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSleep;

impl Sleeper for NoSleep {
    fn sleep(&self) -> SleepFuture {
        SleepFuture::ready()
    }
}

/// `wait(ms = 0)` — an interval, measured by the [`Sleeper`].
///
/// Upstream name kept so a ported call site reads the same. There is no
/// millisecond argument because the default sleeper has no clock: a caller that
/// wants a real delay supplies a `Sleeper` that sleeps for real.
pub async fn wait(sleeper: &dyn Sleeper) {
    sleeper.sleep().await;
}

/// `nextTick()` — `plugins/utils/utils-promise.ts:4`.
///
/// Upstream is `new Promise(res => setTimeout(res, 0))`: "let everything already
/// queued run first". A single executor yield is the faithful equivalent for a
/// single-threaded runtime, which is what D3 makes the default.
pub async fn next_tick() {
    YieldSleeper::new().sleep().await;
}

/// Why [`wait_until`] gave up.
#[derive(Clone, Debug, PartialEq)]
pub struct WaitUntilTimeout {
    /// How many times the predicate was polled.
    pub attempts: usize,
    /// The timeout the caller asked for, if any.
    ///
    /// `f64` because upstream's is milliseconds; not `Eq` because `f64` is not.
    pub timeout: Option<f64>,
}

impl std::fmt::Display for WaitUntilTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.timeout {
            Some(timeout) => write!(
                f,
                "AsyncTestUtil.waitUntil(): reached timeout of {}ms after {} attempts",
                timeout, self.attempts
            ),
            None => write!(
                f,
                "AsyncTestUtil.waitUntil(): gave up after {} attempts",
                self.attempts
            ),
        }
    }
}

impl std::error::Error for WaitUntilTimeout {}

/// `waitUntil(fun, timeout = 0, interval = 20)` —
/// `async-test-util/dist/es/wait-until.js`.
///
/// Upstream's loop, ported in order, because the order is observable:
///
/// 1. check `ok` first, so a predicate that is already true resolves without
///    waiting an interval (upstream reaches this via a first `runLoopOnce`);
/// 2. otherwise wait one interval, then poll the predicate;
/// 3. a predicate that *errors* rejects immediately rather than being retried
///    (`["catch"](err => reject(err))` upstream) — that is what makes a broken
///    storage surface as a failure instead of a timeout;
/// 4. a false result loops.
///
/// `timeout` is expressed in *intervals* here, not milliseconds, for the reason
/// given on [`Sleeper`]: the default sleeper has no clock. `max_attempts` is the
/// hard bound that keeps a wrong assertion from hanging the suite, which upstream
/// gets from its `timeout` default of 0 meaning "no timeout" but its test runner
/// getting bored instead.
///
/// # Errors
///
/// Returns [`WaitUntilTimeout`] when the bound is reached, and propagates the
/// predicate's error.
pub async fn wait_until<F, Fut, T, E>(
    sleeper: &dyn Sleeper,
    max_attempts: usize,
    mut predicate: F,
) -> Result<(), WaitUntilError<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    T: Into<bool>,
{
    let mut attempts = 0usize;
    loop {
        attempts += 1;
        match predicate().await {
            Ok(value) => {
                if value.into() {
                    return Ok(());
                }
            }
            // Upstream: `promisify(fun()).catch(err => reject(err))`.
            Err(err) => return Err(WaitUntilError::Predicate(err)),
        }
        if attempts > max_attempts {
            return Err(WaitUntilError::Timeout(WaitUntilTimeout {
                attempts,
                timeout: None,
            }));
        }
        sleeper.sleep().await;
    }
}

/// The failure modes of [`wait_until`].
#[derive(Clone, Debug, PartialEq)]
pub enum WaitUntilError<E> {
    /// The bound was reached with the predicate still false.
    Timeout(WaitUntilTimeout),
    /// The predicate itself failed.
    Predicate(E),
}

impl<E: std::fmt::Debug> std::fmt::Display for WaitUntilError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitUntilError::Timeout(inner) => write!(f, "{inner}"),
            WaitUntilError::Predicate(err) => write!(f, "waitUntil() predicate failed: {err:?}"),
        }
    }
}

impl<E: std::fmt::Debug> std::error::Error for WaitUntilError<E> {}

/// A counter-based stand-in for a predicate, so a test can prove
/// [`wait_until`]'s retry and bound behaviour without a storage.
pub struct PollCounter<'a> {
    calls: std::cell::Cell<usize>,
    succeed_on: usize,
    observer: Option<&'a std::cell::Cell<usize>>,
}

impl<'a> PollCounter<'a> {
    /// Succeeds on the `succeed_on`-th poll (1-based).
    pub fn new(succeed_on: usize) -> Self {
        PollCounter {
            calls: std::cell::Cell::new(0),
            succeed_on,
            observer: None,
        }
    }

    /// Also records the poll count into `observer`, which outlives the borrow.
    pub fn observe(mut self, observer: &'a std::cell::Cell<usize>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// The number of times the predicate has been called.
    pub fn calls(&self) -> usize {
        self.calls.get()
    }

    /// Polls once; `true` on and after the configured poll.
    ///
    /// The `Result` mirrors the shape [`wait_until`] takes, so the counter can be used
    /// as a predicate without an adapter.
    pub fn poll(&self) -> Result<bool, std::convert::Infallible> {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if let Some(observer) = self.observer {
            observer.set(n);
        }
        Ok(n >= self.succeed_on)
    }
}
