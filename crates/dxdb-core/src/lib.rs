//! dxdb core: database, collection, document, query surface, hooks.
//!
//! The parity oracle is `dxdb-memory`, and it is the only storage this crate may
//! name in dev-dependencies (PLAN.md section 4.1). The rule exists so the core
//! cannot grow a dependency on a storage that would then be unavailable on one
//! of the WASM targets.
//!
//! Scaffolded in M0; M2 fills it in.

#![forbid(unsafe_code)]
