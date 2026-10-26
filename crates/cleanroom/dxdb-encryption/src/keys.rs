//! The key hierarchy: password → master key → per-generation key-encryption key →
//! per-generation data key.
//!
//! ```text
//! password + master_salt  --Argon2id-->  master_key (32 bytes)      [cached, process memory only]
//! master_key  --AES-256-GCM-->  generation 1 KEK                    [unwrapped at open]
//! KEK(n)      --AES-256-GCM-->  generation n+1 data key             [wrapped, stored]
//! data key(n) --XChaCha20-Poly1305-->  field ciphertexts            [the envelope]
//! ```
//!
//! Two keys per generation rather than one, because rotation must replace the key
//! that encrypts data (so a leaked data key dies) while the *unlock* path stays
//! one Argon2id call per database open. The alternative — re-deriving the data key
//! from the password at each generation — would make rotating cost another
//! Argon2id call at every open, and there is no second password to derive from, so
//! the only way to make generation `n+1` different from generation `n` is to chain
//! a wrapped key, not to re-hash.
//!
//! The chain is what makes retirement mean something. Deleting generation `n`'s
//! wrapped blob makes generation `n+1` undecryptable, because its key is not
//! derivable from the password alone. See `specs/encryption.md` section 3.6.

use std::sync::Arc;

use zeroize::Zeroizing;

use crate::config::Password;
use crate::envelope::EnvelopeError;
use crate::entropy::EntropySource;
use crate::keywrap::{unwrap_key, wrap_key};

/// Argon2id: memory in 1 KiB blocks — RFC 9106 §4 SECOND RECOMMENDED `m = 2^16`.
pub const KDF_M_COST_KIB: u32 = 65_536;
/// Argon2id: iterations — RFC 9106 §4 SECOND RECOMMENDED `t = 3`.
pub const KDF_T_COST: u32 = 3;
/// Argon2id: lanes. `p = 1` because the default wasm build is single-threaded; see
/// `docs/CLEANROOM-GAPS.md` G-035 and `specs/encryption.md` section 3.3.
pub const KDF_P_COST: u32 = 1;
/// Master and data keys are 256-bit, matching the cipher.
pub const KEY_LEN: usize = 32;
/// Salt length: RFC 9106 §3.1 recommends 16 bytes.
pub const SALT_LEN: usize = 16;
/// Argon2 version 1.3, the only one RFC 9106 defines.
pub const ARGON2_VERSION: u8 = 0x13;

/// A 256-bit key that wipes itself when dropped.
pub type Key = Zeroizing<[u8; KEY_LEN]>;

/// Everything a database needs to reopen its key chain, and nothing that reveals a
/// key.
///
/// Stored by the caller in the internal store beside upstream's `passwordHash`
/// (`specs/encryption.md` section 3.5). This crate owns the *format* of these
/// fields, not the store they live in.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyMaterial {
    /// The per-database KDF salt.
    pub salt: [u8; SALT_LEN],
    /// The parameters the salt was used with.
    pub params: KdfParams,
    /// `SHA-256("dxdb.enc.verifier.v1" || master_key)`, so a wrong password is
    /// detected before any document has to be read.
    pub verifier: [u8; 32],
    /// The generation new writes use.
    pub current_generation: u8,
    /// Data keys for generations 2..=current, each wrapped under the previous
    /// generation's key. Generation 1 has no entry, because its key derives from
    /// the master key.
    pub wrapped_keys: Vec<Vec<u8>>,
}

/// The Argon2id parameters a database was created with.
///
/// Stored per database so a future parameter change is a *migration*, not a silent
/// re-derivation of everyone's key material. `p = 1` always, per G-035; the field
/// exists so a record of the value is persisted rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory cost in 1 KiB blocks.
    pub m_cost_kib: u32,
    /// Iteration count.
    pub t_cost: u32,
    /// Parallelism, always 1.
    pub p_cost: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            m_cost_kib: KDF_M_COST_KIB,
            t_cost: KDF_T_COST,
            p_cost: KDF_P_COST,
        }
    }
}

impl KdfParams {
    /// The documented parameter set, spelled out so a reader can compare it with
    /// RFC 9106 §4 without opening the code.
    pub const RFC9106_SECOND_RECOMMENDED: Self = Self {
        m_cost_kib: KDF_M_COST_KIB,
        t_cost: KDF_T_COST,
        p_cost: KDF_P_COST,
    };

    /// Derives the master key from `password` and `salt`.
    ///
    /// This is the only expensive call in the crate: 64 MiB and three passes. It
    /// runs **once per database open**, never per operation and never per field
    /// (`specs/encryption.md` section 3.7).
    pub fn derive_master_key(
        &self,
        password: &[u8],
        salt: &[u8; SALT_LEN],
    ) -> Result<Key, KeyError> {
        let params = argon2::Params::new(
            self.m_cost_kib,
            self.t_cost,
            self.p_cost,
            Some(KEY_LEN),
        )
        .map_err(|_| KeyError::Parameters)?;
        let argon = argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            params,
        );
        let mut out = Zeroizing::new([0u8; KEY_LEN]);
        argon
            .hash_password_into(password, salt, out.as_mut())
            .map_err(|_| KeyError::Derivation)?;
        Ok(out)
    }
}

/// Why key derivation failed. Both variants are "this build cannot do it", never
/// "your password is wrong" — a wrong password is detected by authenticating the
/// verifier, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The parameter set is not one Argon2 accepts.
    #[error("unsupported kdf parameters")]
    Parameters,
    /// Argon2 could not derive: it could not allocate its memory, or the inputs
    /// were outside what the algorithm accepts.
    #[error("kdf derivation failed")]
    Derivation,
}

/// Chains one generation's key to the next.
///
/// `kek_n -> wrap -> data key n+1`. Wrapping rather than deriving is what lets a
/// retired generation take its successors with it.
#[derive(Debug, Clone)]
pub struct KeyRing {
    /// The generation whose data key is currently used for writes.
    pub current: u8,
    /// Generation `n`'s KEK, unwrapped and held in memory while the database is
    /// open. Index 0 is generation 1.
    kes: Vec<Key>,
    /// The data key of every generation still readable. Index 0 is generation 1.
    ///
    /// Kept separately from `kes` because generation 1's *data* key is derived from
    /// the master key rather than being a wrapped key, so "the previous KEK" is not
    /// the same thing as "the previous data key".
    data_keys: Vec<Key>,
    /// The current generation's data key, i.e. the last of `data_keys`.
    data_key: Key,
}

impl KeyRing {
    /// Builds the ring for a database whose stored state is known.
    ///
    /// `wrapped_keys[i]` is generation `i+2`'s data key, wrapped under generation
    /// `i+1`'s KEK. Generation 1's KEK comes from `master_key`.
    ///
    /// Returns [`EnvelopeError::AuthenticationFailed`] if any link does not
    /// unwrap, which is how "the wrapped-key chain was tampered with" surfaces.
    pub fn restore(
        master_key: &Key,
        current: u8,
        wrapped_keys: &[Vec<u8>],
    ) -> Result<Self, EnvelopeError> {
        if current == 0 {
            return Err(EnvelopeError::ZeroGeneration);
        }
        if wrapped_keys.len() != usize::from(current) - 1 {
            // Every generation after the first must have exactly one wrapped key.
            return Err(EnvelopeError::AuthenticationFailed);
        }
        // Generation 1's KEK is the master key itself: the master key never leaves
        // memory and is never stored, so it is the root of the chain.
        let mut kes: Vec<Key> = Vec::with_capacity(usize::from(current));
        kes.push(master_key.clone());
        let root = self_root_data_key(master_key);
        let mut data_keys = vec![root.clone()];
        let mut data_key = root;
        for (index, wrapped) in wrapped_keys.iter().enumerate() {
            let generation = u8::try_from(index + 2).expect("generation fits in u8");
            // `rotate` wraps the new *data key* under the previous generation's KEK,
            // and that wrapped key then serves as the next link's KEK too, so the
            // chain is kek1 -> data2 -> data3 -> ...
            let kek = kes[index].clone();
            let next = unwrap_key(&kek, wrapped, generation)
                .ok_or(EnvelopeError::AuthenticationFailed)?;
            kes.push(next.clone());
            data_keys.push(next.clone());
            data_key = next;
        }
        Ok(Self {
            current,
            kes,
            data_keys,
            data_key,
        })
    }

    /// Creates a brand-new ring: generation 1, with the master key as KEK and a
    /// data key derived from it.
    pub fn new(master_key: &Key) -> Self {
        let data_key = self_root_data_key(master_key);
        Self {
            current: 1,
            kes: vec![master_key.clone()],
            data_keys: vec![data_key.clone()],
            data_key,
        }
    }

    /// The data key for `generation`, if this ring still holds it.
    ///
    /// After rotation the older generations remain readable until their wrapped
    /// keys are dropped, which is exactly the "both generations readable during the
    /// rewrite" behaviour section 3.6 requires.
    pub fn data_key(&self, generation: u8) -> Option<Key> {
        if generation == 0 || generation > self.current {
            return None;
        }
        self.data_keys
            .get(usize::from(generation) - 1)
            .cloned()
    }

    /// The current generation's data key.
    pub fn current_data_key(&self) -> Key {
        self.data_key.clone()
    }

    /// Advances to a new generation with a fresh data key, returning the wrapped
    /// key that must be persisted.
    ///
    /// `new_data_key` is caller-supplied so the entropy source stays injected
    /// (section 5.2) and so a test can assert exact bytes.
    pub fn rotate(&self, new_data_key: Key) -> Option<(KeyRing, Vec<u8>)> {
        let next_generation = self.current.checked_add(1)?;
        let current_kek = self.kes.last()?.clone();
        let wrapped = wrap_key(&current_kek, &new_data_key, next_generation);
        let mut kes = self.kes.clone();
        kes.push(new_data_key.clone());
        let mut data_keys = self.data_keys.clone();
        data_keys.push(new_data_key.clone());
        Some((
            KeyRing {
                current: next_generation,
                kes,
                data_keys,
                data_key: new_data_key,
            },
            wrapped,
        ))
    }

    /// How many generations this ring can still read.
    pub fn readable_generations(&self) -> u8 {
        self.current
    }
}

/// The open key state of one database: the ring plus what is needed to persist it.
pub struct Keys {
    /// The current key ring.
    pub(crate) ring: KeyRing,
    /// The password, kept so a rotation does not need the user to re-enter it. Never
    /// printed.
    pub(crate) password: Password,
    /// The persisted material this state came from, or will be written as.
    pub(crate) material: KeyMaterial,
    /// The injected entropy source.
    pub(crate) entropy: Arc<dyn EntropySource>,
    /// The collection's schema hash, bound into every AAD.
    pub(crate) schema_hash: [u8; 32],
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys")
            .field("generation", &self.ring.current)
            .field("readable_generations", &self.ring.readable_generations())
            .finish_non_exhaustive()
    }
}

impl Keys {
    /// Opens existing key material. One Argon2id derivation, then nothing expensive
    /// for the life of the database (`specs/encryption.md` section 3.7).
    pub fn unlock(
        password: Password,
        material: KeyMaterial,
        schema_hash: [u8; 32],
        entropy: Arc<dyn EntropySource>,
    ) -> Result<Self, crate::EncryptionError> {
        let master = material
            .params
            .derive_master_key(password.as_bytes(), &material.salt)
            .map_err(|_| crate::EncryptionError::Kdf)?;
        if !crate::config::verifier_matches(&master, &material.verifier) {
            return Err(crate::EncryptionError::WrongPassword);
        }
        let ring = KeyRing::restore(&master, material.current_generation, &material.wrapped_keys)
            .map_err(crate::EncryptionError::Envelope)?;
        Ok(Self {
            ring,
            password,
            material,
            entropy,
            schema_hash,
        })
    }

    /// Creates fresh key material for a new database.
    pub fn create(
        password: Password,
        schema_hash: [u8; 32],
        entropy: Arc<dyn EntropySource>,
    ) -> Result<(Self, KeyMaterial), crate::EncryptionError> {
        crate::config::check_password_strength(&password)?;
        let salt = entropy.salt();
        let params = KdfParams::RFC9106_SECOND_RECOMMENDED;
        let master = params
            .derive_master_key(password.as_bytes(), &salt)
            .map_err(|_| crate::EncryptionError::Kdf)?;
        let verifier = crate::config::verifier_for(&master);
        let ring = KeyRing::new(&master);
        let material = KeyMaterial {
            salt,
            params,
            verifier,
            current_generation: 1,
            wrapped_keys: Vec::new(),
        };
        let stored = material.clone();
        Ok((
            Self {
                ring,
                password,
                material,
                entropy,
                schema_hash,
            },
            stored,
        ))
    }

    /// The same keys with a different ring, for a rotation's re-encryption pass.
    pub(crate) fn with_ring(&self, ring: KeyRing) -> Self {
        Self {
            ring,
            password: Password::new(String::new()),
            material: self.material.clone(),
            entropy: Arc::clone(&self.entropy),
            schema_hash: self.schema_hash,
        }
    }

    /// The generation new writes use.
    pub fn current_generation(&self) -> u8 {
        self.ring.current
    }

    /// The persisted form of this state.
    pub fn material(&self) -> &KeyMaterial {
        &self.material
    }

    /// The password, so a rotation does not ask the user to re-enter it.
    pub fn password(&self) -> &Password {
        &self.password
    }

    /// The current key ring, for a rotation's re-encryption pass.
    pub(crate) fn ring(&self) -> &KeyRing {
        &self.ring
    }

    /// The entropy source.
    pub fn entropy(&self) -> &Arc<dyn EntropySource> {
        &self.entropy
    }

    /// The schema hash every AAD for this collection binds.
    pub fn schema_hash(&self) -> [u8; 32] {
        self.schema_hash
    }

    /// The current generation's data key.
    ///
    /// Public because a rotation must hand the new key to the codec explicitly, and
    /// because a test proving INV-9/INV-11 needs to demonstrate that errors do not
    /// contain it. `Key` is `Zeroizing` and has no `Display`, so this cannot end up
    /// in a log by accident.
    pub fn current_data_key(&self) -> Key {
        self.ring.current_data_key()
    }

    /// The data key of `generation`, if it is still readable.
    pub fn data_key(&self, generation: u8) -> Option<Key> {
        self.ring.data_key(generation)
    }

    /// Assembles state from parts, for tests that assert the codec independently of
    /// the KDF. Everyone else uses [`Keys::unlock`] or [`Keys::create`].
    #[doc(hidden)]
    pub fn from_parts(
        ring: KeyRing,
        material: KeyMaterial,
        schema_hash: [u8; 32],
        entropy: Arc<dyn EntropySource>,
    ) -> Self {
        Self {
            ring,
            password: Password::new(String::new()),
            material,
            entropy,
            schema_hash,
        }
    }
}

/// Generation 1's data key, derived from the master key with domain separation.
///
/// A plain `master_key.clone()` would make the KEK and the data key the same
/// value, so a wrapped-key blob and a field ciphertext would share a key — exactly
/// the kind of cross-protocol key reuse that turns one bug into two.
fn self_root_data_key(master_key: &Key) -> Key {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"dxdb.enc.data.v1");
    hasher.update(master_key.as_ref());
    let digest = hasher.finalize();
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::{self_root_data_key, KeyError, KeyRing, KdfParams};
    use zeroize::Zeroizing;

    fn master() -> Zeroizing<[u8; 32]> {
        Zeroizing::new([5u8; 32])
    }

    #[test]
    fn domain_separation_gives_a_different_data_key() {
        let m = master();
        assert_ne!(self_root_data_key(&m).as_ref(), m.as_ref());
    }

    #[test]
    fn a_new_ring_holds_only_generation_one() {
        let ring = KeyRing::new(&master());
        assert_eq!(ring.current, 1);
        assert!(ring.data_key(1).is_some());
        assert!(ring.data_key(2).is_none());
        assert!(ring.data_key(0).is_none());
    }

    #[test]
    fn rotation_chains_and_the_old_generation_stays_readable() {
        let ring = KeyRing::new(&master());
        let old_key = ring.current_data_key();
        let (rotated, wrapped) = ring
            .rotate(Zeroizing::new([9u8; 32]))
            .expect("can rotate from generation 1");
        assert_eq!(rotated.current, 2);
        assert_eq!(rotated.data_key(2).map(|k| *k), Some([9u8; 32]));
        // Generation 1 is still readable during the rewrite.
        assert_eq!(rotated.data_key(1).map(|k| *k), Some(*old_key));

        // And a restored ring reaches the same keys from the stored wrap.
        let restored = KeyRing::restore(&master(), 2, &[wrapped]).expect("chain restores");
        assert_eq!(restored.data_key(2).map(|k| *k), Some([9u8; 32]));
        assert_eq!(restored.data_key(1).map(|k| *k), Some(*old_key));
    }

    #[test]
    fn retiring_a_generation_breaks_its_successors() {
        // This is the whole point of rotation: with generation 1's wrapped key
        // gone, generation 2 cannot be reconstructed even with the password.
        let ring = KeyRing::new(&master());
        let (_rotated, _wrapped) = ring.rotate(Zeroizing::new([9u8; 32])).expect("rotate");
        // Restoring with the wrapped key missing is a chain-length mismatch.
        assert!(KeyRing::restore(&master(), 2, &[]).is_err());
    }

    #[test]
    fn a_tampered_wrap_does_not_restore() {
        let ring = KeyRing::new(&master());
        let (_rotated, mut wrapped) = ring.rotate(Zeroizing::new([9u8; 32])).expect("rotate");
        wrapped[0] ^= 0xff;
        assert!(KeyRing::restore(&master(), 2, &[wrapped]).is_err());
    }

    #[test]
    fn generation_zero_is_refused() {
        assert!(KeyRing::restore(&master(), 0, &[]).is_err());
    }

    #[test]
    fn the_kdf_parameters_are_the_rfc_second_recommended_option() {
        let p = KdfParams::default();
        assert_eq!(p.m_cost_kib, 65_536, "2^16 KiB = 64 MiB");
        assert_eq!(p.t_cost, 3);
        assert_eq!(p.p_cost, 1);
        assert_eq!(p, KdfParams::RFC9106_SECOND_RECOMMENDED);
    }

    #[test]
    fn derivation_is_deterministic_and_salt_separated() {
        // Kept to the cheapest legal parameter set so the test suite stays fast: the
        // *shape* of the derivation is what is under test here, not the cost, and
        // the default parameters are exercised once in the KDF timing probe.
        let cheap = KdfParams {
            m_cost_kib: 64,
            t_cost: 1,
            p_cost: 1,
        };
        let salt_a = [1u8; 16];
        let salt_b = [2u8; 16];
        let a = cheap.derive_master_key(b"password", &salt_a).expect("ok");
        let b = cheap.derive_master_key(b"password", &salt_a).expect("ok");
        let c = cheap.derive_master_key(b"password", &salt_b).expect("ok");
        let d = cheap.derive_master_key(b"other", &salt_a).expect("ok");
        assert_eq!(*a, *b, "same inputs give the same key");
        assert_ne!(*a, *c, "a different salt gives a different key");
        assert_ne!(*a, *d, "a different password gives a different key");
    }

    #[test]
    fn impossible_parameters_are_rejected() {
        let bad = KdfParams {
            // Below Argon2's minimum for one lane.
            m_cost_kib: 1,
            t_cost: 1,
            p_cost: 1,
        };
        assert_eq!(
            bad.derive_master_key(b"x", &[0u8; 16]),
            Err(KeyError::Parameters)
        );
    }
}
