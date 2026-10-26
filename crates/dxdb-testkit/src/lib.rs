//! dxdb's conformance testkit: the ported storage contract.
//!
//! PLAN.md section 2 names two upstream files as the acceptance oracle for every
//! storage crate in this workspace — `test/unit/rx-storage-implementations.test.ts`
//! (4,115 lines) and `test/unit/rx-storage-query-correctness.test.ts` (2,663 lines)
//! — and says to port them first because "every storage after that is mostly
//! plumbing". This crate is that port.
//!
//! # What is here
//!
//! | Module | What it is |
//! | --- | --- |
//! | [`util`] | upstream's `test-utils` / `async-test-util` ergonomics: `random_token`, `random_string`, `wait_until`, `is_fast_mode`, `next_tick`, `clone_value` |
//! | [`schemas`] | `plugins/test-utils/schemas.ts`, ported |
//! | [`fixtures`] | `schema-objects.ts` generators, the write-data idiom, and the 25 extracted correctness groups |
//! | [`decl`] | the case declaration table: 90 cases, each naming its upstream file and line |
//! | [`harness`] | opening instances, building `PreparedQuery`s, assertion helpers |
//! | [`suite`] | the executable suites, generic over the storage traits |
//! | [`report`] | the coverage tally a lane cites |
//!
//! # The crate is generic, on purpose
//!
//! The only workspace dependency is [`dxdb_storage`]. It does not name a concrete
//! storage, not even in `dev-dependencies` (the one exception is
//! `dxdb-memory` for the gate tests, which is the reference storage and is what
//! every other storage is measured against). That is what lets a clean-room storage
//! — which must not depend on the port's core — be measured by this crate.
//!
//! # What this crate does not do
//!
//! Two capabilities live in other lanes and are *declared* here rather than
//! approximated:
//!
//! * **the query planner** (`dxdb-query`, lane B). Upstream's suites call
//!   `prepareQuery` and assert on its `selectorSatisfiedByIndex` output. This crate
//!   builds the weakest possible plan instead and says so in [`harness`]; the
//!   planner-dependent cases are [`decl::PortStatus::NeedsQueryCrate`].
//! * **`fillWithDefaultSettings`** and schema hashing (`dxdb-schema`, also lane B).
//!   [`harness::fill_with_default_settings`] is a documented stopgap covering only
//!   the parts a storage reads.
//!
//! # Fidelity
//!
//! Every ported case names its upstream file and line. The declaration table is
//! **generated** from the upstream source by `gen/declare_cases.py`, so a line
//! number cannot drift from the file it claims to describe, and the correctness
//! suite's data is extracted by `gen/gen_correctness.py` rather than transcribed.
//! Both generators support `--check`, which the Makefile's
//! `testkit-declaration-check` target runs.
//!
//! # No `unsafe`, no `tokio`
//!
//! `#![forbid(unsafe_code)]`, and concurrency is `futures` plus the sequential
//! `Spawner` from D3. A case that needs to wait yields to the executor rather than
//! sleeping on a clock, because the default build must run on
//! `wasm32-unknown-unknown` where there is no thread to sleep on.

#![forbid(unsafe_code)]

pub mod decl;
pub mod fixtures;
pub mod harness;
pub mod report;
pub mod schemas;
pub mod suite;
pub mod util;

pub use decl::{Case, Coverage, Kind, PortStatus};
pub use harness::{
    build_correctness_query, build_prepared_query, fill_with_default_settings, Harness,
    InstanceSpec, mango_query,
};
pub use schemas::{
    average_schema, hero_array, human, human_age_index, human_composite_primary,
    human_minimal, human_with_all_index, nested_doc_schema, nested_human, no_index_human,
    point, pseudo_schema_for_version, simple_array_hero, test_data_schema,
};
pub use util::{
    clone_value, current_seed, fast_or, is_fast_mode, next_tick, random_number, random_string,
    random_string_with_special_chars, random_token, seed, wait, wait_until, NoSleep, Rng,
    Sleeper, WaitUntilError, WaitUntilTimeout, YieldSleeper,
};
