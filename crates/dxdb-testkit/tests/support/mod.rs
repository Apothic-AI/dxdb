//! Test-only support code.
//!
//! Lives in `tests/` rather than `src/` because none of it belongs in a published
//! library: `reference_storage` is a scaffold whose only job is to keep the gate targets
//! from passing vacuously, and `fuzz` is a harness whose job is to generate cases rather
//! than check a list.

// `support` is included by four test binaries (`storage_implementations`,
// `query_correctness`, `storage_matrix`, `differential_fuzz`) and each uses a
// different subset. `differential_fuzz` is the only user of `fuzz`, so the other
// three see its types as dead — a property of Rust's per-binary compilation of a
// shared module, not of the code. Allowing dead_code here is narrower than
// allowing it on every item in the module.
#[allow(dead_code)]
pub mod fuzz;
pub mod reference_storage;
