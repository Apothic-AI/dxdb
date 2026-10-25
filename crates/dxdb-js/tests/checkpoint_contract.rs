//! The checkpoint contract, checked against the storage trait surface as
//! published.
//!
//! This module exists because the lane brief made the checkpoint encoding a
//! thing to verify rather than assume: *"the checkpoint type's encoding (it must
//! round-trip through ordered JSON with JavaScript number formatting)"*.
//!
//! [`dxdb_storage::Checkpoint::to_json`] documents exactly that requirement —
//! *"this must round-trip through `JSON.stringify` and back without drift — hence
//! `dxdb-jsnum`"* — but its body builds a [`serde_json::Number`] with
//! `Number::from_f64` and relies on whoever serialises the value to format
//! numbers correctly. `serde_json`'s own writer does not: it emits `1.0` where
//! JavaScript emits `1`.
//!
//! So the *type* is fine and the *untyped path* is a trap. These tests pin both
//! halves: the correct path works, and the naive path is demonstrably wrong, so
//! the trap stays visible instead of being rediscovered by whoever wires the
//! replication checkpoint to a `JSON.stringify`-shaped emitter.

#![cfg(test)]

use dxdb_json::to_js_json;
use dxdb_storage::{Checkpoint, META_LWT_MINIMUM};

/// The lwt values a real clock produces: `ms + k * 0.01`.
///
/// Chosen to include an integral-looking value (`1.0`, the minimum), a two-decimal
/// fraction, and values that a lossy formatter would mangle.
const CLOCK_VALUES: &[f64] = &[
    1700000000000.07,
    1700000000000.01,
    1700000000000.29,
    1700000000000.99,
    1234567890123.07,
    META_LWT_MINIMUM,
];

/// The path a replication checkpoint must take: format through `dxdb-jsnum`, the
/// crate that reproduces `JSON.stringify`.
#[test]
fn checkpoint_json_uses_javascript_number_formatting() {
    for &lwt in CLOCK_VALUES {
        let checkpoint = Checkpoint::new("doc-1", lwt);
        let rendered = to_js_json(&checkpoint.to_json());
        let expected = format!(
            "{{\"id\":\"doc-1\",\"lwt\":{}}}",
            dxdb_jsnum::to_json_string(lwt)
        );
        assert_eq!(rendered, expected, "lwt {lwt} did not render as JS would");
    }
}

/// Every rendered checkpoint must parse back to the same pair. This is the
/// property replication depends on, and it is why the formatting cannot be
/// "close enough".
#[test]
fn checkpoint_roundtrips_through_json() {
    for &lwt in CLOCK_VALUES {
        let original = Checkpoint::new("doc-1", lwt);
        let rendered = to_js_json(&original.to_json());
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        let back = Checkpoint::from_json(&parsed).expect("parses as a checkpoint");
        assert_eq!(
            back, original,
            "round trip changed the checkpoint for lwt {lwt}"
        );
    }
}

/// The minimum lwt is `1.0`, and `JSON.stringify(1.0)` is `1`, not `1.0`.
///
/// This is the single value that makes the naive path visibly wrong, and it is
/// reachable: it is the checkpoint of a document written before any clock read.
#[test]
fn minimum_lwt_renders_without_a_trailing_zero() {
    let checkpoint = Checkpoint::new("", META_LWT_MINIMUM);
    let rendered = to_js_json(&checkpoint.to_json());
    assert_eq!(rendered, r#"{"id":"","lwt":1}"#);
    assert!(
        !rendered.contains("1.0"),
        "rendered `{rendered}`, which JSON.stringify would never produce"
    );
}

/// The trap, asserted as a fact rather than left as a comment.
///
/// `serde_json::Value::to_string()` — the obvious thing to reach for — formats
/// floats with Rust's rules. Any code path that serialises a checkpoint with it
/// (or with `serde_json::to_string`) produces bytes that differ from what the
/// same checkpoint produces in JavaScript, and a checkpoint is compared and
/// persisted, so the drift is a correctness bug and not cosmetics.
///
/// If a future `serde_json` changes this, the test fails and the note in
/// `docs/lanes/m4-browser-plan.md` gets updated — which is the point.
#[test]
fn serde_json_alone_diverges_from_javascript() {
    let checkpoint = Checkpoint::new("", META_LWT_MINIMUM);
    let naive = checkpoint.to_json().to_string();
    let correct = to_js_json(&checkpoint.to_json());
    assert_ne!(
        naive, correct,
        "serde_json now matches JavaScript for integer-valued floats; \
         the jsonfmt warning in the plan can be removed"
    );
    assert_eq!(naive, r#"{"id":"","lwt":1.0}"#);
    assert_eq!(correct, r#"{"id":"","lwt":1}"#);
}

/// `Checkpoint::beginning()` is what `get_changed_documents_since` uses when the
/// caller has no checkpoint, so its lwt must be the minimum and its id empty.
#[test]
fn beginning_checkpoint_is_the_lowest_possible() {
    let beginning = Checkpoint::beginning();
    assert_eq!(beginning.id, "");
    assert_eq!(beginning.lwt, META_LWT_MINIMUM);
    // Lower than any clock value, which is what makes the first
    // `get_changed_documents_since(None)` return everything.
    for &lwt in CLOCK_VALUES {
        assert!(beginning.lwt <= lwt);
    }
}

/// An absent `lwt` is not a checkpoint; `from_json` must reject rather than
/// default to something that would silently resume from the wrong place.
#[test]
fn malformed_checkpoints_are_rejected() {
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"id": "a"}),
        serde_json::json!({"lwt": 5.0}),
        serde_json::json!({"id": 1, "lwt": 5.0}),
        serde_json::json!({"id": "a", "lwt": "5"}),
        serde_json::json!(null),
        serde_json::json!("a"),
    ] {
        assert!(
            Checkpoint::from_json(&bad).is_none(),
            "accepted a malformed checkpoint: {bad}"
        );
    }
}
