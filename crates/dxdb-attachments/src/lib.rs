//! The attachment data model: stubs, digest, and blob access.
//!
//! Ported from upstream `src/plugins/attachments/` (397 lines) and
//! `src/types/rx-attachment.d.ts` (46 lines) at pin `3b763ee17`. The concrete
//! porting plan is `docs/lanes/m3-port-plan.md`.
//!
//! # The two shapes, and why they are distinct
//!
//! Upstream keeps attachment metadata in the document and payloads out of it:
//!
//! - `RxAttachmentData` — the stub stored in a document's `_attachments`:
//!   `{ length, digest, type }`. It is what a decoder sees, what a peer
//!   receives, and what is compared during conflict resolution.
//! - `RxAttachmentWriteData` — the same stub plus the payload, used only on the
//!   write path. Callers hand this to `bulk_write`, and a storage may accept it
//!   in place of a stub.
//!
//! `length` is the payload size in bytes and `digest` is the content hash the
//! caller's hash function produced (SHA-256 in the default configuration). The
//! only guarantee upstream makes is that the digest changes when the payload
//! changes, so it is an identity token, not a format to parse.
//!
//! The invariant that matters for interop: a payload never appears inside a
//! document's `_attachments` on the wire. Readers resolve one with
//! `get_attachment_data(doc_id, attachment_id, digest)`, addressing it by digest
//! so a mismatch is a genuine error rather than a silently stale read.
//!
//! # Serialised attachments are a transport concern
//!
//! Some transports cannot carry binary payloads and serialise them into a
//! sibling `_attachments_data` field (base64), stripping the payload out of
//! `_attachments`. That is a transport shim; it must not leak into the
//! replication protocol, which only ever exchanges stubs.
//!
//! # Status
//!
//! Scaffolded in M3 tranche 1, before `dxdb-storage` published its traits. The
//! digest and blob types need the storage layer's payload type, so this crate
//! deliberately holds no code yet.

#![forbid(unsafe_code)]
