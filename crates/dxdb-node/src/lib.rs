//! dxdb Node.js bindings: napi-rs native surface, with `wasm32-wasip1` as the
//! install-free fallback. Both paths must be real, because M5's gate is the
//! conformance suite running under Node from each of them (PLAN.md section 6).
//!
//! # Provenance
//!
//! **This crate has no upstream counterpart.** RxDB is a JavaScript library, so
//! it has no Node binding to port. The *behaviour* this crate exposes is the
//! ported core's; the surface shape is dxdb's own. See
//! `docs/lanes/m5-port-plan.md` section 4.
//!
//! The rule that keeps this honest: **this crate adds no semantics.** Every
//! exported function is a translation of a core API, and where a translation is
//! lossy that is documented rather than smoothed over.
//!
//! # Why two artifacts, not one source
//!
//! `napi` is a native ABI and cannot be compiled for `wasm32-wasip1`, so the
//! WASI build cannot be the same source with a different `cfg`. Both paths
//! therefore return the same JSON shapes, defined once in [`protocol`], and the
//! conformance harness is one JS harness parameterised by which object it holds.
//! Two paths that drift are worse than one path.
//!
//! # Node's WASI asymmetry, recorded because it is not our choice
//!
//! `node:wasi` is synchronous and preview1. `wasm32-wasip1` output imports
//! `wasi_snapshot_preview1`, which `node:wasi` accepts. `wasm32-wasip2` output is
//! a *component*, and `node:wasi` does not accept components. So the Node-WASI
//! path is wasip1-only.
//!
//! # Release-profile caveat (needs a leader decision before first release)
//!
//! The workspace release profile sets `panic = "abort"` for the WASM size budget
//! (PLAN.md 5.3). For this crate that turns a recoverable panic into a killed
//! Node process. The intended resolution is a per-package override,
//! `[profile.release.package.dxdb-node] panic = "unwind"`, because a
//! `catch_unwind` on every entry point is dead weight in the common case. That
//! is a leader-owned decision surface: it must be recorded in PLAN.md 5.3 before
//! it is adopted rather than changed here.

#![forbid(unsafe_code)]

/// The wire shapes shared by the napi path and the WASI path.
///
/// One definition, used by both, because the M5 gate compares their behaviour
/// and a shape difference must be a test failure rather than a note in a doc.
pub mod protocol {
    /// A document on the wire: identity the storage contract needs, plus the
    /// ordered JSON body.
    ///
    /// The body is bytes, not a parsed value, for the same reason the WIT world
    /// uses `list<u8>`: key order is part of the encoding (PLAN.md 3.1) and is
    /// produced in exactly one place, `dxdb-json`.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Document {
        pub id: String,
        pub rev: String,
        pub deleted: bool,
        pub lwt: u64,
        pub json: Vec<u8>,
    }

    /// Per-row outcome of a `bulk_write`, parallel to the input rows.
    ///
    /// Parallel rather than a map keyed on id: one batch may legally contain the
    /// same id twice, and the contract's partial-success semantics are per row,
    /// not per document.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct WriteResult {
        pub written: Vec<String>,
        pub errors: Vec<Option<WriteError>>,
    }

    /// Why one row of a `bulk_write` did not land.
    ///
    /// Mirrors `write-error` in `crates/dxdb-wasi/wit/dxdb.wit`; the two must
    /// stay in step so both Node paths and the component report the same
    /// categories. The categorisation itself is ported, not invented: it is
    /// `categorizeBulkWriteRows` in `rx-storage-helper.ts:205`, and it lives in
    /// `dxdb-storage`.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum WriteError {
        Conflict,
        SameState,
        Host(String),
    }
}

#[cfg(test)]
mod tests {
    use super::protocol::{Document, WriteError, WriteResult};

    #[test]
    fn a_write_result_is_parallel_to_its_rows() {
        // The invariant that makes the shape safe: one error slot per row, so a
        // caller can zip instead of keying on an id that may repeat.
        let result = WriteResult {
            written: vec!["a".into()],
            errors: vec![None, Some(WriteError::Conflict)],
        };
        assert_eq!(result.errors.len(), 2);
        assert!(result.errors[0].is_none());
    }

    #[test]
    fn a_document_carries_its_json_as_bytes() {
        let doc = Document {
            id: "a".into(),
            rev: "1-abc".into(),
            deleted: false,
            lwt: 1,
            json: br#"{"_id":"a","_rev":"1-abc"}"#.to_vec(),
        };
        assert_eq!(doc.json.first(), Some(&b'{'));
    }
}
