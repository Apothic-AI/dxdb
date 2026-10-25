//! The reference in-memory storage: dxdb's parity oracle.
//!
//! PLAN.md section 4.1 makes this the only storage `dxdb-core` may name in
//! dev-dependencies, and section 2 makes the two upstream conformance suites
//! (`rx-storage-implementations.test.ts`, `rx-storage-query-correctness.test.ts`)
//! its acceptance gate. Every other storage is expected to behave identically.
//!
//! Scaffolded in M0; the implementation lands with `dxdb-storage` in M2.

#![forbid(unsafe_code)]
