//! The `DXE1` envelope: layout, AAD, and the AEAD calls.
//!
//! Implements `specs/encryption.md` section 3.8 exactly:
//!
//! ```text
//! magic      4 bytes   "DXE1"
//! version    1 byte    0x01
//! suite      1 byte    0x01     (Argon2id + XChaCha20-Poly1305)
//! gen        1 byte    key generation, 1..=255
//! nonce     24 bytes   random, per write
//! tag       16 bytes   Poly1305
//! body       N bytes   XChaCha20 keystream
//! ```
//!
//! 47 bytes of fixed overhead. Anything that is not exactly this is rejected;
//! there is no partial parse and no "try the newest parser" fallback, because a
//! parser that guesses is a parser that accepts forged data.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

/// Envelope family magic. Anything else is not a dxdb envelope.
pub const MAGIC: [u8; 4] = *b"DXE1";
/// Envelope layout version. One writer writes this until a `0x02` reader ships.
pub const VERSION: u8 = 0x01;
/// KDF + cipher suite identifier: Argon2id + XChaCha20-Poly1305.
pub const SUITE: u8 = 0x01;

/// `magic + version + suite + gen`.
pub const HEADER_PREFIX_LEN: usize = 7;
/// Nonce length for XChaCha20 (192-bit).
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;
/// Fixed bytes an envelope adds on top of its body.
pub const OVERHEAD: usize = HEADER_PREFIX_LEN + NONCE_LEN + TAG_LEN;

/// Domain separator, so an envelope encrypted for one purpose cannot verify
/// against another (no second purpose exists yet; the separator is here before it
/// does).
const AAD_DOMAIN: &[u8] = b"dxdb.enc.v1";

/// The associated data for one field write.
///
/// Length-prefixed, not joined: a document id containing the separator cannot
/// introduce ambiguity, which a joined string would allow silently.
#[derive(Debug, Clone)]
pub struct Aad {
    /// The document's primary key value.
    pub document_id: String,
    /// The collection's schema hash, 32 raw bytes.
    pub schema_hash: [u8; 32],
    /// The encrypted field's path as written in the schema, e.g. `assignee`.
    pub field_path: String,
    /// The document's `_rev` for the revision being written. `None` only for a
    /// write that carries no revision, which the storage contract does not permit
    /// but which this type must still be able to represent.
    pub revision: Option<String>,
    /// Which key generation the ciphertext is sealed under.
    pub generation: u8,
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("aad component longer than 4 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

impl Aad {
    /// The bytes the AEAD authenticates.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            AAD_DOMAIN.len() + self.document_id.len() + 32 + self.field_path.len() + 16 + 8,
        );
        push_len_prefixed(&mut out, AAD_DOMAIN);
        push_len_prefixed(&mut out, self.document_id.as_bytes());
        push_len_prefixed(&mut out, &self.schema_hash);
        push_len_prefixed(&mut out, self.field_path.as_bytes());
        // The revision is distinguished from "absent" rather than encoded as an
        // empty string, so a document with an empty revision cannot collide with
        // one that has none.
        match &self.revision {
            Some(rev) => {
                push_len_prefixed(&mut out, b"rev");
                push_len_prefixed(&mut out, rev.as_bytes());
            }
            None => push_len_prefixed(&mut out, b""),
        }
        out.push(self.generation);
        out
    }
}

/// A parsed envelope: borrowed from the byte string it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope<'a> {
    /// Key generation the ciphertext was sealed under.
    pub generation: u8,
    /// The 24-byte nonce.
    pub nonce: [u8; NONCE_LEN],
    /// Ciphertext followed by the 16-byte tag, which is the shape
    /// `chacha20poly1305` expects.
    pub body: &'a [u8],
}

impl<'a> Envelope<'a> {
    /// Parses an envelope from its stored (base64-decoded) bytes.
    ///
    /// Rejects: a bad magic, an unknown version or suite (with the byte
    /// reported, so a user on an old build is told the database is newer rather
    /// than told decryption failed), and a body too short to hold a tag.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, EnvelopeError> {
        if bytes.len() < OVERHEAD {
            return Err(EnvelopeError::TooShort { len: bytes.len() });
        }
        if bytes[0..4] != MAGIC {
            return Err(EnvelopeError::NotAnEnvelope);
        }
        let version = bytes[4];
        let suite = bytes[5];
        if version != VERSION || suite != SUITE {
            return Err(EnvelopeError::Unsupported { version, suite });
        }
        let generation = bytes[6];
        if generation == 0 {
            return Err(EnvelopeError::ZeroGeneration);
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[HEADER_PREFIX_LEN..HEADER_PREFIX_LEN + NONCE_LEN]);
        let body = &bytes[HEADER_PREFIX_LEN + NONCE_LEN..];
        Ok(Envelope {
            generation,
            nonce,
            body,
        })
    }
}

/// Why an envelope was rejected.
///
/// Every variant is structural. None carries plaintext, a nonce, a key, or the
/// ciphertext, so no error can leak secret material into a log
/// (`specs/encryption.md` INV-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    /// Shorter than the fixed overhead.
    #[error("envelope is {len} bytes, shorter than the {OVERHEAD}-byte minimum")]
    TooShort {
        /// The length that was found.
        len: usize,
    },
    /// The magic does not match, so this is not a dxdb envelope at all.
    #[error("not a dxdb envelope")]
    NotAnEnvelope,
    /// A version or suite this build does not implement.
    #[error("unsupported envelope version {version} suite {suite}")]
    Unsupported {
        /// The version byte that was found.
        version: u8,
        /// The suite byte that was found.
        suite: u8,
    },
    /// Generation 0 is never valid; generations start at 1.
    #[error("envelope has key generation 0")]
    ZeroGeneration,
    /// The Poly1305 tag did not verify.
    #[error("envelope authentication failed")]
    AuthenticationFailed,
}

/// Encrypts `plaintext` under `key`.
///
/// `nonce` is supplied rather than generated here so the caller owns the entropy
/// source (section 5.2: a browser build has no OS entropy, so randomness is
/// injected) and so tests can be deterministic where that is useful.
pub fn seal(
    key: &[u8; 32],
    generation: u8,
    nonce: [u8; NONCE_LEN],
    aad: &Aad,
    plaintext: &[u8],
) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("32-byte key");
    let aad_bytes = aad.to_bytes();
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &aad_bytes,
            },
        )
        .expect("XChaCha20-Poly1305 encryption cannot fail for a valid key");

    let mut out = Vec::with_capacity(OVERHEAD + plaintext.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(SUITE);
    out.push(generation);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    out
}

/// Decrypts `bytes`, authenticating `aad`.
///
/// Returns `Zeroizing` so the plaintext is wiped when the caller drops it.
pub fn open(key: &[u8; 32], aad: &Aad, bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, EnvelopeError> {
    let envelope = Envelope::parse(bytes)?;
    if envelope.generation != aad.generation {
        // The generation is authenticated as AAD as well, so a mismatch here would
        // fail the tag anyway. Checking first means a wrong-generation read is
        // reported as what it is instead of as a generic authentication failure,
        // and it keeps the two failure modes from being confused in a test.
        return Err(EnvelopeError::Unsupported {
            version: VERSION,
            suite: SUITE,
        });
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("32-byte key");
    let aad_bytes = aad.to_bytes();
    cipher
        .decrypt(
            &XNonce::from(envelope.nonce),
            Payload {
                msg: envelope.body,
                aad: &aad_bytes,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| EnvelopeError::AuthenticationFailed)
}

/// SHA-256, used for the schema hash that the AAD binds.
///
/// The schema hash is upstream's collection identity: `RxSchema.hash` is
/// `hashFunction(JSON.stringify(schema))` (`src/rx-schema.ts:113`). dxdb computes
/// it from `dxdb-json::canonical_bytes` so that JS number and key-order semantics
/// are preserved (PLAN.md 3.1) — the same canonical form the port uses everywhere
/// it hashes a document.
pub fn schema_hash(schema: &serde_json::Value) -> [u8; 32] {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(dxdb_json::canonical_bytes(schema));
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::{open, schema_hash, seal, Aad, Envelope, EnvelopeError, NONCE_LEN, OVERHEAD};

    fn aad() -> Aad {
        Aad {
            document_id: "doc-1".into(),
            schema_hash: [3u8; 32],
            field_path: "note".into(),
            revision: Some("1-abc".into()),
            generation: 1,
        }
    }

    #[test]
    fn overhead_is_exactly_forty_seven_bytes() {
        let key = [0u8; 32];
        let sealed = seal(&key, 1, [7u8; NONCE_LEN], &aad(), b"hello");
        assert_eq!(sealed.len(), OVERHEAD + 5);
        assert_eq!(OVERHEAD, 47);
    }

    #[test]
    fn round_trips() {
        let key = [1u8; 32];
        let sealed = seal(&key, 1, [2u8; NONCE_LEN], &aad(), b"the plaintext");
        let opened = open(&key, &aad(), &sealed).expect("valid");
        assert_eq!(&opened[..], b"the plaintext");
    }

    #[test]
    fn a_short_envelope_is_rejected() {
        assert_eq!(
            Envelope::parse(&[0u8; 10]),
            Err(EnvelopeError::TooShort { len: 10 })
        );
    }

    #[test]
    fn a_foreign_magic_is_rejected() {
        let mut sealed = seal(&[0u8; 32], 1, [0u8; NONCE_LEN], &aad(), b"x");
        sealed[0] = b'X';
        assert_eq!(Envelope::parse(&sealed), Err(EnvelopeError::NotAnEnvelope));
    }

    #[test]
    fn generation_zero_is_rejected() {
        let mut sealed = seal(&[0u8; 32], 1, [0u8; NONCE_LEN], &aad(), b"x");
        sealed[6] = 0;
        assert_eq!(Envelope::parse(&sealed), Err(EnvelopeError::ZeroGeneration));
    }

    #[test]
    fn schema_hash_is_stable_and_canonical() {
        use serde_json::json;
        // Key order matters to the hash (PLAN.md 3.1), and equal order gives equal
        // hashes.
        let a = json!({"primaryKey": "id", "version": 0, "indexes": []});
        let b = json!({"primaryKey": "id", "version": 0, "indexes": []});
        assert_eq!(schema_hash(&a), schema_hash(&b));
        let reordered = json!({"indexes": [], "version": 0, "primaryKey": "id"});
        assert_ne!(schema_hash(&a), schema_hash(&reordered));
        // A JS-typed 1 and 1.0 must hash the same, because a JS client sees one
        // number.
        let int_one = json!({"v": 1});
        let float_one = json!({"v": 1.0});
        assert_eq!(schema_hash(&int_one), schema_hash(&float_one));
    }
}
