//! Test-only support code.
//!
//! Lives in `tests/` rather than `src/` because none of it belongs in a published
//! library: `reference_storage` is a scaffold whose only job is to keep the gate targets
//! from passing vacuously before lane D's `dxdb-memory` lands.

pub mod reference_storage;
