//! dxdb wasm-bindgen surface: the JS boundary.
//!
//! This crate is the *only* place in the workspace that knows about
//! `wasm-bindgen`, JS promises, or `Observable` (PLAN.md 5.1). The core crates
//! know nothing about JS, and they must keep knowing nothing, so they stay
//! buildable for every target in the capability matrix (PLAN.md 3.4).
//!
//! # Status: M4 tranche 1, size baseline only
//!
//! What is here today is deliberately the smallest thing that is a real
//! `wasm-bindgen` module: it exists to pin the toolchain pairing and to put a
//! measured floor under the 250 KiB brotli core budget (PLAN.md 5.3). The
//! exported surface, the `Observable` adapter and the storage bindings arrive
//! once `dxdb-storage` publishes the traits — see
//! `docs/lanes/m4-browser-plan.md` for the contract they will satisfy.
//!
//! `no_std`-style dependency discipline still applies from day one: nothing may
//! be added here that breaks `wasm32-unknown-unknown` on stable 1.95.0.

#![forbid(unsafe_code)]

use wasm_bindgen::prelude::*;

/// The `wasm-bindgen` ABI version this module was built against.
///
/// Exported so the demo harness can assert that the `.wasm` and the JS glue it
/// loaded are the same build, which is the failure mode that otherwise shows up
/// as a trap at instantiation time with a confusing message.
#[wasm_bindgen]
pub fn abi_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// A round-trip through the boundary, used as the smoke test that the module
/// instantiated and that arguments/returns cross correctly.
#[wasm_bindgen]
pub fn echo(input: &str) -> String {
    input.to_string()
}

/// Resolve a JS `Promise` from Rust, which is the mechanism every async storage
/// operation will use. Kept minimal: proves `wasm-bindgen-futures` links and
/// that the executor is available in the target environment.
#[wasm_bindgen]
pub async fn resolve_after_tick(value: u32) -> Result<u32, JsValue> {
    // `wasm-bindgen-futures` polls this on the JS microtask queue.
    YieldOnce::new().await;
    Ok(value.wrapping_add(1))
}

/// A one-shot future that completes on the next microtask.
struct YieldOnce {
    yielded: bool,
}

impl YieldOnce {
    fn new() -> Self {
        Self { yielded: false }
    }
}

impl std::future::Future for YieldOnce {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.yielded {
            std::task::Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_round_trips() {
        assert_eq!(echo("dxdb"), "dxdb");
    }

    #[test]
    fn abi_version_is_the_package_version() {
        assert_eq!(abi_version(), env!("CARGO_PKG_VERSION"));
    }
}
