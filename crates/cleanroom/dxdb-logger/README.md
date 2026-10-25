# `dxdb-logger`

Structured records for every operation that reaches a storage engine.

This crate is clean-room: it was implemented from
[`specs/logger.md`](../../../specs/logger.md), which was written from public
documentation before any code existed. Provenance is recorded per crate in
[`specs/READING-LIST.md`](../../../specs/READING-LIST.md). No upstream paid source
was consulted, and no upstream paid format is reproduced.

## What it does

Wrap a storage and every documented operation produces two structured records: a
start record and exactly one terminal record (`end`, `error`, or `cancelled`).
Records go to `tracing` and to a hook trait, and they can be converted into a span
that matches the public Sentry span interface.

The wrapper returns the inner storage's values unchanged. That is the invariant
everything else rests on: adding logging cannot change what a query returns, what
an error says, or what a count is.

## The documented option set, mapped

The upstream documentation lists a `settings` object and three callbacks. Every
one of them exists here; see `specs/logger.md` section 2 for the full table.

| Documented | dxdb |
| --- | --- |
| `prefix` (default `''`) | `LoggerSettings::prefix` |
| `times` (default `true`) | `LoggerSettings::times` |
| `metaStorageInstances` (default `true`) | `LoggerSettings::meta_storage_instances` |
| ten per-operation booleans (default `true`) | `LoggerSettings::operations` / `OperationToggles` |
| `onOperationStart` | `OperationHooks::on_operation_start` |
| `onOperationEnd` | `OperationHooks::on_operation_end` |
| `onOperationError` | `OperationHooks::on_operation_error` |
| — | `OperationHooks::on_operation_cancelled` (dxdb addition, G-028) |

## Using it

```rust
use std::sync::Arc;
use dxdb_logger::{
    Clock, LoggerSettings, LoggerStorage, NoopHooks, OperationName, OperationToggles,
};

# fn demo<S>(inner: S, clock: Arc<dyn Clock>) -> LoggerStorage<S> {
let logged = LoggerStorage::new(
    inner,
    LoggerSettings::default()
        .with_prefix("app")
        .with_operation(OperationName::Close, false),
    Arc::new(NoopHooks),
    clock,
);
# logged
# }
```

Three collaborators are explicit rather than defaulted:

* **hooks** — the documented callback trio, as one trait with defaulted methods.
* **clock** — because `std::time::Instant::now()` aborts on
  `wasm32-unknown-unknown`, so the target supplies its own clock
  (`docs/CLEANROOM-GAPS.md` G-022). On native, `MonotonicClock` is provided.
* **settings** — the documented object, with the documented defaults.

## Sentry / APM

`SentrySpan::from_records(start, terminal, ids)` turns a record pair into a span
matching the public interface: `span_id` (16 hex), `trace_id` (32 hex),
`start_timestamp`, `timestamp`, `op`, `description`, `status`, and a `data` map
carrying OpenTelemetry-shaped `db.*` attributes.

There is no Sentry SDK dependency, no transport, no API key, and no network call.
Compatibility is a documented field set plus a pure function
(`docs/CLEANROOM-GAPS.md` G-026). Wire it to your own client.

## Deliberate divergences from the paid plugin

Each is recorded in [`docs/CLEANROOM-GAPS.md`](../../../docs/CLEANROOM-GAPS.md):

* Records carry identifiers and counts, never document contents. G-001.
* No `console.time`; the duration is a structured field. G-021.
* The record schema and log format are ours, so a parser written for the paid
  plugin's output needs a formatter. G-001.
* No artificial limits, no trial state, no phone-home, no flag check. G-006.

## Invariants

Proven against this crate's own contract, not against a reference implementation.
Each invariant in `specs/logger.md` section 3 names the test that proves it:
`inv_01_transparency_is_total`, `inv_02_start_and_terminal_are_paired`,
`inv_02_cancellation_is_terminal`, `inv_03_filtered_operations_are_silent_and_cheap`,
`inv_04_log_ids_are_unique_and_paired`,
`inv_05_durations_are_exact_and_present_iff_timing_enabled`,
`inv_06_sentry_spans_are_valid`,
`inv_07_start_precedes_terminal_in_delivery_order`.

The settings space is finite, so `inv_01` enumerates all 8,192 combinations
(2^10 operation subsets, timing on/off, meta filter on/off) instead of sampling
them. No random generator is involved and every failure is reproducible.
