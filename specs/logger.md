# `dxdb-logger`: behavioural specification

| | |
| --- | --- |
| Stage | B1 (CLEANROOM.md section 7) |
| Crate | `crates/cleanroom/dxdb-logger` |
| Documented upstream module | `logger` (CLEANROOM.md section 4 row 17) |
| Spec author read | `docs-src/docs/logger.md`, `docs-src/docs/releases/15.0.0.md:110`, the Apache-2.0 storage interface and helper, the Sentry span interface, OTel database span conventions, `tracing` crate docs |
| Spec author did not read | `rxdb-premium` in any form (source, bundles, `.d.ts`, source maps, installer, payloads) |
| Full provenance | [`READING-LIST.md`](./READING-LIST.md), row `dxdb-logger` |
| Gap rows this spec depends on | G-001, G-003, G-004, G-005, G-006, G-020, G-021, G-022, G-023, G-024, G-025, G-026, G-027, G-028, G-029 |
| Status | implemented and verified (B1) |

## 1. Observable behaviour

### 1.1 It is a storage wrapper, and nothing else

> "The logger is a wrapper that can be wrapped around any RxStorage. Once your
> storage is wrapped, you can create your database with the wrapped storage and
> the logging will automatically happen."
> — `docs-src/docs/logger.md`, "Using the logger plugin"

Observable: wrapping a storage changes what is logged and nothing else. Every
document, query result, count, attachment, checkpoint and error that goes
through the wrapper is the value the inner storage produced. The wrapper does not
inspect document contents, does not reorder results, and does not rewrite errors.
A caller that swaps the wrapped storage for the plain storage cannot tell the
difference from data alone, only from the log.

This is the invariant the whole feature rests on, and it is the reason this crate
is safe to place anywhere in a storage stack (`INV-1`).

### 1.2 Every documented operation is logged

> "With the logger plugin you can log all operations to the storage layer of your
> RxDatabase." — `docs-src/docs/logger.md`

The settings list enumerates the operations: `bulkWrite`, `findDocumentsById`,
`query`, `count`, `info`, `getAttachmentData`, `getChangedDocumentsSince`,
`cleanup`, `close`, `remove`. That is the complete documented operation set, and
dxdb logs exactly those ten. It does not log `createStorageInstance`, the change
stream, or `getAttachmentData` internals, because the documentation does not list
them and adding unlogged-but-plausible operations would be inventing behaviour.

Each logged operation produces a **start** record and exactly one **terminal**
record: `end` on success, `error` on failure, and `cancelled` if the caller
dropped the operation before it finished. Never more than one. A failed operation
has no duration in the "completed successfully" sense, and one that emits both
`end` and `error` would double-count in an APM. See G-025.

`cancelled` is dxdb's addition, not a documented upstream behaviour: a JavaScript
`Promise` has no cancellation, so the paid plugin could never have emitted one. In
Rust, dropping a future mid-flight is routine (`select!`, a timeout wrapper, a
cancelled query), and leaving its start record unpaired would make `INV-2` false
in the one place storage code is most likely to be interrupted. It maps onto the
public Sentry status `cancelled`, which the span interface documents as "the
operation was cancelled, typically by the caller". Recorded in G-028.

### 1.3 Timing

> "if true, it will log timings with `console.time()` and `console.timeEnd()`"
> — `docs-src/docs/logger.md`, on `settings.times`

Observable in dxdb: with `times: true` (the default, "By default, all settings are
true"), every terminal record carries the elapsed time between the start and the
terminal record, and the wrapper reads the clock exactly once at each of those two
points. With `times: false`, no duration is recorded **and the clock is never
read**, so disabling timings removes the instrumentation cost rather than merely
hiding the number (G-021, `INV-5`).

`console.time`/`console.timeEnd` is a browser API with no Rust equivalent and no
observable contract beyond "a duration is reported somewhere". dxdb reports the
duration in nanoseconds as an integer field, from a caller-supplied `Clock`, which
also makes it deterministic under test and available on targets whose `std` has no
clock at all (G-022).

### 1.4 Selectivity

> "You can specify what to log so that your logs are less noisy."

Three filters, all documented:

- `prefix` — "can used to prefix all log strings, default=''".
- `metaStorageInstances` — "if false, it will not log meta storage instances like
  used in replication".
- the ten per-operation booleans.

Observable: a filtered-out operation produces no records at all and still executes
normally (`INV-3`). The default for all three groups is on.

A meta storage instance is identified by collection name. The two names the open
tree uses are `_rxdb_internal` (`src/rx-storage-helper.ts:51`, the instance RxDB
creates at `src/rx-database.ts:769`) and `rxdatabase_storage_local`
(`src/rx-storage-helper.ts:52`). Those strings are Apache-2.0 constants, they are
part of the open tree's on-disk naming, and they are the only public signal
available; using them is recorded in G-024. dxdb's port may later add an explicit
marker, at which point that becomes the test and the strings stay as fallbacks.

### 1.5 Custom log functions

> "onOperationStart: (operationsName, logId, args) => void,
> onOperationEnd: (operationsName, logId, args) => void,
> onOperationError: (operationsName, logId, args, error) => void"

Observable: three callbacks, called at the three moments, with an operation name,
an id that pairs a start with its terminal record, the call's arguments summary,
and — for the error callback — the error. dxdb models this as one `OperationHooks`
trait with three methods, because a Rust struct with three optional closures is a
worse API than a trait with three defaults (G-004).

### 1.6 APM / Sentry compatibility

> "This is useful to debug performance problems and for monitoring with
> Application Performance Monitoring (APM) tools like Bugsnag, Datadog, Elastic,
> Sentry and others."

Observable: one conversion function produces a JSON span matching the public
Sentry span interface — required `span_id` (16 hex characters), `trace_id` (32 hex
characters), `start_timestamp` and `timestamp` (Unix seconds, `start <= end`),
recommended `op`, optional `status`, `description`, `data`, `origin`, `tags`.
`status` comes from the documented 17-state vocabulary. `data` keys are
lower-case with underscores, per Sentry's span data conventions, and carry
database span attributes in the OpenTelemetry style.

There is no Sentry SDK dependency, no transport, no API key and no network call:
compatibility is a documented field set plus a pure function (G-026). The crate
emits records; the user's APM subscriber consumes them.

## 2. Public API shape (from docs)

The documented JavaScript call shape, and dxdb's mapping. Every documented option
appears exactly once; nothing is added that the documentation does not name, and
nothing named in the documentation is dropped.

| Documented | dxdb | Notes |
| --- | --- | --- |
| `wrappedLoggerStorage({ storage })` | `LoggerStorage::new(inner, settings, hooks, clock)` | constructor takes the inner storage plus the three collaborators; see G-020 |
| `settings.prefix` (default `''`) | `LoggerSettings::prefix: String` (default `""`) | appears on every record |
| `settings.times` (default `true`) | `LoggerSettings::times: bool` (default `true`) | see 1.3 |
| `settings.metaStorageInstances` (default `true`) | `LoggerSettings::meta_storage_instances: bool` (default `true`) | see 1.4 |
| `settings.bulkWrite` … `settings.remove` (default `true`, ten flags) | `LoggerSettings::operations: OperationToggles` | one bool per operation, all default `true` |
| `onOperationStart(name, logId, args)` | `OperationHooks::on_operation_start(&LogRecord)` | record carries name, id, details |
| `onOperationEnd(name, logId, args)` | `OperationHooks::on_operation_end(&LogRecord)` | success only |
| `onOperationError(name, logId, args, error)` | `OperationHooks::on_operation_error(&LogRecord)` | record carries the error |
| *(log the operation)* | `tracing` events at `INFO`, target `dxdb_logger`, plus the record fields | see 1.6 and G-021 |

Types:

```rust
pub struct LogRecord {                      // one record, one phase of one operation
    pub log_id: LogId,                      // u64, unique per operation
    pub operation: OperationName,           // the ten documented names
    pub phase: Phase,                       // Start | End | Error
    pub database_name: String,
    pub collection_name: String,
    pub prefix: String,
    pub wall_clock_ns: Option<u64>,         // None when the clock has no wall time
    pub duration_ns: Option<u64>,           // Some iff timing enabled and phase is End
    pub details: OperationDetails,          // argument summary, never document contents
    pub error: Option<StorageError>,        // Some iff phase is Error
}

pub enum OperationName { BulkWrite, FindDocumentsById, Query, Count, Info,
                         GetAttachmentData, GetChangedDocumentsSince, Cleanup, Close, Remove }

pub struct LoggerSettings { prefix, times, meta_storage_instances, operations }
pub struct OperationToggles { /* one bool per OperationName */ }
pub trait OperationHooks { on_operation_start, on_operation_end, on_operation_error }
pub trait Clock { fn monotonic_ns(&self) -> u64; fn unix_ns(&self) -> Option<u64>; }
pub struct SentrySpan { span_id, trace_id, parent_span_id, op, description, status,
                        start_timestamp, timestamp, data, origin }
```

The wrapped-storage surface is `dxdb_logger::storage`: `LoggedStorage` (the
factory, `name` + `create_instance`) and `LoggedStorageInstance` (the ten
operations). Documents, queries, checkpoints and attachments are opaque to this
crate: the logger must not be able to look inside them, which is how `INV-1` is
enforced by the type system rather than by discipline. That trait is an explicit
stand-in for the port's own storage traits, which did not exist when B1 ran; see
G-020 and section 6.

## 3. Invariants

Each invariant names what enforces it and which test proves it. "Enforced by"
means a mechanism in the code; "proved by" means a test that fails if the
mechanism is removed.

**INV-1 — transparency.** For every operation, the value the wrapper returns
equals the value the inner storage returns, including error values, and no
document contents appear in any record.
*Enforced by* the wrapper delegating and returning the inner value directly, plus
opaque document types in the envelope. *Proved by* `inv_01_transparency_is_total`
over an exhaustive settings matrix, and `acceptance_12_...`.

**INV-2 — exactly one start and one terminal record per logged operation.** The
terminal record is `End` on success and `Error` on failure; never both, never
neither.
*Enforced by* a single code path that emits the terminal record once, branching on
the result. *Proved by* `inv_02_start_and_terminal_are_paired`, which drives
successes and failures through every operation.

**INV-3 — a filtered operation produces no records and no clock reads.** If the
operation's toggle is off, or the instance is a filtered meta storage instance,
then zero records mention it, and the clock is not read for it.
*Enforced by* the filter check running before the clock read and before any
emission. *Proved by* `inv_03_filtered_operations_are_silent_and_cheap` (a counting
clock asserts zero reads) and `acceptance_02`, `acceptance_03`.

**INV-4 — log ids are unique and pair records within one process.** No two
operations share a `LogId`, and the start and terminal records of one operation do
so. Ids increase in *poll* order, not call order, because the start record — and
therefore the id — is allocated at first poll (`INV-7`).
*Enforced by* a monotonic counter allocated per wrapped call, inside the future.
*Proved by* `inv_04_log_ids_are_unique_and_paired`, including interleaved
concurrent operations (`acceptance_11`).

**INV-5 — durations are honest.** `duration_ns` is `Some` if and only if timing is
enabled and the phase is `End`, and its value equals the difference between the
terminal and start clock readings, so it is never negative and never invented.
*Enforced by* taking both readings from the same `Clock` inside the call and
subtracting with a saturating operation. *Proved by*
`inv_05_durations_are_exact_and_present_iff_timing_enabled`.

**INV-6 — emitted spans are valid Sentry spans.** Every conversion satisfies the
public interface: 16 hex characters in `span_id`, 32 in `trace_id`, `start <=
end`, `status` in the documented vocabulary, lower-case underscored `data` keys.
*Enforced by* construction through a typed `SentrySpan` builder that owns the
hex encoding and the status enum. *Proved by* `inv_06_sentry_spans_are_valid` and
`acceptance_13_...`.

**INV-7 — order within a record pair.** For a given `LogId`, the start record is
delivered before its terminal record, and a start record is emitted at the
operation's first poll, never at future construction, so an operation that is
never polled emits nothing and costs nothing.

What this invariant does **not** say, deliberately: that all start records of
concurrently-created operations precede all their terminal records. That would be a
claim about the executor's polling schedule, not about the logger. A caller may
build two futures and have the first driven to completion before the second is
polled at all — `futures::future::join` polls in order, and these operations
resolve on their first poll — and no wrapper that defers its start record to first
poll can avoid that. The logger's responsibility is the pair: every terminal record
closes one log id, and no operation emits a record without having been polled.

*Enforced by* opening the record inside the returned future rather than in the
method body, awaiting the inner call between the two emissions, and calling hooks
synchronously in the emitting task. *Proved by*
`inv_07_start_precedes_terminal_in_delivery_order` (pairing and non-interleaving)
and `inv_07_unpolled_operations_emit_nothing` (nothing is emitted for an operation
that was never polled), together with `acceptance_11_...`.

## 4. Failure modes

| Failure | What the caller sees | What the logger does |
| --- | --- | --- |
| Inner operation returns an error | the inner error, unchanged, plus an `Error` record with `error` set | emits `Error`, calls `on_operation_error`, no `End` record, no duration |
| Inner `bulk_write` reports per-row errors | the inner response, unchanged; `details.error_count > 0` and the record is still `End` (the *call* succeeded) | counts errors for the record, does not convert a partial write into a failed call |
| Inner storage is closed | the inner error (typically kind `Closed`) | logs it like any other error |
| Optional operation unsupported | the documented not-supported answer, which is what the storage layer above interprets as "use the fallback" | `Error` record with kind `NotSupported`; never fabricates a result (G-023) |
| Clock has no wall time (wasm target, G-022) | unaffected: records carry `wall_clock_ns: None` | Sentry conversion returns `NoWallClock` rather than a span with a fake timestamp |
| Hook panics | the panic propagates to the caller | no catch: a logger that swallows panics corrupts the caller's error handling. Documented, and the port's JS boundary decides whether to contain it |
| Tracing subscriber absent | nothing observable | `tracing` no-ops; the records still reach hooks |
| Meta storage instance, filter off | nothing observable | no records, no clock reads |
| Two operations interleave | complete pairs, each start before its terminal, distinct ids | per-call ids, no shared state, no lock on the record path |
| A future is built but never polled | nothing at all | the start record, its id and the clock read all happen inside the future |
| The caller drops the future mid-flight | the inner call is dropped with it; a `cancelled` record closes the pair | the terminal-record guard fires from `Drop` (`INV-2`) |
| A batch write carries thousands of ids | the record stays small | ids are capped at `OperationDetails::MAX_LOGGED_IDS` (16) and the true count is kept separately |
| Wrapper used after inner `close` | whatever the inner storage does with it | not special-cased: the wrapper has no liveness state of its own |

Errors are the inner storage's own error type (`StorageError` in this crate's
envelope). The logger adds exactly one error of its own: `NoWallClock`, from the
Sentry conversion, and nothing else. A wrapper that invents error kinds forces
every caller above it to handle errors the storage never produces.

## 5. Performance targets

Targets only. Nothing here may be restated as a result until a number from
`docs/BENCHMARKS.md` exists (G-003).

| Target | Value | How it is measured |
| --- | --- | --- |
| Added latency per logged operation, timing on, recording hooks installed | **unmeasured**; target ≤ 1 µs on x86-64 release | `criterion` bench `logger_overhead` in `benches/overhead.rs` (B1 ships the bench target; the number is not yet recorded) |
| Added latency per operation, timing off | **unmeasured**; target ≤ 100 ns | same bench, `times: false` |
| Records per operation | exactly 2 (start + terminal) | asserted by `INV-2`; not a measurement |
| Allocations | **unmeasured**; target ≤ 2 per record (`String` prefix and `Vec` details excluded) | bench + `dhat` when available |

Upstream publishes no number for the logger. There is nothing to compare against,
which is itself worth stating: the logger is the one clean-room module where the
plan has no external performance claim to chase.

## 6. Non-goals

- **No `console.time`.** There is no Rust equivalent and its output format is not
  a contract (G-021).
- **No document contents in records.** Logging payloads would change the memory
  profile of every write and leak user data into APM systems. The record carries
  counts and identifiers, never the documents. This is a deliberate narrowing of
  "log every operation".
- **No Sentry SDK, no transport, no network.** G-026, and CLEANROOM.md section 3
  (no phone-home).
- **No log-format compatibility with the paid plugin.** G-001. A consumer of
  upstream's log lines needs a formatter we have not written.
- **No `RXDB_UTILS_GLOBAL`/`PREMIUM_FLAG_HASH` machinery, no collection limit, no
  trial state.** G-006.
- **No clock of its own on wasm.** The host supplies one (G-022).
- **No change stream logging.** Not in the documented operation list.
- **Not a storage engine.** This crate wraps; it stores nothing.
- **Not dyn-compatible on purpose.** The wrapper is generic over the inner
  storage. The port's `dynosaur` twin arrives with the real traits (G-020); adding
  a hand-written trait object now would be a second trait to maintain.
- **No sample rate / probabilistic logging** in B1: not documented, and an
  undocumented knob is worse than a missing one.

## Acceptance list

Numbered, observable, each implemented by a test that names its number. Tests
live in `crates/cleanroom/dxdb-logger/tests/`.

1. With all toggles enabled, each of the ten documented operations produces
   exactly one `Start` record and exactly one `End` record naming that operation,
   and the inner storage is called exactly once. `[INV-2]`
2. With an operation's toggle disabled, that operation produces zero records and
   the call returns the inner result unchanged. `[INV-1, INV-3]`
3. With `meta_storage_instances: false`, no records are produced for a collection
   named `_rxdb_internal` or `rxdatabase_storage_local`, and records are still
   produced for an ordinary collection name. `[INV-3]`
4. The prefix appears verbatim on every record, and defaulting to an empty string
   leaves it empty. `[INV-2]`
5. With timing enabled and a manual clock, each `End` record's duration equals the
   difference between the terminal and start clock readings, exactly. `[INV-5]`
6. With timing disabled, no record carries a duration and the clock is never read.
   `[INV-3, INV-5]`
7. `on_operation_start` fires for the start record, `on_operation_end` for a
   successful terminal record, `on_operation_error` for a failed one, and a failed
   operation never fires `on_operation_end`. `[INV-2]`
8. A failing operation returns the inner error value unchanged and records it once
   with `phase == Error`. `[INV-1, INV-2]`
9. `get_changed_documents_since` forwards its result when the inner storage
   reports support, reports not-supported without calling the inner storage when
   it does not, and never fabricates a result. `[INV-1]`
10. The `info` operation exists as a setting and is logged when the inner storage
    supports it, and reports not-supported otherwise (G-005). `[INV-1, INV-2]`
11. Operations that interleave in time produce distinct log ids, each pairing a
    start with its own terminal record, with the start observed first. `[INV-4,
    INV-7]`
12. Payload transparency: documents, query results, counts, attachments and
    checkpoints pass through by value equality, and no record contains document
    contents. `[INV-1]`
13. A completed operation maps to a Sentry span with the public interface's
    required and recommended fields, a status from the documented vocabulary, and
    `start_timestamp <= timestamp`. `[INV-6]`
14. Emitting a record reaches a `tracing` subscriber with the documented field
    names (`log_id`, `operation`, `phase`, `collection`, `prefix`, `duration_ns`).
    `[INV-2]`

## Test map

| Acceptance | Test | File |
| --- | --- | --- |
| 1 | `acceptance_01_all_documented_operations_are_logged` | `tests/acceptance.rs` |
| 2 | `acceptance_02_disabled_operation_is_silent_and_unchanged` | `tests/acceptance.rs` |
| 3 | `acceptance_03_meta_storage_instances_are_filtered_by_collection_name` | `tests/acceptance.rs` |
| 4 | `acceptance_04_prefix_appears_on_every_record` | `tests/acceptance.rs` |
| 5 | `acceptance_05_duration_equals_clock_readings` | `tests/acceptance.rs` |
| 6 | `acceptance_06_timing_off_never_reads_the_clock` | `tests/acceptance.rs` |
| 7 | `acceptance_07_hooks_map_one_to_one_onto_phases` | `tests/acceptance.rs` |
| 8 | `acceptance_08_errors_pass_through_unchanged_and_are_recorded_once` | `tests/acceptance.rs` |
| 9 | `acceptance_09_optional_changed_documents_since_is_not_fabricated` | `tests/acceptance.rs` |
| 10 | `acceptance_10_info_operation_exists_and_logs_when_supported` | `tests/acceptance.rs` |
| 11 | `acceptance_11_interleaved_operations_pair_by_id_in_order` | `tests/acceptance.rs` |
| 12 | `acceptance_12_payloads_pass_through_without_inclusion` | `tests/acceptance.rs` |
| 13 | `acceptance_13_sentry_span_has_the_documented_field_set` | `tests/acceptance.rs` |
| 14 | `acceptance_14_tracing_receives_the_documented_fields` | `tests/acceptance.rs` |
| INV-1 | `inv_01_transparency_is_total` | `tests/invariants.rs` |
| INV-2 | `inv_02_start_and_terminal_are_paired` | `tests/invariants.rs` |
| INV-3 | `inv_03_filtered_operations_are_silent_and_cheap` | `tests/invariants.rs` |
| INV-4 | `inv_04_log_ids_are_unique_and_paired` | `tests/invariants.rs` |
| INV-5 | `inv_05_durations_are_exact_and_present_iff_timing_enabled` | `tests/invariants.rs` |
| INV-6 | `inv_06_sentry_spans_are_valid` | `tests/invariants.rs` |
| INV-2 (cancel) | `inv_02_cancellation_is_terminal` | `tests/invariants.rs` |
| INV-7 | `inv_07_start_precedes_terminal_in_delivery_order` | `tests/invariants.rs` |
| INV-7 (unpolled) | `inv_07_unpolled_operations_emit_nothing` | `tests/invariants.rs` |

Property-testing note: the settings space is finite (2^10 operation subsets x 2
timing x 2 meta = 8,192), so `inv_01_transparency_is_total` enumerates it
exhaustively rather than sampling it, and the concurrency invariants drive
explicit interleavings instead of random ones. There is no `proptest`
dependency, which also keeps `make wasm` — which builds every target, tests
included — free of a proptest-on-wasm risk. Every failure is reproducible by
construction, which matters more here than sample size: this crate's unit of
correctness is a settings combination, not a random byte string.

## Verification record

Stage B1 closed with all four workspace gates green in one contiguous run on the
shared build server (rustc 1.95.0, `make` targets as written in the `Makefile`):

| Gate | Exit |
| --- | --- |
| `make check` | 0 |
| `make clippy` | 0 |
| `make test` | 0 |
| `make wasm` | 0 |

Test counts in that run, for this crate: 8 unit tests, 14 acceptance tests, 8
invariant tests. Every acceptance number 1-14 and every invariant `INV-1` to
`INV-7` is covered by a passing test named in the test map above. The section 5
performance targets remain **unmeasured**: no bench has been run, so nothing in
this crate may be described as fast.
