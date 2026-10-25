//! The `Observable` adapter: `futures::Stream` in Rust, `Observable` in JS, with
//! no RxJS anywhere (PLAN.md 3.3).
//!
//! Two modules, deliberately separated:
//!
//! - [`state`] is the delivery *policy* — pure Rust, no `wasm-bindgen`, tested on
//!   the host. It owns the rules the lane brief cares about: exactly-once error,
//!   no completion after error, no delivery after unsubscribe (including a value
//!   already queued), and unsubscribe-inside-a-callback.
//! - `bind` is the *shell* — the `#[wasm_bindgen]` objects JS touches, compiled
//!   only for `wasm32`. It performs the calls the policy authorises and nothing
//!   else.
//!
//! The split is the whole design. wasm-bindgen's known gap is the callback-table
//! leak: a `Closure` moved into a `JsValue` that JS retains is never freed, and
//! the workspace has no `any` to hide it behind. Keeping the ordering discipline
//! out of the binding means the discipline is testable exhaustively, and the
//! browser tests only have to prove the binding faithfully calls it.

pub mod state;

#[cfg(target_arch = "wasm32")]
mod bind;

#[cfg(target_arch = "wasm32")]
pub use bind::{from_stream, Emitter, Observable, StreamItem, Subscription};
