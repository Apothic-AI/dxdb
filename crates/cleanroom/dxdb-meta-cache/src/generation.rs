//! The generation document: the one thing the cache reads from the real store.
//!
//! `specs/meta-cache.md` section 3.1. A monotonically increasing counter plus an
//! identity for the database *incarnation*.
//!
//! The identity exists for one failure mode, and it is the failure mode
//! CLEANROOM.md section 5.10 names: a database that is removed and created again
//! under the same name restarts its generation at 1, so a surviving snapshot tagged
//! `generation: 1` would validate against a completely unrelated database. Its
//! `instance_id` differs, so the snapshot is rejected.

use serde_json::{json, Value};

/// Snapshot layout version. A snapshot whose `format` is not this is rejected before
/// any document is parsed (`INV-8`).
pub const SNAPSHOT_FORMAT: u8 = 1;

/// The `localStorage` key prefix for a database's snapshot.
pub const SNAPSHOT_KEY_PREFIX: &str = "dxdb-meta-cache/v1/";

/// The internal-store key holding the generation document.
pub const GENERATION_KEY: &str = "dxdb-meta-cache-generation";

/// Identity and revision of one database incarnation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationDoc {
    /// Strictly increases on every mutation of the metadata.
    pub generation: u64,
    /// Identifies this incarnation. Random 16 bytes as hex, chosen on creation.
    pub instance_id: String,
    /// The snapshot format this writer understands.
    pub format: u8,
}

impl GenerationDoc {
    /// A new incarnation at generation 1.
    pub fn first(instance_id: impl Into<String>) -> Self {
        Self {
            generation: 1,
            instance_id: instance_id.into(),
            format: SNAPSHOT_FORMAT,
        }
    }

    /// The next generation of the same incarnation.
    ///
    /// `checked_add` rather than `+`: a generation of `u64::MAX` is not reachable in
    /// practice, but wrapping to 0 would make every snapshot valid forever, which is
    /// the one direction of error the spec forbids.
    pub fn next(&self) -> Option<Self> {
        Some(Self {
            generation: self.generation.checked_add(1)?,
            instance_id: self.instance_id.clone(),
            format: self.format,
        })
    }

    /// The stored JSON shape.
    pub fn to_value(&self) -> Value {
        json!({
            "generation": self.generation,
            "instance_id": self.instance_id,
            "format": self.format,
        })
    }

    /// Reads the stored shape.
    ///
    /// A corrupted document yields `None` rather than an error, because the spec
    /// treats a corrupt generation as *missing*: a cache must not be able to veto a
    /// real document permanently (`INV-1`'s "corrupted generation" acceptance case).
    pub fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let generation = object.get("generation")?.as_u64()?;
        let instance_id = object.get("instance_id")?.as_str()?.to_string();
        let format = u8::try_from(object.get("format")?.as_u64()?).ok()?;
        Some(Self {
            generation,
            instance_id,
            format,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::GenerationDoc;

    #[test]
    fn round_trips_through_json() {
        let document = GenerationDoc::first("abc123");
        assert_eq!(
            GenerationDoc::from_value(&document.to_value()),
            Some(document)
        );
    }

    #[test]
    fn a_corrupt_document_reads_as_missing() {
        // Every one of these is a shape a truncated write or a foreign writer could
        // produce; none may be treated as a valid generation, and none may panic.
        for corrupt in [
            serde_json::json!("not an object"),
            serde_json::json!({}),
            serde_json::json!({"generation": "one"}),
            serde_json::json!({"generation": 1}),
            serde_json::json!({"generation": 1, "instance_id": 5, "format": 1}),
            serde_json::json!({"generation": 1, "instance_id": "a", "format": "x"}),
        ] {
            assert_eq!(GenerationDoc::from_value(&corrupt), None, "{corrupt}");
        }
    }

    #[test]
    fn next_refuses_to_wrap() {
        let document = GenerationDoc {
            generation: u64::MAX,
            instance_id: "a".to_string(),
            format: 1,
        };
        assert!(
            document.next().is_none(),
            "wrapping to 0 would make every snapshot valid"
        );
    }

    #[test]
    fn next_preserves_the_instance() {
        let first = GenerationDoc::first("abc");
        let second = first.next().expect("room");
        assert_eq!(second.generation, first.generation + 1);
        assert_eq!(second.instance_id, first.instance_id);
    }
}
