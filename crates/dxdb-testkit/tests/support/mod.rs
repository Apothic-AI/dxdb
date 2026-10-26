//! Test-only support code.
//!
//! Lives in `tests/` rather than `src/` because none of it belongs in a published
//! library: `reference_storage` is a scaffold whose only job is to keep the gate targets
//! from passing vacuously, and `fuzz` is a harness whose job is to generate cases rather
//! than check a list.

pub mod fuzz;
pub mod reference_storage;
