//! The storage trait layer: the spine of the system (PLAN.md section 4.2).
//!
//! Every wrapper feature — compression, encryption, sharding, worker, memory
//! tier, logging — is a type that implements these traits over an inner storage,
//! which is also why the clean-room crates can plug in without touching core.
//!
//! Scaffolded in M0; the traits land in M2.

#![forbid(unsafe_code)]
