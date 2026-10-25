//! Contract checks against the published `dxdb-storage` surface.
//!
//! These are not storage behaviour tests (those live in `src/lib.rs`). They check
//! assumptions a *dependent* makes, cheaply, and report a mismatch as a failing
//! test rather than as a comment.

use dxdb_storage::Checkpoint;

/// Formats a value the way `JSON.stringify` does, via `dxdb-json`.
fn js_json(value: &serde_json::Value) -> String {
    dxdb_json::to_js_json(value)
}

/// `Checkpoint`'s doc comment promises "`{ id, lwt }` as JSON, with JavaScript
/// number formatting for `lwt`", and that it round-trips through `JSON.stringify`
/// without drift.
///
/// It does not do that yet. `to_json` builds a `serde_json::Number` and
/// `from_json` reads with `as_f64()`, so `lwt` survives Rust-side but is
/// formatted by `serde_json`'s float writer, which emits `1700000000123.0` where
/// JavaScript emits `1700000000123`. Those two methods do not reference
/// `dxdb_jsnum`.
///
/// This test pins the behaviour that *exists* so the gap cannot widen silently,
/// and it fails the moment the encoding is fixed — at which point it should be
/// replaced by the byte round-trip assertion in the next test. Deliberately a
/// tripwire, not an endorsement.
#[test]
fn checkpoint_json_is_not_yet_js_formatted_this_pins_the_gap() {
    let cp = Checkpoint::new("doc-a", 1_700_000_000_123.0);
    let serialised = serde_json::to_string(&cp.to_json()).expect("serialise");

    // What JavaScript writes for the same value.
    let js = js_json(&cp.to_json());
    assert_eq!(
        js, r#"{"id":"doc-a","lwt":1700000000123}"#,
        "the JS-formatted form is what a cross-language peer expects"
    );

    assert_ne!(
        serialised, js,
        "if these now match, the Checkpoint encoding was fixed: replace this \
         test with the round-trip assertion below and update the port plan"
    );
    assert!(
        serialised.contains("1700000000123.0"),
        "expected serde_json's float formatting, got {serialised}"
    );

    // Round-trip through Rust is still exact: this is a formatting gap, not data
    // loss within dxdb.
    let back = Checkpoint::from_json(&cp.to_json()).expect("parse");
    assert_eq!(back.id, cp.id);
    assert_eq!(back.lwt, cp.lwt);
}

/// The property a cross-process consumer actually needs: a checkpoint written as
/// bytes and read back is the same checkpoint, with JavaScript number formatting.
///
/// This passes today because it goes through the JS-formatted writer rather than
/// through `Checkpoint::to_json`. It is the shape the storage layer should use, and
/// asserting it puts the requirement on record.
#[test]
fn a_checkpoint_round_trips_through_js_formatted_bytes() {
    for lwt in [0.0, 1.0, 1_700_000_000_123.0, 17.0] {
        let cp = Checkpoint::new("id-1", lwt);
        let bytes = js_json(&cp.to_json());
        let parsed: serde_json::Value = serde_json::from_str(&bytes).expect("parse bytes");
        let back = Checkpoint::from_json(&parsed).expect("from_json");
        assert_eq!(back.id, cp.id);
        assert_eq!(
            back.lwt, cp.lwt,
            "lwt must survive byte round-trip: {bytes}"
        );
    }
}
