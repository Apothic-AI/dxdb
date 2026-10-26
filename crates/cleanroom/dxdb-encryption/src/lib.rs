//! `dxdb-encryption`: field encryption as a storage wrapper.
//!
//! Implemented from `specs/encryption.md`, not from any closed-source module. The
//! specification was written from public documentation, the Apache-2.0 `crypto-js`
//! plugin (read in order to document what we deliberately do not reproduce), and
//! RFC 9106 / RFC 8439 / draft-irtf-cfrg-xchacha-03. Provenance is recorded per
//! crate in `specs/READING-LIST.md`.
//!
//! # What this is
//!
//! A `RxStorage` decorator. The fields a schema lists in `encrypted` are sealed on
//! the way into the wrapped storage and opened on the way out, so everything above
//! the wrapper sees plaintext and everything below sees ciphertext
//! (`specs/encryption.md` section 1.1).
//!
//! # What it is not
//!
//! **Not compatible with WebCrypto, with upstream's `crypto-js` plugin, or with the
//! paid WebCrypto plugin, and it cannot be made compatible with the latter two.**
//! The envelope is dxdb's own: `DXE1`, XChaCha20-Poly1305, Argon2id, with an AAD
//! that binds each ciphertext to its document, its schema, its field and its
//! revision. A field encrypted by any upstream plugin cannot be decrypted here and
//! vice versa. The migration path is the portable one: read the old data in
//! plaintext, write it with dxdb. See `docs/CLEANROOM-GAPS.md` G-002, G-030.
//!
//! # The invariants
//!
//! Fifteen, numbered in `specs/encryption.md` section 4, each with a test named
//! after it. The ones worth knowing before reading the code:
//!
//! * **INV-2** no ciphertext ever reaches the caller. Every read path decrypts,
//!   including the change stream, which is the easiest one to forget because it
//!   carries whole documents.
//! * **INV-3/4/5/6** a ciphertext cannot be moved to another document, another
//!   collection, another field, or an older revision of the same document, because
//!   all four are in the AAD.
//! * **INV-7** rotation preserves plaintext byte for byte, because it decrypts and
//!   re-encrypts rather than re-wrapping.
//!
//! ```no_run
//! use std::sync::Arc;
//! use dxdb_encryption::{
//!     create_key_material, EncryptionConfig, EncryptionStorage, OsEntropy, Password,
//! };
//!
//! # fn demo<S>(inner: S, schema: &serde_json::Value)
//! #     -> Result<EncryptionStorage<S>, Box<dyn std::error::Error>> {
//! let config = EncryptionConfig::from_schema(schema)?;
//! let (keys, material) = create_key_material(
//!     Password::new("a long passphrase"),
//!     dxdb_encryption::schema_hash(schema),
//!     Arc::new(OsEntropy),
//! )?;
//! // `material` is persisted in the internal store; `keys` lives in memory.
//! let storage = EncryptionStorage::new(inner, Arc::new(keys), config);
//! # let _ = material;
//! # Ok(storage)
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod b64;
pub mod codec;
pub mod config;
pub mod entropy;
pub mod envelope;
pub mod keys;
pub mod keywrap;
pub mod wrapper;

pub use codec::{attachment_field_path, FieldCodec};
pub use config::{
    check_password_strength, EncryptedPaths, Password, PasswordError, PathError, WeaknessReason,
    MIN_PASSWORD_LEN,
};
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub use entropy::OsEntropy;
pub use entropy::{EntropySource, FixedEntropy};
pub use envelope::{
    schema_hash, Aad, Envelope, EnvelopeError, MAGIC, NONCE_LEN, OVERHEAD, SUITE, VERSION,
};
pub use keys::{KdfParams, Key, KeyError, KeyMaterial, KeyRing, Keys};
pub use wrapper::{
    create_key_material, inert_config, rotate, EncryptionConfig, EncryptionInstance,
    EncryptionStorage, RotationProgress,
};

/// Everything this crate can fail with.
///
/// A closed list. No variant carries plaintext, a password, a salt, a key, or a
/// nonce, so an error can never leak secret material into a log or a crash report
/// (INV-11). The envelope and password variants wrap types with the same property.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncryptionError {
    /// The password does not open this database. Detected at open, from the stored
    /// verifier, before any document is read (INV-8).
    #[error("the password does not match this database")]
    WrongPassword,
    /// An encrypted collection was opened with no password.
    #[error("a password is required to open an encrypted collection")]
    PasswordRequired,
    /// The password is below the documented floor (`docs/CLEANROOM-GAPS.md` G-032).
    #[error(transparent)]
    Password(#[from] PasswordError),
    /// The envelope is structurally invalid, of an unknown version, or failed
    /// authentication (a transplant, a rollback, or tampering).
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    /// A ciphertext is well formed but its key generation is not available to this
    /// build: either the generation was retired (so the data is deliberately
    /// unrecoverable, `specs/encryption.md` section 3.6), or the material we were
    /// given is older than the data (a rotation was persisted but not picked up).
    #[error("key generation {0} is not available to this key material")]
    UnavailableGeneration(u8),
    /// `schema.encrypted` is malformed, or a child of an encrypted parent
    /// (INV-15).
    #[error(transparent)]
    Path(#[from] PathError),
    /// A query named an encrypted path (INV-12, `docs/CLEANROOM-GAPS.md` G-031).
    #[error("encrypted path in query: {0}")]
    EncryptedPathInQuery(String),
    /// A document has no readable primary key, so its AAD cannot be built.
    #[error("document has no readable primary key")]
    MissingDocumentId,
    /// An encrypted path could not be written into the document's JSON shape.
    #[error("cannot write encrypted path {0}")]
    MalformedPath(String),
    /// Argon2id could not derive a key with the configured parameters.
    #[error("key derivation failed")]
    Kdf,
    /// The `localStorage`-style metadata cache refused, which cannot happen here but
    /// keeps the variant list stable across the wrapper family.
    #[error("encryption in a worker is required for this configuration")]
    EncryptionInWorkerRequired,
}
