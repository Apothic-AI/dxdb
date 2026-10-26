//! Routing: which shard a document lives in.
//!
//! `specs/sharding.md` section 3.4. Two properties are load-bearing and both are
//! testable rather than asserted:
//!
//! * **pure** — the same primary key routes to the same shard in every process, after
//!   every restart, on every target. No `HashMap` iteration order, no
//!   `DefaultHasher` (whose output is explicitly unspecified across releases), no
//!   address, no random seed. A hash that differed between two processes would make
//!   two tabs disagree about where a document lives, which is data loss that looks
//!   like a missing document.
//! * **stable across the wasm boundary** — a database can be opened by a browser and
//!   by Node, so the hash must be specified, not inherited from a library whose
//!   algorithm may change.
//!
//! Hence FNV-1a 64-bit over the UTF-8 bytes of the primary key string, then `mod N`.
//! Chosen by us: nothing public specifies a routing hash, so
//! `docs/CLEANROOM-GAPS.md` G-039 records that we picked one and why. FNV-1a is not
//! cryptographic and does not need to be; shard assignment is not an attack surface,
//! because an attacker who can choose primary keys can already choose which shard to
//! fill and the storage does not care.

use std::num::NonZeroU32;

/// The FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// The FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64-bit over `bytes`.
///
/// Spelled out rather than taken from a crate so the algorithm is pinned by this
/// file's tests and cannot change under us in a dependency bump.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// How a collection is sharded.
///
/// `shards` is `NonZeroU32` so zero is unrepresentable and the modulo cannot divide by
/// zero. There is no `Default`: the documented guidance ("10 shards is a good value to
/// start with") is a recommendation, not a default we should bake in
/// (`docs/CLEANROOM-GAPS.md` G-038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardingConfig {
    /// How many shards one collection is split across.
    pub shards: NonZeroU32,
    /// Whether the shards are separate storage instances in one database or separate
    /// databases.
    pub mode: ShardMode,
}

/// The two documented sharding modes.
///
/// The mode changes only how shard instances are *named*. It does not change routing,
/// merging, or any result, which is deliberate: correctness is proven once rather than
/// once per mode (`specs/sharding.md` section 1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardMode {
    /// Shards are separate stores within one underlying database.
    Collection,
    /// Shards are separate underlying databases.
    Database,
}

impl ShardMode {
    /// The documented spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            ShardMode::Collection => "collection",
            ShardMode::Database => "database",
        }
    }

    /// Parses the documented spelling.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "collection" => Some(ShardMode::Collection),
            "database" => Some(ShardMode::Database),
            _ => None,
        }
    }
}

impl ShardingConfig {
    /// Builds a config, rejecting a shard count of zero.
    pub fn new(shards: u32, mode: ShardMode) -> Option<Self> {
        NonZeroU32::new(shards).map(|shards| Self { shards, mode })
    }

    /// The shard index `primary_key` belongs to.
    pub fn shard_of(&self, primary_key: &str) -> u32 {
        shard_of(primary_key, self.shards.get())
    }

    /// Reads `sharding` out of a schema value.
    ///
    /// A missing block means the collection is not sharded. A present-but-invalid one
    /// is an error rather than being ignored, because silently not sharding a
    /// collection the schema asked to shard would leave the data in one shard while
    /// the config says otherwise.
    pub fn from_schema(schema: &serde_json::Value) -> Result<Option<Self>, ConfigError> {
        let Some(block) = schema.get("sharding") else {
            return Ok(None);
        };
        if block.is_null() {
            return Ok(None);
        }
        let Some(object) = block.as_object() else {
            return Err(ConfigError::NotAnObject);
        };
        let shards = object
            .get("shards")
            .and_then(serde_json::Value::as_u64)
            .ok_or(ConfigError::MissingShards)?;
        let shards = u32::try_from(shards).map_err(|_| ConfigError::TooManyShards)?;
        let Some(shards) = NonZeroU32::new(shards) else {
            return Err(ConfigError::ZeroShards);
        };
        let mode = object
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .ok_or(ConfigError::MissingMode)?;
        let Some(mode) = ShardMode::parse(mode) else {
            return Err(ConfigError::UnknownMode(mode.to_string()));
        };
        Ok(Some(Self { shards, mode }))
    }
}

/// Why a `sharding` block was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `sharding` is not an object.
    #[error("schema.sharding must be an object")]
    NotAnObject,
    /// No `shards` value.
    #[error("schema.sharding.shards is required")]
    MissingShards,
    /// `shards: 0`.
    #[error("schema.sharding.shards must be at least 1")]
    ZeroShards,
    /// `shards` does not fit in `u32`.
    #[error("schema.sharding.shards does not fit in u32")]
    TooManyShards,
    /// No `mode` value.
    #[error("schema.sharding.mode is required")]
    MissingMode,
    /// A mode other than the two documented ones.
    #[error("schema.sharding.mode {0:?} is not one of 'collection' or 'database'")]
    UnknownMode(String),
}

/// The shard index for `primary_key` out of `shards`.
pub fn shard_of(primary_key: &str, shards: u32) -> u32 {
    debug_assert!(shards > 0, "shard count must be positive");
    // `as u32` is a deliberate truncation of a 64-bit hash, not a narrowing bug: the
    // FNV output is uniform over the low bits, and a shard count is at most `u32`.
    let hash = fnv1a64(primary_key.as_bytes());
    (hash % u64::from(shards)) as u32
}

/// The `(database, collection)` name a shard instance uses.
///
/// The mode determines which component carries the shard suffix, which is what makes
/// `collection` mode produce separate stores in one database and `database` mode
/// produce separate databases. Nothing else depends on it.
pub fn shard_names(
    mode: ShardMode,
    database_name: &str,
    collection_name: &str,
    shard: u32,
) -> (String, String) {
    match mode {
        ShardMode::Collection => (database_name.to_string(), format!("{collection_name}-shard{shard}")),
        ShardMode::Database => (format!("{database_name}-shard{shard}"), collection_name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{fnv1a64, shard_names, shard_of, ShardMode, ShardingConfig};
    use std::num::NonZeroU32;

    // INV-4: the hash is a specified constant, not whatever a library happens to do.
    #[test]
    fn fnv1a64_matches_the_published_vectors() {
        // The canonical FNV test vectors for the 64-bit variant.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn routing_is_a_pure_function_of_the_key_and_the_count() {
        for shards in [1u32, 2, 3, 7, 10, 64, 1000] {
            for key in ["a", "doc-1", "", "😀", "_rxdb_internal", "z".repeat(200).as_str()] {
                let first = shard_of(key, shards);
                assert_eq!(first, shard_of(key, shards), "pure for {key:?}/{shards}");
                assert!(first < shards, "in range for {key:?}/{shards}");
            }
        }
    }

    #[test]
    fn one_shard_sends_everything_to_shard_zero() {
        for key in ["a", "b", "c", "😀"] {
            assert_eq!(shard_of(key, 1), 0);
        }
    }

    #[test]
    fn a_large_corpus_spreads_across_shards() {
        // Not a distribution test, just a guard against a hash that collapses: if
        // every key landed on one shard, sharding would be pointless and every
        // equivalence test would pass vacuously.
        for shards in [2u32, 10] {
            let mut seen = vec![0usize; shards as usize];
            for index in 0..1000 {
                seen[shard_of(&format!("doc-{index}"), shards) as usize] += 1;
            }
            let occupied = seen.iter().filter(|count| **count > 0).count();
            assert_eq!(occupied, shards as usize, "every shard got data at {shards}");
        }
    }

    #[test]
    fn config_reads_the_documented_shape() {
        let config = ShardingConfig::from_schema(&serde_json::json!({
            "sharding": { "shards": 10, "mode": "collection" }
        }))
        .expect("valid")
        .expect("present");
        assert_eq!(config.shards, NonZeroU32::new(10).expect("nonzero"));
        assert_eq!(config.mode, ShardMode::Collection);

        let database = ShardingConfig::from_schema(&serde_json::json!({
            "sharding": { "shards": 4, "mode": "database" }
        }))
        .expect("valid")
        .expect("present");
        assert_eq!(database.mode, ShardMode::Database);
    }

    #[test]
    fn a_missing_block_means_not_sharded() {
        assert!(ShardingConfig::from_schema(&serde_json::json!({"primaryKey": "id"}))
            .expect("valid")
            .is_none());
        assert!(ShardingConfig::from_schema(&serde_json::json!({"sharding": null}))
            .expect("valid")
            .is_none());
    }

    #[test]
    fn a_malformed_block_is_an_error_not_a_silent_skip() {
        assert!(ShardingConfig::from_schema(&serde_json::json!({"sharding": "yes"})).is_err());
        assert!(ShardingConfig::from_schema(&serde_json::json!({"sharding": {"mode": "collection"}})).is_err());
        assert!(ShardingConfig::from_schema(&serde_json::json!({"sharding": {"shards": 0, "mode": "collection"}})).is_err());
        assert!(ShardingConfig::from_schema(&serde_json::json!({"sharding": {"shards": 2}})).is_err());
        assert!(ShardingConfig::from_schema(
            &serde_json::json!({"sharding": {"shards": 2, "mode": "bogus"}})
        )
        .is_err());
    }

    #[test]
    fn zero_shards_is_unrepresentable() {
        assert!(ShardingConfig::new(0, ShardMode::Collection).is_none());
        assert!(ShardingConfig::new(1, ShardMode::Collection).is_some());
    }

    #[test]
    fn mode_decides_which_name_carries_the_suffix() {
        let (database, collection) =
            shard_names(ShardMode::Collection, "mydb", "humans", 3);
        assert_eq!(database, "mydb");
        assert_eq!(collection, "humans-shard3");

        let (database, collection) = shard_names(ShardMode::Database, "mydb", "humans", 3);
        assert_eq!(database, "mydb-shard3");
        assert_eq!(collection, "humans");
    }
}
