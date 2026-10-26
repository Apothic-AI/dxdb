//! Randomness, injected.
//!
//! `specs/encryption.md` section 5.2: a 24-byte nonce per field write and a 16-byte
//! salt per database. Neither can come from `getrandom` here, because a browser
//! build has no OS entropy source behind it, and a crate that silently required one
//! would fail at the boundary rather than at construction.
//!
//! So the platform supplies an [`EntropySource`]. Native and `wasm32-wasip1` get
//! [`OsEntropy`] (the platform CSPRNG); `wasm32-unknown-unknown` gets an adapter
//! over `crypto.getRandomValues` at the JS boundary, which is a few lines in the JS
//! surface crate; tests get [`FixedEntropy`], which is deliberately not random.
//!
//! A caller that reaches for [`FixedEntropy`] in production defeats every nonce
//! guarantee in the spec, so its documentation says so and its name says it.

use std::sync::atomic::{AtomicU64, Ordering};

/// A source of cryptographically random bytes.
///
/// Two concrete methods rather than one generic one, because a generic method makes
/// the trait non-dyn-compatible and the whole point is to hold an
/// `Arc<dyn EntropySource>` supplied by the platform. The sizes are the only two the
/// spec needs, so the type system still checks every call site's length.
pub trait EntropySource: Send + Sync + std::fmt::Debug {
    /// Fills a 24-byte nonce. Called once per field write.
    fn nonce(&self) -> [u8; crate::envelope::NONCE_LEN];

    /// Fills a 16-byte salt. Called once per database, and once per rotation.
    fn salt(&self) -> [u8; crate::keys::SALT_LEN];

    /// A name for diagnostics, so a log line can say which source is in use
    /// without carrying any bytes.
    fn name(&self) -> &'static str;
}

/// The platform CSPRNG.
///
/// Compiled on every target that has one. `wasm32-unknown-unknown` does not (no
/// WASI, no OS), so the platform supplies its own source there; that is why this is
/// behind a `cfg` rather than always present.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
#[derive(Debug, Clone, Copy, Default)]
pub struct OsEntropy;

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
impl EntropySource for OsEntropy {
    fn nonce(&self) -> [u8; crate::envelope::NONCE_LEN] {
        let mut out = [0u8; crate::envelope::NONCE_LEN];
        fill_platform(&mut out).expect("platform CSPRNG unavailable");
        out
    }

    fn salt(&self) -> [u8; crate::keys::SALT_LEN] {
        let mut out = [0u8; crate::keys::SALT_LEN];
        fill_platform(&mut out).expect("platform CSPRNG unavailable");
        out
    }

    fn name(&self) -> &'static str {
        "os"
    }
}

/// Fills `out` from the platform CSPRNG.
///
/// Kept separate from the trait so the per-platform code is in one place and the
/// failure is an explicit `Result` rather than a panic inside a `match`.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn fill_platform(out: &mut [u8]) -> Result<(), ()> {
    #[cfg(unix)]
    {
        use std::io::Read as _;
        let mut file = std::fs::File::open("/dev/urandom").map_err(|_| ())?;
        file.read_exact(out).map_err(|_| ())
    }
    #[cfg(not(unix))]
    {
        // A hash of whatever the platform exposes is not good enough for key
        // material, so this refuses rather than shipping a weak source. A Windows
        // build must supply its own `EntropySource` (BCryptGenRandom) at the binding
        // layer until this crate grows one.
        let _ = out;
        Err(())
    }
}

/// A source that cycles a fixed pattern.
///
/// **Not random.** For tests and for reproducing a failure. Using this in
/// production means every field encryption under one key reuses its nonce, which
/// for XChaCha20-Poly1305 is a confidentiality break — the ciphertexts XOR against
/// each other. The type name and this paragraph are the warning.
#[derive(Debug)]
pub struct FixedEntropy {
    counter: AtomicU64,
}

impl FixedEntropy {
    /// A source starting at zero.
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }
}

impl Default for FixedEntropy {
    fn default() -> Self {
        Self::new()
    }
}

impl EntropySource for FixedEntropy {
    fn nonce(&self) -> [u8; crate::envelope::NONCE_LEN] {
        self.word_stream()
    }

    fn salt(&self) -> [u8; crate::keys::SALT_LEN] {
        self.word_stream()
    }

    fn name(&self) -> &'static str {
        "fixed(test-only)"
    }
}

impl FixedEntropy {
    fn word_stream<const N: usize>(&self) -> [u8; N] {
        // A counter, so successive calls differ — a fixed constant would make two
        // encryptions in one test collide and hide a real bug behind a test-only
        // nonce reuse.
        let seed = self.counter.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let mut out = [0u8; N];
        let mut state = seed;
        for chunk in out.chunks_mut(8) {
            // splitmix64, deterministic and diffusing.
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            let bytes = z.to_be_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{EntropySource, FixedEntropy};

    #[test]
    fn fixed_entropy_varies_between_calls() {
        // Two calls must differ, or a test that forgets to inject a real source
        // would silently reuse a nonce.
        let source = FixedEntropy::new();
        let a = source.nonce();
        let b = source.nonce();
        assert_ne!(a, b);
    }

    #[test]
    fn fixed_entropy_is_deterministic_across_instances() {
        // Reproducibility: two sources started fresh give the same stream, so a
        // failing test can be re-run.
        let a = FixedEntropy::new().salt();
        let b = FixedEntropy::new().salt();
        assert_eq!(a, b);
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    #[test]
    fn os_entropy_varies_between_calls() {
        use super::OsEntropy;
        let source = OsEntropy;
        let a = source.salt();
        let b = source.salt();
        assert_ne!(a, b, "the platform CSPRNG must not repeat");
        assert_ne!(a, [0u8; 16], "and must not return zeros");
        assert_ne!(source.nonce(), source.nonce());
    }
}
