//! The reference in-memory storage: dxdb's parity oracle.
//!
//! PLAN.md 4.1 makes this the only storage `dxdb-core` may name in
//! dev-dependencies, and section 2 makes the two upstream conformance suites
//! (`rx-storage-implementations.test.ts`, `rx-storage-query-correctness.test.ts`)
//! its acceptance gate. Every other storage is expected to behave identically, so
//! a quirk here becomes a quirk everywhere — which is why the encoding and the
//! write decisions are tested against recorded upstream output rather than against
//! this crate's own expectations.

#![forbid(unsafe_code)]

// No `index_strings` module: the encoder lives in `dxdb-query`, which this crate now
// depends on. Between the first commit and lane B's publication this crate carried a
// local port of `custom-index.ts`; the vectors in `tests/index_strings_oracle.rs` were
// run against both implementations and agree on all 896 index-string cases, 808 bound
// cases and 89 length cases, which is why deleting the duplicate is safe rather than
// hopeful.
