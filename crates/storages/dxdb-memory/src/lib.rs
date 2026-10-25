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

pub mod index_strings;
