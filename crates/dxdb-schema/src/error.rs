//! Error type for `dxdb-schema`.
//!
//! Upstream throws `RxError` values carrying a code from a catalogue of 196
//! (`src/rx-error.ts`, `src/plugins/dev-mode/error-messages.ts`). The codes that
//! this crate can raise are reproduced by name so a caller can react to `SC39`
//! (primary key without `maxLength`) exactly as it would upstream, and so a
//! ported test can assert the same code it asserts upstream.

use thiserror::Error;

/// The upstream error codes this crate can produce.
///
/// Each variant names the code upstream uses; the doc comment records where it
/// is raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SchemaErrorCode {
    /// `SC1` — a field name does not match the allowed regex.
    Sc1,
    /// `SC7` — a `default` on a nested field.
    Sc7,
    /// `SC8` — a `_`-prefixed field name that is not `_id`/`_deleted`.
    Sc8,
    /// `SC11` — missing or invalid `version`.
    Sc11,
    /// `SC18` — `indexes` is not an array.
    Sc18,
    /// `SC19` — an index entry is neither a string nor an array.
    Sc19,
    /// `SC20` — a compound index entry is not a string.
    Sc20,
    /// `SC24` — `required` given as a boolean.
    Sc24,
    /// `SC29` — no `properties`.
    Sc29,
    /// `SC30` — no `primaryKey`.
    Sc30,
    /// `SC32` — primary key type is not string/number/integer.
    Sc32,
    /// `SC33` — the primary key field is not in `properties`.
    Sc33,
    /// `SC39` — the primary key has no `maxLength`.
    Sc39,
    /// `SC41` — the primary key `maxLength` is not finite.
    Sc41,
    /// `DOC18` — a composite primary key component is missing.
    Doc18,
    /// `DOC19` — a document's own primary key disagrees with the composed one.
    Doc19,
    /// `CI1` — an index field is not in the schema.
    Ci1,
    /// `CI2` — an index field has a type that cannot be encoded.
    Ci2,
    /// A document failed draft-07 validation of the schema.
    Validation,
    /// A schema failed draft-07 validation of itself.
    InvalidSchema,
}

impl SchemaErrorCode {
    /// The code as upstream spells it, for messages and test assertions.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sc1 => "SC1",
            Self::Sc7 => "SC7",
            Self::Sc8 => "SC8",
            Self::Sc11 => "SC11",
            Self::Sc18 => "SC18",
            Self::Sc19 => "SC19",
            Self::Sc20 => "SC20",
            Self::Sc24 => "SC24",
            Self::Sc29 => "SC29",
            Self::Sc30 => "SC30",
            Self::Sc32 => "SC32",
            Self::Sc33 => "SC33",
            Self::Sc39 => "SC39",
            Self::Sc41 => "SC41",
            Self::Doc18 => "DOC18",
            Self::Doc19 => "DOC19",
            Self::Ci1 => "CI1",
            Self::Ci2 => "CI2",
            Self::Validation => "VD1",
            Self::InvalidSchema => "SC44",
        }
    }
}

/// A schema error: an upstream code plus human-readable detail.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {message}", code = .code.as_str())]
pub struct SchemaError {
    /// The upstream code.
    pub code: SchemaErrorCode,
    /// What went wrong, in terms a caller debugging a schema can act on.
    pub message: String,
}

impl SchemaError {
    /// Builds an error from a code and a message.
    pub fn new(code: SchemaErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
