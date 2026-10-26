//! The replication protocol: checkpoint-stack pull/push, meta instance, conflicts.
//!
//! Ported from upstream RxDB `src/replication-protocol/*` (2,121 lines at pin
//! `3b763ee17`). The concrete porting plan — data model, flows, byte-identical
//! obligations, state machine — is `docs/lanes/m3-port-plan.md`.
//!
//! # Shape of the protocol
//!
//! The fork (local storage) pulls from and pushes to a master reached only
//! through a transport trait. Three artefacts define interop with every other
//! RxDB peer, so they are reproduced exactly rather than approximated:
//!
//! - **Checkpoints.** A checkpoint is opaque to the protocol except for three
//!   operations: stacking (`stackCheckpoints` upstream: `Object.assign` over a
//!   list, later wins, first-insertion key order), string comparison with
//!   JavaScript `JSON.stringify` equality, and persistence. Bytes therefore
//!   matter and must come from `dxdb_json::to_js_json`, not `serde_json`.
//! - **Document states.** What crosses the wire is `WithDeletedAndAttachments`:
//!   JS number formatting, preserved key order, attachment stubs only.
//! - **Revisions.** `{height}-{token}`, height 1 for a new document and
//!   `previous + 1` after. The token is local; the height arithmetic is not.
//!
//! # Direction naming
//!
//! `downstream` is pull (master to fork) and `upstream` is push (fork to
//! master). The persisted checkpoint `itemId` is the literal `"up"` or `"down"`.
//! Conflicts are resolved only in the upstream; the downstream never asks a
//! conflict handler to decide anything.
//!
//! # Constraints
//!
//! - No concrete storage dependency: traits come from `dxdb-storage` only.
//! - No `tokio`. Futures are driven by an executor the caller supplies.
//! - Must build for `wasm32-unknown-unknown`: the HTTP transport belongs to the
//!   caller, behind a trait, and time is injected rather than read from a
//!   native clock.
//! - No `unsafe`.
//!
//! # Status
//!
//! Scaffolded in M3 tranche 1, before `dxdb-storage` published its traits. The
//! planned module split is recorded in the port plan (section 9.1); this crate
//! deliberately holds no protocol code until those traits are real.

#![forbid(unsafe_code)]

pub mod adapters;
pub mod attachments;
pub mod checkpoint;
pub mod conflict;
pub mod conformance;
pub mod convert;
pub mod downstream;
pub mod master;
pub mod meta_instance;
pub mod state;
pub mod upstream;

use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use dxdb_storage::{ChangeStream, EventBulk};

/// Reads everything currently queued on a change stream without blocking.
///
/// The protocol is pumped by the caller rather than by a runtime (D2/D3), so "did anything
/// arrive" has to be a question that returns instead of awaiting. Polling with a no-op
/// waker leaves a real wake registered for a real executor and returns `Pending` for
/// "nothing yet", which is exactly what a drain wants.
pub fn drain_change_stream(stream: &mut ChangeStream) -> Vec<EventBulk> {
    let mut out = Vec::new();
    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    loop {
        match stream.as_mut().poll_next(&mut context) {
            Poll::Ready(Some(bulk)) => out.push(bulk),
            Poll::Ready(None) | Poll::Pending => return out,
        }
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}
