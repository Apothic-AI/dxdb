//! Reactivity: decide what a live query must do when a document changes.
//!
//! | Module | Upstream | Contents |
//! | --- | --- | --- |
//! | [`event_reduce`] | `src/event-reduce.ts` + `event-reduce-js` 7.4.0 | The 18 states, the recorded decision table, and the verdict |
//! | [`apply`] | `event-reduce-js` `src/actions/` | The 14 result-array transformations |
//!
//! # The asymmetry that governs every decision here
//!
//! Treating a sufficient event as insufficient costs one query. Treating an insufficient
//! event as sufficient makes a live query **silently stale**. So every uncertain case in this
//! crate resolves towards `RunFullQueryAgain`, and the decision table is the complete
//! 2^18-row surface of the real library rather than a sample.

#![forbid(unsafe_code)]

pub mod apply;
pub mod event_reduce;

pub use apply::apply_action;
pub use event_reduce::{
    action_for, reduce, resolve_states, Action, EventReduceResult, ReduceParams, States,
};
