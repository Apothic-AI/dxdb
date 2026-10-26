//! Reactivity: decide what a live query must do when a document changes.
//!
//! | Module | Upstream | Contents |
//! | --- | --- | --- |
//! | [`event_reduce`] | `src/event-reduce.ts` + `event-reduce-js` 7.4.0 | The 18 states, the recorded decision table, and the verdict |
//! | [`apply`] | `event-reduce-js` `src/actions/` | The 14 result-array transformations |
//! | [`change_event_buffer`] | `src/change-event-buffer.ts` | The bounded event window and its pointers |
//! | [`query_cache`] | `src/query-cache.ts`, `src/rx-query.ts:454` | One live query per distinct normalised query |
//! | `rx_change_event` | `src/rx-change-event.ts` | See the note below |
//!
//! # The asymmetry that governs every decision here
//!
//! Treating a sufficient event as insufficient costs one query. Treating an insufficient
//! event as sufficient makes a live query **silently stale**. So every uncertain case in this
//! crate resolves towards `RunFullQueryAgain`, and the decision table is the complete
//! 2^18-row surface of the real library rather than a sample.

//! # `rx-change-event.ts`: no new type
//!
//! Upstream defines `RxStorageChangeEvent` there and converts it to event-reduce-js's
//! `ChangeEvent` shape, which differs in one way: event-reduce-js uses `doc`/`previous` where
//! `ChangeEvent` uses `documentData`/`previousDocumentData`, and a delete has `doc: null`.
//!
//! `dxdb-storage::ChangeEvent` already carries the same four facts, so a parallel type would be
//! two representations of one thing. The conversion is [`apply::apply_action`]'s
//! `inserted_document`, which returns `None` for a delete — i.e. the `doc: null` case — so the
//! adapter is one function rather than a module, and there is nothing for this crate to add.

#![forbid(unsafe_code)]

pub mod apply;
pub mod change_event_buffer;
pub mod event_reduce;
pub mod query_cache;

pub use apply::apply_action;
pub use change_event_buffer::{BufferClosed, ChangeEventBuffer, OutOfBounds, DEFAULT_LIMIT};
pub use event_reduce::{
    action_for, reduce, resolve_states, Action, EventReduceResult, ReduceParams, States,
};
pub use query_cache::{
    cache_key, QueryCache, QueryOp, DEFAULT_TRY_TO_KEEP_MAX, DEFAULT_UNEXECUTED_LIFETIME,
};
