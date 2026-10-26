//! The field codec: encrypting and decrypting the paths a schema declares.
//!
//! Separate from the wrapper so that the same code seals document fields, seals
//! attachment payloads, and decrypts change-stream events — one implementation of
//! the AAD rules, in one place. A second call site that built AAD by hand is how
//! the transplant defenses would quietly diverge.

use dxdb_storage::DocumentData;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::config::{get_path, set_path, EncryptedPaths};
use crate::envelope::{open, seal, Aad, Envelope, EnvelopeError};
use crate::keys::Keys;
use crate::EncryptionError;

/// Seals and opens the encrypted paths of one collection.
#[derive(Debug, Clone)]
pub struct FieldCodec {
    /// The validated `encrypted` list.
    pub paths: EncryptedPaths,
    /// The collection's schema hash, bound into every AAD.
    pub schema_hash: [u8; 32],
    /// The primary key path, needed to read a document's id for the AAD.
    pub primary_path: String,
}

impl FieldCodec {
    /// The AAD for one field of one document at one revision and generation.
    pub fn aad(
        &self,
        document_id: &str,
        field_path: &str,
        revision: Option<&str>,
        generation: u8,
    ) -> Aad {
        Aad {
            document_id: document_id.to_string(),
            schema_hash: self.schema_hash,
            field_path: field_path.to_string(),
            revision: revision.map(str::to_string),
            generation,
        }
    }

    /// Seals every configured path of `document` in place.
    ///
    /// A path that is absent stays absent (upstream: `if (typeof value ===
    /// 'undefined') return;`). A path that is present but cannot be sealed is an
    /// error: writing it in plaintext would be a silent downgrade, which is the
    /// failure mode this whole crate exists to prevent.
    pub fn seal_document(
        &self,
        document: &mut DocumentData,
        keys: &Keys,
    ) -> Result<(), EncryptionError> {
        if self.paths.is_empty() {
            return Ok(());
        }
        let document_id = document
            .id(&self.primary_path)
            .map_err(|_| EncryptionError::MissingDocumentId)?
            .to_string();
        // The revision *being written* is, by construction, the `_rev` the stored
        // document carries. Authenticating it is what defeats a rollback to an
        // older ciphertext (INV-6).
        let revision = document.rev().map(str::to_string);
        let generation = keys.current_generation();
        let data_key = keys.current_data_key();
        for path in self.paths.as_slice() {
            let Some(value) = get_path(document.as_value(), path).cloned() else {
                continue;
            };
            // The plaintext is the field's JSON serialisation through the JS-typed
            // encoder, so an encrypted field round-trips with the same number and
            // string semantics as every other field (PLAN.md 3.1, INV-1).
            let plaintext = dxdb_json::to_js_json(&value);
            let nonce = keys.entropy().nonce();
            let aad = self.aad(&document_id, path, revision.as_deref(), generation);
            let key_bytes: &[u8; 32] = &data_key;
            let sealed = seal(key_bytes, generation, nonce, &aad, plaintext.as_bytes());
            if !set_path(
                document.as_object_mut(),
                path,
                Value::String(crate::b64::encode(&sealed)),
            ) {
                return Err(EncryptionError::MalformedPath(path.clone()));
            }
        }
        Ok(())
    }

    /// Opens every configured path of `document` in place.
    ///
    /// A path that is absent stays absent. A path that is present but not a valid
    /// envelope is an error, never a pass-through: returning stored ciphertext as
    /// if it were plaintext is INV-2's failure.
    pub fn open_document(
        &self,
        document: &mut DocumentData,
        keys: &Keys,
    ) -> Result<(), EncryptionError> {
        if self.paths.is_empty() {
            return Ok(());
        }
        let document_id = document
            .id(&self.primary_path)
            .map_err(|_| EncryptionError::MissingDocumentId)?
            .to_string();
        let revision = document.rev().map(str::to_string);
        for path in self.paths.as_slice() {
            let Some(raw) = get_path(document.as_value(), path) else {
                continue;
            };
            let Value::String(stored) = raw else {
                return Err(EncryptionError::Envelope(EnvelopeError::NotAnEnvelope));
            };
            let value = self.open_sealed(keys, &document_id, path, revision.as_deref(), stored)?;
            if !set_path(document.as_object_mut(), path, value) {
                return Err(EncryptionError::MalformedPath(path.clone()));
            }
        }
        Ok(())
    }

    /// Opens one stored envelope string, returning the field's JSON value.
    ///
    /// Shared by the document path and the attachment path, so both obey the same
    /// generation, AAD and canonical-decoding rules.
    pub fn open_sealed(
        &self,
        keys: &Keys,
        document_id: &str,
        field_path: &str,
        revision: Option<&str>,
        stored: &str,
    ) -> Result<Value, EncryptionError> {
        let bytes = crate::b64::decode(stored)
            .map_err(|_| EncryptionError::Envelope(EnvelopeError::NotAnEnvelope))?;
        let envelope = Envelope::parse(&bytes).map_err(EncryptionError::Envelope)?;
        let data_key = keys
            .data_key(envelope.generation)
            // The generation's key was retired while a ciphertext still carried it:
            // the data is deliberately unrecoverable (section 3.6).
            .ok_or(EncryptionError::UnavailableGeneration(envelope.generation))?;
        let aad = self.aad(document_id, field_path, revision, envelope.generation);
        let plaintext = open(&data_key, &aad, &bytes).map_err(EncryptionError::Envelope)?;
        let text = std::str::from_utf8(&plaintext)
            .map_err(|_| EncryptionError::Envelope(EnvelopeError::NotAnEnvelope))?;
        dxdb_json::parse(text).map_err(|_| EncryptionError::Envelope(EnvelopeError::NotAnEnvelope))
    }

    /// Seals arbitrary bytes under a field path, for attachments.
    ///
    /// Returns the base64 envelope. Attachment payloads are not JSON, so they do
    /// not go through [`Self::seal_document`]; they share the AAD construction,
    /// which is the part that must not diverge.
    pub fn seal_bytes(
        &self,
        keys: &Keys,
        document_id: &str,
        field_path: &str,
        revision: Option<&str>,
        payload: &[u8],
    ) -> String {
        let generation = keys.current_generation();
        let data_key = keys.current_data_key();
        let nonce = keys.entropy().nonce();
        let aad = self.aad(document_id, field_path, revision, generation);
        let key_bytes: &[u8; 32] = &data_key;
        let sealed = seal(key_bytes, generation, nonce, &aad, payload);
        crate::b64::encode(&sealed)
    }

    /// Opens bytes sealed by [`Self::seal_bytes`].
    pub fn open_bytes(
        &self,
        keys: &Keys,
        document_id: &str,
        field_path: &str,
        revision: Option<&str>,
        stored: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, EncryptionError> {
        let text = std::str::from_utf8(stored)
            .map_err(|_| EncryptionError::Envelope(EnvelopeError::NotAnEnvelope))?;
        let bytes = crate::b64::decode(text)
            .map_err(|_| EncryptionError::Envelope(EnvelopeError::NotAnEnvelope))?;
        let envelope = Envelope::parse(&bytes).map_err(EncryptionError::Envelope)?;
        let data_key = keys
            .data_key(envelope.generation)
            .ok_or(EncryptionError::UnavailableGeneration(envelope.generation))?;
        let aad = self.aad(document_id, field_path, revision, envelope.generation);
        open(&data_key, &aad, &bytes).map_err(EncryptionError::Envelope)
    }
}

/// The AAD field path used for an attachment.
///
/// Namespaced so an attachment ciphertext can never be transplanted into a
/// document field whose name happens to collide with an attachment id.
pub fn attachment_field_path(attachment_id: &str) -> String {
    format!("attachment:{attachment_id}")
}

#[cfg(test)]
mod tests {
    use super::{attachment_field_path, FieldCodec};
    use crate::config::EncryptedPaths;
    use crate::entropy::FixedEntropy;
    use crate::keys::{KdfParams, KeyMaterial, KeyRing, Keys};
    use dxdb_storage::DocumentData;
    use serde_json::json;
    use std::sync::Arc;

    fn keys() -> Keys {
        // Cheap KDF parameters: this is a codec test, not a KDF-cost test.
        let material = KeyMaterial {
            salt: [1u8; 16],
            params: KdfParams {
                m_cost_kib: 64,
                t_cost: 1,
                p_cost: 1,
            },
            verifier: [0u8; 32],
            current_generation: 1,
            wrapped_keys: Vec::new(),
        };
        // Build the ring directly so the verifier does not have to match.
        let master = material
            .params
            .derive_master_key(b"pw", &material.salt)
            .expect("derive");
        let ring = KeyRing::new(&master);
        Keys::from_parts(ring, material, [7u8; 32], Arc::new(FixedEntropy::new()))
    }

    fn codec(paths: &[&str]) -> FieldCodec {
        FieldCodec {
            paths: EncryptedPaths::new(paths.iter().map(|p| (*p).to_string())).expect("valid"),
            schema_hash: [7u8; 32],
            primary_path: "id".to_string(),
        }
    }

    fn document() -> DocumentData {
        DocumentData::from_value(json!({
            "id": "doc-1",
            "note": "the plaintext",
            "rev": "1-abc",
            "_rev": "1-abc"
        }))
        .expect("valid")
    }

    #[test]
    fn a_field_round_trips_through_seal_and_open() {
        let key_ring = keys();
        let codec = codec(&["note"]);
        let mut doc = document();
        codec.seal_document(&mut doc, &key_ring).expect("seal");
        assert_ne!(doc.get("note"), Some(&json!("the plaintext")), "sealed");
        codec.open_document(&mut doc, &key_ring).expect("open");
        assert_eq!(doc.get("note"), Some(&json!("the plaintext")));
    }

    #[test]
    fn an_absent_field_stays_absent() {
        let key_ring = keys();
        let codec = codec(&["note", "missing"]);
        let mut doc = document();
        codec.seal_document(&mut doc, &key_ring).expect("seal");
        codec.open_document(&mut doc, &key_ring).expect("open");
        assert_eq!(doc.get("missing"), None);
    }

    #[test]
    fn transplanting_between_documents_fails() {
        let key_ring = keys();
        let codec = codec(&["note"]);
        let mut a = document();
        codec.seal_document(&mut a, &key_ring).expect("seal");
        let sealed = a.get("note").cloned().expect("present");

        // Put A's ciphertext into B, same field, same revision, same schema.
        let mut b = DocumentData::from_value(json!({
            "id": "doc-2", "note": "b's own", "rev": "1-abc", "_rev": "1-abc"
        }))
        .expect("valid");
        b.set("note", sealed);
        let error = codec
            .open_document(&mut b, &key_ring)
            .expect_err("must be rejected");
        assert!(matches!(
            error,
            crate::EncryptionError::Envelope(crate::envelope::EnvelopeError::AuthenticationFailed)
        ));
    }

    #[test]
    fn transplanting_between_fields_fails() {
        let key_ring = keys();
        let codec = codec(&["note", "secret"]);
        let mut doc = DocumentData::from_value(json!({
            "id": "doc-1", "note": "n", "secret": "s", "_rev": "1-abc"
        }))
        .expect("valid");
        codec.seal_document(&mut doc, &key_ring).expect("seal");
        let note_ciphertext = doc.get("note").cloned().expect("present");
        // Move the note ciphertext into the secret field.
        doc.set("secret", note_ciphertext);
        let error = codec
            .open_document(&mut doc, &key_ring)
            .expect_err("must be rejected");
        assert!(matches!(
            error,
            crate::EncryptionError::Envelope(crate::envelope::EnvelopeError::AuthenticationFailed)
        ));
    }

    #[test]
    fn replaying_an_older_revision_fails() {
        let key_ring = keys();
        let codec = codec(&["note"]);
        // Revision 1's ciphertext.
        let mut v1 = document();
        codec.seal_document(&mut v1, &key_ring).expect("seal v1");
        let old_ciphertext = v1.get("note").cloned().expect("present");

        // Revision 2's document.
        let mut v2 = DocumentData::from_value(json!({
            "id": "doc-1", "note": "newer value", "_rev": "2-def"
        }))
        .expect("valid");
        codec.seal_document(&mut v2, &key_ring).expect("seal v2");

        // Replay revision 1's ciphertext into revision 2's document.
        v2.set("note", old_ciphertext);
        let error = codec
            .open_document(&mut v2, &key_ring)
            .expect_err("must be rejected");
        assert!(matches!(
            error,
            crate::EncryptionError::Envelope(crate::envelope::EnvelopeError::AuthenticationFailed)
        ));
    }

    #[test]
    fn attachment_paths_are_namespaced() {
        assert_eq!(attachment_field_path("att-1"), "attachment:att-1");
        // A document field with the same name would produce a different path,
        // because the prefix is added rather than reused.
        assert_ne!(attachment_field_path("x"), "x");
    }

    #[test]
    fn attachment_bytes_round_trip_and_bind_their_id() {
        let key_ring = keys();
        let codec = codec(&[]);
        let path = attachment_field_path("att-1");
        let sealed = codec.seal_bytes(&key_ring, "doc-1", &path, Some("1-a"), b"payload");
        let opened = codec
            .open_bytes(&key_ring, "doc-1", &path, Some("1-a"), sealed.as_bytes())
            .expect("opens");
        assert_eq!(&opened[..], b"payload");
        // Same bytes under a different attachment id fails.
        let other = attachment_field_path("att-2");
        assert!(codec
            .open_bytes(&key_ring, "doc-1", &other, Some("1-a"), sealed.as_bytes())
            .is_err());
    }
}
