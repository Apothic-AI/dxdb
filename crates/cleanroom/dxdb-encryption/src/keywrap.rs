//! AES-256-GCM: the **key generation** credential, not the data cipher.
//!
//! `specs/encryption.md` section 3.6 requires that a key generation can be
//! retired, which means a stored ciphertext must be undecryptable after its
//! generation's salt is dropped. The salt alone cannot do that: an attacker who
//! copied the database keeps a copy of the salt, and the password has not
//! changed, so they could re-derive the generation key.
//!
//! The gap is closed by requiring a second input that an attacker does **not**
//! have. When a database is created, the deployment supplies a random 16-byte
//! **device secret** which lives wherever it wants it to (keychain, OS keystore,
//! an env var, a file the user protects) — explicitly outside the database.
//! Generation 1's key-encryption key is the device secret. Each rotation
//! generates a fresh 32-byte key and stores it *wrapped* under the previous
//! generation's key. Retiring generation `n` deletes the wrapped blobs, and every
//! later generation becomes undecryptable even for someone holding the password
//! and the whole database, because they are missing the one input that was never
//! in it.
//!
//! This is a documented limitation as much as a feature: **lose the device secret
//! and the generations after the first rotation are unrecoverable**, exactly like
//! a lost password. It is the only construction that makes retirement mean
//! anything, and section 3.6's guarantee was written with it in mind.
//!
//! AES-GCM is correct here and wrong for the data path: there is exactly one
//! encryption per generation under a key that is never reused, so the
//! nonce-reuse hazard that makes AES-GCM a poor fit for per-field encryption
//! (section 3.2) cannot arise. Using a *different* primitive here also means a
//! bug in one path cannot silently degrade the other.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use zeroize::Zeroizing;

/// HChaCha-style domain separation is unnecessary here because the key is never
/// reused, but the generation number still goes in the AAD so a wrapped blob
/// cannot be replayed at a different generation if two were swapped.
fn wrap_aad(generation: u8) -> [u8; 16] {
    let mut aad = [0u8; 16];
    aad[..12].copy_from_slice(b"dxdb.enc.kw.");
    aad[15] = generation;
    aad
}

/// Wraps `key` under `kek`, binding the generation number.
pub fn wrap_key(kek: &[u8; 32], key: &[u8; 32], generation: u8) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(kek).expect("32-byte key");
    // All-zero nonce is safe because this key encrypts exactly one message for the
    // lifetime of a generation. Recorded here so a future reviewer does not read it
    // as an oversight.
    let nonce = [0u8; 12];
    let aad = wrap_aad(generation);
    cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: key.as_slice(),
                aad: &aad,
            },
        )
        .expect("AES-256-GCM encryption of a 32-byte key cannot fail")
}

/// Unwraps a key wrapped at `generation`. `None` means wrong kek, wrong
/// generation, or tampering — deliberately indistinguishable, because telling the
/// caller which of the three it was is an oracle.
pub fn unwrap_key(kek: &[u8; 32], wrapped: &[u8], generation: u8) -> Option<Zeroizing<[u8; 32]>> {
    let cipher = Aes256Gcm::new_from_slice(kek).ok()?;
    let nonce = [0u8; 12];
    let aad = wrap_aad(generation);
    let plaintext = cipher
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: wrapped,
                aad: &aad,
            },
        )
        .ok()?;
    let key: [u8; 32] = plaintext.as_slice().try_into().ok()?;
    Some(Zeroizing::new(key))
}

#[cfg(test)]
mod tests {
    use super::{unwrap_key, wrap_key};

    #[test]
    fn wrapped_key_round_trips_at_its_own_generation() {
        let wrapped = wrap_key(&[7u8; 32], &[9u8; 32], 2);
        assert_eq!(unwrap_key(&[7u8; 32], &wrapped, 2).map(|k| *k), Some([9u8; 32]));
    }

    #[test]
    fn a_wrapped_key_does_not_unwrap_at_another_generation() {
        let wrapped = wrap_key(&[7u8; 32], &[9u8; 32], 2);
        assert!(unwrap_key(&[7u8; 32], &wrapped, 3).is_none());
        assert!(unwrap_key(&[7u8; 32], &wrapped, 1).is_none());
    }

    #[test]
    fn a_wrong_kek_does_not_unwrap() {
        let wrapped = wrap_key(&[7u8; 32], &[9u8; 32], 1);
        assert!(unwrap_key(&[8u8; 32], &wrapped, 1).is_none());
    }

    #[test]
    fn a_bit_flip_in_the_wrapped_key_is_rejected() {
        let mut wrapped = wrap_key(&[7u8; 32], &[9u8; 32], 1);
        wrapped[0] ^= 1;
        assert!(unwrap_key(&[7u8; 32], &wrapped, 1).is_none());
    }
}
