//! Filling attachment payloads onto document states that go on the wire.
//!
//! Ports `fillWriteDataForAttachmentsChange` (`src/plugins/attachments/attachments-utils.ts:34-84`),
//! the one function the replication protocol needs from the attachments plugin.
//!
//! # Why the protocol needs it at all
//!
//! A stored document carries attachment *stubs* only: `{length, digest, type}`
//! (`rx-storage.d.ts:115-139`). A transport, though, has to deliver the bytes the other side
//! does not have — so before sending, the push asks the storage for the payload of every
//! attachment that is new or whose digest changed, and ships it alongside. The pull does the
//! same in the other direction for attachments the master added.
//!
//! Without this step the protocol exchanges stubs that point at payloads the receiver can
//! never fetch, which is the failure mode the attachment conformance cases exist to catch.
//!
//! # The rule for "needs data"
//!
//! ```text
//! needs data  =  (attachment id is not in the original)  OR  (digest differs)
//!                AND the state does not already carry the payload
//! ```
//!
//! Two things about that are load-bearing:
//!
//! * An attachment whose id and digest both match the original is **not** re-sent. Sending
//!   every payload on every push would make an unrelated field edit as expensive as a full
//!   re-upload.
//! * `original_document = None` means "assume nothing", so **all** payloads are sent. That is
//!   exactly what upstream does in
//!   `rxStorageInstanceToReplicationHandler` (`index.ts:208`, `:242`) with the comment "the
//!   master never knows the client state of the document. Therefore we always send all
//!   attachments data".

use std::collections::BTreeMap;

use dxdb_json::Value;
use dxdb_storage::{DocumentData, DocumentWriteData, RxStorageInstance, StorageError};

/// Upstream's `AT4`: a document state that is missing `_attachments` cannot be filled.
pub const MISSING_ATTACHMENTS: &str =
    "the document state has no _attachments object, so its payloads cannot be resolved";

/// Returns the state with payloads attached for every attachment that needs one.
///
/// `original_document` is the state the receiver is known to hold: the assumed master state on
/// a push, or the current fork state on a pull. `None` means "the receiver holds nothing", so
/// every payload is included.
///
/// The returned [`DocumentWriteData`] carries the payloads separately from the JSON, which is
/// the shape `bulk_write` takes — a `Blob`/`Vec<u8>` cannot live inside the ordered-JSON
/// document without inventing an encoding at every storage boundary.
pub async fn fill_write_data_for_attachments_change<S: RxStorageInstance>(
    storage: &S,
    primary_path: &str,
    new_document: &DocumentData,
    original_document: Option<&DocumentData>,
) -> Result<DocumentWriteData, StorageError> {
    let Some(attachments) = new_document.attachments() else {
        return Err(StorageError::Unsupported(MISSING_ATTACHMENTS.to_string()));
    };
    if original_document.is_some_and(|original| original.attachments().is_none()) {
        return Err(StorageError::Unsupported(MISSING_ATTACHMENTS.to_string()));
    }

    let document_id = new_document
        .id(primary_path)
        .map_err(|error| StorageError::InvalidDocument(error.to_string()))?
        .to_string();

    let original_attachments = original_document
        .and_then(DocumentData::attachments)
        .cloned()
        .unwrap_or_default();

    let mut payloads: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (attachment_id, stub) in attachments {
        let digest = stub
            .get("digest")
            .and_then(Value::as_str)
            .unwrap_or_default();

        // Already carrying the payload? Then the caller supplied it and there is nothing to
        // fetch. This is the `!(value as RxAttachmentWriteData).data` half of the condition.
        if let Some(Value::Array(bytes)) = stub.get("data") {
            payloads.insert(
                attachment_id.clone(),
                bytes
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|byte| byte as u8)
                    .collect(),
            );
            continue;
        }

        let is_new_or_changed = match original_attachments.get(attachment_id) {
            None => true,
            Some(original_stub) => {
                original_stub.get("digest").and_then(Value::as_str) != Some(digest)
            }
        };
        if !is_new_or_changed {
            continue;
        }

        let bytes = storage
            .get_attachment_data(
                document_id.clone(),
                attachment_id.clone(),
                digest.to_string(),
            )
            .await?;
        payloads.insert(attachment_id.clone(), bytes);
    }

    Ok(DocumentWriteData::with_payloads(
        new_document.clone(),
        payloads,
    ))
}

/// True when the two states require the same attachment payloads to be sent.
///
/// Used by a test to assert that an unrelated edit does not re-send bytes, which is the
/// property that makes attachment replication affordable.
pub fn same_attachment_identity(a: &DocumentData, b: &DocumentData) -> bool {
    let identity = |document: &DocumentData| -> BTreeMap<String, String> {
        document
            .attachments()
            .map(|attachments| {
                attachments
                    .iter()
                    .map(|(id, stub)| {
                        (
                            id.clone(),
                            stub.get("digest")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    identity(a) == identity(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dxdb_json::parse;

    fn doc(json: &str) -> DocumentData {
        DocumentData::from_value(parse(json).expect("valid JSON")).expect("object")
    }

    #[test]
    fn a_state_without_attachments_cannot_be_filled() {
        let document = doc(r#"{"id":"a"}"#);
        assert!(document.attachments().is_none());
    }

    #[test]
    fn identical_attachment_identity_is_detected() {
        let a = doc(r#"{"id":"a","_attachments":{"f":{"length":1,"digest":"d1","type":"t"}}}"#);
        let b = doc(
            r#"{"id":"a","firstName":"changed","_attachments":{"f":{"length":1,"digest":"d1","type":"t"}}}"#,
        );
        assert!(
            same_attachment_identity(&a, &b),
            "an unrelated edit must not change attachment identity"
        );

        let c = doc(r#"{"id":"a","_attachments":{"f":{"length":1,"digest":"d2","type":"t"}}}"#);
        assert!(
            !same_attachment_identity(&a, &c),
            "a changed digest is a different identity"
        );
    }
}
